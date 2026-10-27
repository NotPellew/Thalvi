import { WIRE_RATE } from "./pcm.ts";
import { normalizeSpeed } from "./speed.ts";
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
  | { type: "push"; samples: Float32Array }
  | { type: "rate"; rate: number }
  | { type: "flush" };

class OutputResampler {
  private readonly step = WIRE_RATE / sampleRate;
  private pos = 1;
  private prev = 0;

  process(input: Float32Array): Float32Array {
    const n = input.length;
    if (n === 0) return new Float32Array(0);
    const out = new Float32Array(Math.max(0, Math.ceil((n - this.pos) / this.step) + 1));
    const at = (i: number) => (i === 0 ? this.prev : (input[i - 1] ?? 0));
    let count = 0;
    while (this.pos < n) {
      const i = Math.floor(this.pos);
      const a = at(i);
      out[count++] = a + (at(i + 1) - a) * (this.pos - i);
      this.pos += this.step;
    }
    this.pos -= n;
    this.prev = input[n - 1] ?? this.prev;
    return out.subarray(0, count);
  }

  reset(): void {
    this.pos = 1;
    this.prev = 0;
  }
}

class PlaybackProcessor extends AudioWorkletProcessor {
  private readonly stretcher = new TimeStretcher();
  private readonly resampler = new OutputResampler();
  private rate = 1;
  private queue: Float32Array[] = [];
  private queueOffset = 0;
  private playing = false;
  private starved = 0;

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<ToWorklet>) => {
      const message = event.data;
      if (message.type === "push") {
        this.stretcher.push(message.samples);
        if (!this.playing) {
          this.playing = true;
          this.port.postMessage({ type: "playing", playing: true });
        }
      } else if (message.type === "rate") {
        this.rate = normalizeSpeed(message.rate);
      } else {
        this.stretcher.reset();
        this.resampler.reset();
        this.queue = [];
        this.queueOffset = 0;
        this.starved = 0;
        if (this.playing) {
          this.playing = false;
          this.port.postMessage({ type: "playing", playing: false });
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
        this.starved = 0;
        continue;
      }
      const stretched = this.stretcher.pull(this.rate, 1024);
      if (stretched.length > 0) {
        const resampled = this.resampler.process(stretched);
        if (resampled.length > 0) this.queue.push(resampled.slice());
        this.starved = 0;
        continue;
      }
      if (this.stretcher.pendingInputSamples > 0) {
        this.starved += 1;
        if (this.starved > 40) {
          this.starved = 0;
          const tail = this.stretcher.finish(this.rate);
          if (tail.length > 0) {
            const resampled = this.resampler.process(tail);
            if (resampled.length > 0) this.queue.push(resampled.slice());
            continue;
          }
          if (this.playing) {
            this.playing = false;
            this.port.postMessage({ type: "playing", playing: false });
          }
        }
        break;
      }
      if (this.playing) {
        this.playing = false;
        this.port.postMessage({ type: "playing", playing: false });
      }
      break;
    }
    for (let i = written; i < channel.length; i++) channel[i] = 0;
    return true;
  }
}

registerProcessor("playback-stretcher", PlaybackProcessor);
