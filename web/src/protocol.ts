export type Phase = "idle" | "starting" | "listening" | "thinking" | "speaking";

export interface Turn {
  readonly role: "user" | "assistant";
  readonly text: string;
}

export type ServerMessage =
  | { type: "session"; id: string; turns: Turn[] }
  | { type: "state"; state: Phase }
  | { type: "partial"; text: string }
  | { type: "user"; text: string }
  | { type: "reply"; text: string }
  | { type: "interrupted" }
  | { type: "reply_end" }
  | { type: "error"; message: string };

export const CANCEL_MESSAGE = JSON.stringify({ type: "cancel" });
export const MAX_TEXT_LENGTH = 20000;

export function voiceMessage(on: boolean): string {
  return JSON.stringify({ type: "voice", on });
}

export function textMessage(text: string): string {
  return JSON.stringify({ type: "text", text });
}

const PHASES: readonly unknown[] = ["idle", "starting", "listening", "thinking", "speaking"];

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
    case "partial":
    case "user":
    case "reply":
      return typeof text === "string" ? { type, text } : null;
    case "interrupted":
    case "reply_end":
      return { type };
    case "error":
      return typeof message === "string" ? { type, message } : null;
    default:
      return null;
  }
}
