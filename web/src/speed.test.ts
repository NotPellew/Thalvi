import { describe, expect, it } from "vitest";
import { DEFAULT_SPEED, SPEED_STEPS, formatSpeed, loadSpeed, normalizeSpeed, saveSpeed } from "./speed.ts";

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
  it.each(SPEED_STEPS)("accepts step %s", (rate) => {
    expect(normalizeSpeed(rate)).toBe(rate);
    expect(normalizeSpeed(String(rate))).toBe(rate);
  });

  it.each([null, undefined, "", "fast", "1.3", "0", "-1", 0, 1.3, 3, Number.NaN])(
    "falls back to 1× for invalid stored value %s",
    (value) => {
      expect(normalizeSpeed(value)).toBe(DEFAULT_SPEED);
    },
  );

  it.each(SPEED_STEPS)("round-trips stored step %s", (rate) => {
    const storage = fakeStorage();
    saveSpeed(storage, rate);
    expect(loadSpeed(storage)).toBe(rate);
  });

  it("loads 1× when nothing is stored and for an invalid stored value", () => {
    expect(loadSpeed(fakeStorage())).toBe(1);
    expect(loadSpeed(fakeStorage({ "thalvi.speed": "banana" }))).toBe(1);
    expect(loadSpeed(fakeStorage({ "thalvi.speed": "1.3" }))).toBe(1);
  });

  it("an invalid value passed to save is stored as 1×", () => {
    const storage = fakeStorage();
    saveSpeed(storage, 1.3);
    expect(loadSpeed(storage)).toBe(1);
  });

  it.each([
    [0.75, "0.75×"],
    [1, "1×"],
    [1.1, "1.1×"],
    [2, "2×"],
  ])("formats %s as %s", (rate, label) => {
    expect(formatSpeed(rate)).toBe(label);
  });
});
