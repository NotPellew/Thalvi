// The holo and gunmetal overlay: a module hanging from the top edge with a mic wave over one line of text.

import type { Route, Status } from "./contract.ts";
import { el } from "./dom.ts";
import { createTicker } from "./ticker.ts";
import { createWave, type Rgb, WAVE_HEIGHT, WAVE_WIDTH } from "./wave.ts";

const WIDTH = 370;
const HEIGHT = 84;
const SHOULDER = 16;
/** Longest target the countdown label spells out, so the whole label fits between its stubs. */
const MAX_TARGET = 10;
const CYAN: Rgb = [107, 248, 255];
const QUIET: Rgb = [143, 179, 196];

type Tone = "live" | "quiet" | "pending" | "working" | "done" | "failed";

/** The corner pieces and slanted end caps drawn over the holo frame. */
function accents(): string {
  const reach = 20;
  const lean = Math.atan(SHOULDER / HEIGHT);
  const dx = Math.sin(lean) * reach;
  const dy = Math.cos(lean) * reach;
  const cap = (side: 1 | -1): string => {
    const x = (share: number): number => (side === 1 ? SHOULDER * share + 9 : WIDTH - SHOULDER * share - 9);
    return `<path d="M${String(x(0.35))} ${String(HEIGHT * 0.3)} L${String(x(0.65))} ${String(HEIGHT * 0.7)}"/>`;
  };
  const corner = (side: 1 | -1): string => {
    const edge = side === 1 ? SHOULDER : WIDTH - SHOULDER;
    return `<path d="M${String(edge + side * dx)} ${String(HEIGHT - dy)} L${String(edge)} ${String(HEIGHT)} L${String(edge + side * reach)} ${String(HEIGHT)}"/>`;
  };
  return `<svg class="acc" viewBox="0 0 ${String(WIDTH)} ${String(HEIGHT)}" width="${String(WIDTH)}" height="${String(HEIGHT)}" aria-hidden="true">${corner(1)}${corner(-1)}${cap(1)}${cap(-1)}</svg>`;
}

/** What the label says while an ending counts down: the action, its target, and whole seconds rounded up. */
export function countdownLabel(route: Route, target: string | null, ms: number): string {
  const seconds = String(Math.max(1, Math.ceil(ms / 1000)));
  const named = target?.toUpperCase();
  const to = named === undefined ? null : named.length > MAX_TARGET ? `${named.slice(0, MAX_TARGET - 1)}…` : named;
  const action = {
    paste: to === null ? "PASTE" : `PASTE INTO ${to}`,
    submit: to === null ? "SEND" : `SEND TO ${to}`,
    task: "TASK",
  }[route];
  return `${action} IN ${seconds} s`;
}

/** What the slot says once a delivery finished. */
export function doneMessage(route: Route, target: string | null, notice: string): string {
  if (target === null || route === "task") return `✓ ${notice.toUpperCase()}`;
  return `✓ ${route === "paste" ? "PASTED INTO" : "SENT TO"} ${target.toUpperCase()}`;
}

export interface Module {
  node: HTMLElement;
  render(status: Status, options: { showDraft: boolean; cancellable: boolean }): void;
  /** Time left of the ending's pause, while it counts down. */
  countdown(left: number, total: number): void;
  /** Measures the text again once the module is on screen. */
  fit(): void;
  /** Draws the wave if a frame is due. */
  draw(now: number, level: number): void;
}

export function createModule(onCancel: () => void): Module {
  const canvas = el("canvas", { "aria-hidden": "true" });
  const glow = el("div", { class: "glow" });
  const label = el("span", { class: "label" });
  const tag = el("div", { class: "tag", role: "status" }, el("span", {}, label));
  const wave = el("div", { class: "wv" }, glow, canvas, tag);
  const ticker = createTicker();
  const message = el("div", { class: "msg", role: "status" });
  const cancel = el("button", { class: "mx", type: "button", "aria-label": "Cancel capture", title: "Cancel" }, "×");
  const slot = el("div", { class: "slot" }, wave, el("div", { class: "sep" }), ticker.node, message, cancel);
  const strip = el("div", { class: "line" });
  const node = el("div", { class: "module", style: `width:${String(WIDTH)}px;height:${String(HEIGHT)}px` });
  node.append(
    el("div", { class: "shadow" }, el("div", { class: "outer" })),
    el("div", { class: "panel" }, el("i", { class: "hex" }), el("i", { class: "scan" }), el("i", { class: "vignette" }), el("i", { class: "shine" })),
    el("div", { class: "bezel" }),
    slot,
    strip,
  );
  node.insertAdjacentHTML("beforeend", accents());
  const ribbon = createWave(canvas, glow);
  canvas.style.width = `${String(WAVE_WIDTH)}px`;
  canvas.style.height = `${String(WAVE_HEIGHT)}px`;
  cancel.addEventListener("click", onCancel);

  let tone: Tone = "live";
  let drawing = false;
  let hasText = false;
  let counting: { route: Route; target: string | null } | null = null;

  function show(next: Tone, options: { label?: string; wave?: boolean; motion?: "sweep" | "drain" }): void {
    node.dataset["tone"] = next;
    node.dataset["motion"] = options.motion ?? "none";
    node.dataset["text"] = String(hasText);
    label.textContent = options.label ?? "";
    tag.hidden = options.label === undefined;
    const waveOn = options.wave ?? false;
    canvas.hidden = !waveOn;
    if (waveOn && !drawing) ribbon.reset();
    drawing = waveOn;
    tone = next;
    counting = null;
    strip.style.transform = "";
  }

  function lines(draft: boolean, write?: () => void): void {
    hasText = draft;
    node.dataset["text"] = String(draft);
    if (draft) write?.();
    else ticker.clear();
  }

  function render(status: Status, options: { showDraft: boolean; cancellable: boolean }): void {
    cancel.hidden = !options.cancellable;
    message.textContent = "";
    switch (status.state) {
      case "paused":
      case "listening":
        lines(false);
        show("live", {});
        break;
      case "starting":
        lines(false);
        show("live", { wave: true });
        break;
      case "capturing":
        lines(options.showDraft, () => {
          ticker.words(status.committed, status.partial ?? "", true);
        });
        show("live", { wave: true });
        break;
      case "pending":
        lines(options.showDraft, () => {
          ticker.words(status.text, "", false);
        });
        if (status.waitingForQuiet) {
          show("quiet", { wave: true });
        } else {
          show("pending", { label: countdownLabel(status.route, status.target, status.pauseMs), motion: "drain" });
          counting = { route: status.route, target: status.target };
        }
        break;
      case "working":
        show("working", { label: "WORKING", motion: "sweep" });
        break;
      case "loading":
        lines(true, () => {
          ticker.notice(status.notice);
        });
        show("working", { label: "LOADING", motion: "sweep" });
        break;
      case "done":
        message.textContent = doneMessage(status.route, status.target, status.notice);
        show("done", {});
        break;
      case "failed":
        lines(true, () => {
          ticker.notice(status.headline);
        });
        show("failed", { label: "NOT SENT" });
        break;
    }
  }

  function countdown(left: number, total: number): void {
    if (counting === null) return;
    const words = countdownLabel(counting.route, counting.target, left);
    if (label.textContent !== words) label.textContent = words;
    strip.style.transform = `scaleX(${(left / total).toFixed(3)})`;
  }

  function draw(now: number, level: number): void {
    if (drawing) ribbon.draw(now, level, tone === "quiet" ? QUIET : CYAN);
  }

  return {
    node,
    render,
    countdown,
    draw,
    fit: () => {
      ticker.fit();
    },
  };
}
