// The dictation overlay: a pill and a draft box driven by status and level events.

import type { Backend, Look, Route, Status } from "./contract.ts";
import { el } from "./dom.ts";
import { applyLook } from "./look.ts";
import { copiesText, createModule, doneMessage } from "./module.ts";

/** Frame scheduling and time, injectable so tests drive the animation. */
export interface Clock {
  now(): number;
  frame(callback: () => void): void;
}

export const realClock: Clock = {
  now: () => performance.now(),
  frame: (callback) => {
    requestAnimationFrame(callback);
  },
};

const BARS = 9;
const FLAT = 4 / 22;
const ENVELOPE = [0.45, 0.65, 0.82, 0.95, 1, 0.95, 0.82, 0.65, 0.45];
const ROUTE_WORD: Record<Route, string> = { paste: "PASTE", submit: "SUBMIT", task: "TASK" };

function actionWord(route: Route, copies: boolean): string {
  return copiesText(route, copies) ? "COPY" : ROUTE_WORD[route];
}

type Tone = "live" | "quiet" | "pending" | "working" | "done" | "failed";

function secondsLeft(ms: number): string {
  return String(Math.max(1, Math.floor(ms / 1000)));
}

export function mountOverlay(root: HTMLElement, backend: Backend, clock: Clock): void {
  const bars = Array.from({ length: BARS }, () => el("i"));
  const dot = el("span", { class: "dot" });
  const wave = el("div", { class: "wave", "aria-hidden": "true" }, ...bars);
  const label = el("span", { class: "lbl", role: "status" });
  const target = el("span", { class: "tgt" });
  const cancel = el("button", { class: "x", type: "button", "aria-label": "Cancel capture", title: "Cancel" }, "×");
  const pill = el("div", { class: "pill" }, dot, wave, label, target, cancel);
  const committed = el("span", { class: "c" });
  const partial = el("span", { class: "p" });
  const ending = el("span", { class: "end" });
  const caret = el("span", { class: "caret" });
  const text = el("div", { class: "text" }, committed, partial, ending, caret);
  const countdownBar = el("b");
  const countdown = el("div", { class: "cd" }, countdownBar);
  const draft = el("div", { class: "draft" }, el("div", { class: "tail" }, text), countdown);
  const skin = createModule(requestCancel);
  root.append(pill, draft, skin.node);
  root.hidden = true;
  applyLook(root, "gunmetal");

  let status: Status = { state: "paused" };
  let showDraft = true;
  let level = 0;
  let shown = 0;
  let countdownStart: number | null = null;
  let pauseMs = 0;
  let route: Route = "paste";
  let copies = false;
  let looping = false;
  let flat = true;
  let look: Look = "gunmetal";
  let clickable = false;

  function requestCancel(): void {
    backend.cancelCapture().catch((error: unknown) => {
      console.error("cancel_capture failed", error);
    });
  }
  cancel.addEventListener("click", requestCancel);

  function setBars(height: (index: number) => number): void {
    bars.forEach((bar, index) => {
      bar.style.transform = `scaleY(${height(index).toFixed(3)})`;
    });
  }

  function tick(): void {
    looping = false;
    if (!live()) return;
    const now = clock.now();
    if (look !== "neon") {
      skin.draw(now, level);
    } else if (!flat) {
      shown += (level - shown) * (level > shown ? 0.6 : 0.2);
      setBars((index) => {
        const wobble = 0.75 + 0.25 * Math.sin(now / 110 + index * 1.3);
        return FLAT + (1 - FLAT) * Math.min(1, shown * (ENVELOPE[index] ?? 1) * wobble * 1.2);
      });
    }
    updateCountdown(now);
    schedule();
  }

  function updateCountdown(now: number): void {
    if (countdownStart === null) return;
    const left = Math.max(0, pauseMs - (now - countdownStart));
    countdownBar.style.transform = `scaleX(${(left / pauseMs).toFixed(3)})`;
    label.textContent = `${actionWord(route, copies)} IN ${secondsLeft(left)} s`;
    skin.countdown(left, pauseMs);
  }

  function live(): boolean {
    return status.state === "starting" || status.state === "capturing" || status.state === "pending";
  }

  function cancellable(): boolean {
    return clickable && (live() || (status.state === "working" && status.cancellable));
  }

  function schedule(): void {
    if (looping) return;
    looping = true;
    clock.frame(tick);
  }

  function pillView(tone: Tone, words: string, to: string | null): void {
    pill.dataset["tone"] = tone;
    label.textContent = words;
    target.textContent = to === null ? "" : `→ ${to}`;
    flat = tone !== "live" && tone !== "quiet";
    if (flat) {
      shown = 0;
      setBars(() => FLAT);
    }
  }

  function render(next: Status): void {
    const previous = status;
    status = next;
    root.hidden = next.state === "paused" || next.state === "listening";
    root.dataset["state"] = next.state;
    const noticing = next.state === "capturing" && next.notice !== null;
    const drafting = noticing || (showDraft && (next.state === "capturing" || next.state === "pending"));
    text.dataset["notice"] = String(noticing);
    draft.hidden = !drafting;
    ending.hidden = true;
    caret.hidden = true;
    countdown.dataset["running"] = "false";
    cancel.hidden = !cancellable();
    skin.render(next, { showDraft, cancellable: cancellable() });
    switch (next.state) {
      case "paused":
      case "listening":
        countdownStart = null;
        return;
      case "starting":
        pillView("live", "LISTENING", next.target);
        break;
      case "capturing":
        pillView("live", "LISTENING", next.target);
        committed.textContent = next.notice ?? next.committed;
        partial.textContent = next.notice === null ? (next.partial ?? "") : "";
        caret.hidden = noticing;
        break;
      case "pending": {
        const wasCounting = previous.state === "pending" && !previous.waitingForQuiet;
        committed.textContent = next.text;
        partial.textContent = "";
        ending.hidden = false;
        ending.textContent = next.route;
        route = next.route;
        copies = next.copies;
        pauseMs = next.pauseMs;
        if (next.waitingForQuiet) {
          countdownStart = null;
          countdownBar.style.transform = "scaleX(1)";
          pillView("quiet", "QUIET", next.target);
        } else {
          countdownStart = wasCounting && countdownStart !== null ? countdownStart : clock.now();
          countdown.dataset["running"] = "true";
          pillView("pending", `${actionWord(next.route, next.copies)} IN ${secondsLeft(next.pauseMs)} s`, next.target);
        }
        break;
      }
      case "working":
        pillView("working", "WORKING", next.target);
        break;
      case "done":
        pillView("done", doneMessage(next.route, next.target, next.notice), null);
        break;
      case "failed":
        pillView("failed", next.headline, null);
        break;
      case "loading":
        pillView("working", next.notice, null);
        break;
    }
    if (next.state !== "pending") countdownStart = null;
    updateCountdown(clock.now());
    if (live()) {
      schedule();
    } else {
      level = 0;
    }
  }

  let seen = false;
  backend.onStatus((next) => {
    seen = true;
    render(next);
    if (idle(next)) refreshSettings();
  });
  backend.onSettingsSaved(() => {
    if (idle(status)) refreshSettings();
  });
  backend.onLevel((next) => {
    level = Math.min(1, Math.max(0, next));
  });
  backend
    .getStatus()
    .then((initial) => {
      if (!seen) render(initial);
    })
    .catch((error: unknown) => {
      console.error("get_status failed", error);
    });

  backend
    .overlayTakesClicks()
    .then((takes) => {
      clickable = takes;
      render(status);
    })
    .catch((error: unknown) => {
      console.error("overlay_takes_clicks failed", error);
    });

  function idle(of: Status): boolean {
    return of.state === "paused" || of.state === "listening";
  }

  function refreshSettings(): void {
    backend
      .getSettings()
      .then((settings) => {
        showDraft = settings.overlay.draft;
        look = settings.look;
        applyLook(root, look);
        skin.fit();
      })
      .catch((error: unknown) => {
        console.error("get_settings failed", error);
      });
  }
  refreshSettings();
}
