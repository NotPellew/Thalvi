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
  SecondaryApi,
  Settings,
  SpeechModelInfo,
  SpeechModelProgress,
  Status,
  SttEngine,
  SwitchState,
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
  };
  /** Checks a page adds that must pass before Save sends; each returns null when it has nothing to check. */
  checks: (() => Promise<void> | null)[];
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
const ENGINES: readonly (readonly [SttEngine, string])[] = [
  ["local", "On this computer"],
  ["server", "Through the server"],
];
const SPEECH_MODEL = "nemotron_160ms";
const CUES: readonly (readonly [keyof Settings["cues"], string, string])[] = [
  ["wake", "Wake", "Thalvi heard the wake phrase and is listening."],
  ["provisional", "Ending heard", "An ending phrase was heard; the countdown has begun."],
  ["cancelled", "Cancelled", "The capture was cancelled."],
  ["accepted", "Accepted", "The capture was delivered."],
  ["error", "Error", "Something failed."],
];
const WAKE_MODELS = ["hey_thalvi", "hey_mycroft", "hey_jarvis"];
const OUTCOMES: Record<HistoryOutcome, string> = { accepted: "Delivered", cancelled: "Cancelled", failed: "Failed" };
const DRAFT_OUTCOMES: Record<DraftOutcome, string> = {
  delivered: "Delivered.",
  cancelled: "You cancelled this one, so nothing was typed.",
  failed: "This was not delivered.",
  maybePartial: "Typing stopped partway: some of this text may already be in the window. Check it there, and use Copy text rather than pasting again.",
};
const CAPTURE_STATES: readonly Status["state"][] = ["starting", "capturing", "pending", "working"];
const WHEN = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

export function row(label: string, hint: string | null, control: Field | HTMLElement): HTMLElement {
  const labelId = uid("l");
  const field = control instanceof HTMLElement ? { node: control, field: null } : control;
  const labelable =
    field.field instanceof HTMLInputElement ||
    field.field instanceof HTMLSelectElement ||
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

function miniField(label: string, field: Field): HTMLElement {
  return el("label", { class: "mini" }, el("span", {}, label), field.node);
}

function head(
  draft: Settings,
  controls: Controls,
  key: "capture" | "conversation",
  title: string,
  listId: string,
): HTMLElement[] {
  const config = draft.wake[key];
  const name = key === "capture" ? "dictation" : "conversation";
  return [
    group(title),
    row(
      `Wake model (${name})`,
      "A model name or the path of an .onnx file. Catalog models download on first use.",
      controls.text(config.model, (value) => {
        config.model = value;
      }, { list: listId }),
    ),
    row(
      `Threshold (${name})`,
      "Higher wakes less often by mistake; lower catches quieter speech.",
      controls.slider(config.threshold, { min: 0.05, max: 1, step: 0.05 }, (value) => {
        config.threshold = value;
      }),
    ),
  ];
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
    say(problem, "");
    draft.endings[words] = chosen;
    (phrase.field as HTMLInputElement).value = "";
    controls.touched();
    render();
  });
  return el("div", { class: "stack wide" }, list, el("div", { class: "trow add" }, phrase.node, action.node, add), problem);
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
  const models = el("datalist", { id: "wake-models" }, ...WAKE_MODELS.map((model) => el("option", { value: model })));
  return [
    models,
    ...head(draft, controls, "capture", "Dictation wake word", "wake-models"),
    ...head(draft, controls, "conversation", "Conversation wake word", "wake-models"),
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
  select: (on: boolean) => void;
}

/** One catalog speech model: what it is, whether it is downloaded, and how to get it. */
function modelCard(info: SpeechModelInfo, backend: Backend, pick: () => void): ModelCard {
  const picker = el("button", { type: "button", role: "radio", "aria-checked": "false", class: "pick" }, info.title);
  picker.addEventListener("click", pick);
  const badge = el("span", { class: "badge" });
  const source = el("a", { href: info.source, target: "_blank", rel: "noreferrer" }, "Source");
  const meter = el("progress", { max: "100", value: "0", "aria-label": `Download of ${info.title}`, hidden: true });
  const status = el("span", { class: "unit", role: "status" });
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
    if (event.status === "done") cached(true);
    if (event.status === "failed") say(problem, event.message ?? "The download failed.");
  }

  cached(info.cached);
  return {
    node: el(
      "div",
      { class: "model" },
      el("div", { class: "trow" }, picker, el("span", { class: "unit" }, formatSize(info.size)), badge),
      el("small", {}, info.description),
      el("div", { class: "trow" }, source, download, meter, status),
      problem,
    ),
    progress: show,
    select: (on) => {
      picker.setAttribute("aria-checked", String(on));
    },
  };
}

function speech({ draft, controls, backend, checks, live }: PageContext): HTMLElement[] {
  const engine = controls.select(ENGINES, draft.stt.engine, (value) => {
    draft.stt.engine = value;
    syncModel();
  });
  const catalog = new Map<string, ModelCard>();
  const listed: string[] = [];
  let chosen = draft.stt.model ?? "";
  const isCatalog = (name: string): boolean => name === SPEECH_MODEL || listed.includes(name);
  const speechModel = controls.text(
    isCatalog(chosen) ? "" : chosen,
    (value) => {
      chosen = value.trim();
      syncModel();
    },
    { placeholder: "/path/to/model-folder", label: "Your own model folder" },
  );
  const modelInput = speechModel.field as HTMLInputElement;
  const folderResult = el("p", { class: "note", role: "status", hidden: true });
  const modelList = el("div", { class: "models", role: "radiogroup", "aria-label": "Speech models" });
  const listProblem = note();
  const ownFolder = el(
    "div",
    { class: "own" },
    el("span", { class: "unit" }, "Your own model folder"),
  );

  live.speechModel = (event) => {
    catalog.get(event.name)?.progress(event);
  };

  function showSelection(): void {
    const active = chosen === "" ? SPEECH_MODEL : chosen;
    for (const [name, card] of catalog) card.select(name === active);
    ownFolder.dataset["active"] = String(!isCatalog(active));
  }

  function pickModel(name: string): void {
    chosen = name;
    modelInput.value = "";
    syncModel();
    controls.touched();
  }

  backend.listSpeechModels().then(
    (models) => {
      for (const info of models) {
        listed.push(info.name);
        const card = modelCard(info, backend, () => {
          pickModel(info.name);
        });
        catalog.set(info.name, card);
        modelList.append(card.node);
      }
      showSelection();
    },
    (error: unknown) => {
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
    else if (path === SPEECH_MODEL) showFolder(`${SPEECH_MODEL} is downloaded on first use; there is no folder to check.`, false);
    else checkFolder(path).catch(() => undefined);
  });
  checks.push(() => {
    const path = draft.stt.model;
    return draft.stt.engine === "local" && path !== undefined && !isCatalog(path) ? checkFolder(path) : null;
  });
  ownFolder.append(el("div", { class: "trow check" }, speechModel.node, checkButton), folderResult);
  const speechModelField: Field = {
    node: el("div", { class: "stack wide" }, modelList, listProblem, ownFolder),
    field: modelInput,
  };

  function syncModel(): void {
    latestCheck += 1;
    say(folderResult, "");
    if (draft.stt.engine === "local" && chosen !== "") {
      draft.stt.model = chosen;
    } else {
      delete draft.stt.model;
    }
    showSelection();
  }

  const secondary = draft.stt.secondary ?? null;
  let secondaryOn = secondary?.enabled ?? false;
  const url = controls.text(secondary?.url ?? "", sync, { placeholder: "http://127.0.0.1:9000/v1" });
  const model = controls.text(secondary?.model ?? "", sync);
  const key = controls.text(secondary?.api_key ?? "", sync, { type: "password" });
  const api = controls.select(APIS, secondary?.api ?? "openai", sync);
  const inputs = [url, model, key].map((field) => field.field as HTMLInputElement);
  const picker = api.field as HTMLSelectElement;
  const toggle = controls.toggle(secondaryOn, (value) => {
    secondaryOn = value;
    syncEnabled();
    sync();
  });

  function syncEnabled(): void {
    for (const input of inputs) input.disabled = !secondaryOn;
    picker.disabled = !secondaryOn;
  }

  function sync(): void {
    const [urlInput, modelInput, keyInput] = inputs;
    const blank = inputs.every((input) => input.value.trim() === "");
    draft.stt.secondary =
      !secondaryOn && blank
        ? null
        : {
            enabled: secondaryOn,
            url: urlInput?.value ?? "",
            model: modelInput?.value ?? "",
            api: APIS.find(([value]) => value === picker.value)?.[0] ?? "openai",
            api_key: keyInput?.value ?? "",
          };
  }
  syncEnabled();
  const server = controls.text(draft.server, (value) => {
    draft.server = value;
  }, { placeholder: "ws://127.0.0.1:8080" });
  return [
    group("Server"),
    row("Server address", "Where Thalvi sends speech for recognition.", server),
    group("Dictation"),
    row("Recognize speech", "Where your dictation is turned into text. Conversation mode always uses the server.", engine),
    row(
      "Speech model",
      `For recognition on this computer. Pick a model, downloading it first or on first use, or point to your own model folder, which holds encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx and tokens.txt.`,
      speechModelField,
    ),
    group("Second transcription"),
    row("Second-pass transcription", "Transcribes each accepted capture again on the endpoint below. Off keeps the endpoint and key.", toggle),
    row("Endpoint address", "Where the second transcription is sent.", url),
    row("Model", null, model),
    row("API", "Which transcription API the endpoint speaks.", api),
    row("API key", "Stored in the desktop config file.", key),
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

function entryCard(entry: HistoryEntry, remove: () => void): HTMLElement {
  const when = new Date(entry.timestamp_ms);
  const time = el("time", { datetime: when.toISOString() }, WHEN.format(when));
  const header = el("header", {}, time);
  if (entry.target !== null) header.append(el("span", { class: "target" }, entry.target));
  header.append(el("span", { class: "outcome" }, OUTCOMES[entry.outcome]));
  const del = button("Delete", remove, "quiet");
  del.setAttribute("aria-label", `Delete the entry from ${time.textContent}`);
  return el("article", { class: "entry", "data-outcome": entry.outcome }, header, el("p", {}, entry.text), del);
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

/** The last dictation: editable, with Copy, Paste into its window, and Discard. Nothing is sent until a button is pressed. */
function lastDictation({ backend, live }: PageContext): { nodes: HTMLElement[]; refresh: () => void } {
  let current: Draft | null = null;
  let busy = false;
  // After a capture the shown draft may be stale until the new one loads; Paste waits for it.
  let loading = false;
  let pasting = false;
  const empty = el("p", { class: "note ok" }, "Nothing to recover yet. The last dictation appears here, also when it failed or you cancelled it.");
  const outcome = el("p", { class: "note ok" });
  const newer = el("p", { class: "note why", hidden: true }, "A newer dictation is waiting. Discard this one to see it.");
  const why = el("p", { class: "note why", hidden: true });
  const text = el("textarea", { rows: "4", spellcheck: "false", "aria-label": "Last dictation text" });
  const done = el("p", { class: "note ok", role: "status", hidden: true });
  const problem = note();
  const copy = button("Copy text", () => {
    say(problem, "");
    backend.copyDraft(text.value).then(
      () => {
        say(done, "Copied to the clipboard.");
      },
      (error: unknown) => {
        say(done, "");
        say(problem, errorText(error));
      },
    );
  });
  const paste = button("Paste", () => {
    if (paste.disabled || current === null) return;
    const target = current.target ?? "the window";
    pasting = true;
    sync();
    say(problem, "");
    say(done, "");
    backend.pasteDraft(text.value).then(
      () => {
        say(done, `Pasted into ${target}. Enter was not pressed.`);
      },
      (error: unknown) => {
        say(problem, errorText(error));
      },
    ).finally(() => {
      pasting = false;
      refresh();
    });
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
  const editor = el("div", { class: "stack wide", hidden: true }, outcome, newer, text, el("span", { class: "trow" }, copy, paste, reset, discard), why, done);

  function saveEdit(): void {
    backend.editDraft(text.value).catch((error: unknown) => {
      say(problem, errorText(error));
    });
  }
  function sync(): void {
    if (current === null) return;
    const blank = text.value.trim() === "";
    paste.textContent = current.target === null ? "Paste" : `Paste into ${current.target}`;
    paste.disabled = blank || busy || loading || pasting || current.pasting || current.target === null;
    copy.disabled = blank;
    reset.disabled = text.value === current.original;
    say(why, current.target === null ? "No window was bound when you dictated this, so only Copy is possible."
      : busy ? "Thalvi is capturing or delivering. Paste is available again when it has finished."
      : "");
  }
  function show(next: Draft | null): void {
    loading = false;
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
  function refresh(): void {
    backend.getDraft().then(show, (error: unknown) => {
      loading = false;
      sync();
      say(problem, errorText(error));
    });
  }
  text.addEventListener("input", () => {
    sync();
    saveEdit();
  });
  live.status = (status) => {
    busy = CAPTURE_STATES.includes(status.state);
    loading = !busy;
    sync();
    if (!busy) refresh();
  };
  refresh();

  return {
    nodes: [
      group("Last dictation"),
      row("Last dictation", "Fix it, then Copy it or Paste it into the window it was meant for. Editing sends nothing, and Paste never presses Enter. Kept in memory only, until you discard it or quit.",
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
        list.replaceChildren(...(entries.length === 0 ? [nothing] : entries.map((entry) => entryCard(entry, () => { change(backend.deleteHistoryEntry(entry.id)); }))));
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
