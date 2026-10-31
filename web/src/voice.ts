import type { Fetch } from "./conversations.ts";

export const VOICE_STORAGE_KEY = "thalvi.voice";

export interface VoiceList {
  readonly voices: readonly string[];
  readonly defaultVoice: string;
}

export interface VoiceApi {
  list(): Promise<VoiceList>;
  /** The WAV the server synthesized for `voice`. */
  preview(voice: string): Promise<ArrayBuffer>;
}

/** Plays one preview at a time; `play` settles when it ends or is stopped. */
export interface PreviewPlayer {
  play(wav: ArrayBuffer): Promise<void>;
  stop(): void;
}

const UNREACHABLE = "Can't reach the server.";

function parseList(value: unknown): VoiceList | null {
  if (typeof value !== "object" || value === null) return null;
  const { voices, default: defaultVoice } = value as Record<string, unknown>;
  if (!Array.isArray(voices) || typeof defaultVoice !== "string") return null;
  const names = voices as unknown[];
  if (!names.every((name): name is string => typeof name === "string") || !names.includes(defaultVoice)) return null;
  return { voices: names, defaultVoice };
}

async function failure(response: Response): Promise<Error> {
  try {
    const body = (await response.json()) as unknown;
    const message = typeof body === "object" && body !== null ? (body as Record<string, unknown>)["error"] : null;
    if (typeof message === "string" && message !== "") return new Error(message);
  } catch {
    // The body was not the server's JSON error; fall through to the status.
  }
  return new Error(`The server answered with an error (${String(response.status)}).`);
}

/** Talks to the server's `/voices` and `/voice/preview`; every failure is an Error with a message fit to show. */
export function voiceApi(fetchImpl: Fetch): VoiceApi {
  async function call(path: string, init?: RequestInit): Promise<Response> {
    let response: Response;
    try {
      response = await fetchImpl(path, init);
    } catch {
      throw new Error(UNREACHABLE);
    }
    if (!response.ok) throw await failure(response);
    return response;
  }
  return {
    async list() {
      const list = parseList((await (await call("/voices")).json()) as unknown);
      if (!list) throw new Error("The server sent a list of voices this page can't read.");
      return list;
    },
    async preview(voice) {
      const response = await call("/voice/preview", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ voice }),
      });
      return response.arrayBuffer();
    },
  };
}

/** The saved voice when the server still offers it, else the server's default. Never writes. */
export function loadVoice(storage: Pick<Storage, "getItem">, list: VoiceList): string {
  try {
    const saved = storage.getItem(VOICE_STORAGE_KEY);
    return saved !== null && list.voices.includes(saved) ? saved : list.defaultVoice;
  } catch {
    return list.defaultVoice;
  }
}

export function saveVoice(storage: Pick<Storage, "setItem">, voice: string): void {
  try {
    storage.setItem(VOICE_STORAGE_KEY, voice);
  } catch {
    return;
  }
}

export interface VoicePickerState {
  readonly open: boolean;
  readonly loading: boolean;
  readonly voices: readonly string[];
  /** The voice this browser uses; null until the server's list has loaded. */
  readonly applied: string | null;
  /** The voice picked in the list, not yet applied. */
  readonly candidate: string | null;
  readonly preview: "idle" | "loading" | "playing";
  readonly error: string | null;
  readonly notice: string | null;
}

const CLOSED: VoicePickerState = {
  open: false,
  loading: false,
  voices: [],
  applied: null,
  candidate: null,
  preview: "idle",
  error: null,
  notice: null,
};

function plain(error: unknown): string {
  return error instanceof Error ? error.message : "Something went wrong.";
}

export interface VoicePickerDeps {
  readonly api: VoiceApi;
  readonly player: PreviewPlayer;
  readonly storage: Pick<Storage, "getItem" | "setItem">;
  /** Tells the live session which voice to use; only called while the socket is open. */
  readonly send: (voice: string) => void;
}

/** A collapsed voice list: previews on request, applies on request, remembers the choice per browser. */
export class VoicePicker {
  state: VoicePickerState = CLOSED;
  private readonly deps: VoicePickerDeps;
  private handler: (state: VoicePickerState) => void = () => undefined;
  private connected = false;
  private previewRun = 0;

  constructor(deps: VoicePickerDeps) {
    this.deps = deps;
  }

  onChange(handler: (state: VoicePickerState) => void): void {
    this.handler = handler;
  }

  /** Fetches the server's voices without opening the picker. */
  async load(): Promise<void> {
    if (this.state.loading) return;
    if (this.state.preview !== "idle") this.stopPreview();
    this.set({ ...this.state, loading: true, applied: null, candidate: null, voices: [], error: null, notice: null });
    try {
      const list = await this.deps.api.list();
      const applied = loadVoice(this.deps.storage, list);
      this.set({ ...this.state, loading: false, voices: list.voices, applied, candidate: applied });
      this.sendApplied();
    } catch (failure) {
      this.set({ ...this.state, loading: false, error: plain(failure) });
    }
  }

  /** Refreshes the offered voices on connection so a removed saved voice falls back to the default. */
  async socketOpened(): Promise<void> {
    this.connected = true;
    await this.load();
  }

  socketLost(): void {
    this.connected = false;
  }

  show(): void {
    this.set({ ...this.state, open: true, notice: null });
    if (this.state.applied === null) void this.load();
  }

  hide(): void {
    this.stopPreview();
    this.set({ ...this.state, open: false, notice: null });
  }

  choose(voice: string): void {
    if (!this.state.voices.includes(voice) || voice === this.state.candidate) return;
    this.stopPreview();
    this.set({ ...this.state, candidate: voice, error: null, notice: null });
  }

  /** Plays the candidate, replacing any earlier preview; the same call again while it runs stops it. */
  async preview(): Promise<void> {
    const voice = this.state.candidate;
    if (voice === null) return;
    if (this.state.preview !== "idle") {
      this.stopPreview();
      return;
    }
    const run = ++this.previewRun;
    this.set({ ...this.state, preview: "loading", error: null, notice: null });
    try {
      const wav = await this.deps.api.preview(voice);
      if (run !== this.previewRun) return;
      this.set({ ...this.state, preview: "playing" });
      await this.deps.player.play(wav);
    } catch (failure) {
      if (run === this.previewRun) this.set({ ...this.state, preview: "idle", error: plain(failure) });
      return;
    }
    if (run === this.previewRun) this.set({ ...this.state, preview: "idle" });
  }

  stopPreview(): void {
    this.previewRun++;
    this.deps.player.stop();
    if (this.state.preview !== "idle") this.set({ ...this.state, preview: "idle" });
  }

  apply(): void {
    const voice = this.state.candidate;
    if (voice === null || voice === this.state.applied) return;
    saveVoice(this.deps.storage, voice);
    this.set({ ...this.state, applied: voice, error: null, notice: `Using ${voice} on this device.` });
    this.sendApplied();
  }

  private sendApplied(): void {
    if (this.connected && this.state.applied !== null) this.deps.send(this.state.applied);
  }

  private set(state: VoicePickerState): void {
    this.state = state;
    this.handler(state);
  }
}
