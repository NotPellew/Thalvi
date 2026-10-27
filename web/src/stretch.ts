//! Streaming WSOLA time-stretcher for 24 kHz mono speech.

const WINDOW = 512;
const HOP = 128;
const OVERLAP = WINDOW - HOP;
const TOLERANCE = 64;

const WINDOW_TABLE = new Float32Array(WINDOW);
for (let i = 0; i < WINDOW; i++) WINDOW_TABLE[i] = 0.5 * (1 - Math.cos((2 * Math.PI * i) / WINDOW));

function checkRate(rate: number): void {
  if (!Number.isFinite(rate) || rate <= 0) throw new RangeError(`invalid playback rate: ${String(rate)}`);
}

export class TimeStretcher {
  private buffer = new Float32Array(8192);
  private length = 0;
  private base = 0;
  private analysisPos = 0;
  private nominalPos = 0;
  private readPos = 0;
  private prevAnalysis: number | null = null;
  private active = false;
  private finished = false;
  private readonly ola = new Float32Array(WINDOW);
  private readonly olaWin = new Float32Array(WINDOW);
  private outChunks: Float32Array[] = [];
  private outOffset = 0;
  private totalPushed = 0;
  private produced = 0;

  get pendingInputSamples(): number {
    const cursor = this.active ? Math.floor(this.nominalPos) : this.readPos;
    return Math.max(0, this.base + this.length - cursor);
  }

  push(input: Float32Array): void {
    if (input.length === 0) return;
    this.finished = false;
    this.compact();
    const needed = this.length + input.length;
    if (needed > this.buffer.length) {
      let capacity = this.buffer.length;
      while (capacity < needed) capacity *= 2;
      const next = new Float32Array(capacity);
      next.set(this.buffer.subarray(0, this.length));
      this.buffer = next;
    }
    this.buffer.set(input, this.length);
    this.length += input.length;
    this.totalPushed += input.length;
  }

  pull(rate: number, maxSamples = Number.POSITIVE_INFINITY): Float32Array {
    checkRate(rate);
    if (maxSamples <= 0) return new Float32Array(0);
    while (this.queuedSamples() < maxSamples) {
      if (rate === 1 && !this.active) {
        const available = this.base + this.length - this.readPos;
        if (available <= 0) break;
        const take = Math.min(available, maxSamples - this.queuedSamples());
        const start = this.readPos - this.base;
        const chunk = this.buffer.slice(start, start + take);
        this.readPos += take;
        this.analysisPos = this.readPos;
        this.nominalPos = this.readPos;
        this.enqueueOut(chunk);
      } else {
        const frame = this.nextFrame(rate);
        if (!frame) break;
        this.enqueueOut(frame);
      }
    }
    return this.dequeue(maxSamples);
  }

  finish(rate: number): Float32Array {
    checkRate(rate);
    this.finished = true;
    const emittedBefore = this.produced - this.queuedSamples();
    const expected = Math.round(this.totalPushed / rate);
    const parts: Float32Array[] = [];
    let length = 0;
    for (;;) {
      const chunk = this.pull(rate);
      if (chunk.length === 0) break;
      parts.push(chunk);
      length += chunk.length;
      if (emittedBefore + length >= expected) break;
    }
    const wanted = Math.max(0, expected - emittedBefore);
    if (length < wanted && this.active) {
      const tail = new Float32Array(OVERLAP);
      for (let i = 0; i < OVERLAP; i++) {
        const w = this.olaWin[i] ?? 0;
        tail[i] = w > 1e-6 ? (this.ola[i] ?? 0) / w : 0;
      }
      const take = Math.min(tail.length, wanted - length);
      if (take > 0) {
        parts.push(tail.subarray(0, take));
        length += take;
      }
    }
    const all = new Float32Array(Math.min(length, wanted));
    let at = 0;
    for (const part of parts) {
      if (at >= all.length) break;
      const take = Math.min(part.length, all.length - at);
      all.set(part.subarray(0, take), at);
      at += take;
    }
    this.reset();
    return all;
  }

  reset(): void {
    this.length = 0;
    this.base = 0;
    this.analysisPos = 0;
    this.nominalPos = 0;
    this.readPos = 0;
    this.prevAnalysis = null;
    this.active = false;
    this.finished = false;
    this.ola.fill(0);
    this.olaWin.fill(0);
    this.outChunks = [];
    this.outOffset = 0;
    this.totalPushed = 0;
    this.produced = 0;
  }

  private get end(): number {
    return this.base + this.length;
  }

  private compact(): void {
    const cursor = this.active ? Math.floor(this.nominalPos) - TOLERANCE - WINDOW : this.readPos;
    const drop = Math.min(Math.max(0, cursor - this.base), this.length);
    if (drop <= 0) return;
    this.buffer.copyWithin(0, drop, this.length);
    this.length -= drop;
    this.base += drop;
  }

  private at(absIndex: number): number {
    const i = absIndex - this.base;
    if (i < 0 || i >= this.length) return 0;
    return this.buffer[i] ?? 0;
  }

  private queuedSamples(): number {
    let total = -this.outOffset;
    for (const chunk of this.outChunks) total += chunk.length;
    return total;
  }

  private enqueueOut(chunk: Float32Array): void {
    this.outChunks.push(chunk);
    this.produced += chunk.length;
  }

  private dequeue(maxSamples: number): Float32Array {
    const take = Math.min(maxSamples, this.queuedSamples());
    if (take <= 0) return new Float32Array(0);
    const out = new Float32Array(take);
    let written = 0;
    while (written < take) {
      const chunk = this.outChunks[0];
      if (!chunk) break;
      const available = chunk.length - this.outOffset;
      const n = Math.min(available, take - written);
      out.set(chunk.subarray(this.outOffset, this.outOffset + n), written);
      written += n;
      this.outOffset += n;
      if (this.outOffset >= chunk.length) {
        this.outChunks.shift();
        this.outOffset = 0;
      }
    }
    return out;
  }

  private nextFrame(rate: number): Float32Array | null {
    if (!this.active) this.nominalPos = this.readPos;
    const nominal = Math.round(this.nominalPos);
    if (this.finished && nominal >= this.end) return null;
    if (!this.finished && this.end < nominal + TOLERANCE + WINDOW) return null;
    let candidate = nominal;
    if (this.prevAnalysis !== null) {
      const targetStart = this.prevAnalysis + HOP;
      let targetEnergy = 0;
      for (let i = 0; i < OVERLAP; i++) {
        const v = this.at(targetStart + i);
        targetEnergy += v * v;
      }
      if (targetEnergy > 1e-12) {
        const scores = new Float32Array(2 * TOLERANCE + 1);
        let bestScore = Number.NEGATIVE_INFINITY;
        for (let delta = -TOLERANCE; delta <= TOLERANCE; delta++) {
          const start = nominal + delta;
          let dot = 0;
          let energy = 0;
          for (let i = 0; i < OVERLAP; i++) {
            const v = this.at(start + i);
            dot += v * this.at(targetStart + i);
            energy += v * v;
          }
          const score = dot / Math.sqrt(targetEnergy * energy + 1e-12);
          scores[delta + TOLERANCE] = score;
          if (score > bestScore) bestScore = score;
        }
        let bestAbs = Number.POSITIVE_INFINITY;
        for (let delta = -TOLERANCE; delta <= TOLERANCE; delta++) {
          if ((scores[delta + TOLERANCE] ?? 0) >= bestScore - 1e-3 && Math.abs(delta) < bestAbs) {
            bestAbs = Math.abs(delta);
            candidate = nominal + delta;
          }
        }
      }
    }
    for (let i = 0; i < WINDOW; i++) {
      const w = WINDOW_TABLE[i] ?? 0;
      this.ola[i] = (this.ola[i] ?? 0) + this.at(candidate + i) * w;
      this.olaWin[i] = (this.olaWin[i] ?? 0) + w;
    }
    const out = new Float32Array(HOP);
    for (let i = 0; i < HOP; i++) {
      const w = this.olaWin[i] ?? 0;
      out[i] = w > 1e-6 ? (this.ola[i] ?? 0) / w : 0;
    }
    this.ola.copyWithin(0, HOP);
    this.olaWin.copyWithin(0, HOP);
    this.ola.fill(0, OVERLAP);
    this.olaWin.fill(0, OVERLAP);
    this.prevAnalysis = candidate;
    this.nominalPos = nominal + HOP * rate;
    this.analysisPos = candidate;
    this.readPos = Math.min(Math.round(this.nominalPos), this.end);
    this.active = true;
    return out;
  }
}
