import { displayState, type DisplayState, type SessionState, type Turn } from "./session.ts";

const LABELS: Record<DisplayState, string> = {
  connecting: "Connecting",
  listening: "Listening",
  thinking: "Thinking",
  speaking: "Speaking",
  reconnecting: "Reconnecting",
};

function element<T extends HTMLElement>(id: string, type: new () => T): T {
  const found = document.getElementById(id);
  if (!(found instanceof type)) throw new Error(`#${id} is missing from index.html`);
  return found;
}

export interface View {
  render(state: SessionState): void;
  setStarting(starting: boolean): void;
  onStart(handler: () => void): void;
  onStop(handler: () => void): void;
  onEnd(handler: () => void): void;
}

export function createView(): View {
  const state = element("state", HTMLHeadingElement);
  const notice = element("notice", HTMLParagraphElement);
  const conversation = element("conversation", HTMLElement);
  const partial = element("partial", HTMLParagraphElement);
  const start = element("start", HTMLButtonElement);
  const controls = element("controls", HTMLElement);
  const stop = element("stop", HTMLButtonElement);
  const end = element("end", HTMLButtonElement);
  let shownTurns: readonly Turn[] = [];

  return {
    render(s) {
      const active = s.screen === "active";
      const display = active ? displayState(s) : "idle";
      state.dataset["state"] = display;
      state.textContent = active ? LABELS[displayState(s)] : "Thalvi";
      notice.hidden = s.notice === null;
      notice.textContent = s.notice;
      partial.textContent = s.partial;
      start.hidden = active;
      controls.hidden = !active;
      if (s.turns !== shownTurns) {
        shownTurns = s.turns;
        conversation.replaceChildren(
          ...s.turns.map((turn) => {
            const line = document.createElement("p");
            line.className = turn.role;
            line.textContent = turn.text;
            return line;
          }),
        );
      }
      conversation.scrollTop = conversation.scrollHeight;
    },
    setStarting(starting) {
      start.disabled = starting;
      start.textContent = starting ? "Starting…" : "Start";
    },
    onStart(handler) {
      start.onclick = handler;
    },
    onStop(handler) {
      stop.onclick = handler;
    },
    onEnd(handler) {
      end.onclick = handler;
    },
  };
}
