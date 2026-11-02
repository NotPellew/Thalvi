// The settings pages: each builds its rows against the draft and edits it in place.

import type { Backend, Ending, ScriptActionKind, SecondaryApi, Settings, SttEngine } from "./contract.ts";
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
  /** The settings window, so a look picked here shows at once. */
  root: HTMLElement;
  /** Hooks the shell calls on live events; a page assigns the ones it shows. */
  live: { level: (level: number) => void; listening: (on: boolean) => void };
}

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

function general({ draft, backend, controls, live, root }: PageContext): HTMLElement[] {
  const server = controls.text(draft.server, (value) => {
    draft.server = value;
  }, { placeholder: "ws://127.0.0.1:8080" });
  const listening = controls.toggle(true, () => undefined);
  const input = listening.field as HTMLInputElement;
  const problem = note();
  input.addEventListener("change", () => {
    say(problem, "");
    const wanted = input.checked;
    backend.setListening(wanted).catch((error: unknown) => {
      input.checked = !wanted;
      say(problem, errorText(error));
    });
  });
  live.listening = (on) => {
    input.checked = on;
  };
  return [
    row("Server address", "Where Thalvi sends speech for recognition.", server),
    row("Listening", "Turns the microphone on or off now; this is not part of Save.", {
      node: el("span", { class: "stack" }, listening.node, problem),
      field: input,
    }),
    row("Look", "How the overlay and this window look. The overlay changes after the current capture.",
      lookPicker(draft.look, (look) => {
        draft.look = look;
        applyLook(root, look);
        controls.touched();
      })),
  ];
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

function wake(context: PageContext): HTMLElement[] {
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
  ];
}

function recognition({ draft, controls }: PageContext): HTMLElement[] {
  const engine = controls.select(ENGINES, draft.stt.engine, (value) => {
    draft.stt.engine = value;
    syncModel();
  });
  const speechModel = controls.text(draft.stt.model ?? "", syncModel, { placeholder: SPEECH_MODEL });
  const modelInput = speechModel.field as HTMLInputElement;

  function syncModel(): void {
    const model = modelInput.value.trim();
    if (draft.stt.engine === "local" && model !== "") {
      draft.stt.model = model;
    } else {
      delete draft.stt.model;
    }
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
  return [
    group("Dictation"),
    row("Recognize speech", "Where your dictation is turned into text. Conversation mode always uses the server.", engine),
    row(
      "Speech model",
      `For recognition on this computer: ${SPEECH_MODEL} or the folder of a model. Empty uses ${SPEECH_MODEL}. The first use downloads about 650 MB.`,
      speechModel,
    ),
    group("Second transcription"),
    row("Second-pass transcription", "Transcribes each accepted capture again on the endpoint below. Off keeps the endpoint and key.", toggle),
    row("Endpoint address", "Where the second transcription is sent.", url),
    row("Model", null, model),
    row("API", "Which transcription API the endpoint speaks.", api),
    row("API key", "Stored in the desktop config file.", key),
  ];
}

function microphone({ draft, controls, microphones, live }: PageContext): HTMLElement[] {
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
  const meter = el("b");
  live.level = (level) => {
    meter.style.transform = `scaleX(${Math.min(1, Math.max(0, level)).toFixed(3)})`;
  };
  const rows = [
    row("Input device", Array.isArray(microphones) ? "Takes effect after the current capture." : null, picker),
    row("Level", "Moves while Thalvi is capturing: say the wake phrase to test.", {
      node: el("span", { class: "meter", role: "meter", "aria-valuemin": "0", "aria-valuemax": "1" }, meter),
      field: el("span", { id: uid("f") }),
    }),
  ];
  if (!Array.isArray(microphones)) {
    rows.unshift(el("p", { class: "note", role: "alert" }, `Microphones could not be listed: ${microphones.error}`));
  }
  return rows;
}

function overlay({ draft, controls }: PageContext): HTMLElement[] {
  return [
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

function sounds({ draft, controls }: PageContext): HTMLElement[] {
  return CUES.map(([key, label, hint]) =>
    row(`${label} sound`, hint, controls.toggle(draft.cues[key], (value) => {
      draft.cues[key] = value;
    })),
  );
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

function recordings({ draft, backend }: PageContext): HTMLElement[] {
  const kinds = [
    ["Metrics", draft.recordings.metrics, "Timings and outcomes of every model call."],
    ["Text", draft.recordings.text, "What was said and what Thalvi answered."],
    ["Audio", draft.recordings.audio, "Microphone audio, with about two seconds before each wake."],
  ] as const;
  const problem = note();
  const open = button("Open folder", () => {
    say(problem, "");
    backend.openRecordings().catch((error: unknown) => {
      say(problem, errorText(error));
    });
  });
  return [
    ...kinds.map(([label, on, hint]) =>
      row(`${label} recording`, `${hint} Set THALVI_RECORD before starting Thalvi to change it.`, {
        node: el("output", { class: "badge", "data-on": String(on) }, on ? "On" : "Off"),
        field: el("span", { id: uid("f") }),
      }),
    ),
    row("Recordings folder", "Everything is stored on this computer.", {
      node: el("span", { class: "stack" }, open, problem),
      field: open,
    }),
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
    row("Version", null, { node: version, field: el("span", { id: uid("f") }) }),
  ];
}

const PAGES: readonly (readonly [string, string, (context: PageContext) => HTMLElement[]])[] = [
  ["general", "General", general],
  ["wake", "Wake & endings", wake],
  ["recognition", "Speech recognition", recognition],
  ["microphone", "Microphone", microphone],
  ["overlay", "Overlay", overlay],
  ["sounds", "Sounds", sounds],
  ["tasks", "Apps & tasks", tasks],
  ["limits", "Capture limits", limits],
  ["recordings", "Recordings", recordings],
  ["about", "About", about],
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
