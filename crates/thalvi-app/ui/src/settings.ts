// The settings window: sidebar, search, pages, and save or revert of the whole draft.

import type { Backend, Settings } from "./contract.ts";
import { errorText } from "./contract.ts";
import { createControls } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook } from "./look.ts";
import { buildPages, type Page, type PageContext } from "./settings-pages.ts";

export const SAVED_TEXT = "Saved — applies after the current capture";

export function mountSettings(root: HTMLElement, backend: Backend): void {
  const search = el("input", { type: "search", "aria-label": "Search settings", placeholder: "Search settings", class: "search" });
  const items = el("div", { class: "items" });
  const nav = el("nav", { "aria-label": "Settings pages" }, el("span", { class: "brand" }, "THALVI"), search, items);
  const pagesNode = el("div", { class: "pages" });
  const empty = el("p", { class: "empty", hidden: true }, "No settings match your search.");
  const message = el("span", { class: "msg", role: "status" });
  const failure = el("span", { class: "err", role: "alert" });
  const revert = el("button", { type: "button", class: "quiet" }, "Revert");
  const save = el("button", { type: "button", class: "primary" }, "Save");
  const main = el("main", {}, pagesNode, empty, el("footer", { class: "bar" }, message, failure, revert, save));
  root.append(el("div", { class: "app" }, nav, main));

  let pages: Page[] = [];
  let navButtons: HTMLButtonElement[] = [];
  let current = "general";
  let draft: Settings | null = null;
  let baseline = "";
  let listening = true;
  const live: PageContext["live"] = { level: () => undefined, listening: () => undefined };

  function applyView(): void {
    const query = search.value.trim().toLowerCase();
    let matches = 0;
    for (const page of pages) {
      const rows = page.node.querySelectorAll<HTMLElement>(".row");
      let shown = 0;
      for (const row of rows) {
        const match = query === "" || (row.dataset["label"] ?? "").includes(query);
        row.hidden = !match;
        if (match) shown += 1;
      }
      for (const heading of page.node.querySelectorAll<HTMLElement>("[data-group]")) heading.hidden = query !== "";
      page.node.hidden = query === "" ? page.id !== current : shown === 0;
      matches += shown;
    }
    empty.hidden = query === "" || matches > 0;
    for (const button of navButtons) {
      if (query === "" && button.dataset["page"] === current) button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    }
  }

  function changed(): void {
    if (draft === null) return;
    failure.textContent = "";
    message.textContent = JSON.stringify(draft) === baseline ? "" : "Unsaved changes";
  }

  async function load(): Promise<void> {
    failure.textContent = "";
    message.textContent = "";
    let loaded: Settings;
    try {
      loaded = await backend.getSettings();
    } catch (error) {
      failure.textContent = errorText(error);
      return;
    }
    const microphones = await backend.listMicrophones().catch((error: unknown) => ({ error: errorText(error) }));
    draft = structuredClone(loaded);
    baseline = JSON.stringify(draft);
    applyLook(root, draft.look);
    pages = buildPages({ draft, backend, root, controls: createControls(changed), microphones, live });
    pagesNode.replaceChildren(...pages.map((page) => page.node));
    navButtons = pages.map((page) => {
      const button = el("button", { type: "button", "data-page": page.id }, page.title);
      button.addEventListener("click", () => {
        current = page.id;
        search.value = "";
        applyView();
      });
      return button;
    });
    items.replaceChildren(...navButtons);
    live.listening(listening);
    applyView();
  }

  search.addEventListener("input", applyView);

  revert.addEventListener("click", () => {
    void load();
  });
  save.addEventListener("click", () => {
    if (draft === null) return;
    const sending = draft;
    failure.textContent = "";
    message.textContent = "Saving…";
    save.disabled = true;
    backend
      .saveSettings(sending)
      .then(() => {
        baseline = JSON.stringify(sending);
        message.textContent = SAVED_TEXT;
      })
      .catch((error: unknown) => {
        message.textContent = "";
        failure.textContent = errorText(error);
      })
      .finally(() => {
        save.disabled = false;
      });
  });

  backend.onLevel((level) => {
    live.level(level);
  });
  backend.onStatus((status) => {
    listening = status.state !== "paused";
    live.listening(listening);
  });
  backend
    .getStatus()
    .then((status) => {
      listening = status.state !== "paused";
      live.listening(listening);
    })
    .catch((error: unknown) => {
      console.error("get_status failed", error);
    });
  void load();
}
