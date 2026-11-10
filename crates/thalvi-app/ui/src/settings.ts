// The settings window: sidebar, search, pages, and each valid change saved at once.

import type { AudioStatus, Backend, Settings, Status } from "./contract.ts";
import { errorText } from "./contract.ts";
import { createControls } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook } from "./look.ts";
import { buildPages, type Page, type PageContext, type SwitchRead } from "./settings-pages.ts";
import { CAPTURE_STATES } from "./settings-pages.ts";

export const APPLIED_TEXT = "Applied.";

export const APPLIED_NEXT_CAPTURE = "Applied. From the next dictation.";

export function mountSettings(root: HTMLElement, backend: Backend): void {
  const search = el("input", { type: "search", "aria-label": "Search settings", placeholder: "Search settings", class: "search" });
  const items = el("div", { class: "items" });
  const nav = el("nav", { "aria-label": "Settings pages" }, el("span", { class: "brand" }, "THALVI"), search, items);
  const pagesNode = el("div", { class: "pages" });
  const empty = el("p", { class: "empty", hidden: true }, "No settings match your search.");
  const failure = el("span", { class: "err", role: "alert" });
  const copy = el("button", { type: "button", class: "quiet" }, "Copy");
  const dismiss = el("button", { type: "button", class: "quiet", "aria-label": "Dismiss error" }, "×");
  const main = el("main", {}, pagesNode, empty, el("footer", { class: "bar" }, failure, copy, dismiss));
  root.append(el("div", { class: "app" }, nav, main));

  let pages: Page[] = [];
  let checks: PageContext["checks"] = [];
  let navButtons: HTMLButtonElement[] = [];
  let current = "speech";
  let applied: Settings | null = null;
  let draft: Settings | null = null;
  let runtimeFailure = "";
  let runtimeCauses: string[] = [];
  let saveFailure = "";
  let saving = false;
  let capturing = false;
  let audio: AudioStatus = { selected: null, active: null, paused: false };
  const typing = new Set<HTMLElement>();
  const live: PageContext["live"] = {
    level: () => undefined,
    listening: () => undefined,
    audio: () => undefined,
    speechModel: () => undefined,
    status: () => undefined,
    opened: () => undefined,
    refresh: () => undefined,
    saved: () => undefined,
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
    const shownFailure = saveFailure || runtimeFailure;
    if (failure.textContent !== shownFailure) {
      copy.textContent = "Copy";
    }
    failure.textContent = shownFailure;
    copy.hidden = shownFailure === "";
    dismiss.hidden = shownFailure === "";
  }

  let lastState: Status["state"] | null = null;

  function showStatus(status: Status): void {
    capturing = CAPTURE_STATES.includes(status.state);
    if (lastState !== null && status.state !== lastState) live.refresh();
    lastState = status.state;
    runtimeFailure = status.state === "failed" ? status.notice : "";
    runtimeCauses = status.state === "failed" ? status.causes : [];
    live.status(status);
    feedback();
  }

  function showAudio(next: AudioStatus): void {
    audio = next;
    live.audio(next);
    live.listening(!next.paused);
    if (next.paused || next.active === null) live.level(0);
    feedback();
  }

  function syncDraft(): void {
    if (applied === null || draft === null) return;
    const keep = new Map<HTMLElement, string>();
    for (const field of typing) {
      if (field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement) {
        keep.set(field, field.value);
      }
    }
    draft = structuredClone(applied);
    for (const [field, value] of keep) {
      if (field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement) field.value = value;
    }
  }

  async function load(): Promise<void> {
    saveFailure = "";
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
    const holdKeyUnavailable = await backend.getHoldKeyUnavailable();
    const startAtLogin = await backend.getStartAtLogin().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    const menuEntry = await backend.getMenuEntry().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    audio = await backend.getAudio().catch(() => ({
      selected: loaded.microphone,
      active: null,
      paused: false,
    }));
    applied = structuredClone(loaded);
    draft = structuredClone(loaded);
    applyLook(root, draft.look);
    checks = [];
    pages = buildPages({
      draft,
      backend,
      root,
      controls: createControls({ typing }),
      microphones,
      audio,
      holdKeyUnavailable,
      startAtLogin,
      menuEntry,
      live,
      checks,
      saved: () => applied,
      capturing: () => capturing,
      typing,
      apply,
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
  }

  function confirmAt(target: HTMLElement | undefined): void {
    if (!target) return;
    target.textContent = capturing ? APPLIED_NEXT_CAPTURE : APPLIED_TEXT;
    target.hidden = false;
    target.classList.add("ok");
  }

  /** Saves one change on top of the applied settings; typed fields keep local text until they apply. */
  async function apply(
    change: (settings: Settings) => void,
    options: { check?: () => Promise<void> | null; confirm?: HTMLElement } = {},
  ): Promise<void> {
    if (applied === null) return;
    if (saving) {
      saveFailure = "Wait for the save in progress to finish.";
      feedback();
      throw new Error(saveFailure);
    }
    const before = structuredClone(applied);
    const next = structuredClone(applied);
    change(next);
    saveFailure = "";
    saving = true;
    feedback();
    try {
      const pre: Promise<void>[] = checks
        .map((check) => check())
        .filter((check): check is Promise<void> => check !== null);
      const extra = options.check?.();
      if (extra) pre.push(extra);
      if (pre.length > 0) await Promise.all(pre);
      await backend.saveSettings(next);
      const listBefore = before.stt.phrases;
      applied = structuredClone(next);
      syncDraft();
      live.saved();
      confirmAt(options.confirm);
      if (JSON.stringify(listBefore ?? []) !== JSON.stringify(next.stt.phrases ?? [])) {
        await backend.shareWordList().catch((error: unknown) => {
          saveFailure = errorText(error);
        });
      }
    } catch (error) {
      if (draft !== null) syncDraft();
      saveFailure = errorText(error);
      throw error;
    } finally {
      saving = false;
      feedback();
    }
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
    const causes = saveFailure === "" ? runtimeCauses : [];
    backend.copyReport(failure.textContent, causes).then(
      () => {
        feedback();
        copy.textContent = "Copied";
      },
      (error: unknown) => {
        saveFailure = `Could not copy: ${errorText(error)}`;
        feedback();
      },
    );
  });
  dismiss.addEventListener("click", () => {
    if (saveFailure !== "") {
      saveFailure = "";
    } else {
      runtimeFailure = "";
      backend.dismissError().catch((error: unknown) => {
        saveFailure = errorText(error);
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
  backend.onSettingsChangedElsewhere(() => {
    if (!saving) void load();
  });
  backend.onStatus(showStatus);
  backend.getStatus().then(showStatus).catch((error: unknown) => {
    runtimeFailure = errorText(error);
    runtimeCauses = [];
    feedback();
  });
  void load();
}
