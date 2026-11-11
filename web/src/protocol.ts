export type Phase = "idle" | "starting" | "recognizer_retrying" | "listening" | "thinking" | "working";

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

/** Where the user's turns come from: typing, or microphone audio the server recognizes. */
export type InputSource = "text" | "mic";

export type ServerMessage =
  | { type: "session"; id: string; turns: Turn[]; private?: true }
  | { type: "state"; state: Phase }
  | { type: "task"; step: TaskStep }
  | { type: "partial"; text: string }
  | { type: "user"; text: string }
  | { type: "reply"; text: string }
  | { type: "input"; source: InputSource; ready: boolean }
  | { type: "reply_audio"; on: boolean }
  | { type: "interrupted" }
  | { type: "reply_end" }
  | { type: "error"; message: string }
  | { type: "ended"; reason: "moved" | "deleted" | "private"; message: string };

export const CANCEL_MESSAGE = JSON.stringify({ type: "cancel" });
export const MAX_TEXT_LENGTH = 20000;

/** Chooses text input (no recognizer, no mic upload) or server-recognized microphone input. */
export function inputMessage(source: InputSource): string {
  return JSON.stringify({ type: "input", source });
}

/** Turns spoken replies on or off, whatever the input. */
export function replyAudioMessage(on: boolean): string {
  return JSON.stringify({ type: "reply_audio", on });
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

const PHASES: readonly unknown[] = ["idle", "starting", "recognizer_retrying", "listening", "thinking", "working"];

export function parseTurns(value: unknown): Turn[] | null {
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
      if (typeof id !== "string" || !restored) return null;
      return fields["private"] === true
        ? { type, id, turns: restored, private: true }
        : { type, id, turns: restored };
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
    case "input": {
      const { source, ready } = fields;
      return (source === "text" || source === "mic") && typeof ready === "boolean"
        ? { type, source, ready }
        : null;
    }
    case "reply_audio":
      return typeof fields["on"] === "boolean" ? { type, on: fields["on"] } : null;
    case "interrupted":
    case "reply_end":
      return { type };
    case "error":
      return typeof message === "string" ? { type, message } : null;
    case "ended":
      return (fields["reason"] === "moved" || fields["reason"] === "deleted" || fields["reason"] === "private") &&
        typeof message === "string"
        ? { type, reason: fields["reason"], message }
        : null;
    default:
      return null;
  }
}
