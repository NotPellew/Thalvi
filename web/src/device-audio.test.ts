import { afterEach, describe, expect, it, vi } from "vitest";
import { Controller } from "./controller.ts";
import { DeviceAudio } from "./device-audio.ts";

const ENDED = "This conversation continued on another device.";

interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
  reject(error: unknown): void;
}

function deferred<T>(): Deferred<T> {
  let resolve: (value: T) => void = () => undefined;
  let reject: (error: unknown) => void = () => undefined;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

class FakeTrack {
  readyState: "live" | "ended" = "live";
  onended: (() => void) | null = null;
  stop(): void {
    this.readyState = "ended";
  }
}

class FakeStream {
  readonly tracks = [new FakeTrack()];
  getTracks(): FakeTrack[] {
    return this.tracks;
  }
  getAudioTracks(): FakeTrack[] {
    return this.tracks;
  }
}

const node = () => ({ port: {}, connect: () => undefined, disconnect: () => undefined });

/** A browser whose permission prompt and `ctx.resume()` the test settles by hand. */
function browser() {
  const contexts: FakeContext[] = [];
  const resume = deferred<undefined>();
  const permission = deferred<FakeStream>();

  class FakeContext {
    state: "running" | "closed" = "running";
    readonly destination = {};
    readonly audioWorklet = { addModule: () => Promise.resolve() };
    constructor() {
      contexts.push(this);
    }
    resume = () => resume.promise;
    close = () => {
      this.state = "closed";
      return Promise.resolve();
    };
    createMediaStreamSource = node;
  }

  // Like a closed context in Chromium: addModule resolves, then building the node throws.
  class FakeWorkletNode {
    readonly port = {};
    constructor(ctx: FakeContext) {
      if (ctx.state === "closed") throw new DOMException("The context is closed.", "InvalidStateError");
    }
    connect = () => undefined;
    disconnect = () => undefined;
  }

  const getUserMedia = vi.fn(() => permission.promise);
  vi.stubGlobal("AudioContext", FakeContext);
  vi.stubGlobal("AudioWorkletNode", FakeWorkletNode);
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia } });
  return { contexts, resume, permission, getUserMedia };
}

function setup() {
  const sent: string[] = [];
  const busy: (string | null)[] = [];
  const t = browser();
  const controller: Controller = new Controller({
    send: (text) => sent.push(text),
    playback: { enqueue: () => undefined, flush: () => undefined },
    remember: () => undefined,
    forget: () => undefined,
    stopConnection: () => undefined,
    connect: () => undefined,
    connectPrivate: () => undefined,
    releaseUnused: () => {
      deviceAudio.releaseUnused();
    },
    onChange: () => undefined,
  });
  const deviceAudio = new DeviceAudio(
    controller,
    { setBusy: (b) => busy.push(b), sendFrame: () => undefined },
    1,
  );
  controller.opened();
  sent.length = 0;
  const end = () => {
    controller.message(JSON.stringify({ type: "ended", reason: "moved", message: ENDED }));
  };
  return { ...t, controller, deviceAudio, sent, busy, end };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("DeviceAudio.startVoice", () => {
  it("turns voice on and keeps the microphone while it is on", async () => {
    const t = setup();
    const started = t.deviceAudio.startVoice();
    t.resume.resolve(undefined);
    const stream = new FakeStream();
    await vi.waitFor(() => {
      expect(t.getUserMedia).toHaveBeenCalled();
    });
    t.permission.resolve(stream);
    await started;
    expect(t.controller.state.voice).toBe(true);
    expect(stream.tracks[0]?.readyState).toBe("live");

    t.controller.setVoice(false);
    expect(stream.tracks[0]?.readyState).toBe("ended");
    expect(t.contexts[0]?.state).toBe("closed");
    expect(t.busy).toEqual(["mic", null]);
  });

  it("tells the user when the microphone is refused, and releases the audio context", async () => {
    const t = setup();
    const started = t.deviceAudio.startVoice();
    t.resume.resolve(undefined);
    await vi.waitFor(() => {
      expect(t.getUserMedia).toHaveBeenCalled();
    });
    t.permission.reject(new DOMException("", "NotAllowedError"));
    await started;
    expect(t.controller.state.voice).toBe(false);
    expect(t.controller.state.notice).toBe(
      "Microphone access was denied. Allow it in your browser settings, then tap the reticle again.",
    );
    expect(t.contexts[0]?.state).toBe("closed");
  });

  it.each([
    ["the permission prompt is open", "permission"],
    ["the audio context is resuming", "resume"],
  ] as const)("holds no microphone and keeps voice off when the conversation ends while %s", async (_, when) => {
    const t = setup();
    const started = t.deviceAudio.startVoice();
    if (when === "permission") {
      t.resume.resolve(undefined);
      await vi.waitFor(() => {
      expect(t.getUserMedia).toHaveBeenCalled();
    });
    }
    t.end();
    if (when === "resume") t.resume.resolve(undefined);
    await vi.waitFor(() => {
      expect(t.getUserMedia).toHaveBeenCalled();
    });
    const stream = new FakeStream();
    t.permission.resolve(stream);
    await started;

    expect(stream.tracks.map((track) => track.readyState)).toEqual(["ended"]);
    expect(t.controller.state.voice).toBe(false);
    expect(t.sent).toEqual([]);
    expect(t.contexts.map((ctx) => ctx.state)).toEqual(["closed"]);
    expect(t.controller.state.notice).toBe(ENDED);
    expect(t.busy).toEqual(["mic", null]);
  });
});
