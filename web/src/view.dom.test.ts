// @vitest-environment jsdom

import { afterEach, expect, it, vi } from "vitest";
import { INITIAL, reduce, type SessionState } from "./session.ts";
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
