import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Connection, reconnectDelay, sessionUrl, type Socket } from "./connection.ts";

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
  const events: string[] = [];
  const connection = () =>
    new Connection(
      "ws://x/session",
      {
        onOpen: () => events.push("open"),
        onLost: () => events.push("lost"),
        onMessage: (data) => events.push(`message ${String(data)}`),
      },
      () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    );

  beforeEach(() => {
    vi.useFakeTimers();
    sockets.length = 0;
    events.length = 0;
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
    ["https:", "wss://phone.ts.net/session"],
    ["http:", "ws://phone.ts.net/session"],
  ])("uses the page's scheme for %s", (protocol, url) => {
    expect(sessionUrl({ protocol, host: "phone.ts.net" })).toBe(url);
  });
});
