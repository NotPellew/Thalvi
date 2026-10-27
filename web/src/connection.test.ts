import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Connection, reconnectDelay, sessionUrl, type Socket } from "./connection.ts";
import { Controller } from "./controller.ts";

class FakeSocket implements Socket {
  binaryType: BinaryType = "blob";
  readyState = 0;
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  sent: unknown[] = [];
  send(data: string | ArrayBuffer): void {
    this.sent.push(data);
  }
  close(): void {
    this.readyState = 3;
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.(new Event("open"));
  }
  drop(): void {
    this.readyState = 3;
    this.onclose?.(new CloseEvent("close"));
  }
}

describe("Connection", () => {
  const sockets: FakeSocket[] = [];
  const urls: string[] = [];
  const events: string[] = [];
  let resumeId: string | null = null;
  const connection = () =>
    new Connection(
      () => sessionUrl({ protocol: "http:", host: "x" }, resumeId),
      {
        onOpen: () => events.push("open"),
        onLost: () => events.push("lost"),
        onMessage: (data) => events.push(`message ${String(data)}`),
      },
      (url) => {
        urls.push(url);
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    );

  beforeEach(() => {
    vi.useFakeTimers();
    sockets.length = 0;
    urls.length = 0;
    events.length = 0;
    resumeId = null;
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("retries after 0.5 s doubling to 5 s, and starts over once connected", () => {
    const conn = connection();
    conn.connect();
    const delays: number[] = [];
    for (let i = 0; i < 6; i++) {
      const count = sockets.length;
      sockets.at(-1)?.drop();
      let waited = 0;
      while (sockets.length === count) {
        vi.advanceTimersByTime(100);
        waited += 100;
      }
      delays.push(waited);
    }
    expect(delays).toEqual([500, 1000, 2000, 4000, 5000, 5000]);

    sockets.at(-1)?.open();
    sockets.at(-1)?.drop();
    const count = sockets.length;
    vi.advanceTimersByTime(499);
    expect(sockets).toHaveLength(count);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(count + 1);
  });

  it("sends only while open, and close() stops reconnecting", () => {
    const conn = connection();
    conn.connect();
    const socket = sockets[0];
    conn.send("early");
    socket?.open();
    conn.send("later");
    expect(socket?.sent).toEqual(["later"]);
    expect(socket?.binaryType).toBe("arraybuffer");

    socket?.drop();
    conn.send("while down");
    conn.close();
    vi.advanceTimersByTime(10_000);
    expect(sockets).toHaveLength(1);
    expect(events).toEqual(["open", "lost"]);
  });

  it("rejoins with the latest session id on every connect", () => {
    const conn = connection();
    conn.connect();
    resumeId = "s1";
    sockets[0]?.drop();
    vi.advanceTimersByTime(500);
    expect(urls).toEqual(["ws://x/session", "ws://x/session?resume=s1"]);
  });

  it("does not report a loss when the user closes the session", () => {
    const conn = connection();
    conn.connect();
    sockets[0]?.open();
    conn.close();
    sockets[0]?.drop();
    expect(events).toEqual(["open"]);
  });

  it("passes server data through", () => {
    connection().connect();
    sockets[0]?.onmessage?.(new MessageEvent("message", { data: "hi" }));
    expect(events).toEqual(["message hi"]);
  });
});

describe("a conversation the server ended", () => {
  it("is not rejoined when the server closes the socket, and a replaced socket cannot speak for the new one", () => {
    vi.useFakeTimers();
    const sockets: FakeSocket[] = [];
    const urls: string[] = [];
    let resumeId: string | null = null;
    const remembered: string[] = [];
    const connection: Connection = new Connection(
      () => sessionUrl({ protocol: "http:", host: "x" }, resumeId),
      {
        onOpen: () => {
          controller.opened();
        },
        onLost: () => {
          controller.lost();
        },
        onMessage: (data) => {
          controller.message(data);
        },
      },
      (url) => {
        urls.push(url);
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    );
    const controller = new Controller({
      send: () => undefined,
      playback: { enqueue: () => undefined, flush: () => undefined },
      remember: (id) => remembered.push(id),
      forget: () => {
        resumeId = null;
      },
      stopConnection: () => {
        connection.close();
      },
      connect: (id) => {
        resumeId = id;
        connection.close();
        connection.connect();
      },
      releaseVoice: () => undefined,
      onChange: () => undefined,
    });
    const deliver = (socket: FakeSocket | undefined, message: object) => {
      socket?.onmessage?.(new MessageEvent("message", { data: JSON.stringify(message) }));
    };

    connection.connect();
    sockets[0]?.open();
    deliver(sockets[0], { type: "ended", reason: "moved", message: "This conversation continued on another device." });
    sockets[0]?.drop();
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1);
    expect(controller.state.link).toBe("ended");

    controller.switchTo("s2");
    expect(urls).toEqual(["ws://x/session", "ws://x/session?resume=s2"]);
    sockets[1]?.open();
    deliver(sockets[1], { type: "session", id: "s2", turns: [] });
    controller.switchTo(null);
    deliver(sockets[1], { type: "ended", reason: "deleted", message: "This conversation was deleted." });
    sockets[1]?.drop();
    vi.advanceTimersByTime(60_000);
    expect(urls).toEqual(["ws://x/session", "ws://x/session?resume=s2", "ws://x/session"]);
    expect(controller.state.link).toBe("connecting");
    expect(remembered).toEqual(["s2"]);
    vi.useRealTimers();
  });
});

describe("reconnectDelay", () => {
  it.each([
    [0, 500],
    [3, 4000],
    [4, 5000],
    [30, 5000],
  ])("failure %i waits %i ms", (failures, ms) => {
    expect(reconnectDelay(failures)).toBe(ms);
  });
});

describe("sessionUrl", () => {
  it.each([
    ["https:", null, "wss://phone.ts.net/session"],
    ["http:", null, "ws://phone.ts.net/session"],
    ["https:", "abc-123", "wss://phone.ts.net/session?resume=abc-123"],
    ["https:", "a b&c", "wss://phone.ts.net/session?resume=a%20b%26c"],
  ])("for %s and resume id %s", (protocol, resumeId, url) => {
    expect(sessionUrl({ protocol, host: "phone.ts.net" }, resumeId)).toBe(url);
  });
});
