import { describe, expect, it } from "vitest";
import { parseServerMessage, type ServerMessage } from "./protocol.ts";

describe("parseServerMessage", () => {
  const valid: [string, ServerMessage][] = [
    ['{"type":"state","state":"thinking"}', { type: "state", state: "thinking" }],
    ['{"type":"partial","text":"hel"}', { type: "partial", text: "hel" }],
    ['{"type":"user","text":"hello"}', { type: "user", text: "hello" }],
    ['{"type":"reply","text":"Hi."}', { type: "reply", text: "Hi." }],
    ['{"type":"interrupted"}', { type: "interrupted" }],
    ['{"type":"error","message":"TTS failed"}', { type: "error", message: "TTS failed" }],
  ];
  it.each(valid)("accepts %s", (raw, expected) => {
    expect(parseServerMessage(raw)).toEqual(expected);
  });

  const ignored = [
    ["not json", "{nope"],
    ["not an object", "42"],
    ["null", "null"],
    ["unknown type", '{"type":"telemetry","text":"x"}'],
    ["missing type", '{"text":"x"}'],
    ["unknown state", '{"type":"state","state":"dancing"}'],
    ["non-string text", '{"type":"reply","text":7}'],
    ["missing text", '{"type":"partial"}'],
    ["non-string error", '{"type":"error","message":{}}'],
  ];
  it.each(ignored)("ignores %s", (_name, raw) => {
    expect(parseServerMessage(raw)).toBeNull();
  });
});
