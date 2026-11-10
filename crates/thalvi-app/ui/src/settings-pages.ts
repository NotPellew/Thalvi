// The settings pages: each builds its rows against the draft and edits it in place.

import type {
  AudioStatus,
  Backend,
  Draft,
  DraftOutcome,
  Ending,
  HistoryEntry,
  HistoryOutcome,
  ScriptActionKind,
  LiveEntry,
  OnDeviceEntry,
  SecondaryApi,
  SecondPassEntry,
  Settings,
  SpeechModelInfo,
  SpeechModelProgress,
  Status,
  SpeechEntry,
  WakeModelInfo,
  SwitchState,
  WritingOpening,
  WritingSettings,
} from "./contract.ts";
import { errorText } from "./contract.ts";
import type { Controls, Field } from "./controls.ts";
import { uid } from "./controls.ts";
import { el } from "./dom.ts";
import { applyLook, lookPicker } from "./look.ts";

export interface PageContext {
  draft: Settings;
  backend: Backend;
  controls: Controls;
  /** Microphone names, or why they could not be listed. */
  microphones: string[] | { error: string };
  /** The microphone actually open when the pages were built. */
  audio: AudioStatus;
  /** Why this desktop offers no hold key, or null where it does. */
  holdKeyUnavailable: string | null;
  /** Whether the OS starts Thalvi at login, or why that could not be read. */
  startAtLogin: SwitchRead;
  /** Whether Thalvi is in the app menu, or why that could not be read. */
  menuEntry: SwitchRead;
  /** The settings window, so a look picked here shows at once. */
  root: HTMLElement;
  /** Hooks the shell calls on live events; a page assigns the ones it shows. */
  live: {
    level: (level: number) => void;
    listening: (on: boolean) => void;
    audio: (audio: AudioStatus) => void;
    speechModel: (progress: SpeechModelProgress) => void;
    status: (status: Status) => void;
    /** The page with this id was opened. */
    opened: (id: string) => void;
    /** The app's state changed, so what a page shows from disk may be out of date. */
    refresh: () => void;
    /** Settings were saved, so what a page marks as unsaved is out of date. */
    saved: () => void;
  };
  /** Checks a page adds that must pass before Save sends; each returns null when it has nothing to check. */
  checks: (() => Promise<void> | null)[];
  /** The settings as last saved, or null before they are read. */
  saved: () => Settings | null;
  /** Saves the last saved settings with `change` applied, leaving other unsaved edits in the draft; says why in the footer when it fails. */
  applyNow: (change: (settings: Settings) => void) => Promise<void>;
}

/** An OS switch as read, or why it could not be. */
export type SwitchRead = SwitchState | { kind: "unreadable"; message: string };

export interface Page {
  id: string;
  title: string;
  node: HTMLElement;
}

const ENDINGS: readonly (readonly [Ending, string])[] = [
  ["paste", "Paste"],
  ["write", "Write out, then paste"],
  ["submit", "Paste and Enter"],
  ["task", "Run task"],
  ["scratch", "Scratch the draft"],
  ["cancel", "Cancel"],
];
const SCRIPT_ACTIONS: readonly (readonly [ScriptActionKind, string])[] = [
  ["open_url", "Open web address"],
  ["focus_app", "Focus app"],
  ["discord_message", "Send Discord message"],
];
const APIS: readonly (readonly [SecondaryApi, string])[] = [
  ["openai", "OpenAI-style"],
  ["meta", "Meta"],
];
const SPEECH_MODEL = "nemotron_160ms";
/** Like the config, a speech model name with a path separator is a folder of the user's own. */
const isModelFolder = (name: string): boolean => /[\\/]/.test(name);
const CUES: readonly (readonly [keyof Settings["cues"], string, string])[] = [
  ["wake", "Wake", "Thalvi heard the wake phrase and is listening."],
  ["provisional", "Ending heard", "An ending phrase was heard; the countdown has begun."],
  ["cancelled", "Cancelled", "The capture was cancelled."],
  ["accepted", "Accepted", "The capture was delivered."],
  ["error", "Error", "Something failed."],
];
const WAKE_MODELS: readonly (readonly [string, string])[] = [
  ["hey_thalvi", "Hey Thalvi"],
  ["hey_mycroft", "Hey Mycroft"],
  ["hey_jarvis", "Hey Jarvis"],
];
const CUSTOM_MODEL = "__custom";
const OUTCOMES: Record<HistoryOutcome, string> = { accepted: "Delivered", cancelled: "Cancelled", failed: "Failed" };
const DRAFT_OUTCOMES: Record<DraftOutcome, string> = {
  delivered: "Delivered.",
  cancelled: "You cancelled this one, so nothing was pasted.",
  failed: "This was not delivered.",
  maybePartial: "Thalvi may or may not have pasted this text. Check the window before trying again, or use Copy text.",
};
/** While one of these runs the status changes with every word; the draft is read again once it ends. */
const CAPTURE_STATES: readonly Status["state"][] = ["starting", "capturing", "pending", "writing", "thinking", "working"];
const WHEN = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

export function row(label: string, hint: string | null, control: Field | HTMLElement): HTMLElement {
  const labelId = uid("l");
  const field = control instanceof HTMLElement ? { node: control, field: null } : control;
  const labelable =
    field.field instanceof HTMLInputElement ||
    field.field instanceof HTMLSelectElement ||
    field.field instanceof HTMLTextAreaElement ||
    field.field instanceof HTMLButtonElement;
  const name =
    labelable && field.field
      ? el("label", { id: labelId, for: field.field.id }, label)
      : el("span", { id: labelId }, label);
  if (!labelable) {
    field.node.setAttribute("aria-labelledby", labelId);
    if (field.field === null) field.node.setAttribute("role", "group");
  }
  const what = el("div", { class: "what" }, name);
  if (hint) what.append(el("small", {}, hint));
  const result = el("div", { class: "row" }, what, el("div", { class: "ctl" }, field.node));
  result.dataset["label"] = label.toLowerCase();
  if (field.node.classList.contains("wide")) result.classList.add("wide");
  return result;
}

function group(title: string): HTMLElement {
  return el("h4", { "data-group": "" }, title);
}

function button(label: string, onClick: () => void, className = ""): HTMLButtonElement {
  const node = el("button", { type: "button", class: className }, label);
  node.addEventListener("click", onClick);
  return node;
}

function note(): HTMLElement {
  return el("p", { class: "note", role: "alert", hidden: true });
}

function say(target: HTMLElement, message: string): void {
  target.textContent = message;
  target.hidden = message === "";
}

function listText(values: string[]): string {
  return values.join(", ");
}

function parseList(value: string): string[] {
  return value
    .split(",")
    .map((part) => part.trim())
    .filter((part) => part !== "");
}

function normalize(phrase: string): string {
  return phrase.toLowerCase().split(/\s+/).filter(Boolean).join(" ");
}

/** Reads a phrase the way the desktop does: lowercase, runs of anything but letters and digits one space. */
function openingWords(phrase: string): string {
  return phrase.toLowerCase().split(/[^\p{Alphabetic}\p{N}]+/u).filter(Boolean).join(" ");
}

function miniField(label: string, field: Field): HTMLElement {
  return el("label", { class: "mini" }, el("span", {}, label), field.node);
}

function fileName(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

interface WakeHead {
  custom: boolean;
  customPath: string;
  render: () => void;
  sayProblem: (message: string) => void;
}

/** The dictation wake picker: catalog phrases with download badges, or a custom `.onnx` file. */
function wakeHeads(context: PageContext): HTMLElement[] {
  const { draft, controls, backend, checks, live } = context;
  const infos = new Map<string, WakeModelInfo>();
  const downloading = new Set<string>();
  const heads: WakeHead[] = [];
  let latestCheck = 0;

  function validate(target?: WakeHead): Promise<void> {
    const head = heads[0];
    if (head.custom && head.customPath === "") {
      const message = "Pick a wake model file with Browse… first.";
      (target ?? head).sayProblem(message);
      return Promise.reject(new Error(message));
    }
    const check = ++latestCheck;
    return backend.validateWakeModels(draft.wake.capture.model).then(
      () => {
        if (check === latestCheck) head.sayProblem("");
      },
      (error: unknown) => {
        if (check === latestCheck) {
          if (target) target.sayProblem(errorText(error));
          else head.sayProblem(errorText(error));
        }
        throw error;
      },
    );
  }
  checks.push(() => (heads[0].custom ? validate() : null));

  function head(title: string): HTMLElement[] {
    const config = draft.wake.capture;
    const name = "dictation";
    const picker = el("select", { id: uid("f") });
    const badge = el("span", { class: "badge" });
    const problem = note();
    const chosenFile = el("span", { class: "unit" });
    const state: WakeHead = {
      custom: !WAKE_MODELS.some(([model]) => model === config.model),
      customPath: "",
      render: () => undefined,
      sayProblem: (message) => {
        say(problem, message);
      },
    };
    if (state.custom) state.customPath = config.model;
    const download = button("Download", () => {
      const model = config.model;
      downloading.add(model);
      state.render();
      backend.downloadWakeModel(model).then(
        () => {
          downloading.delete(model);
          const info = infos.get(model);
          if (info) info.cached = true;
          for (const other of heads) other.render();
        },
        (error: unknown) => {
          downloading.delete(model);
          state.sayProblem(errorText(error));
          state.render();
        },
      );
    });
    const browse = button("Browse…", () => {
      backend.pickWakeModelFile().then(
        (path) => {
          if (path === null) return;
          state.customPath = path;
          config.model = path;
          controls.touched();
          for (const other of heads) other.render();
          validate(state).catch(() => undefined);
        },
        (error: unknown) => {
          state.sayProblem(errorText(error));
        },
      );
    });
    const customRow = el("div", { class: "trow file" }, el("span", { class: "unit" }, "Custom model"), browse, chosenFile);
    state.render = () => {
      picker.replaceChildren(
        ...WAKE_MODELS.map(([model, label]) => {
          const info = infos.get(model);
          const status = downloading.has(model) ? "Downloading…" : info ? (info.cached ? "Ready" : "Not downloaded") : "";
          const option = el("option", { value: model }, status === "" ? label : `${label} — ${status}`);
          return option;
        }),
        el("option", { value: CUSTOM_MODEL }, "Custom model…"),
      );
      picker.value = state.custom ? CUSTOM_MODEL : config.model;
      const info = infos.get(config.model);
      badge.textContent = state.custom
        ? state.customPath === ""
          ? "No file chosen"
          : "Custom model"
        : downloading.has(config.model)
          ? "Downloading…"
          : info
            ? info.cached
              ? "Ready"
              : "Not downloaded"
            : "Checking…";
      badge.dataset["on"] = String(!state.custom && info?.cached === true);
      download.hidden = state.custom || downloading.has(config.model) || info?.cached !== false;
      customRow.hidden = !state.custom;
      chosenFile.textContent = state.customPath === "" ? "" : fileName(state.customPath);
    };
    picker.addEventListener("change", () => {
      if (picker.value === CUSTOM_MODEL) {
        state.custom = true;
        if (state.customPath === "") state.sayProblem("Pick a wake model file with Browse… first.");
      } else {
        state.custom = false;
        config.model = picker.value;
        const start = infos.get(picker.value)?.threshold;
        if (start !== undefined) {
          const slider = threshold.field as HTMLInputElement;
          slider.value = String(start);
          slider.dispatchEvent(new Event("input"));
        }
        state.sayProblem("");
      }
      controls.touched();
      for (const other of heads) other.render();
    });
    const threshold = controls.slider(config.threshold, { min: 0.05, max: 1, step: 0.05 }, (value) => {
      config.threshold = value;
    });
    heads.push(state);
    const field: Field = {
      node: el("div", { class: "stack wide" }, el("div", { class: "trow pick" }, picker, badge, download), customRow, problem),
      field: picker,
    };
    return [
      group(title),
      row(`Wake model (${name})`, "Pick a built-in phrase, or your own .onnx wake model file.", field),
      row(
        `Threshold (${name})`,
        "Higher wakes less often by mistake; lower catches quieter speech.",
        threshold,
      ),
    ];
  }

  const nodes = head("Dictation wake word");
  function refresh(): void {
    backend.listWakeModels().then(
      (models) => {
        const changed = models.some((info) => infos.get(info.name)?.cached !== info.cached);
        for (const info of models) infos.set(info.name, info);
        // Rebuilding the options would disturb a dropdown the user has open.
        if (changed) for (const state of heads) state.render();
      },
      (error: unknown) => {
        for (const state of heads) state.sayProblem(errorText(error));
      },
    );
  }
  const before = live.refresh;
  live.refresh = () => {
    before();
    refresh();
  };
  refresh();
  for (const state of heads) state.render();
  return nodes;
}

function endingsTable({ draft, controls }: PageContext): HTMLElement {
  const list = el("div", { class: "table" });
  const problem = note();
  const phrase = controls.text("", () => undefined, { label: "New ending phrase", placeholder: "new phrase" });
  let chosen: Ending = "paste";
  const action = controls.select(ENDINGS, chosen, (value) => {
    chosen = value;
  }, "New ending action");

  function render(): void {
    list.replaceChildren(
      ...Object.entries(draft.endings).map(([words, ending]) => {
        const picker = controls.select(ENDINGS, ending, (value) => {
          draft.endings[words] = value;
        }, `Action for “${words}”`);
        const remove = button("Remove", () => {
          draft.endings = Object.fromEntries(Object.entries(draft.endings).filter(([known]) => known !== words));
          controls.touched();
          render();
        }, "quiet");
        remove.setAttribute("aria-label", `Remove “${words}”`);
        return el("div", { class: "trow" }, el("span", { class: "phrase" }, `“${words}”`), picker.node, remove);
      }),
    );
  }
  render();
  const add = button("Add", () => {
    const words = normalize((phrase.field as HTMLInputElement).value);
    if (words === "") {
      say(problem, "Type the phrase to add first.");
      return;
    }
    if (Object.hasOwn(draft.endings, words)) {
      say(problem, `“${words}” is already in the list.`);
      return;
    }
    say(problem, "");
    draft.endings[words] = chosen;
    (phrase.field as HTMLInputElement).value = "";
    controls.touched();
    render();
  });
  return el("div", { class: "stack wide" }, list, el("div", { class: "trow add phrase-add" }, phrase.node, action.node, add), problem);
}

/** The word list as chips with a remove button each, and one box whose + (or Enter) adds its word. */
function wordListTable(draft: Settings, controls: Controls): { node: HTMLElement; enable: (on: boolean) => void } {
  const chips = el("div", { class: "chips" });
  const problem = note();
  const entry = controls.text("", () => undefined, { label: "New word", placeholder: "Add a name or term" });
  const input = entry.field as HTMLInputElement;
  let enabled = true;

  function render(): void {
    const words = draft.stt.phrases ?? [];
    chips.replaceChildren(
      ...words.map((word) => {
        const remove = button("×", () => {
          draft.stt.phrases = (draft.stt.phrases ?? []).filter((kept) => kept !== word);
          controls.touched();
          render();
        }, "quiet");
        remove.setAttribute("aria-label", `Remove ${word}`);
        remove.disabled = !enabled;
        return el("span", { class: "badge chip" }, el("span", {}, word), remove);
      }),
    );
  }

  function addWord(): void {
    const word = input.value.trim();
    const words = draft.stt.phrases ?? [];
    if (word === "") {
      say(problem, "Type the word to add first.");
      return;
    }
    if (words.includes(word)) {
      say(problem, `${word} is already in the list.`);
      return;
    }
    say(problem, "");
    draft.stt.phrases = [...words, word];
    input.value = "";
    controls.touched();
    render();
    input.focus();
  }

  const add = button("+", addWord);
  add.setAttribute("aria-label", "Add the word");
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      addWord();
    }
  });
  render();
  return {
    node: el("div", { class: "stack wide" }, chips, el("div", { class: "trow add word-add" }, entry.node, add), problem),
    enable: (on) => {
      enabled = on;
      input.disabled = !on;
      add.disabled = !on;
      render();
    },
  };
}

/** The hold key has no setting here; the group appears only where the desktop gives the app no way to see it. */
function holdKey({ holdKeyUnavailable }: PageContext): HTMLElement[] {
  if (holdKeyUnavailable === null) return [];
  return [
    group("Hold key"),
    row("Hold a key to dictate", null, unavailable({ kind: "unavailable", reason: holdKeyUnavailable, line: null })),
    row("Hold a key to write", null, unavailable({ kind: "unavailable", reason: holdKeyUnavailable, line: null })),
  ];
}

function capture(context: PageContext): HTMLElement[] {
  const { draft, controls } = context;
  const pause = (key: "paste" | "submit" | "task", label: string): HTMLElement =>
    row(
      label,
      "Time to keep talking or cancel, 0 to 5 s.",
      controls.number(draft.ending_pause[key], { min: 0, max: 5, step: 0.1, unit: "s" }, (value) => {
        draft.ending_pause[key] = value;
      }),
    );
  return [
    ...wakeHeads(context),
    ...holdKey(context),
    group("Endings"),
    row("Ending phrases", "Say one to finish a capture. Every action needs at least one phrase.", endingsTable(context)),
    group("Quiet before acting"),
    pause("paste", "Quiet before paste"),
    pause("submit", "Quiet before paste and Enter"),
    pause("task", "Quiet before running a task"),
    ...limits(context),
  ];
}

function formatSize(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  if (bytes >= 1e6) return `${String(Math.round(bytes / 1e6))} MB`;
  return `${String(Math.round(bytes / 1e3))} kB`;
}

interface ModelCard {
  node: HTMLElement;
  /** Shows a download step, from this card's button or any other start. */
  progress: (event: SpeechModelProgress) => void;
  setSelected: (on: boolean) => void;
}

/** One catalog speech model: what it is, whether it is downloaded, and how to get it. */
function modelCard(info: SpeechModelInfo, backend: Backend): ModelCard {
  const badge = el("span", { class: "badge" });
  const selected = el("span", { class: "badge", "data-on": "true", hidden: true }, "Selected");
  const source = el("a", { href: info.source, target: "_blank", rel: "noreferrer" }, "Source");
  const meter = el("progress", { max: "100", value: "0", "aria-label": `Download of ${info.title}`, hidden: true });
  const status = el("span", { class: "unit", role: "status" });
  const device = el("small", { class: "device" });
  const problem = note();
  const download = button("Download", () => {
    say(problem, "");
    show({ name: info.name, percent: 0, status: "verifying", message: null });
    backend.downloadSpeechModel(info.name).then(
      () => {
        show({ name: info.name, percent: 100, status: "done", message: null });
      },
      (error: unknown) => {
        show({ name: info.name, percent: 0, status: "failed", message: errorText(error) });
      },
    );
  });
  source.addEventListener("click", (event) => {
    event.preventDefault();
    backend.openSpeechModelSource(info.name).catch((error: unknown) => {
      say(problem, errorText(error));
    });
  });

  function cached(on: boolean): void {
    badge.textContent = on ? "Downloaded" : "Not downloaded";
    badge.dataset["on"] = String(on);
    download.hidden = on;
  }

  function show(event: SpeechModelProgress): void {
    const running = event.status === "verifying" || event.status === "downloading";
    meter.hidden = !running;
    meter.value = event.percent;
    status.textContent = !running
      ? ""
      : event.status === "verifying"
        ? "Checking files…"
        : `Downloading… ${String(event.percent)}%`;
    download.disabled = running;
    if (event.status === "done") {
      cached(true);
      showDevice();
    }
    if (event.status === "failed") say(problem, event.message ?? "The download failed.");
  }

  function showDevice(): void {
    backend.speechDevice(info.name).then(
      (line) => {
        device.textContent = line;
      },
      (error: unknown) => {
        device.textContent = errorText(error);
      },
    );
  }
  showDevice();

  cached(info.cached);
  const details = el(
    "details",
    { class: "model-details" },
    el("summary", {}, "Details"),
    el("small", {}, `ID: ${info.name}. License: ${info.license}. Source: ${info.source}. `),
    source,
  );
  return {
    node: el(
      "div",
      { class: "model" },
      el("div", { class: "trow" }, el("span", { class: "phrase" }, info.title), el("span", { class: "unit" }, formatSize(info.size)), badge, selected),
      el("small", {}, info.description),
      el(
        "small",
        { class: "facts" },
        `Languages: ${info.languages.join(", ")}.${info.languages.length > 1 ? " The language is detected automatically." : ""}`,
      ),
      device,
      details,
      el("div", { class: "trow" }, download, meter, status),
      problem,
    ),
    progress: show,
    setSelected: (on) => {
      selected.hidden = !on;
    },
  };
}

/** The picker's entry for a folder the user points to. */
const OWN_FOLDER = "__folder";
/** The language filter's entry that lists every model. */
const ALL_LANGUAGES = "";

function modelOption(info: SpeechModelInfo): HTMLOptionElement {
  const languages = info.languages.length === 1 ? info.languages.join("") : `${String(info.languages.length)} languages`;
  return el("option", { value: info.name }, `${info.title} (${languages}, ${formatSize(info.size)})`);
}

const KIND_NAMES: Record<SpeechEntry["kind"], string> = {
  realtime: "Realtime speech server",
  thalvi_server: "Thalvi server",
  on_device: "On this computer",
  second_pass: "Second pass",
};
const LIVE_KINDS: readonly (readonly [LiveEntry["kind"], string])[] = [
  ["realtime", "Realtime speech server"],
  ["thalvi_server", "Through a Thalvi server"],
  ["on_device", "On this computer"],
];

function isLive(entry: SpeechEntry): entry is LiveEntry {
  return entry.kind !== "second_pass";
}

function hasKey(entry: SpeechEntry): boolean {
  if (entry.kind !== "realtime" && entry.kind !== "second_pass") return false;
  return entry.api_key === undefined ? entry.has_key === true : entry.api_key !== "";
}

function summary(entry: SpeechEntry): string {
  switch (entry.kind) {
    case "realtime":
      return `${entry.address} · ${entry.model}`;
    case "thalvi_server":
      return entry.address;
    case "on_device":
      return entry.model ?? SPEECH_MODEL;
    case "second_pass":
      return `${entry.address} · ${entry.model} · ${APIS.find(([value]) => value === entry.api)?.[1] ?? entry.api}`;
  }
}

function blankEntry(kind: SpeechEntry["kind"], name: string): SpeechEntry {
  const base = { id: `e-${Math.random().toString(36).slice(2, 10)}`, name, active: false };
  switch (kind) {
    case "realtime":
      return { ...base, kind, address: "", model: "" };
    case "thalvi_server":
      return { ...base, kind, address: "ws://127.0.0.1:8080" };
    case "on_device":
      return { ...base, kind };
    case "second_pass":
      return { ...base, kind, address: "", model: "", api: "openai" };
  }
}

/** The model and folder rows of an On this computer entry; they write into `working`. */
function onDeviceRows(working: OnDeviceEntry, { controls, backend, live }: PageContext, sharedModels?: Promise<SpeechModelInfo[]>): { rows: HTMLElement[]; validate: () => Promise<void> } {
  const catalog = new Map<string, ModelCard>();
  const listed: SpeechModelInfo[] = [];
  // Only narrows the model list; it is not saved and does not reach recognition.
  let language = ALL_LANGUAGES;
  let chosen = working.model ?? "";
  const isCatalog = (name: string): boolean => name === SPEECH_MODEL || listed.some((info) => info.name === name);
  // The entry stays picked while its path is still empty.
  let ownFolderPicked = isModelFolder(chosen);
  const speechModel = controls.text(
    ownFolderPicked ? chosen : "",
    (value) => {
      chosen = value.trim();
      ownFolderPicked = chosen !== "";
      syncModel();
    },
    { placeholder: "/path/to/model-folder", label: "Your own model folder" },
  );
  const modelInput = speechModel.field as HTMLInputElement;
  const folderResult = el("p", { class: "note", role: "status", hidden: true });
  const languagePicker = el("select", { id: uid("f"), "aria-label": "Show models for" });
  languagePicker.append(el("option", { value: ALL_LANGUAGES }, "All languages"));
  const modelPicker = el("select", { id: uid("f"), "aria-label": "Speech model" });
  modelPicker.append(el("option", { value: "", disabled: true }, "Loading models…"));
  const modelList = el("div", { class: "models" });
  const listProblem = note();
  const ownFolder = el("div", { class: "own" }, el("span", { class: "unit" }, "Your own model folder"));

  live.speechModel = (event) => {
    catalog.get(event.name)?.progress(event);
  };

  const inLanguage = (info: SpeechModelInfo): boolean => language === ALL_LANGUAGES || info.languages.includes(language);

  function listModels(): void {
    modelPicker.replaceChildren(
      ...listed.filter(inLanguage).map(modelOption),
      el("option", { value: OWN_FOLDER }, "Your own model folder"),
    );
    showSelection();
  }

  function showSelection(): void {
    const active = chosen === "" ? SPEECH_MODEL : chosen;
    const own = ownFolderPicked;
    modelPicker.value = own ? OWN_FOLDER : active;
    for (const [name, card] of catalog) {
      card.node.hidden = own || name !== active;
      card.setSelected(!own && name === active);
    }
    ownFolder.dataset["active"] = String(own);
  }

  languagePicker.addEventListener("change", () => {
    language = languagePicker.value;
    const active = chosen === "" ? SPEECH_MODEL : chosen;
    const current = listed.find((info) => info.name === active);
    const first = listed.find(inLanguage);
    if (!ownFolderPicked && first && !(current && inLanguage(current))) {
      chosen = first.name;
      syncModel();
    }
    listModels();
  });

  modelPicker.addEventListener("change", () => {
    if (modelPicker.value === OWN_FOLDER) {
      ownFolderPicked = true;
      chosen = modelInput.value.trim();
      modelInput.focus();
    } else {
      ownFolderPicked = false;
      chosen = modelPicker.value;
      modelInput.value = "";
    }
    syncModel();
  });

  (sharedModels ?? backend.listSpeechModels()).then(
    (models) => {
      for (const info of models) {
        listed.push(info);
        const card = modelCard(info, backend);
        catalog.set(info.name, card);
        modelList.append(card.node);
      }
      const spoken = new Set(models.flatMap((info) => info.languages));
      languagePicker.append(...Array.from(spoken).sort((a, b) => a.localeCompare(b)).map((name) => el("option", { value: name }, name)));
      listModels();
    },
    (error: unknown) => {
      listModels();
      say(listProblem, errorText(error));
    },
  );

  function showFolder(message: string, valid: boolean): void {
    folderResult.classList.toggle("ok", valid);
    say(folderResult, message);
  }

  // Bumped by every check and edit, so a slow answer never describes a path that is no longer typed.
  let latestCheck = 0;

  function checkFolder(path: string): Promise<void> {
    const check = ++latestCheck;
    return backend.validateSpeechModelFolder(path).then(
      () => {
        if (check === latestCheck) showFolder("This folder holds a speech model.", true);
      },
      (error: unknown) => {
        if (check === latestCheck) showFolder(errorText(error), false);
        throw error;
      },
    );
  }

  const checkButton = button("Check folder", () => {
    const path = modelInput.value.trim();
    if (path === "") showFolder("Type the folder path first.", false);
    else if (isCatalog(path)) showFolder(`${path} is downloaded on first use; there is no folder to check.`, false);
    else checkFolder(path).catch(() => undefined);
  });
  ownFolder.append(el("div", { class: "trow check" }, speechModel.node, checkButton), folderResult);
  const speechModelField: Field = {
    node: el("div", { class: "stack wide" }, modelPicker, modelList, listProblem, ownFolder),
    field: modelPicker,
  };

  function syncModel(): void {
    latestCheck += 1;
    say(folderResult, "");
    if (chosen !== "") {
      working.model = chosen;
    } else {
      delete working.model;
    }
    showSelection();
  }

  return {
    rows: [
      row("Show models for", "Filters this list. Each model recognizes the languages on its card.", { node: languagePicker, field: languagePicker }),
      row(
        "Speech model",
        `For recognition on this computer. Pick a model; it downloads on first use, or with its Download button. A model you pick applies from the next capture. Or point to your own model folder, which holds encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx and tokens.txt.`,
        speechModelField,
      ),
    ],
    validate: () => {
      if (ownFolderPicked && chosen === "") {
        showFolder("Type the folder path first.", false);
        return Promise.reject(new Error("Type the folder path of your own speech model first."));
      }
      return ownFolderPicked && !isCatalog(chosen) ? checkFolder(chosen) : Promise.resolve();
    },
  };
}

function speech(context: PageContext): HTMLElement[] {
  const { draft, controls, backend } = context;
  const entries = (): SpeechEntry[] => draft.stt.entries;
  const activeLive = (): LiveEntry | undefined => entries().filter(isLive).find((entry) => entry.active);
  const savedEntries = (): SpeechEntry[] => context.saved()?.stt.entries ?? [];
  const isSaved = (entry: SpeechEntry): boolean => JSON.stringify(savedEntries().find((known) => known.id === entry.id)) === JSON.stringify(entry);

  const liveList = el("div", { class: "table" });
  const secondList = el("div", { class: "table" });
  const editor = el("div", { class: "editor", hidden: true });
  const problem = note();
  let editingId: string | null = null;
  let editingSecond = false;
  let editingAdd = false;
  const nextDictation = el("p", { class: "next-dictation", role: "status" }, "");
  const retryProblem = note();
  const useServer = button("Use a speech server instead", () => {
    openEditor(null, "realtime");
  });
  const retry = button("Retry", retryDownload);
  const summaryActions = el("div", { class: "trow", hidden: true }, useServer, retry, retryProblem);
  let currentStatus: Status = { state: "listening" };
  const modelTitles = new Map<string, string>();
  const speechModels: Promise<SpeechModelInfo[]> = backend.listSpeechModels();
  speechModels.then(
    (models) => {
      for (const info of models) modelTitles.set(info.name, info.title);
      render();
    },
    () => undefined,
  );
  backend.onStatus((status) => {
    currentStatus = status;
    updateSummary();
  });
  backend.getStatus().then(
    (status) => {
      currentStatus = status;
      updateSummary();
    },
    () => undefined,
  );

  function modelTitleFor(entry: OnDeviceEntry): string {
    const model = entry.model ?? SPEECH_MODEL;
    return modelTitles.get(model) ?? model;
  }

  function summaryEntry(): LiveEntry | undefined {
    const draftActive = activeLive();
    const savedActive = savedEntries().filter(isLive).find((entry) => entry.active);
    if (draftActive === undefined) return savedActive;
    if (savedActive === undefined) return draftActive;
    if (draftActive.id !== savedActive.id) return savedActive;
    return isSaved(draftActive) ? draftActive : savedActive;
  }

  function statusState(status: Status): string {
    switch (status.state) {
      case "listening":
      case "starting":
      case "capturing":
      case "pending":
      case "writing":
      case "thinking":
      case "working":
      case "done":
        return "Ready";
      case "loading":
        return status.notice;
      case "paused":
        return "Paused — resume to dictate";
      case "failed":
        return status.headline;
    }
  }

  function updateSummary(): void {
    const entry = summaryEntry();
    if (entry === undefined) {
      nextDictation.textContent = "";
      return;
    }
    const modelPart = entry.kind === "on_device" ? ` · ${modelTitleFor(entry)}` : "";
    nextDictation.textContent = `Next dictation: ${entry.name}${modelPart} — ${statusState(currentStatus)}`;
    const preparing = entry.kind === "on_device" && (currentStatus.state === "loading" || currentStatus.state === "failed");
    summaryActions.hidden = !preparing;
    // Only a catalog model can be downloaded again; a folder of the user's own is fixed on disk.
    retry.hidden = currentStatus.state !== "failed" || (entry.kind === "on_device" && isModelFolder(entry.model ?? SPEECH_MODEL));
    if (!preparing) say(retryProblem, "");
  }

  function retryDownload(): void {
    const entry = summaryEntry();
    if (entry?.kind !== "on_device") return;
    say(retryProblem, "");
    retry.disabled = true;
    // The model card's downloader checks every file again and fetches what is missing or corrupt.
    backend
      .downloadSpeechModel(entry.model ?? SPEECH_MODEL)
      .then(() => backend.setListening(true))
      .catch((error: unknown) => {
        say(retryProblem, errorText(error));
      })
      .finally(() => {
        retry.disabled = false;
      });
  }

  function entryDescription(entry: SpeechEntry): string {
    switch (entry.kind) {
      case "realtime":
        return "Dictation through a realtime speech server.";
      case "thalvi_server":
        return "Dictation through a Thalvi server.";
      case "on_device":
        return `Recognition with ${modelTitleFor(entry)}.`;
      case "second_pass":
        return "Transcribes each accepted capture again.";
    }
  }

  function entryDetails(entry: SpeechEntry): HTMLElement {
    return el(
      "details",
      { class: "entry-details" },
      el("summary", {}, "Details"),
      el("small", {}, `ID: ${entry.id}. ${summary(entry)}.`),
    );
  }

  function uniqueName(base: string): string {
    const taken = new Set(entries().map((entry) => entry.name.toLowerCase()));
    for (let count = 1; ; count += 1) {
      const name = count === 1 ? base : `${base} ${String(count)}`;
      if (!taken.has(name.toLowerCase())) return name;
    }
  }

  /** Changes a saved entry's flag at once, as one click; an entry not saved yet changes in the draft only. */
  function flag(id: string, change: (list: SpeechEntry[]) => void, after: () => void = () => undefined): void {
    const savedAlready = savedEntries().some((known) => known.id === id);
    if (!savedAlready) {
      change(entries());
      controls.touched();
      render();
      after();
      return;
    }
    context.applyNow((settings) => {
      change(settings.stt.entries);
    }).then(() => {
      render();
      after();
    }, () => {
      render();
    });
  }

  function useLive(id: string): void {
    flag(id, (list) => {
      for (const entry of list) if (isLive(entry)) entry.active = entry.id === id;
    }, checkWordList);
  }

  function tick(id: string, on: boolean): void {
    flag(id, (list) => {
      const entry = list.find((known) => known.id === id);
      if (entry) entry.active = on;
    });
  }

  function move(id: string, by: -1 | 1): void {
    const list = entries();
    const here = list.findIndex((entry) => entry.id === id);
    let there = here + by;
    while (there >= 0 && there < list.length && list[there]?.kind !== "second_pass") there += by;
    const moved = list[here];
    const swapped = list[there];
    if (moved === undefined || swapped === undefined) return;
    list[here] = swapped;
    list[there] = moved;
    controls.touched();
    render();
  }

  function remove(entry: SpeechEntry): void {
    if (editingId === entry.id) closeEditor(false);
    draft.stt.entries = entries().filter((known) => known.id !== entry.id);
    controls.touched();
    render();
  }

  function badges(entry: SpeechEntry): HTMLElement[] {
    const found: HTMLElement[] = [];
    if (KIND_NAMES[entry.kind] !== entry.name) found.push(el("span", { class: "badge" }, KIND_NAMES[entry.kind]));
    if (isLive(entry) && entry.active) found.push(el("span", { class: "badge", "data-on": "true" }, "In use"));
    if (hasKey(entry)) found.push(el("span", { class: "badge" }, "Key saved"));
    if (!isSaved(entry)) found.push(el("span", { class: "badge", "data-saved": "false" }, "Not saved"));
    return found;
  }

  function liveRow(entry: LiveEntry): HTMLElement {
    const use = button("Use", () => {
      useLive(entry.id);
    }, "quiet");
    use.setAttribute("aria-label", `Use ${entry.name}`);
    const edit = button("Edit", () => {
      openEditor(entry);
    }, "quiet");
    edit.setAttribute("aria-label", `Edit ${entry.name}`);
    edit.setAttribute("aria-expanded", String(editingId === entry.id));
    const drop = button("Remove", () => {
      remove(entry);
    }, "quiet");
    drop.setAttribute("aria-label", `Remove ${entry.name}`);
    drop.disabled = entry.active;
    if (entry.active) drop.title = "Switch to another server first.";
    return el(
      "div",
      { class: "trow server", "data-id": entry.id },
      el("span", { class: "phrase" }, entry.name),
      ...badges(entry),
      el("small", { class: "unit" }, entryDescription(entry)),
      ...deviceLine(entry),
      ...(entry.active ? [] : [use]),
      edit,
      drop,
      entryDetails(entry),
    );
  }

  /** Where an on-device catalog model runs; a model folder of the user's own has no line. */
  function deviceLine(entry: LiveEntry): HTMLElement[] {
    const model = entry.kind === "on_device" ? (entry.model ?? SPEECH_MODEL) : "";
    if (model === "" || isModelFolder(model)) return [];
    const line = el("small", { class: "device" });
    backend.speechDevice(model).then(
      (text) => {
        line.textContent = text;
      },
      (error: unknown) => {
        line.textContent = errorText(error);
      },
    );
    return [line];
  }

  function secondRow(entry: SecondPassEntry, index: number, count: number): HTMLElement {
    const ticked = controls.toggle(entry.active, (on) => {
      tick(entry.id, on);
    });
    (ticked.field as HTMLInputElement).setAttribute("aria-label", `Run ${entry.name}`);
    const up = button("↑", () => {
      move(entry.id, -1);
    }, "quiet");
    up.setAttribute("aria-label", `Move ${entry.name} up`);
    up.disabled = index === 0;
    const down = button("↓", () => {
      move(entry.id, 1);
    }, "quiet");
    down.setAttribute("aria-label", `Move ${entry.name} down`);
    down.disabled = index === count - 1;
    const edit = button("Edit", () => {
      openEditor(entry);
    }, "quiet");
    edit.setAttribute("aria-label", `Edit ${entry.name}`);
    edit.setAttribute("aria-expanded", String(editingId === entry.id));
    const drop = button("Remove", () => {
      remove(entry);
    }, "quiet");
    drop.setAttribute("aria-label", `Remove ${entry.name}`);
    return el(
      "div",
      { class: "trow server", "data-id": entry.id },
      ticked.node,
      el("span", { class: "phrase" }, entry.name),
      ...badges(entry),
      el("small", { class: "unit" }, entryDescription(entry)),
      up,
      down,
      edit,
      drop,
      entryDetails(entry),
    );
  }

  function editButtonFor(id: string): HTMLButtonElement | null {
    for (const list of [liveList, secondList]) {
      const row = list.querySelector<HTMLElement>(`.trow.server[data-id="${id}"]`);
      const found = row?.querySelector<HTMLButtonElement>('button[aria-label^="Edit "]');
      if (found) return found;
    }
    return null;
  }

  function render(): void {
    if (editingId !== null && !entries().some((entry) => entry.id === editingId)) {
      editingId = null;
      editor.hidden = true;
      editor.replaceChildren();
    }
    const liveChildren: HTMLElement[] = [];
    for (const entry of entries().filter(isLive)) {
      liveChildren.push(liveRow(entry));
      if (editingId === entry.id && !editingSecond) liveChildren.push(editor);
    }
    if (editingAdd && !editingSecond) liveChildren.push(editor);
    const second = entries().filter((entry): entry is SecondPassEntry => entry.kind === "second_pass");
    const secondChildren: HTMLElement[] = [];
    second.forEach((entry, index) => {
      secondChildren.push(secondRow(entry, index, second.length));
      if (editingId === entry.id && editingSecond) secondChildren.push(editor);
    });
    if (editingAdd && editingSecond) secondChildren.push(editor);
    if (!liveChildren.includes(editor) && !secondChildren.includes(editor)) liveChildren.push(editor);
    liveList.replaceChildren(...liveChildren);
    secondList.replaceChildren(...secondChildren);
    secondList.hidden = second.length === 0 && !((editingId !== null && editingSecond) || (editingAdd && editingSecond));
    updateSummary();
  }
  context.live.saved = render;
  // The model loads behind this long-lived window, so what runs where is asked again when the app's state changes.
  const before = context.live.refresh;
  context.live.refresh = () => {
    before();
    render();
  };

  function closeEditor(returnFocus = true): void {
    const wasId = editingId;
    editingId = null;
    editingAdd = false;
    editor.replaceChildren();
    editor.hidden = true;
    context.live.speechModel = () => undefined;
    for (const list of [liveList, secondList]) {
      for (const btn of list.querySelectorAll('button[aria-label^="Edit "]')) btn.setAttribute("aria-expanded", "false");
    }
    if (returnFocus && wasId !== null) {
      editButtonFor(wasId)?.focus();
      queueMicrotask(() => {
        editButtonFor(wasId)?.focus();
      });
    }
  }

  function openEditor(existing: SpeechEntry | null, kind?: SpeechEntry["kind"]): void {
    editingAdd = existing === null;
    editingId = existing?.id ?? null;
    editingSecond = existing ? existing.kind === "second_pass" : kind === "second_pass";
    const working: SpeechEntry = existing ? structuredClone(existing) : blankEntry(kind ?? "realtime", uniqueName(KIND_NAMES[kind ?? "realtime"]));
    const message = note();
    let validate: () => Promise<void> = () => Promise.resolve();
    const rows: HTMLElement[] = [
      row("Name", "How this server is listed, and named when something goes wrong.", controls.text(working.name, (value) => {
        working.name = value;
      })),
    ];
    const address = (hint: string, placeholder: string): HTMLElement =>
      row("Address", hint, controls.text("address" in working ? working.address : "", (value) => {
        if ("address" in working) working.address = value;
      }, { placeholder }));
    const modelRow = (): HTMLElement =>
      row("Model", "The model named in each request.", controls.text("model" in working ? (working.model ?? "") : "", (value) => {
        if (working.kind === "realtime" || working.kind === "second_pass") working.model = value;
      }));
    const key: { input?: HTMLInputElement } = {};
    let clearKey = false;
    const keyRow = (): HTMLElement => {
      const saved = hasKey(working);
      const field = controls.text("", () => undefined, {
        type: "password",
        placeholder: saved ? "Saved — leave blank to keep it" : "",
        label: "API key",
      });
      key.input = field.field as HTMLInputElement;
      const cleared = el("small", { class: "unit", hidden: true }, "The saved key is cleared when you save this server.");
      const clear = button("Clear saved key", () => {
        clearKey = true;
        clear.hidden = true;
        cleared.hidden = false;
      }, "quiet");
      clear.hidden = !saved;
      return row("API key", "Stored in the desktop config file. The saved key is never shown again.", {
        node: el("div", { class: "stack wide" }, field.node, clear, cleared),
        field: field.field,
      });
    };
    switch (working.kind) {
      case "realtime":
        rows.push(address("The ws:// or wss:// address of a speech server that speaks Meta's realtime protocol, such as the self-hosted one.", "ws://127.0.0.1:8012"), modelRow(), keyRow());
        break;
      case "thalvi_server":
        rows.push(address("The Thalvi server's address. Its own recognizer key stays in the server's .env.", "ws://127.0.0.1:8080"));
        break;
      case "on_device": {
        const built = onDeviceRows(working, context, speechModels);
        rows.push(...built.rows);
        validate = built.validate;
        break;
      }
      case "second_pass": {
        rows.push(
          address("Where the second transcription is sent.", "http://127.0.0.1:9000/v1"),
          modelRow(),
          row("API", "Which transcription API the endpoint speaks.", controls.select(APIS, working.api, (value) => {
            working.api = value;
          })),
          keyRow(),
        );
        break;
      }
    }
    const done = button("Save server", () => {
      say(message, "");
      const typed = key.input?.value ?? "";
      const entry = structuredClone(working);
      if (entry.kind === "realtime" || entry.kind === "second_pass") {
        if (typed !== "") entry.api_key = typed;
        else if (clearKey) entry.api_key = "";
      }
      entry.name = entry.name.trim();
      const others = entries().filter((known) => known.id !== entry.id);
      if (others.some((known) => known.name.toLowerCase() === entry.name.toLowerCase())) {
        say(message, `Two speech servers are called "${entry.name}". Give each its own name.`);
        return;
      }
      const sent: SpeechEntry & { has_key?: boolean } = structuredClone(entry);
      delete sent.has_key;
      validate()
        .then(() => backend.checkSpeechEntry(sent))
        .then(
          () => {
            const index = entries().findIndex((known) => known.id === entry.id);
            if (index >= 0) entries()[index] = entry;
            else entries().push(entry);
            controls.touched();
            closeEditor();
            render();
            checkWordList();
          },
          (error: unknown) => {
            say(message, errorText(error));
          },
        );
    }, "primary");
    const cancel = button("Cancel", () => {
      closeEditor();
    }, "quiet");
    const headingId = uid("editor");
    const heading = el(
      "h4",
      { id: headingId },
      existing
        ? `Editing “${existing.name}” · ${existing.kind === "second_pass" ? "second pass" : "live dictation"}`
        : `Add a ${KIND_NAMES[working.kind].toLowerCase()}`,
    );
    editor.setAttribute("aria-labelledby", headingId);
    editor.replaceChildren(
      heading,
      ...rows,
      el("div", { class: "trow add" }, done, cancel),
      message,
    );
    editor.hidden = false;
    render();
    editor.querySelector<HTMLElement>("input, select, textarea")?.focus();
  }

  const kindPicker = controls.select(LIVE_KINDS, "realtime", () => undefined, "Kind of speech server to add");
  function addSelected(): void {
    openEditor(null, (kindPicker.field as HTMLSelectElement).value as LiveEntry["kind"]);
  }
  const addLive = button("Add", addSelected);
  addLive.setAttribute("aria-label", "Add a speech server");
  const addSecond = button("Add a second-pass API", () => {
    openEditor(null, "second_pass");
  });
  const liveBlock = el("div", { class: "stack wide" }, liveList, problem, el("div", { class: "trow add" }, kindPicker.node, addLive));
  const secondBlock = el("div", { class: "stack wide" }, secondList, addSecond);

  const writing = draft.writing;
  const writingUrl = controls.text(writing?.url ?? "", () => { syncWriting(); }, { placeholder: "http://127.0.0.1:8000/v1" });
  const writingModel = controls.text(writing?.model ?? "", () => { syncWriting(); });
  const writingKey = controls.text(writing?.api_key ?? "", () => { syncWriting(true); }, { type: "password", placeholder: writing?.has_key === true ? "A key is saved" : "" });
  const writingInputs = [writingUrl, writingModel, writingKey].map((field) => field.field as HTMLInputElement);
  const defaultPrompt = draft.writing_default_prompt;
  const promptArea = el("textarea", { id: uid("f"), rows: "8", spellcheck: "false" });
  promptArea.value = writing?.prompt ?? defaultPrompt;
  const restorePrompt = button("Restore default prompt", () => {
    promptArea.value = defaultPrompt;
    syncWriting();
    controls.touched();
  }, "quiet");
  promptArea.addEventListener("input", () => {
    syncWriting();
    controls.touched();
  });
  const writingPromptField = { node: el("div", { class: "stack wide" }, promptArea, restorePrompt), field: promptArea };
  const clearWritingKey = button("Clear saved writing key", () => {
    const input = writingKey.field as HTMLInputElement;
    input.value = "";
    input.placeholder = "Cleared on Save";
    syncWriting(true);
    clearWritingKey.hidden = true;
    controls.touched();
  });
  clearWritingKey.hidden = writing?.has_key !== true;
  const writingKeyField = { node: el("div", { class: "stack" }, writingKey.node, clearWritingKey), field: writingKey.field };

  /** The prompt is sent only when it is not the default, so an unchanged one follows future defaults. */
  function syncWriting(replaceKey = false): void {
    const [urlInput, modelInput, keyInput] = writingInputs;
    const custom = promptArea.value.trim() !== defaultPrompt.trim();
    restorePrompt.disabled = !custom;
    if (!custom && writingInputs.every((input) => input.value.trim() === "") && (draft.writing?.openings ?? []).length === 0) {
      draft.writing = null;
      return;
    }
    const next: WritingSettings = { ...draft.writing, url: urlInput?.value ?? "", model: modelInput?.value ?? "", ...(replaceKey ? { api_key: keyInput?.value ?? "" } : {}) };
    if (custom) next.prompt = promptArea.value;
    else delete next.prompt;
    draft.writing = next;
  }
  restorePrompt.disabled = promptArea.value.trim() === defaultPrompt.trim();

  const openingsList = el("div", { class: "stack wide" });
  const openingProblem = note();
  const openingPhrase = controls.text("", () => undefined, { label: "New opening phrase", placeholder: "write me an email" });

  function setOpenings(openings: WritingOpening[]): void {
    const next: WritingSettings = { url: "", model: "", ...draft.writing };
    if (openings.length > 0) next.openings = openings;
    else delete next.openings;
    draft.writing = next;
    syncWriting();
    controls.touched();
    renderOpenings();
  }

  function renderOpenings(): void {
    const openings = draft.writing?.openings ?? [];
    openingsList.replaceChildren(
      ...openings.map((opening) => {
        const prompt = el("textarea", { id: uid("f"), rows: "5", spellcheck: "false", "aria-label": `Prompt for “${opening.phrase}”` });
        prompt.value = opening.prompt;
        prompt.addEventListener("input", () => {
          opening.prompt = prompt.value;
          controls.touched();
        });
        const remove = button("Remove", () => {
          setOpenings(openings.filter((known) => known !== opening));
        }, "quiet");
        remove.setAttribute("aria-label", `Remove “${opening.phrase}”`);
        return el("div", { class: "stack wide" }, el("div", { class: "trow" }, el("span", { class: "phrase" }, `“${opening.phrase}”`), remove), prompt);
      }),
    );
  }
  renderOpenings();
  const addOpening = button("Add", () => {
    const input = openingPhrase.field as HTMLInputElement;
    const phrase = openingWords(input.value);
    if (phrase === "") {
      say(openingProblem, "Type the phrase to add first.");
      return;
    }
    const openings = draft.writing?.openings ?? [];
    if (openings.some((known) => known.phrase === phrase)) {
      say(openingProblem, `“${phrase}” is already in the list.`);
      return;
    }
    say(openingProblem, "");
    input.value = "";
    setOpenings([...openings, { phrase, prompt: promptArea.value }]);
  });
  const openingsField = el(
    "div",
    { class: "stack wide" },
    openingsList,
    el("div", { class: "trow add phrase-add" }, openingPhrase.node, addOpening),
    openingProblem,
  );

  const words = wordListTable(draft, controls);
  const wordProblem = note();
  const wordRow = row(
    "Word list",
    "Names and terms to spell your way, added one at a time. Recognition on this computer favours them, and so does a recognizer that supports it.",
    words.node,
  );
  words.node.append(wordProblem);
  let asked = 0;
  function checkWordList(): void {
    const ask = (asked += 1);
    wordRow.classList.add("off");
    say(wordProblem, "");
    const active = activeLive();
    if (active === undefined || active.kind === "on_device") {
      wordRow.classList.remove("off");
      words.enable(true);
      return;
    }
    backend
      .wordListSupport(active)
      .then((supported) => {
        if (ask !== asked) return;
        wordRow.classList.toggle("off", !supported);
        words.enable(true);
      })
      .catch((error: unknown) => {
        if (ask !== asked) return;
        wordRow.classList.remove("off");
        words.enable(false);
        say(wordProblem, errorText(error));
      });
  }
  render();
  checkWordList();

  return [
    group("Live dictation"),
    nextDictation,
    summaryActions,
    row(
      "Speech servers",
      "One is in use for dictation. Use switches at once and applies from the next capture; a capture already running keeps its recognizer. Conversation mode always uses the Thalvi server's own recognizer.",
      liveBlock,
    ),
    wordRow,
    group("Second pass"),
    row(
      "Second-pass servers",
      "Ticked servers transcribe each accepted capture again. The first one in this order that answers replaces the live text; the rest are only recorded. Ticking applies at once.",
      secondBlock,
    ),
    group("Writing"),
    row(
      "Writing endpoint address",
      "A chat completions endpoint that writes a dictation out as proper text when you say “write that out” or hold the write key. Only the words you said are sent. Saving never calls it.",
      writingUrl,
    ),
    row("Writing model", "The model the endpoint should use.", writingModel),
    row("Writing API key", "Stored in the desktop config file; unchanged keeps the saved key.", writingKeyField),
    row(
      "Writing prompt",
      "The instructions the writing model gets with every dictation; the words you said go in as the message itself. Blank is not allowed. Saving never calls the model.",
      writingPromptField,
    ),
    row(
      "Opening phrases",
      "When a dictation you write out begins with one of these phrases, its prompt is used instead of the Writing prompt; the longest matching phrase wins. A new rule starts as a copy of the Writing prompt. Case and punctuation are ignored. Saving never calls the model.",
      openingsField,
    ),
  ];
}

function microphone({ draft, backend, controls, microphones, live, audio }: PageContext): HTMLElement[] {
  const names = Array.isArray(microphones) ? microphones : [];
  const saved = draft.microphone;
  const options: (readonly [string, string])[] = [
    ["", "System default"],
    ...names.map((name) => [name, name] as const),
  ];
  if (saved !== null && !names.includes(saved)) options.push([saved, `${saved} (not found)`]);
  const picker = controls.select(options, saved ?? "", (value) => {
    draft.microphone = value === "" ? null : value;
  });
  const pickerInput = picker.field as HTMLSelectElement;
  const whyLocked = el("p", { class: "note why", id: uid("n") }, "Pause listening above to change the microphone.");
  pickerInput.setAttribute("aria-describedby", whyLocked.id);
  const active = el("output", { class: "badge" }, "");
  const showAudio = (next: AudioStatus): void => {
    pickerInput.disabled = !next.paused;
    whyLocked.hidden = next.paused;
    active.textContent = next.active ?? "None — no microphone is open";
    active.dataset["on"] = String(next.active !== null);
  };
  showAudio(audio);
  live.audio = showAudio;
  const savedDevice = row("Saved device", Array.isArray(microphones) ? "Saving writes this choice; it does not prove a microphone opened." : null, picker);
  savedDevice.querySelector(".what")?.append(whyLocked);
  const meter = el("b");
  live.level = (level) => {
    meter.style.transform = `scaleX(${Math.min(1, Math.max(0, level)).toFixed(3)})`;
  };
  const listening = controls.toggle(true, () => undefined);
  const listeningInput = listening.field as HTMLInputElement;
  const problem = note();
  listeningInput.addEventListener("change", () => {
    say(problem, "");
    const wanted = listeningInput.checked;
    backend.setListening(wanted).catch((error: unknown) => {
      listeningInput.checked = !wanted;
      say(problem, errorText(error));
    });
  });
  live.listening = (on) => {
    listeningInput.checked = on;
  };
  const rows = [
    group("Right now"),
    row("Listening", "Turns the microphone on or off now; this is not part of Save.", {
      node: el("span", { class: "stack" }, listening.node, problem),
      field: listeningInput,
    }),
    row("Active microphone", "The device actually open now. This is different from the saved choice below.", {
      node: active,
      field: el("span", { id: uid("f") }),
    }),
    row("Level", "Moves while Thalvi is listening or capturing.", {
      node: el("span", { class: "meter", role: "meter", "aria-valuemin": "0", "aria-valuemax": "1" }, meter),
      field: el("span", { id: uid("f") }),
    }),
    group("Saved with Save"),
    savedDevice,
  ];
  if (!Array.isArray(microphones)) {
    rows.unshift(el("p", { class: "note", role: "alert" }, `Microphones could not be listed: ${microphones.error}`));
  }
  return rows;
}

function overlay({ draft, controls }: PageContext): HTMLElement[] {
  return [
    group("Overlay"),
    row("Show the overlay", "A small pill at the top of the screen while Thalvi is capturing.",
      controls.toggle(draft.overlay.show, (value) => {
        draft.overlay.show = value;
      })),
    row("Show the draft", "The text so far, under the pill.",
      controls.toggle(draft.overlay.draft, (value) => {
        draft.overlay.draft = value;
      })),
  ];
}

function look({ draft, controls, root }: PageContext): HTMLElement[] {
  return [
    group("Look"),
    row("Look", "How the overlay and this window look. The overlay changes after the current capture.",
      lookPicker(draft.look, (next) => {
        draft.look = next;
        applyLook(root, next);
        controls.touched();
      })),
  ];
}

function sounds({ draft, controls }: PageContext): HTMLElement[] {
  return [group("Sounds"), ...CUES.map(([key, label, hint]) =>
    row(`${label} sound`, hint, controls.toggle(draft.cues[key], (value) => {
      draft.cues[key] = value;
    })),
  )];
}

function feedback(context: PageContext): HTMLElement[] {
  return [...overlay(context), ...look(context), ...sounds(context)];
}

function appsTable({ draft, controls }: PageContext, onApps: () => void): HTMLElement {
  const list = el("div", { class: "cards" });
  const problem = note();
  const name = controls.text("", () => undefined, { label: "New app name", placeholder: "app name" });

  function render(): void {
    list.replaceChildren(
      ...Object.entries(draft.apps).map(([appName, app]) => {
        const remove = button("Remove", () => {
          draft.apps = Object.fromEntries(Object.entries(draft.apps).filter(([known]) => known !== appName));
          controls.touched();
          render();
          onApps();
        }, "quiet");
        remove.setAttribute("aria-label", `Remove ${appName}`);
        const edit = (label: string, values: string[], set: (next: string[]) => void): HTMLElement =>
          miniField(label, controls.text(listText(values), (value) => {
            set(parseList(value));
          }, { label: `${label} of ${appName}` }));
        return el(
          "fieldset",
          { class: "card" },
          el("legend", {}, appName),
          edit("Window classes", app.classes, (next) => { app.classes = next; }),
          edit("Executables", app.executables, (next) => { app.executables = next; }),
          edit("Keys pressed after focusing", app.input_keys, (next) => { app.input_keys = next; }),
          remove,
        );
      }),
    );
  }
  render();
  const add = button("Add app", () => {
    const appName = (name.field as HTMLInputElement).value.trim();
    if (appName === "") {
      say(problem, "Type the app's name first.");
      return;
    }
    if (appName in draft.apps) {
      say(problem, `An app named ${appName} already exists.`);
      return;
    }
    say(problem, "");
    draft.apps[appName] = { classes: [], executables: [], input_keys: [] };
    (name.field as HTMLInputElement).value = "";
    controls.touched();
    render();
    onApps();
  });
  return el("div", { class: "stack wide" }, list, el("div", { class: "trow add" }, name.node, add), problem);
}

function scriptsTable({ draft, controls }: PageContext, appNames: HTMLElement): HTMLElement {
  const list = el("div", { class: "cards" });

  function render(): void {
    list.replaceChildren(
      ...draft.scripts.map((script, index) => {
        const remove = button("Remove", () => {
          draft.scripts.splice(index, 1);
          controls.touched();
          render();
        }, "quiet");
        remove.setAttribute("aria-label", `Remove script ${String(index + 1)}`);
        return el(
          "div",
          { class: "card" },
          miniField("Phrase", controls.text(script.phrase, (value) => { script.phrase = value; }, { label: `Phrase of script ${String(index + 1)}` })),
          miniField("Action", controls.select(SCRIPT_ACTIONS, script.action, (value) => { script.action = value; }, `Action of script ${String(index + 1)}`)),
          miniField("Destination", controls.text(script.destination, (value) => { script.destination = value; }, { list: appNames.id, label: `Destination of script ${String(index + 1)}` })),
          miniField("Label", controls.text(script.label ?? "", (value) => { script.label = value === "" ? null : value; }, { label: `Label of script ${String(index + 1)}` })),
          remove,
        );
      }),
    );
  }
  render();
  const add = button("Add script", () => {
    draft.scripts.push({ phrase: "", action: "open_url", destination: "", label: null });
    controls.touched();
    render();
  });
  return el("div", { class: "stack wide" }, list, el("div", { class: "trow add" }, add));
}

function tasks(context: PageContext): HTMLElement[] {
  const appNames = el("datalist", { id: uid("apps") });
  const refresh = (): void => {
    appNames.replaceChildren(...Object.keys(context.draft.apps).map((name) => el("option", { value: name })));
  };
  refresh();
  return [
    appNames,
    group("Apps"),
    row("Apps", "Windows a script can focus, matched by window class or executable.", appsTable(context, refresh)),
    group("Scripts"),
    row("Scripts", "Say “do that” after one of these phrases to run it. For Discord, the words after the phrase are the message.", scriptsTable(context, appNames)),
  ];
}

function limits({ draft, controls }: PageContext): HTMLElement[] {
  return [
    group("Capture limits"),
    row("Stop after silence", "Stops a capture that hears no words for this long, 10 to 600 s.",
      controls.number(draft.capture_limits.idle, { min: 10, max: 600, step: 1, unit: "s" }, (value) => {
        draft.capture_limits.idle = value;
      })),
    row("Stop after", "Stops any capture that runs this long, 60 to 3600 s.",
      controls.number(draft.capture_limits.max, { min: 60, max: 3600, step: 1, unit: "s" }, (value) => {
        draft.capture_limits.max = value;
      })),
  ];
}

interface SwitchOps {
  read: () => Promise<SwitchState>;
  set: (on: boolean) => Promise<void>;
  copy?: () => Promise<void>;
}

/** A switch that takes effect at once, shows the OS's state, and re-reads it after a failure; or the reason there is none. */
function switchRow(label: string, hint: string, shown: SwitchRead, controls: Controls, ops: SwitchOps): HTMLElement {
  if (shown.kind === "unavailable") return row(label, hint, unavailable(shown, ops.copy));
  const toggle = controls.toggle(shown.kind === "available" && shown.on, () => undefined);
  const input = toggle.field as HTMLInputElement;
  const problem = note();
  if (shown.kind === "unreadable") {
    input.disabled = true;
    say(problem, shown.message);
  } else {
    say(problem, shown.problem ?? "");
  }
  input.addEventListener("change", () => {
    say(problem, "");
    const wanted = input.checked;
    input.disabled = true;
    ops
      .set(wanted)
      .catch(async (error: unknown) => {
        say(problem, errorText(error));
        input.checked = await ops
          .read()
          .then((state) => state.kind === "available" && state.on)
          .catch(() => !wanted);
      })
      .finally(() => {
        input.disabled = false;
      });
  });
  return row(label, hint, { node: el("span", { class: "stack" }, toggle.node, problem), field: input });
}

function unavailable({ reason, line }: Extract<SwitchState, { kind: "unavailable" }>, copy?: () => Promise<void>): HTMLElement {
  const node = el("span", { class: "stack wide" }, el("p", { class: "note ok" }, reason));
  if (line !== null && copy) {
    const problem = note();
    const copyButton = button("Copy", () => {
      say(problem, "");
      copy()
        .then(() => {
          copyButton.textContent = "Copied";
        })
        .catch((error: unknown) => {
          say(problem, errorText(error));
        });
    });
    node.append(el("code", { class: "line" }, line), copyButton, problem);
  }
  return node;
}

function startup({ backend, controls, startAtLogin, menuEntry }: PageContext): HTMLElement[] {
  return [
    group("Startup"),
    switchRow(
      "Start Thalvi when I log in",
      "Starts the voice app at login, listening as it normally does. This takes effect now; it is not part of Save.",
      startAtLogin,
      controls,
      { read: () => backend.getStartAtLogin(), set: (on) => backend.setStartAtLogin(on), copy: () => backend.copyLoginLine() },
    ),
    switchRow(
      "Show Thalvi in the app menu",
      "Lets you start Thalvi from your desktop's app menu. This takes effect now; it is not part of Save.",
      menuEntry,
      controls,
      { read: () => backend.getMenuEntry(), set: (on) => backend.setMenuEntry(on) },
    ),
  ];
}

function recordings({ draft, backend, controls }: PageContext): HTMLElement[] {
  const kinds = [
    ["Metrics", "metrics", "When each wake, capture and model call happened, how long it took and how it ended. No words or audio."],
    ["Text", "text", "The words you dictated and what Thalvi did with them."],
    ["Audio", "audio", "What the microphone heard during each capture, and about two seconds before each wake word."],
  ] as const;
  const problem = note();
  const open = button("Open folder", () => {
    say(problem, "");
    backend.openRecordings().catch((error: unknown) => {
      say(problem, errorText(error));
    });
  });
  return [
    group("Recordings"),
    ...kinds.map(([label, kind, hint]) =>
      row(`${label} recording`, `${hint} Kept on this computer only. Off unless you turn it on.`,
        controls.toggle(draft.recordings[kind], (value) => {
          draft.recordings[kind] = value;
        })),
    ),
    group("Right now"),
    row("Recordings folder", "Everything is stored on this computer. Opening it is not part of Save.", {
      node: el("span", { class: "stack" }, open, problem),
      field: open,
    }),
  ];
}

function entryCard(entry: HistoryEntry, remove: () => void, copyText: (text: string) => Promise<void>): HTMLElement {
  const when = new Date(entry.timestamp_ms);
  const time = el("time", { datetime: when.toISOString() }, WHEN.format(when));
  const header = el("header", {}, time);
  if (entry.target !== null) header.append(el("span", { class: "target" }, entry.target));
  header.append(el("span", { class: "outcome" }, OUTCOMES[entry.outcome]));
  const result = el("p", { class: "note", role: "status", hidden: true });
  const copy = button("Copy", () => {
    copyText(entry.text).then(
      () => {
        result.className = "note ok";
        say(result, "Copied to the clipboard.");
      },
      (error: unknown) => {
        result.className = "note";
        say(result, errorText(error));
      },
    );
  }, "quiet");
  copy.setAttribute("aria-label", `Copy the entry from ${time.textContent}`);
  const del = button("Delete", remove, "quiet");
  del.setAttribute("aria-label", `Delete the entry from ${time.textContent}`);
  return el("article", { class: "entry", "data-outcome": entry.outcome }, header, el("p", {}, entry.text), result, el("span", { class: "trow" }, copy, del));
}

/** A button that asks before it acts: it is replaced by the question and a yes and a no. */
function confirmed(label: string, question: string, yes: string, no: string, act: () => void): HTMLElement {
  const ask = button(label, () => {
    ask.hidden = true;
    sure.hidden = false;
    keep.focus();
  });
  const close = (): void => {
    sure.hidden = true;
    ask.hidden = false;
    ask.focus();
  };
  const keep = button(no, close);
  const sure = el("span", { class: "trow", role: "group", "aria-label": question, hidden: true }, question, button(yes, () => {
    close();
    act();
  }), keep);
  return el("span", { class: "stack" }, ask, sure);
}

/** The last dictation: editable, with Copy, Reset and Discard. Editing sends and copies nothing. */
function lastDictation({ backend, live }: PageContext): { nodes: HTMLElement[]; refresh: () => void } {
  let current: Draft | null = null;
  const empty = el("p", { class: "note ok" }, "Nothing to recover yet. The last dictation appears here, also when it failed or you cancelled it.");
  const outcome = el("p", { class: "note ok" });
  const newer = el("p", { class: "note why", hidden: true }, "A newer dictation is waiting. Discard this one to see it.");
  const text = el("textarea", { rows: "4", spellcheck: "false", "aria-label": "Last dictation text" });
  const done = el("p", { class: "note ok", role: "status", hidden: true });
  const problem = note();
  const copy = button("Copy text", () => {
    say(problem, "");
    backend.copyText(text.value).then(
      () => {
        say(done, "Copied to the clipboard.");
      },
      (error: unknown) => {
        say(done, "");
        say(problem, errorText(error));
      },
    );
  });
  const reset = button("Reset to original", () => {
    if (current === null) return;
    text.value = current.original;
    sync();
    saveEdit();
  }, "quiet");
  const discard = button("Discard", () => {
    say(problem, "");
    say(done, "");
    backend.discardDraft().then(refresh, (error: unknown) => {
      say(problem, errorText(error));
    });
  }, "quiet");
  const editor = el("div", { class: "stack wide", hidden: true }, outcome, newer, text, el("span", { class: "trow" }, copy, reset, discard), done);

  function saveEdit(): void {
    latestRefresh += 1;
    backend.editDraft(text.value).catch((error: unknown) => {
      say(problem, errorText(error));
    });
  }
  function sync(): void {
    if (current === null) return;
    copy.disabled = text.value.trim() === "";
    reset.disabled = text.value === current.original;
  }
  function show(next: Draft | null): void {
    if (next?.id !== current?.id) {
      say(done, "");
      say(problem, "");
    }
    if (next !== null && next.id !== current?.id) text.value = next.text;
    current = next;
    empty.hidden = next !== null;
    editor.hidden = next === null;
    if (next === null) return;
    outcome.textContent = next.target === null ? DRAFT_OUTCOMES[next.outcome] : `${DRAFT_OUTCOMES[next.outcome]} Meant for ${next.target}.`;
    newer.hidden = !next.newerWaiting;
    sync();
  }
  // Bumped by every refresh and edit, so a slow answer never replaces a newer draft or an edit.
  let latestRefresh = 0;
  function refresh(): void {
    const asked = ++latestRefresh;
    backend.getDraft().then(
      (next) => {
        if (asked === latestRefresh) show(next);
      },
      (error: unknown) => {
        if (asked === latestRefresh) say(problem, errorText(error));
      },
    );
  }
  text.addEventListener("input", () => {
    sync();
    saveEdit();
  });
  live.status = (status) => {
    if (!CAPTURE_STATES.includes(status.state)) refresh();
  };
  refresh();

  return {
    nodes: [
      group("Last dictation"),
      row("Last dictation", "Fix it, then copy it. Editing sends and copies nothing. Kept in memory only, until you discard it or quit.",
        { node: el("div", { class: "stack wide" }, empty, editor, problem), field: text }),
    ],
    refresh,
  };
}

function history(context: PageContext): HTMLElement[] {
  const { draft, backend, controls, live } = context;
  const last = lastDictation(context);
  const list = el("div", { class: "entries" });
  const problem = note();
  const nothing = el("p", { class: "note ok" }, "Nothing has been kept yet.");

  function refresh(): void {
    backend.getHistory().then(
      (entries) => {
        say(problem, "");
        list.replaceChildren(...(entries.length === 0 ? [nothing] : entries.map((entry) => entryCard(entry, () => { change(backend.deleteHistoryEntry(entry.id)); }, (text) => backend.copyText(text)))));
      },
      (error: unknown) => {
        say(problem, errorText(error));
        list.replaceChildren();
      },
    );
  }
  function change(done: Promise<void>): void {
    say(problem, "");
    done.then(refresh, (error: unknown) => {
      say(problem, errorText(error));
    });
  }
  live.opened = (id) => {
    if (id === "history") {
      refresh();
      last.refresh();
    }
  };
  refresh();

  return [
    ...last.nodes,
    group("Keeping"),
    row("Keep a history", "What you dictate, when, where it went and how it ended; never audio. Kept on this computer only, never uploaded. Turning this off stops new entries and deletes none: delete them below.",
      controls.toggle(draft.history.enabled, (value) => {
        draft.history.enabled = value;
      })),
    row("Entries to keep", "The oldest entries go first past this number, 1 to 1000.",
      controls.number(draft.history.limit, { min: 1, max: 1000, step: 1 }, (value) => {
        draft.history.limit = value;
      })),
    group("Kept so far"),
    row("Recent dictations", "Newest first. Deleting takes effect at once; it is not part of Save.", el("div", { class: "stack wide" }, problem, list)),
    row("Clear history", "Deletes every entry for good, and a damaged history file with them.",
      confirmed("Clear history", "Delete every entry? This cannot be undone.", "Yes, clear history", "Keep history", () => {
        change(backend.clearHistory());
      })),
  ];
}

function about({ backend }: PageContext): HTMLElement[] {
  const version = el("output", { class: "badge" }, "…");
  backend
    .appVersion()
    .then((value) => {
      version.textContent = value;
    })
    .catch((error: unknown) => {
      version.textContent = errorText(error);
    });
  return [
    group("About"),
    row("Version", null, { node: version, field: el("span", { id: uid("f") }) }),
  ];
}

function app(context: PageContext): HTMLElement[] {
  return [...startup(context), ...recordings(context), ...about(context)];
}

const PAGES: readonly (readonly [string, string, (context: PageContext) => HTMLElement[]])[] = [
  ["speech", "Speech", speech],
  ["microphone", "Microphone", microphone],
  ["capture", "Capture", capture],
  ["feedback", "Feedback and appearance", feedback],
  ["tasks", "Apps & tasks", tasks],
  ["history", "History", history],
  ["app", "App", app],
];

/** Wraps each group heading and the rows after it in a box the looks can frame. */
function grouped(nodes: HTMLElement[]): HTMLElement[] {
  const result: HTMLElement[] = [];
  let current: HTMLElement | null = null;
  for (const node of nodes) {
    if (node instanceof HTMLDataListElement) {
      result.push(node);
      continue;
    }
    if (node.hasAttribute("data-group")) current = null;
    if (current === null) {
      current = el("div", { class: "grp" });
      result.push(current);
    }
    current.append(node);
  }
  return result;
}

export function buildPages(context: PageContext): Page[] {
  return PAGES.map(([id, title, build]) => {
    const node = el("section", { "data-page": id, "aria-labelledby": `page-${id}` }, el("h3", { id: `page-${id}` }, title), ...grouped(build(context)));
    return { id, title, node };
  });
}
