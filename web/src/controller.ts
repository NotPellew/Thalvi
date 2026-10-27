import {
  CANCEL_MESSAGE,
  MAX_TEXT_LENGTH,
  parseServerMessage,
  playedMessage,
  textMessage,
  voiceMessage,
} from "./protocol.ts";
import { INITIAL, reduce, type SessionEvent, type SessionState } from "./session.ts";

export interface ControllerPorts {
  send(text: string): void;
  playback: { enqueue(bytes: ArrayBuffer): void; flush(): void };
  remember(sessionId: string): void;
  /** Drops the remembered id, so a reload starts a new conversation instead of taking this one back. */
  forget(): void;
  /** Stops reconnecting: the server ended this connection for good. */
  stopConnection(): void;
  /** Drops the current connection and joins conversation `id`, or starts a new one for null. */
  connect(id: string | null): void;
  /** Stops the mic and audio context; the server has turned voice off by itself. */
  releaseVoice(): void;
  onChange(state: SessionState): void;
}

export class Controller {
  state: SessionState = INITIAL;
  private readonly ports: ControllerPorts;
  private receivedAudioBytes = 0;

  constructor(ports: ControllerPorts) {
    this.ports = ports;
  }

  opened(): void {
    this.receivedAudioBytes = 0;
    this.apply({ type: "opened" });
    if (this.state.voice) this.ports.send(voiceMessage(true));
  }

  lost(): void {
    this.ports.playback.flush();
    this.apply({ type: "lost" });
  }

  /** `notice` explains why voice went off by itself; null for the user's own choice. */
  setVoice(on: boolean, notice: string | null = null): void {
    this.ports.send(voiceMessage(on));
    if (!on) this.ports.playback.flush();
    this.apply({ type: "voice", on, notice });
  }

  notify(notice: string): void {
    this.apply({ type: "notice", notice });
  }

  /** Returns whether the text was sent, so the caller knows to clear the composer. */
  sendText(raw: string): boolean {
    const text = raw.trim();
    if (text === "") return false;
    if (text.length > MAX_TEXT_LENGTH) {
      this.notify(`That message is too long. The limit is ${String(MAX_TEXT_LENGTH)} characters.`);
      return false;
    }
    this.ports.playback.flush();
    this.ports.send(textMessage(text));
    return true;
  }

  stop(): void {
    this.ports.send(CANCEL_MESSAGE);
    this.ports.playback.flush();
    this.apply({ type: "stopped" });
  }

  /** Leaves the current conversation for `id`, or a new one for null; voice stays as it is. */
  switchTo(id: string | null): void {
    this.ports.playback.flush();
    this.ports.connect(id);
    this.apply({ type: "switched" });
  }

  playing(playing: boolean): void {
    if (!playing) this.ports.send(playedMessage(this.receivedAudioBytes));
    this.apply({ type: "playing", playing });
  }

  message(data: unknown): void {
    if (data instanceof ArrayBuffer) {
      this.receivedAudioBytes += data.byteLength;
      this.ports.playback.enqueue(data);
      return;
    }
    if (typeof data !== "string") return;
    const message = parseServerMessage(data);
    if (!message) return;
    if (message.type === "interrupted") this.ports.playback.flush();
    if (message.type === "session") this.ports.remember(message.id);
    if (message.type === "ended") {
      this.ports.playback.flush();
      if (this.state.voice) this.ports.releaseVoice();
      this.ports.forget();
      this.ports.stopConnection();
    }
    if (message.type === "voice") {
      if (!this.state.voice) return;
      this.ports.playback.flush();
      this.ports.releaseVoice();
    }
    this.apply({ type: "server", message });
  }

  private apply(event: SessionEvent): void {
    this.state = reduce(this.state, event);
    this.ports.onChange(this.state);
  }
}
