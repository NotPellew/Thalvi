import type { Fetch } from "./conversations.ts";

export interface Learning {
  readonly id: string;
  readonly text: string;
  /** The conversation it was learned from, or null when the user wrote it. */
  readonly sourceSession: string | null;
}

export interface LearningsApi {
  list(): Promise<Learning[]>;
  edit(id: string, text: string): Promise<void>;
}

/** The longest learning the server accepts, in characters. */
export const MAX_LEARNING_CHARS = 1000;

const UNREACHABLE = "Can't reach the server.";

function parseList(value: unknown): Learning[] | null {
  if (typeof value !== "object" || value === null) return null;
  const rows = (value as Record<string, unknown>)["learnings"];
  if (!Array.isArray(rows)) return null;
  const list: Learning[] = [];
  for (const row of rows as unknown[]) {
    if (typeof row !== "object" || row === null) return null;
    const { id, text, source_session: source } = row as Record<string, unknown>;
    if (typeof id !== "string" || typeof text !== "string") return null;
    if (source !== null && typeof source !== "string") return null;
    list.push({ id, text, sourceSession: source });
  }
  return list;
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

/** Talks to the server's `/learnings` endpoints; every failure is an Error with a message fit to show. */
export function learningsApi(fetchImpl: Fetch): LearningsApi {
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
      const response = await call("/learnings");
      const list = parseList((await response.json()) as unknown);
      if (!list) throw new Error("The server sent a list of learnings this page can't read.");
      return list;
    },
    async edit(id, text) {
      await call(`/learnings/${encodeURIComponent(id)}`, {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text }),
      });
    },
  };
}

export interface LearningsState {
  readonly open: boolean;
  readonly loading: boolean;
  readonly items: readonly Learning[];
  /** The learning whose text is being edited. */
  readonly editing: string | null;
  /** What the user last tried to save, kept so a failed save can be retried. */
  readonly draft: string | null;
  readonly saving: boolean;
  readonly error: string | null;
}

const CLOSED: LearningsState = {
  open: false,
  loading: false,
  items: [],
  editing: null,
  draft: null,
  saving: false,
  error: null,
};

function plain(error: unknown): string {
  return error instanceof Error ? error.message : "Something went wrong.";
}

/** The collapsible list of learnings: opened only by the user, edited one row at a time. */
export class Learnings {
  state: LearningsState = CLOSED;
  private readonly api: LearningsApi;
  private handler: (state: LearningsState) => void = () => undefined;
  private generation = 0;

  constructor(api: LearningsApi) {
    this.api = api;
  }

  onChange(handler: (state: LearningsState) => void): void {
    this.handler = handler;
  }

  /** Opens the list and loads it fresh, so rows learned since the last look are there. */
  async show(): Promise<void> {
    const generation = ++this.generation;
    this.set({ ...CLOSED, open: true, loading: true });
    try {
      const items = await this.api.list();
      if (generation === this.generation) this.set({ ...this.state, loading: false, items });
    } catch (failure) {
      if (generation === this.generation) this.set({ ...this.state, loading: false, error: plain(failure) });
    }
  }

  hide(): void {
    this.generation++;
    this.set(CLOSED);
  }

  startEdit(id: string): void {
    if (this.state.saving) return;
    this.set({ ...this.state, editing: id, draft: null, error: null });
  }

  cancelEdit(): void {
    if (this.state.saving) return;
    this.set({ ...this.state, editing: null, draft: null, error: null });
  }

  async save(id: string, text: string): Promise<void> {
    if (this.state.editing !== id || this.state.saving) return;
    const generation = this.generation;
    const trimmed = text.trim();
    if (trimmed === "") {
      this.set({ ...this.state, draft: text, error: "The text can't be empty." });
      return;
    }
    this.set({ ...this.state, draft: text, saving: true, error: null });
    try {
      await this.api.edit(id, trimmed);
    } catch (failure) {
      if (generation === this.generation) this.set({ ...this.state, saving: false, error: plain(failure) });
      return;
    }
    if (generation !== this.generation) return;
    const items = this.state.items.map((item) => (item.id === id ? { ...item, text: trimmed } : item));
    const stillEditing = this.state.editing === id;
    this.set({
      ...this.state,
      items,
      editing: stillEditing ? null : this.state.editing,
      draft: stillEditing ? null : this.state.draft,
      saving: false,
      error: null,
    });
  }

  private set(state: LearningsState): void {
    this.state = state;
    this.handler(state);
  }
}
