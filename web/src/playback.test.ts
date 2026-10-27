import { afterEach, describe, expect, it, vi } from "vitest";
import { Playback } from "./playback.ts";

interface PostedMessage {
  type: string;
  seq?: number;
}

class FakePort {
  readonly messages: PostedMessage[] = [];
  onmessage: ((event: MessageEvent<unknown>) => void) | null = null;

  postMessage(message: PostedMessage): void {
    this.messages.push(message);
  }

  deliver(data: unknown): void {
    this.onmessage?.({ data } as MessageEvent<unknown>);
  }
}

class FakeNode {
  static instances: FakeNode[] = [];
  readonly port = new FakePort();

  constructor() {
    FakeNode.instances.push(this);
  }

  connect(): void {
    return;
  }
}

function fakeContext(): AudioContext {
  return {
    state: "running",
    destination: {},
    audioWorklet: { addModule: () => Promise.resolve() },
  } as unknown as AudioContext;
}

function pcmBytes(...samples: number[]): ArrayBuffer {
  return new Int16Array(samples).buffer;
}

async function setup() {
  FakeNode.instances = [];
  vi.stubGlobal("AudioWorkletNode", FakeNode);
  const calls: boolean[] = [];
  const playback = new Playback(fakeContext(), (playing) => calls.push(playing));
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  const node = FakeNode.instances[0];
  if (!node) throw new Error("worklet node was not created");
  return { playback, calls, node };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("Playback", () => {
  it("marks playing synchronously when audio is enqueued", async () => {
    const { playback, calls } = await setup();
    playback.enqueue(pcmBytes(0, 1000, -1000));
    expect(calls).toEqual([true]);
  });

  it("marks playing synchronously even while the worklet is still loading", async () => {
    FakeNode.instances = [];
    vi.stubGlobal("AudioWorkletNode", FakeNode);
    const calls: boolean[] = [];
    const playback = new Playback(fakeContext(), (playing) => calls.push(playing));
    playback.enqueue(pcmBytes(0, 1000));
    expect(calls).toEqual([true]);
    await Promise.resolve();
  });

  it("ignores a drain report for an earlier chunk once a later chunk is enqueued", async () => {
    const { playback, calls, node } = await setup();
    playback.enqueue(pcmBytes(0, 1000));
    playback.enqueue(pcmBytes(0, 2000));
    const seqs = node.port.messages.filter((m) => m.type === "push").map((m) => m.seq);
    expect(seqs).toHaveLength(2);
    node.port.deliver({ type: "playing", playing: false, seq: seqs[0] });
    expect(calls).toEqual([true]);
    node.port.deliver({ type: "playing", playing: false, seq: seqs[1] });
    expect(calls).toEqual([true, false]);
  });

  it("does not flip back to playing from messages in flight across a flush", async () => {
    const { playback, calls, node } = await setup();
    playback.enqueue(pcmBytes(0, 1000));
    const pushSeq = node.port.messages.find((m) => m.type === "push")?.seq;
    playback.flush();
    expect(calls).toEqual([true, false]);
    node.port.deliver({ type: "playing", playing: true, seq: pushSeq });
    node.port.deliver({ type: "playing", playing: false, seq: pushSeq });
    expect(calls).toEqual([true, false]);
  });
});
