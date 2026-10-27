import { describe, expect, it } from "vitest";
import { parseServerMessage, type ServerMessage } from "./protocol.ts";

describe("parseServerMessage", () => {
  const valid: [string, ServerMessage][] = [
    ['{"type":"state","state":"thinking"}', { type: "state", state: "thinking" }],
    ['{"type":"state","state":"idle"}', { type: "state", state: "idle" }],
    ['{"type":"state","state":"starting"}', { type: "state", state: "starting" }],
    ['{"type":"session","id":"abc","turns":[]}', { type: "session", id: "abc", turns: [] }],
    [
      '{"type":"session","id":"abc","turns":[{"role":"user","text":"hi"},{"role":"assistant","text":"Hello."}]}',
      {
        type: "session",
        id: "abc",
        turns: [
          { role: "user", text: "hi" },
          { role: "assistant", text: "Hello." },
        ],
      },
    ],
    ['{"type":"partial","text":"hel"}', { type: "partial", text: "hel" }],
    ['{"type":"user","text":"hello"}', { type: "user", text: "hello" }],
    ['{"type":"reply","text":"Hi."}', { type: "reply", text: "Hi." }],
    ['{"type":"interrupted"}', { type: "interrupted" }],
    ['{"type":"voice","on":false}', { type: "voice", on: false }],
    ['{"type":"reply_end"}', { type: "reply_end" }],
    ['{"type":"error","message":"TTS failed"}', { type: "error", message: "TTS failed" }],
    [
      '{"type":"ended","reason":"moved","message":"This conversation continued on another device."}',
      { type: "ended", reason: "moved", message: "This conversation continued on another device." },
    ],
    [
      '{"type":"ended","reason":"deleted","message":"This conversation was deleted."}',
      { type: "ended", reason: "deleted", message: "This conversation was deleted." },
    ],
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
    ["voice turned on by the server", '{"type":"voice","on":true}'],
    ["voice without a flag", '{"type":"voice"}'],
    ["missing text", '{"type":"partial"}'],
    ["session without id", '{"type":"session","turns":[]}'],
    ["session without turns", '{"type":"session","id":"abc"}'],
    ["session with non-array turns", '{"type":"session","id":"abc","turns":"none"}'],
    ["session turn with unknown role", '{"type":"session","id":"abc","turns":[{"role":"system","text":"x"}]}'],
    ["session turn without text", '{"type":"session","id":"abc","turns":[{"role":"user"}]}'],
    ["non-string error", '{"type":"error","message":{}}'],
    ["ended with an unknown reason", '{"type":"ended","reason":"expired","message":"x"}'],
    ["ended without a message", '{"type":"ended","reason":"moved"}'],
  ];
  it.each(ignored)("ignores %s", (_name, raw) => {
    expect(parseServerMessage(raw)).toBeNull();
  });
});
