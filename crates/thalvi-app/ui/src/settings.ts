// The settings window: sidebar, search, pages, and the one way a change is applied.

import type { AudioStatus, Backend, Settings, Status } from "./contract.ts";
import { errorText } from "./contract.ts";
import { createControls } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook } from "./look.ts";
import {
  APPLIED_LATER_TEXT,
  APPLIED_TEXT,
  type ApplyOptions,
  buildPages,
  DURING_CAPTURE,
  type Page,
  type PageContext,
  type SwitchRead,
} from "./settings-pages.ts";

export { APPLIED_LATER_TEXT, APPLIED_TEXT };

/** Where the user was typing, so a refresh from outside leaves the text and the caret alone. */
interface Typing {
  page: string;
  row: string;
  index: number;
  value: string;
  start: number | null;
  end: number | null;
}

export function mountSettings(root: HTMLElement, backend: Backend): void {
  const search = el("input", { type: "search", "aria-label": "Search settings", placeholder: "Search settings", class: "search" });
  const items = el("div", { class: "items" });
  const nav = el("nav", { "aria-label": "Settings pages" }, el("span", { class: "brand" }, "THALVI"), search, items);
  const pagesNode = el("div", { class: "pages" });
  const empty = el("p", { class: "empty", hidden: true }, "No settings match your search.");
  const message = el("span", { class: "msg", role: "status" });
  const failure = el("span", { class: "err", role: "alert" });
  const copy = el("button", { type: "button", class: "quiet" }, "Copy");
  const dismiss = el("button", { type: "button", class: "quiet", "aria-label": "Dismiss error" }, "×");
  const bar = el("footer", { class: "bar", hidden: true }, message, failure, copy, dismiss);
  const main = el("main", {}, pagesNode, empty, bar);
  root.append(el("div", { class: "app" }, nav, main));

  let pages: Page[] = [];
  let navButtons: HTMLButtonElement[] = [];
  let current = "recognition";
  // What the desktop holds; pages read it and change it only through `apply`.
  let settings: Settings | null = null;
  let runtimeFailure = "";
  let runtimeCauses: string[] = [];
  let loadFailure = "";
  let copyNote = "";
  let audio: AudioStatus = { selected: null, active: null, paused: false, problem: null };
  let status: Status = { state: "listening" };
  // Applies run one after another, each on top of the one before.
  let queue: Promise<unknown> = Promise.resolve();
  const live: PageContext["live"] = {
    level: () => undefined,
    listening: () => undefined,
    audio: () => undefined,
    speechModel: () => undefined,
    status: () => undefined,
    opened: () => undefined,
    refresh: () => undefined,
    update: () => undefined,
  };
  const controls = createControls(() => (DURING_CAPTURE.includes(status.state) ? APPLIED_LATER_TEXT : APPLIED_TEXT));

  function applyView(): void {
    const query = search.value.trim().toLowerCase();
    let matches = 0;
    for (const page of pages) {
      const rows = page.node.querySelectorAll<HTMLElement>(".row");
      let shown = 0;
      for (const row of rows) {
        const match = query === "" || (row.dataset["label"] ?? "").includes(query);
        row.hidden = !match;
        // A row its page can't offer right now hides by its class and doesn't count as found.
        if (match && !row.classList.contains("off")) shown += 1;
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
    const shownFailure = loadFailure || runtimeFailure;
    if (failure.textContent !== shownFailure) {
      copy.textContent = "Copy";
      copyNote = "";
    }
    failure.textContent = shownFailure;
    copy.hidden = shownFailure === "";
    dismiss.hidden = shownFailure === "";
    message.textContent = copyNote;
    bar.hidden = shownFailure === "" && copyNote === "";
  }

  let lastState: Status["state"] | null = null;

  function showStatus(next: Status): void {
    // A download or retry finishes behind this long-lived window, so what lives on disk is read again when the app's state changes.
    if (lastState !== null && next.state !== lastState) live.refresh();
    lastState = next.state;
    status = next;
    runtimeFailure = next.state === "failed" ? next.notice : "";
    runtimeCauses = next.state === "failed" ? next.causes : [];
    live.status(next);
    feedback();
  }

  function showAudio(next: AudioStatus): void {
    audio = next;
    live.audio(next);
    live.listening(!next.paused);
    if (next.paused || next.active === null) live.level(0);
    feedback();
  }

  function typingNow(): Typing | null {
    const active = document.activeElement;
    if (!(active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement) || !pagesNode.contains(active)) return null;
    const row = active.closest<HTMLElement>(".row");
    const page = active.closest<HTMLElement>("section[data-page]");
    if (!row || !page) return null;
    const inputs = Array.from(row.querySelectorAll("input, textarea"));
    return {
      page: page.dataset["page"] ?? "",
      row: row.dataset["label"] ?? "",
      index: inputs.indexOf(active),
      value: active.value,
      start: active.selectionStart,
      end: active.selectionEnd,
    };
  }

  function typeAgain(typing: Typing): void {
    const page = pages.find((known) => known.id === typing.page);
    const row = Array.from(page?.node.querySelectorAll<HTMLElement>(".row") ?? []).find((known) => known.dataset["label"] === typing.row);
    const field = row?.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input, textarea")[typing.index];
    if (!field) return;
    field.value = typing.value;
    field.focus();
    // A number field has no caret.
    if (typing.start !== null && typing.end !== null) field.setSelectionRange(typing.start, typing.end);
    // Setting a value from code is not an edit, so leaving the field would not apply it unless it is told.
    field.addEventListener("blur", () => {
      field.dispatchEvent(new Event("change", { bubbles: true }));
    }, { once: true });
  }

  async function load(typing: Typing | null = null): Promise<void> {
    loadFailure = "";
    feedback();
    let loaded: Settings;
    try {
      await queue;
      loaded = await backend.getSettings();
    } catch (error) {
      loadFailure = errorText(error);
      feedback();
      return;
    }
    const microphones = await backend.listMicrophones().catch((error: unknown) => ({ error: errorText(error) }));
    const holdKeyUnavailable = await backend.getHoldKeyUnavailable();
    const holdKeyDefaults = await backend.getHoldKeyDefaults();
    const startAtLogin = await backend.getStartAtLogin().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    const menuEntry = await backend.getMenuEntry().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    audio = await backend.getAudio().catch(() => ({
      selected: loaded.microphone,
      active: null,
      paused: false,
      problem: null,
    }));
    settings = loaded;
    applyLook(root, loaded.look);
    pages = buildPages({
      settings: loaded, backend, root, open, controls, microphones, audio, holdKeyUnavailable, holdKeyDefaults, startAtLogin, menuEntry, live, apply,
    });
    pagesNode.replaceChildren(...pages.map((page) => page.node));
    navButtons = pages.map((page) => {
      const button = el("button", { type: "button", "data-page": page.id }, page.title);
      button.addEventListener("click", () => {
        open(page.id);
      });
      return button;
    });
    items.replaceChildren(...navButtons);
    showAudio(audio);
    feedback();
    applyView();
    if (typing) typeAgain(typing);
  }

  /** Applies one change on top of what the desktop holds; rejects with the plain reason it was refused, and then nothing changed. */
  function apply(change: (settings: Settings) => void, options: ApplyOptions = {}): Promise<void> {
    const run = async (): Promise<void> => {
      if (settings === null) throw new Error("The settings are not loaded.");
      const next = structuredClone(settings);
      change(next);
      try {
        await backend.saveSettings(next);
      } catch (error) {
        const reason = errorText(error);
        throw new Error(options.explain ? options.explain(reason) : reason);
      }
      for (const key of Object.keys(settings)) Reflect.deleteProperty(settings, key);
      Object.assign(settings, next);
    };
    const result = queue.then(run);
    queue = result.catch(() => undefined);
    return result;
  }

  function open(id: string): void {
    if (!pages.some((page) => page.id === id)) return;
    current = id;
    search.value = "";
    applyView();
    live.opened(id);
  }

  search.addEventListener("input", applyView);

  copy.addEventListener("click", () => {
    const causes = loadFailure === "" ? runtimeCauses : [];
    backend.copyReport(failure.textContent, causes).then(
      () => {
        copyNote = "";
        feedback();
        copy.textContent = "Copied";
      },
      (error: unknown) => {
        copyNote = `Could not copy: ${errorText(error)}`;
        feedback();
      },
    );
  });
  dismiss.addEventListener("click", () => {
    if (loadFailure !== "") {
      loadFailure = "";
    } else {
      runtimeFailure = "";
      backend.dismissError().catch((error: unknown) => {
        loadFailure = errorText(error);
        feedback();
      });
    }
    feedback();
  });

  backend.onLevel((level) => {
    live.level(audio.paused || audio.active === null ? 0 : level);
  });
  backend.onAudio(showAudio);
  backend.onSpeechModelProgress((progress) => {
    live.speechModel(progress);
    if (progress.status === "done") live.refresh();
  });
  backend.onOpenPage(open);
  backend.onUpdate((state) => {
    live.update(state);
  });
  backend.onSettingsChangedElsewhere(() => {
    void load(typingNow());
  });
  backend.onStatus(showStatus);
  backend.getStatus().then(showStatus).catch((error: unknown) => {
    runtimeFailure = errorText(error);
    runtimeCauses = [];
    feedback();
  });
  void load();
}
