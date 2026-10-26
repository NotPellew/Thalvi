export type Phase = "listening" | "thinking" | "speaking";

export type ServerMessage =
  | { type: "state"; state: Phase }
  | { type: "partial"; text: string }
  | { type: "user"; text: string }
  | { type: "reply"; text: string }
  | { type: "interrupted" }
  | { type: "error"; message: string };

export const CANCEL_MESSAGE = JSON.stringify({ type: "cancel" });

const PHASES: readonly unknown[] = ["listening", "thinking", "speaking"];

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
  const { type, state, text, message } = fields;
  switch (type) {
    case "state":
      return PHASES.includes(state) ? { type, state: state as Phase } : null;
    case "partial":
    case "user":
    case "reply":
      return typeof text === "string" ? { type, text } : null;
    case "interrupted":
      return { type };
    case "error":
      return typeof message === "string" ? { type, message } : null;
    default:
      return null;
  }
}
