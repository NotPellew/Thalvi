// The overlay's single line of text: it never wraps, and slides left as words arrive.

import { el } from "./dom.ts";

/** Clear space kept at each end while a notice scrolls through, so it starts and ends inside the fade. */
const MARGIN = 16;

export interface Ticker {
  node: HTMLElement;
  /** Settled words, the live partial in grey, and a caret after them. */
  words(committed: string, partial: string, caret: boolean): void;
  /** A message shown once from its start to its end, however long. */
  notice(text: string): void;
  clear(): void;
  /** Measures the text again, after the line was laid out for the first time or resized. */
  fit(): void;
}

export function createTicker(): Ticker {
  const node = el("div", { class: "tick" });
  let run: HTMLElement | null = null;
  let kind: "words" | "notice" | "none" = "none";

  function fresh(): HTMLElement {
    const next = el("span", { class: "run" });
    node.replaceChildren(next);
    run = next;
    return next;
  }

  function words(committed: string, partial: string, caret: boolean): void {
    const animate = kind === "words";
    const line = animate && run !== null ? run : fresh();
    kind = "words";
    node.classList.remove("fail");
    const parts: (Node | string)[] = [partial === "" || committed === "" ? committed : `${committed} `];
    if (partial !== "") parts.push(el("span", { class: "p2" }, partial));
    if (caret) parts.push(el("span", { class: "caret" }));
    line.replaceChildren(...parts);
    place(line, animate);
  }

  function place(line: HTMLElement, animate: boolean): void {
    const over = line.scrollWidth - node.clientWidth;
    node.classList.toggle("over", over > 0);
    line.style.transition = animate ? "" : "none";
    line.style.transform = `translateX(${String(Math.min(0, -over))}px)`;
  }

  function notice(text: string): void {
    const progress = (value: string): string | undefined => value.match(/^(.* — )\d+%$/)?.[1];
    const prefix = progress(text);
    const line = kind === "notice" && run !== null && prefix !== undefined && progress(run.textContent) === prefix ? run : fresh();
    kind = "notice";
    line.textContent = text;
    node.classList.remove("over");
    node.classList.add("fail");
    scrollIfLong(line);
  }

  function scrollIfLong(line: HTMLElement): void {
    const over = line.scrollWidth - node.clientWidth;
    line.classList.toggle("scroll", over > 0);
    if (over > 0) line.style.setProperty("--over", `${String(over + 2 * MARGIN)}px`);
  }

  function fit(): void {
    if (run === null) return;
    if (kind === "words") place(run, false);
    else scrollIfLong(run);
  }

  function clear(): void {
    kind = "none";
    run = null;
    node.replaceChildren();
    node.classList.remove("over", "fail");
  }

  return { node, words, notice, clear, fit };
}
