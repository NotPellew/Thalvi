import { CANCEL_MESSAGE, parseServerMessage } from "./protocol.ts";
import { reduce, IDLE, type SessionEvent, type SessionState } from "./session.ts";

export interface ControllerPorts {
  send(text: string): void;
  playback: { enqueue(bytes: ArrayBuffer): void; flush(): void };
  onChange(state: SessionState): void;
}

export class Controller {
  state: SessionState = IDLE;
  private readonly ports: ControllerPorts;

  constructor(ports: ControllerPorts) {
    this.ports = ports;
  }

  begin(): void {
    this.apply({ type: "begin" });
  }

  end(): void {
    this.ports.playback.flush();
    this.apply({ type: "end" });
  }

  failed(notice: string): void {
    this.ports.playback.flush();
    this.apply({ type: "failed", notice });
  }

  opened(): void {
    this.apply({ type: "opened" });
  }

  lost(): void {
    this.ports.playback.flush();
    this.apply({ type: "lost" });
  }

  stop(): void {
    this.ports.send(CANCEL_MESSAGE);
    this.ports.playback.flush();
    this.apply({ type: "stopped" });
  }

  playing(playing: boolean): void {
    this.apply({ type: "playing", playing });
  }

  message(data: unknown): void {
    if (data instanceof ArrayBuffer) {
      this.ports.playback.enqueue(data);
      return;
    }
    if (typeof data !== "string") return;
    const message = parseServerMessage(data);
    if (!message) return;
    if (message.type === "interrupted") this.ports.playback.flush();
    this.apply({ type: "server", message });
  }

  private apply(event: SessionEvent): void {
    this.state = reduce(this.state, event);
    this.ports.onChange(this.state);
  }
}
