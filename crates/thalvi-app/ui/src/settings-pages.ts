// The settings pages: each builds its rows from the settings and applies a change as soon as it is valid.

import type {
  AudioStatus,
  Backend,
  ChatBuild,
  Draft,
  DraftOutcome,
  Ending,
  HistoryEntry,
  HistoryOutcome,
  HoldKeyDefaults,
  ScriptActionKind,
  ScriptSettings,
  AppSettings,
  LiveEntry,
  OnDeviceEntry,
  OpenAiRequest,
  SecondaryApi,
  Settings,
  SpeechModelInfo,
  SpeechModelProgress,
  Status,
  SpeechEntry,
  UpdateChannel,
  UpdateState,
  WakeModelInfo,
  SwitchState,
  WritingAppStyle,
  WritingOpening,
  WritingSettings,
} from "./contract.ts";
import { DownloadFailure, errorText } from "./contract.ts";
import type { Controls, Field } from "./controls.ts";
import { uid } from "./controls.ts";
import { disclosureMemory, el, focusAfterRemoval, setChildren } from "./dom.ts";
import { applyLook, lookPicker } from "./look.ts";

/** What a control says once its value is applied. */
export const APPLIED_TEXT = "Applied.";
/** What it says while a capture runs: the running one keeps the settings it started with. */
export const APPLIED_LATER_TEXT = "Applied. From the next dictation.";

/** While one of these runs, a change is held until the capture is over. */
export const DURING_CAPTURE: readonly Status["state"][] = ["starting", "capturing", "pending", "writing", "thinking", "working", "transcribing", "merging", "cleaningUp"];

/** The settings as the desktop holds them: pages read them and change them only through `apply`. */
type DeepReadonly<T> = T extends (infer U)[] ? readonly DeepReadonly<U>[] : T extends object ? { readonly [K in keyof T]: DeepReadonly<T[K]> } : T;

export interface PageContext {
  settings: DeepReadonly<Settings>;
  backend: Backend;
  controls: Controls;
  /** Microphone names, or why they could not be listed. */
  microphones: string[] | { error: string };
  /** The microphone actually open when the pages were built. */
  audio: AudioStatus;
  /** Why this desktop offers no hold key, or null where it does. */
  holdKeyUnavailable: string | null;
  /** Why this desktop cannot match app styles, or null where it can. */
  appStylesUnavailable: string | null;
  /** The keys this desktop starts with. */
  holdKeyDefaults: HoldKeyDefaults;
  /** Whether the OS starts Thalvi at login, or why that could not be read. */
  startAtLogin: SwitchRead;
  /** Whether Thalvi is in the app menu, or why that could not be read. */
  menuEntry: SwitchRead;
  /** The settings window, so a look picked here shows at once. */
  root: HTMLElement;
  /** Shows the page with this id. */
  open: (id: string) => void;
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
    /** The update's state changed. */
    update: (state: UpdateState) => void;
  };
  /** Applies `change` on top of the settings now held; rejects with the plain reason it was refused, and then nothing changed. */
  apply: (change: (settings: Settings) => void, options?: ApplyOptions) => Promise<void>;
}

export interface ApplyOptions {
  /** Rewrites the reason a refused change is shown with. */
  explain?: (reason: string) => string;
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
const OPEN_AI_REQUESTS: readonly (readonly [OpenAiRequest, string])[] = [
  ["transcriptions", "Transcriptions"],
  ["chat", "Chat with audio"],
];
const SPEECH_MODEL = "nemotron_160ms";
/** Like the config, a speech model name with a path separator is a folder of the user's own. */
const isModelFolder = (name: string): boolean => /[\\/]/.test(name);
const CUES: readonly (readonly [keyof Settings["cues"], string, string])[] = [
  ["wake", "Wake", "Thalvi heard the wake phrase and is listening."],
  ["provisional", "Ending heard", "An ending phrase was heard; the countdown began."],
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
const CAPTURE_STATES: readonly Status["state"][] = ["starting", "capturing", "pending", "writing", "thinking", "working", "cleaningUp"];
const WHEN = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

/**
 * Characters a hint may use and still fit on one line at a 400 px window, in every look.
 * At `max-width: 560px` the page padding is 10 px a side. Neon leaves a folded row 372 px.
 * Holo and Gunmetal pad the group as well (16 px and 14 px a side) and draw a 1 px or 3 px
 * border, leaving 340 px and 344 px. Their hints are 11.5 px monospace (`--mono`, Noto Sans
 * Mono here) at 6.90 px a character of ordinary prose, so `floor(340 / 6.90)` is 49. Neon is
 * wider. 48 stays inside that and inside the kind-help line, which has no row padding.
 */
export const HINT_BUDGET = 48;

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

/** Open disclosures, for the life of this window. Keyed by page, group and entry, not by the node a rebuild replaces. */
const disclosed = disclosureMemory();

/**
 * Secondary controls behind the existing native Details pattern.
 * A default starts closed. A changed value stays closed and the summary names what changed.
 * An invalid value opens the disclosure so it is never hidden. A secret is only "key set".
 */
function details(
  id: string,
  summary: string,
  body: HTMLElement[],
  options: { open?: boolean; invalid?: boolean } = {},
): HTMLDetailsElement {
  const node = el("details", { class: "fold", "data-fold": id }, el("summary", {}, summary), ...body);
  const open = options.invalid === true || disclosed.has(id);
  node.open = open;
  if (options.invalid === true) disclosed.add(id);
  const remember = (): void => {
    if (node.open) disclosed.add(id);
    else disclosed.delete(id);
  };
  node.addEventListener("toggle", remember);
  // A summary click flips the disclosure. jsdom does that without a toggle event, so the click is remembered too.
  node.querySelector("summary")?.addEventListener("click", () => {
    queueMicrotask(remember);
  });
  return node;
}

/** The catalog threshold of a dictation wake model; a file of the user's own starts at 0.5. */
function defaultThreshold(model: string): number {
  if (model === "hey_thalvi") return 0.7;
  return 0.5;
}

/** A number as its row shows it: a slider prints two decimals and drops trailing zeros. */
export function shownNumber(value: number): string {
  return value.toFixed(2).replace(/\.?0+$/, "");
}

/** A short summary of values that differ from the defaults; empty when nothing does. */
function changedSummary(parts: readonly (readonly [string, string | number, string | number])[]): string {
  const text = (value: string | number): string => typeof value === "number" ? shownNumber(value) : value;
  const changed = parts.filter(([, value, start]) => value !== start).map(([name, value]) => `${name} ${text(value)}`);
  return changed.length === 0 ? "" : ` · ${changed.join(", ")}`;
}

function button(label: string, onClick: () => void, className = ""): HTMLButtonElement {
  const node = el("button", { type: "button", class: className }, label);
  node.addEventListener("click", () => {
    if (node.getAttribute("aria-disabled") !== "true") onClick();
  });
  return node;
}

/** Marks a `button` busy: unlike `disabled`, it keeps keyboard focus, and its presses are ignored. */
function busy(node: HTMLButtonElement, on: boolean): void {
  if (on) node.setAttribute("aria-disabled", "true");
  else node.removeAttribute("aria-disabled");
}

function note(): HTMLElement {
  return el("p", { class: "note", role: "alert", hidden: true });
}

function say(target: HTMLElement, message: string): void {
  target.textContent = message;
  target.hidden = message === "";
}

function listText(values: readonly string[]): string {
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
  const { settings, controls, backend, live, apply } = context;
  const infos = new Map<string, WakeModelInfo>();
  const downloading = new Set<string>();
  const heads: WakeHead[] = [];

  function head(title: string): HTMLElement[] {
    const name = "dictation";
    const picker = el("select", { id: uid("f") });
    const badge = el("span", { class: "badge" });
    const problem = note();
    const chosenFile = el("span", { class: "unit" });
    const applied = el("small", { class: "applied", role: "status" });
    const model = (): string => settings.wake.capture.model;
    const state: WakeHead = {
      custom: !WAKE_MODELS.some(([known]) => known === model()),
      customPath: "",
      render: () => undefined,
      sayProblem: (message) => {
        say(problem, message);
      },
    };
    if (state.custom) state.customPath = model();
    const download = button("Download", () => {
      const wanted = model();
      downloading.add(wanted);
      state.render();
      backend.downloadWakeModel(wanted).then(
        () => {
          downloading.delete(wanted);
          const info = infos.get(wanted);
          if (info) info.cached = true;
          for (const other of heads) other.render();
        },
        (error: unknown) => {
          downloading.delete(wanted);
          state.sayProblem(errorText(error));
          state.render();
        },
      );
    });

    /** Applies a model, and the threshold it starts at when it has one; says why not, and keeps the model in use. */
    function use(wanted: string, threshold: number | undefined, wasCustom: boolean): Promise<boolean> {
      state.sayProblem("");
      applied.textContent = "";
      return apply((next) => {
        next.wake.capture.model = wanted;
        if (threshold !== undefined) next.wake.capture.threshold = threshold;
      }).then(
        () => {
          applied.textContent = controls.confirmation();
          return true;
        },
        (error: unknown) => {
          state.custom = wasCustom;
          state.sayProblem(errorText(error));
          return false;
        },
      );
    }

    /** Checks the model file is a wake model, then applies it. */
    function useFile(path: string, wasCustom: boolean): void {
      backend.validateWakeModels(path).then(
        () => use(path, undefined, wasCustom),
        (error: unknown) => {
          state.sayProblem(errorText(error));
        },
      ).then(() => {
        for (const other of heads) other.render();
      }, () => undefined);
    }

    const browse = button("Browse…", () => {
      backend.pickWakeModelFile().then(
        (path) => {
          if (path === null) return;
          state.customPath = path;
          state.render();
          useFile(path, state.custom);
        },
        (error: unknown) => {
          state.sayProblem(errorText(error));
        },
      );
    });
    const customRow = el("div", { class: "trow file" }, el("span", { class: "unit" }, "Custom model"), browse, chosenFile);
    state.render = () => {
      picker.replaceChildren(
        ...WAKE_MODELS.map(([known, label]) => {
          const info = infos.get(known);
          const status = downloading.has(known) ? "Downloading…" : info ? (info.cached ? "Ready" : "Not downloaded") : "";
          return el("option", { value: known }, status === "" ? label : `${label} — ${status}`);
        }),
        el("option", { value: CUSTOM_MODEL }, "Custom model…"),
      );
      picker.value = state.custom ? CUSTOM_MODEL : model();
      const info = infos.get(model());
      badge.textContent = state.custom
        ? state.customPath === ""
          ? "No file chosen"
          : "Custom model"
        : downloading.has(model())
          ? "Downloading…"
          : info
            ? info.cached
              ? "Ready"
              : "Not downloaded"
            : "Checking…";
      badge.dataset["on"] = String(!state.custom && info?.cached === true);
      download.hidden = state.custom || downloading.has(model()) || info?.cached !== false;
      customRow.hidden = !state.custom;
      chosenFile.textContent = state.customPath === "" ? "" : fileName(state.customPath);
    };
    picker.addEventListener("change", () => {
      const wasCustom = state.custom;
      if (picker.value === CUSTOM_MODEL) {
        state.custom = true;
        if (state.customPath === "") state.sayProblem("Pick a wake model file with Browse… first.");
        else useFile(state.customPath, wasCustom);
      } else {
        state.custom = false;
        const start = infos.get(picker.value)?.threshold;
        if (start !== undefined) {
          const slider = threshold.field as HTMLInputElement;
          slider.value = String(start);
          slider.dispatchEvent(new Event("input"));
        }
        use(picker.value, start, wasCustom).then((used) => {
          if (!used) {
            const slider = threshold.field as HTMLInputElement;
            slider.value = String(settings.wake.capture.threshold);
            slider.dispatchEvent(new Event("input"));
          }
          for (const other of heads) other.render();
        }, () => undefined);
      }
      for (const other of heads) other.render();
    });
    const threshold = controls.slider(settings.wake.capture.threshold, { min: 0.05, max: 1, step: 0.05 }, (value) =>
      apply((next) => {
        next.wake.capture.threshold = value;
      }),
    );
    heads.push(state);
    const field: Field = {
      node: el("div", { class: "stack wide" }, el("div", { class: "trow pick" }, picker, badge, download), customRow, applied, problem),
      field: picker,
    };
    const thresholdRow = row(
      `Threshold (${name})`,
      "Higher wakes less often; lower catches more.",
      threshold,
    );
    const shown = settings.wake.capture.threshold;
    const start = defaultThreshold(model());
    return [
      group(title),
      row(`Wake model (${name})`, "A built-in phrase, or your own .onnx file.", field),
      details(`listening/${title}/threshold`, `Details${changedSummary([["threshold", shown, start]])}`, [thresholdRow]),
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

function endingsTable({ settings, controls, apply }: PageContext): HTMLElement {
  const list = el("div", { class: "table" });
  const problem = note();
  const phrase = controls.draftText("", () => undefined, { label: "New ending phrase", placeholder: "new phrase" });
  let chosen: Ending = "paste";
  const action = controls.draftSelect(ENDINGS, chosen, (value) => {
    chosen = value;
  }, "New ending action");

  /** Applies a change to the list and shows it; says why not in the table's note. */
  function change(edit: (endings: Record<string, Ending>) => void): Promise<void> {
    say(problem, "");
    return apply((next) => {
      edit(next.endings);
    }).then(render, (error: unknown) => {
      say(problem, errorText(error));
    });
  }

  let removes: [string, HTMLButtonElement][] = [];

  function render(): void {
    const removed = removes.findIndex(([words, node]) => node === document.activeElement && !Object.hasOwn(settings.endings, words));
    removes = [];
    list.replaceChildren(
      ...Object.entries(settings.endings).map(([words, ending]) => {
        const picker = controls.select(ENDINGS, ending, (value) =>
          apply((next) => {
            next.endings[words] = value;
          }), `Action for “${words}”`);
        const remove = button("Remove", () => {
          void change((endings) => {
            Reflect.deleteProperty(endings, words);
          });
        }, "quiet");
        remove.setAttribute("aria-label", `Remove “${words}”`);
        removes.push([words, remove]);
        return el("div", { class: "trow" }, el("span", { class: "phrase" }, `“${words}”`), picker.node, remove);
      }),
    );
    if (removed !== -1) focusAfterRemoval(removes.map(([, node]) => node), removed, phrase.field);
  }
  render();
  const add = button("Add", () => {
    const input = phrase.field as HTMLInputElement;
    const words = normalize(input.value);
    if (words === "") {
      say(problem, "Type the phrase to add first.");
      return;
    }
    if (Object.hasOwn(settings.endings, words)) {
      say(problem, `“${words}” is already in the list.`);
      return;
    }
    const action = chosen;
    void change((endings) => {
      endings[words] = action;
    }).then(() => {
      if (Object.hasOwn(settings.endings, words)) input.value = "";
    });
  });
  return el("div", { class: "stack wide" }, list, el("div", { class: "trow add phrase-add" }, phrase.node, action.node, add), problem);
}

/** The word list as chips with a remove button each, and one box whose + (or Enter) adds its word. */
function wordListTable({ settings, controls, apply }: PageContext): { node: HTMLElement; enable: (on: boolean) => void } {
  const chips = el("div", { class: "chips" });
  const problem = note();
  const entry = controls.draftText("", () => undefined, { label: "New word", placeholder: "Add a name or term" });
  const input = entry.field as HTMLInputElement;
  let enabled = true;

  function change(edit: (words: string[]) => string[]): Promise<boolean> {
    say(problem, "");
    return apply((next) => {
      const words = edit(next.stt.phrases ?? []);
      if (words.length > 0) next.stt.phrases = words;
      else delete next.stt.phrases;
    }).then(
      () => {
        render();
        return true;
      },
      (error: unknown) => {
        say(problem, errorText(error));
        return false;
      },
    );
  }

  function render(): void {
    const words = settings.stt.phrases ?? [];
    chips.replaceChildren(
      ...words.map((word) => {
        const remove = button("×", () => {
          void change((kept) => kept.filter((known) => known !== word));
        }, "quiet");
        remove.setAttribute("aria-label", `Remove ${word}`);
        remove.disabled = !enabled;
        return el("span", { class: "badge chip" }, el("span", {}, word), remove);
      }),
    );
  }

  function addWord(): void {
    const word = input.value.trim();
    const words = settings.stt.phrases ?? [];
    if (word === "") {
      say(problem, "Type the word to add first.");
      return;
    }
    if (words.includes(word)) {
      say(problem, `${word} is already in the list.`);
      return;
    }
    void change((kept) => [...kept, word]).then((applied) => {
      if (!applied) return;
      input.value = "";
      input.focus();
    });
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

/** The Wake words switch: off, Thalvi keeps listening but only the hold keys start a capture. */
function wakeSwitch({ settings, controls, backend, apply }: PageContext): HTMLElement[] {
  const state = el("small", { class: "unit", role: "status" });
  let paused = false;
  let input: HTMLInputElement | null = null;
  const show = (): void => {
    state.textContent = paused
      ? "Paused: neither the hold keys nor the wake words start anything."
      : input?.checked === true
        ? "Listening for the wake words and the hold keys."
        : "Listening for the hold keys only.";
  };
  const toggle = controls.toggle(settings.wake.enabled, (wanted) => {
    input = toggle.field as HTMLInputElement;
    show();
    return apply((next) => {
      next.wake.enabled = wanted;
    }).catch((error: unknown) => {
      if (input) input.checked = !wanted;
      show();
      throw error;
    });
  });
  input = toggle.field as HTMLInputElement;
  const follow = (status: Status): void => {
    paused = status.state === "paused";
    show();
  };
  backend.onStatus(follow);
  backend.getStatus().then(follow, () => undefined);
  show();
  return [
    group("Wake words"),
    row("Wake words", "Off: only the hold keys start a dictation.", {
      node: el("span", { class: "stack" }, toggle.node, state),
      field: input,
    }),
  ];
}

/** The key a `keydown` names as the settings file does, such as `Alt+KeyZ`; null for a modifier alone, except AltGr. */
function recordedKey(event: KeyboardEvent): string | null {
  // Windows reports AltGr as a left Ctrl it adds, then the right Alt with both flags set; neither is the user's.
  const altGr = event.code === "AltRight";
  if (!altGr && /^(?:Control|Shift|Alt|Meta)(?:Left|Right)$/.test(event.code)) return null;
  const modifiers = [!altGr && event.ctrlKey && "Ctrl", event.shiftKey && "Shift", !altGr && event.altKey && "Alt", event.metaKey && "Win"].filter((name) => name !== false);
  return [...modifiers, event.code].join("+");
}

/** Text with every key written as the keyboard shows it, not as the settings file names it: `Alt+KeyZ` is "Alt+Z" and `AltRight` is "AltGr". */
function keyLabel(text: string): string {
  return text.replace(/(?:Key|Digit)(?=[A-Z0-9](?![A-Za-z0-9]))/g, "").replaceAll("AltRight", "AltGr");
}

/** Why `recorded` was refused, in the words of the rows; a name that is no key at all can't be used, however it is spelled. */
function holdKeyNotice(reason: string, recorded: string): string {
  if (reason.startsWith(`The key "${recorded}" is not a key name`)) {
    return `${keyLabel(recorded)} can't be used as a hold key. Use a letter, a digit, Space or F1 to F12, alone or with Ctrl, Shift, Alt or Win.`;
  }
  return keyLabel(reason);
}

/** One hold key: the key in effect, a recorder that applies the next combination pressed, and a way back to the default. `notices` holds every hold key row's notice, which any new attempt clears. */
function holdKeyRow(label: string, hint: string, field: "hold_key" | "write_key", { settings, apply, holdKeyDefaults }: PageContext, notices: HTMLElement[]): HTMLElement {
  const shown = el("kbd", { class: "key", "aria-live": "polite" });
  const problem = note();
  // A key is watched as soon as it is applied, also during a capture.
  const applied = el("small", { class: "applied", role: "status" });
  notices.push(problem);
  const clearNotices = (): void => {
    applied.textContent = "";
    for (const notice of notices) say(notice, "");
  };
  const restore = button("Restore default", () => {
    set(holdKeyDefaults[field]);
  }, "quiet");
  const change = button("Change…", () => {
    clearNotices();
    shown.textContent = "Press the new key. Escape cancels.";
    busy(change, true);
    restore.disabled = true;
    change.focus();
    const stop = (): void => {
      window.removeEventListener("keydown", record, true);
      change.removeEventListener("blur", cancel);
      busy(change, false);
    };
    const cancel = (): void => {
      stop();
      render();
    };
    const record = (event: KeyboardEvent): void => {
      event.preventDefault();
      event.stopPropagation();
      const name = recordedKey(event);
      if (event.code === "Escape") cancel();
      else if (name !== null) {
        stop();
        set(name);
      }
    };
    window.addEventListener("keydown", record, true);
    change.addEventListener("blur", cancel);
  });

  function render(): void {
    shown.textContent = keyLabel(settings[field]);
    const isDefault = settings[field] === holdKeyDefaults[field];
    if (isDefault && document.activeElement === restore) change.focus();
    restore.disabled = isDefault;
  }

  function set(name: string): void {
    clearNotices();
    apply((next) => {
      next[field] = name;
    }, { explain: (reason) => holdKeyNotice(reason, name) }).then(() => {
      applied.textContent = APPLIED_TEXT;
      render();
    }, (error: unknown) => {
      say(problem, errorText(error));
      render();
    });
  }

  render();
  return row(label, hint, el("span", { class: "stack" }, el("span", { class: "trow" }, shown, change, restore), applied, problem));
}

/** Where the desktop gives the app no way to see a key, the group says why and has no controls. */
function holdKeys(context: PageContext): HTMLElement[] {
  const { holdKeyUnavailable } = context;
  if (holdKeyUnavailable !== null) {
    const why = (): HTMLElement => unavailable({ kind: "unavailable", reason: holdKeyUnavailable, line: null });
    return [group("Hold keys"), row("Dictate key", null, why()), row("Write key", null, why())];
  }
  const notices: HTMLElement[] = [];
  return [
    group("Hold keys"),
    holdKeyRow("Dictate key", "Hold it, speak, let go to paste.", "hold_key", context, notices),
    holdKeyRow("Write key", "Hold it to write the words out, then paste.", "write_key", context, notices),
  ];
}

function commands(context: PageContext): HTMLElement[] {
  const { settings, controls, apply } = context;
  const pause = (key: "paste" | "submit" | "task", label: string): HTMLElement =>
    row(
      label,
      "Time to keep talking or cancel, 0 to 5 s.",
      controls.number(settings.ending_pause[key], { min: 0, max: 5, step: 0.1, unit: "s" }, (value) =>
        apply((next) => {
          next.ending_pause[key] = value;
        }),
      ),
    );
  const timings = [
    pause("paste", "Quiet before paste"),
    pause("submit", "Quiet before paste and Enter"),
    pause("task", "Quiet before running a task"),
  ];
  const pauses = settings.ending_pause;
  return [
    group("Endings"),
    row("Ending phrases", "Say one to finish. Every action needs one.", endingsTable(context)),
    group("Quiet before acting"),
    details(
      "commands/quiet/timings",
      `Details${changedSummary([["paste", pauses.paste, 0.7], ["paste and Enter", pauses.submit, 2.5], ["task", pauses.task, 2.5]])}`,
      timings,
    ),
    ...tasks(context),
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

/** One catalog speech model: what it is, whether it is downloaded, and how to get it; focus moves to `picker` when Download goes away under it. */
function modelCard(info: SpeechModelInfo, backend: Backend, picker: HTMLElement): ModelCard {
  const badge = el("span", { class: "badge" });
  const selected = el("span", { class: "badge", "data-on": "true", hidden: true }, "Selected");
  const source = el("a", { href: info.source, target: "_blank", rel: "noreferrer" }, "Source");
  const meter = el("progress", { max: "100", value: "0", "aria-label": `Download of ${info.title}`, hidden: true });
  const status = el("span", { class: "unit", role: "status" });
  const device = el("small", { class: "device" });
  const problem = note();
  const copyProblem = note();
  let failure: Pick<SpeechModelProgress, "message" | "causes"> = { message: null, causes: [] };
  const copyReport = button("Copy report", () => {
    backend.copyReport(failure.message ?? "", failure.causes).then(
      () => {
        say(copyProblem, "");
        copyReport.textContent = "Copied";
      },
      (error: unknown) => {
        say(copyProblem, `Could not copy: ${errorText(error)}`);
      },
    );
  });
  copyReport.hidden = true;
  const download = button("Download", () => {
    say(problem, "");
    show({ name: info.name, percent: 0, status: "verifying", message: null, causes: [] });
    backend.downloadSpeechModel(info.name).then(
      () => {
        show({ name: info.name, percent: 100, status: "done", message: null, causes: [] });
      },
      (error: unknown) => {
        show({ name: info.name, percent: 0, status: "failed", message: errorText(error), causes: error instanceof DownloadFailure ? error.causes : [] });
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
    busy(download, running);
    if (event.status === "done") {
      if (document.activeElement === download) picker.focus();
      cached(true);
      showDevice();
    }
    if (event.status === "failed") {
      failure = event;
      say(problem, event.message ?? "The download failed.");
    }
    copyReport.hidden = event.status !== "failed";
    copyReport.textContent = "Copy report";
    say(copyProblem, "");
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
  const detailsNode = details(
    `recognition/model/${info.name}/details`,
    "Details",
    [el("small", {}, `ID: ${info.name}. License: ${info.license}. Source: ${info.source}. `), source],
  );
  detailsNode.classList.add("model-details");
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
      detailsNode,
      el("div", { class: "trow" }, download, copyReport, meter, status),
      problem,
      copyProblem,
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

/** How each kind is grouped, explained and added on the Recognition page. */
interface Kind {
  title: string;
  hint: string;
  /** What search finds the group by. */
  terms: string;
  add: string;
  about: string;
  /** The default name of a new entry. */
  name: string;
}

const KINDS: Record<SpeechEntry["kind"], Kind> = {
  realtime: {
    title: "Streaming servers",
    hint: "Words appear while you speak.",
    terms: "streaming servers · speech servers",
    add: "Add a streaming server",
    about: "Streams your audio to a Meta realtime ASR server, so words appear while you speak.",
    name: "Meta realtime ASR",
  },
  open_ai: {
    title: "After-you-stop servers",
    hint: "Sends each capture once you stop.",
    terms: "after-you-stop servers · speech servers",
    add: "Add an after-you-stop server",
    about: "Sends each capture once you stop to an OpenAI-compatible endpoint.",
    name: "OpenAI transcriptions",
  },
  on_device: {
    title: "On this computer",
    hint: "A downloaded model; audio stays here.",
    terms: "on this computer",
    add: "Add an on-device model",
    about: "Runs a downloaded speech model on this computer, so your audio stays here.",
    name: "On this computer",
  },
  extra: {
    title: "Extra-only servers",
    hint: "Only check the live text; need a chat model.",
    terms: "extra-only servers · extra recognizers · speech servers",
    add: "Add an extra-only server",
    about: "Transcribes each capture again for the chat model to merge with the live text; it never shows your words itself.",
    name: "Extra recognizer",
  },
};
const KIND_ORDER: readonly SpeechEntry["kind"][] = ["realtime", "open_ai", "on_device", "extra"];
/** A live entry's group, as the Live recognizer drop-down names it. */
const LIVE_GROUP: Record<LiveEntry["kind"], string> = { realtime: "streaming", open_ai: "after you stop", on_device: "on this computer" };
const ARRIVES: Record<Exclude<LiveEntry["kind"], "on_device">, string> = { realtime: "text arrives as you speak", open_ai: "text arrives after you stop" };

function isLive(entry: SpeechEntry): entry is LiveEntry {
  return entry.kind !== "extra";
}

/** An entry's label in the Live recognizer drop-down: its name, then its group unless the name already says it. */
function liveChoice(entry: LiveEntry): string {
  const group = LIVE_GROUP[entry.kind];
  return entry.name.trim().toLowerCase() === group ? entry.name : `${entry.name} (${group})`;
}

/** Whether the entry also transcribes as an extra recognizer: an upload extra when active, a live one when flagged. */
function isExtraOn(entry: SpeechEntry): boolean {
  return isLive(entry) ? entry.extra === true : entry.active;
}

function hasKey(entry: SpeechEntry): boolean {
  if (entry.kind !== "realtime" && entry.kind !== "open_ai" && entry.kind !== "extra") return false;
  return entry.api_key === undefined ? entry.has_key === true : entry.api_key !== "";
}

function blankEntry(kind: SpeechEntry["kind"], name: string): SpeechEntry {
  const base = { id: `e-${Math.random().toString(36).slice(2, 10)}`, name, active: false };
  switch (kind) {
    case "realtime":
      return { ...base, kind, address: "", model: "" };
    case "open_ai":
      return { ...base, kind, address: "", model: "", request: "transcriptions", language: "" };
    case "on_device":
      return { ...base, kind };
    case "extra":
      return { ...base, kind, address: "", model: "", api: "openai" };
  }
}

/** The model and folder rows of an On this computer entry; they write into `working`. */
function onDeviceRows(working: OnDeviceEntry, { controls, backend, live }: PageContext, sharedModels?: Promise<SpeechModelInfo[]>): { rows: HTMLElement[]; validate: () => Promise<void> } {
  const catalog = new Map<string, ModelCard>();
  const listed: SpeechModelInfo[] = [];
  // Only narrows the model list; it is not applied and does not reach recognition.
  let language = ALL_LANGUAGES;
  let chosen = working.model ?? "";
  const isCatalog = (name: string): boolean => name === SPEECH_MODEL || listed.some((info) => info.name === name);
  // The entry stays picked while its path is still empty.
  let ownFolderPicked = isModelFolder(chosen);
  const speechModel = controls.draftText(
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

  (sharedModels ?? backend.listSpeechModels()).then(
    (models) => {
      for (const info of models) {
        listed.push(info);
        const card = modelCard(info, backend, modelPicker);
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

  const folderEmpty = ownFolderPicked && chosen === "";
  const folderDetails = details(
    `recognition/on-device/${working.id}/folder`,
    "Details · your own model folder",
    [
      ownFolder,
      el("p", { class: "kind-help" }, "encoder, decoder, joiner .int8.onnx, tokens.txt."),
    ],
    { open: folderEmpty || ownFolderPicked, invalid: folderEmpty },
  );
  modelPicker.addEventListener("change", () => {
    if (modelPicker.value === OWN_FOLDER) {
      ownFolderPicked = true;
      chosen = modelInput.value.trim();
      folderDetails.open = true;
      disclosed.add(folderDetails.dataset["fold"] ?? "");
      modelInput.focus();
    } else {
      ownFolderPicked = false;
      chosen = modelPicker.value;
      modelInput.value = "";
    }
    syncModel();
  });
  const speechModelField: Field = {
    node: el("div", { class: "stack wide" }, modelPicker, modelList, listProblem, folderDetails),
    field: modelPicker,
  };
  return {
    rows: [
      row("Show models for", "Filters this list by each card's languages.", { node: languagePicker, field: languagePicker }),
      row("Speech model", "Downloads on first use. A pick applies next.", speechModelField),
    ],
    validate: () => {
      if (ownFolderPicked && chosen === "") {
        showFolder("Type the folder path first.", false);
        folderDetails.open = true;
        disclosed.add(folderDetails.dataset["fold"] ?? "");
        return Promise.reject(new Error("Type the folder path of your own speech model first."));
      }
      if (!(ownFolderPicked && !isCatalog(chosen))) return Promise.resolve();
      return checkFolder(chosen).catch((error: unknown) => {
        folderDetails.open = true;
        disclosed.add(folderDetails.dataset["fold"] ?? "");
        throw error;
      });
    },
  };
}

/** The rows of the Recognition and Chat model pages, built together because the extras need the writing endpoint. */
interface SpeechRows {
  recognition: HTMLElement[];
  chat: HTMLElement[];
}

function speech(context: PageContext): SpeechRows {
  const { settings, controls, backend, apply } = context;
  const entries = (): readonly SpeechEntry[] => settings.stt.entries;
  const activeLive = (): LiveEntry | undefined => entries().filter(isLive).find((entry) => entry.active);

  const editor = el("div", { class: "editor", hidden: true });
  const problem = note();
  let editingId: string | null = null;
  let editingKind: SpeechEntry["kind"] = "realtime";
  let editingAdd = false;
  let focusOnClose: () => HTMLElement | null = () => null;
  /** Keeps the open form in step with the entries, such as Remove on the entry that just became Live. */
  let syncEditor: () => void = () => undefined;
  /** The "Applied." of the last saved form, shown on that entry's row until the next change. */
  let rowNote: { id: string; text: string } | null = null;
  const nextDictation = el("p", { class: "next-dictation", role: "status" }, "");
  const retryProblem = note();
  const useServer = button("Use a speech server instead", () => {
    openEditor(useServer, null, "realtime");
  });
  const retry = button("Retry", retryDownload);
  const summaryActions = el("div", { class: "trow", hidden: true }, useServer, retry, retryProblem);
  let currentStatus: Status = { state: "listening" };
  const modelTitles = new Map<string, string>();
  let speechModelList: SpeechModelInfo[] = [];
  const speechModels: Promise<SpeechModelInfo[]> = backend.listSpeechModels();
  speechModels.then(
    (models) => {
      speechModelList = models;
      for (const info of models) modelTitles.set(info.name, info.title);
      render();
      checkWordList();
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
    return modelTitles.get(model) ?? (isModelFolder(model) ? fileName(model) : model);
  }

  function statusState(status: Status): string {
    switch (status.state) {
      case "listening":
      case "keyboardOnly":
      case "starting":
      case "capturing":
      case "pending":
      case "writing":
      case "thinking":
      case "working":
      case "done":
        return "Ready";
      case "transcribing":
        return "Transcribing…";
      case "merging":
        return "Merging…";
      case "cleaningUp":
        return "Cleaning up…";
      case "loading":
        return status.notice;
      case "paused":
        return "Paused — resume to dictate";
      case "failed":
        return status.headline;
    }
  }

  function updateSummary(): void {
    const entry = activeLive();
    if (entry === undefined) {
      nextDictation.textContent = "";
      return;
    }
    const how = entry.kind === "on_device" ? modelTitleFor(entry) : ARRIVES[entry.kind];
    nextDictation.textContent = `Next dictation: ${entry.name} · ${how} · ${statusState(currentStatus)}`;
    const preparing = entry.kind === "on_device" && (currentStatus.state === "loading" || currentStatus.state === "failed");
    summaryActions.hidden = !preparing;
    // Only a catalog model can be downloaded again; a folder of the user's own is fixed on disk.
    retry.hidden = currentStatus.state !== "failed" || (entry.kind === "on_device" && isModelFolder(entry.model ?? SPEECH_MODEL));
    if (!preparing) say(retryProblem, "");
  }

  function retryDownload(): void {
    const entry = activeLive();
    if (entry?.kind !== "on_device") return;
    say(retryProblem, "");
    retry.disabled = true;
    // The model card's downloader checks every file again and fetches what is missing or corrupt; the engine loads the model once it is there.
    backend
      .downloadSpeechModel(entry.model ?? SPEECH_MODEL)
      .catch((error: unknown) => {
        say(retryProblem, errorText(error));
      })
      .finally(() => {
        retry.disabled = false;
      });
  }

  /** Where the entry runs: a server's host and whether a key is saved, or an on-device model's title. */
  function whereLine(entry: SpeechEntry): HTMLElement {
    if (entry.kind === "on_device") return el("small", { class: "unit" }, modelTitleFor(entry));
    return el("small", { class: "unit" }, `${new URL(entry.address).host}${hasKey(entry) ? " · key saved" : ""}`);
  }

  /** Where an on-device catalog model runs, in the desktop's words; a model folder of the user's own has no line. */
  function deviceLine(entry: SpeechEntry): HTMLElement[] {
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

  function uniqueName(base: string): string {
    const taken = new Set(entries().map((entry) => entry.name.toLowerCase()));
    for (let count = 1; ; count += 1) {
      const name = count === 1 ? base : `${base} ${String(count)}`;
      if (!taken.has(name.toLowerCase())) return name;
    }
  }

  const liveApplied = el("small", { class: "applied", role: "status" });

  /** Applies a change to the entries and shows it; `where` says how it went. */
  function change(edit: (list: SpeechEntry[]) => void, where: HTMLElement, redraw: () => void): Promise<void> {
    forgetNotes();
    say(problem, "");
    return apply((next) => {
      edit(next.stt.entries);
    }).then(
      () => {
        redraw();
        where.textContent = controls.confirmation();
      },
      (error: unknown) => {
        say(problem, errorText(error));
        redraw();
      },
    );
  }

  function useLive(id: string): void {
    void change((list) => {
      for (const entry of list) {
        if (!isLive(entry)) continue;
        entry.active = entry.id === id;
        if (entry.active) delete entry.extra;
      }
    }, liveApplied, () => {
      render();
      checkWordList();
    });
  }

  function tick(id: string, on: boolean): Promise<void> {
    return apply((next) => {
      const entry = next.stt.entries.find((known) => known.id === id);
      if (!entry) return;
      if (!isLive(entry)) entry.active = on;
      else if (on) entry.extra = true;
      else delete entry.extra;
    }).then(() => {
      syncExtras();
      updateSummary();
    });
  }

  function remove(entry: SpeechEntry): Promise<void> {
    forgetNotes();
    return apply((next) => {
      next.stt.entries.splice(next.stt.entries.findIndex((known) => known.id === entry.id), 1);
    });
  }

  const liveSelect = el("select", { id: uid("f") });
  liveSelect.addEventListener("change", () => {
    useLive(liveSelect.value);
  });
  const liveRow = row("Live recognizer", "Shows your words and hears ending phrases.", {
    node: el("span", { class: "stack" }, liveSelect, liveApplied, problem),
    field: liveSelect,
  });

  function syncLiveChoice(): void {
    liveSelect.replaceChildren(...entries().filter(isLive).map((entry) => el("option", { value: entry.id }, liveChoice(entry))));
    liveSelect.value = activeLive()?.id ?? "";
  }

  function kindGroup(kind: SpeechEntry["kind"]) {
    const list = el("div", { class: "table", "data-kind": kind });
    const add = button("+", () => {
      openEditor(add, null, kind);
    });
    add.setAttribute("aria-label", KINDS[kind].add);
    const head = el(
      "div",
      { class: "row" },
      el("div", { class: "what" }, el("span", {}, KINDS[kind].title), el("small", {}, KINDS[kind].hint)),
      el("div", { class: "ctl" }, add),
    );
    head.dataset["label"] = KINDS[kind].terms;
    const applied = el("small", { class: "applied", role: "status" });
    return { kind, list, add, applied, node: el("div", { class: "grp" }, head, list, applied) };
  }
  const byKind: Record<SpeechEntry["kind"], ReturnType<typeof kindGroup>> = {
    realtime: kindGroup("realtime"),
    open_ai: kindGroup("open_ai"),
    on_device: kindGroup("on_device"),
    extra: kindGroup("extra"),
  };
  const groups = KIND_ORDER.map((kind) => byKind[kind]);

  /** Clears what the last change said, so "Applied." only ever describes the latest one. */
  function forgetNotes(): void {
    rowNote = null;
    liveApplied.textContent = "";
    for (const { node } of groups) for (const note of node.querySelectorAll(".applied")) note.textContent = "";
  }

  function extraSwitch(entry: SpeechEntry): HTMLElement {
    const ticked = controls.toggle(isExtraOn(entry), (on) => {
      forgetNotes();
      return tick(entry.id, on);
    });
    const input = ticked.field as HTMLInputElement;
    input.setAttribute("aria-label", `Extra: ${entry.name}`);
    return el("span", { class: "extra" }, el("label", { for: input.id }, "Extra"), ticked.node);
  }

  function entryRow(entry: SpeechEntry): HTMLElement {
    const edit = button("Edit", () => {
      openEditor(edit, entry);
    }, "quiet");
    edit.setAttribute("aria-label", `Edit ${entry.name}`);
    edit.setAttribute("aria-expanded", "false");
    const role = isLive(entry) && entry.active ? el("span", { class: "badge", "data-on": "true" }, "Live") : extraSwitch(entry);
    return el(
      "div",
      { class: "trow server", "data-id": entry.id },
      el("span", { class: "phrase" }, entry.name),
      whereLine(entry),
      el("span", { class: "tail" }, role, edit),
      ...deviceLine(entry),
      ...(rowNote?.id === entry.id ? [el("small", { class: "applied", role: "status" }, rowNote.text)] : []),
    );
  }

  const extraState = el("p", { class: "next-dictation extras", role: "status" }, "");
  const needsEndpoint = el(
    "p",
    { class: "needs-chat", id: uid("n") },
    "Extra switches need a chat model. ",
    button("Go to the endpoint setting", () => {
      context.open("chat");
      writingDetails.open = true;
      disclosed.add(writingDetails.dataset["fold"] ?? "");
      (writingUrl.field as HTMLInputElement).focus();
    }, "quiet"),
  );

  /** An extra can be switched on only while there is a writing endpoint; one already on can always be switched off. */
  function lockTick(input: HTMLInputElement): void {
    const locked = !input.checked && settings.writing === null;
    input.disabled = locked;
    if (locked) input.setAttribute("aria-describedby", needsEndpoint.id);
    else input.removeAttribute("aria-describedby");
  }

  /** What the page says follows the settings: the endpoint, and which extras are on. */
  function syncExtras(): void {
    for (const { list } of groups) {
      for (const input of list.querySelectorAll<HTMLInputElement>(".trow.server input[type=checkbox]")) lockTick(input);
    }
    const on = entries().filter(isExtraOn);
    const merged = `The chat model merges ${on.length === 1 ? "its text" : "their texts"} with the live text.`;
    const loads = on.some((entry) => entry.kind === "on_device") ? " Each on-device extra loads its own model, which adds memory and processing." : "";
    extraState.textContent = on.length === 0 ? "Extra: none" : `Extra: ${on.map((entry) => entry.name).join(", ")}. ${merged}${loads}`;
    needsEndpoint.hidden = settings.writing !== null;
    syncCleanup();
  }

  function editButtonFor(id: string): HTMLButtonElement | null {
    for (const { list } of groups) {
      const found = list.querySelector<HTMLButtonElement>(`.trow.server[data-id="${id}"] button[aria-label^="Edit "]`);
      if (found) return found;
    }
    return null;
  }

  const hasEndpoint = (): boolean => settings.writing !== null;
  const cleanup = controls.toggle(settings.writing?.clean_dictation === true, (on) => {
    if (!hasEndpoint()) return Promise.reject(new Error("Set a writing endpoint first"));
    return apply((next) => {
      if (next.writing === null) throw new Error("Set a writing endpoint first");
      if (on) next.writing.clean_dictation = true;
      else delete next.writing.clean_dictation;
    });
  });
  const cleanupInput = cleanup.field as HTMLInputElement;
  const cleanupWhy = el("p", { class: "note why" }, "Set a writing endpoint first");
  function syncCleanup(): void {
    const locked = !hasEndpoint();
    cleanupInput.disabled = locked;
    cleanupWhy.hidden = !locked;
    if (locked) cleanupInput.checked = false;
  }
  const cleanupField = { node: el("div", { class: "stack" }, cleanup.node, cleanupWhy), field: cleanupInput };

  function render(): void {
    if (editingId !== null && !entries().some((entry) => entry.id === editingId)) {
      editingId = null;
      editor.hidden = true;
      editor.replaceChildren();
    }
    const open = !editor.hidden;
    for (const { kind, list } of groups) {
      const kept = entries().filter((entry) => entry.kind === kind);
      const children: HTMLElement[] = [];
      if (open && editingAdd && editingKind === kind) children.push(editor);
      for (const entry of kept) children.push(open && editingId === entry.id ? editor : entryRow(entry));
      if (children.length === 0) children.push(el("p", { class: "none" }, "None yet."));
      setChildren(list, ...children);
    }
    if (!open) byKind.realtime.list.append(editor);
    syncLiveChoice();
    syncExtras();
    updateSummary();
    syncEditor();
  }
  // The model loads behind this long-lived window, so what runs where is asked again when the app's state changes.
  const before = context.live.refresh;
  context.live.refresh = () => {
    before();
    render();
  };

  function closeEditor(returnFocus = true): void {
    const back = focusOnClose;
    editingId = null;
    editingAdd = false;
    editor.replaceChildren();
    editor.hidden = true;
    syncEditor = () => undefined;
    context.live.speechModel = () => undefined;
    render();
    if (returnFocus) {
      back()?.focus();
      queueMicrotask(() => {
        back()?.focus();
      });
    }
  }

  /** Closing gives focus to the entry's Edit, or to `opener` while the entry is not in the list. */
  function openEditor(opener: HTMLElement, existing: SpeechEntry | null, kind?: SpeechEntry["kind"]): void {
    editingAdd = existing === null;
    editingId = existing?.id ?? null;
    editingKind = existing?.kind ?? kind ?? "realtime";
    syncEditor = () => undefined;
    const working: SpeechEntry = existing ? structuredClone(existing) : blankEntry(editingKind, uniqueName(KINDS[editingKind].name));
    focusOnClose = () => editButtonFor(working.id) ?? opener;
    const message = note();
    let validate: () => Promise<void> = () => Promise.resolve();
    const sized = (node: HTMLElement, size: "half" | "third"): HTMLElement => {
      node.classList.add(size);
      return node;
    };
    const rows: HTMLElement[] = [
      row("Name", "How this server is listed when something fails.", controls.draftText(working.name, (value) => {
        working.name = value;
      })),
    ];
    const address = (hint: string, placeholder: string): HTMLElement =>
      sized(row("Address", hint, controls.draftText("address" in working ? working.address : "", (value) => {
        if ("address" in working) working.address = value;
      }, { placeholder })), "half");
    const modelRow = (size: "half" | "third"): HTMLElement =>
      sized(row("Model", "The model named in each request.", controls.draftText("model" in working ? (working.model ?? "") : "", (value) => {
        if (working.kind === "realtime" || working.kind === "open_ai" || working.kind === "extra") working.model = value;
      })), size);
    const key: { input?: HTMLInputElement } = {};
    let clearKey = false;
    const keyRow = (): HTMLElement => {
      const saved = hasKey(working);
      const field = controls.draftText("", () => undefined, {
        type: "password",
        placeholder: saved ? "Saved — leave blank to keep it" : "",
        label: "API key",
      });
      key.input = field.field as HTMLInputElement;
      const cleared = el("small", { class: "unit", hidden: true }, "The saved key is cleared when you save.");
      const clear = button("Clear saved key", () => {
        clearKey = true;
        clear.hidden = true;
        cleared.hidden = false;
      }, "quiet");
      clear.hidden = !saved;
      return sized(row("API key", "Saved in the config. Never shown again.", {
        node: el("div", { class: "stack wide" }, field.node, clear, cleared),
        field: field.field,
      }), "half");
    };
    switch (working.kind) {
      case "realtime":
        rows.push(address("A ws:// or wss:// Meta realtime server.", "ws://127.0.0.1:8012"), keyRow(), modelRow("half"));
        break;
      case "open_ai":
        rows.push(
          address("Where each capture is sent for transcription.", "http://127.0.0.1:9000/v1"),
          keyRow(),
          modelRow("third"),
          sized(row("Request", "Transcriptions, or Chat with audio.", controls.draftSelect(OPEN_AI_REQUESTS, working.request, (value) => {
            working.request = value;
          })), "third"),
          sized(row("Language", "A two-letter code such as de. Empty detects it.", controls.draftText(working.language, (value) => {
            working.language = value;
          }, { placeholder: "Detect automatically" })), "third"),
        );
        break;
      case "on_device": {
        const built = onDeviceRows(working, context, speechModels);
        rows.push(...built.rows);
        validate = built.validate;
        break;
      }
      case "extra":
        rows.push(
          address("Where the extra transcription is sent.", "http://127.0.0.1:9000/v1"),
          keyRow(),
          modelRow("half"),
          sized(row("API", "Which transcription API the endpoint speaks.", controls.draftSelect(APIS, working.api, (value) => {
            working.api = value;
          })), "half"),
        );
        break;
    }
    const save = button("Save", () => {
      say(message, "");
      const typed = key.input?.value ?? "";
      const entry = structuredClone(working);
      if (entry.kind === "realtime" || entry.kind === "open_ai" || entry.kind === "extra") {
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
        .then(() =>
          apply((next) => {
            const index = next.stt.entries.findIndex((known) => known.id === entry.id);
            if (index >= 0) next.stt.entries[index] = entry;
            else next.stt.entries.push(entry);
          }),
        )
        .then(() => {
          forgetNotes();
          rowNote = { id: entry.id, text: controls.confirmation() };
          closeEditor();
          checkWordList();
        })
        .catch((error: unknown) => {
          say(message, errorText(error));
        });
    }, "primary");
    const cancel = button("Cancel", () => {
      closeEditor();
    }, "quiet");
    const actions = el("div", { class: "trow actions" }, cancel, save);
    if (existing) actions.prepend(removal(existing, message));
    const headingId = uid("editor");
    editor.setAttribute("aria-labelledby", headingId);
    editor.replaceChildren(
      el("h4", { id: headingId }, existing ? `Editing “${existing.name}”` : KINDS[editingKind].add),
      el("p", { class: "about" }, KINDS[editingKind].about),
      el("div", { class: "fields" }, ...rows),
      actions,
      message,
    );
    editor.hidden = false;
    render();
    editor.querySelector<HTMLElement>("input, select, textarea")?.focus();
  }

  /** Remove in an entry's form: it asks first when a saved key would be lost, and is locked while the entry is Live. */
  function removal(entry: SpeechEntry, message: HTMLElement): HTMLElement {
    const group = byKind[entry.kind];
    const act = (): void => {
      say(message, "");
      const position = entries().filter((known) => known.kind === entry.kind).findIndex((known) => known.id === entry.id);
      remove(entry).then(
        () => {
          closeEditor(false);
          checkWordList();
          group.applied.textContent = controls.confirmation();
          focusAfterRemoval(Array.from(group.list.querySelectorAll<HTMLElement>('.trow.server button[aria-label^="Edit "]')), position, group.add);
        },
        (error: unknown) => {
          say(message, errorText(error));
        },
      );
    };
    const label = `Remove ${entry.name}`;
    const plain = button("Remove", act, "quiet");
    const [node, ask] = hasKey(entry)
      ? confirmed("Remove", `Remove ${entry.name} and its saved key?`, "Yes, remove", "Keep", act, label)
      : [plain, plain];
    node.setAttribute("aria-label", label);
    const reason = el("small", { class: "unit", id: uid("r") }, "Choose another Live recognizer first.");
    syncEditor = () => {
      const locked = entries().some((known) => known.id === entry.id && isLive(known) && known.active);
      ask.disabled = locked;
      reason.hidden = !locked;
      if (locked) ask.setAttribute("aria-describedby", reason.id);
      else ask.removeAttribute("aria-describedby");
    };
    return el("span", { class: "trow remove" }, node, reason);
  }

  const writing = settings.writing;
  const chatProblem = note();
  const source = controls.select([["manual", "Manual endpoint"], ["local", "On this computer"]], writing?.local === true ? "local" : "manual", (value) => {
    return apply((next) => {
      if (value === "local") next.writing = { ...next.writing, local: true };
      else if (next.writing?.url && next.writing.model) next.writing.local = false;
      else next.writing = null;
    }).then(() => { syncChatSource(); syncExtras(); });
  });
  let chosenBuild: ChatBuild = writing?.chat_build ?? "vulkan";
  const chatBuild = controls.select([["vulkan", "Vulkan GPU, with CPU fallback"], ["cpu", "CPU"]], chosenBuild, (value) => {
    chosenBuild = value === "cpu" ? "cpu" : "vulkan";
    return apply((next) => {
      if (next.writing?.local === true) next.writing.chat_build = chosenBuild;
    }).then(refreshChat);
  });
  const chatDetails = el("p", { class: "note ok" });
  const chatProgress = el("progress", { max: "100", value: "0", hidden: true, "aria-label": "Chat model download" });
  const chatState = el("output", { class: "note ok" });
  let installingChat = false;
  const installChat = button("Download chat model", () => {
    say(chatProblem, "");
    installingChat = true;
    installChat.disabled = true;
    chatProgress.hidden = false;
    backend.downloadChatModel(chosenBuild).then(() => {
      chatState.textContent = "Installed. Starts when writing or merging first needs it.";
    }, (error: unknown) => { say(chatProblem, errorText(error)); }).finally(() => {
      installingChat = false;
      chatProgress.hidden = true;
      void refreshChat();
    });
  });
  const [removeChat] = confirmed("Remove chat model", "Delete the downloaded chat model from this computer?", "Yes, delete model", "Keep model", () => {
    say(chatProblem, "");
    backend.removeChatModel().then(refreshChat, (error: unknown) => { say(chatProblem, errorText(error)); });
  });
  function refreshChat(): Promise<void> {
    return backend.getChatModel(chosenBuild).then((info) => {
      chatDetails.textContent = `${info.title} · ${formatSize(info.size)} · ${info.license} · ${info.languages.join(", ")}. Runs on ${info.build_label}.`;
      const installed = info.model_installed && info.runtime_installed;
      installChat.textContent = installed ? "Installed" : "Download chat model";
      installChat.disabled = installed || installingChat;
      removeChat.hidden = !info.model_installed;
    }, (error: unknown) => { say(chatProblem, errorText(error)); });
  }
  backend.onChatModelProgress((progress) => {
    chatProgress.value = progress.percent;
    if (progress.status === "verifying") chatState.textContent = "Checking the installation and download checksums…";
    else if (progress.status === "downloading") chatState.textContent = `Downloading and checking checksums… ${String(progress.percent)}%`;
    else if (progress.status === "failed") { say(chatProblem, progress.message ?? "The download failed."); }
  });
  void refreshChat();
  const localRows = [
    row("Local chat runtime", "A usable GPU, otherwise the CPU.", chatBuild),
    row("Local chat model", "Downloads only to this computer.", el("div", { class: "stack wide" }, chatDetails, el("span", { class: "trow" }, installChat, removeChat), chatProgress, chatState, chatProblem)),
  ];
  const manualRows: HTMLElement[] = [];
  function syncChatSource(): void {
    const local = settings.writing?.local === true;
    for (const item of localRows) item.classList.toggle("off", !local);
    for (const item of manualRows) {
      item.classList.toggle("off", local);
      for (const row of item.querySelectorAll<HTMLElement>(".row")) row.classList.toggle("off", local);
    }
  }
  const writingUrl = controls.text(writing?.url ?? "", () => applyWriting(), { placeholder: "http://127.0.0.1:8000/v1" });
  const writingModel = controls.text(writing?.model ?? "", () => applyWriting());
  const writingKey = controls.text("", () => applyWriting(true), { type: "password", placeholder: writing?.has_key === true ? "A key is saved" : "" });
  const writingInputs = [writingUrl, writingModel, writingKey].map((field) => field.field as HTMLInputElement);
  const defaultPrompt = settings.writing_default_prompt;
  const writingPrompt = controls.text(writing?.prompt ?? defaultPrompt, () => applyWriting().then(showPromptState), { rows: 8 });
  const promptArea = writingPrompt.field as HTMLTextAreaElement;
  const restorePrompt = button("Restore default prompt", () => {
    promptArea.value = defaultPrompt;
    promptArea.dispatchEvent(new Event("change"));
  }, "quiet");
  const writingPromptField = { node: el("div", { class: "stack wide" }, writingPrompt.node, restorePrompt), field: promptArea };
  const clearProblem = note();
  const clearWritingKey = button("Clear saved writing key", () => {
    const input = writingKey.field as HTMLInputElement;
    input.value = "";
    say(clearProblem, "");
    applyWriting(true).then(() => {
      input.placeholder = "Cleared";
      clearWritingKey.hidden = true;
    }, (error: unknown) => {
      say(clearProblem, errorText(error));
    });
  });
  clearWritingKey.hidden = writing?.has_key !== true;
  const writingKeyField = { node: el("div", { class: "stack" }, writingKey.node, clearWritingKey, clearProblem), field: writingKey.field };
  const showPromptState = (): void => {
    restorePrompt.disabled = promptArea.value.trim() === defaultPrompt.trim();
  };
  showPromptState();

  /** Applies the endpoint, key and prompt as one: nothing half-filled is sent. The prompt is kept only when it is not the default, so an unchanged one follows future defaults. */
  function applyWriting(replaceKey = false): Promise<void> {
    const [urlInput, modelInput, keyInput] = writingInputs;
    const url = urlInput?.value ?? "";
    const model = modelInput?.value ?? "";
    const custom = promptArea.value.trim() !== defaultPrompt.trim();
    const hasOpenings = (settings.writing?.openings ?? []).length > 0;
    const hasAppStyles = (settings.writing?.apps ?? []).length > 0;
    const local = settings.writing?.local === true;
    if (!local && url.trim() === "" && model.trim() === "") {
      if (custom || hasOpenings || hasAppStyles || (keyInput?.value ?? "") !== "") {
        return Promise.reject(new Error("Set the address and the model of the writing endpoint first."));
      }
      return apply((next) => {
        next.writing = null;
      }).then(syncExtras);
    }
    if (!local && (url.trim() === "" || model.trim() === "")) {
      return Promise.reject(new Error("The address and the model of the writing endpoint go together: fill in both."));
    }
    return apply((next) => {
      const endpoint: WritingSettings = { ...next.writing, ...(url.trim() !== "" || model.trim() !== "" ? { url, model } : {}), ...(replaceKey ? { api_key: keyInput?.value ?? "" } : {}) };
      if (custom) endpoint.prompt = promptArea.value;
      else delete endpoint.prompt;
      next.writing = endpoint;
    }).then(() => { syncExtras(); syncCleanup(); });
  }

  const openingsList = el("div", { class: "stack wide" });
  const openingProblem = note();
  const openingPhrase = controls.draftText("", () => undefined, { label: "New opening phrase", placeholder: "write me an email" });

  function setOpenings(change: (openings: WritingOpening[]) => WritingOpening[]): Promise<void> {
    say(openingProblem, "");
    return apply((next) => {
      if (next.writing === null) throw new Error("Set the writing endpoint first: the address and the model.");
      const openings = change(next.writing.openings ?? []);
      if (openings.length > 0) next.writing.openings = openings;
      else delete next.writing.openings;
    }).then(renderOpenings, (error: unknown) => {
      say(openingProblem, errorText(error));
    });
  }

  function renderOpenings(): void {
    const openings = settings.writing?.openings ?? [];
    openingsList.replaceChildren(
      ...openings.map((opening) => {
        const prompt = controls.text(opening.prompt, (value) =>
          setOpenings((known) => known.map((other) => (other.phrase === opening.phrase ? { ...other, prompt: value } : other))),
        { rows: 5, label: `Prompt for “${opening.phrase}”` });
        const remove = button("Remove", () => {
          void setOpenings((known) => known.filter((other) => other.phrase !== opening.phrase));
        }, "quiet");
        remove.setAttribute("aria-label", `Remove “${opening.phrase}”`);
        return el("div", { class: "stack wide" }, el("div", { class: "trow" }, el("span", { class: "phrase" }, `“${opening.phrase}”`), remove), prompt.node);
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
    const openings = settings.writing?.openings ?? [];
    if (openings.some((known) => known.phrase === phrase)) {
      say(openingProblem, `“${phrase}” is already in the list.`);
      return;
    }
    const prompt = promptArea.value;
    void setOpenings((known) => [...known, { phrase, prompt }]).then(() => {
      if ((settings.writing?.openings ?? []).some((known) => known.phrase === phrase)) input.value = "";
    });
  });
  const openingsField = el(
    "div",
    { class: "stack wide" },
    openingsList,
    el("div", { class: "trow add phrase-add" }, openingPhrase.node, addOpening),
    openingProblem,
  );

  const appStylesList = el("div", { class: "stack wide" });
  const appStyleProblem = note();
  const appStylesField = el("div", { class: "stack wide" }, appStylesList, appStyleProblem);

  function setAppStyles(change: (apps: WritingAppStyle[]) => WritingAppStyle[]): Promise<void> {
    say(appStyleProblem, "");
    return apply((next) => {
      if (next.writing === null) throw new Error("Set the writing endpoint first: the address and the model.");
      const apps = change(next.writing.apps ?? []);
      if (apps.length > 0) next.writing.apps = apps;
      else delete next.writing.apps;
    }).then(renderAppStyles);
  }

  function renderAppStyles(): void {
    appStylesList.replaceChildren(...(settings.writing?.apps ?? []).map((style, index) => {
      if (context.appStylesUnavailable !== null) {
        const prompt = el("textarea", { rows: "4", readonly: "", "aria-label": `Prompt for app “${style.app}”` });
        prompt.value = style.prompt;
        return el("div", { class: "stack wide" }, el("div", { class: "trow server" }, el("span", { class: "phrase" }, style.app), el("span", { class: "unit" }, `Title contains: ${style.title ?? "—"}`)), prompt);
      }
      const update = (change: (app: WritingAppStyle) => WritingAppStyle): Promise<void> =>
        setAppStyles((known) => known.map((other, at) => at === index ? change(other) : other)).catch((error: unknown) => {
          say(appStyleProblem, errorText(error));
          throw error;
        });
      const app = controls.text(style.app, (value) => update((other) => ({ ...other, app: value.trim() })), { label: `App for “${style.app}”` });
      const title = controls.text(style.title ?? "", (value) => update((other) => {
        const next = { ...other };
        if (value.trim() === "") delete next.title;
        else next.title = value.trim();
        return next;
      }), { label: `Title contains for “${style.app}”`, placeholder: "—" });
      const prompt = controls.text(style.prompt, (value) => update((other) => ({ ...other, prompt: value })), { rows: 4, label: `Prompt for app “${style.app}”` });
      const remove = button("Remove", () => {
        void setAppStyles((known) => known.filter((_, at) => at !== index)).catch((error: unknown) => {
          say(appStyleProblem, errorText(error));
        });
      }, "quiet");
      remove.setAttribute("aria-label", `Remove app style for “${style.app}”`);
      return el("div", { class: "stack wide" }, el("div", { class: "trow kind-add" }, miniField("App", app), miniField("Title contains (optional)", title), remove), miniField("Prompt", prompt));
    }));
  }
  renderAppStyles();

  if (context.appStylesUnavailable !== null) {
    appStylesField.prepend(unavailable({ kind: "unavailable", reason: context.appStylesUnavailable, line: null }));
  } else {
    const newApp = controls.draftText("", () => undefined, { label: "New app", placeholder: "Slack.exe or firefox" });
    const newTitle = controls.draftText("", () => undefined, { label: "New title contains (optional)", placeholder: "Any window title" });
    const appInput = newApp.field as HTMLInputElement;
    const titleInput = newTitle.field as HTMLInputElement;
    const add = button("Add", () => {
      const app = appInput.value.trim();
      if (app === "") {
        say(appStyleProblem, "Type the app name to add first.");
        return;
      }
      const title = titleInput.value.trim();
      void setAppStyles((known) => [...known, { app, ...(title === "" ? {} : { title }), prompt: promptArea.value }]).then(() => {
        appInput.value = "";
        titleInput.value = "";
        addRow.hidden = true;
        byName.focus();
      }, (error: unknown) => {
        say(appStyleProblem, errorText(error));
      });
    });
    const addRow = el("div", { class: "trow kind-add", hidden: true }, miniField("App", newApp), miniField("Title contains (optional)", newTitle), add);
    const byName = button("Add by name", () => {
      say(appStyleProblem, "");
      appInput.value = "";
      titleInput.value = "";
      addRow.hidden = false;
      appInput.focus();
    }, "quiet");
    const currentApp = button("Add the app you're in now", () => {
      say(appStyleProblem, "");
      void backend.getCurrentApp().then((current) => {
        if (current === null) {
          say(appStyleProblem, "Thalvi doesn't know which app you were in yet. Choose Add by name, or write into the app once and try again.");
          return;
        }
        appInput.value = current.app;
        titleInput.value = current.title;
        addRow.hidden = false;
        titleInput.focus();
      }, (error: unknown) => {
        say(appStyleProblem, errorText(error));
      });
    }, "quiet");
    appStylesField.insertBefore(el("div", { class: "trow kind-add" }, currentApp, byName), appStyleProblem);
    appStylesField.insertBefore(addRow, appStyleProblem);
  }

  const words = wordListTable(context);
  const wordProblem = note();
  words.node.append(wordProblem);
  const wordListHint = "Names and terms, added one at a time.";
  const wordRow = row("Word list", wordListHint, words.node);
  const wordHint = wordRow.querySelector(".what small");
  let wordListRequest = 0;
  const cantUse = (name: string): string => {
    const named = `${name} can't use the word list`;
    return named.length <= HINT_BUDGET ? named : "This recognizer can't use the word list";
  };
  function wordListNotice(active: LiveEntry | undefined): string {
    if (active?.kind !== "on_device") return wordListHint;
    const name = active.model;
    if (name === undefined) return wordListHint;
    const info = speechModelList.find((known) => known.name === name);
    if (info === undefined || info.word_list) return wordListHint;
    return cantUse(active.name);
  }
  function checkWordList(): void {
    const request = (wordListRequest += 1);
    const active = activeLive();
    say(wordProblem, "");
    if (wordHint) wordHint.textContent = wordListNotice(active);
    if (active === undefined || active.kind !== "realtime") {
      words.enable(true);
      return;
    }
    backend.wordListSupport(active).then(
      (supported) => {
        if (request !== wordListRequest) return;
        if (wordHint) wordHint.textContent = supported ? wordListHint : cantUse(active.name);
        words.enable(true);
      },
      (error: unknown) => {
        if (request !== wordListRequest) return;
        words.enable(false);
        say(wordProblem, errorText(error));
      },
    );
  }
  render();
  checkWordList();

  const writingUnset = (writing?.url ?? "").trim() === "" || (writing?.model ?? "").trim() === "";
  const writingWhere = [writing?.url ?? "", writing?.model ?? ""].filter((part) => part !== "").join(" · ");
  const writingDetails = details(
    "chat/writing/endpoint",
    `Details${writingWhere === "" ? "" : ` · ${writingWhere}`}${writing?.has_key === true ? " · key set" : ""}`,
    [
      row("Writing endpoint address", "Writes a dictation out on “write that out”.", writingUrl),
      row("Writing model", "The model the endpoint should use.", writingModel),
      row("Writing API key", "Saved in the config. Blank keeps the saved key.", writingKeyField),
    ],
    { open: writingUnset, invalid: writingUnset },
  );
  manualRows.push(writingDetails);
  syncChatSource();
  return {
    recognition: [
      el("div", { class: "grp" }, liveRow, nextDictation, summaryActions, extraState, needsEndpoint),
      ...groups.map(({ node }) => node),
      el("div", { class: "grp" }, wordRow),
    ],
    chat: [
      group("Writing"),
      row("Chat model source", "Writing and extras use this.", source),
      ...localRows,
      ...manualRows,
      row(
        "Clean up dictation",
        "Tidies each dictation before it is pasted.",
        cleanupField,
      ),
      row(
        "Writing prompt",
        "Sent with every dictation. Blank is not allowed.",
        writingPromptField,
      ),
      row(
        "Opening phrases",
        "A matching start uses its prompt.",
        openingsField,
      ),
      row(
        "App styles",
        "A prompt for the app you write into.",
        appStylesField,
      ),
    ],
  };
}

function openMicrophoneName(audio: AudioStatus): string {
  if (audio.active === null) return "None — no microphone is open";
  if (audio.selected === null) return `System default (${audio.active})`;
  return audio.active;
}

function microphone({ settings, backend, controls, microphones, live, audio, apply }: PageContext): HTMLElement[] {
  const names = Array.isArray(microphones) ? microphones : [];
  const chosen = settings.microphone;
  const options: (readonly [string, string])[] = [
    ["", "System default"],
    ...names.map((name) => [name, name] as const),
  ];
  if (chosen !== null && !names.includes(chosen)) options.push([chosen, `${chosen} (not found)`]);
  const picker = controls.select(options, chosen ?? "", (value) =>
    apply((next) => {
      next.microphone = value === "" ? null : value;
    }),
  );
  const active = el("output", { class: "badge" }, "");
  const refused = el("p", { class: "note", role: "alert", hidden: true });
  const showAudio = (next: AudioStatus): void => {
    active.textContent = openMicrophoneName(next);
    active.dataset["on"] = String(next.active !== null);
    say(
      refused,
      next.problem === null ? "" : `${next.problem}${next.active === null ? "" : ` ${next.active} is open instead.`}`,
    );
  };
  showAudio(audio);
  live.audio = showAudio;
  const chosenDevice = row("Microphone", Array.isArray(microphones) ? "Opens now, or after the capture that is running." : null, {
    node: el("div", { class: "stack wide" }, el("div", { class: "trow pick" }, picker.node), refused),
    field: picker.field,
  });
  const meter = el("b");
  live.level = (level) => {
    meter.style.transform = `scaleX(${Math.min(1, Math.max(0, level)).toFixed(3)})`;
  };
  const listening = controls.bareToggle(true);
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
    row("Listening", "Turns the microphone on or off now.", {
      node: el("span", { class: "stack" }, listening.node, problem),
      field: listeningInput,
    }),
    row("Active microphone", "The device actually open now.", {
      node: active,
      field: el("span", { id: uid("f") }),
    }),
    row("Level", "Moves while Thalvi is listening or capturing.", {
      node: el("span", { class: "meter", role: "meter", "aria-valuemin": "0", "aria-valuemax": "1" }, meter),
      field: el("span", { id: uid("f") }),
    }),
    group("Device"),
    chosenDevice,
  ];
  if (!Array.isArray(microphones)) {
    rows.unshift(el("p", { class: "note", role: "alert" }, `Microphones could not be listed: ${microphones.error}`));
  }
  return rows;
}

function overlay({ settings, controls, apply }: PageContext): HTMLElement[] {
  return [
    group("Overlay"),
    row("Show the overlay", "A small pill at the top while capturing.",
      controls.toggle(settings.overlay.show, (value) =>
        apply((next) => {
          next.overlay.show = value;
        }), true)),
    row("Show the draft", "The text so far, under the pill.",
      controls.toggle(settings.overlay.draft, (value) =>
        apply((next) => {
          next.overlay.draft = value;
        }), true)),
  ];
}

function look({ settings, controls, root, apply }: PageContext): HTMLElement[] {
  const applied = el("small", { class: "applied", role: "status" });
  const problem = note();
  const picker = lookPicker(settings.look, (next) => {
    applied.textContent = "";
    say(problem, "");
    applyLook(root, next);
    apply((draft) => {
      draft.look = next;
    }).then(
      () => {
        applied.textContent = controls.confirmation(true);
      },
      (error: unknown) => {
        say(problem, errorText(error));
        applyLook(root, settings.look);
        for (const input of picker.querySelectorAll<HTMLInputElement>("input")) input.checked = input.value === settings.look;
      },
    );
  });
  return [
    group("Look"),
    row("Look", "How the overlay and this window look.",
      el("div", { class: "stack wide" }, picker, applied, problem)),
  ];
}

function sounds({ settings, controls, apply }: PageContext): HTMLElement[] {
  return [group("Sounds"), ...CUES.map(([key, label, hint]) =>
    row(`${label} sound`, hint, controls.toggle(settings.cues[key], (value) =>
      apply((next) => {
        next.cues[key] = value;
      }))),
  )];
}

function overlayAndSounds(context: PageContext): HTMLElement[] {
  return [...overlay(context), ...look(context), ...sounds(context)];
}

function appsTable({ settings, controls, apply }: PageContext, onApps: () => void): HTMLElement {
  const list = el("div", { class: "cards" });
  const problem = note();
  const name = controls.draftText("", () => undefined, { label: "New app name", placeholder: "app name" });

  function change(edit: (apps: Record<string, AppSettings>) => void): Promise<boolean> {
    say(problem, "");
    return apply((next) => {
      edit(next.apps);
    }).then(
      () => {
        render();
        onApps();
        return true;
      },
      (error: unknown) => {
        say(problem, errorText(error));
        return false;
      },
    );
  }

  let removes: [string, HTMLButtonElement][] = [];

  function render(): void {
    const removed = removes.findIndex(([appName, node]) => node === document.activeElement && !(appName in settings.apps));
    removes = [];
    list.replaceChildren(
      ...Object.entries(settings.apps).map(([appName, app]) => {
        const remove = button("Remove", () => {
          void change((apps) => {
            Reflect.deleteProperty(apps, appName);
          });
        }, "quiet");
        remove.setAttribute("aria-label", `Remove ${appName}`);
        removes.push([appName, remove]);
        const edit = (label: string, values: readonly string[], key: keyof AppSettings): HTMLElement =>
          miniField(label, controls.text(listText(values), (value) =>
            apply((next) => {
              const entry = next.apps[appName];
              if (entry) entry[key] = parseList(value);
            }), { label: `${label} of ${appName}` }));
        const fields = [
          edit("Window classes", app.classes, "classes"),
          edit("Executables", app.executables, "executables"),
          edit("Keys pressed after focusing", app.input_keys, "input_keys"),
        ];
        const where = [...app.classes, ...app.executables].filter((part) => part !== "").join(", ");
        return el(
          "fieldset",
          { class: "card" },
          el("legend", {}, appName),
          details(
            `commands/apps/${appName}/match`,
            `Details · ${appName}${where === "" ? "" : ` · ${where}`}`,
            fields,
          ),
          remove,
        );
      }),
    );
    if (removed !== -1) focusAfterRemoval(removes.map(([, node]) => node), removed, add);
  }
  const add = button("Add app", () => {
    const input = name.field as HTMLInputElement;
    const appName = input.value.trim();
    if (appName === "") {
      say(problem, "Type the app's name first.");
      return;
    }
    if (appName in settings.apps) {
      say(problem, `An app named ${appName} already exists.`);
      return;
    }
    void change((apps) => {
      apps[appName] = { classes: [], executables: [], input_keys: [] };
    }).then((applied) => {
      if (applied) input.value = "";
    });
  });
  render();
  return el("div", { class: "stack wide" }, list, el("div", { class: "trow add" }, name.node, add), problem);
}

function scriptsTable({ settings, controls, apply }: PageContext, appNames: HTMLElement): HTMLElement {
  const list = el("div", { class: "cards" });
  const problem = note();

  function change(edit: (scripts: ScriptSettings[]) => void, removed: number | null = null): Promise<boolean> {
    say(problem, "");
    return apply((next) => {
      edit(next.scripts);
    }).then(
      () => {
        render(removed);
        return true;
      },
      (error: unknown) => {
        say(problem, errorText(error));
        return false;
      },
    );
  }

  let removes: HTMLButtonElement[] = [];

  /** Rebuilds the list; a focused Remove keeps its script's focus, or passes it on when its script was `removed`. */
  function render(removed: number | null = null): void {
    const focused = removes.findIndex((node) => node === document.activeElement);
    removes = [];
    list.replaceChildren(
      ...settings.scripts.map((script, index) => {
        const remove = button("Remove", () => {
          void change((scripts) => {
            scripts.splice(index, 1);
          }, index);
        }, "quiet");
        remove.setAttribute("aria-label", `Remove script ${String(index + 1)}`);
        removes.push(remove);
        const edit = (apply_: (script: ScriptSettings, value: string) => void) => (value: string): Promise<void> =>
          apply((next) => {
            const found = next.scripts[index];
            if (found) apply_(found, value);
          });
        return el(
          "div",
          { class: "card" },
          miniField("Phrase", controls.text(script.phrase, edit((found, value) => { found.phrase = value; }), { label: `Phrase of script ${String(index + 1)}` })),
          miniField("Action", controls.select(SCRIPT_ACTIONS, script.action, (value) =>
            apply((next) => {
              const found = next.scripts[index];
              if (found) found.action = value;
            }), `Action of script ${String(index + 1)}`)),
          miniField("Destination", controls.text(script.destination, edit((found, value) => { found.destination = value; }), { list: appNames.id, label: `Destination of script ${String(index + 1)}` })),
          details(
            `commands/scripts/${String(index)}/label`,
            `Details${script.label ? ` · ${script.label}` : ""}`,
            [miniField("Label", controls.text(script.label ?? "", edit((found, value) => { found.label = value === "" ? null : value; }), { label: `Label of script ${String(index + 1)}` }))],
          ),
          remove,
        );
      }),
    );
    if (focused === -1) return;
    if (focused === removed) focusAfterRemoval(removes, focused, phrase.field);
    else removes[removed !== null && focused > removed ? focused - 1 : focused]?.focus();
  }
  render();

  let action: ScriptActionKind = "open_url";
  const phrase = controls.draftText("", () => undefined, { label: "New script phrase", placeholder: "phrase" });
  const kind = controls.draftSelect(SCRIPT_ACTIONS, action, (value) => {
    action = value;
  }, "New script action");
  const destination = controls.draftText("", () => undefined, { list: appNames.id, label: "New script destination", placeholder: "destination" });
  const add = button("Add script", () => {
    const phraseInput = phrase.field as HTMLInputElement;
    const destinationInput = destination.field as HTMLInputElement;
    const entry: ScriptSettings = { phrase: phraseInput.value, action, destination: destinationInput.value, label: null };
    if (entry.phrase.trim() === "" || entry.destination.trim() === "") {
      say(problem, "Type the phrase and the destination first.");
      return;
    }
    void change((scripts) => {
      scripts.push(entry);
    }).then((applied) => {
      if (!applied) return;
      phraseInput.value = "";
      destinationInput.value = "";
    });
  });
  return el(
    "div",
    { class: "stack wide" },
    list,
    el("div", { class: "trow add" }, phrase.node, kind.node, destination.node, add),
    problem,
  );
}

function tasks(context: PageContext): HTMLElement[] {
  const appNames = el("datalist", { id: uid("apps") });
  const refresh = (): void => {
    appNames.replaceChildren(...Object.keys(context.settings.apps).map((name) => el("option", { value: name })));
  };
  refresh();
  return [
    appNames,
    group("Apps"),
    row("Apps", "Windows a script can focus.", appsTable(context, refresh)),
    group("Scripts"),
    row("Scripts", "Say “do that” after a phrase to run it.", scriptsTable(context, appNames)),
  ];
}

function limits({ settings, controls, apply }: PageContext): HTMLElement[] {
  const limits = settings.capture_limits;
  return [
    group("Capture limits"),
    details(
      "listening/capture-limits/limits",
      `Details${changedSummary([["silence", limits.idle, 60], ["stop after", limits.max, 300]])}`,
      [
        row("Stop after silence", "No words for this long ends it, 10 to 600 s.",
          controls.number(limits.idle, { min: 10, max: 600, step: 1, unit: "s" }, (value) =>
            apply((next) => {
              next.capture_limits.idle = value;
            }))),
        row("Stop after", "This long stops any capture, 60 to 3600 s.",
          controls.number(limits.max, { min: 60, max: 3600, step: 1, unit: "s" }, (value) =>
            apply((next) => {
              next.capture_limits.max = value;
            }))),
      ],
    ),
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
  const toggle = controls.bareToggle(shown.kind === "available" && shown.on);
  const input = toggle.field as HTMLInputElement;
  const problem = note();
  const applied = el("small", { class: "applied", role: "status" });
  if (shown.kind === "unreadable") {
    input.disabled = true;
    say(problem, shown.message);
  } else {
    say(problem, shown.problem ?? "");
  }
  input.addEventListener("change", () => {
    say(problem, "");
    applied.textContent = "";
    const wanted = input.checked;
    input.disabled = true;
    ops
      .set(wanted)
      .then(() => {
        applied.textContent = controls.confirmation();
      })
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
  return row(label, hint, { node: el("span", { class: "stack" }, toggle.node, applied, problem), field: input });
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
      "Starts the voice app at login.",
      startAtLogin,
      controls,
      { read: () => backend.getStartAtLogin(), set: (on) => backend.setStartAtLogin(on), copy: () => backend.copyLoginLine() },
    ),
    switchRow(
      "Show Thalvi in the app menu",
      "Starts Thalvi from your desktop's app menu.",
      menuEntry,
      controls,
      { read: () => backend.getMenuEntry(), set: (on) => backend.setMenuEntry(on) },
    ),
  ];
}

function recordings({ settings, backend, controls, apply }: PageContext): HTMLElement[] {
  const kinds = [
    ["Metrics", "metrics", "When each wake, capture and model call happened."],
    ["Text", "text", "The words you dictated, and what Thalvi did."],
    ["Audio", "audio", "What the microphone heard during a capture."],
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
      row(`${label} recording`, hint,
        controls.toggle(settings.recordings[kind], (value) =>
          apply((next) => {
            next.recordings[kind] = value;
          }))),
    ),
    group("Folder"),
    row("Recordings folder", "Everything is stored on this computer.", {
      node: el("span", { class: "stack" }, open, problem),
      field: open,
    }),
  ];
}

function entryCard(entry: HistoryEntry, remove: () => void, copyText: (text: string) => Promise<void>): [HTMLElement, HTMLButtonElement] {
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
  const [deleting, del] = confirmed("Delete", "Delete this entry? This cannot be undone.", "Yes, delete", "Keep", remove, `Delete the entry from ${time.textContent}`);
  del.classList.add("quiet");
  return [el("article", { class: "entry", "data-outcome": entry.outcome }, header, el("p", {}, entry.text), result, el("span", { class: "trow" }, copy, deleting)), del];
}

/** A button that asks before it acts: it is replaced by the question and a yes and a no. Returns the whole and the button. */
function confirmed(label: string, question: string, yes: string, no: string, act: () => void, askLabel?: string): [HTMLElement, HTMLButtonElement] {
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
  if (askLabel) ask.setAttribute("aria-label", askLabel);
  const keep = button(no, close);
  const sure = el("span", { class: "trow", role: "group", "aria-label": question, hidden: true }, question, button(yes, () => {
    close();
    act();
  }), keep);
  return [el("span", { class: "stack" }, ask, sure), ask];
}

/** The last dictation: editable, with Copy, Reset and Discard. Editing sends and copies nothing. */
function lastDictation({ backend, live }: PageContext): { nodes: HTMLElement[]; refresh: () => void } {
  let current: Draft | null = null;
  const empty = el("p", { class: "note ok", tabindex: "-1" }, "Nothing to recover yet. The last dictation appears here, also when it failed or you cancelled it.");
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
  const [discard, discardButton] = confirmed("Discard", "Discard the last dictation? This cannot be undone.", "Yes, discard", "Keep", () => {
    say(problem, "");
    say(done, "");
    backend.discardDraft().then(refresh, (error: unknown) => {
      say(problem, errorText(error));
    });
  });
  discardButton.classList.add("quiet");
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
    const hadFocus = editor.contains(document.activeElement);
    empty.hidden = next !== null;
    editor.hidden = next === null;
    if (next === null) {
      if (hadFocus) empty.focus();
      return;
    }
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
      row("Last dictation", "Fix it, then copy it. Editing sends nothing.",
        { node: el("div", { class: "stack wide" }, empty, editor, problem), field: text }),
    ],
    refresh,
  };
}

function history(context: PageContext): HTMLElement[] {
  const { settings, backend, controls, live, apply } = context;
  const last = lastDictation(context);
  const list = el("div", { class: "entries" });
  const problem = note();
  const nothing = el("p", { class: "note ok", tabindex: "-1" }, "Nothing has been kept yet.");
  let shown: { id: number; card: HTMLElement; del: HTMLButtonElement }[] = [];

  function refresh(): void {
    backend.getHistory().then(
      (entries) => {
        say(problem, "");
        const removed = shown.findIndex(({ id, del }) => del === document.activeElement && !entries.some((entry) => entry.id === id));
        shown = entries.map((entry) => {
          const [card, del] = entryCard(entry, () => { change(backend.deleteHistoryEntry(entry.id)); }, (text) => backend.copyText(text));
          return { id: entry.id, card, del };
        });
        list.replaceChildren(...(shown.length === 0 ? [nothing] : shown.map(({ card }) => card)));
        if (removed !== -1) focusAfterRemoval(shown.map(({ del }) => del), removed, nothing);
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
    row("Keep a history", "What you dictate and where it went.",
      controls.toggle(settings.history.enabled, (value) =>
        apply((next) => {
          next.history.enabled = value;
        }))),
    row("Entries to keep", "The oldest go first past this number, 1 to 1000.",
      controls.number(settings.history.limit, { min: 1, max: 1000, step: 1 }, (value) =>
        apply((next) => {
          next.history.limit = value;
        }))),
    group("Kept so far"),
    row("Recent dictations", "Newest first. Deleting takes effect at once.", el("div", { class: "stack wide" }, problem, list)),
    row("Clear history", "Deletes every entry, and a damaged file.",
      confirmed("Clear history", "Delete every entry? This cannot be undone.", "Yes, clear history", "Keep history", () => {
        change(backend.clearHistory());
      })[0]),
    ...recordings(context),
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

const CHANNELS: readonly (readonly [UpdateChannel, string])[] = [
  ["stable", "Stable"],
  ["development", "Development"],
];

/** Whether a version such as `0.2.0-dev.3` is a pre-release; build metadata after `+` does not count. */
function isPrerelease(version: string): boolean {
  return (version.split("+")[0] ?? "").includes("-");
}

/** What the update is doing, in the words the Updates rows show; a failure is told apart from being up to date. */
function updateText(update: UpdateState, version: string): string {
  const phase = update.phase;
  switch (phase.state) {
    case "idle":
      return "Thalvi has not checked for updates yet. It checks by itself shortly after it starts.";
    case "checking":
      return "Checking for updates…";
    case "upToDate":
      return `Thalvi ${version} is up to date.`;
    case "downloading":
      return `Downloading ${phase.version}${phase.percent === null ? "" : ` — ${String(phase.percent)}%`}…`;
    case "ready":
      return `${phase.version} is downloaded. Thalvi installs it and restarts when it is idle: not during a dictation.`;
    case "installing":
      return `Installing ${phase.version}. Thalvi restarts in a moment.`;
    case "failed":
    case "unsupported":
      return phase.message;
  }
}

function updates({ settings, backend, controls, live, apply }: PageContext): HTMLElement[] {
  let installed = "";
  let latest: UpdateState = { phase: { state: "idle" }, notes: null, notice: null };
  const status = el("output", { class: "note ok", role: "status" });
  const notice = el("p", { class: "note ok", role: "status", hidden: true });
  const problem = note();
  const switchNote = el("p", { class: "note ok", hidden: true });
  const check = button("Check for updates", () => {
    say(problem, "");
    backend.checkForUpdates().catch((error: unknown) => {
      say(problem, errorText(error));
    });
  });
  const notes = button("Release notes", () => {
    say(problem, "");
    backend.openReleaseNotes().catch((error: unknown) => {
      say(problem, errorText(error));
    });
  }, "quiet");
  notes.hidden = true;

  function showSwitchNote(): void {
    const stays = isPrerelease(installed) && settings.update_channel === "stable";
    switchNote.hidden = !stays;
    switchNote.textContent = stays
      ? `You are on the development build ${installed}. On Stable, Thalvi stays on this build until a newer stable release is published.`
      : "";
  }

  function show(next: UpdateState): void {
    latest = next;
    const phase = next.phase;
    status.textContent = updateText(next, installed);
    status.className = phase.state === "failed" || phase.state === "unsupported" ? "note" : "note ok";
    check.textContent = phase.state === "failed" ? "Retry" : "Check for updates";
    check.disabled = phase.state === "unsupported";
    busy(check, phase.state === "checking" || phase.state === "downloading" || phase.state === "installing");
    notes.hidden = next.notes === null;
    const told = next.notice !== null && (phase.state === "idle" || phase.state === "upToDate");
    notice.hidden = !told;
    notice.textContent = told ? (next.notice ?? "") : "";
  }

  backend
    .appVersion()
    .then((value) => {
      installed = value;
      showSwitchNote();
      show(latest);
    })
    .catch(() => undefined);
  backend.getUpdateState().then(show, (error: unknown) => {
    say(problem, errorText(error));
  });
  live.update = show;
  show(latest);

  const channel = controls.select(CHANNELS, settings.update_channel, (value) =>
    apply((next) => {
      next.update_channel = value;
    }).then(showSwitchNote),
  );
  return [
    group("Updates"),
    row("Update channel", "Stable, or Development for test builds.",
      { node: el("span", { class: "stack" }, channel.node, switchNote), field: channel.field }),
    row("Check for updates", "Checks once a day and installs while idle.",
      { node: el("span", { class: "stack" }, status, notice, el("span", { class: "num" }, check, notes), problem), field: check }),
  ];
}

function app(context: PageContext): HTMLElement[] {
  return [...startup(context), ...updates(context), ...about(context)];
}

function listening(context: PageContext): HTMLElement[] {
  return [...microphone(context), ...wakeSwitch(context), ...wakeHeads(context), ...holdKeys(context), ...limits(context)];
}

const PAGES: readonly (readonly [string, string, (context: PageContext, speech: SpeechRows) => HTMLElement[]])[] = [
  ["listening", "Listening", listening],
  ["recognition", "Recognition", (_, { recognition }) => recognition],
  ["commands", "Commands", commands],
  ["chat", "Chat model", (_, { chat }) => chat],
  ["overlay", "Overlay & sounds", overlayAndSounds],
  ["history", "History & recordings", history],
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
    if (node.classList.contains("grp")) {
      result.push(node);
      current = null;
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
  const rows = speech(context);
  return PAGES.map(([id, title, build]) => {
    const node = el("section", { "data-page": id, "aria-labelledby": `page-${id}` }, el("h3", { id: `page-${id}` }, title), ...grouped(build(context, rows)));
    return { id, title, node };
  });
}
