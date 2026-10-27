import { describe, expect, it } from "vitest";
import { TimeStretcher } from "./stretch.ts";
import { SPEED_STEPS } from "./speed.ts";

const RATE = 24000;

function tone(seconds: number, freq = 440): Float32Array {
  const out = new Float32Array(Math.round(seconds * RATE));
  for (let i = 0; i < out.length; i++) out[i] = 0.8 * Math.sin((2 * Math.PI * freq * i) / RATE);
  return out;
}

function stretchAll(input: Float32Array, rate: number, chunk = 4096): Float32Array {
  const s = new TimeStretcher();
  const parts: Float32Array[] = [];
  for (let i = 0; i < input.length; i += chunk) {
    s.push(input.subarray(i, i + chunk));
    parts.push(s.pull(rate));
  }
  parts.push(s.finish(rate));
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Float32Array(total);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

function measureFreq(samples: Float32Array): number {
  let crossings = 0;
  for (let i = 1; i < samples.length; i++) {
    const a = samples[i - 1] ?? 0;
    const b = samples[i] ?? 0;
    if ((a < 0 && b >= 0) || (a >= 0 && b < 0)) crossings++;
  }
  return (crossings / 2) * (RATE / samples.length);
}

describe("TimeStretcher", () => {
  it.each(SPEED_STEPS)("output length is input length / %s", (rate) => {
    const input = tone(1);
    const out = stretchAll(input, rate);
    const expected = input.length / rate;
    expect(Math.abs(out.length - expected)).toBeLessThanOrEqual(Math.max(64, expected * 0.02));
  });

  it.each([0.75, 1.5, 2])("a 440 Hz tone keeps its frequency at %s×", (rate) => {
    const out = stretchAll(tone(1), rate);
    const middle = out.subarray(Math.floor(out.length * 0.25), Math.floor(out.length * 0.75));
    expect(Math.abs(measureFreq(middle) - 440)).toBeLessThan(15);
  });

  it("passes audio through essentially unchanged at rate 1", () => {
    const input = tone(0.5, 330);
    const s = new TimeStretcher();
    const parts: Float32Array[] = [];
    for (let i = 0; i < input.length; i += 1000) {
      s.push(input.subarray(i, i + 1000));
      parts.push(s.pull(1));
    }
    parts.push(s.finish(1));
    const out = new Float32Array(input.length);
    let at = 0;
    for (const p of parts) {
      out.set(p.subarray(0, input.length - at), at);
      at += p.length;
    }
    expect(at).toBeGreaterThanOrEqual(input.length);
    let maxDiff = 0;
    for (let i = 0; i < input.length; i++) maxDiff = Math.max(maxDiff, Math.abs((out[i] ?? 0) - (input[i] ?? 0)));
    expect(maxDiff).toBeLessThan(1e-6);
  });

  it("a rate change mid-stream takes effect on the following output", () => {
    const input = tone(2);
    const fast = stretchAll(input, 2);
    const s = new TimeStretcher();
    s.push(input);
    const head = s.pull(2, 2048);
    expect(head).toHaveLength(2048);
    const tail = s.finish(0.5);
    expect(tail.length).toBeGreaterThan(fast.length);
    const tailMiddle = tail.subarray(Math.floor(tail.length * 0.3), Math.floor(tail.length * 0.7));
    expect(Math.abs(measureFreq(tailMiddle) - 440)).toBeLessThan(20);
  });

  it("rejects an invalid rate", () => {
    const s = new TimeStretcher();
    s.push(tone(0.1));
    expect(() => s.pull(0)).toThrow(RangeError);
    expect(() => s.pull(Number.NaN)).toThrow(RangeError);
  });
});
