import {
  CANCEL_MESSAGE,
  MAX_TEXT_LENGTH,
  parseServerMessage,
  inputMessage,
  playedMessage,
  replyAudioMessage,
  textMessage,
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
  /** Stops the mic and the audio context that neither Voice nor Speak replies needs any more. */
  releaseUnused(): void;
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
    if (this.state.voice) this.ports.send(inputMessage("mic"));
    if (this.state.speak) this.ports.send(replyAudioMessage(true));
  }

  lost(): void {
    this.ports.playback.flush();
    this.apply({ type: "lost" });
  }

  /** The one-click Voice control: microphone input and spoken replies together. */
  setVoice(on: boolean): void {
    this.ports.send(inputMessage(on ? "mic" : "text"));
    this.ports.send(replyAudioMessage(on));
    if (!on) this.ports.playback.flush();
    this.apply({ type: "voice", on, speak: on, notice: null });
    if (!on) this.ports.releaseUnused();
  }

  /** Spoken replies on or off, whatever the input; off drops the queued reply audio. */
  setSpeak(on: boolean): void {
    this.ports.send(replyAudioMessage(on));
    if (!on) this.ports.playback.flush();
    this.apply({ type: "speak", on });
    if (!on) this.ports.releaseUnused();
  }

  /** The microphone failed: falls back to text input and leaves spoken replies as they were. */
  micStopped(notice: string): void {
    this.ports.send(inputMessage("text"));
    this.apply({ type: "voice", on: false, speak: this.state.speak, notice });
    this.ports.releaseUnused();
  }

  notify(notice: string): void {
    this.apply({ type: "notice", notice });
  }

  dismissNotice(): void {
    this.apply({ type: "notice", notice: null });
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
      this.apply({ type: "server", message });
      this.ports.releaseUnused();
      this.ports.forget();
      this.ports.stopConnection();
      return;
    }
    if (message.type === "input" && message.source === "text") {
      if (!this.state.voice) return;
      this.apply({ type: "server", message });
      this.ports.releaseUnused();
      return;
    }
    this.apply({ type: "server", message });
  }

  private apply(event: SessionEvent): void {
    this.state = reduce(this.state, event);
    this.ports.onChange(this.state);
  }
}
