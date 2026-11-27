// The settings window: sidebar, search, pages, and the one way a change is applied.

import type { AudioStatus, Backend, Look, LookStyles, Settings, Status } from "./contract.ts";
import { errorText } from "./contract.ts";
import { createControls } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook, applyLookStyles } from "./look.ts";
import { settingsPatch } from "./patch.ts";
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
  /** Disclosures this search opened, so clearing it closes only those. */
  const openedBySearch = new Set<HTMLDetailsElement>();
  /** Disclosures whose open state search just set, so that toggle is not a user touch. */
  const openedProgrammatically = new Set<HTMLDetailsElement>();
  /** Disclosures the user opened or closed while searching; those keep that state. */
  const touchedDuringSearch = new Set<HTMLDetailsElement>();
  // What the desktop holds; pages read it and change it only through `apply`.
  let settings: Settings | null = null;
  // The looks' resolved styles, and which read is newest so a slow answer cannot win.
  let lookStyles: LookStyles | null = null;
  let displayedLook: Look = "gunmetal";
  let styleRequest = 0;
  let loadRequest = 0;
  let windowReady = false;
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
    look: () => undefined,
    lookStyles: () => undefined,
    refresh: () => undefined,
    update: () => undefined,
  };
  const controls = createControls((atOnce = false) => (!atOnce && DURING_CAPTURE.includes(status.state) ? APPLIED_LATER_TEXT : APPLIED_TEXT));

  /** A row, a nested field, or its hint matches. */
  function matchesQuery(node: HTMLElement, query: string): boolean {
    if (query === "") return true;
    const label = node.dataset["label"] ?? "";
    return label.toLowerCase().includes(query);
  }

  function applyView(): void {
    const query = search.value.trim().toLowerCase();
    if (query === "") {
      for (const node of openedBySearch) {
        if (!touchedDuringSearch.has(node)) node.open = false;
      }
      openedBySearch.clear();
      touchedDuringSearch.clear();
    }
    let matches = 0;
    for (const page of pages) {
      const rows = page.node.querySelectorAll<HTMLElement>(".row");
      let shown = 0;
      for (const row of rows) {
        const match = matchesQuery(row, query);
        row.hidden = !match;
        // A row its page can't offer right now hides by its class and doesn't count as found.
        if (match && !row.classList.contains("off")) shown += 1;
        if (query !== "" && match) reveal(row);
      }
      if (query !== "") {
        for (const field of page.node.querySelectorAll<HTMLElement>(".mini")) {
          const name = field.querySelector("span")?.textContent ?? "";
          const labelled = field.querySelector("[aria-label]")?.getAttribute("aria-label") ?? "";
          if (`${name}\n${labelled}`.toLowerCase().includes(query)) {
            shown += 1;
            reveal(field);
          }
        }
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

  /** Opens every disclosure and hidden ancestor between `node` and its page, so a folded match can be used. */
  function reveal(node: HTMLElement): void {
    let currentNode: HTMLElement | null = node;
    while (currentNode !== null && !currentNode.matches("section[data-page]")) {
      if (currentNode instanceof HTMLDetailsElement && !currentNode.open) {
        openedBySearch.add(currentNode);
        openedProgrammatically.add(currentNode);
        currentNode.open = true;
      }
      const ancestor: HTMLElement | null = currentNode.parentElement;
      if (ancestor?.hidden === true && !ancestor.matches("section[data-page]")) ancestor.hidden = false;
      currentNode = ancestor;
    }
  }

  function feedback(): void {
    const shownFailure = loadFailure || runtimeFailure;
    if (failure.textContent !== shownFailure) {
      copy.textContent = "Copy";
      copyNote = "";
    }
    failure.textContent = shownFailure;
    if (shownFailure === "" && bar.contains(document.activeElement)) {
      const target = pages.find((page) => page.id === current && !page.node.hidden)?.node
        ?? pages.find((page) => !page.node.hidden)?.node
        ?? search;
      target.focus({ preventScroll: true });
    }
    copy.hidden = shownFailure === "";
    dismiss.hidden = shownFailure === "";
    message.textContent = copyNote;
    bar.hidden = shownFailure === "" && copyNote === "";
  }

  let lastState: Status["state"] | null = null;

  /** Reads again what lives on disk; focus stays on the user's control, or moves to its replacement when a page rebuilt it. */
  function refresh(): void {
    const focusable = ":is(button, input, select, textarea, summary, [tabindex]):not(:disabled)";
    const active = document.activeElement;
    const container = active && pagesNode.contains(active) ? active.closest<HTMLElement>("[data-id]") : null;
    const page = container?.closest<HTMLElement>("section[data-page]")?.dataset["page"];
    const id = container?.dataset["id"];
    const index = active && container ? Array.from(container.querySelectorAll(focusable)).indexOf(active) : -1;
    live.refresh();
    if (index < 0 || pagesNode.contains(document.activeElement)) return;
    const rebuilt = Array.from(pages.find((known) => known.id === page)?.node.querySelectorAll<HTMLElement>("[data-id]") ?? [])
      .find((known) => known.dataset["id"] === id);
    rebuilt?.querySelectorAll<HTMLElement>(focusable)[index]?.focus();
  }

  function showStatus(next: Status): void {
    // A download or retry finishes behind this long-lived window, so what lives on disk is read again when the app's state changes.
    if (lastState !== null && next.state !== lastState) refresh();
    lastState = next.state;
    // A change made during a capture is in effect once the capture is over.
    if (DURING_CAPTURE.includes(status.state) && !DURING_CAPTURE.includes(next.state)) {
      for (const applied of pagesNode.querySelectorAll(".applied")) {
        if (applied.textContent === APPLIED_LATER_TEXT) applied.textContent = APPLIED_TEXT;
      }
    }
    // Dismissing takes the offer off the failure, and the app publishes it again; that keeps the dismissal.
    const offerTakenAway =
      status.state === "failed" && next.state === "failed" && status.notice === next.notice && status.offer !== null && next.offer === null;
    status = next;
    if (!offerTakenAway) {
      runtimeFailure = next.state === "failed" ? next.notice : "";
      runtimeCauses = next.state === "failed" ? next.causes : [];
    }
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

  /** Shows a look at once; the styles read last are on hand, so picking one does not wait. */
  function showLook(look: Look): void {
    displayedLook = look;
    applyLook(root, look);
    if (lookStyles) applyLookStyles(root, lookStyles.looks[look]);
    live.look(look);
  }

  /** Reads the resolved look styles and puts the current look's on the window. */
  async function refreshLookStyles(): Promise<void> {
    const request = ++styleRequest;
    let styles: LookStyles;
    try {
      styles = await backend.getLookStyles();
    } catch (error) {
      if (request === styleRequest) {
        loadFailure = errorText(error);
        feedback();
      }
      return;
    }
    if (request !== styleRequest) return;
    lookStyles = styles;
    applyLookStyles(root, styles.looks[displayedLook]);
    live.lookStyles(styles);
  }

  function load(typing: Typing | null = null): void {
    const request = ++loadRequest;
    void loadSettings(request, typing).catch((error: unknown) => {
      if (request !== loadRequest) return;
      loadFailure = errorText(error);
      feedback();
    }).finally(() => {
      if (request !== loadRequest || windowReady) return;
      windowReady = true;
      void backend.settingsReady().catch((error: unknown) => {
        loadFailure = errorText(error);
        feedback();
      });
    });
  }

  async function loadSettings(request: number, typing: Typing | null): Promise<void> {
    loadFailure = "";
    feedback();
    await queue;
    if (request !== loadRequest) return;
    const stylesReady = refreshLookStyles();
    const loaded = await backend.getSettings().finally(() => stylesReady);
    if (request !== loadRequest) return;
    const microphones = await backend.listMicrophones().catch((error: unknown) => ({ error: errorText(error) }));
    const holdKeyUnavailable = await backend.getHoldKeyUnavailable();
    const appStylesUnavailable = await backend.getAppStylesUnavailable();
    const holdKeyDefaults = await backend.getHoldKeyDefaults();
    const startAtLogin = await backend.getStartAtLogin().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    const menuEntry = await backend.getMenuEntry().catch((error: unknown): SwitchRead => ({ kind: "unreadable", message: errorText(error) }));
    const loadedAudio = await backend.getAudio().catch(() => ({
      selected: loaded.microphone,
      active: null,
      paused: false,
      problem: null,
    }));
    if (request !== loadRequest) return;
    audio = loadedAudio;
    settings = loaded;
    showLook(loaded.look);
    live.opened = () => undefined;
    pages = buildPages({
      settings: loaded, backend, open, controls, microphones, audio, holdKeyUnavailable, appStylesUnavailable, holdKeyDefaults, startAtLogin, menuEntry, live, apply, showLook, lookStyles,
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
    live.opened(current);
  }

  /** Applies one change on top of what the desktop holds; rejects with the plain reason it was refused, and then nothing changed. */
  function apply(change: (settings: Settings) => void, options: ApplyOptions = {}): Promise<void> {
    const run = async (): Promise<void> => {
      if (settings === null) throw new Error("The settings are not loaded.");
      const next = structuredClone(settings);
      change(next);
      const patch = settingsPatch(settings, next);
      if (Object.keys(patch).length === 0) return;
      try {
        await backend.saveSettings(patch);
      } catch (error) {
        const reason = errorText(error);
        throw new Error(options.explain ? options.explain(reason) : reason);
      }
      for (const key of Object.keys(settings)) Reflect.deleteProperty(settings, key);
      Object.assign(settings, next);
      if (Object.hasOwn(patch, "look") || Object.hasOwn(patch, "themes")) void refreshLookStyles();
    };
    const result = queue.then(run);
    queue = result.catch(() => undefined);
    return result;
  }

  function open(id: string): void {
    const next = pages.find((page) => page.id === id);
    if (!next) return;
    const active = document.activeElement;
    if (active !== null && pagesNode.contains(active) && !next.node.contains(active)) {
      // A hidden page's focused select keeps WebKitGTK holding the keyboard, so focus must leave it.
      next.node.hidden = false;
      next.node.focus({ preventScroll: true });
    }
    current = id;
    search.value = "";
    applyView();
    live.opened(id);
  }

  search.addEventListener("input", applyView);
  const touchedByUser = (event: Event): void => {
    if (search.value.trim() === "") return;
    const node = event.target instanceof Element ? event.target.closest("details") : null;
    if (!(node instanceof HTMLDetailsElement) || !openedBySearch.has(node)) return;
    // Setting open fires toggle, synchronously in jsdom and later in WebKit. Both are the open
    // search made, so they are ignored until the user clicks the summary.
    if (event.type === "toggle" && openedProgrammatically.has(node)) return;
    openedProgrammatically.delete(node);
    touchedDuringSearch.add(node);
  };
  pagesNode.addEventListener("toggle", touchedByUser, true);
  pagesNode.addEventListener("click", (event) => {
    if (!(event.target instanceof Element) || !event.target.closest("summary")) return;
    queueMicrotask(() => { touchedByUser(event); });
  }, true);

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
    if (progress.status === "done") refresh();
  });
  backend.onOpenPage(open);
  backend.onUpdate((state) => {
    live.update(state);
  });
  backend.onSettingsChangedElsewhere(() => {
    load(typingNow());
  });
  backend.onThemeChanged(() => { void refreshLookStyles(); });
  backend.onStatus(showStatus);
  backend.getStatus().then(showStatus).catch((error: unknown) => {
    runtimeFailure = errorText(error);
    runtimeCauses = [];
    feedback();
  });
  load();
}
