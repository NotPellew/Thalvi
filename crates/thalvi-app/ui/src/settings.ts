// The settings window: sidebar, search, pages, and save or revert of the whole draft.

import type { AudioStatus, Backend, Settings, Status } from "./contract.ts";
import { errorText } from "./contract.ts";
import { createControls } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook } from "./look.ts";
import { buildPages, type Page, type PageContext } from "./settings-pages.ts";

export const SAVED_TEXT = "Saved — applies after the current capture; the saved microphone is not active until it opens";

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
  let checks: PageContext["checks"] = [];
  let navButtons: HTMLButtonElement[] = [];
  let current = "general";
  let draft: Settings | null = null;
  let baseline = "";
  let runtimeFailure = "";
  let saveFailure = "";
  let saving = false;
  let saved = false;
  let audio: AudioStatus = { selected: null, active: null, paused: false };
  const live: PageContext["live"] = {
    level: () => undefined,
    listening: () => undefined,
    audio: () => undefined,
    speechModel: () => undefined,
  };

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

  function feedback(): void {
    failure.textContent = saveFailure || runtimeFailure;
    const dirty = draft !== null && JSON.stringify(draft) !== baseline;
    message.textContent = saving ? "Saving…" : dirty ? "Unsaved changes"
      : saved && failure.textContent === "" ? SAVED_TEXT : "";
  }

  function changed(): void {
    saveFailure = "";
    feedback();
  }

  function showStatus(status: Status): void {
    if (status.state === "failed") runtimeFailure = status.notice;
    else if (["listening", "capturing", "done"].includes(status.state)) runtimeFailure = "";
    feedback();
  }

  function showAudio(next: AudioStatus): void {
    audio = next;
    live.audio(next);
    live.listening(!next.paused);
    if (next.paused || next.active === null) live.level(0);
  }

  async function load(): Promise<void> {
    saveFailure = "";
    saved = false;
    feedback();
    let loaded: Settings;
    try {
      loaded = await backend.getSettings();
    } catch (error) {
      saveFailure = errorText(error);
      feedback();
      return;
    }
    const microphones = await backend.listMicrophones().catch((error: unknown) => ({ error: errorText(error) }));
    audio = await backend.getAudio().catch(() => ({
      selected: loaded.microphone,
      active: null,
      paused: false,
    }));
    draft = structuredClone(loaded);
    baseline = JSON.stringify(draft);
    applyLook(root, draft.look);
    checks = [];
    pages = buildPages({ draft, backend, root, controls: createControls(changed), microphones, audio, live, checks });
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
    showAudio(audio);
    feedback();
    applyView();
  }

  search.addEventListener("input", applyView);

  revert.addEventListener("click", () => {
    void load();
  });
  save.addEventListener("click", () => {
    if (draft === null) return;
    const sending = structuredClone(draft);
    saveFailure = "";
    saving = true;
    feedback();
    save.disabled = true;
    revert.disabled = true;
    const pending = checks.map((check) => check()).filter((check) => check !== null);
    const sent = pending.length === 0 ? backend.saveSettings(sending) : Promise.all(pending).then(() => backend.saveSettings(sending));
    sent
      .then(() => {
        baseline = JSON.stringify(sending);
        saved = true;
      })
      .catch((error: unknown) => {
        saved = false;
        saveFailure = errorText(error);
      })
      .finally(() => {
        saving = false;
        save.disabled = false;
        revert.disabled = false;
        feedback();
      });
  });

  backend.onLevel((level) => {
    live.level(audio.paused || audio.active === null ? 0 : level);
  });
  backend.onAudio(showAudio);
  backend.onSpeechModelProgress((progress) => {
    live.speechModel(progress);
  });
  backend.onStatus(showStatus);
  backend.getStatus().then(showStatus).catch((error: unknown) => {
    runtimeFailure = errorText(error);
    feedback();
  });
  void load();
}
