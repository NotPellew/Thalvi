import { describe, expect, it } from "vitest";
import { SPEED_STEPS, formatSpeed, loadSpeed, saveSpeed, type Speed } from "./speed.ts";

function fakeStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => {
      map.set(k, v);
    },
    map,
  };
}

describe("speed validation and persistence", () => {
  it.each(SPEED_STEPS)("loads stored step %s", (rate) => {
    expect(loadSpeed(fakeStorage({ "thalvi.speed": String(rate) }))).toBe(rate);
  });

  it.each(["banana", "1.3", "0", "-1", "3", ""])(
    "loads 1× for an invalid stored value %s",
    (stored) => {
      expect(loadSpeed(fakeStorage({ "thalvi.speed": stored }))).toBe(1);
    },
  );

  it("loads 1× when nothing is stored", () => {
    expect(loadSpeed(fakeStorage())).toBe(1);
  });

  it.each(SPEED_STEPS)("round-trips stored step %s", (rate: Speed) => {
    const storage = fakeStorage();
    saveSpeed(storage, rate);
    expect(loadSpeed(storage)).toBe(rate);
  });

  it.each([
    [0.75, "0.75×"],
    [1, "1×"],
    [1.1, "1.1×"],
    [2, "2×"],
  ] as const)("formats %s as %s", (rate, label) => {
    expect(formatSpeed(rate)).toBe(label);
  });
});
