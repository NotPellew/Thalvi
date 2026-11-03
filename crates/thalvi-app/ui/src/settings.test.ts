import { beforeEach, describe, expect, it } from "vitest";
import type { Settings } from "./contract.ts";
import { mountSettings, SAVED_TEXT } from "./settings.ts";
import { NEMOTRON, SETTINGS, settle, stubBackend } from "./testing.ts";

let root: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '<div id="settings"></div>';
  root = document.getElementById("settings") as HTMLElement;
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

function radio(title: string): HTMLButtonElement {
  const found = Array.from(root.querySelectorAll<HTMLButtonElement>("[role=radio]")).find((candidate) => candidate.textContent === title);
  if (!found) throw new Error(`no model ${title}`);
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

describe("save", () => {
  it("sends the settings unchanged when nothing was edited", async () => {
    const stub = await open();
    button("Save").click();
    expect(sent(stub)).toEqual(SETTINGS);
    await settle();
    expect(root.querySelector(".msg")?.textContent).toBe(SAVED_TEXT);
  });

  it("sends exactly the edits", async () => {
    const stub = await open();
    goTo("general");
    type(control("Server address", "input"), "wss://thalvi.example");
    goTo("wake");
    type(control("Quiet before paste", "input"), "1.5");
    type(control("Threshold (dictation)", "input"), "0.8");
    type(control("Wake model (conversation)", "input"), "hey_jarvis");
    goTo("overlay");
    flip(control("Show the draft", "input"));
    goTo("sounds");
    flip(control("Wake sound", "input"));
    goTo("limits");
    type(control("Stop after", "input"), "120");
    goTo("microphone");
    stub.emit({ state: "paused" });
    choose(control("Saved device", "select"), "USB Microphone");
    button("Save").click();
    const expected = structuredClone(SETTINGS);
    expected.server = "wss://thalvi.example";
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
    type(control("Server address", "input"), "ws://other");
    expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
    button("Revert").click();
    await settle();
    expect(stub.backend.getSettings).toHaveBeenCalledTimes(2);
    expect(control("Server address", "input").value).toBe(SETTINGS.server);
    expect(root.querySelector(".msg")?.textContent).toBe("");
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
      stub.emit({ state: "failed", notice: "The new settings could not be applied." });
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
    goTo("wake");
    const add = rowOf("Ending phrases");
    type(add.querySelector<HTMLInputElement>(".add input") as HTMLInputElement, "  Send   It ");
    choose(add.querySelector<HTMLSelectElement>(".add select") as HTMLSelectElement, "submit");
    (add.querySelector(".add button") as HTMLElement).click();
    root.querySelector<HTMLElement>('button[aria-label="Remove “never mind”"]')?.click();
    button("Save").click();
    const expected = structuredClone(SETTINGS);
    expected.endings = { "paste that": "paste", submit: "submit", "do that": "task", "scratch that": "scratch", "send it": "submit" };
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

describe("a custom speech model folder", () => {
  async function openLocal() {
    const stub = await open({ ...SETTINGS, stt: { engine: "local", secondary: null } });
    goTo("recognition");
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

  it("asks for a path instead of checking nothing or the built-in model", async () => {
    const stub = await openLocal();
    button("Check folder").click();
    expect(result()?.textContent).toBe("Type the folder path first.");
    type(control("Speech model", "input"), "nemotron_160ms");
    button("Check folder").click();
    expect(result()?.textContent).toBe("nemotron_160ms is downloaded on first use; there is no folder to check.");
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
  });

  it("saves a valid folder as stt.model", async () => {
    const stub = await openLocal();
    type(control("Speech model", "input"), "/models/mine");
    button("Save").click();
    await settle();
    expect(stub.backend.validateSpeechModelFolder).toHaveBeenCalledWith("/models/mine");
    expect(sent(stub).stt).toEqual({ engine: "local", model: "/models/mine", secondary: null });
    expect(result()?.textContent).toBe("This folder holds a speech model.");
  });

  it("does not save a folder that fails the check, and says why", async () => {
    const stub = await openLocal();
    stub.backend.validateSpeechModelFolder.mockRejectedValueOnce("There is no folder at /models/gone.");
    type(control("Speech model", "input"), "/models/gone");
    button("Save").click();
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    expect(result()?.textContent).toBe("There is no folder at /models/gone.");
    expect(root.querySelector(".err")?.textContent).toBe("There is no folder at /models/gone.");
  });

  it.each([
    ["the built-in model", { engine: "local", model: "nemotron_160ms" }],
    ["no model", { engine: "local" }],
    ["the server engine", { engine: "server", model: "/models/mine" }],
  ] as const)("saves without checking a folder for %s", async (_name, stt) => {
    const stub = await open({ ...SETTINGS, stt: { ...stt, secondary: null } });
    goTo("recognition");
    button("Save").click();
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
  });
});

describe("where speech is recognized", () => {
  it("saves the engine, and the model only for the local engine", async () => {
    const stub = await open();
    goTo("recognition");
    expect(control("Recognize speech", "select").value).toBe("server");
    expect(Array.from(control("Recognize speech", "select").options).map((option) => option.textContent)).toEqual([
      "On this computer",
      "Through the server",
    ]);
    type(control("Speech model", "input"), "/models/mine");
    choose(control("Recognize speech", "select"), "local");
    button("Save").click();
    await settle();
    expect(sent(stub).stt.engine).toBe("local");
    expect(sent(stub).stt.model).toBe("/models/mine");

    await settle();
    type(control("Speech model", "input"), "  ");
    button("Save").click();
    expect(stub.backend.saveSettings.mock.calls[1]?.[0].stt).toEqual({ engine: "local", secondary: null });

    await settle();
    type(control("Speech model", "input"), "nemotron_160ms");
    choose(control("Recognize speech", "select"), "server");
    button("Save").click();
    expect(stub.backend.saveSettings.mock.calls[2]?.[0].stt).toEqual({ engine: "server", secondary: null });
  });

  it("shows the saved local model", async () => {
    await open({ ...SETTINGS, stt: { engine: "local", model: "nemotron_160ms", secondary: null } });
    goTo("recognition");
    expect(control("Recognize speech", "select").value).toBe("local");
    expect(radio("Nemotron Speech Streaming 0.6B").getAttribute("aria-checked")).toBe("true");
    expect(control("Speech model", "input").value).toBe("");
  });
});

describe("the speech model catalog", () => {
  async function openCatalog(settings: Settings = { ...SETTINGS, stt: { engine: "local", secondary: null } }) {
    const stub = await open(settings);
    goTo("recognition");
    return stub;
  }
  const card = (): HTMLElement => {
    const found = rowOf("Speech model").querySelector<HTMLElement>(".model");
    if (!found) throw new Error("no model card");
    return found;
  };
  const text = (selector: string): string | undefined => card().querySelector(selector)?.textContent ?? undefined;
  const alert = (): string | undefined => card().querySelector(".note:not([hidden])")?.textContent ?? undefined;
  const download = (): HTMLButtonElement => within(card(), "Download");
  const meter = (): HTMLProgressElement => card().querySelector("progress") as HTMLProgressElement;

  it("lists the model with its description, size, state and source", async () => {
    await openCatalog();
    expect(rowOf("Speech model").querySelectorAll(".model")).toHaveLength(1);
    expect(card().textContent).toContain("NVIDIA's English streaming recognizer");
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
    const stub = stubBackend({ ...SETTINGS, stt: { engine: "local", secondary: null } });
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...NEMOTRON, size }]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    expect(text(".unit")).toBe(shown);
  });

  it("marks a downloaded model and offers no download", async () => {
    const stub = stubBackend({ ...SETTINGS, stt: { engine: "local", secondary: null } });
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...NEMOTRON, cached: true }]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
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
    const stub = stubBackend({ ...SETTINGS, stt: { engine: "local", secondary: null } });
    stub.backend.listSpeechModels.mockRejectedValueOnce("Thalvi could not find a folder to keep its models in.");
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    expect(rowOf("Speech model").querySelector(".note:not([hidden])")?.textContent).toBe(
      "Thalvi could not find a folder to keep its models in.",
    );
  });

  it("saves the selected model as stt.model and leaves the own-folder box empty", async () => {
    const stub = await openCatalog();
    type(control("Speech model", "input"), "/models/mine");
    expect(radio("Nemotron Speech Streaming 0.6B").getAttribute("aria-checked")).toBe("false");

    radio("Nemotron Speech Streaming 0.6B").click();
    expect(radio("Nemotron Speech Streaming 0.6B").getAttribute("aria-checked")).toBe("true");
    expect(control("Speech model", "input").value).toBe("");
    button("Save").click();
    await settle();
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
    expect(sent(stub).stt).toEqual({ engine: "local", model: "nemotron_160ms", secondary: null });
  });

  it("selects the default model when none is saved", async () => {
    await openCatalog();
    expect(radio("Nemotron Speech Streaming 0.6B").getAttribute("aria-checked")).toBe("true");
  });
});

describe("second transcription", () => {
  it("stays off until the switch is flipped, and keeps its fields when flipped off", async () => {
    const stub = await open();
    goTo("recognition");
    expect(control("Second-pass transcription", "input").checked).toBe(false);
    expect(control("Endpoint address", "input").disabled).toBe(true);
    flip(control("Second-pass transcription", "input"));
    expect(control("Endpoint address", "input").disabled).toBe(false);
    type(control("Endpoint address", "input"), "http://127.0.0.1:9000/v1");
    type(control("Model", "input"), "whisper");
    type(control("API key", "input"), "k");
    choose(control("API", "select"), "meta");
    button("Save").click();
    expect(sent(stub).stt.secondary).toEqual({ enabled: true, url: "http://127.0.0.1:9000/v1", model: "whisper", api: "meta", api_key: "k" });
    expect(control("API key", "input").type).toBe("password");

    await settle();
    flip(control("Second-pass transcription", "input"));
    expect(control("Endpoint address", "input").value).toBe("http://127.0.0.1:9000/v1");
    expect(control("Endpoint address", "input").disabled).toBe(true);
    button("Save").click();
    expect(stub.backend.saveSettings.mock.calls[1]?.[0].stt.secondary).toEqual({
      enabled: false,
      url: "http://127.0.0.1:9000/v1",
      model: "whisper",
      api: "meta",
      api_key: "k",
    });
  });

  it("shows a saved off endpoint with the switch off and its fields kept", async () => {
    await open({
      ...SETTINGS,
      stt: { engine: "server", secondary: { enabled: false, url: "http://127.0.0.1:9000/v1", model: "whisper", api: "meta", api_key: "k" } },
    });
    goTo("recognition");
    expect(control("Second-pass transcription", "input").checked).toBe(false);
    expect(control("Endpoint address", "input").value).toBe("http://127.0.0.1:9000/v1");
    expect(control("Model", "input").value).toBe("whisper");
  });
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
    expect(root.querySelector<HTMLElement>("section[data-page=general]")?.hidden).toBe(false);
    expect(root.querySelector<HTMLElement>("section[data-page=wake]")?.hidden).toBe(true);
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

  it("disables the saved-device picker until listening is paused", async () => {
    const stub = await open();
    goTo("microphone");
    expect(control("Saved device", "select").disabled).toBe(true);
    stub.emit({ state: "paused" });
    expect(control("Saved device", "select").disabled).toBe(false);
    stub.audio({ selected: null, active: "Built-in", paused: false });
    expect(control("Saved device", "select").disabled).toBe(true);
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
    expect(root.querySelector(".msg")?.textContent).toBe(SAVED_TEXT);
  });

  it("shows a failed apply plainly and clears the saved message", async () => {
    const stub = await open();
    button("Save").click();
    await settle();
    expect(root.querySelector(".msg")?.textContent).toBe(SAVED_TEXT);
    stub.emit({ state: "failed", notice: "The saved microphone could not be opened." });
    expect(root.querySelector(".msg")?.textContent).toBe("");
    expect(root.querySelector(".err")?.textContent).toBe("The saved microphone could not be opened.");
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
    stub.emit({ state: "failed", notice: "The last operation failed." });
    expect(control("Listening", "input").checked).toBe(!paused);
    expect(control("Saved device", "select").disabled).toBe(!paused);
  });

  it.each(["save", "revert", "edit"])("retains a runtime problem through %s and clears it on recovery", async (action) => {
    const stub = await open();
    stub.emit({ state: "failed", notice: "The chosen microphone is unavailable." });
    if (action === "save") button("Save").click();
    else if (action === "revert") button("Revert").click();
    else type(control("Server address", "input"), "ws://other");
    await settle();
    expect(root.querySelector(".err")?.textContent).toBe("The chosen microphone is unavailable.");
    expect(root.querySelector(".msg")?.textContent).toBe(action === "edit" ? "Unsaved changes" : "");
    stub.emit({ state: "listening" });
    expect(root.querySelector(".err")?.textContent).toBe("");
    if (action === "edit") expect(root.querySelector(".msg")?.textContent).toBe("Unsaved changes");
  });

  it("shows the recordings kinds and opens the folder", async () => {
    const stub = await open();
    expect(rowOf("Metrics recording").querySelector("output")?.textContent).toBe("On");
    expect(rowOf("Text recording").querySelector("output")?.textContent).toBe("Off");
    button("Open folder").click();
    expect(stub.backend.openRecordings).toHaveBeenCalledOnce();
  });

  it("shows the version", async () => {
    await open();
    expect(rowOf("Version").querySelector("output")?.textContent).toBe("1.2.3");
  });
});
