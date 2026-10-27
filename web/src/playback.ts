import { PcmDecoder, WIRE_RATE } from "./pcm.ts";

const START_LEAD_SECONDS = 0.02;

export class Playback {
  private readonly ctx: AudioContext;
  private readonly onPlaying: (playing: boolean) => void;
  private readonly decoder = new PcmDecoder();
  private readonly sources = new Set<AudioBufferSourceNode>();
  private nextStart = 0;

  constructor(ctx: AudioContext, onPlaying: (playing: boolean) => void) {
    this.ctx = ctx;
    this.onPlaying = onPlaying;
  }

  enqueue(bytes: ArrayBuffer): void {
    const samples = this.decoder.decode(new Uint8Array(bytes));
    if (samples.length === 0) return;
    const buffer = this.ctx.createBuffer(1, samples.length, WIRE_RATE);
    buffer.copyToChannel(samples, 0);
    const source = this.ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(this.ctx.destination);
    source.onended = () => {
      this.sources.delete(source);
      if (this.sources.size === 0) this.onPlaying(false);
    };
    const at = Math.max(this.nextStart, this.ctx.currentTime + START_LEAD_SECONDS);
    source.start(at);
    this.nextStart = at + buffer.duration;
    this.sources.add(source);
    if (this.sources.size === 1) this.onPlaying(true);
  }

  flush(): void {
    this.decoder.reset();
    this.nextStart = 0;
    if (this.sources.size === 0) return;
    for (const source of this.sources) {
      source.onended = null;
      source.stop();
    }
    this.sources.clear();
    this.onPlaying(false);
  }
}
