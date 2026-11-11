import { describe, expect, it } from "vitest";
import { Controller } from "./controller.ts";
import { displayState, PRIVATE_LOST_NOTICE, UNREACHABLE_NOTICE } from "./session.ts";

const INPUT_MIC = 'send {"type":"input","source":"mic"}';
const INPUT_TEXT = 'send {"type":"input","source":"text"}';
const AUDIO_ON = 'send {"type":"reply_audio","on":true}';
const AUDIO_OFF = 'send {"type":"reply_audio","on":false}';

function setup() {
  const calls: string[] = [];
  const controller = new Controller({
    send: (text) => calls.push(`send ${text}`),
    playback: {
      enqueue: (bytes) => calls.push(`enqueue ${String(bytes.byteLength)}`),
      flush: () => calls.push("flush"),
    },
    remember: (id) => calls.push(`remember ${id}`),
    forget: () => calls.push("forget"),
    stopConnection: () => calls.push("stopConnection"),
    connect: (id) => calls.push(`connect ${String(id)}`),
    connectPrivate: () => calls.push("connectPrivate"),
    releaseUnused: () => calls.push("release"),
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
    expect(calls).toEqual([INPUT_MIC, AUDIO_ON]);
    expect(shown()).toBe("starting");
    server({ type: "state", state: "starting" });
    expect(shown()).toBe("starting");
    server({ type: "state", state: "listening" });
    expect(shown()).toBe("listening");
  });

  it("shows recognizer retrying until ready clears its notice, and Voice can still stop", () => {
    const { controller, calls, server, shown } = online();
    const notice = "Can't reach speech recognition. Trying again…";
    controller.setVoice(true);
    server({ type: "error", message: notice });
    server({ type: "state", state: "recognizer_retrying" });

    expect(shown()).toBe("recognizer_retrying");
    expect(controller.state.notice).toBe(notice);

    server({ type: "input", source: "mic", ready: true });
    expect(controller.state.notice).toBeNull();
    server({ type: "state", state: "listening" });
    expect(shown()).toBe("listening");

    server({ type: "error", message: "The reply failed." });
    server({ type: "input", source: "mic", ready: true });
    expect(controller.state.notice).toBe("The reply failed.");

    server({ type: "error", message: notice });
    server({ type: "state", state: "recognizer_retrying" });
    calls.length = 0;
    controller.setVoice(false);
    expect(calls).toEqual([INPUT_TEXT, AUDIO_OFF, "flush", "release"]);
    expect(shown()).toBe("idle");
    expect(controller.state.notice).toBeNull();
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

    server({ type: "state", state: "thinking" });
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
    expect(calls).toEqual(["enqueue 3840", 'send {"type":"played","bytes":3840}']);
  });

  it("shows Speaking while the spoken acknowledgement plays, though the server is still thinking", () => {
    const { controller, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "listening" });
    server({ type: "user", text: "Plan my route." });
    server({ type: "state", state: "thinking" });

    controller.message(new ArrayBuffer(3840));
    controller.playing(true);
    expect(shown()).toBe("speaking");
    controller.playing(false);
    expect(shown()).toBe("thinking");

    server({ type: "state", state: "thinking" });
    controller.message(new ArrayBuffer(3840));
    controller.playing(true);
    expect(shown()).toBe("speaking");
  });

  it("voice off tells the server, drops local audio and keeps thinking while the reply text continues", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "thinking" });
    server({ type: "reply", text: "Once upon" });
    server({ type: "partial", text: "and then" });
    controller.playing(true);
    calls.length = 0;

    controller.setVoice(false);
    expect(calls).toEqual([INPUT_TEXT, AUDIO_OFF, "flush", "release"]);
    expect(shown()).toBe("thinking");
    expect(controller.state.playing).toBe(false);
    expect(controller.state.partial).toBe("");

    server({ type: "reply", text: " a time." });
    server({ type: "state", state: "idle" });
    expect(controller.state.turns).toEqual([{ role: "assistant", text: "Once upon a time." }]);
    expect(shown()).toBe("idle");
  });

  it.each([
    ["spoken replies were on", true],
    ["spoken replies were off", false],
  ])("tears the mic down and leaves reply audio as it was when the server falls back to text input and %s", (_name, speak) => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    if (!speak) controller.setSpeak(false);
    server({ type: "state", state: "starting" });
    if (speak) controller.playing(true);
    calls.length = 0;

    server({ type: "error", message: "The speech service refused the connection." });
    server({ type: "input", source: "text", ready: true });
    server({ type: "state", state: "idle" });

    expect(calls).toEqual(["release"]);
    expect(controller.state).toMatchObject({ voice: false, speak, playing: speak });
    expect(shown()).toBe(speak ? "speaking" : "idle");
    expect(controller.state.notice).toBe("The speech service refused the connection.");
  });

  it("shows Working and task progress without opening or closing the reply", () => {
    const { controller, server, shown } = online();
    server({ type: "user", text: "what did we decide?" });
    server({ type: "state", state: "working" });
    server({ type: "task", step: { id: "search-1", label: "Searching conversations", status: "running" } });
    expect(shown()).toBe("working");
    expect(controller.state.tasks).toEqual([
      { id: "search-1", label: "Searching conversations", status: "running" },
    ]);
    server({ type: "task", step: { id: "search-1", label: "Searching conversations", status: "done", detail: "1 match" } });
    server({ type: "reply", text: "We decided to keep it simple." });
    expect(controller.state.replyOpen).toBe(true);
    expect(controller.state.tasks[0]?.status).toBe("done");
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
    server({ type: "state", state: "thinking" });
    server({ type: "reply", text: "Once upon" });
    controller.playing(true);
    calls.length = 0;

    server({ type: "interrupted" });
    controller.playing(false);
    server({ type: "state", state: "listening" });
    expect(calls).toEqual(["flush", 'send {"type":"played","bytes":0}']);
    expect(displayState(controller.state)).toBe("listening");

    server({ type: "reply", text: "Sure." });
    expect(controller.state.turns.map((t) => t.text)).toEqual(["tell me a story", "Once upon", "Sure."]);
  });

  it("reports all reply audio received on the connection each time playback drains", () => {
    const { controller, calls } = online();
    controller.message(new ArrayBuffer(100));
    controller.playing(true);
    controller.playing(false);
    controller.message(new ArrayBuffer(50));
    controller.playing(false);
    controller.lost();
    controller.opened();
    controller.message(new ArrayBuffer(30));
    controller.playing(false);
    expect(calls.filter((call) => call.includes("played"))).toEqual([
      'send {"type":"played","bytes":100}',
      'send {"type":"played","bytes":150}',
      'send {"type":"played","bytes":30}',
    ]);
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

  it("shows why the server ended the conversation, stops voice and reconnecting, and forgets it", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "thinking" });
    controller.playing(true);
    calls.length = 0;

    server({ type: "ended", reason: "moved", message: "This conversation continued on another device." });
    expect(calls).toEqual(["flush", "release", "forget", "stopConnection"]);
    expect(shown()).toBe("ended");
    expect(controller.state).toMatchObject({
      voice: false,
      speak: false,
      playing: false,
      notice: "This conversation continued on another device.",
    });
  });

  it("switching joins the chosen conversation, shows its history when the server sends it, and keeps voice on", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "user", text: "old question" });
    calls.length = 0;

    controller.switchTo("s9");
    expect(calls).toEqual(["flush", "connect s9"]);
    expect(shown()).toBe("connecting");
    expect(controller.state.turns).toEqual([]);
    expect(controller.state.notice).toBeNull();

    controller.opened();
    server({
      type: "session",
      id: "s9",
      turns: [
        { role: "user", text: "what's twelve times seven?" },
        { role: "assistant", text: "Eighty-four." },
      ],
    });
    expect(calls.slice(2)).toEqual([INPUT_MIC, AUDIO_ON, "remember s9"]);
    expect(controller.state.turns.map((turn) => turn.text)).toEqual(["what's twelve times seven?", "Eighty-four."]);

    controller.switchTo(null);
    expect(calls.at(-1)).toBe("connect null");
    expect(controller.state.turns).toEqual([]);
  });

  it("starts a private conversation by forgetting the saved id, and remembers no id the server gives it", () => {
    const { controller, calls, server, shown } = online();
    server({ type: "session", id: "s1", turns: [] });
    calls.length = 0;

    controller.startPrivate();
    expect(calls).toEqual(["flush", "forget", "connectPrivate"]);
    expect(shown()).toBe("connecting");
    expect(controller.state.private).toBe(true);

    controller.opened();
    server({ type: "session", id: "private-1", turns: [], private: true });
    expect(calls).not.toContain("remember private-1");
    expect(controller.state.private).toBe(true);

    controller.switchTo(null);
    expect(controller.state.private).toBe(false);
  });

  it("shows Private only when the server says the conversation is private", () => {
    const { controller, server } = online();
    controller.startPrivate();
    controller.opened();
    server({ type: "session", id: "s2", turns: [] });
    expect(controller.state.private).toBe(false);
  });

  it("ends a private conversation when the connection drops, and says it can't be resumed", () => {
    const { controller, calls, server, shown } = online();
    controller.startPrivate();
    controller.opened();
    server({ type: "session", id: "private-1", turns: [], private: true });
    server({ type: "user", text: "a private question" });
    calls.length = 0;

    controller.lost();
    expect(shown()).toBe("ended");
    expect(controller.state).toMatchObject({ private: true, notice: PRIVATE_LOST_NOTICE });
    expect(calls).toEqual(["flush", "release"]);
  });

  it("ignores input readiness updates and a text fallback it already acted on", () => {
    const { controller, calls, server } = online();
    controller.setVoice(true);
    calls.length = 0;
    server({ type: "input", source: "mic", ready: false });
    server({ type: "input", source: "mic", ready: true });
    expect(controller.state.voice).toBe(true);

    controller.setVoice(false);
    calls.length = 0;
    server({ type: "input", source: "text", ready: true });
    expect(calls).toEqual([]);
  });

  it("shows Reconnecting when the link drops, then re-enables voice on the new connection", () => {
    const { controller, calls, server, shown } = online();
    controller.setVoice(true);
    server({ type: "state", state: "thinking" });
    calls.length = 0;

    controller.lost();
    expect(shown()).toBe("reconnecting");
    expect(controller.state.notice).toBe(UNREACHABLE_NOTICE);
    expect(calls).toEqual(["flush"]);

    controller.opened();
    expect(calls).toEqual(["flush", INPUT_MIC, AUDIO_ON]);
    expect(shown()).toBe("starting");
    expect(controller.state.notice).toBeNull();
  });

  it("does not send voice on after a reconnect when voice is off", () => {
    const { controller, calls } = online();
    controller.lost();
    controller.opened();
    expect(calls).toEqual(["flush"]);
  });

  it("shows server errors until dismissed or the next user turn", () => {
    const { controller, server } = online();
    server({ type: "error", message: "The voice service failed." });
    expect(controller.state.notice).toBe("The voice service failed.");
    controller.dismissNotice();
    expect(controller.state.notice).toBeNull();
    server({ type: "error", message: "The voice service failed." });
    server({ type: "user", text: "hello" });
    expect(controller.state.notice).toBeNull();
  });

  it("keeps voice off and tells the user when the mic cannot start or stops, leaving reply audio as it was", () => {
    const { controller, calls } = online();
    controller.notify("No microphone was found.");
    expect(controller.state.voice).toBe(false);
    expect(controller.state.notice).toBe("No microphone was found.");

    controller.setVoice(true);
    controller.micStopped("The microphone stopped working.");
    expect(controller.state).toMatchObject({ voice: false, speak: true });
    expect(controller.state.notice).toBe("The microphone stopped working.");
    expect(calls).toEqual([INPUT_MIC, AUDIO_ON, INPUT_TEXT, "release"]);
  });

  const SPEAK_OFF = ["flush", "release"];
  it.each([
    ["Voice on speaks replies too", (c: Controller) => { c.setVoice(true); }, [INPUT_MIC, AUDIO_ON], true, true],
    [
      "Voice on, Speak replies off: speaking, read replies",
      (c: Controller) => { c.setVoice(true); c.setSpeak(false); },
      [INPUT_MIC, AUDIO_ON, AUDIO_OFF, ...SPEAK_OFF],
      true,
      false,
    ],
    ["typing with Speak replies on", (c: Controller) => { c.setSpeak(true); }, [AUDIO_ON], false, true],
    [
      "Voice off also turns Speak replies off",
      (c: Controller) => { c.setVoice(true); c.setVoice(false); },
      [INPUT_MIC, AUDIO_ON, INPUT_TEXT, AUDIO_OFF, "flush", "release"],
      false,
      false,
    ],
    [
      "Voice off after Speak replies was turned off stays off",
      (c: Controller) => { c.setVoice(true); c.setSpeak(false); c.setVoice(false); },
      [INPUT_MIC, AUDIO_ON, AUDIO_OFF, ...SPEAK_OFF, INPUT_TEXT, AUDIO_OFF, "flush", "release"],
      false,
      false,
    ],
    [
      "Voice on turns Speak replies back on",
      (c: Controller) => { c.setVoice(true); c.setSpeak(false); c.setVoice(false); c.setVoice(true); },
      [INPUT_MIC, AUDIO_ON, AUDIO_OFF, ...SPEAK_OFF, INPUT_TEXT, AUDIO_OFF, "flush", "release", INPUT_MIC, AUDIO_ON],
      true,
      true,
    ],
    [
      "Speak replies on, then off, with typing",
      (c: Controller) => { c.setSpeak(true); c.setSpeak(false); },
      [AUDIO_ON, AUDIO_OFF, ...SPEAK_OFF],
      false,
      false,
    ],
  ] as const)("%s", (_name, act, calls, voice, speak) => {
    const t = online();
    act(t.controller);
    expect(t.calls).toEqual(calls);
    expect(t.controller.state).toMatchObject({ voice, speak });
  });

  it("turning Speak replies off mid-reply drops the queued audio and keeps the text", () => {
    const { controller, calls, server, shown } = online();
    controller.setSpeak(true);
    server({ type: "reply_audio", on: true });
    server({ type: "user", text: "tell me a story" });
    server({ type: "reply", text: "Once upon" });
    controller.playing(true);
    expect(shown()).toBe("speaking");
    calls.length = 0;

    controller.setSpeak(false);
    controller.playing(false);
    server({ type: "reply_audio", on: false });
    server({ type: "reply", text: " a time." });
    expect(calls).toEqual([AUDIO_OFF, "flush", "release", 'send {"type":"played","bytes":0}']);
    expect(controller.state.turns.at(-1)).toEqual({ role: "assistant", text: "Once upon a time." });
    expect(controller.state.replyAudio).toBe(false);
  });

  it("shows Speak replies as on only once the server confirms, and asks again on a new connection", () => {
    const { controller, calls, server } = online();
    controller.setSpeak(true);
    expect(controller.state).toMatchObject({ speak: true, replyAudio: false });
    server({ type: "reply_audio", on: true });
    expect(controller.state.replyAudio).toBe(true);

    controller.lost();
    controller.opened();
    expect(controller.state).toMatchObject({ speak: true, replyAudio: false });
    expect(calls).toEqual([AUDIO_ON, "flush", AUDIO_ON]);
    server({ type: "reply_audio", on: true });
    expect(controller.state.replyAudio).toBe(true);
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
