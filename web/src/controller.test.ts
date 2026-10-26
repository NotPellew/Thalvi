import { describe, expect, it } from "vitest";
import { Controller } from "./controller.ts";
import { displayState, UNREACHABLE_NOTICE } from "./session.ts";

function setup() {
  const calls: string[] = [];
  const controller = new Controller({
    send: (text) => calls.push(`send ${text}`),
    playback: {
      enqueue: (bytes) => calls.push(`enqueue ${String(bytes.byteLength)}`),
      flush: () => calls.push("flush"),
    },
    onChange: () => undefined,
  });
  const server = (message: object) => {
    controller.message(JSON.stringify(message));
  };
  const shown = () => displayState(controller.state);
  return { controller, calls, server, shown };
}

describe("Controller", () => {
  it("shows a full turn: transcript, reply text, and Speaking until local audio ends", () => {
    const { controller, calls, server, shown } = setup();
    controller.begin();
    expect(shown()).toBe("connecting");
    controller.opened();
    expect(shown()).toBe("listening");

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

  it("drops queued audio on interrupted and starts a new reply afterwards", () => {
    const { controller, calls, server, shown } = setup();
    controller.begin();
    controller.opened();
    server({ type: "user", text: "tell me a story" });
    server({ type: "state", state: "speaking" });
    server({ type: "reply", text: "Once upon" });
    controller.playing(true);

    server({ type: "interrupted" });
    controller.playing(false);
    expect(calls).toEqual(["flush"]);
    expect(shown()).toBe("listening");

    server({ type: "reply", text: "Sure." });
    expect(controller.state.turns.map((t) => t.text)).toEqual(["tell me a story", "Once upon", "Sure."]);
  });

  it("Stop sends cancel and drops local audio at once", () => {
    const { controller, calls } = setup();
    controller.begin();
    controller.opened();
    controller.stop();
    expect(calls).toEqual(['send {"type":"cancel"}', "flush"]);
  });

  it("shows Reconnecting with a notice when the link drops, then resumes", () => {
    const { controller, calls, server, shown } = setup();
    controller.begin();
    controller.opened();
    server({ type: "state", state: "speaking" });

    controller.lost();
    expect(shown()).toBe("reconnecting");
    expect(controller.state.notice).toBe(UNREACHABLE_NOTICE);
    expect(calls).toEqual(["flush"]);

    controller.opened();
    expect(shown()).toBe("listening");
    expect(controller.state.notice).toBeNull();
  });

  it("shows server errors until the next user turn", () => {
    const { controller, server } = setup();
    controller.begin();
    controller.opened();
    server({ type: "error", message: "The voice service failed." });
    expect(controller.state.notice).toBe("The voice service failed.");
    server({ type: "user", text: "hello" });
    expect(controller.state.notice).toBeNull();
  });

  it("returns to the start screen with the failure when the mic cannot start", () => {
    const { controller } = setup();
    controller.failed("No microphone was found.");
    expect(controller.state.screen).toBe("idle");
    expect(controller.state.notice).toBe("No microphone was found.");
  });

  it("ignores unknown and malformed messages", () => {
    const { controller, calls, server } = setup();
    controller.begin();
    controller.opened();
    const before = controller.state;
    controller.message("{broken");
    server({ type: "weather", text: "sunny" });
    server({ type: "reply", text: 5 });
    controller.message(42);
    expect(controller.state).toEqual(before);
    expect(calls).toEqual([]);
  });
});
