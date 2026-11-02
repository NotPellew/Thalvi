// The mic wave: nine twisting strands on a canvas, swelling with the recent loudness.

export const WAVE_WIDTH = 332;
export const WAVE_HEIGHT = 44;

export type Rgb = readonly [number, number, number];

const HISTORY = 40;
const SAMPLE_MS = 80;
const SETTLED_FRAME_MS = 100;
const SETTLED_BELOW = 0.015;

const TAU = 2 * Math.PI;
const rgba = (color: Rgb, alpha: number): string => `rgba(${String(color[0])},${String(color[1])},${String(color[2])},${alpha.toFixed(3)})`;
const envelope = (x: number): number => Math.exp(-(((x - 0.5) / 0.27) ** 2));
const strandA = (x: number, phase: number): number =>
  Math.sin(TAU * 3.1 * x + phase) * 0.62 + Math.sin(TAU * 5.2 * x - phase * 1.35 + 1) * 0.38;
const strandB = (x: number, phase: number): number =>
  Math.sin(TAU * 2.5 * x - phase * 0.8 + 2) * 0.55 + Math.sin(TAU * 4.3 * x + phase * 1.1) * 0.45;

/** How far from the centre line the strands reach at `x` (0..1 across the wave). */
function amplitude(x: number, history: readonly number[], level: number, seconds: number): number {
  const distance = Math.abs(x - 0.5) * 2;
  const at = history.length - 1 - distance * 14;
  const index = Math.floor(at);
  const share = at - index;
  const sample = (history[index] ?? 0) * (1 - share) + (history[index + 1] ?? 0) * share;
  const breathing = 0.035 + 0.012 * Math.sin(seconds * 2.2);
  return WAVE_HEIGHT * 0.47 * envelope(x) * (breathing + 0.95 * Math.max(level * (1 - distance * 0.6), sample));
}

function ribbon(
  context: CanvasRenderingContext2D,
  color: Rgb,
  level: number,
  history: readonly number[],
  seconds: number,
  phase: number,
): void {
  const middle = WAVE_HEIGHT / 2;
  const gradient = context.createLinearGradient(0, 0, WAVE_WIDTH, 0);
  for (const [offset, alpha] of [[0, 0], [0.18, 0.6], [0.5, 1], [0.82, 0.6], [1, 0]] as const) {
    gradient.addColorStop(offset, rgba(color, alpha));
  }
  context.globalCompositeOperation = "lighter";
  context.strokeStyle = gradient;
  for (const [width, alpha] of [[3.2, 0.07], [1, 0.55]] as const) {
    context.lineWidth = width;
    for (let strand = 0; strand < 9; strand += 1) {
      const side = (strand / 8) * 2 - 1;
      const twist = side * 1.25;
      const cosine = Math.cos(twist);
      const sine = Math.sin(twist);
      context.globalAlpha = alpha * (0.45 + 0.55 * (1 - Math.abs(side)));
      context.beginPath();
      for (let px = 0; px <= WAVE_WIDTH; px += 2) {
        const x = px / WAVE_WIDTH;
        const y = middle + amplitude(x, history, level, seconds) * (strandA(x, phase) * cosine + strandB(x, phase) * sine);
        if (px === 0) context.moveTo(px, y);
        else context.lineTo(px, y);
      }
      context.stroke();
    }
  }
}

export interface Wave {
  /** Draws a frame for time `now` (ms) when one is due: 30 per second, 10 once settled. */
  draw(now: number, level: number, color: Rgb): void;
  /** Forgets the last capture so the next one starts as a thin line. */
  reset(): void;
}

/** `glow` is lit behind the wave in proportion to the loudness. */
export function createWave(canvas: HTMLCanvasElement, glow: HTMLElement): Wave {
  const scale = Math.min(2, window.devicePixelRatio || 1);
  canvas.width = WAVE_WIDTH * scale;
  canvas.height = WAVE_HEIGHT * scale;
  let context: CanvasRenderingContext2D | null = null;
  let history: number[] = Array<number>(HISTORY).fill(0);
  let smoothed = 0;
  let phase = 0;
  let lastFrame = Number.NEGATIVE_INFINITY;
  let lastSample = Number.NEGATIVE_INFINITY;

  function reset(): void {
    history = Array<number>(HISTORY).fill(0);
    smoothed = 0;
    phase = 0;
    lastFrame = Number.NEGATIVE_INFINITY;
    lastSample = Number.NEGATIVE_INFINITY;
  }

  function draw(now: number, level: number, color: Rgb): void {
    context ??= canvas.getContext("2d");
    if (context === null) return;
    const settled = smoothed < SETTLED_BELOW && level < SETTLED_BELOW && history.every((sample) => sample < SETTLED_BELOW);
    if (settled && now - lastFrame < SETTLED_FRAME_MS) return;
    const seconds = Math.min(0.05, Math.max(0, (now - lastFrame) / 1000) || 0);
    lastFrame = now;
    const due = Math.floor((now - lastSample) / SAMPLE_MS);
    if (due > 0) {
      for (let sample = 0; sample < Math.min(due, HISTORY); sample += 1) history.push(level);
      history = history.slice(-HISTORY);
      lastSample = due > HISTORY || !Number.isFinite(lastSample) ? now : lastSample + due * SAMPLE_MS;
    }
    const pace = (rate: number): number => 1 - (1 - rate) ** (seconds * 60);
    smoothed += (level - smoothed) * pace(level > smoothed ? 0.35 : 0.08);
    phase += seconds * (1.1 + 3.2 * smoothed);

    context.setTransform(scale, 0, 0, scale, 0, 0);
    context.globalCompositeOperation = "source-over";
    context.globalAlpha = 1;
    context.clearRect(0, 0, WAVE_WIDTH, WAVE_HEIGHT);
    ribbon(context, color, smoothed, history, now / 1000, phase);
    glow.style.opacity = Math.min(1, smoothed * 1.3).toFixed(2);
  }

  return { draw, reset };
}
