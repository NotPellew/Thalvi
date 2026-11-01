import type { Phase, ServerMessage, TaskStep, Turn } from "./protocol.ts";

export interface SessionState {
  readonly voice: boolean;
  readonly link: "connecting" | "online" | "reconnecting" | "ended";
  readonly phase: Phase;
  readonly playing: boolean;
  readonly partial: string;
  readonly turns: readonly Turn[];
  readonly tasks: readonly TaskStep[];
  readonly replyOpen: boolean;
  readonly notice: string | null;
}

export type DisplayState = Phase | "speaking" | "connecting" | "reconnecting" | "ended";

export type SessionEvent =
  | { type: "opened" }
  | { type: "lost" }
  | { type: "voice"; on: boolean; notice: string | null }
  | { type: "notice"; notice: string }
  | { type: "stopped" }
  | { type: "switched" }
  | { type: "playing"; playing: boolean }
  | { type: "server"; message: ServerMessage };

export const INITIAL: SessionState = {
  voice: false,
  link: "connecting",
  phase: "idle",
  playing: false,
  partial: "",
  turns: [],
  tasks: [],
  replyOpen: false,
  notice: null,
};

export const UNREACHABLE_NOTICE = "Can't reach the server. Trying again…";

/** What the user is told: the server phase, corrected for what the client knows locally. */
export function displayState(s: SessionState, micPending = false): DisplayState {
  if (s.link !== "online") return s.link;
  if (!s.voice) {
    if (s.phase === "thinking" || s.phase === "working") return s.phase;
    return micPending ? "starting" : "idle";
  }
  if (s.playing) return "speaking";
  return s.phase === "idle" ? "starting" : s.phase;
}

export function reduce(s: SessionState, event: SessionEvent): SessionState {
  switch (event.type) {
    case "opened":
      return { ...s, link: "online", phase: "idle", partial: "", tasks: [], replyOpen: false, notice: null };
    case "lost":
      return { ...s, link: "reconnecting", phase: "idle", partial: "", replyOpen: false, notice: UNREACHABLE_NOTICE };
    case "switched":
      return { ...s, link: "connecting", phase: "idle", playing: false, partial: "", turns: [], tasks: [], replyOpen: false, notice: null };
    case "voice":
      return {
        ...s,
        voice: event.on,
        playing: event.on && s.playing,
        partial: event.on ? s.partial : "",
        notice: event.notice,
      };
    case "notice":
      return { ...s, notice: event.notice };
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
    case "session":
      return { ...s, turns: m.turns, partial: "", tasks: [], replyOpen: false };
    case "state":
      return { ...s, phase: m.state, partial: m.state === "listening" ? s.partial : "" };
    case "task": {
      const index = s.tasks.findIndex((step) => step.id === m.step.id);
      const tasks = index < 0
        ? [...s.tasks, m.step]
        : s.tasks.map((step, at) => at === index ? m.step : step);
      return { ...s, tasks };
    }
    case "partial":
      return { ...s, partial: m.text };
    case "user":
      return {
        ...s,
        partial: "",
        tasks: [],
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
    case "voice":
      return { ...s, voice: false, playing: false, partial: "" };
    case "interrupted":
    case "reply_end":
      return { ...s, replyOpen: false };
    case "error":
      return { ...s, notice: m.message };
    case "ended":
      return { ...s, link: "ended", voice: false, phase: "idle", playing: false, partial: "", replyOpen: false, notice: m.message };
  }
}
