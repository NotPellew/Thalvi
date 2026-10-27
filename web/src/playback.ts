import { PcmDecoder } from "./pcm.ts";
import type { Speed } from "./speed.ts";
import playbackWorkletUrl from "./playback-worklet.ts?worker&url";

type Pending =
  | { kind: "push"; seq: number; samples: Float32Array<ArrayBuffer> }
  | { kind: "rate"; rate: Speed }
  | { kind: "flush"; seq: number };

interface PlayingMessage {
  type: "playing";
  playing: boolean;
  seq: number;
}

function isPlayingMessage(data: unknown): data is PlayingMessage {
  if (typeof data !== "object" || data === null) return false;
  const record = data as Record<string, unknown>;
  return (
    record["type"] === "playing" &&
    typeof record["playing"] === "boolean" &&
    typeof record["seq"] === "number"
  );
}

export class Playback {
  private readonly ctx: AudioContext;
  private readonly onPlaying: (playing: boolean) => void;
  private readonly decoder = new PcmDecoder();
  private node: AudioWorkletNode | null = null;
  private pending: Pending[] = [];
  private playingState = false;
  private latestSeq = 0;

  constructor(ctx: AudioContext, onPlaying: (playing: boolean) => void) {
    this.ctx = ctx;
    this.onPlaying = onPlaying;
    void ctx.audioWorklet
      .addModule(playbackWorkletUrl)
      .then(() => {
        if (ctx.state === "closed") {
          this.pending = [];
          return;
        }
        const node = new AudioWorkletNode(ctx, "playback-stretcher", {
          numberOfInputs: 0,
          numberOfOutputs: 1,
          outputChannelCount: [1],
        });
        node.port.onmessage = (event: MessageEvent<unknown>) => {
          if (!isPlayingMessage(event.data)) return;
          if (event.data.playing) return;
          if (event.data.seq !== this.latestSeq) return;
          this.emitPlaying(false);
        };
        node.connect(ctx.destination);
        this.node = node;
        const queued = this.pending;
        this.pending = [];
        for (const message of queued) this.deliver(message);
      })
      .catch(() => {
        this.pending = [];
        this.emitPlaying(false);
      });
  }

  enqueue(bytes: ArrayBuffer): void {
    const samples = this.decoder.decode(new Uint8Array(bytes));
    if (samples.length === 0) return;
    this.latestSeq += 1;
    this.deliver({ kind: "push", seq: this.latestSeq, samples });
    this.emitPlaying(true);
  }

  flush(): void {
    this.decoder.reset();
    this.latestSeq += 1;
    const seq = this.latestSeq;
    if (this.node) {
      this.node.port.postMessage({ type: "flush", seq });
    } else {
      this.pending = this.pending.filter((message) => message.kind === "rate");
      this.pending.push({ kind: "flush", seq });
    }
    this.emitPlaying(false);
  }

  setRate(rate: Speed): void {
    this.deliver({ kind: "rate", rate });
  }

  private deliver(message: Pending): void {
    if (!this.node) {
      this.pending.push(message);
      return;
    }
    if (message.kind === "push") {
      this.node.port.postMessage({ type: "push", seq: message.seq, samples: message.samples }, [
        message.samples.buffer,
      ]);
    } else if (message.kind === "rate") {
      this.node.port.postMessage({ type: "rate", rate: message.rate });
    } else {
      this.node.port.postMessage({ type: "flush", seq: message.seq });
    }
  }

  private emitPlaying(playing: boolean): void {
    if (playing === this.playingState) return;
    this.playingState = playing;
    this.onPlaying(playing);
  }
}
