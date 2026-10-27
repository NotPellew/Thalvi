export const WIRE_RATE = 24000;
/** 80 ms at the wire rate. */
export const FRAME_SAMPLES = 1920;

export function floatToPcm16(input: Float32Array): Int16Array {
  const out = new Int16Array(input.length);
  for (let i = 0; i < input.length; i++) {
    const x = Math.max(-1, Math.min(1, input[i] ?? 0));
    out[i] = Math.round(x < 0 ? x * 0x8000 : x * 0x7fff);
  }
  return out;
}

/** Streaming linear-interpolation resampler between two rates. */
export class Resampler {
  private readonly step: number;
  private pos = 1;
  private prev = 0;

  constructor(inputRate: number, outputRate: number = WIRE_RATE) {
    this.step = inputRate / outputRate;
  }

  reset(): void {
    this.pos = 1;
    this.prev = 0;
  }

  process(input: Float32Array): Float32Array {
    const n = input.length;
    const out = new Float32Array(Math.max(0, Math.ceil((n - this.pos) / this.step)));
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
}

/** Turns arbitrary-rate mic blocks into fixed 80 ms wire frames. */
export class MicEncoder {
  private readonly resampler: Resampler;
  private frame = new Int16Array(FRAME_SAMPLES);
  private filled = 0;

  constructor(inputRate: number) {
    this.resampler = new Resampler(inputRate);
  }

  push(input: Float32Array): Int16Array[] {
    const pcm = floatToPcm16(this.resampler.process(input));
    const frames: Int16Array[] = [];
    let read = 0;
    while (read < pcm.length) {
      const take = Math.min(FRAME_SAMPLES - this.filled, pcm.length - read);
      this.frame.set(pcm.subarray(read, read + take), this.filled);
      this.filled += take;
      read += take;
      if (this.filled === FRAME_SAMPLES) {
        frames.push(this.frame);
        this.frame = new Int16Array(FRAME_SAMPLES);
        this.filled = 0;
      }
    }
    return frames;
  }
}

/** Decodes streamed s16le bytes to floats; WebSocket frames may split a sample. */
export class PcmDecoder {
  private carry: number | null = null;

  decode(bytes: Uint8Array): Float32Array<ArrayBuffer> {
    let data = bytes;
    if (this.carry !== null) {
      data = new Uint8Array(bytes.length + 1);
      data[0] = this.carry;
      data.set(bytes, 1);
      this.carry = null;
    }
    const samples = data.length >> 1;
    if (data.length & 1) this.carry = data[data.length - 1] ?? 0;
    const view = new DataView(data.buffer, data.byteOffset, samples * 2);
    const out = new Float32Array(samples);
    for (let i = 0; i < samples; i++) out[i] = view.getInt16(i * 2, true) / 0x8000;
    return out;
  }

  reset(): void {
    this.carry = null;
  }
}
