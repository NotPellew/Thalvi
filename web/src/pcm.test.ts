import { describe, expect, it } from "vitest";
import { FRAME_SAMPLES, MicEncoder, PcmDecoder, Resampler, floatToPcm16 } from "./pcm.ts";

function ramp(length: number, slope: number): Float32Array {
  return Float32Array.from({ length }, (_, i) => i * slope);
}

function inBlocks(input: Float32Array, size: number): Float32Array[] {
  const blocks: Float32Array[] = [];
  for (let i = 0; i < input.length; i += size) blocks.push(input.subarray(i, i + size));
  return blocks;
}

describe("floatToPcm16", () => {
  it.each([
    [0, 0],
    [1, 32767],
    [-1, -32768],
    [0.5, 16384],
    [-0.5, -16384],
    [3, 32767],
    [-3, -32768],
  ])("maps %f to %i", (input, expected) => {
    expect(Array.from(floatToPcm16(Float32Array.of(input)))).toEqual([expected]);
  });
});

describe("Resampler", () => {
  it.each([48000, 44100, 16000])(
    "resamples a %i Hz ramp to 24 kHz identically however the blocks fall",
    (rate) => {
      const slope = 1e-5;
      const input = ramp(4800, slope);
      const whole = new Resampler(rate).process(input);
      const resampler = new Resampler(rate);
      const chunked = inBlocks(input, 128).flatMap((b) => Array.from(resampler.process(b)));

      expect(chunked).toHaveLength(whole.length);
      const step = rate / 24000;
      expect(whole.length).toBeGreaterThan(Math.floor(4800 / step) - 2);
      whole.forEach((value, k) => {
        expect(value).toBeCloseTo(k * step * slope, 6);
        expect(chunked[k]).toBeCloseTo(value, 6);
      });
    },
  );
});

describe("MicEncoder", () => {
  it("emits 80 ms frames of 24 kHz s16le from 48 kHz input blocks", () => {
    const encoder = new MicEncoder(48000);
    const frames = inBlocks(ramp(3 * 2 * FRAME_SAMPLES + 1000, 1e-5), 128).flatMap((b) =>
      encoder.push(b),
    );

    expect(frames).toHaveLength(3);
    expect(frames.every((f) => f.length === FRAME_SAMPLES)).toBe(true);
    expect(frames[0]?.[1]).toBe(Math.round(2e-5 * 0x7fff));
    expect(frames[1]?.[0]).toBe(Math.round(FRAME_SAMPLES * 2e-5 * 0x7fff));
  });
});

describe("PcmDecoder", () => {
  it("decodes samples that a frame boundary splits in two", () => {
    const decoder = new PcmDecoder();
    const bytes = new Uint8Array(new Int16Array([0, 16384, -32768]).buffer);

    const first = decoder.decode(bytes.subarray(0, 3));
    const second = decoder.decode(bytes.subarray(3));

    expect([...first, ...second]).toEqual([0, 0.5, -1]);
  });
});
