import type { Phase, ServerMessage } from "./protocol.ts";

export interface Turn {
  readonly role: "user" | "assistant";
  readonly text: string;
}

export interface SessionState {
  readonly screen: "idle" | "active";
  readonly link: "connecting" | "online" | "reconnecting";
  readonly phase: Phase;
  readonly playing: boolean;
  readonly partial: string;
  readonly turns: readonly Turn[];
  readonly replyOpen: boolean;
  readonly notice: string | null;
}

export type DisplayState = Phase | "connecting" | "reconnecting";

export type SessionEvent =
  | { type: "begin" }
  | { type: "end" }
  | { type: "failed"; notice: string }
  | { type: "opened" }
  | { type: "lost" }
  | { type: "stopped" }
  | { type: "playing"; playing: boolean }
  | { type: "server"; message: ServerMessage };

export const IDLE: SessionState = {
  screen: "idle",
  link: "connecting",
  phase: "listening",
  playing: false,
  partial: "",
  turns: [],
  replyOpen: false,
  notice: null,
};

export const UNREACHABLE_NOTICE = "Can't reach the server. Trying again…";

export function displayState(s: SessionState): DisplayState {
  if (s.link !== "online") return s.link;
  return s.playing ? "speaking" : s.phase;
}

export function reduce(s: SessionState, event: SessionEvent): SessionState {
  switch (event.type) {
    case "begin":
      return { ...IDLE, screen: "active" };
    case "end":
      return IDLE;
    case "failed":
      return { ...IDLE, notice: event.notice };
    case "opened":
      return { ...s, link: "online", phase: "listening", partial: "", replyOpen: false, notice: null };
    case "lost":
      return { ...s, link: "reconnecting", phase: "listening", partial: "", replyOpen: false, notice: UNREACHABLE_NOTICE };
    case "stopped":
      return { ...s, replyOpen: false };
    case "playing":
      return { ...s, playing: event.playing };
    case "server":
      return reduceServer(s, event.message);
  }
}

function reduceServer(s: SessionState, m: ServerMessage): SessionState {
  switch (m.type) {
    case "state":
      return { ...s, phase: m.state };
    case "partial":
      return { ...s, partial: m.text };
    case "user":
      return {
        ...s,
        partial: "",
        replyOpen: false,
        notice: null,
        turns: [...s.turns, { role: "user", text: m.text }],
      };
    case "reply": {
      const last = s.turns.at(-1);
      const turns =
        s.replyOpen && last
          ? [...s.turns.slice(0, -1), { role: "assistant" as const, text: last.text + m.text }]
          : [...s.turns, { role: "assistant" as const, text: m.text }];
      return { ...s, turns, replyOpen: true };
    }
    case "interrupted":
      return { ...s, phase: "listening", replyOpen: false };
    case "error":
      return { ...s, notice: m.message };
  }
}
