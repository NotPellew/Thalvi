import "./view.css";
import { displayState, type DisplayState, type SessionState } from "./session.ts";
import { SPEED_STEPS, formatSpeed, normalizeSpeed } from "./speed.ts";
import type { Turn } from "./protocol.ts";

const STATES: Record<DisplayState, { label: string; hint: string }> = {
  connecting: { label: "Connecting", hint: "Reaching the server." },
  reconnecting: { label: "Reconnecting", hint: "Voice is paused. Trying again." },
  idle: { label: "Idle", hint: "Tap the reticle to start talking, or type below." },
  starting: { label: "Starting", hint: "Getting ready to listen." },
  listening: { label: "Listening", hint: "Go ahead, I am listening." },
  thinking: { label: "Thinking", hint: "Working on your answer. Tap the reticle to stop." },
  speaking: { label: "Speaking", hint: "Reading the answer aloud. Tap the reticle to stop." },
};

const PATHS = {
  mic: '<path d="M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3z"/><path d="M5 11a7 7 0 0 0 14 0M12 18v3"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor"/>',
  send: '<path d="M4 12h15M13 6l6 6-6 6"/>',
  wait: '<path d="M20 12a8 8 0 1 1-2.6-5.9"/><path d="M20 4v5h-5"/>',
  listening:
    '<path d="M12 3v5M12 16v5M3 12h5M16 12h5"/><rect x="10" y="10" width="4" height="4" fill="currentColor"/>',
  thinking:
    '<circle cx="5" cy="12" r="2.2" fill="currentColor"/><circle cx="12" cy="12" r="2.2" fill="currentColor"/><circle cx="19" cy="12" r="2.2" fill="currentColor"/>',
  speaking: '<path d="M4 10v4"/><path d="M8 6v12"/><path d="M12 3v18"/><path d="M16 7v10"/><path d="M20 10v4"/>',
  alert: '<path d="M12 3 2 20h20z"/><path d="M12 10v4M12 17.5v.5"/>',
} as const;

export interface CollapseInput {
  readonly voice: boolean;
  readonly display: DisplayState;
  readonly atBottom: boolean;
  /** A message's start is pinned: the user is following, not scrolled away. */
  readonly startPinned: boolean;
  readonly composerFocused: boolean;
  readonly keyboardOpen: boolean;
  readonly latestTall: boolean;
}

const LIVE_DISPLAYS: readonly DisplayState[] = ["starting", "listening", "thinking", "speaking"];

/** The big orb stays only for a live voice exchange read at the bottom; anything else collapses it. */
export function shouldCollapse(input: CollapseInput): boolean {
  return !(
    input.voice &&
    LIVE_DISPLAYS.includes(input.display) &&
    (input.atBottom || input.startPinned) &&
    !input.composerFocused &&
    !input.keyboardOpen &&
    !input.latestTall
  );
}

export interface ScrollInput {
  readonly nearBottom: boolean;
  readonly latestTall: boolean;
  /** Some message's start is pinned and the user has not scrolled since. */
  readonly pinned: boolean;
  /** The latest message already had its start pinned, even if the user has scrolled since. */
  readonly latestHandled: boolean;
  /** The latest message's start is pinned and the user has not scrolled since. */
  readonly latestPinned: boolean;
}

/** Follow the bottom, but keep the start of a message taller than the viewport in view. */
export function scrollTarget(input: ScrollInput): "start" | "bottom" | "stay" {
  if (input.latestPinned) return "start";
  if (!input.nearBottom && !input.pinned) return "stay";
  return input.latestTall && !input.latestHandled ? "start" : "bottom";
}

export interface SpeedOption {
  readonly rate: number;
  readonly label: string;
  readonly selected: boolean;
}

export function speedOptions(current: number): SpeedOption[] {
  const selected = normalizeSpeed(current);
  return SPEED_STEPS.map((rate) => ({ rate, label: formatSpeed(rate), selected: rate === selected }));
}

export type TextSegment =
  | { readonly kind: "text"; readonly text: string }
  | { readonly kind: "link"; readonly text: string; readonly href: string };

const URL_PATTERN = /(?:https?:\/\/|www\.)[^\s<>"']+/g;
const TRAILING = /[.,;:!?)\]}]+$/;

/** Splits message text into plain and tappable-link segments; no markup is ever built from the text. */
export function splitLinks(text: string): TextSegment[] {
  const segments: TextSegment[] = [];
  let last = 0;
  for (const match of text.matchAll(URL_PATTERN)) {
    const raw = match[0];
    const trimmed = raw.replace(TRAILING, "");
    const href = trimmed.startsWith("www.") ? `https://${trimmed}` : trimmed;
    if (match.index > last) segments.push({ kind: "text", text: text.slice(last, match.index) });
    segments.push({ kind: "link", text: trimmed, href });
    last = match.index + trimmed.length;
  }
  if (last < text.length) segments.push({ kind: "text", text: text.slice(last) });
  return segments;
}

type IconName = keyof typeof PATHS;

function icon(name: IconName, cls = ""): string {
  return `<svg class="o-ic ${cls}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${PATHS[name]}</svg>`;
}

function coreIcon(display: DisplayState): IconName {
  switch (display) {
    case "connecting":
    case "reconnecting":
      return "wait";
    case "idle":
    case "starting":
      return "mic";
    case "listening":
      return "listening";
    case "thinking":
      return "thinking";
    case "speaking":
      return "speaking";
  }
}

type Point = readonly [number, number];

/** Corner `k` of the octagon with flat sides facing up, down, left and right. */
function corner(radius: number, k: number): Point {
  const angle = ((22.5 + 45 * k) * Math.PI) / 180;
  return [140 + radius * Math.cos(angle), 140 + radius * Math.sin(angle)];
}

function at(point: Point): string {
  return `${point[0].toFixed(1)} ${point[1].toFixed(1)}`;
}

function orbSvg(display: DisplayState): string {
  const core = Array.from({ length: 8 }, (_, k) => at(corner(84, k)).replace(" ", ",")).join(" ");
  let segments = "";
  for (let k = 0; k < 8; k += 1) {
    const [x1, y1] = corner(118, k);
    const [x2, y2] = corner(118, k + 1);
    const from: Point = [x1 + (x2 - x1) * 0.16, y1 + (y2 - y1) * 0.16];
    const to: Point = [x1 + (x2 - x1) * 0.84, y1 + (y2 - y1) * 0.84];
    segments += `<path class="o-seg" style="--i:${String(k)}" d="M${at(from)}L${at(to)}"/>`;
  }
  let ornament = "";
  if (display === "speaking") {
    for (let i = 0; i < 32; i += 1) {
      const h = Math.round(6 + 10 * Math.abs(Math.sin(i * 0.9) * Math.cos(i * 0.37)));
      ornament += `<g transform="rotate(${String(i * 11.25)} 140 140)"><rect class="o-bar" style="animation-delay:${String((i % 8) * -0.13)}s" x="138" y="${String(18 - h)}" width="4" height="${String(h)}"/></g>`;
    }
  } else {
    let ticks = "";
    for (let i = 0; i < 24; i += 1) {
      ticks += `<line class="o-tick" x1="140" y1="${i % 6 === 0 ? "6" : "12"}" x2="140" y2="20" transform="rotate(${String(i * 15)} 140 140)"/>`;
    }
    ornament = `<g class="o-ticks">${ticks}</g>`;
  }
  const sweep =
    display === "listening" || display === "starting"
      ? '<g class="o-sweep"><line x1="140" y1="140" x2="140" y2="34"/></g>'
      : "";
  const cross = '<path class="o-cross" d="M140 42v18M140 220v18M42 140h18M220 140h18"/>';
  return `<svg class="o-svg" viewBox="0 0 280 280" aria-hidden="true">${ornament}${segments}${sweep}${cross}<polygon class="o-core" points="${core}"/></svg>`;
}

const MARKUP = `
  <div class="orb-app o-collapsed">
    <header class="o-top">
      <span class="o-brand">Thalvi</span>
      <div class="o-head-actions">
        <button class="o-speed-chip" type="button" aria-haspopup="true" aria-expanded="false" aria-label="Playback speed 1×">1×</button>
        <button class="o-voice" type="button" aria-pressed="false">${icon("mic")}<span class="o-voice-text">Voice off</span></button>
      </div>
    </header>
    <div class="o-speed-row" role="group" aria-label="Playback speed" hidden></div>
    <div class="o-layout">
      <section class="o-stage">
        <button class="o-orb" type="button" aria-label="Start talking">
          <span class="o-hud" aria-hidden="true"><i class="o-corner tl"></i><i class="o-corner tr"></i><i class="o-corner bl"></i><i class="o-corner br"></i></span>
          <span class="o-orb-art"></span>
          <span class="o-core-ic"></span>
        </button>
        <h1 class="o-state" role="status"></h1>
        <p class="o-hint"></p>
      </section>
      <div class="o-right">
        <p class="o-notice" role="alert" hidden>${icon("alert")}<span class="o-notice-text"></span></p>
        <section class="o-scroll" tabindex="0" aria-label="Conversation">
          <div class="o-flow">
            <div class="o-empty" hidden>
              <p>Ask anything while your hands are busy.</p>
              <p>You can also paste a link below and say what to do with it.</p>
            </div>
            <div class="o-turns" aria-live="polite"></div>
            <p class="o-turn o-assistant o-pending" hidden><span class="o-who">Thalvi</span><span class="o-dots" role="img" aria-label="Thinking">${icon("thinking")}</span></p>
            <p class="o-turn o-user o-live" hidden><span class="o-who">Hearing you</span><span class="o-live-text"></span></p>
          </div>
        </section>
        <footer class="o-foot">
          <form class="o-composer">
            <label class="o-sr" for="o-message">Message</label>
            <textarea id="o-message" rows="1" placeholder="Type or paste a message" enterkeyhint="send" disabled></textarea>
            <button class="o-send" type="submit" aria-label="Send" disabled>${icon("send")}</button>
          </form>
        </footer>
      </div>
    </div>
  </div>`;

function find<T extends Element>(root: ParentNode, selector: string, type: new () => T): T {
  const found = root.querySelector(selector);
  if (!(found instanceof type)) throw new Error(`${selector} is missing from the view markup`);
  return found;
}

function turnElement(turn: Turn): HTMLParagraphElement {
  const line = document.createElement("p");
  line.className = `o-turn o-${turn.role === "user" ? "user" : "assistant"}`;
  const who = document.createElement("span");
  who.className = "o-who";
  who.textContent = turn.role === "user" ? "You" : "Thalvi";
  const text = document.createElement("span");
  text.className = "o-text";
  line.append(who, text);
  return line;
}

function renderMessageText(text: HTMLSpanElement, value: string): void {
  if (text.dataset["text"] === value) return;
  text.dataset["text"] = value;
  const nodes: Node[] = [];
  for (const segment of splitLinks(value)) {
    if (segment.kind === "link") {
      const anchor = document.createElement("a");
      anchor.href = segment.href;
      anchor.target = "_blank";
      anchor.rel = "noopener noreferrer";
      anchor.textContent = segment.text;
      nodes.push(anchor);
    } else {
      nodes.push(document.createTextNode(segment.text));
    }
  }
  text.replaceChildren(...nodes);
}

function syncTurns(list: HTMLElement, turns: readonly Turn[], growing: boolean): void {
  while (list.children.length > turns.length) list.lastElementChild?.remove();
  turns.forEach((turn, index) => {
    let line = list.children[index];
    const want = turn.role === "user" ? "o-user" : "o-assistant";
    if (!(line instanceof HTMLParagraphElement) || !line.classList.contains(want)) {
      const fresh = turnElement(turn);
      if (line) line.replaceWith(fresh);
      else list.append(fresh);
      line = fresh;
    }
    const text = find(line, ".o-text", HTMLSpanElement);
    renderMessageText(text, turn.text);
    line.classList.toggle(
      "o-growing",
      growing && index === turns.length - 1 && turn.role === "assistant",
    );
    line.classList.toggle(
      "o-latest",
      index === turns.length - 1 && turn.role === "assistant",
    );
  });
}

/** Builds the page inside `root`; the controller drives it through `render`. */
export function createView(root: HTMLElement) {
  root.innerHTML = MARKUP;
  const page = find(root, ".orb-app", HTMLDivElement);
  const voice = find(page, ".o-voice", HTMLButtonElement);
  const voiceText = find(page, ".o-voice-text", HTMLSpanElement);
  const orb = find(page, ".o-orb", HTMLButtonElement);
  const orbArt = find(page, ".o-orb-art", HTMLSpanElement);
  const coreIc = find(page, ".o-core-ic", HTMLSpanElement);
  const stateName = find(page, ".o-state", HTMLHeadingElement);
  const hint = find(page, ".o-hint", HTMLParagraphElement);
  const notice = find(page, ".o-notice", HTMLParagraphElement);
  const noticeText = find(page, ".o-notice-text", HTMLSpanElement);
  const scroller = find(page, ".o-scroll", HTMLElement);
  const empty = find(page, ".o-empty", HTMLDivElement);
  const list = find(page, ".o-turns", HTMLDivElement);
  const pending = find(page, ".o-pending", HTMLParagraphElement);
  const live = find(page, ".o-live", HTMLParagraphElement);
  const liveText = find(page, ".o-live-text", HTMLSpanElement);
  const form = find(page, ".o-composer", HTMLFormElement);
  const composer = find(page, "textarea", HTMLTextAreaElement);
  const send = find(page, ".o-send", HTMLButtonElement);
  let currentSpeed = 1;
  let speedOpen = false;
  let speedHandler: ((rate: number) => void) | null = null;
  const speedChip = find(page, ".o-speed-chip", HTMLButtonElement);
  const speedRow = find(page, ".o-speed-row", HTMLDivElement);
  const speedButtons = SPEED_STEPS.map((rate) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "o-speed-step";
    button.textContent = formatSpeed(rate);
    button.setAttribute("aria-label", `Playback speed ${formatSpeed(rate)}`);
    button.addEventListener("click", () => {
      currentSpeed = rate;
      renderSpeed();
      setSpeedOpen(false);
      speedChip.focus();
      speedHandler?.(rate);
    });
    speedRow.append(button);
    return button;
  });

  function renderSpeed(): void {
    speedChip.textContent = formatSpeed(currentSpeed);
    speedChip.setAttribute("aria-label", `Playback speed ${formatSpeed(currentSpeed)}`);
    speedChip.setAttribute("aria-expanded", String(speedOpen));
    speedRow.hidden = !speedOpen;
    speedButtons.forEach((button, index) => {
      const selected = SPEED_STEPS[index] === currentSpeed;
      button.setAttribute("aria-pressed", String(selected));
      button.classList.toggle("o-selected", selected);
    });
  }

  function setSpeedOpen(open: boolean): void {
    speedOpen = open;
    renderSpeed();
  }

  speedChip.addEventListener("click", () => {
    setSpeedOpen(!speedOpen);
  });
  speedChip.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && speedOpen) {
      setSpeedOpen(false);
    }
  });
  speedRow.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      setSpeedOpen(false);
      speedChip.focus();
    }
  });
  renderSpeed();

  let submitHandler: ((text: string) => void) | null = null;
  let stopHandler: (() => void) | null = null;
  let voiceHandler: (() => void) | null = null;
  let voiceBusy = false;
  let online = false;
  let shownState: DisplayState | null = null;
  let orbAction: "stop" | "voice" | "none" = "none";
  let lastState: SessionState | null = null;
  let atBottom = true;
  let composerFocused = false;
  let keyboardOpen = false;
  let latestTall = false;
  let collapsed = true;
  let handledLatest: Element | null = null;
  let startPinned = false;

  const applyCollapse = (): void => {
    if (!lastState) return;
    const want = shouldCollapse({
      voice: lastState.voice,
      display: displayState(lastState),
      atBottom,
      startPinned,
      composerFocused,
      keyboardOpen,
      latestTall,
    });
    if (want === collapsed) return;
    const topBefore = scroller.getBoundingClientRect().top;
    const scrollBefore = scroller.scrollTop;
    collapsed = want;
    page.classList.toggle("o-collapsed", want);
    scroller.scrollTop = scrollBefore + (scroller.getBoundingClientRect().top - topBefore);
    if (!want && atBottom) scroller.scrollTop = scroller.scrollHeight;
  };

  for (const input of ["wheel", "touchmove", "keydown", "pointerdown"] as const) {
    scroller.addEventListener(input, () => {
      startPinned = false;
    }, { passive: true });
  }
  let observedHeight = scroller.clientHeight;
  scroller.addEventListener("scroll", () => {
    // Chrome fires a queued scroll after a resize but before the ResizeObserver, which re-pins first.
    if (scroller.clientHeight !== observedHeight) return;
    atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 96;
    applyCollapse();
  }, { passive: true });
  new ResizeObserver(() => {
    observedHeight = scroller.clientHeight;
    if (atBottom) scroller.scrollTop = scroller.scrollHeight;
  }).observe(scroller);
  composer.addEventListener("focus", () => {
    composerFocused = true;
    applyCollapse();
  });
  composer.addEventListener("blur", () => {
    composerFocused = false;
    applyCollapse();
  });

  const viewport = window.visualViewport;
  if (viewport) {
    const fit = (): void => {
      page.style.setProperty("--o-height", `${String(viewport.height)}px`);
      page.style.setProperty("--o-top", `${String(viewport.offsetTop)}px`);
      keyboardOpen = window.innerHeight - viewport.height > 120;
      window.scrollTo(0, 0);
      applyCollapse();
    };
    viewport.addEventListener("resize", fit);
    viewport.addEventListener("scroll", fit);
    fit();
  }

  const syncSend = (): void => {
    send.disabled = !online || composer.value.trim() === "";
  };
  composer.addEventListener("input", syncSend);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    submitHandler?.(composer.value);
  });
  composer.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    submitHandler?.(composer.value);
  });
  orb.addEventListener("click", () => {
    if (orbAction === "stop") stopHandler?.();
    else if (orbAction === "voice") voiceHandler?.();
  });

  return {
    render(s: SessionState) {
      lastState = s;
      const display = displayState(s);
      const nearBottom =
        scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 96;
      atBottom = nearBottom;
      page.dataset["state"] = display;
      online = s.link === "online";

      if (display !== shownState) {
        shownState = display;
        page.classList.remove("o-glitching");
        void page.getBoundingClientRect();
        page.classList.add("o-glitching");
        stateName.textContent = STATES[display].label;
        hint.textContent = STATES[display].hint;
        orbArt.innerHTML = orbSvg(display);
        const busy = display === "speaking" || display === "thinking";
        if (busy) {
          coreIc.innerHTML = `${icon("stop")}<span class="o-lab">Stop</span>`;
        } else if (display === "idle") {
          coreIc.innerHTML = `${icon("mic")}<span class="o-lab">Talk</span>`;
        } else {
          coreIc.innerHTML = icon(coreIcon(display));
        }
      }

      const busy = display === "speaking" || display === "thinking";
      orbAction = busy ? "stop" : display === "connecting" || display === "reconnecting" ? "none" : "voice";
      orb.disabled = orbAction === "none" || (orbAction === "voice" && voiceBusy);
      orb.setAttribute(
        "aria-label",
        busy ? "Stop" : display === "idle" ? "Start talking" : display === "connecting" || display === "reconnecting" ? STATES[display].label : "End voice",
      );
      orb.classList.toggle("o-is-stop", busy);

      notice.hidden = s.notice === null;
      noticeText.textContent = s.notice ?? "";

      voice.setAttribute("aria-pressed", String(s.voice));
      voiceText.textContent = s.voice ? "Voice on" : "Voice off";
      voice.disabled = voiceBusy;

      empty.hidden = s.turns.length > 0 || s.partial !== "";
      syncTurns(list, s.turns, s.replyOpen);
      pending.hidden = display !== "thinking" || s.turns.at(-1)?.role !== "user";
      live.hidden = s.partial === "";
      liveText.textContent = s.partial;

      const latest = list.lastElementChild;
      latestTall = latest instanceof HTMLElement && latest.offsetHeight > window.innerHeight;
      applyCollapse();

      composer.disabled = !online;
      syncSend();
      const target = scrollTarget({
        nearBottom,
        latestTall,
        pinned: startPinned,
        latestHandled: latest !== null && latest === handledLatest,
        latestPinned: latest !== null && latest === handledLatest && startPinned,
      });
      if (target === "bottom") {
        startPinned = false;
        scroller.scrollTop = scroller.scrollHeight;
      } else if (target === "start" && latest instanceof HTMLElement) {
        handledLatest = latest;
        startPinned = true;
        scroller.scrollTop += latest.getBoundingClientRect().top - scroller.getBoundingClientRect().top - 26;
      }
    },
    setVoiceBusy(busy: boolean) {
      voiceBusy = busy;
      voice.disabled = busy;
      if (orbAction === "voice") orb.disabled = busy;
    },
    clearComposer() {
      composer.value = "";
      syncSend();
    },
    onSubmit(handler: (text: string) => void) {
      submitHandler = handler;
    },
    onStop(handler: () => void) {
      stopHandler = handler;
    },
    onVoice(handler: () => void) {
      voiceHandler = handler;
      voice.onclick = handler;
    },
    setSpeed(rate: number) {
      currentSpeed = normalizeSpeed(rate);
      renderSpeed();
    },
    onSpeed(handler: (rate: number) => void) {
      speedHandler = handler;
    },
  };
}
