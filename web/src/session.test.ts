import { describe, expect, it } from "vitest";
import { displayState, INITIAL, reduce, type SessionState } from "./session.ts";

const online: SessionState = { ...INITIAL, link: "online" };

describe("displayState", () => {
  const cases: [string, SessionState, boolean, string][] = [
    ["is Idle with voice off", online, false, "idle"],
    ["is Starting while the microphone permission prompt is open", online, true, "starting"],
    ["keeps Thinking over a pending microphone prompt", { ...online, phase: "thinking" }, true, "thinking"],
    ["keeps Working with voice off", { ...online, phase: "working" }, false, "working"],
    ["keeps Reconnecting over a pending microphone prompt", { ...online, link: "reconnecting" }, true, "reconnecting"],
  ];
  it.each(cases)("%s", (_name, state, micPending, expected) => {
    expect(displayState(state, micPending)).toBe(expected);
  });
});

describe("task steps", () => {
  it("adds and updates steps by id, then clears them for the next turn", () => {
    const running = { id: "call-1", label: "Searching conversations", status: "running" as const };
    let state = reduce(online, { type: "server", message: { type: "task", step: running } });
    expect(state.tasks).toEqual([running]);
    const done = { ...running, status: "done" as const, detail: "2 matches" };
    state = reduce(state, { type: "server", message: { type: "task", step: done } });
    expect(state.tasks).toEqual([done]);
    state = reduce(state, { type: "server", message: { type: "user", text: "next question" } });
    expect(state.tasks).toEqual([]);
  });
});
