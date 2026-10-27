import { describe, expect, it } from "vitest";
import { displayState, INITIAL, type SessionState } from "./session.ts";

const online: SessionState = { ...INITIAL, link: "online" };

describe("displayState", () => {
  const cases: [string, SessionState, boolean, string][] = [
    ["is Idle with voice off", online, false, "idle"],
    ["is Starting while the microphone permission prompt is open", online, true, "starting"],
    ["keeps Thinking over a pending microphone prompt", { ...online, phase: "thinking" }, true, "thinking"],
    ["keeps Reconnecting over a pending microphone prompt", { ...online, link: "reconnecting" }, true, "reconnecting"],
  ];
  it.each(cases)("%s", (_name, state, micPending, expected) => {
    expect(displayState(state, micPending)).toBe(expected);
  });
});
