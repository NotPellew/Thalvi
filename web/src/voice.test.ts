import { describe, expect, it } from "vitest";
import type { Fetch } from "./conversations.ts";
import {
  VOICE_STORAGE_KEY,
  VoicePicker,
  loadVoice,
  voiceApi,
  type PreviewPlayer,
  type VoiceApi,
  type VoiceList,
} from "./voice.ts";

const LIST: VoiceList = { voices: ["af_heart", "sohee", "af_bella"], defaultVoice: "af_heart" };

function respond(status: number, body: unknown, init: ResponseInit = {}): Response {
  return new Response(typeof body === "string" || body instanceof ArrayBuffer ? body : JSON.stringify(body), {
    status,
    ...init,
  });
}

function recording(response: () => Response | Promise<Response>) {
  const requests: { path: string; init: RequestInit | undefined }[] = [];
  const fetchImpl: Fetch = async (path, init) => {
    requests.push({ path, init });
    return response();
  };
  return { api: voiceApi(fetchImpl), requests };
}

function fakeStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => map.get(key) ?? null,
    setItem: (key: string, value: string) => {
      map.set(key, value);
    },
    map,
  };
}

describe("voiceApi", () => {
  it("lists the voices and the default the server offers", async () => {
    const { api, requests } = recording(() =>
      respond(200, { voices: ["af_heart", "sohee"], default: "af_heart" }),
    );
    expect(await api.list()).toEqual({ voices: ["af_heart", "sohee"], defaultVoice: "af_heart" });
    expect(requests.map((r) => r.path)).toEqual(["/voices"]);
  });

  it.each([
    ["a default that is not offered", { voices: ["sohee"], default: "af_heart" }],
    ["a non-string voice", { voices: ["sohee", 3], default: "sohee" }],
    ["no voices", { default: "sohee" }],
    ["no default", { voices: ["sohee"] }],
  ])("refuses a list with %s", async (_name, body) => {
    const { api } = recording(() => respond(200, body));
    await expect(api.list()).rejects.toThrow("can't read");
  });

  it("posts the voice as JSON and returns the audio bytes", async () => {
    const { api, requests } = recording(() => respond(200, new Uint8Array([1, 2, 3]).buffer));
    expect(new Uint8Array(await api.preview("sohee"))).toEqual(new Uint8Array([1, 2, 3]));
    expect(requests.map((r) => [r.path, r.init?.method, r.init?.body])).toEqual([
      ["/voice/preview", "POST", '{"voice":"sohee"}'],
    ]);
  });

  it.each([
    ["the server's own words", () => respond(500, { error: "Couldn't play that voice." }), "Couldn't play that voice."],
    ["the status when the body is not its JSON", () => respond(502, "<html>"), "The server answered with an error (502)."],
  ])("explains a failed preview with %s", async (_name, response, message) => {
    const { api } = recording(response);
    await expect(api.preview("sohee")).rejects.toThrow(message);
  });

  it("says the server is unreachable when the request itself fails", async () => {
    const api = voiceApi(() => Promise.reject(new TypeError("network")));
    await expect(api.preview("sohee")).rejects.toThrow("Can't reach the server.");
  });
});

describe("loadVoice", () => {
  it.each([
    ["a saved voice the server offers", "sohee", "sohee"],
    ["no saved voice", null, "af_heart"],
    ["a saved voice the server no longer offers", "gone", "af_heart"],
    ["an empty saved value", "", "af_heart"],
  ])("uses %s", (_name, saved, expected) => {
    const storage = fakeStorage(saved === null ? {} : { [VOICE_STORAGE_KEY]: saved });
    expect(loadVoice(storage, LIST)).toBe(expected);
    expect(storage.map.get(VOICE_STORAGE_KEY) ?? null).toBe(saved);
  });

  it("falls back to the default when storage throws", () => {
    const storage = {
      getItem: () => {
        throw new Error("blocked");
      },
    };
    expect(loadVoice(storage, LIST)).toBe("af_heart");
  });
});

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (error: Error) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function setup(options: { saved?: string; list?: Promise<VoiceList> } = {}) {
  const storage = fakeStorage(options.saved === undefined ? {} : { [VOICE_STORAGE_KEY]: options.saved });
  const sent: string[] = [];
  const previews: { voice: string; reply: Deferred<ArrayBuffer> }[] = [];
  const played: { wav: ArrayBuffer; end: () => void }[] = [];
  let stops = 0;
  const api: VoiceApi = {
    list: () => options.list ?? Promise.resolve(LIST),
    preview: (voice) => {
      const reply = deferred<ArrayBuffer>();
      previews.push({ voice, reply });
      return reply.promise;
    },
  };
  const player: PreviewPlayer = {
    play: (wav) =>
      new Promise<void>((resolve) => {
        played.push({ wav, end: resolve });
      }),
    stop: () => {
      stops++;
      for (const clip of played.splice(0)) clip.end();
    },
  };
  const picker = new VoicePicker({ api, player, storage, send: (voice) => sent.push(voice) });
  const stopCount = () => stops;
  return { picker, storage, sent, previews, played, stopCount };
}

const WAV = new Uint8Array([9, 9]).buffer;

async function settle(): Promise<void> {
  for (let i = 0; i < 5; i++) await Promise.resolve();
}

describe("VoicePicker choice", () => {
  it("loads the voices without opening and offers them with the server default selected", async () => {
    const { picker, sent } = setup();
    await picker.load();
    expect(picker.state).toMatchObject({
      open: false,
      voices: LIST.voices,
      applied: "af_heart",
      candidate: "af_heart",
    });
    expect(sent).toEqual([]);
  });

  it("tells the server the saved voice on every socket open, once the list is known", async () => {
    const { picker, sent } = setup({ saved: "sohee" });
    const opened = picker.socketOpened();
    expect(sent).toEqual([]);
    await opened;
    expect(sent).toEqual(["sohee"]);
    picker.socketLost();
    await picker.socketOpened();
    expect(sent).toEqual(["sohee", "sohee"]);
  });

  it("falls back to the server default for an unknown saved voice without overwriting it", async () => {
    const { picker, sent, storage } = setup({ saved: "gone" });
    await picker.socketOpened();
    expect(sent).toEqual(["af_heart"]);
    expect(picker.state.applied).toBe("af_heart");
    expect(storage.map.get(VOICE_STORAGE_KEY)).toBe("gone");
  });

  it("refreshes the server's voices on reconnect before applying a saved choice", async () => {
    const options = { saved: "sohee", list: Promise.resolve(LIST) };
    const { picker, sent, storage } = setup(options);
    await picker.socketOpened();
    expect(sent).toEqual(["sohee"]);
    picker.socketLost();
    options.list = Promise.resolve({ voices: ["af_heart"], defaultVoice: "af_heart" });
    await picker.socketOpened();
    expect(sent).toEqual(["sohee", "af_heart"]);
    expect(picker.state).toMatchObject({ voices: ["af_heart"], applied: "af_heart", candidate: "af_heart" });
    expect(storage.map.get(VOICE_STORAGE_KEY)).toBe("sohee");
  });

  it("applies only on request: saves, sends, and marks the voice in use", async () => {
    const { picker, sent, storage } = setup();
    await picker.load();
    await picker.socketOpened();
    picker.choose("sohee");
    expect(sent).toEqual(["af_heart"]);
    expect(storage.map.has(VOICE_STORAGE_KEY)).toBe(false);
    picker.apply();
    expect(sent).toEqual(["af_heart", "sohee"]);
    expect(storage.map.get(VOICE_STORAGE_KEY)).toBe("sohee");
    expect(picker.state).toMatchObject({ applied: "sohee", notice: "Using sohee on this device." });
  });

  it("keeps a choice made offline and sends it when the socket returns", async () => {
    const { picker, sent, storage } = setup();
    await picker.load();
    picker.choose("af_bella");
    picker.apply();
    expect(sent).toEqual([]);
    expect(storage.map.get(VOICE_STORAGE_KEY)).toBe("af_bella");
    await picker.socketOpened();
    expect(sent).toEqual(["af_bella"]);
  });

  it("ignores a voice the server did not offer and re-applying the voice in use", async () => {
    const { picker, sent, storage } = setup();
    await picker.load();
    await picker.socketOpened();
    picker.choose("nobody");
    picker.apply();
    expect(picker.state.candidate).toBe("af_heart");
    expect(sent).toEqual(["af_heart"]);
    expect(storage.map.has(VOICE_STORAGE_KEY)).toBe(false);
  });

  it("opens only when asked, retries a failed load, and says why it failed", async () => {
    let fail = true;
    const list = () => (fail ? Promise.reject(new Error("Can't reach the server.")) : Promise.resolve(LIST));
    const storage = fakeStorage();
    const picker = new VoicePicker({
      api: { list, preview: () => Promise.resolve(WAV) },
      player: { play: () => Promise.resolve(), stop: () => undefined },
      storage,
      send: () => undefined,
    });
    await picker.load();
    expect(picker.state).toMatchObject({ open: false, error: "Can't reach the server.", applied: null });
    fail = false;
    picker.show();
    await settle();
    expect(picker.state).toMatchObject({ open: true, error: null, applied: "af_heart" });
    picker.hide();
    expect(picker.state.open).toBe(false);
  });
});

describe("VoicePicker preview", () => {
  async function ready() {
    const harness = setup();
    await harness.picker.load();
    harness.picker.show();
    return harness;
  }

  it("synthesizes and plays nothing until Preview is tapped, then plays the returned audio once", async () => {
    const { picker, previews, played } = await ready();
    expect(previews).toEqual([]);
    const run = picker.preview();
    expect(picker.state.preview).toBe("loading");
    expect(previews.map((p) => p.voice)).toEqual(["af_heart"]);
    previews[0]?.reply.resolve(WAV);
    await settle();
    expect(picker.state.preview).toBe("playing");
    expect(played.map((p) => p.wav)).toEqual([WAV]);
    played[0]?.end();
    await run;
    expect(picker.state).toMatchObject({ preview: "idle", error: null });
  });

  it("stops the clip when Preview is tapped again or another voice is picked", async () => {
    const harness = await ready();
    const { picker, previews, played } = harness;
    void picker.preview();
    previews[0]?.reply.resolve(WAV);
    await settle();
    expect(played).toHaveLength(1);
    await picker.preview();
    expect(harness.stopCount()).toBe(1);
    expect(picker.state.preview).toBe("idle");

    void picker.preview();
    previews[1]?.reply.resolve(WAV);
    await settle();
    picker.choose("sohee");
    expect(harness.stopCount()).toBe(2);
    expect(picker.state).toMatchObject({ preview: "idle", candidate: "sohee" });
  });

  it("drops a preview that arrives after it was replaced or the panel closed", async () => {
    const { picker, previews, played } = await ready();
    void picker.preview();
    picker.choose("sohee");
    previews[0]?.reply.resolve(WAV);
    await settle();
    expect(played).toEqual([]);
    expect(picker.state.preview).toBe("idle");

    void picker.preview();
    expect(previews.map((p) => p.voice)).toEqual(["af_heart", "sohee"]);
    picker.hide();
    previews[1]?.reply.resolve(WAV);
    await settle();
    expect(played).toEqual([]);
  });

  it("reports a failed preview in plain words and lets the user try again", async () => {
    const { picker, previews } = await ready();
    const run = picker.preview();
    previews[0]?.reply.reject(new Error("Couldn't play that voice. Speech isn't responding."));
    await run;
    expect(picker.state).toMatchObject({
      preview: "idle",
      error: "Couldn't play that voice. Speech isn't responding.",
    });
    void picker.preview();
    expect(picker.state).toMatchObject({ preview: "loading", error: null });
  });

  it("never sends a voice or changes the saved choice", async () => {
    const { picker, previews, played, sent, storage } = await ready();
    await picker.socketOpened();
    picker.choose("sohee");
    const run = picker.preview();
    previews[0]?.reply.resolve(WAV);
    await settle();
    played[0]?.end();
    await run;
    expect(sent).toEqual(["af_heart"]);
    expect(storage.map.has(VOICE_STORAGE_KEY)).toBe(false);
  });
});
