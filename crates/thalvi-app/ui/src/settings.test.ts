import { beforeEach, describe, expect, it } from "vitest";
import type { Settings } from "./contract.ts";
import { mountSettings, SAVED_TEXT } from "./settings.ts";
import { SETTINGS, settle, stubBackend } from "./testing.ts";

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
    choose(control("Input device", "select"), "USB Microphone");
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
    expect(control("Speech model", "input").value).toBe("nemotron_160ms");
  });
});

describe("second transcription", () => {
  it("is saved as an object when filled and as null when cleared", async () => {
    const stub = await open();
    goTo("recognition");
    type(control("Endpoint address", "input"), "http://127.0.0.1:9000/v1");
    type(control("Model", "input"), "whisper");
    type(control("API key", "input"), "k");
    choose(control("API", "select"), "meta");
    button("Save").click();
    expect(sent(stub).stt.secondary).toEqual({ url: "http://127.0.0.1:9000/v1", model: "whisper", api: "meta", api_key: "k" });
    expect(control("API key", "input").type).toBe("password");

    await settle();
    for (const label of ["Endpoint address", "Model", "API key"]) type(control(label, "input"), "");
    button("Save").click();
    expect(stub.backend.saveSettings.mock.calls[1]?.[0].stt.secondary).toBeNull();
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
    const options = Array.from(control("Input device", "select").options).map((option) => option.textContent);
    expect(options).toEqual(["System default", "USB Microphone", "Built-in", "Old Mic (not found)"]);
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
