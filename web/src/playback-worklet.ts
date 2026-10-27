import { Resampler, WIRE_RATE } from "./pcm.ts";
import type { Speed } from "./speed.ts";
import { TimeStretcher } from "./stretch.ts";

declare const sampleRate: number;
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(
  name: string,
  processor: new () => AudioWorkletProcessor & {
    process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean;
  },
): void;

type ToWorklet =
  | { type: "push"; seq: number; samples: Float32Array }
  | { type: "rate"; rate: Speed }
  | { type: "flush"; seq: number };

class PlaybackProcessor extends AudioWorkletProcessor {
  private readonly stretcher = new TimeStretcher();
  private readonly resampler = new Resampler(WIRE_RATE, sampleRate);
  private rate: Speed = 1;
  private queue: Float32Array[] = [];
  private queueOffset = 0;
  private playing = false;
  private seq = 0;

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<ToWorklet>) => {
      const message = event.data;
      if (message.type === "push") {
        this.seq = message.seq;
        this.stretcher.push(message.samples);
        this.playing = true;
      } else if (message.type === "rate") {
        this.rate = message.rate;
      } else {
        this.seq = message.seq;
        this.stretcher.reset();
        this.resampler.reset();
        this.queue = [];
        this.queueOffset = 0;
        if (this.playing) {
          this.playing = false;
          this.port.postMessage({ type: "playing", playing: false, seq: this.seq });
        }
      }
    };
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const channel = outputs[0]?.[0];
    if (!channel) return true;
    let written = 0;
    while (written < channel.length) {
      const head = this.queue[0];
      if (head) {
        const available = head.length - this.queueOffset;
        const take = Math.min(available, channel.length - written);
        channel.set(head.subarray(this.queueOffset, this.queueOffset + take), written);
        written += take;
        this.queueOffset += take;
        if (this.queueOffset >= head.length) {
          this.queue.shift();
          this.queueOffset = 0;
        }
        continue;
      }
      const stretched = this.stretcher.pull(this.rate, 128);
      if (stretched.length > 0) {
        const resampled = this.resampler.process(stretched);
        if (resampled.length > 0) this.queue.push(resampled);
        continue;
      }
      const tail = this.stretcher.finish(this.rate);
      if (tail.length > 0) {
        const resampled = this.resampler.process(tail);
        if (resampled.length > 0) this.queue.push(resampled);
        continue;
      }
      this.resampler.reset();
      if (this.playing) {
        this.playing = false;
        this.port.postMessage({ type: "playing", playing: false, seq: this.seq });
      }
      break;
    }
    for (let i = written; i < channel.length; i++) channel[i] = 0;
    return true;
  }
}

registerProcessor("playback-stretcher", PlaybackProcessor);
