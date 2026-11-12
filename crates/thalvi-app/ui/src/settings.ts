// The settings window: sidebar, search, pages, and save or revert of the whole draft.

import type { AudioStatus, Backend, Settings, Status } from "./contract.ts";
import { errorText } from "./contract.ts";
import { createControls } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook } from "./look.ts";
import { buildPages, type Page, type PageContext, type SwitchRead } from "./settings-pages.ts";

export const ELSEWHERE_TEXT = "Changed outside this window — Revert loads the saved settings";

export const SAVED_TEXT = "Saved — applies after the current capture";

export const SAVED_MICROPHONE_TEXT = `${SAVED_TEXT}; the saved microphone is not active until it opens`;

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
  const revert = el("button", { type: "button", class: "quiet" }, "Revert");
  const save = el("button", { type: "button", class: "primary" }, "Save");
  const main = el("main", {}, pagesNode, empty, el("footer", { class: "bar" }, message, failure, copy, dismiss, revert, save));
  root.append(el("div", { class: "app" }, nav, main));

  let pages: Page[] = [];
  let checks: PageContext["checks"] = [];
  let navButtons: HTMLButtonElement[] = [];
  let current = "speech";
  let draft: Settings | null = null;
  let baseline = "";
  let runtimeFailure = "";
  let runtimeCauses: string[] = [];
  let copyNote = "";
  let saveFailure = "";
  let saving = false;
  let saved = false;
  // The microphone the last save chose (null is the system default); unset when that save left it alone.
  let savedMicrophone: { name: string | null } | null = null;
  let changedElsewhere = false;
  let audio: AudioStatus = { selected: null, active: null, paused: false };
  const live: PageContext["live"] = {
    level: () => undefined,
    listening: () => undefined,
    audio: () => undefined,
    speechModel: () => undefined,
    status: () => undefined,
    opened: () => undefined,
    refresh: () => undefined,
    saved: () => undefined,
    update: () => undefined,
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
    const shownFailure = saveFailure || runtimeFailure;
    if (failure.textContent !== shownFailure) {
      copy.textContent = "Copy";
      copyNote = "";
    }
    failure.textContent = shownFailure;
    copy.hidden = shownFailure === "";
    dismiss.hidden = shownFailure === "";
    const dirty = draft !== null && JSON.stringify(draft) !== baseline;
    message.textContent = saving ? "Saving…" : changedElsewhere ? ELSEWHERE_TEXT : dirty ? "Unsaved changes"
      : copyNote !== "" ? copyNote
      : saved && failure.textContent === "" ? savedText() : "";
  }

  function savedText(): string {
    const open = savedMicrophone !== null && audio.active !== null && audio.selected === savedMicrophone.name;
    return savedMicrophone === null || open ? SAVED_TEXT : SAVED_MICROPHONE_TEXT;
  }

  function markSaved(before: Settings | undefined, after: Settings): void {
    saved = true;
    savedMicrophone = before === undefined || before.microphone === after.microphone ? null : { name: after.microphone };
  }

  let lastState: Status["state"] | null = null;

  function showStatus(status: Status): void {
    // A download or retry finishes behind this long-lived window, so what lives on disk is read again when the app's state changes.
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

  async function load(): Promise<void> {
    saveFailure = "";
    saved = false;
    changedElsewhere = false;
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
    const holdKeyDefaults = await backend.getHoldKeyDefaults();
    const startAtLogin = await backend.getStartAtLogin().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    const menuEntry = await backend.getMenuEntry().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    audio = await backend.getAudio().catch(() => ({
      selected: loaded.microphone,
      active: null,
      paused: false,
    }));
    draft = structuredClone(loaded);
    baseline = JSON.stringify(draft);
    applyLook(root, draft.look);
    checks = [];
    pages = buildPages({
      draft, backend, root, controls: createControls(feedback), microphones, audio, holdKeyUnavailable, holdKeyDefaults, startAtLogin, menuEntry, live, checks,
      saved: () => (baseline === "" ? null : (JSON.parse(baseline) as Settings)),
      applyNow,
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

  /** Saves the saved settings with `change` applied; unsaved edits stay in the draft and unsaved. */
  async function applyNow(change: (settings: Settings) => void): Promise<void> {
    if (baseline === "") return;
    if (saving) {
      saveFailure = "Wait for the save in progress to finish.";
      feedback();
      throw new Error(saveFailure);
    }
    const before = JSON.parse(baseline) as Settings;
    const next = structuredClone(before);
    change(next);
    saveFailure = "";
    saving = true;
    feedback();
    try {
      await backend.saveSettings(next);
      baseline = JSON.stringify(next);
      if (draft !== null) change(draft);
      markSaved(before, next);
      changedElsewhere = false;
    } catch (error) {
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
    const before = baseline === "" ? undefined : (JSON.parse(baseline) as Settings);
    const listBefore = before?.stt.phrases;
    sent
      .then(() => {
        baseline = JSON.stringify(sending);
        markSaved(before, sending);
        changedElsewhere = false;
        live.saved();
        if (JSON.stringify(listBefore ?? []) === JSON.stringify(sending.stt.phrases ?? [])) return undefined;
        // The server keeps its own copy for dictation and conversations; a miss is said, the save stands.
        return backend.shareWordList().catch((error: unknown) => {
          saveFailure = errorText(error);
        });
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
    if (progress.status === "done") live.refresh();
  });
  backend.onOpenPage(open);
  backend.onUpdate((state) => {
    live.update(state);
  });
  backend.onSettingsChangedElsewhere(() => {
    if (!saving && draft !== null && JSON.stringify(draft) === baseline) void load();
    else changedElsewhere = true;
    feedback();
  });
  backend.onStatus(showStatus);
  backend.getStatus().then(showStatus).catch((error: unknown) => {
    runtimeFailure = errorText(error);
    runtimeCauses = [];
    feedback();
  });
  void load();
}
