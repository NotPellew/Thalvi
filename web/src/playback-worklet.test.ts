import { beforeAll, describe, expect, it, vi } from "vitest";

interface Port {
  posted: unknown[];
  onmessage: ((event: { data: unknown }) => void) | null;
  postMessage(message: unknown): void;
}

interface Processor {
  readonly port: Port;
  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean;
}

let Registered: new () => Processor;

beforeAll(async () => {
  vi.stubGlobal("sampleRate", 48000);
  vi.stubGlobal(
    "AudioWorkletProcessor",
    class {
      readonly port: Port = {
        posted: [],
        onmessage: null,
        postMessage(message: unknown) {
          this.posted.push(message);
        },
      };
    },
  );
  vi.stubGlobal("registerProcessor", (_name: string, ctor: new () => Processor) => {
    Registered = ctor;
  });
  await import("./playback-worklet.ts");
});

function tone(samples: number): Float32Array {
  return Float32Array.from({ length: samples }, (_, i) => 0.5 * Math.sin((2 * Math.PI * 440 * i) / 24000));
}

/** Renders quanta until the processor reports a drain; returns the quanta and the drain message. */
function render(processor: Processor, maxQuanta = 2000): { quanta: Float32Array[]; drained: unknown } {
  const quanta: Float32Array[] = [];
  for (let i = 0; i < maxQuanta; i++) {
    const channel = new Float32Array(128);
    processor.process([], [[channel]]);
    quanta.push(channel);
    if (processor.port.posted.length > 0) return { quanta, drained: processor.port.posted.at(-1) };
  }
  return { quanta, drained: undefined };
}

const loud = (quantum: Float32Array) => quantum.some((sample) => Math.abs(sample) > 1e-3);

describe("PlaybackProcessor", () => {
  it.each([1, 1.5, 0.75] as const)("plays a %s× run without a silent gap and reports its drain with the latest sequence", (rate) => {
    const processor = new Registered();
    processor.port.onmessage?.({ data: { type: "rate", rate } });
    processor.port.onmessage?.({ data: { type: "push", seq: 1, samples: tone(2400) } });
    processor.port.onmessage?.({ data: { type: "push", seq: 2, samples: tone(2400) } });

    const { quanta, drained } = render(processor);

    expect(drained).toEqual({ type: "playing", playing: false, seq: 2 });
    const lastLoud = quanta.findLastIndex(loud);
    expect(quanta.slice(0, lastLoud).every(loud)).toBe(true);
    const expectedQuanta = (4800 / rate) * 2 / 128;
    expect(Math.abs(lastLoud + 1 - expectedQuanta)).toBeLessThanOrEqual(3);
  });

  it("drops queued audio on flush without posting, then reports the next run's drain", () => {
    const processor = new Registered();
    processor.port.onmessage?.({ data: { type: "push", seq: 1, samples: tone(4800) } });
    processor.process([], [[new Float32Array(128)]]);
    processor.port.onmessage?.({ data: { type: "flush", seq: 2 } });
    expect(processor.port.posted).toEqual([]);

    const silent = new Float32Array(128);
    processor.process([], [[silent]]);
    expect(loud(silent)).toBe(false);

    processor.port.onmessage?.({ data: { type: "push", seq: 3, samples: tone(480) } });
    expect(render(processor).drained).toEqual({ type: "playing", playing: false, seq: 3 });
  });
});
