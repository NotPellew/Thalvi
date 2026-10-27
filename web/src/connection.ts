export interface Socket {
  binaryType: BinaryType;
  readyState: number;
  onopen: ((event: Event) => void) | null;
  onmessage: ((event: MessageEvent) => void) | null;
  onclose: ((event: CloseEvent) => void) | null;
  send(data: string | ArrayBuffer): void;
  close(): void;
}

export interface ConnectionHandlers {
  onOpen(): void;
  onLost(): void;
  onMessage(data: unknown): void;
}

const OPEN = 1;

export function reconnectDelay(failures: number): number {
  return Math.min(500 * 2 ** failures, 5000);
}

export function sessionUrl(
  location: Pick<Location, "protocol" | "host">,
  resumeId: string | null,
): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const query = resumeId === null ? "" : `?resume=${encodeURIComponent(resumeId)}`;
  return `${scheme}://${location.host}/session${query}`;
}

export class Connection {
  private socket: Socket | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private failures = 0;
  private readonly url: () => string;
  private readonly handlers: ConnectionHandlers;
  private readonly openSocket: (url: string) => Socket;

  constructor(
    url: () => string,
    handlers: ConnectionHandlers,
    openSocket: (url: string) => Socket = (u) => new WebSocket(u),
  ) {
    this.url = url;
    this.handlers = handlers;
    this.openSocket = openSocket;
  }

  connect(): void {
    this.timer = null;
    const socket = this.openSocket(this.url());
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      this.failures = 0;
      this.handlers.onOpen();
    };
    socket.onmessage = (event) => {
      const data: unknown = event.data;
      this.handlers.onMessage(data);
    };
    socket.onclose = () => {
      this.socket = null;
      this.handlers.onLost();
      this.timer = setTimeout(() => {
        this.connect();
      }, reconnectDelay(this.failures++));
    };
    this.socket = socket;
  }

  /** Drops the data when the socket is not open: stale audio is worse than none. */
  send(data: string | ArrayBuffer): void {
    if (this.socket?.readyState === OPEN) this.socket.send(data);
  }

  close(): void {
    if (this.timer) clearTimeout(this.timer);
    if (this.socket) {
      this.socket.onclose = null;
      this.socket.close();
    }
  }
}
