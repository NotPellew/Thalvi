export type Phase = "idle" | "starting" | "listening" | "thinking" | "working";

export interface Turn {
  readonly role: "user" | "assistant";
  readonly text: string;
}

export interface TaskStep {
  readonly id: string;
  readonly label: string;
  readonly status: "running" | "done" | "failed";
  readonly detail?: string;
}

export type ServerMessage =
  | { type: "session"; id: string; turns: Turn[] }
  | { type: "state"; state: Phase }
  | { type: "task"; step: TaskStep }
  | { type: "partial"; text: string }
  | { type: "user"; text: string }
  | { type: "reply"; text: string }
  | { type: "voice"; on: false }
  | { type: "interrupted" }
  | { type: "reply_end" }
  | { type: "error"; message: string }
  | { type: "ended"; reason: "moved" | "deleted"; message: string };

export const CANCEL_MESSAGE = JSON.stringify({ type: "cancel" });
export const MAX_TEXT_LENGTH = 20000;

export function voiceMessage(on: boolean): string {
  return JSON.stringify({ type: "voice", on });
}

/** Tells the server that everything up to `bytes` of reply audio on this connection has played or been dropped. */
export function playedMessage(bytes: number): string {
  return JSON.stringify({ type: "played", bytes });
}

/** Picks the voice this connection's replies are spoken in. */
export function voiceNameMessage(voice: string): string {
  return JSON.stringify({ type: "voice_name", voice });
}

export function textMessage(text: string): string {
  return JSON.stringify({ type: "text", text });
}

const PHASES: readonly unknown[] = ["idle", "starting", "listening", "thinking", "working"];

function parseTurns(value: unknown): Turn[] | null {
  if (!Array.isArray(value)) return null;
  const turns: Turn[] = [];
  for (const item of value as unknown[]) {
    if (typeof item !== "object" || item === null) return null;
    const { role, text } = item as Record<string, unknown>;
    if ((role !== "user" && role !== "assistant") || typeof text !== "string") return null;
    turns.push({ role, text });
  }
  return turns;
}

function parseTaskStep(value: unknown): TaskStep | null {
  if (typeof value !== "object" || value === null) return null;
  const { id, label, status, detail } = value as Record<string, unknown>;
  if (typeof id !== "string" || typeof label !== "string") return null;
  if (status !== "running" && status !== "done" && status !== "failed") return null;
  if (detail !== undefined && typeof detail !== "string") return null;
  return detail === undefined ? { id, label, status } : { id, label, status, detail };
}

/** Returns the typed message, or null for anything unknown or malformed. */
export function parseServerMessage(raw: string): ServerMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const fields = value as Record<string, unknown>;
  const { type, id, turns, state, text, message } = fields;
  switch (type) {
    case "session": {
      const restored = parseTurns(turns);
      return typeof id === "string" && restored ? { type, id, turns: restored } : null;
    }
    case "state":
      return PHASES.includes(state) ? { type, state: state as Phase } : null;
    case "task": {
      const step = parseTaskStep(fields["step"]);
      return step ? { type, step } : null;
    }
    case "partial":
    case "user":
    case "reply":
      return typeof text === "string" ? { type, text } : null;
    case "voice":
      return fields["on"] === false ? { type, on: false } : null;
    case "interrupted":
    case "reply_end":
      return { type };
    case "error":
      return typeof message === "string" ? { type, message } : null;
    case "ended":
      return (fields["reason"] === "moved" || fields["reason"] === "deleted") &&
        typeof message === "string"
        ? { type, reason: fields["reason"], message }
        : null;
    default:
      return null;
  }
}
