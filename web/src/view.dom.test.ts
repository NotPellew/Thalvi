// @vitest-environment jsdom

import { afterEach, expect, it, vi } from "vitest";
import { INITIAL, reduce, type SessionState, UNREACHABLE_NOTICE } from "./session.ts";
import { createView } from "./view.ts";

afterEach(() => {
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

it("collapses task steps on the next user turn while retaining expansion during the current turn", () => {
  vi.stubGlobal("ResizeObserver", class { observe() { return undefined; } });
  const root = document.createElement("div");
  document.body.append(root);
  const view = createView(root);
  let state: SessionState = { ...INITIAL, link: "online" };
  const render = (message: Parameters<typeof reduce>[1]): void => {
    state = reduce(state, message);
    view.render(state);
  };
  const details = root.querySelector<HTMLDetailsElement>(".o-tasks");
  if (!details) throw new Error("Task steps is missing");

  render({ type: "server", message: { type: "user", text: "Search for metric units" } });
  render({ type: "server", message: { type: "task", step: { id: "1", label: "Searching conversations", status: "running" } } });
  expect(details.open).toBe(false);
  details.open = true;
  render({ type: "server", message: { type: "task", step: { id: "1", label: "Searching conversations", status: "done" } } });
  expect(details.open).toBe(true);

  render({ type: "server", message: { type: "user", text: "Search for bread dough" } });
  render({ type: "server", message: { type: "task", step: { id: "2", label: "Searching conversations", status: "running" } } });
  expect(details.open).toBe(false);
  expect(details.hidden).toBe(false);
});

it("shows a Private indicator with its explanation for as long as the conversation is private", () => {
  vi.stubGlobal("ResizeObserver", class { observe() { return undefined; } });
  const root = document.createElement("div");
  document.body.append(root);
  const view = createView(root);
  const badge = root.querySelector<HTMLElement>(".o-private");
  const note = root.querySelector<HTMLElement>(".o-private-note");
  if (!badge || !note) throw new Error("the Private indicator is missing");

  view.render({ ...INITIAL, link: "online" });
  expect([badge.hidden, note.hidden]).toEqual([true, true]);

  view.render({ ...INITIAL, link: "online", private: true, turns: [{ role: "user", text: "hi" }] });
  expect([badge.hidden, badge.textContent, note.hidden]).toEqual([false, "Private", false]);
  expect(note.textContent).toMatch(/Providers still process your requests/);

  view.render({ ...INITIAL, link: "ended", private: true });
  expect(badge.hidden).toBe(false);

  view.render({ ...INITIAL, link: "online" });
  expect([badge.hidden, note.hidden]).toEqual([true, true]);
});

it("shows Speak replies as a switch that follows the server's confirmation and waits while offline", () => {
  vi.stubGlobal("ResizeObserver", class { observe() { return undefined; } });
  const root = document.createElement("div");
  document.body.append(root);
  const view = createView(root);
  const speak = root.querySelector<HTMLButtonElement>("button[role=switch]");
  if (!speak) throw new Error("Speak replies is missing");
  const clicks: string[] = [];
  view.onSpeak(() => clicks.push("click"));

  view.render({ ...INITIAL, link: "reconnecting", speak: true });
  expect(speak.disabled).toBe(true);

  view.render({ ...INITIAL, link: "online", speak: true });
  expect(speak.disabled).toBe(false);
  expect(speak.getAttribute("aria-checked")).toBe("false");
  speak.click();
  view.render({ ...INITIAL, link: "online", speak: true, replyAudio: true });
  expect(speak.getAttribute("aria-checked")).toBe("true");
  expect(speak.textContent).toBe("Speak replies");
  expect(clicks).toEqual(["click"]);
});

type SessionMessage = Parameters<typeof reduce>[1];

function noticeView() {
  vi.stubGlobal("ResizeObserver", class { observe() { return undefined; } });
  const root = document.createElement("div");
  document.body.append(root);
  const view = createView(root);
  let state: SessionState = { ...INITIAL, link: "online" };
  const render = (message: SessionMessage): void => {
    state = reduce(state, message);
    view.render(state);
  };
  view.onDismissNotice(() => {
    render({ type: "notice", notice: null });
  });
  const part = (selector: string): HTMLElement => {
    const found = root.querySelector<HTMLElement>(selector);
    if (!found) throw new Error(`${selector} is missing`);
    return found;
  };
  return { render, part };
}

it.each<[string, SessionMessage, string, SessionMessage]>([
  [
    "a server error",
    { type: "server", message: { type: "error", message: "The voice service failed." } },
    "The voice service failed.",
    { type: "server", message: { type: "user", text: "try again" } },
  ],
  ["a lost connection", { type: "lost" }, UNREACHABLE_NOTICE, { type: "opened" }],
  [
    "a microphone failure",
    { type: "voice", on: false, speak: false, notice: "The microphone stopped working." },
    "The microphone stopped working.",
    { type: "voice", on: true, speak: true, notice: null },
  ],
])("keeps %s on screen until dismissed or fixed, and copies its text", async (_name, raise, text, fix) => {
  vi.useFakeTimers();
  const writeText = vi.fn(() => Promise.resolve());
  vi.stubGlobal("navigator", { clipboard: { writeText } });
  const { render, part } = noticeView();
  const notice = part(".o-notice");

  render(raise);
  vi.advanceTimersByTime(60_000);
  expect(notice.hidden).toBe(false);
  expect(part(".o-notice-text").textContent).toBe(text);

  part(".o-notice-copy").click();
  await vi.advanceTimersByTimeAsync(0);
  expect(writeText).toHaveBeenCalledWith(text);
  expect(part(".o-notice-copy").textContent).toBe("Copied");

  part(".o-notice-close").click();
  expect(notice.hidden).toBe(true);

  render(raise);
  expect(part(".o-notice-copy").textContent).toBe("Copy");
  render(fix);
  expect(notice.hidden).toBe(true);
  vi.useRealTimers();
});

it("selects the notice text for copying by hand when the clipboard refuses", async () => {
  vi.stubGlobal("navigator", { clipboard: { writeText: () => Promise.reject(new Error("denied")) } });
  const { render, part } = noticeView();
  render({ type: "notice", notice: "The voice service failed." });

  part(".o-notice-copy").click();
  await vi.waitFor(() => {
    expect(window.getSelection()?.toString()).toBe("The voice service failed.");
  });
  expect(part(".o-notice-copy").textContent).toBe("Copy");
});
