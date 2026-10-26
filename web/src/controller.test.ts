import { describe, expect, it } from "vitest";
import { Controller } from "./controller.ts";
import { displayState, UNREACHABLE_NOTICE } from "./session.ts";

const VOICE_ON = 'send {"type":"voice","on":true}';
const VOICE_OFF = 'send {"type":"voice","on":false}';

function setup() {
  const calls: string[] = [];
  const controller = new Controller({
    send: (text) => calls.push(`send ${text}`),
    playback: {
      enqueue: (bytes) => calls.push(`enqueue ${String(bytes.byteLength)}`),
      flush: () => calls.push("flush"),
    },
    remember: (id) => calls.push(`remember ${id}`),
    releaseVoice: () => calls.push("release"),
    onChange: () => undefined,
  });
  const server = (message: object) => {
    controller.message(JSON.stringify(message));
  };
  const shown = () => displayState(controller.state);
  return { controller, calls, server, shown };
}

function online() {
  const t = setup();
  t.controller.opened();
  t.calls.length = 0;
  return t;
}

describe("Controller", () => {
  it("is Connecting until the socket opens, then Idle with voice off", () => {
    const { controller, shown } = setup();
    expect(shown()).toBe("connecting");
    controller.opened();
    expect(shown()).toBe("idle");
  });

  it("shows Starting from the voice tap until the server reports listening", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    expect(calls).toEqual([VOICE_ON]);
    expect(shown()).toBe("starting");
    server({ type: "state", state: "starting" });
    expect(shown()).toBe("starting");
    server({ type: "state", state: "listening" });
    expect(shown()).toBe("listening");
  });

  it("shows a spoken turn: transcript, reply text, and Speaking until local audio ends", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "listening" });
    calls.length = 0;

    server({ type: "partial", text: "what is" });
    server({ type: "partial", text: "what is the time" });
    expect(controller.state.partial).toBe("what is the time");
    server({ type: "user", text: "What is the time?" });
    server({ type: "state", state: "thinking" });
    expect(controller.state.partial).toBe("");
    expect(shown()).toBe("thinking");

    server({ type: "state", state: "speaking" });
    server({ type: "reply", text: "It is " });
    server({ type: "reply", text: "noon." });
    controller.message(new ArrayBuffer(3840));
    controller.playing(true);
    server({ type: "state", state: "listening" });
    expect(shown()).toBe("speaking");
    controller.playing(false);
    expect(shown()).toBe("listening");

    expect(controller.state.turns).toEqual([
      { role: "user", text: "What is the time?" },
      { role: "assistant", text: "It is noon." },
    ]);
    expect(calls).toEqual(["enqueue 3840"]);
  });

  it("voice off tells the server, drops local audio and goes idle while the reply text continues", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "speaking" });
    server({ type: "reply", text: "Once upon" });
    server({ type: "partial", text: "and then" });
    controller.playing(true);
    calls.length = 0;

    controller.setVoice(false);
    expect(calls).toEqual([VOICE_OFF, "flush"]);
    expect(shown()).toBe("idle");
    expect(controller.state.playing).toBe(false);
    expect(controller.state.partial).toBe("");

    server({ type: "reply", text: " a time." });
    server({ type: "state", state: "idle" });
    expect(controller.state.turns).toEqual([{ role: "assistant", text: "Once upon a time." }]);
    expect(shown()).toBe("idle");
  });

  it("tears voice down without telling the server when the server turns it off after a fatal error", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "starting" });
    controller.playing(true);
    calls.length = 0;

    server({ type: "error", message: "The speech service refused the connection." });
    server({ type: "voice", on: false });
    server({ type: "state", state: "idle" });

    expect(calls).toEqual(["flush", "release"]);
    expect(controller.state.voice).toBe(false);
    expect(controller.state.playing).toBe(false);
    expect(shown()).toBe("idle");
    expect(controller.state.notice).toBe("The speech service refused the connection.");
  });

  it("shows Thinking for a text reply with voice off, then Idle with the reply closed", () => {
    const { controller, server, shown } = online();
    server({ type: "user", text: "hello" });
    server({ type: "state", state: "thinking" });
    expect(shown()).toBe("thinking");
    server({ type: "reply", text: "Hi." });
    expect(controller.state.replyOpen).toBe(true);
    server({ type: "reply_end" });
    server({ type: "state", state: "idle" });
    expect(shown()).toBe("idle");
    expect(controller.state.replyOpen).toBe(false);
  });

  it.each([
    ["trims and sends", "  hello\nthere \n", 'send {"type":"text","text":"hello\\nthere"}'],
    ["sends a long paste at the limit", "x".repeat(20000), `send {"type":"text","text":"${"x".repeat(20000)}"}`],
  ])("%s, dropping reply audio still playing first", (_name, typed, sent) => {
    const { controller, calls } = online();
    expect(controller.sendText(typed)).toBe(true);
    expect(calls).toEqual(["flush", sent]);
  });

  it.each([
    ["starting", false],
    ["thinking", false],
    ["idle", false],
    ["listening", true],
  ] as const)("state %s keeps the live partial transcript: %s", (state, kept) => {
    const { controller, server } = online();
    controller.setVoice(true);
    server({ type: "partial", text: "mm" });
    server({ type: "state", state });
    expect(controller.state.partial).toBe(kept ? "mm" : "");
  });

  it.each([
    ["empty", "", null],
    ["blank", " \n\t ", null],
    ["past the limit", "x".repeat(20001), "That message is too long. The limit is 20000 characters."],
  ])("keeps %s text in the composer and sends nothing", (_name, typed, notice) => {
    const { controller, calls } = online();
    expect(controller.sendText(typed)).toBe(false);
    expect(calls).toEqual([]);
    expect(controller.state.notice).toBe(notice);
  });

  it("drops queued audio on interrupted and starts a new reply afterwards", () => {
    const { controller, calls, server } = online();
    controller.setVoice(true);
    server({ type: "user", text: "tell me a story" });
    server({ type: "state", state: "speaking" });
    server({ type: "reply", text: "Once upon" });
    controller.playing(true);
    calls.length = 0;

    server({ type: "interrupted" });
    controller.playing(false);
    server({ type: "state", state: "listening" });
    expect(calls).toEqual(["flush"]);
    expect(displayState(controller.state)).toBe("listening");

    server({ type: "reply", text: "Sure." });
    expect(controller.state.turns.map((t) => t.text)).toEqual(["tell me a story", "Once upon", "Sure."]);
  });

  it("Stop sends cancel and drops local audio at once", () => {
    const { controller, calls } = online();
    controller.stop();
    expect(calls).toEqual(['send {"type":"cancel"}', "flush"]);
  });

  it("remembers the session id and renders the restored turns, replacing what was shown", () => {
    const { controller, calls, server } = online();
    server({ type: "session", id: "s1", turns: [{ role: "user", text: "hi" }] });
    server({ type: "reply", text: "Hello." });
    expect(calls).toEqual(["remember s1"]);

    controller.lost();
    controller.opened();
    server({
      type: "session",
      id: "s1",
      turns: [
        { role: "user", text: "hi" },
        { role: "assistant", text: "Hello." },
      ],
    });
    expect(controller.state.turns).toEqual([
      { role: "user", text: "hi" },
      { role: "assistant", text: "Hello." },
    ]);
    server({ type: "reply", text: "More." });
    expect(controller.state.turns.at(-1)).toEqual({ role: "assistant", text: "More." });
    expect(controller.state.turns).toHaveLength(3);
  });

  it("shows Reconnecting when the link drops, then re-enables voice on the new connection", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "speaking" });
    calls.length = 0;

    controller.lost();
    expect(shown()).toBe("reconnecting");
    expect(controller.state.notice).toBe(UNREACHABLE_NOTICE);
    expect(calls).toEqual(["flush"]);

    controller.opened();
    expect(calls).toEqual(["flush", VOICE_ON]);
    expect(shown()).toBe("starting");
    expect(controller.state.notice).toBeNull();
  });

  it("does not send voice on after a reconnect when voice is off", () => {
    const { controller, calls } = online();
    controller.lost();
    controller.opened();
    expect(calls).toEqual(["flush"]);
  });

  it("shows server errors until the next user turn", () => {
    const { controller, server } = online();
    server({ type: "error", message: "The voice service failed." });
    expect(controller.state.notice).toBe("The voice service failed.");
    server({ type: "user", text: "hello" });
    expect(controller.state.notice).toBeNull();
  });

  it("keeps voice off and tells the user when the mic cannot start or stops", () => {
    const { controller, calls } = online();
    controller.notify("No microphone was found.");
    expect(controller.state.voice).toBe(false);
    expect(controller.state.notice).toBe("No microphone was found.");

    controller.setVoice(true);
    controller.setVoice(false, "The microphone stopped working.");
    expect(controller.state.voice).toBe(false);
    expect(controller.state.notice).toBe("The microphone stopped working.");
    expect(calls).toEqual([VOICE_ON, VOICE_OFF, "flush"]);
  });

  it("ignores unknown and malformed messages", () => {
    const { controller, calls, server } = online();
    const before = controller.state;
    controller.message("{broken");
    server({ type: "weather", text: "sunny" });
    server({ type: "reply", text: 5 });
    server({ type: "session", turns: [] });
    controller.message(42);
    expect(controller.state).toEqual(before);
    expect(calls).toEqual([]);
  });
});
