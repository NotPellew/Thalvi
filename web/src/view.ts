import "./view.css";
import { displayState, type DisplayState, type SessionState } from "./session.ts";
import type { Turn } from "./protocol.ts";

const STATES: Record<DisplayState, { label: string; hint: string }> = {
  connecting: { label: "Connecting", hint: "Reaching the server." },
  reconnecting: { label: "Reconnecting", hint: "Voice is paused. Trying again." },
  idle: { label: "Idle", hint: "Tap the circle to start talking, or type below." },
  starting: { label: "Starting", hint: "Getting ready to listen." },
  listening: { label: "Listening", hint: "Go ahead, I am listening." },
  thinking: { label: "Thinking", hint: "Working on your answer. Tap the circle to stop." },
  speaking: { label: "Speaking", hint: "Reading the answer aloud. Tap the circle to stop." },
};

const PATHS = {
  mic: '<path d="M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3z"/><path d="M5 11a7 7 0 0 0 14 0M12 18v3"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor"/>',
  send: '<path d="M4 12h15M13 6l6 6-6 6"/>',
  wait: '<path d="M20 12a8 8 0 1 1-2.6-5.9"/><path d="M20 4v5h-5"/>',
  listening:
    '<circle cx="12" cy="12" r="2.4" fill="currentColor"/><circle cx="12" cy="12" r="6"/><circle cx="12" cy="12" r="10"/>',
  thinking:
    '<circle cx="5" cy="12" r="2.2" fill="currentColor"/><circle cx="12" cy="12" r="2.2" fill="currentColor"/><circle cx="19" cy="12" r="2.2" fill="currentColor"/>',
  speaking: '<path d="M4 10v4"/><path d="M8 6v12"/><path d="M12 3v18"/><path d="M16 7v10"/><path d="M20 10v4"/>',
  alert: '<path d="M12 3 2 20h20z"/><path d="M12 10v4M12 17.5v.5"/>',
} as const;

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

function orbSvg(display: DisplayState): string {
  let ticks = "";
  for (let i = 0; i < 24; i += 1) {
    const long = i % 6 === 0;
    ticks += `<line class="o-tick" x1="140" y1="${long ? "6" : "12"}" x2="140" y2="20" transform="rotate(${String(i * 15)} 140 140)"/>`;
  }
  let ornament = ticks;
  if (display === "listening" || display === "starting") {
    ornament +=
      '<circle class="o-ring r1" cx="140" cy="140" r="84"/><circle class="o-ring r2" cx="140" cy="140" r="84"/><circle class="o-ring r3" cx="140" cy="140" r="84"/><circle class="o-scan" cx="140" cy="140" r="100"/>';
  } else if (display === "thinking") {
    ornament =
      '<circle class="o-track" cx="140" cy="140" r="112"/><g class="o-orbit"><circle cx="140" cy="28" r="10"/><circle cx="140" cy="28" r="10" transform="rotate(120 140 140)"/><circle cx="140" cy="28" r="10" transform="rotate(240 140 140)"/></g>';
  } else if (display === "connecting" || display === "reconnecting") {
    ornament = '<circle class="o-dash" cx="140" cy="140" r="112"/>';
  } else if (display === "idle") {
    ornament = '<circle class="o-idle-ring" cx="140" cy="140" r="112"/>';
  } else {
    for (let i = 0; i < 40; i += 1) {
      const h = Math.round(14 + 20 * Math.abs(Math.sin(i * 0.9) * Math.cos(i * 0.37)));
      ornament += `<g transform="rotate(${String(i * 9)} 140 140)"><rect class="o-bar" style="animation-delay:${String((i % 8) * -0.13)}s" x="138" y="${String(40 - h)}" width="5" height="${String(h)}" rx="2.5"/></g>`;
    }
  }
  return `<svg class="o-svg" viewBox="0 0 280 280" aria-hidden="true">${ornament}<circle class="o-core" cx="140" cy="140" r="78"/></svg>`;
}

const MARKUP = `
  <div class="orb-app">
    <header class="o-top">
      <span class="o-brand">Thalvi</span>
      <button class="o-voice" type="button" aria-pressed="false">${icon("mic")}<span class="o-voice-text">Voice off</span></button>
    </header>
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
    if (text.dataset["text"] !== turn.text) {
      text.dataset["text"] = turn.text;
      text.textContent = turn.text;
    }
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

  let submitHandler: ((text: string) => void) | null = null;
  let stopHandler: (() => void) | null = null;
  let voiceHandler: (() => void) | null = null;
  let voiceBusy = false;
  let online = false;
  let shownState: DisplayState | null = null;
  let orbAction: "stop" | "voice" | "none" = "none";

  const viewport = window.visualViewport;
  if (viewport) {
    const fit = (): void => {
      page.style.setProperty("--o-height", `${String(viewport.height)}px`);
      page.style.setProperty("--o-top", `${String(viewport.offsetTop)}px`);
      window.scrollTo(0, 0);
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
      const display = displayState(s);
      const nearBottom =
        scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 96;
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

      composer.disabled = !online;
      syncSend();
      if (nearBottom) scroller.scrollTop = scroller.scrollHeight;
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
  };
}
