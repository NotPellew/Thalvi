import { describe, expect, it } from "vitest";
import type { Settings } from "./contract.ts";
import { applyPatch } from "./fake.ts";
import { settingsPatch } from "./patch.ts";
import { SETTINGS } from "./testing.ts";

describe("the patch of a change", () => {
  const changes: [string, (settings: Settings) => void, unknown][] = [
    ["nothing", () => undefined, {}],
    ["a switch", (s) => { s.overlay.draft = false; }, { overlay: { draft: false } }],
    ["two fields of one group", (s) => { s.capture_limits.idle = 90; s.capture_limits.max = 600; }, { capture_limits: { idle: 90, max: 600 } }],
    ["a setting set", (s) => { s.microphone = "USB Microphone"; }, { microphone: "USB Microphone" }],
    ["a key of a table removed", (s) => { Reflect.deleteProperty(s.endings, "never mind"); }, { endings: { "never mind": null } }],
    ["a key of a table added", (s) => { s.endings["send it"] = "submit"; }, { endings: { "send it": "submit" } }],
    ["a list, which is sent whole", (s) => { s.scripts.push({ phrase: "a", action: "open_url", destination: "b", label: null }); }, { scripts: [...SETTINGS.scripts, { phrase: "a", action: "open_url", destination: "b", label: null }] }],
    ["an optional setting added then removed", (s) => { s.stt.phrases = ["Thalvi"]; }, { stt: { phrases: ["Thalvi"] } }],
    ["the endpoint set", (s) => { s.writing = { url: "http://x", model: "m" }; }, { writing: { url: "http://x", model: "m" } }],
  ];

  it.each(changes)("carries only %s", (_name, change, patch) => {
    const after = structuredClone(SETTINGS);
    change(after);
    expect(settingsPatch(SETTINGS, after)).toEqual(patch);
  });

  it.each(changes)("is merged back into what the desktop holds: %s", (_name, change) => {
    const after = structuredClone(SETTINGS);
    change(after);
    expect(applyPatch(SETTINGS, settingsPatch(SETTINGS, after))).toEqual(after);
  });

  it("removes a setting that goes back to its default, with null", () => {
    const before = { ...structuredClone(SETTINGS), microphone: "USB Microphone" };
    expect(settingsPatch(before, SETTINGS)).toEqual({ microphone: null });
    expect(applyPatch(before, { microphone: null })).not.toHaveProperty("microphone");
  });

  it("leaves a setting changed since alone, which is the point of sending a patch", () => {
    const heldNow = structuredClone(SETTINGS);
    heldNow.look = "holo";
    const mine = structuredClone(SETTINGS);
    mine.cues.wake = false;
    const merged = applyPatch(heldNow, settingsPatch(SETTINGS, mine));
    expect([merged.look, merged.cues.wake]).toEqual(["holo", false]);
  });
});
