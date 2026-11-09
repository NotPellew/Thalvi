import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Draft, HistoryEntry, OnDeviceEntry, Settings, SpeechEntry } from "./contract.ts";
import settingsCss from "./settings.css?raw";
import { ELSEWHERE_TEXT, mountSettings, SAVED_MICROPHONE_TEXT, SAVED_TEXT } from "./settings.ts";
import { NEMOTRON, SETTINGS, settle, stubBackend, UNIFIED, WAKE_MODELS_INFO } from "./testing.ts";

let root: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '<div id="settings"></div>';
  root = document.getElementById("settings") as HTMLElement;
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function open(settings: Settings = SETTINGS) {
  const stub = stubBackend(settings);
  mountSettings(root, stub.backend);
  await settle();
  return stub;
}

function rowOf(label: string): HTMLElement {
  const found = Array.from(root.querySelectorAll<HTMLElement>(".row")).find((row) => row.dataset["label"] === label.toLowerCase());
  if (!found) throw new Error(`no row ${label}`);
  return found;
}

function control<K extends "input" | "select">(label: string, tag: K): HTMLElementTagNameMap[K] {
  const found = rowOf(label).querySelector(tag);
  if (!found) throw new Error(`no ${tag} in ${label}`);
  return found;
}

function type(input: HTMLInputElement, value: string): void {
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

function choose(select: HTMLSelectElement, value: string): void {
  select.value = value;
  select.dispatchEvent(new Event("change", { bubbles: true }));
}

function flip(input: HTMLInputElement): void {
  input.click();
}

function button(name: string): HTMLButtonElement {
  const found = Array.from(root.querySelectorAll("button")).find((candidate) => candidate.textContent === name);
  if (!found) throw new Error(`no button ${name}`);
  return found;
}

function within(parent: HTMLElement, name: string): HTMLButtonElement {
  const found = Array.from(parent.querySelectorAll("button")).find((candidate) => candidate.textContent === name);
  if (!found) throw new Error(`no button ${name}`);
  return found;
}

function goTo(page: string): void {
  (root.querySelector(`nav [data-page="${page}"]`) as HTMLElement).click();
}

function sent(stub: Awaited<ReturnType<typeof open>>): Settings {
  const call = stub.backend.saveSettings.mock.calls[0];
  if (!call) throw new Error("nothing was saved");
  return call[0];
}

const SERVER = SETTINGS.stt.entries[0] as SpeechEntry;

function device(model?: string, active = true): OnDeviceEntry {
  return { kind: "on_device", id: "on-device", name: "On this computer", active, ...(model ? { model } : {}) };
}

/** Settings whose live entry in use is the on-device model, beside a Thalvi server entry not in use. */
function local(model?: string): Settings {
  return { ...SETTINGS, stt: { entries: [device(model), { ...SERVER, active: false }] } };
}

function deviceOf(settings: Settings): OnDeviceEntry {
  const found = settings.stt.entries.find((entry): entry is OnDeviceEntry => entry.kind === "on_device");
  if (!found) throw new Error("no on-device entry");
  return found;
}

function edit(name: string): void {
  (root.querySelector(`button[aria-label="Edit ${name}"]`) as HTMLButtonElement).click();
}

/** Opens the editor of the on-device entry; its model list loads meanwhile. */
async function editLocal(): Promise<void> {
  edit("On this computer");
  await settle();
}

function use(name: string): void {
  (root.querySelector(`button[aria-label="Use ${name}"]`) as HTMLButtonElement).click();
}

async function saveServer(): Promise<void> {
  button("Save server").click();
  await settle();
}

const editorNote = (): string => root.querySelector(".editor > .note:not([hidden])")?.textContent ?? "";

describe("pages", () => {
  it("offers seven pages in order, each holding its controls under the right group", async () => {
    await open();
    const shape = Array.from(root.querySelectorAll<HTMLElement>("section[data-page]")).map((page) => ({
      id: page.dataset["page"],
      title: page.querySelector("h3")?.textContent,
      groups: Object.fromEntries(
        Array.from(page.querySelectorAll(".grp")).map((group): [string, (string | undefined)[]] => [
          group.querySelector("h4")?.textContent ?? "",
          Array.from(group.querySelectorAll<HTMLElement>(".row")).map((row) => row.dataset["label"]),
        ]),
      ),
    }));
    expect(Array.from(root.querySelectorAll("nav [data-page]")).map((item) => item.getAttribute("data-page"))).toEqual(
      shape.map((page) => page.id),
    );
    expect(shape).toEqual([
      {
        id: "speech",
        title: "Speech",
        groups: {
          "Live dictation": ["speech servers", "word list"],
          "Second pass": ["second-pass servers"],
          Writing: ["writing endpoint address", "writing model", "writing api key", "writing prompt"],
        },
      },
      {
        id: "microphone",
        title: "Microphone",
        groups: {
          "Right now": ["listening", "active microphone", "level"],
          "Saved with Save": ["saved device"],
        },
      },
      {
        id: "capture",
        title: "Capture",
        groups: {
          "Dictation wake word": ["wake model (dictation)", "threshold (dictation)"],
          "Conversation wake word": ["wake model (conversation)", "threshold (conversation)"],
          Endings: ["ending phrases"],
          "Quiet before acting": ["quiet before paste", "quiet before paste and enter", "quiet before running a task"],
          "Capture limits": ["stop after silence", "stop after"],
        },
      },
      {
        id: "feedback",
        title: "Feedback and appearance",
        groups: {
          Overlay: ["show the overlay", "show the draft"],
          Look: ["look"],
          Sounds: ["wake sound", "ending heard sound", "cancelled sound", "accepted sound", "error sound"],
        },
      },
      {
        id: "tasks",
        title: "Apps & tasks",
        groups: { Apps: ["apps"], Scripts: ["scripts"] },
      },
      {
        id: "history",
        title: "History",
        groups: {
          "Last dictation": ["last dictation"],
          Keeping: ["keep a history", "entries to keep"],
          "Kept so far": ["recent dictations", "clear history"],
        },
      },
      {
        id: "app",
        title: "App",
        groups: {
          Startup: ["start thalvi when i log in", "show thalvi in the app menu"],
          Recordings: ["metrics recording", "text recording", "audio recording"],
          "Right now": ["recordings folder"],
          About: ["version"],
        },
      },
    ]);
  });
});

describe("save", () => {
  it("sends the settings unchanged when nothing was edited", async () => {
    const stub = await open();
    button("Save").click();
    expect(sent(stub)).toEqual(SETTINGS);
    await settle();
    expect(root.querySelector(".msg")?.textContent).toBe(SAVED_TEXT);
  });

  it.each<[string, boolean, boolean, boolean, string]>([
    ["only a non-microphone setting changed", false, true, false, "Saved — applies after the current capture"],
    ["the microphone changed and has not opened", true, false, false, "Saved — applies after the current capture; the saved microphone is not active until it opens"],
    ["the microphone changed and opens after the save", true, false, true, "Saved — applies after the current capture"],
    ["the microphone and another setting changed", true, true, false, "Saved — applies after the current capture; the saved microphone is not active until it opens"],
  ])("says only what applies to the change saved: %s", async (_case, microphone, other, opens, footer) => {
    const stub = await open();
    if (other) {
      goTo("capture");
      type(control("Quiet before paste", "input"), "1.5");
    }
    if (microphone) {
      goTo("microphone");
      stub.emit({ state: "paused" });
      choose(control("Saved device", "select"), "USB Microphone");
    }
    button("Save").click();
    await settle();
    if (opens) stub.audio({ selected: "USB Microphone", active: "USB Microphone", paused: false });
    expect(root.querySelector(".msg")?.textContent).toBe(footer);
  });

  it("sends exactly the edits", async () => {
    const stub = await open();
    goTo("speech");
    edit("Thalvi server");
    type(control("Address", "input"), "wss://thalvi.example");
    await saveServer();
    goTo("capture");
    type(control("Quiet before paste", "input"), "1.5");
    type(control("Threshold (dictation)", "input"), "0.8");
    choose(control("Wake model (conversation)", "select"), "hey_jarvis");
    goTo("feedback");
    flip(control("Show the draft", "input"));
    flip(control("Wake sound", "input"));
    goTo("capture");
    type(control("Stop after", "input"), "120");
    goTo("microphone");
    stub.emit({ state: "paused" });
    choose(control("Saved device", "select"), "USB Microphone");
    button("Save").click();
    const expected = structuredClone(SETTINGS);
    expected.stt.entries = [{ ...SERVER, address: "wss://thalvi.example" } as SpeechEntry];
    expected.ending_pause.paste = 1.5;
    expected.wake.capture.threshold = 0.8;
    expected.wake.conversation.model = "hey_jarvis";
    expected.overlay.draft = false;
    expected.cues.wake = false;
    expected.capture_limits.max = 120;
    expected.microphone = "USB Microphone";
    expect(sent(stub)).toEqual(expected);
  });

  it("shows the rejection inline and keeps the draft", async () => {
    const stub = await open();
    stub.backend.saveSettings.mockRejectedValue("The desktop config is not valid: bad server.");
    button("Save").click();
    await settle();
    expect(root.querySelector(".err")?.textContent).toBe("The desktop config is not valid: bad server.");
    expect(root.querySelector(".msg")?.textContent).toBe("");
  });

  it("marks unsaved edits and clears the mark on revert", async () => {
    const stub = await open();
    goTo("speech");
    edit("Thalvi server");
    type(control("Address", "input"), "ws://other");
    await saveServer();
    expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
    button("Revert").click();
    await settle();
    expect(stub.backend.getSettings).toHaveBeenCalledTimes(2);
    expect(root.querySelector(".trow.server")?.textContent).toContain("ws://127.0.0.1:8080");
    expect(root.querySelector(".msg")?.textContent).toBe("");
  });

  describe("when an agent changes the saved settings behind an open window", () => {
    async function openChanged() {
      const stub = await open(several());
      const changed = several();
      changed.stt.entries = changed.stt.entries.map((entry) => ({ ...entry, active: entry.id === "cybrex2" }));
      stub.backend.getSettings.mockImplementation(() => Promise.resolve(structuredClone(changed)));
      goTo("speech");
      return stub;
    }
    const activeNames = (): (string | null)[] =>
      Array.from(root.querySelectorAll(".trow.server")).filter((row) => row.textContent.includes("Active")).map((row) => row.querySelector(".phrase")?.textContent ?? null);

    it("loads the new settings and stays on the current page when nothing is unsaved", async () => {
      const stub = await openChanged();
      expect(activeNames()).toEqual(["Thalvi server"]);
      stub.changedElsewhere();
      await settle();
      expect(activeNames()).toEqual(["Cybrex2"]);
      expect(root.querySelector('nav [aria-current="page"]')?.getAttribute("data-page")).toBe("speech");
      expect(root.querySelector(".msg")?.textContent).toBe("");
    });

    it.each([
      ["revert", "Quiet before paste"],
      ["save", "Quiet before paste"],
      ["revert", "Threshold (dictation)"],
    ] as const)("keeps unsaved edits and says so until the user chooses to %s (edit of %s)", async (action, edited) => {
      const stub = await openChanged();
      goTo("capture");
      type(control(edited, "input"), edited === "Threshold (dictation)" ? "0.8" : "1.5");
      goTo("speech");
      stub.changedElsewhere();
      await settle();
      expect(activeNames()).toEqual(["Thalvi server"]);
      expect(stub.backend.getSettings).toHaveBeenCalledTimes(1);
      expect(root.querySelector(".msg")?.textContent).toBe(ELSEWHERE_TEXT);
      button(action === "revert" ? "Revert" : "Save").click();
      await settle();
      if (action === "revert") {
        expect(activeNames()).toEqual(["Cybrex2"]);
      } else {
        expect(activeNames()).toEqual(["Thalvi server"]);
        expect(sent(stub).stt.entries.find((entry) => entry.active)?.id).toBe(SERVER.id);
      }
      expect(root.querySelector(".msg")?.textContent).toBe(action === "revert" ? "" : SAVED_TEXT);
    });
  });

  it.each(["edit", "apply failure"] as const)("keeps a later %s visible when an in-flight save completes", async (event) => {
    const stub = await open();
    let complete = (): void => { throw new Error("Save has not started"); };
    stub.backend.saveSettings.mockImplementationOnce(() => new Promise<void>((resolve) => { complete = resolve; }));
    goTo("tasks");
    button("Save").click();
    expect(button("Revert").disabled).toBe(true);
    if (event === "edit") {
      type(root.querySelector<HTMLInputElement>('input[aria-label="Label of script 1"]') as HTMLInputElement, "new label");
    } else {
      stub.emit({ state: "failed", notice: "The new settings could not be applied.", headline: "Failed — Settings", causes: [], kind: "other" });
    }
    complete();
    await settle();
    expect(button("Revert").disabled).toBe(false);
    expect(sent(stub).scripts[0]?.label).toBe("docs");
    expect(root.querySelector(".msg")?.textContent).toBe(event === "edit" ? "Unsaved changes" : "");
    if (event === "apply failure") expect(root.querySelector(".err")?.textContent).toBe("The new settings could not be applied.");
  });
});

describe("tables", () => {
  it("adds an ending phrase normalized and removes another", async () => {
    const stub = await open();
    goTo("capture");
    const add = rowOf("Ending phrases");
    type(add.querySelector<HTMLInputElement>(".add input") as HTMLInputElement, "  Send   It ");
    choose(add.querySelector<HTMLSelectElement>(".add select") as HTMLSelectElement, "submit");
    (add.querySelector(".add button") as HTMLElement).click();
    const phrases = Array.from(add.querySelectorAll(".table .phrase"), (node) => node.textContent);
    expect(phrases.at(-1)).toBe("“send it”");
    expect(add.querySelector(".table")?.nextElementSibling).toBe(add.querySelector(".trow.add"));
    root.querySelector<HTMLElement>('button[aria-label="Remove “never mind”"]')?.click();
    button("Save").click();
    const expected = structuredClone(SETTINGS);
    expected.endings = {
      "paste that": "paste",
      "write that out": "write",
      submit: "submit",
      "do that": "task",
      "scratch that": "scratch",
      "send it": "submit",
    };
    expect(sent(stub).endings).toEqual(expected.endings);
  });

  it("edits an app and a script", async () => {
    const stub = await open();
    goTo("tasks");
    type(root.querySelector<HTMLInputElement>('input[aria-label="Executables of slack"]') as HTMLInputElement, "slack, slack.exe");
    type(root.querySelector<HTMLInputElement>('input[aria-label="Label of script 1"]') as HTMLInputElement, "");
    button("Add script").click();
    type(root.querySelector<HTMLInputElement>('input[aria-label="Phrase of script 2"]') as HTMLInputElement, "focus slack");
    choose(root.querySelector<HTMLSelectElement>('select[aria-label="Action of script 2"]') as HTMLSelectElement, "focus_app");
    type(root.querySelector<HTMLInputElement>('input[aria-label="Destination of script 2"]') as HTMLInputElement, "slack");
    button("Save").click();
    const saved = sent(stub);
    expect(saved.apps["slack"]?.executables).toEqual(["slack", "slack.exe"]);
    expect(saved.scripts).toEqual([
      { phrase: "open the docs", action: "open_url", destination: "https://example.com", label: null },
      { phrase: "focus slack", action: "focus_app", destination: "slack", label: null },
    ]);
  });

  it("refuses a duplicate app name inline", async () => {
    await open();
    goTo("tasks");
    type(root.querySelector<HTMLInputElement>('input[aria-label="New app name"]') as HTMLInputElement, "slack");
    button("Add app").click();
    expect(root.querySelector(".note[role=alert]:not([hidden])")?.textContent).toBe("An app named slack already exists.");
  });
});

describe("the wake word picker", () => {
  async function openCapture(settings: Settings = SETTINGS) {
    const stub = await open(settings);
    goTo("capture");
    return stub;
  }
  const badge = (label: string): string => rowOf(label).querySelector(".badge")?.textContent ?? "";
  const problem = (label: string): string =>
    rowOf(label).querySelector<HTMLElement>(".note:not([hidden])")?.textContent ?? "";

  it("shows each catalog phrase with whether it is ready or not downloaded", async () => {
    const stub = await openCapture();
    expect(badge("Wake model (dictation)")).toBe("Ready");
    expect(badge("Wake model (conversation)")).toBe("Not downloaded");
    const dictation = control("Wake model (dictation)", "select");
    expect(Array.from(dictation.options).map((option) => option.textContent)).toEqual([
      "Hey Thalvi — Ready",
      "Hey Mycroft — Not downloaded",
      "Hey Jarvis — Not downloaded",
      "Custom model…",
    ]);
    let finish = (): void => { throw new Error("download has not started"); };
    stub.backend.downloadWakeModel.mockImplementationOnce(() => new Promise<void>((resolve) => { finish = resolve; }));
    within(rowOf("Wake model (conversation)"), "Download").click();
    expect(badge("Wake model (conversation)")).toBe("Downloading…");
    finish();
    await settle();
    expect(stub.backend.downloadWakeModel).toHaveBeenCalledWith("hey_mycroft");
    expect(badge("Wake model (conversation)")).toBe("Ready");
  });

  it("shows models a retry downloaded while the window stayed open, and still flags the rest", async () => {
    const stub = await openCapture();
    expect(badge("Wake model (conversation)")).toBe("Not downloaded");
    stub.backend.listWakeModels.mockResolvedValueOnce(
      WAKE_MODELS_INFO.map((info) => ({ ...info, cached: info.name !== "hey_jarvis" })),
    );
    stub.emit({ state: "loading", notice: "Downloading the wake word model." });
    await settle();
    expect(badge("Wake model (conversation)")).toBe("Ready");
    expect(Array.from(control("Wake model (dictation)", "select").options).map((option) => option.textContent)).toEqual([
      "Hey Thalvi — Ready",
      "Hey Mycroft — Ready",
      "Hey Jarvis — Not downloaded",
      "Custom model…",
    ]);
  });

  it("leaves the wake pickers alone when a state change finds nothing new on disk", async () => {
    const stub = await openCapture();
    const option = control("Wake model (dictation)", "select").options[0];
    stub.emit({ state: "loading", notice: "Loading." });
    await settle();
    expect(stub.backend.listWakeModels.mock.calls.length).toBeGreaterThan(1);
    expect(control("Wake model (dictation)", "select").options[0]).toBe(option);
  });

  it("prevents choosing the other wake word's phrase", async () => {
    await openCapture();
    const dictation = control("Wake model (dictation)", "select");
    const conversation = control("Wake model (conversation)", "select");
    expect(dictation.querySelector<HTMLOptionElement>('option[value="hey_mycroft"]')?.disabled).toBe(true);
    expect(conversation.querySelector<HTMLOptionElement>('option[value="hey_thalvi"]')?.disabled).toBe(true);
    expect(dictation.querySelector<HTMLOptionElement>('option[value="hey_thalvi"]')?.disabled).toBe(false);
  });

  it("moves the threshold to the chosen catalog phrase's default and saves it", async () => {
    const stub = await openCapture({ ...SETTINGS, wake: { ...SETTINGS.wake, capture: { model: "hey_thalvi", threshold: 0.9 } } });
    const shown = (): string => rowOf("Threshold (dictation)").querySelector("output")?.textContent ?? "";
    expect(shown()).toBe("0.90");
    choose(control("Wake model (dictation)", "select"), "hey_jarvis");
    expect(shown()).toBe("0.50");
    choose(control("Wake model (dictation)", "select"), "hey_thalvi");
    expect(shown()).toBe("0.70");
    button("Save").click();
    await settle();
    expect(sent(stub).wake.capture).toEqual({ model: "hey_thalvi", threshold: 0.7 });
  });

  it("shows the picked custom file name and saves its path", async () => {
    const stub = await openCapture();
    choose(control("Wake model (dictation)", "select"), "__custom");
    within(rowOf("Wake model (dictation)"), "Browse…").click();
    await settle();
    expect(stub.backend.pickWakeModelFile).toHaveBeenCalledTimes(1);
    expect(rowOf("Wake model (dictation)").textContent).toContain("custom.onnx");
    button("Save").click();
    await settle();
    expect(stub.backend.validateWakeModels).toHaveBeenCalledWith("/models/custom.onnx", "hey_mycroft");
    expect(sent(stub).wake.capture.model).toBe("/models/custom.onnx");
  });

  it("shows an invalid custom file at once and does not save it", async () => {
    const stub = await openCapture();
    stub.backend.pickWakeModelFile.mockResolvedValue("/models/bad.onnx");
    choose(control("Wake model (dictation)", "select"), "__custom");
    within(rowOf("Wake model (dictation)"), "Browse…").click();
    await settle();
    expect(problem("Wake model (dictation)")).toBe("The wake model /models/bad.onnx could not be loaded: not a wake model.");
    button("Save").click();
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    expect(root.querySelector(".err")?.textContent).toBe("The wake model /models/bad.onnx could not be loaded: not a wake model.");
  });

  it("does not save the same custom file for both wake words", async () => {
    const stub = await openCapture();
    for (const label of ["Wake model (dictation)", "Wake model (conversation)"] as const) {
      choose(control(label, "select"), "__custom");
      within(rowOf(label), "Browse…").click();
      await settle();
    }
    expect(problem("Wake model (conversation)")).toBe("The capture and conversation wake models must be different.");
    button("Save").click();
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
  });

  it("shows a saved custom file by name when Settings opens", async () => {
    await openCapture({
      ...SETTINGS,
      wake: { capture: { model: "/models/mine.onnx", threshold: 0.5 }, conversation: { model: "hey_mycroft", threshold: 0.5 } },
    });
    expect(control("Wake model (dictation)", "select").value).toBe("__custom");
    expect(rowOf("Wake model (dictation)").textContent).toContain("mine.onnx");
  });
});

describe("a custom speech model folder", () => {
  async function openLocal() {
    const stub = await open(local());
    goTo("speech");
    await editLocal();
    return stub;
  }
  const result = (): HTMLElement | null => rowOf("Speech model").querySelector<HTMLElement>(".note:not([hidden])");

  it("checks the typed folder and shows the plain answer inline", async () => {
    const stub = await openLocal();
    const input = control("Speech model", "input");
    type(input, "  /models/mine ");
    button("Check folder").click();
    await settle();
    expect(stub.backend.validateSpeechModelFolder).toHaveBeenLastCalledWith("/models/mine");
    expect(result()?.textContent).toBe("This folder holds a speech model.");

    stub.backend.validateSpeechModelFolder.mockRejectedValueOnce("Missing required files: encoder.int8.onnx, tokens.txt");
    button("Check folder").click();
    await settle();
    expect(result()?.textContent).toBe("Missing required files: encoder.int8.onnx, tokens.txt");

    type(input, "/models/other");
    expect(result()).toBeNull();
  });

  it("drops the answer of a check whose path was edited while it ran", async () => {
    const stub = await openLocal();
    const input = control("Speech model", "input");
    let answer: () => void = () => undefined;
    stub.backend.validateSpeechModelFolder.mockReturnValueOnce(new Promise<void>((resolve) => (answer = resolve)));
    type(input, "/models/mine");
    button("Check folder").click();
    type(input, "/models/unchecked");
    answer();
    await settle();
    expect(result()).toBeNull();
  });

  it("asks for a path instead of checking nothing or the built-in model", async () => {
    const stub = await openLocal();
    button("Check folder").click();
    expect(result()?.textContent).toBe("Type the folder path first.");
    type(control("Speech model", "input"), "nemotron_160ms");
    button("Check folder").click();
    expect(result()?.textContent).toBe("nemotron_160ms is downloaded on first use; there is no folder to check.");
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
  });

  it("keeps a valid folder as the entry's model once the folder is checked", async () => {
    const stub = await openLocal();
    type(control("Speech model", "input"), "/models/mine");
    await saveServer();
    expect(stub.backend.validateSpeechModelFolder).toHaveBeenCalledWith("/models/mine");
    expect(root.querySelector(".editor")?.hasAttribute("hidden")).toBe(true);
    button("Save").click();
    await settle();
    expect(deviceOf(sent(stub)).model).toBe("/models/mine");
  });

  it("does not keep a folder that fails the check, and says why", async () => {
    const stub = await openLocal();
    stub.backend.validateSpeechModelFolder.mockRejectedValueOnce("There is no folder at /models/gone.");
    type(control("Speech model", "input"), "/models/gone");
    await saveServer();
    expect(stub.backend.checkSpeechEntry).not.toHaveBeenCalled();
    expect(root.querySelector(".editor")?.hasAttribute("hidden")).toBe(false);
    expect(result()?.textContent).toBe("There is no folder at /models/gone.");
    expect(editorNote()).toBe("There is no folder at /models/gone.");
    expect(root.querySelector(".msg")?.textContent).toBe("");
  });

  it.each([
    ["the built-in model", local("nemotron_160ms")],
    ["no model", local()],
  ])("keeps %s without checking a folder", async (_name, settings) => {
    const stub = await open(settings);
    goTo("speech");
    await editLocal();
    await saveServer();
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
    expect(stub.backend.checkSpeechEntry).toHaveBeenCalledTimes(1);
  });
});

describe("the word list", () => {
  async function openWith(engine: "server" | "local", support: () => Promise<boolean>, phrases?: string[]) {
    const entries: SpeechEntry[] = [{ ...SERVER, active: engine === "server" }, device(undefined, engine === "local")];
    const stub = stubBackend({ ...SETTINGS, stt: { entries, ...(phrases ? { phrases } : {}) } });
    stub.backend.wordListSupport.mockImplementation(support);
    mountSettings(root, stub.backend);
    await settle();
    goTo("speech");
    return stub;
  }

  function box(): HTMLInputElement {
    return rowOf("Word list").querySelector('input[aria-label="New word"]') as HTMLInputElement;
  }

  function chips(): string[] {
    return Array.from(rowOf("Word list").querySelectorAll(".chip > span")).map((chip) => chip.textContent);
  }

  function add(word: string, by: "plus" | "enter" = "plus"): void {
    type(box(), word);
    if (by === "enter") box().dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    else (rowOf("Word list").querySelector('button[aria-label="Add the word"]') as HTMLButtonElement).click();
  }

  function offered(): boolean {
    const found = rowOf("Word list");
    return !found.hidden && !found.classList.contains("off");
  }

  it.each([
    ["the local engine", "local", () => Promise.reject(new Error("unreachable")), { shown: true, disabled: false, problem: "" }],
    ["a recognizer without boosting", "server", () => Promise.resolve(false), { shown: false, disabled: false, problem: "" }],
    ["a recognizer that boosts", "server", () => Promise.resolve(true), { shown: true, disabled: false, problem: "" }],
    [
      "a server that can't tell",
      "server",
      () => Promise.reject(new Error("Thalvi could not reach the server at ws://127.0.0.1:8080 to ask about a word list.")),
      {
        shown: true,
        disabled: true,
        problem: "Thalvi could not reach the server at ws://127.0.0.1:8080 to ask about a word list.",
      },
    ],
  ] as const)("is offered for %s only when it can be used", async (_name, engine, support, expected) => {
    const stub = await openWith(engine, support);
    expect(stub.backend.wordListSupport).toHaveBeenCalledTimes(engine === "server" ? 1 : 0);
    expect(offered()).toBe(expected.shown);
    expect(box().disabled).toBe(expected.disabled);
    const said = Array.from(rowOf("Word list").querySelectorAll(".note")).map((note) => note.textContent).join("");
    expect(said).toBe(expected.problem);
  });

  it("adds a typed word by + or Enter, refuses a blank or repeated one, and shares the list once per change", async () => {
    const stub = await openWith("server", () => Promise.resolve(true));
    add("  Cybrex2 ");
    add("Thalvi", "enter");
    expect(chips()).toEqual(["Cybrex2", "Thalvi"]);
    expect(box().value).toBe("");
    const problem = (): string => Array.from(rowOf("Word list").querySelectorAll(".note")).map((note) => note.textContent).join("");
    add("  ");
    expect(problem()).toBe("Type the word to add first.");
    add("Cybrex2");
    expect(problem()).toBe("Cybrex2 is already in the list.");
    expect(chips()).toEqual(["Cybrex2", "Thalvi"]);
    button("Save").click();
    await settle();
    expect(sent(stub).stt.phrases).toEqual(["Cybrex2", "Thalvi"]);
    expect(stub.backend.shareWordList).toHaveBeenCalledTimes(1);
    button("Save").click();
    await settle();
    expect(stub.backend.shareWordList).toHaveBeenCalledTimes(1);
  });

  it("says when the server could not take the list, and the save stands", async () => {
    const stub = await openWith("server", () => Promise.resolve(true));
    stub.backend.shareWordList.mockRejectedValue("The word list is saved here, but Thalvi could not reach the server.");
    add("Cybrex2");
    button("Save").click();
    await settle();
    expect(root.querySelector(".err")?.textContent).toBe("The word list is saved here, but Thalvi could not reach the server.");
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    expect(root.querySelector(".msg")?.textContent).not.toBe("Unsaved changes");
  });

  it("shows a saved list as chips and removes a word", async () => {
    const stub = await openWith("server", () => Promise.resolve(true), ["Cybrex2", "Thalvi"]);
    expect(chips()).toEqual(["Cybrex2", "Thalvi"]);
    (rowOf("Word list").querySelector('button[aria-label="Remove Cybrex2"]') as HTMLButtonElement).click();
    expect(chips()).toEqual(["Thalvi"]);
    button("Save").click();
    await settle();
    expect(sent(stub).stt.phrases).toEqual(["Thalvi"]);
  });

  it("is offered without asking the server once the engine switches to this computer", async () => {
    const stub = await openWith("server", () => Promise.reject(new Error("Thalvi could not reach the server.")));
    use("On this computer");
    await settle();
    expect(stub.backend.wordListSupport).toHaveBeenCalledTimes(1);
    expect(offered()).toBe(true);
    expect(box().disabled).toBe(false);
    expect(Array.from(rowOf("Word list").querySelectorAll(".note")).map((note) => note.textContent).join("")).toBe("");
  });

  it("asks again when the engine switches to the server", async () => {
    const stub = await openWith("local", () => Promise.resolve(true));
    use("Thalvi server");
    await settle();
    expect(stub.backend.wordListSupport).toHaveBeenCalledWith(expect.objectContaining({ kind: "thalvi_server", address: "ws://127.0.0.1:8080" }));
    expect(offered()).toBe(true);
  });

  it("keeps chips, the add box and messages in one column inside the word list controls", async () => {
    await openWith(
      "server",
      () => Promise.reject(new Error("Thalvi could not reach the server at ws://127.0.0.1:8080 to ask about a word list.")),
      ["Cybrex2"],
    );
    const found = rowOf("Word list");
    const ctl = found.querySelector(".ctl") as HTMLElement;
    const stack = ctl.firstElementChild as HTMLElement;
    expect(Array.from(ctl.children)).toEqual([stack]);
    const chipsEl = stack.querySelector(".chips");
    const addRow = stack.querySelector(".trow.add");
    const notes = Array.from(stack.querySelectorAll(":scope > .note"));
    expect(chips()).toEqual(["Cybrex2"]);
    expect(Array.from(stack.children)).toEqual([chipsEl, addRow, ...notes]);
    expect(notes.map((note) => note.textContent).join("")).toContain("could not reach the server");
    expect(addRow?.classList.contains("word-add")).toBe(true);
    goTo("capture");
    expect(rowOf("Ending phrases").querySelector(".trow.add")?.classList.contains("word-add")).toBe(false);
  });
});

const CYBREX: SpeechEntry = {
  kind: "realtime",
  id: "cybrex2",
  name: "Cybrex2",
  active: false,
  address: "ws://100.64.0.2:8012",
  model: "nemotron",
  has_key: true,
};
const OPENAI: SpeechEntry = {
  kind: "second_pass",
  id: "openai",
  name: "OpenAI",
  active: true,
  address: "https://api.openai.com/v1",
  model: "whisper-1",
  api: "openai",
  has_key: true,
};
const META: SpeechEntry = {
  kind: "second_pass",
  id: "meta",
  name: "Meta",
  active: false,
  address: "https://meta.example/v1",
  model: "asr",
  api: "meta",
};

/** A Thalvi server in use, a recognizer reached directly, the on-device model, and two second-pass servers. */
function several(): Settings {
  return { ...SETTINGS, stt: { entries: [{ ...SERVER }, { ...CYBREX }, device(undefined, false), { ...OPENAI }, { ...META }] } };
}

function entryNamed(name: string): HTMLElement {
  const found = Array.from(root.querySelectorAll<HTMLElement>(".trow.server")).find((entry) => entry.querySelector(".phrase")?.textContent === name);
  if (!found) throw new Error(`no entry ${name}`);
  return found;
}

const badgesOf = (name: string): (string | null)[] => Array.from(entryNamed(name).querySelectorAll(".badge")).map((badge) => badge.textContent);
const names = (list: HTMLElement): (string | null)[] => Array.from(list.querySelectorAll(".trow.server .phrase")).map((phrase) => phrase.textContent);
const lists = (): HTMLElement[] => Array.from(root.querySelectorAll<HTMLElement>("section[data-page=speech] .table"));

async function addServer(kind: string): Promise<void> {
  choose(root.querySelector('select[aria-label="Kind of speech server to add"]') as HTMLSelectElement, kind);
  (root.querySelector('button[aria-label="Add a speech server"]') as HTMLButtonElement).click();
  await settle();
}

describe("the speech server list", () => {
  async function openSeveral() {
    const stub = await open(several());
    goTo("speech");
    return stub;
  }

  it("lists each server with its kind and where it is, and marks the one in use", async () => {
    await openSeveral();
    const [live, second] = lists();
    expect(names(live as HTMLElement)).toEqual(["Thalvi server", "Cybrex2", "On this computer"]);
    expect(names(second as HTMLElement)).toEqual(["OpenAI", "Meta"]);
    expect(badgesOf("Thalvi server")).toEqual(["Thalvi server", "Active"]);
    expect(badgesOf("Cybrex2")).toEqual(["Realtime speech server", "Key saved"]);
    expect(badgesOf("On this computer")).toEqual(["On this computer"]);
    expect(entryNamed("Cybrex2").querySelector(".unit")?.textContent).toBe("ws://100.64.0.2:8012 · nemotron");
    expect(entryNamed("On this computer").querySelector(".unit")?.textContent).toBe("nemotron_160ms");
    expect(entryNamed("OpenAI").querySelector(".unit")?.textContent).toBe("https://api.openai.com/v1 · whisper-1 · OpenAI-style");
    expect((root.querySelector('button[aria-label="Use Thalvi server"]') as HTMLButtonElement).disabled).toBe(true);
    expect((root.querySelector('button[aria-label="Remove Thalvi server"]') as HTMLButtonElement).disabled).toBe(true);
  });

  it("switches the live server with one click, saving only that and nothing else that is unsaved", async () => {
    const stub = await openSeveral();
    goTo("capture");
    type(control("Quiet before paste", "input"), "1.5");
    goTo("speech");
    use("Cybrex2");
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    const savedSettings = sent(stub);
    expect(savedSettings.ending_pause.paste).toBe(SETTINGS.ending_pause.paste);
    expect(savedSettings.stt.entries.map((entry) => [entry.name, entry.active])).toEqual([
      ["Thalvi server", false],
      ["Cybrex2", true],
      ["On this computer", false],
      ["OpenAI", true],
      ["Meta", false],
    ]);
    expect(badgesOf("Cybrex2")).toContain("Active");
    expect(badgesOf("Thalvi server")).not.toContain("Active");
    expect((root.querySelector('button[aria-label="Use Cybrex2"]') as HTMLButtonElement).disabled).toBe(true);
    expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
  });

  it("keeps the server in use when the switch is refused, and says why", async () => {
    const stub = await openSeveral();
    stub.backend.saveSettings.mockRejectedValueOnce("The settings could not be saved to /config: disk full.");
    use("Cybrex2");
    await settle();
    expect(root.querySelector(".err")?.textContent).toBe("The settings could not be saved to /config: disk full.");
    expect(badgesOf("Thalvi server")).toContain("Active");
    expect(badgesOf("Cybrex2")).not.toContain("Active");
  });

  it("adds a server once its fields are accepted, shown as not saved until Save", async () => {
    const stub = await openSeveral();
    await addServer("realtime");
    expect(root.querySelector(".editor h4")?.textContent).toBe("Add a realtime speech server");
    expect(control("Name", "input").value).toBe("Realtime speech server");
    type(control("Name", "input"), "Office GPU");
    type(control("Address", "input"), "ws://10.0.0.5:8012");
    type(control("Model", "input"), "nemotron");
    type(control("API key", "input"), "sk-new");
    await saveServer();
    expect(stub.backend.checkSpeechEntry).toHaveBeenCalledTimes(1);
    const checked = stub.backend.checkSpeechEntry.mock.calls[0]?.[0];
    expect(checked).toMatchObject({ kind: "realtime", name: "Office GPU", address: "ws://10.0.0.5:8012", model: "nemotron", api_key: "sk-new", active: false });
    expect(checked).not.toHaveProperty("has_key");
    expect(root.querySelector(".editor")?.hasAttribute("hidden")).toBe(true);
    expect(badgesOf("Office GPU")).toEqual(["Realtime speech server", "Key saved", "Not saved"]);
    expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();

    button("Save").click();
    await settle();
    expect(sent(stub).stt.entries.find((entry) => entry.name === "Office GPU")).toMatchObject({ active: false, api_key: "sk-new" });
    expect(badgesOf("Office GPU")).toEqual(["Realtime speech server", "Key saved"]);
  });

  it("does not add a server the app refuses, and keeps the editor open with the reason", async () => {
    const stub = await openSeveral();
    stub.backend.checkSpeechEntry.mockRejectedValueOnce('Speech server "Realtime speech server": the address must be a ws or wss URL.');
    await addServer("realtime");
    type(control("Address", "input"), "http://nope");
    await saveServer();
    expect(editorNote()).toBe('Speech server "Realtime speech server": the address must be a ws or wss URL.');
    expect(root.querySelector(".editor")?.hasAttribute("hidden")).toBe(false);
    expect(names(lists()[0] as HTMLElement)).toEqual(["Thalvi server", "Cybrex2", "On this computer"]);
    expect(root.querySelector(".msg")?.textContent).toBe("");
  });

  it("keeps every name distinct before asking the app", async () => {
    const stub = await openSeveral();
    await addServer("thalvi_server");
    type(control("Name", "input"), "cybrex2");
    await saveServer();
    expect(editorNote()).toBe('Two speech servers are called "cybrex2". Give each its own name.');
    expect(stub.backend.checkSpeechEntry).not.toHaveBeenCalled();
  });

  it("edits a server in place, cancels without a trace, and removes one that is not in use", async () => {
    await openSeveral();
    edit("Cybrex2");
    expect(control("Address", "input").value).toBe("ws://100.64.0.2:8012");
    type(control("Name", "input"), "Cybrex2 GPU");
    type(control("Address", "input"), "ws://100.64.0.3:8012");
    await saveServer();
    expect(names(lists()[0] as HTMLElement)).toEqual(["Thalvi server", "Cybrex2 GPU", "On this computer"]);
    expect(entryNamed("Cybrex2 GPU").querySelector(".unit")?.textContent).toBe("ws://100.64.0.3:8012 · nemotron");
    expect(badgesOf("Cybrex2 GPU")).toContain("Not saved");

    edit("Cybrex2 GPU");
    type(control("Name", "input"), "Dropped");
    button("Cancel").click();
    expect(names(lists()[0] as HTMLElement)).toContain("Cybrex2 GPU");

    (root.querySelector('button[aria-label="Remove Cybrex2 GPU"]') as HTMLButtonElement).click();
    expect(names(lists()[0] as HTMLElement)).toEqual(["Thalvi server", "On this computer"]);
  });

  const keyActions: [string, (clear: HTMLButtonElement) => void, string | undefined, boolean][] = [
    ["leaves it unchanged", () => undefined, undefined, true],
    ["replaces it", () => { type(control("API key", "input"), "sk-new"); }, "sk-new", true],
    ["clears it", (clear) => { clear.click(); }, "", false],
  ];
  it.each(keyActions)("shows a saved key only as saved, and %s", async (_name, act, sentKey, stillSaved) => {
    const stub = await openSeveral();
    edit("Cybrex2");
    expect(control("API key", "input").value).toBe("");
    expect(control("API key", "input").placeholder).toBe("Saved — leave blank to keep it");
    act(within(rowOf("API key"), "Clear saved key"));
    await saveServer();
    button("Save").click();
    await settle();
    const entry = sent(stub).stt.entries.find((known) => known.id === "cybrex2");
    expect(entry && "api_key" in entry ? entry.api_key : undefined).toBe(sentKey);
    expect(badgesOf("Cybrex2").includes("Key saved")).toBe(stillSaved);
  });

  it("asks the app about the word list for the server it switches to", async () => {
    const stub = await openSeveral();
    stub.backend.wordListSupport.mockResolvedValue(true);
    use("Cybrex2");
    await settle();
    expect(stub.backend.wordListSupport).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "realtime", id: "cybrex2" }));
  });
});

describe("second-pass servers", () => {
  async function openSeveral() {
    const stub = await open(several());
    goTo("speech");
    return stub;
  }
  const tickOf = (name: string): HTMLInputElement => entryNamed(name).querySelector("input[type=checkbox]") as HTMLInputElement;

  it("runs the ticked ones, and a tick or an untick applies at once without losing the setup", async () => {
    const stub = await openSeveral();
    expect([tickOf("OpenAI").checked, tickOf("Meta").checked]).toEqual([true, false]);
    flip(tickOf("Meta"));
    await settle();
    expect(sent(stub).stt.entries.filter((entry) => entry.kind === "second_pass").map((entry) => [entry.name, entry.active])).toEqual([
      ["OpenAI", true],
      ["Meta", true],
    ]);
    flip(tickOf("OpenAI"));
    await settle();
    const unticked = stub.backend.saveSettings.mock.calls[1]?.[0].stt.entries.find((entry) => entry.id === "openai");
    expect(unticked).toMatchObject({ active: false, address: "https://api.openai.com/v1", model: "whisper-1", has_key: true });
    expect(root.querySelector(".msg")?.textContent).not.toBe("Unsaved changes");
  });

  it("shows a tick that was refused as still off", async () => {
    const stub = await openSeveral();
    stub.backend.saveSettings.mockRejectedValueOnce("The settings could not be saved.");
    flip(tickOf("Meta"));
    await settle();
    expect(tickOf("Meta").checked).toBe(false);
    expect(root.querySelector(".err")?.textContent).toBe("The settings could not be saved.");
  });

  it("tries them in list order, which the arrows change and Save keeps", async () => {
    const stub = await openSeveral();
    expect((entryNamed("OpenAI").querySelector('button[aria-label="Move OpenAI up"]') as HTMLButtonElement).disabled).toBe(true);
    (entryNamed("Meta").querySelector('button[aria-label="Move Meta up"]') as HTMLButtonElement).click();
    expect(names(lists()[1] as HTMLElement)).toEqual(["Meta", "OpenAI"]);
    expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
    button("Save").click();
    await settle();
    expect(sent(stub).stt.entries.filter((entry) => entry.kind === "second_pass").map((entry) => entry.name)).toEqual(["Meta", "OpenAI"]);
  });

  it("adds a second-pass API, ticks it in the draft only, and saves it with Save", async () => {
    const stub = await openSeveral();
    button("Add a second-pass API").click();
    await settle();
    expect(control("Name", "input").value).toBe("Second pass");
    type(control("Address", "input"), "http://127.0.0.1:9000/v1");
    type(control("Model", "input"), "whisper");
    choose(control("API", "select"), "meta");
    type(control("API key", "input"), "k");
    expect(control("API key", "input").type).toBe("password");
    await saveServer();
    expect(stub.backend.checkSpeechEntry).toHaveBeenLastCalledWith(
      expect.objectContaining({ kind: "second_pass", address: "http://127.0.0.1:9000/v1", model: "whisper", api: "meta", api_key: "k" }),
    );
    expect(badgesOf("Second pass")).toContain("Not saved");
    flip(tickOf("Second pass"));
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    button("Save").click();
    await settle();
    expect(sent(stub).stt.entries.at(-1)).toMatchObject({ name: "Second pass", active: true, api: "meta", api_key: "k" });
  });
});


describe("the attached speech editor", () => {
  async function openSeveral() {
    const stub = await open(several());
    goTo("speech");
    return stub;
  }
  const editorEl = (): HTMLElement => root.querySelector(".editor") as HTMLElement;
  const editButton = (name: string): HTMLButtonElement => root.querySelector(`button[aria-label="Edit ${name}"]`) as HTMLButtonElement;

  it.each([
    ["Cybrex2", 0, "Editing “Cybrex2” · live dictation"],
    ["On this computer", 0, "Editing “On this computer” · live dictation"],
    ["OpenAI", 1, "Editing “OpenAI” · second pass"],
  ] as const)("opens the editor right after the %s row in its own list", async (name, listIndex, heading) => {
    await openSeveral();
    edit(name);
    await settle();
    const editor = editorEl();
    const row = entryNamed(name);
    expect(editor.hidden).toBe(false);
    expect(editor.parentElement).toBe(lists()[listIndex]);
    expect(editor.previousElementSibling).toBe(row);
    const title = editor.querySelector("h4") as HTMLElement;
    expect(title.textContent).toBe(heading);
    expect(editor.getAttribute("aria-labelledby")).toBe(title.id);
    expect(title.id).not.toBe("");
    expect(editButton(name).getAttribute("aria-expanded")).toBe("true");
    expect(document.activeElement).toBe(editor.querySelector("input"));
    button("Cancel").click();
    expect(editor.hidden).toBe(true);
    expect(editButton(name).getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(editButton(name));
  });

  it("moves the one editor when another entry is edited", async () => {
    await openSeveral();
    edit("Cybrex2");
    await settle();
    edit("OpenAI");
    await settle();
    expect(root.querySelectorAll(".editor")).toHaveLength(1);
    expect(editorEl().previousElementSibling).toBe(entryNamed("OpenAI"));
    expect(editorEl().parentElement).toBe(lists()[1]);
    expect(editButton("Cybrex2").getAttribute("aria-expanded")).toBe("false");
    expect(editButton("OpenAI").getAttribute("aria-expanded")).toBe("true");
  });

  it("opens the add editor at the end of the live list, and closes the editor when its entry is removed", async () => {
    await openSeveral();
    await addServer("realtime");
    expect(editorEl().hidden).toBe(false);
    expect(editorEl().parentElement).toBe(lists()[0]);
    expect(editorEl().previousElementSibling).toBe(entryNamed("On this computer"));
    button("Cancel").click();
    edit("Cybrex2");
    await settle();
    expect(editorEl().hidden).toBe(false);
    (root.querySelector('button[aria-label="Remove Cybrex2"]') as HTMLButtonElement).click();
    expect(editorEl().hidden).toBe(true);
    expect(names(lists()[0] as HTMLElement)).not.toContain("Cybrex2");
  });
});

describe("the speech model catalog", () => {
  async function openCatalog(settings: Settings = local()) {
    const stub = await open(settings);
    goTo("speech");
    await editLocal();
    return stub;
  }
  const card = (): HTMLElement => {
    const found = rowOf("Speech model").querySelector<HTMLElement>(".model:not([hidden])");
    if (!found) throw new Error("no model card");
    return found;
  };
  const text = (selector: string): string | undefined => card().querySelector(selector)?.textContent ?? undefined;
  const alert = (): string | undefined => card().querySelector(".note:not([hidden])")?.textContent ?? undefined;
  const download = (): HTMLButtonElement => within(card(), "Download");
  const meter = (): HTMLProgressElement => card().querySelector("progress") as HTMLProgressElement;
  const picker = (): HTMLSelectElement => control("Speech model", "select");
  const languages = (): HTMLSelectElement => control("Language", "select");
  const listed = (select: HTMLSelectElement): (string | null)[][] =>
    Array.from(select.options).map((option) => [option.value, option.textContent]);
  const OWN = ["__folder", "Your own model folder"];
  const STREAMING = [NEMOTRON.name, "Nemotron Streaming (English, 662 MB)"];
  const UNIFIED_OPTION = [UNIFIED.name, "Nemotron Unified (25 languages, 647 MB)"];

  it("offers each model and an own folder in labelled drop-downs, with the default picked", async () => {
    await openCatalog();
    expect(languages().getAttribute("aria-label")).toBe("Language");
    expect(rowOf("Language").querySelector("label")?.getAttribute("for")).toBe(languages().id);
    expect(picker().getAttribute("aria-label")).toBe("Speech model");
    expect(rowOf("Speech model").querySelector("label")?.getAttribute("for")).toBe(picker().id);
    expect(listed(picker())).toEqual([STREAMING, UNIFIED_OPTION, OWN]);
    expect(picker().value).toBe("nemotron_160ms");
    expect(languages().value).toBe("");
  });

  it("lists All languages first, then every language of the catalog sorted", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockResolvedValueOnce([
      { ...UNIFIED, languages: ["Spanish", "Dutch", "English"] },
      { ...NEMOTRON, languages: ["English", "Czech"] },
    ]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("speech");
    await editLocal();
    expect(listed(languages())).toEqual([
      ["", "All languages"],
      ["Czech", "Czech"],
      ["Dutch", "Dutch"],
      ["English", "English"],
      ["Spanish", "Spanish"],
    ]);
  });

  it("shows only the models of the chosen language, and always the own folder", async () => {
    await openCatalog();
    choose(languages(), "Dutch");
    expect(listed(picker())).toEqual([UNIFIED_OPTION, OWN]);
    choose(languages(), "English");
    expect(listed(picker())).toEqual([STREAMING, UNIFIED_OPTION, OWN]);
    choose(languages(), "");
    expect(listed(picker())).toEqual([STREAMING, UNIFIED_OPTION, OWN]);
  });

  it("moves the selection to the first model of a language the selected one lacks, unsaved until Save", async () => {
    const stub = await openCatalog();
    expect(root.querySelector(".msg")?.textContent).toBe("");
    choose(languages(), "Dutch");
    expect(picker().value).toBe("unified_160ms");
    expect(text(".facts")).toContain("OpenMDW-1.1");
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    await saveServer();
    expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
    button("Save").click();
    await settle();
    expect(deviceOf(sent(stub)).model).toBe("unified_160ms");
    expect(JSON.stringify(sent(stub))).not.toContain("Dutch");
  });

  it("keeps the selection when All languages or a language it supports is picked", async () => {
    const stub = await openCatalog(local("unified_160ms"));
    choose(languages(), "");
    expect(picker().value).toBe("unified_160ms");
    choose(languages(), "English");
    expect(picker().value).toBe("unified_160ms");
    choose(languages(), "Dutch");
    choose(languages(), "");
    expect(picker().value).toBe("unified_160ms");
    await saveServer();
    button("Save").click();
    await settle();
    expect(deviceOf(sent(stub)).model).toBe("unified_160ms");
  });

  it("leaves the own folder selected when the language changes", async () => {
    await openCatalog(local("/models/mine"));
    choose(languages(), "Dutch");
    expect(picker().value).toBe("__folder");
    expect(control("Speech model", "input").value).toBe("/models/mine");
  });

  it("keeps the own folder usable with only All languages when the list fails to load", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockRejectedValueOnce("Thalvi could not find a folder to keep its models in.");
    mountSettings(root, stub.backend);
    await settle();
    goTo("speech");
    await editLocal();
    expect(listed(languages())).toEqual([["", "All languages"]]);
    expect(listed(picker())).toEqual([OWN]);
    choose(picker(), "__folder");
    type(control("Speech model", "input"), "/models/mine");
    await saveServer();
    button("Save").click();
    await settle();
    expect(deviceOf(sent(stub)).model).toBe("/models/mine");
  });

  it("shows the picked model's description, size, license, languages, state and source", async () => {
    await openCatalog();
    expect(rowOf("Speech model").querySelectorAll(".model:not([hidden])")).toHaveLength(1);
    expect(card().textContent).toContain(NEMOTRON.description);
    expect(text(".facts")).toBe("License: NVIDIA Open Model License. Languages: English.");
    expect(text(".unit")).toBe("662 MB");
    expect(text(".badge")).toBe("Not downloaded");
    expect(card().querySelector("a")?.getAttribute("href")).toBe(NEMOTRON.source);
    expect(download().hidden).toBe(false);
  });

  it.each([
    [661_919_416, "662 MB"],
    [1_540_000_000, "1.5 GB"],
    [8_952, "9 kB"],
  ])("shows %i bytes as %s", async (size, shown) => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...NEMOTRON, size }]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("speech");
    await editLocal();
    expect(text(".unit")).toBe(shown);
  });

  it("marks a downloaded model and offers no download", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...NEMOTRON, cached: true }]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("speech");
    await editLocal();
    expect(text(".badge")).toBe("Downloaded");
    expect(download().hidden).toBe(true);
  });

  it("shows the download's progress and flips the badge when it is done", async () => {
    const stub = await openCatalog();
    let finish: () => void = () => undefined;
    stub.backend.downloadSpeechModel.mockReturnValueOnce(new Promise<void>((resolve) => { finish = resolve; }));
    download().click();
    expect(stub.backend.downloadSpeechModel).toHaveBeenCalledWith("nemotron_160ms");
    expect(download().disabled).toBe(true);
    expect(text("[role=status]")).toBe("Checking files…");

    stub.speechProgress({ name: "nemotron_160ms", percent: 40, status: "downloading", message: null });
    expect(text("[role=status]")).toBe("Downloading… 40%");
    expect(meter().value).toBe(40);
    expect(meter().hidden).toBe(false);
    expect(text(".badge")).toBe("Not downloaded");

    stub.speechProgress({ name: "nemotron_160ms", percent: 100, status: "done", message: null });
    finish();
    await settle();
    expect(text(".badge")).toBe("Downloaded");
    expect(meter().hidden).toBe(true);
    expect(text("[role=status]")).toBe("");
    expect(download().hidden).toBe(true);
  });

  it("says plainly why a download failed and lets the user try again", async () => {
    const stub = await openCatalog();
    stub.backend.downloadSpeechModel.mockRejectedValueOnce("Could not download the speech model encoder. Check the internet connection and try again.");
    download().click();
    await settle();
    expect(alert()).toBe("Could not download the speech model encoder. Check the internet connection and try again.");
    expect(text(".badge")).toBe("Not downloaded");
    expect(meter().hidden).toBe(true);
    expect(download().disabled).toBe(false);

    download().click();
    expect(alert()).toBeUndefined();
    await settle();
    expect(text(".badge")).toBe("Downloaded");
  });

  it("shows the reason from a failed event too", async () => {
    const stub = await openCatalog();
    stub.speechProgress({ name: "nemotron_160ms", percent: 0, status: "failed", message: "The download did not match its checksum." });
    expect(alert()).toBe("The download did not match its checksum.");
  });

  it("opens the source page through the app, not the web view", async () => {
    const stub = await openCatalog();
    const link = card().querySelector("a") as HTMLAnchorElement;
    const event = new MouseEvent("click", { bubbles: true, cancelable: true });
    link.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(stub.backend.openSpeechModelSource).toHaveBeenCalledWith("nemotron_160ms");

    stub.backend.openSpeechModelSource.mockRejectedValueOnce("No browser could be opened.");
    link.click();
    await settle();
    expect(alert()).toBe("No browser could be opened.");
  });

  it("says why the catalog could not be listed", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockRejectedValueOnce("Thalvi could not find a folder to keep its models in.");
    mountSettings(root, stub.backend);
    await settle();
    goTo("speech");
    await editLocal();
    expect(rowOf("Speech model").querySelector(".note:not([hidden])")?.textContent).toBe(
      "Thalvi could not find a folder to keep its models in.",
    );
  });

  it("switches the card and saves the model picked in the drop-down as stt.model", async () => {
    const stub = await openCatalog();
    type(control("Speech model", "input"), "/models/mine");
    expect(picker().value).toBe("__folder");

    choose(picker(), "unified_160ms");
    expect(control("Speech model", "input").value).toBe("");
    expect(text(".facts")).toBe(
      `License: OpenMDW-1.1. Languages: ${UNIFIED.languages.join(", ")}. The language is detected automatically.`,
    );
    expect(text(".unit")).toBe("647 MB");
    await saveServer();
    button("Save").click();
    await settle();
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
    expect(deviceOf(sent(stub)).model).toBe("unified_160ms");
  });

  it("keeps showing the download of a model while another one is picked", async () => {
    const stub = await openCatalog();
    stub.speechProgress({ name: "unified_160ms", percent: 40, status: "downloading", message: null });
    expect(rowOf("Speech model").querySelector(".badge")?.textContent).toBe("Not downloaded");
    choose(picker(), "unified_160ms");
    expect(text("[role=status]")).toBe("Downloading… 40%");
    expect(meter().value).toBe(40);
  });

  it("shows a saved model, and a saved folder as the own-folder entry", async () => {
    await openCatalog(local("unified_160ms"));
    expect(picker().value).toBe("unified_160ms");
    expect(text(".facts")).toContain("OpenMDW-1.1");
    root.innerHTML = "";
    await openCatalog(local("/models/mine"));
    expect(picker().value).toBe("__folder");
    expect(control("Speech model", "input").value).toBe("/models/mine");
    expect(rowOf("Speech model").querySelector(".model:not([hidden])")).toBeNull();
  });

  it("does not save the own-folder entry without a path", async () => {
    const stub = await openCatalog();
    choose(picker(), "__folder");
    await saveServer();
    expect(stub.backend.checkSpeechEntry).not.toHaveBeenCalled();
    expect(rowOf("Speech model").querySelector(".own .note:not([hidden])")?.textContent).toBe("Type the folder path first.");
    expect(editorNote()).toBe("Type the folder path of your own speech model first.");
  });
});

describe("writing", () => {
  const endpoint = { url: "http://127.0.0.1:8000/v1", model: "glimmer", api_key: "k" };

  it("saves the endpoint with its key masked, without calling it", async () => {
    const stub = await open();
    goTo("speech");
    expect(sent_writing_fields()).toEqual(["", "", ""]);
    type(control("Writing endpoint address", "input"), endpoint.url);
    type(control("Writing model", "input"), endpoint.model);
    type(control("Writing API key", "input"), endpoint.api_key);
    expect(control("Writing API key", "input").type).toBe("password");
    const calls = () => Object.entries(stub.backend).map(([name, call]) => [name, call.mock.calls.length]);
    const before = calls();
    button("Save").click();
    await settle();
    expect(sent(stub).writing).toEqual(endpoint);
    const grew = calls()
      .filter(([, count], index) => count !== before[index]?.[1])
      .map(([name]) => name);
    expect(grew).toEqual(["saveSettings"]);
  });

  it("shows a saved endpoint and saves nothing for one that was cleared", async () => {
    const stub = await open({ ...SETTINGS, writing: { url: endpoint.url, model: endpoint.model, has_key: true } });
    goTo("speech");
    expect(sent_writing_fields()).toEqual([endpoint.url, endpoint.model, ""]);
    expect(control("Writing API key", "input").placeholder).toBe("A key is saved");
    for (const label of ["Writing endpoint address", "Writing model", "Writing API key"]) type(control(label, "input"), "");
    button("Save").click();
    expect(sent(stub).writing).toBeNull();
  });

  it("keeps the saved writing key unless its field is edited", async () => {
    const writing = { url: endpoint.url, model: endpoint.model, has_key: true };
    const stub = await open({ ...SETTINGS, writing });
    goTo("speech");
    type(control("Writing model", "input"), "updated");
    button("Save").click();
    await settle();
    expect(sent(stub).writing).toEqual({ ...writing, model: "updated" });
    button("Clear saved writing key").click();
    button("Save").click();
    await settle();
    expect(stub.backend.saveSettings.mock.calls[1]?.[0].writing).toEqual({ ...writing, model: "updated", api_key: "" });
  });

  describe("prompt", () => {
    const fallback = SETTINGS.writing_default_prompt;
    const area = (): HTMLTextAreaElement => rowOf("Writing prompt").querySelector("textarea") as HTMLTextAreaElement;
    const restore = (): HTMLButtonElement => button("Restore default prompt");
    const edit = (value: string): void => {
      area().value = value;
      area().dispatchEvent(new Event("input", { bubbles: true }));
    };
    const saved = { url: endpoint.url, model: endpoint.model, has_key: true };

    it("shows the default, saves an edit with the endpoint, and restores the default", async () => {
      const stub = await open({ ...SETTINGS, writing: saved });
      goTo("speech");
      expect(area().value).toBe(fallback);
      expect(restore().disabled).toBe(true);

      edit("Write short emails.\nNo greeting.");
      expect(restore().disabled).toBe(false);
      button("Save").click();
      await settle();
      expect(sent(stub).writing).toEqual({ ...saved, prompt: "Write short emails.\nNo greeting." });

      restore().click();
      expect(area().value).toBe(fallback);
      expect(restore().disabled).toBe(true);
      button("Save").click();
      await settle();
      expect(stub.backend.saveSettings.mock.calls[1]?.[0].writing).toEqual(saved);
    });

    it("shows a saved prompt when Settings is reopened and keeps it through other edits", async () => {
      const writing = { ...saved, prompt: "Be brief." };
      const stub = await open({ ...SETTINGS, writing });
      goTo("speech");
      expect(area().value).toBe("Be brief.");
      expect(restore().disabled).toBe(false);
      type(control("Writing model", "input"), "updated");
      button("Save").click();
      await settle();
      expect(sent(stub).writing).toEqual({ ...writing, model: "updated" });
    });

    it("does not drop an edited prompt when the endpoint is empty, and shows the refusal for a blank one", async () => {
      const stub = await open();
      goTo("speech");
      edit("Be brief.");
      button("Save").click();
      await settle();
      expect(sent(stub).writing).toEqual({ url: "", model: "", prompt: "Be brief." });

      stub.backend.saveSettings.mockRejectedValue("writing.prompt must not be blank. Restore the default to start over.");
      edit("  ");
      button("Save").click();
      await settle();
      expect(root.querySelector(".err")?.textContent).toBe("writing.prompt must not be blank. Restore the default to start over.");
      expect(area().value).toBe("  ");
      expect(stub.backend.saveSettings.mock.calls[1]?.[0].writing).toEqual({ url: "", model: "", prompt: "  " });
    });
  });

  function sent_writing_fields(): string[] {
    return ["Writing endpoint address", "Writing model", "Writing API key"].map((label) => control(label, "input").value);
  }
});

describe("search", () => {
  it("shows matching rows from every page and nothing else", async () => {
    await open();
    const search = root.querySelector<HTMLInputElement>(".search") as HTMLInputElement;
    type(search, "quiet");
    const visible = Array.from(root.querySelectorAll<HTMLElement>(".row")).filter((row) => !row.hidden && !row.closest("[hidden]"));
    expect(visible.map((row) => row.dataset["label"])).toEqual(["quiet before paste", "quiet before paste and enter", "quiet before running a task"]);

    type(search, "sound");
    const sounds = Array.from(root.querySelectorAll<HTMLElement>(".row")).filter((row) => !row.hidden && !row.closest("[hidden]"));
    expect(sounds).toHaveLength(5);

    type(search, "zzz");
    expect(root.querySelector(".empty")?.hasAttribute("hidden")).toBe(false);

    type(search, "");
    expect(root.querySelector<HTMLElement>("section[data-page=speech]")?.hidden).toBe(false);
    expect(root.querySelector<HTMLElement>("section[data-page=capture]")?.hidden).toBe(true);
  });
});

describe("live controls", () => {
  it("turns listening off and puts the switch back when that fails", async () => {
    const stub = await open();
    const toggle = control("Listening", "input");
    flip(toggle);
    expect(stub.backend.setListening).toHaveBeenCalledWith(false);

    stub.backend.setListening.mockRejectedValue("No microphone.");
    flip(toggle);
    await settle();
    expect(toggle.checked).toBe(false);
    expect(rowOf("Listening").querySelector(".note")?.textContent).toBe("No microphone.");
  });

  it("starts at login only once the OS holds the entry, and shows the OS state on load", async () => {
    const stub = await open();
    const toggle = control("Start Thalvi when I log in", "input");
    expect(toggle.checked).toBe(false);
    expect(stub.backend.getStartAtLogin).toHaveBeenCalledTimes(1);

    flip(toggle);
    await settle();
    expect(stub.backend.setStartAtLogin).toHaveBeenLastCalledWith(true);
    expect(toggle.checked).toBe(true);
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();

    stub.backend.getStartAtLogin.mockResolvedValue({ kind: "available", on: true, problem: null });
    document.body.innerHTML = '<div id="settings"></div>';
    root = document.getElementById("settings") as HTMLElement;
    mountSettings(root, stub.backend);
    await settle();
    expect(control("Start Thalvi when I log in", "input").checked).toBe(true);
  });

  it("shows the refusal and the real state when the startup entry cannot be changed", async () => {
    const stub = await open();
    const toggle = control("Start Thalvi when I log in", "input");
    stub.backend.setStartAtLogin.mockRejectedValue("Thalvi could not be set to start at login: permission denied");
    flip(toggle);
    expect(toggle.disabled).toBe(true);
    await settle();
    expect(toggle.checked).toBe(false);
    expect(toggle.disabled).toBe(false);
    expect(rowOf("Start Thalvi when I log in").querySelector(".note")?.textContent).toBe(
      "Thalvi could not be set to start at login: permission denied",
    );

    stub.backend.getStartAtLogin.mockResolvedValue({ kind: "available", on: true, problem: null });
    stub.backend.setStartAtLogin.mockRejectedValue("Thalvi could not be stopped from starting at login: busy");
    flip(toggle);
    await settle();
    expect(toggle.checked).toBe(true);
  });

  it("says why the entry was not put back on start while leaving the switch usable", async () => {
    const stub = stubBackend();
    stub.backend.getStartAtLogin.mockResolvedValue({
      kind: "available",
      on: false,
      problem: "Thalvi is turned off in the system's list of startup apps. Turn it on there, or switch it off and on here.",
    });
    mountSettings(root, stub.backend);
    await settle();
    const toggle = control("Start Thalvi when I log in", "input");
    expect(toggle.checked).toBe(false);
    expect(toggle.disabled).toBe(false);
    expect(rowOf("Start Thalvi when I log in").querySelector(".note")?.textContent).toBe(
      "Thalvi is turned off in the system's list of startup apps. Turn it on there, or switch it off and on here.",
    );
  });

  it("keeps the switch off and says why when the OS entry cannot be read", async () => {
    const stub = stubBackend();
    stub.backend.getStartAtLogin.mockRejectedValue("Thalvi could not check its startup entry: no home folder");
    mountSettings(root, stub.backend);
    await settle();
    const toggle = control("Start Thalvi when I log in", "input");
    expect(toggle.checked).toBe(false);
    expect(toggle.disabled).toBe(true);
    expect(rowOf("Start Thalvi when I log in").querySelector(".note")?.textContent).toBe(
      "Thalvi could not check its startup entry: no home folder",
    );
  });

  it("says why there is no hold key where the desktop gives the app no way to see it, and shows no row elsewhere", async () => {
    const stub = stubBackend();
    stub.backend.getHoldKeyUnavailable.mockResolvedValue("GNOME Wayland does not tell Thalvi when a key is let go.");
    mountSettings(root, stub.backend);
    await settle();
    const hold = rowOf("Hold a key to dictate");
    expect(hold.querySelector("input")).toBeNull();
    expect(hold.textContent).toContain("GNOME Wayland does not tell Thalvi when a key is let go.");
    expect(hold.closest("section")?.dataset["page"]).toBe("capture");
    const write = rowOf("Hold a key to write");
    expect(write.querySelector("input")).toBeNull();
    expect(write.textContent).toContain("GNOME Wayland does not tell Thalvi when a key is let go.");
  });

  it("shows the line for hyprland.conf instead of a switch and copies it", async () => {
    const stub = stubBackend();
    const line = "exec-once = /home/me/Thalvi.AppImage --login";
    stub.backend.getStartAtLogin.mockResolvedValue({ kind: "unavailable", reason: "Hyprland does not start login entries.", line });
    mountSettings(root, stub.backend);
    await settle();
    const login = rowOf("Start Thalvi when I log in");
    expect(login.querySelector("input")).toBeNull();
    expect(login.querySelector("code")?.textContent).toBe(line);
    expect(login.textContent).toContain("Hyprland does not start login entries.");

    within(login, "Copy").click();
    await settle();
    expect(stub.backend.copyLoginLine).toHaveBeenCalledTimes(1);
    expect(within(login, "Copied")).toBeDefined();

    stub.backend.copyLoginLine.mockRejectedValue("wl-copy is not installed.");
    within(login, "Copied").click();
    await settle();
    expect(login.querySelector(".note:not(.ok)")?.textContent).toBe("wl-copy is not installed.");
  });

  it("adds and removes the app menu entry at once, and shows the real state after a failure", async () => {
    const stub = await open();
    const toggle = control("Show Thalvi in the app menu", "input");
    expect(toggle.checked).toBe(false);
    flip(toggle);
    await settle();
    expect(stub.backend.setMenuEntry).toHaveBeenLastCalledWith(true);
    expect(toggle.checked).toBe(true);
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();

    stub.backend.getMenuEntry.mockResolvedValue({ kind: "available", on: true, problem: null });
    stub.backend.setMenuEntry.mockRejectedValue("Thalvi could not remove Thalvi from the app menu: read-only folder");
    flip(toggle);
    await settle();
    expect(toggle.checked).toBe(true);
    expect(rowOf("Show Thalvi in the app menu").querySelector(".note")?.textContent).toBe(
      "Thalvi could not remove Thalvi from the app menu: read-only folder",
    );
  });

  it("says who adds the app menu entry instead of offering a switch where Thalvi cannot", async () => {
    const stub = stubBackend();
    stub.backend.getMenuEntry.mockResolvedValue({
      kind: "unavailable",
      reason: "Added by the installer; uninstalling removes it.",
      line: null,
    });
    mountSettings(root, stub.backend);
    await settle();
    const menu = rowOf("Show Thalvi in the app menu");
    expect(menu.querySelector("input")).toBeNull();
    expect(menu.querySelector("button")).toBeNull();
    expect(menu.textContent).toContain("Added by the installer; uninstalling removes it.");
  });

  it("pauses, changes the device, saves and resumes from the Microphone page only", async () => {
    const stub = await open();
    const pageOf = (label: string): string | null => rowOf(label).closest("section")?.getAttribute("data-page") ?? null;
    expect(pageOf("Listening")).toBe("microphone");
    expect(root.querySelectorAll('.row[data-label="listening"]')).toHaveLength(1);
    expect(root.querySelectorAll('section[data-page="app"] .row[data-label="listening"]')).toHaveLength(0);

    goTo("microphone");
    const toggle = control("Listening", "input");
    const picker = control("Saved device", "select");
    expect(picker.disabled).toBe(true);
    flip(toggle);
    expect(stub.backend.setListening).toHaveBeenLastCalledWith(false);
    stub.emit({ state: "paused" });
    expect(picker.disabled).toBe(false);

    choose(picker, "USB Microphone");
    button("Save").click();
    await settle();
    expect(sent(stub).microphone).toBe("USB Microphone");
    expect(toggle.checked).toBe(false);
    expect(stub.backend.setListening).toHaveBeenCalledTimes(1);

    flip(toggle);
    expect(stub.backend.setListening).toHaveBeenLastCalledWith(true);
  });

  it("follows the status for the listening switch", async () => {
    const stub = await open();
    stub.emit({ state: "paused" });
    expect(control("Listening", "input").checked).toBe(false);
  });

  it("moves the microphone meter with the level", async () => {
    const stub = await open();
    stub.level(0.5);
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.500)");
  });

  it("lists the microphones and keeps a saved device that is gone", async () => {
    await open({ ...SETTINGS, microphone: "Old Mic" });
    const options = Array.from(control("Saved device", "select").options).map((option) => option.textContent);
    expect(options).toEqual(["System default", "USB Microphone", "Built-in", "Old Mic (not found)"]);
  });

  it("disables the saved-device picker until listening is paused, and says why beside it", async () => {
    const stub = await open();
    const why = () => rowOf("Saved device").querySelector<HTMLElement>(".note.why") as HTMLElement;
    goTo("microphone");
    expect(control("Saved device", "select").disabled).toBe(true);
    expect(why().hidden).toBe(false);
    expect(why().textContent).toBe("Pause listening above to change the microphone.");
    expect(control("Saved device", "select").getAttribute("aria-describedby")).toBe(why().id);
    stub.emit({ state: "paused" });
    expect(control("Saved device", "select").disabled).toBe(false);
    expect(why().hidden).toBe(true);
    stub.audio({ selected: null, active: "Built-in", paused: false });
    expect(control("Saved device", "select").disabled).toBe(true);
    expect(why().hidden).toBe(false);
  });

  it("shows the active microphone separately from the saved choice and stays paused after saving while paused", async () => {
    const stub = await open();
    goTo("microphone");
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("Built-in");
    stub.emit({ state: "paused" });
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("None — no microphone is open");
    choose(control("Saved device", "select"), "USB Microphone");
    button("Save").click();
    await settle();
    expect(sent(stub).microphone).toBe("USB Microphone");
    expect(control("Saved device", "select").value).toBe("USB Microphone");
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("None — no microphone is open");
    expect(control("Listening", "input").checked).toBe(false);
    expect(root.querySelector(".msg")?.textContent).toBe(SAVED_MICROPHONE_TEXT);
  });

  it("shows a failed apply plainly and clears the saved message", async () => {
    const stub = await open();
    button("Save").click();
    await settle();
    expect(root.querySelector(".msg")?.textContent).toBe(SAVED_TEXT);
    stub.emit({ state: "failed", notice: "The saved microphone could not be opened.", headline: "Failed — Settings", causes: [], kind: "other" });
    expect(root.querySelector(".msg")?.textContent).toBe("");
    expect(root.querySelector(".err")?.textContent).toBe("The saved microphone could not be opened.");
  });

  it.each<[string, (stub: Awaited<ReturnType<typeof open>>) => Promise<void>, string, string[], (stub: Awaited<ReturnType<typeof open>>) => Promise<void>, number]>([
    [
      "a runtime failure",
      (stub) => {
        stub.emit({ state: "failed", notice: "The speech server is not reachable.", headline: "Failed — Settings", causes: ["connection refused (os error 111)"], kind: "other" });
        return Promise.resolve();
      },
      "The speech server is not reachable.",
      ["connection refused (os error 111)"],
      (stub) => {
        stub.emit({ state: "starting", target: null });
        return Promise.resolve();
      },
      1,
    ],
    [
      "a rejected save",
      async (stub) => {
        stub.backend.saveSettings.mockRejectedValueOnce("The server address must start with ws://.");
        button("Save").click();
        await settle();
      },
      "The server address must start with ws://.",
      [],
      async () => {
        button("Save").click();
        await settle();
      },
      0,
    ],
  ])("keeps %s until dismissed or fixed, and copies its report", async (_name, raise, text, causes, fix, dismissals) => {
    vi.useFakeTimers();
    const stub = await open();
    const shown = (): string => root.querySelector(".err")?.textContent ?? "";

    await raise(stub);
    vi.advanceTimersByTime(60_000);
    expect(shown()).toBe(text);

    button("Copy").click();
    await settle();
    expect(stub.backend.copyReport).toHaveBeenCalledWith(text, causes);
    expect(button("Copied").hidden).toBe(false);

    button("×").click();
    await settle();
    expect(shown()).toBe("");
    expect(button("Copy").hidden).toBe(true);
    expect(stub.backend.dismissError).toHaveBeenCalledTimes(dismissals);

    await raise(stub);
    expect(shown()).toBe(text);
    await fix(stub);
    expect(shown()).toBe("");
  });

  it("says plainly when the report could not be copied and shows Copied only on success", async () => {
    const stub = await open();
    stub.backend.copyReport.mockRejectedValueOnce("No clipboard tool is installed.");
    stub.emit({ state: "failed", notice: "The speech server is not reachable.", headline: "Failed — Settings", causes: [], kind: "other" });
    button("Copy").click();
    await settle();
    expect(root.querySelector(".msg")?.textContent).toBe("Could not copy: No clipboard tool is installed.");
    expect(button("Copy").hidden).toBe(false);
    button("Copy").click();
    await settle();
    expect(button("Copied").hidden).toBe(false);
    expect(root.querySelector(".msg")?.textContent).toBe("");
  });

  it("resets the microphone meter when listening pauses", async () => {
    const stub = await open();
    goTo("microphone");
    stub.level(0.5);
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.500)");
    stub.emit({ state: "paused" });
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.000)");
    stub.level(0.6);
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.000)");
  });

  it.each([false, true])("uses audio state for listening controls when a failure occurs (paused=%s)", async (paused) => {
    const stub = await open();
    stub.audio({ selected: null, active: paused ? null : "Built-in", paused });
    stub.emit({ state: "failed", notice: "The last operation failed.", headline: "Failed — Settings", causes: [], kind: "other" });
    expect(control("Listening", "input").checked).toBe(!paused);
    expect(control("Saved device", "select").disabled).toBe(!paused);
  });

  it.each(["save", "revert", "edit"])("retains a runtime problem through %s and clears it on recovery", async (action) => {
    const stub = await open();
    stub.emit({ state: "failed", notice: "The chosen microphone is unavailable.", headline: "Failed — Settings", causes: [], kind: "other" });
    if (action === "save") button("Save").click();
    else if (action === "revert") button("Revert").click();
    else {
      goTo("capture");
      type(control("Quiet before paste", "input"), "1.5");
    }
    await settle();
    expect(root.querySelector(".err")?.textContent).toBe("The chosen microphone is unavailable.");
    expect(root.querySelector(".msg")?.textContent).toBe(action === "edit" ? "Unsaved changes" : "");
    stub.emit({ state: "listening" });
    expect(root.querySelector(".err")?.textContent).toBe("");
    if (action === "edit") expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
  });

  it("opens the recordings folder", async () => {
    const stub = await open();
    button("Open folder").click();
    expect(stub.backend.openRecordings).toHaveBeenCalledOnce();
  });

  it.each([
    ["Metrics recording", "metrics", false],
    ["Text recording", "text", true],
    ["Audio recording", "audio", true],
  ] as const)("shows %s as a switch and sends it with Save", async (label, kind, turnedOn) => {
    const stub = await open();
    const switches = ["Metrics recording", "Text recording", "Audio recording"].map(
      (name) => control(name, "input").checked,
    );
    expect(switches).toEqual([true, false, false]);
    expect(rowOf(label).textContent).toContain("Kept on this computer only");
    expect(rowOf(label).textContent).not.toContain("THALVI_RECORD");

    flip(control(label, "input"));
    button("Save").click();
    const expected = structuredClone(SETTINGS);
    expected.recordings[kind] = turnedOn;
    expect(sent(stub)).toEqual(expected);
  });

  it("shows the version", async () => {
    await open();
    expect(rowOf("Version").querySelector("output")?.textContent).toBe("1.2.3");
  });
});

describe("history", () => {
  const ENTRIES: HistoryEntry[] = [
    { id: 30, timestamp_ms: Date.UTC(2026, 10, 3, 9, 30), text: "Remind me to call.", target: "Slack", outcome: "accepted" },
    { id: 20, timestamp_ms: Date.UTC(2026, 10, 2, 18, 5), text: "The window was gone.", target: null, outcome: "failed" },
    { id: 10, timestamp_ms: Date.UTC(2026, 10, 1, 7, 0), text: "Never mind this one", target: "Firefox", outcome: "cancelled" },
  ];

  async function openHistory(...lists: (HistoryEntry[] | Error)[]) {
    const stub = stubBackend();
    for (const list of lists) {
      if (list instanceof Error) stub.backend.getHistory.mockRejectedValueOnce(list);
      else stub.backend.getHistory.mockResolvedValueOnce(structuredClone(list));
    }
    mountSettings(root, stub.backend);
    await settle();
    return stub;
  }

  const cards = (): HTMLElement[] => Array.from(root.querySelectorAll<HTMLElement>(".entry"));
  const shown = (card: HTMLElement) => ({
    time: card.querySelector("time")?.getAttribute("datetime"),
    target: card.querySelector(".target")?.textContent ?? null,
    outcome: card.querySelector(".outcome")?.textContent,
    text: card.querySelector("p")?.textContent,
  });

  it("lists the entries newest first with their time, target, outcome and text", async () => {
    await openHistory(ENTRIES);
    expect(cards().map(shown)).toEqual([
      { time: "2026-11-03T09:30:00.000Z", target: "Slack", outcome: "Delivered", text: "Remind me to call." },
      { time: "2026-11-02T18:05:00.000Z", target: null, outcome: "Failed", text: "The window was gone." },
      { time: "2026-11-01T07:00:00.000Z", target: "Firefox", outcome: "Cancelled", text: "Never mind this one" },
    ]);
    expect(cards().every((card) => (card.querySelector("time")?.textContent ?? "") !== "")).toBe(true);
  });

  it("says so when nothing has been kept, and reads the list again whenever the page is opened", async () => {
    const stub = await openHistory([], ENTRIES);
    expect(root.querySelector(".entries")?.textContent).toBe("Nothing has been kept yet.");
    expect(stub.backend.getHistory).toHaveBeenCalledTimes(1);
    goTo("history");
    await settle();
    expect(cards()).toHaveLength(3);
  });

  it("deletes one entry by id and shows the list without it", async () => {
    const stub = await openHistory(ENTRIES, [ENTRIES[0] as HistoryEntry, ENTRIES[2] as HistoryEntry]);
    within(cards()[1] as HTMLElement, "Delete").click();
    await settle();
    expect(stub.backend.deleteHistoryEntry).toHaveBeenCalledExactlyOnceWith(20);
    expect(cards().map((card) => shown(card).text)).toEqual(["Remind me to call.", "Never mind this one"]);
  });

  it("keeps the list and says why when an entry cannot be deleted", async () => {
    const stub = await openHistory(ENTRIES);
    stub.backend.deleteHistoryEntry.mockRejectedValueOnce("The history could not be written.");
    within(cards()[0] as HTMLElement, "Delete").click();
    await settle();
    expect(cards()).toHaveLength(3);
    expect(rowOf("Recent dictations").querySelector(".note")?.textContent).toBe("The history could not be written.");
  });

  it("asks before clearing: keeping changes nothing, and confirming clears at once", async () => {
    const stub = await openHistory(ENTRIES, []);
    const group = (): HTMLElement => rowOf("Clear history");
    expect(group().querySelector(".trow")?.hasAttribute("hidden")).toBe(true);

    within(group(), "Clear history").click();
    expect(group().querySelector(".trow")?.hasAttribute("hidden")).toBe(false);
    expect(group().textContent).toContain("Delete every entry? This cannot be undone.");
    expect(within(group(), "Clear history").hidden).toBe(true);
    within(group(), "Keep history").click();
    await settle();
    expect(stub.backend.clearHistory).not.toHaveBeenCalled();
    expect(cards()).toHaveLength(3);
    expect(within(group(), "Clear history").hidden).toBe(false);

    within(group(), "Clear history").click();
    within(group(), "Yes, clear history").click();
    await settle();
    expect(stub.backend.clearHistory).toHaveBeenCalledOnce();
    expect(cards()).toHaveLength(0);
    expect(root.querySelector(".entries")?.textContent).toBe("Nothing has been kept yet.");
    expect(group().querySelector(".trow")?.hasAttribute("hidden")).toBe(true);
  });

  it("copies exactly one entry's text and says so on that row, or says why it could not", async () => {
    const stub = await openHistory(ENTRIES);
    const rows = cards();
    within(rows[1] as HTMLElement, "Copy").click();
    await settle();
    expect(stub.backend.copyText).toHaveBeenCalledExactlyOnceWith("The window was gone.");
    expect(rows.map((card) => card.querySelector(".note")?.textContent ?? "")).toEqual(["", "Copied to the clipboard.", ""]);
    expect(within(rows[1] as HTMLElement, "Copy").getAttribute("aria-label")).toMatch(/^Copy the entry from /);

    stub.backend.copyText.mockRejectedValueOnce("The clipboard is locked.");
    within(rows[0] as HTMLElement, "Copy").click();
    await settle();
    expect(rows.map((card) => card.querySelector(".note")?.textContent ?? "")).toEqual(["The clipboard is locked.", "Copied to the clipboard.", ""]);
  });

  it("shows why the history cannot be read and lets the user start a new one", async () => {
    const damaged = "The history file /home/me/history.json is damaged (EOF). Clear the history to start a new one.";
    const stub = await openHistory(new Error(damaged), [ENTRIES[0] as HistoryEntry]);
    expect(rowOf("Recent dictations").querySelector(".note")?.textContent).toBe(damaged);
    expect(cards()).toHaveLength(0);
    expect(root.querySelector(".entries")?.textContent).toBe("");

    within(rowOf("Clear history"), "Clear history").click();
    within(rowOf("Clear history"), "Yes, clear history").click();
    await settle();
    expect(stub.backend.clearHistory).toHaveBeenCalledOnce();
    expect(rowOf("Recent dictations").querySelector(".note")?.hasAttribute("hidden")).toBe(true);
    expect(cards()).toHaveLength(1);
  });

  it("saves the switch and the limit with Save, and says that turning it off keeps what is there", async () => {
    const stub = await open();
    expect(control("Keep a history", "input").checked).toBe(true);
    expect(control("Entries to keep", "input").value).toBe("200");
    expect(rowOf("Keep a history").textContent).toContain("Kept on this computer only, never uploaded");
    expect(rowOf("Keep a history").textContent).toContain("deletes none");

    flip(control("Keep a history", "input"));
    type(control("Entries to keep", "input"), "50");
    button("Save").click();
    const expected = structuredClone(SETTINGS);
    expected.history = { enabled: false, limit: 50 };
    expect(sent(stub)).toEqual(expected);
    expect(stub.backend.clearHistory).not.toHaveBeenCalled();
  });
});

describe("last dictation", () => {
  const DRAFT: Draft = {
    id: 7,
    original: "Remind me to call the dentist tomorow.",
    text: "Remind me to call the dentist tomorow.",
    target: "foot",
    outcome: "failed",
    newerWaiting: false,
  };

  async function openDraft(draft: Draft | null = DRAFT) {
    const stub = stubBackend();
    stub.backend.getDraft.mockImplementation(() => Promise.resolve(draft === null ? null : structuredClone(draft)));
    mountSettings(root, stub.backend);
    await settle();
    return stub;
  }

  const area = (): HTMLTextAreaElement => rowOf("Last dictation").querySelector("textarea") as HTMLTextAreaElement;
  const section = (): HTMLElement => rowOf("Last dictation");
  const buttons = (): string[] => Array.from(section().querySelectorAll("button")).map((candidate) => candidate.textContent);
  const notes = (): string[] => Array.from(section().querySelectorAll<HTMLElement>(".note")).filter(shownInSection).map((node) => node.textContent);

  function shownInSection(node: HTMLElement): boolean {
    for (let at: HTMLElement | null = node; at !== null && at !== section().parentElement; at = at.parentElement) if (at.hidden) return false;
    return true;
  }

  function edit(value: string): void {
    area().value = value;
    area().dispatchEvent(new Event("input", { bubbles: true }));
  }

  it("says there is nothing to recover when there is no draft", async () => {
    await openDraft(null);
    expect(notes()).toEqual(["Nothing to recover yet. The last dictation appears here, also when it failed or you cancelled it."]);
    expect(section().querySelector<HTMLElement>(".stack > .stack")?.hidden).toBe(true);
  });

  it("shows the draft with how it ended and the window it was meant for", async () => {
    for (const [outcome, sentence] of [
      ["delivered", "Delivered. Meant for foot."],
      ["cancelled", "You cancelled this one, so nothing was pasted. Meant for foot."],
      ["failed", "This was not delivered. Meant for foot."],
      ["maybePartial", "Thalvi may or may not have pasted this text. Check the window before trying again, or use Copy text. Meant for foot."],
    ] as const) {
      document.body.innerHTML = '<div id="settings"></div>';
      root = document.getElementById("settings") as HTMLElement;
      await openDraft({ ...DRAFT, outcome });
      expect(area().value, outcome).toBe(DRAFT.text);
      expect(notes()[0], outcome).toBe(sentence);
      expect(buttons(), outcome).toEqual(["Copy text", "Reset to original", "Discard"]);
    }
  });

  it("keeps an edit without recognizing, copying or sending anything, and can reset it", async () => {
    const stub = await openDraft();
    expect(within(section(), "Reset to original").disabled).toBe(true);
    edit("Remind me to call the dentist tomorrow.");
    expect(stub.backend.editDraft).toHaveBeenLastCalledWith("Remind me to call the dentist tomorrow.");
    expect(within(section(), "Reset to original").disabled).toBe(false);
    within(section(), "Reset to original").click();
    expect(area().value).toBe(DRAFT.original);
    expect(stub.backend.editDraft).toHaveBeenLastCalledWith(DRAFT.original);
    expect(stub.backend.copyText).not.toHaveBeenCalled();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
  });

  it("offers no Copy for an empty edit", async () => {
    await openDraft();
    for (const blank of ["", "   "]) {
      edit(blank);
      expect(within(section(), "Copy text").disabled, blank).toBe(true);
    }
    edit("back");
    expect(within(section(), "Copy text").disabled).toBe(false);
  });

  it("copies the edited text, and says why when it could not", async () => {
    const stub = await openDraft();
    edit("fixed text");
    within(section(), "Copy text").click();
    await settle();
    expect(stub.backend.copyText).toHaveBeenCalledExactlyOnceWith("fixed text");
    expect(notes()).toContain("Copied to the clipboard.");

    stub.backend.copyText.mockRejectedValueOnce("The clipboard is locked.");
    within(section(), "Copy text").click();
    await settle();
    expect(notes()).toContain("The clipboard is locked.");
    expect(notes()).not.toContain("Copied to the clipboard.");
    expect(area().value).toBe("fixed text");
  });

  it("checks the draft again when a capture ends, not while it runs", async () => {
    const stub = await openDraft();
    const calls = stub.backend.getDraft.mock.calls.length;
    for (const state of [
      { state: "capturing", target: "foot", committed: "", partial: null, notice: null },
      { state: "working", target: "foot", route: "paste", cancellable: false },
    ] as const) stub.emit(state);
    await settle();
    expect(stub.backend.getDraft.mock.calls.length).toBe(calls);
    stub.emit({ state: "listening" });
    await settle();
    expect(stub.backend.getDraft.mock.calls.length).toBe(calls + 1);
  });

  it("shows a new dictation, but keeps the edits and says a newer one waits when there are unsent edits", async () => {
    const stub = await openDraft();
    stub.backend.getDraft.mockImplementation(() => Promise.resolve({ ...DRAFT, id: 8, original: "A newer one.", text: "A newer one." }));
    stub.emit({ state: "listening" });
    await settle();
    expect(area().value).toBe("A newer one.");

    edit("A newer one, fixed.");
    stub.backend.getDraft.mockImplementation(() => Promise.resolve({ ...DRAFT, id: 8, original: "A newer one.", text: "A newer one, fixed.", newerWaiting: true }));
    stub.emit({ state: "listening" });
    await settle();
    expect(area().value).toBe("A newer one, fixed.");
    expect(notes()).toContain("A newer dictation is waiting. Discard this one to see it.");
  });

  it("ignores a draft answer that a newer refresh and a later edit superseded", async () => {
    const stub = await openDraft();
    let answerOlder: (draft: Draft | null) => void = () => undefined;
    let answerNewer: (draft: Draft | null) => void = () => undefined;
    stub.backend.getDraft
      .mockImplementationOnce(() => new Promise<Draft | null>((resolve) => { answerOlder = resolve; }))
      .mockImplementationOnce(() => new Promise<Draft | null>((resolve) => { answerNewer = resolve; }));
    stub.emit({ state: "listening" });
    stub.emit({ state: "listening" });
    answerNewer({ ...DRAFT, id: 8, original: "A newer one.", text: "A newer one." });
    await settle();
    expect(area().value).toBe("A newer one.");

    edit("A newer one, fixed.");
    answerOlder(structuredClone(DRAFT));
    await settle();
    expect(area().value).toBe("A newer one, fixed.");
    expect(stub.backend.editDraft).toHaveBeenLastCalledWith("A newer one, fixed.");
  });

  it("keeps Reset when an earlier draft refresh arrives afterward", async () => {
    const stub = await openDraft();
    edit("Typed edit.");
    let answerOlder: (draft: Draft | null) => void = () => undefined;
    stub.backend.getDraft.mockImplementationOnce(() => new Promise<Draft | null>((resolve) => { answerOlder = resolve; }));
    stub.emit({ state: "listening" });
    within(section(), "Reset to original").click();
    answerOlder({ ...DRAFT, id: 6, original: "An older draft.", text: "An older draft." });
    await settle();
    expect(area().value).toBe(DRAFT.original);
    expect(stub.backend.editDraft).toHaveBeenLastCalledWith(DRAFT.original);
  });

  it("discards the draft and shows the one that waited", async () => {
    const stub = await openDraft({ ...DRAFT, newerWaiting: true });
    stub.backend.getDraft.mockResolvedValueOnce({ ...DRAFT, id: 9, original: "Waiting.", text: "Waiting." });
    within(section(), "Discard").click();
    await settle();
    expect(stub.backend.discardDraft).toHaveBeenCalledOnce();
    expect(area().value).toBe("Waiting.");
    expect(notes()).not.toContain("A newer dictation is waiting. Discard this one to see it.");

    stub.backend.getDraft.mockResolvedValueOnce(null);
    within(section(), "Discard").click();
    await settle();
    expect(notes()[0]).toContain("Nothing to recover yet.");
  });

  it("opens on the History page when the tray asks for it", async () => {
    const stub = await openDraft();
    expect(root.querySelector<HTMLElement>('section[data-page="history"]')?.hidden).toBe(true);
    stub.openPage("history");
    expect(root.querySelector<HTMLElement>('section[data-page="history"]')?.hidden).toBe(false);
    stub.openPage("nowhere");
    expect(root.querySelector<HTMLElement>('section[data-page="history"]')?.hidden).toBe(false);
  });
});

describe("footer", () => {
  // jsdom does not load the app's stylesheet, and what shows depends on it.
  beforeEach(() => {
    document.head.appendChild(document.createElement("style")).textContent = settingsCss;
  });
  afterEach(() => {
    document.head.querySelector("style")?.remove();
  });

  it.each(["unsaved edit", "change from elsewhere", "save in flight"] as const)("shows %s next to a runtime error instead of hiding it", async (state) => {
    const stub = await open();
    stub.emit({ state: "failed", notice: "Could not download the wake model.", headline: "Failed — Settings", causes: [], kind: "other" });
    if (state !== "save in flight") {
      goTo("capture");
      type(control("Quiet before paste", "input"), "1.5");
    }
    if (state === "change from elsewhere") {
      stub.changedElsewhere();
    } else if (state === "save in flight") {
      stub.backend.saveSettings.mockImplementationOnce(() => new Promise<void>(() => undefined));
      button("Save").click();
    }
    await settle();
    const message = root.querySelector<HTMLElement>(".msg") as HTMLElement;
    const error = root.querySelector<HTMLElement>(".err") as HTMLElement;
    expect(message.textContent).toBe({ "unsaved edit": "Unsaved changes", "change from elsewhere": ELSEWHERE_TEXT, "save in flight": "Saving…" }[state]);
    expect(error.textContent).toBe("Could not download the wake model.");
    expect(getComputedStyle(message).display).not.toBe("none");
    expect(getComputedStyle(error).display).not.toBe("none");
  });

  it("leaves the whole bar to an error when there is no status text", async () => {
    const stub = await open();
    stub.emit({ state: "failed", notice: "Could not download the wake model.", headline: "Failed — Settings", causes: [], kind: "other" });
    expect(getComputedStyle(root.querySelector(".msg") as HTMLElement).display).toBe("none");
  });
});
