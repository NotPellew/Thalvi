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
