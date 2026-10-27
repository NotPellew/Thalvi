import { describe, expect, it } from "vitest";
import { micErrorMessage } from "./mic.ts";

describe("micErrorMessage", () => {
  it.each([
    ["NotAllowedError", "Microphone access was denied. Allow it in your browser settings, then tap the reticle again."],
    ["NotFoundError", "No microphone was found."],
    ["AbortError", "The microphone could not be started."],
  ])("explains %s in plain words", (name, message) => {
    expect(micErrorMessage(new DOMException("", name))).toBe(message);
  });
});
