import fc from "fast-check";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Draft, HistoryEntry, OnDeviceEntry, Settings, SpeechEntry, Status, UpdateState } from "./contract.ts";
import { DownloadFailure } from "./contract.ts";
import settingsCss from "./settings.css?raw";
import settingsLooksCss from "./settings-looks.css?raw";
import { disclosureMemory } from "./dom.ts";
import { fakeBackend } from "./fake.ts";
import { settingsPatch } from "./patch.ts";
import { HINT_BUDGET, shownNumber } from "./settings-pages.ts";
import { APPLIED_LATER_TEXT, APPLIED_TEXT, mountSettings } from "./settings.ts";
import { NEMOTRON, SETTINGS, settle, stubBackend, UNIFIED, WAKE_MODELS_INFO, WHISPER } from "./testing.ts";

let root: HTMLElement;

beforeEach(() => {
  disclosureMemory().clear();
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

/** Types a value and leaves the field, which is what applies a typed field. */
function type(input: HTMLInputElement | HTMLTextAreaElement, value: string): void {
  typeIn(input, value);
  input.dispatchEvent(new Event("change", { bubbles: true }));
}

/** Types a value and stays in the field. */
function typeIn(input: HTMLInputElement | HTMLTextAreaElement, value: string): void {
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

/** Presses `action` inside `parent`, then the yes of the question it asks. */
function confirmIn(parent: HTMLElement, action: string, yes: string): void {
  within(parent, action).click();
  within(parent, yes).click();
}

function goTo(page: string): void {
  (root.querySelector(`nav [data-page="${page}"]`) as HTMLElement).click();
}

/** The settings of the latest call to `saveSettings`: every change is applied on top of the ones before. */
function sent(stub: Awaited<ReturnType<typeof open>>): Settings {
  const latest = stub.applied().at(-1);
  if (!latest) throw new Error("nothing was applied");
  return latest;
}

const SERVER: SpeechEntry = { kind: "realtime", id: "direct", name: "Realtime speech server", active: true, address: "ws://127.0.0.1:8012", model: "nemotron" };

function device(model?: string, active = true): OnDeviceEntry {
  return { kind: "on_device", id: "on-device", name: "On this computer", active, ...(model ? { model } : {}) };
}

/** Settings whose live entry in use is the on-device model, beside a realtime speech server entry not in use. */
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

function liveSelect(): HTMLSelectElement {
  return control("Live recognizer", "select");
}

/** Picks a live recognizer in the drop-down by the entry's name. */
function use(name: string): void {
  const option = Array.from(liveSelect().options).find((candidate) => candidate.textContent === name || candidate.textContent.startsWith(`${name} (`));
  if (!option) throw new Error(`no live choice ${name}`);
  choose(liveSelect(), option.value);
}

const ADD: Record<SpeechEntry["kind"], string> = {
  realtime: "Add a streaming server",
  open_ai: "Add an after-you-stop server",
  on_device: "Add an on-device model",
  extra: "Add an extra-only server",
};

function plus(kind: SpeechEntry["kind"]): HTMLButtonElement {
  const found = root.querySelector<HTMLButtonElement>(`button[aria-label="${ADD[kind]}"]`);
  if (!found) throw new Error(`no + for ${kind}`);
  return found;
}

async function addServer(kind: SpeechEntry["kind"]): Promise<void> {
  plus(kind).click();
  await settle();
}

async function saveServer(): Promise<void> {
  within(root.querySelector(".editor") as HTMLElement, "Save").click();
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
          group.querySelector("h4")?.textContent ?? group.querySelector(".row .what > :first-child")?.textContent ?? "",
          Array.from(group.querySelectorAll<HTMLElement>(".row")).map((row) => row.dataset["label"]),
        ]),
      ),
    }));
    expect(Array.from(root.querySelectorAll("nav [data-page]")).map((item) => item.getAttribute("data-page"))).toEqual(
      shape.map((page) => page.id),
    );
    expect(shape).toEqual([
      {
        id: "listening",
        title: "Listening",
        groups: {
          "Right now": ["listening", "active microphone", "level"],
          Device: ["microphone"],
          "Wake words": ["wake words"],
          "Dictation wake word": ["wake model (dictation)", "threshold (dictation)"],
          "Hold keys": ["dictate key", "write key"],
          "Capture limits": ["stop after silence", "stop after"],
        },
      },
      {
        id: "recognition",
        title: "Recognition",
        groups: {
          "Live recognizer": ["live recognizer"],
          "Streaming servers": ["streaming servers · speech servers"],
          "After-you-stop servers": ["after-you-stop servers · speech servers"],
          "On this computer": ["on this computer"],
          "Extra-only servers": ["extra-only servers · extra recognizers · speech servers"],
          "Word list": ["word list"],
        },
      },
      {
        id: "commands",
        title: "Commands",
        groups: {
          Endings: ["ending phrases"],
          "Quiet before acting": ["quiet before paste", "quiet before paste and enter", "quiet before running a task"],
          Apps: ["apps"],
          Scripts: ["scripts"],
        },
      },
      {
        id: "chat",
        title: "Chat model",
        groups: {
          Writing: ["chat model source", "local chat runtime", "local chat model", "writing endpoint address", "writing model", "writing api key", "clean up dictation", "writing prompt", "opening phrases", "app styles"],
        },
      },
      {
        id: "overlay",
        title: "Overlay & sounds",
        groups: {
          Overlay: ["show the overlay", "show the draft"],
          Look: ["look"],
          Sounds: ["wake sound", "ending heard sound", "cancelled sound", "accepted sound", "error sound"],
        },
      },
      {
        id: "history",
        title: "History & recordings",
        groups: {
          "Last dictation": ["last dictation"],
          Keeping: ["keep a history", "entries to keep"],
          "Kept so far": ["recent dictations", "clear history"],
          Recordings: ["metrics recording", "text recording", "audio recording"],
          Folder: ["recordings folder"],
        },
      },
      {
        id: "app",
        title: "App",
        groups: {
          Startup: ["start thalvi when i log in", "show thalvi in the app menu"],
          Updates: ["update channel", "check for updates"],
          About: ["version"],
        },
      },
    ]);
    const labels = Array.from(root.querySelectorAll<HTMLElement>(".row")).map((row) => row.dataset["label"]);
    expect(labels.filter((label, at) => labels.indexOf(label) !== at)).toEqual([]);
  });
});

describe("applying a change", () => {
  const rowText = (label: string): string => rowOf(label).querySelector(".applied")?.textContent ?? "";
  const reasonOf = (label: string): string => rowOf(label).querySelector<HTMLElement>(".note:not([hidden])")?.textContent ?? "";

  const leave = (input: HTMLElement): void => {
    input.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const nothing = (): void => undefined;
  const script1Label = (): HTMLInputElement => root.querySelector<HTMLInputElement>('input[aria-label="Label of script 1"]') as HTMLInputElement;

  // (control kind, page, what the user does short of the trigger, the trigger, the applied value read back, what it is)
  const triggers: [string, string, () => void, () => void, (settings: Settings) => unknown, unknown][] = [
    ["a number, when it loses focus", "commands", () => { typeIn(control("Quiet before paste", "input"), "1.5"); }, () => { leave(control("Quiet before paste", "input")); }, (s) => s.ending_pause.paste, 1.5],
    ["a slider, when it is let go", "listening", () => { typeIn(control("Threshold (dictation)", "input"), "0.8"); }, () => { leave(control("Threshold (dictation)", "input")); }, (s) => s.wake.capture.threshold, 0.8],
    ["a typed field, when it loses focus", "commands", () => { typeIn(script1Label(), "new label"); }, () => { leave(script1Label()); }, (s) => s.scripts[0]?.label, "new label"],
    ["a switch, as it is flipped", "overlay", nothing, () => { flip(control("Show the draft", "input")); }, (s) => s.overlay.draft, false],
    ["a choice, as it is made", "app", nothing, () => { choose(control("Update channel", "select"), "development"); }, (s) => s.update_channel, "development"],
    ["a list entry, as it is removed", "commands", nothing, () => { (root.querySelector('button[aria-label="Remove “never mind”"]') as HTMLButtonElement).click(); }, (s) => Object.keys(s.endings).includes("never mind"), false],
  ];

  it.each(triggers)("applies %s, and not before", async (_kind, page, edit, trigger, read, expected) => {
    const stub = await open();
    goTo(page);
    edit();
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    trigger();
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    expect(read(sent(stub))).toEqual(expected);
  });

  // (page, what the user does, the whole of what is sent)
  const patches: [string, () => void, unknown][] = [
    ["overlay", () => { flip(control("Show the draft", "input")); }, { overlay: { draft: false } }],
    ["commands", () => { type(control("Quiet before paste", "input"), "1.5"); }, { ending_pause: { paste: 1.5 } }],
    ["commands", () => { (root.querySelector('button[aria-label="Remove “never mind”"]') as HTMLButtonElement).click(); }, { endings: { "never mind": null } }],
    ["listening", () => { choose(control("Microphone", "select"), "USB Microphone"); }, { microphone: "USB Microphone" }],
    ["app", () => { choose(control("Update channel", "select"), "development"); }, { update_channel: "development" }],
  ];

  it.each(patches)("sends only the changed setting (%s)", async (page, change, patch) => {
    const stub = await open();
    goTo(page);
    change();
    await settle();
    expect(stub.backend.saveSettings.mock.calls.map(([sentPatch]) => sentPatch)).toEqual([patch]);
  });

  it("does not send a change back that another program made since the window looked", async () => {
    const stub = await open();
    const changed = structuredClone(SETTINGS);
    changed.overlay.draft = false;
    stub.backend.getSettings.mockResolvedValue(changed);
    goTo("overlay");
    flip(control("Wake sound", "input"));
    await settle();
    expect(stub.backend.saveSettings.mock.calls.map(([patch]) => patch)).toEqual([{ cues: { wake: false } }]);
  });

  it("sends nothing when the value is the one already applied", async () => {
    const stub = await open();
    goTo("listening");
    choose(control("Microphone", "select"), "");
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
  });

  it("has no Save or Revert on any page", async () => {
    await open();
    const labels = Array.from(root.querySelectorAll("button")).map((candidate) => candidate.textContent);
    expect(labels).not.toContain("Save");
    expect(labels).not.toContain("Revert");
    expect(root.textContent).not.toMatch(/Unsaved changes|Save server|Saved with Save/);
  });

  it("stacks quick changes: each is applied on top of the one before", async () => {
    const stub = await open();
    goTo("overlay");
    flip(control("Show the draft", "input"));
    flip(control("Wake sound", "input"));
    await settle();
    const [first, second] = stub.applied().map((settings) => [settings.overlay.draft, settings.cues.wake]);
    expect([first, second]).toEqual([[false, true], [false, false]]);
  });

  it("shows the reason under a number out of its range and sends nothing", async () => {
    const stub = await open();
    const limits = control("Stop after silence", "input").closest("details");
    if (limits instanceof HTMLDetailsElement) limits.open = true;
    type(control("Stop after silence", "input"), "5");
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    expect(reasonOf("Stop after silence")).toBe("Enter a number from 10 to 600 s.");
    expect(control("Stop after silence", "input").getAttribute("aria-invalid")).toBe("true");
  });

  it("shows the reason under a field the app refuses, keeps what is typed, and keeps the applied value working", async () => {
    const stub = await open();
    goTo("commands");
    stub.backend.saveSettings.mockRejectedValueOnce("The desktop config is not valid: bad pause.");
    type(control("Quiet before paste", "input"), "1.5");
    await settle();
    expect(reasonOf("Quiet before paste")).toBe("The desktop config is not valid: bad pause.");
    expect(control("Quiet before paste", "input").value).toBe("1.5");
    expect(rowText("Quiet before paste")).toBe("");
    flip(control("Wake sound", "input"));
    goTo("overlay");
    flip(control("Wake sound", "input"));
    await settle();
    expect(sent(stub).ending_pause.paste).toBe(SETTINGS.ending_pause.paste);
  });

  it("puts a refused switch back", async () => {
    const stub = await open();
    goTo("overlay");
    stub.backend.saveSettings.mockRejectedValueOnce("Thalvi could not apply it.");
    flip(control("Show the draft", "input"));
    await settle();
    expect(control("Show the draft", "input").checked).toBe(true);
    expect(reasonOf("Show the draft")).toBe("Thalvi could not apply it.");
  });

  it("restores the applied value on Escape", async () => {
    const stub = await open();
    goTo("commands");
    const field = control("Quiet before paste", "input");
    stub.backend.saveSettings.mockRejectedValueOnce("Too long.");
    type(field, "4");
    await settle();
    expect(reasonOf("Quiet before paste")).toBe("Too long.");
    field.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(field.value).toBe(String(SETTINGS.ending_pause.paste));
    expect(reasonOf("Quiet before paste")).toBe("");
    expect(field.getAttribute("aria-invalid")).toBe("false");
  });

  const CAPTURING: Status = { state: "capturing", target: null, committed: "", partial: null, notice: null };
  const DELIVERING: Status = { state: "working", target: null, route: "paste", cancellable: false };
  const flipSwitch = (label: string) => (): void => { flip(control(label, "input")); };
  const pickLook = (): void => { (root.querySelector('[data-choice="holo"] input') as HTMLInputElement).click(); };

  // (when the change is made, the status then, the setting and how it is changed, what its row says)
  const confirmations: [string, Status, string, () => void, string][] = [
    ["an ordinary setting while listening", { state: "listening" }, "Wake sound", flipSwitch("Wake sound"), APPLIED_TEXT],
    ["an ordinary setting while paused", { state: "paused" }, "Wake sound", flipSwitch("Wake sound"), APPLIED_TEXT],
    ["an ordinary setting while a capture runs", CAPTURING, "Wake sound", flipSwitch("Wake sound"), APPLIED_LATER_TEXT],
    ["an ordinary setting while a dictation is being delivered", DELIVERING, "Wake sound", flipSwitch("Wake sound"), APPLIED_LATER_TEXT],
    ["Show the overlay while a capture runs", CAPTURING, "Show the overlay", flipSwitch("Show the overlay"), APPLIED_TEXT],
    ["Show the draft while a capture runs", CAPTURING, "Show the draft", flipSwitch("Show the draft"), APPLIED_TEXT],
    ["the look while a capture runs", CAPTURING, "Look", pickLook, APPLIED_TEXT],
  ];

  it.each(confirmations)("says next to the control how it applies: %s", async (_when, status, label, change, text) => {
    const stub = await open();
    goTo("overlay");
    stub.emit(status);
    change();
    await settle();
    expect(rowText(label)).toBe(text);
  });

  it("says a change made during a capture is in effect once the capture is over", async () => {
    const stub = await open();
    goTo("overlay");
    stub.emit(CAPTURING);
    flipSwitch("Wake sound")();
    await settle();
    expect(rowText("Wake sound")).toBe(APPLIED_LATER_TEXT);
    stub.emit({ state: "listening" });
    expect(rowText("Wake sound")).toBe(APPLIED_TEXT);
  });

  it("applies a form of several fields only as a whole", async () => {
    const stub = await open();
    goTo("commands");
    const phraseBox = root.querySelector<HTMLInputElement>('input[aria-label="New script phrase"]') as HTMLInputElement;
    const destinationBox = root.querySelector<HTMLInputElement>('input[aria-label="New script destination"]') as HTMLInputElement;
    type(phraseBox, "open the news");
    button("Add script").click();
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    expect(rowOf("Scripts").querySelector(".note:not([hidden])")?.textContent).toBe("Type the phrase and the destination first.");
    type(destinationBox, "https://news.example");
    button("Add script").click();
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    expect(sent(stub).scripts.at(-1)).toEqual({ phrase: "open the news", action: "open_url", destination: "https://news.example", label: null });
    expect(phraseBox.value).toBe("");
  });

  it("applies a speech server only once its editor is finished and the app accepted it", async () => {
    const stub = await open();
    goTo("recognition");
    edit("Realtime speech server");
    type(control("Address", "input"), "wss://thalvi.example");
    type(control("Model", "input"), "other");
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    stub.backend.checkSpeechEntry.mockRejectedValueOnce("The address is not reachable.");
    await saveServer();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    expect(editorNote()).toBe("The address is not reachable.");
    await saveServer();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    expect(sent(stub).stt.entries[0]).toMatchObject({ address: "wss://thalvi.example", model: "other" });
  });

  describe("when the settings change from elsewhere", () => {
    it("refreshes the page but keeps the text of the field being typed in, which applies when the user leaves it", async () => {
      const stub = await open();
      goTo("commands");
      const changed = structuredClone(SETTINGS);
      changed.ending_pause.submit = 4;
      changed.overlay.draft = false;
      stub.backend.getSettings.mockImplementation(() => Promise.resolve(structuredClone(changed)));
      const timings = control("Quiet before paste", "input").closest("details");
      if (timings instanceof HTMLDetailsElement) timings.open = true;
      typeIn(control("Quiet before paste", "input"), "1.5");
      control("Quiet before paste", "input").focus();
      stub.changedElsewhere();
      await settle();
      expect(control("Quiet before paste and Enter", "input").value).toBe("4");
      expect(control("Quiet before paste", "input").value).toBe("1.5");
      expect(document.activeElement).toBe(control("Quiet before paste", "input"));
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      control("Quiet before paste", "input").blur();
      await settle();
      // Only the field the user changed is sent, so the change from elsewhere is not sent back stale.
      expect(stub.backend.saveSettings.mock.calls.map(([patch]) => patch)).toEqual([{ ending_pause: { paste: 1.5 } }]);
      goTo("overlay");
      expect(control("Show the draft", "input").checked).toBe(false);
    });

    it("loads the new settings and stays on the current page", async () => {
      const stub = await open(several());
      const changed = several();
      changed.stt.entries = changed.stt.entries.map((entry) => ({ ...entry, active: entry.id === "cybrex2" }));
      stub.backend.getSettings.mockImplementation(() => Promise.resolve(structuredClone(changed)));
      goTo("recognition");
      const activeNames = (): (string | null)[] =>
        Array.from(root.querySelectorAll(".trow.server")).filter((row) => row.querySelector('.badge[data-on="true"]') !== null).map((row) => row.querySelector(".phrase")?.textContent ?? null);
      expect(activeNames()).toEqual(["Realtime speech server"]);
      stub.changedElsewhere();
      await settle();
      expect(activeNames()).toEqual(["Cybrex2"]);
      expect(root.querySelector('nav [aria-current="page"]')?.getAttribute("data-page")).toBe("recognition");
    });
  });
});

describe("tables", () => {
  it("adds an ending phrase normalized and removes another", async () => {
    const stub = await open();
    goTo("commands");
    const add = rowOf("Ending phrases");
    type(add.querySelector<HTMLInputElement>(".add input") as HTMLInputElement, "  Send   It ");
    choose(add.querySelector<HTMLSelectElement>(".add select") as HTMLSelectElement, "submit");
    (add.querySelector(".add button") as HTMLElement).click();
    await settle();
    const phrases = Array.from(add.querySelectorAll(".table .phrase"), (node) => node.textContent);
    expect(phrases.at(-1)).toBe("“send it”");
    expect(add.querySelector(".table")?.nextElementSibling).toBe(add.querySelector(".trow.add"));
    root.querySelector<HTMLElement>('button[aria-label="Remove “never mind”"]')?.click();
    await settle();
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

  it.each([
    ["the same action", "paste that", "paste", true],
    ["a different action", "paste that", "submit", true],
    ["different case and spacing", "  Paste   THAT ", "paste", true],
    ["a new phrase, which also clears the earlier notice", "Hold  On", "submit", false],
  ] as const)("adds an ending phrase given %s only when it is not already listed", async (_name, typed, ending, refused) => {
    await open();
    goTo("commands");
    const table = rowOf("Ending phrases");
    const field = table.querySelector<HTMLInputElement>(".add input") as HTMLInputElement;
    const picker = table.querySelector<HTMLSelectElement>(".add select") as HTMLSelectElement;
    const notice = (): string => table.querySelector(".note:not([hidden])")?.textContent ?? "";
    const phrases = (): (string | null)[] => Array.from(table.querySelectorAll(".table .phrase"), (node) => node.textContent);
    const existing = (): string => table.querySelector<HTMLSelectElement>('select[aria-label="Action for “paste that”"]')?.value ?? "";
    const before = phrases();
    const tryAdd = async (text: string, action: string): Promise<void> => {
      type(field, text);
      choose(picker, action);
      (table.querySelector(".add button") as HTMLElement).click();
      await settle();
    };
    if (!refused) {
      await tryAdd("paste that", "scratch");
      expect(notice()).toBe("“paste that” is already in the list.");
    }
    await tryAdd(typed, ending);
    expect(existing()).toBe("paste");
    if (refused) {
      expect(notice()).toBe("“paste that” is already in the list.");
      expect(field.value).toBe(typed);
      expect(picker.value).toBe(ending);
      expect(phrases()).toEqual(before);
    } else {
      expect(notice()).toBe("");
      expect(field.value).toBe("");
      expect(phrases()).toEqual([...before, "“hold on”"]);
      expect(table.querySelector<HTMLSelectElement>('select[aria-label="Action for “hold on”"]')?.value).toBe("submit");
    }
  });

  it("edits an app and a script, and adds a script from its own form", async () => {
    const stub = await open();
    goTo("commands");
    const app = root.querySelector<HTMLDetailsElement>('details[data-fold="commands/apps/slack/match"]');
    const script = root.querySelector<HTMLDetailsElement>('details[data-fold="commands/scripts/0/label"]');
    if (app) app.open = true;
    if (script) script.open = true;
    type(root.querySelector<HTMLInputElement>('input[aria-label="Executables of slack"]') as HTMLInputElement, "slack, slack.exe");
    type(root.querySelector<HTMLInputElement>('input[aria-label="Label of script 1"]') as HTMLInputElement, "");
    type(root.querySelector<HTMLInputElement>('input[aria-label="New script phrase"]') as HTMLInputElement, "focus slack");
    choose(root.querySelector<HTMLSelectElement>('select[aria-label="New script action"]') as HTMLSelectElement, "focus_app");
    type(root.querySelector<HTMLInputElement>('input[aria-label="New script destination"]') as HTMLInputElement, "slack");
    button("Add script").click();
    await settle();
    const saved = sent(stub);
    expect(saved.apps["slack"]?.executables).toEqual(["slack", "slack.exe"]);
    expect(saved.scripts).toEqual([
      { phrase: "open the docs", action: "open_url", destination: "https://example.com", label: null },
      { phrase: "focus slack", action: "focus_app", destination: "slack", label: null },
    ]);
  });

  it("refuses a duplicate app name inline", async () => {
    await open();
    goTo("commands");
    type(root.querySelector<HTMLInputElement>('input[aria-label="New app name"]') as HTMLInputElement, "slack");
    button("Add app").click();
    expect(root.querySelector(".note[role=alert]:not([hidden])")?.textContent).toBe("An app named slack already exists.");
  });

  const twoApps = {
    firefox: { classes: ["firefox"], executables: [], input_keys: [] },
    slack: { classes: ["Slack"], executables: [], input_keys: [] },
  };
  it.each([
    ["the first app", ["firefox", "slack"], "firefox", true, null, "Remove slack"],
    ["the last app", ["firefox", "slack"], "slack", true, null, "Remove firefox"],
    ["the only app", ["slack"], "slack", true, null, "Add app"],
    ["an app, refused", ["firefox", "slack"], "slack", true, "Thalvi could not apply it.", "Remove slack"],
    ["an app while nothing has focus", ["firefox", "slack"], "slack", false, null, "BODY"],
  ] as const)("after removing %s, focus is on its neighbour's Remove, else Add app", async (_case, apps, removed, focused, refusal, expected) => {
    const stub = await open({ ...SETTINGS, apps: Object.fromEntries(apps.map((name) => [name, twoApps[name]])) });
    if (refusal) stub.backend.saveSettings.mockRejectedValueOnce(refusal);
    goTo("commands");
    const remove = root.querySelector<HTMLButtonElement>(`button[aria-label="Remove ${removed}"]`) as HTMLButtonElement;
    if (focused) remove.focus();
    remove.click();
    await settle();
    const active = document.activeElement as HTMLElement;
    expect(active.getAttribute("aria-label") ?? (active === document.body ? "BODY" : active.textContent)).toBe(expected);
    expect(rowOf("Apps").querySelector(".note:not([hidden])")?.textContent ?? null).toBe(refusal);
  });

  it.each([
    ["the first phrase", SETTINGS.endings, "paste that", true, null, "Remove “write that out”"],
    ["the last phrase", SETTINGS.endings, "never mind", true, null, "Remove “scratch that”"],
    ["the only phrase", { "never mind": "cancel" }, "never mind", true, null, "New ending phrase"],
    ["a phrase, refused", SETTINGS.endings, "never mind", true, "Thalvi could not apply it.", "Remove “never mind”"],
    ["a phrase while nothing has focus", SETTINGS.endings, "never mind", false, null, "BODY"],
  ] as const)("after removing %s, focus is on its neighbour's Remove, else New ending phrase", async (_case, endings, removed, focused, refusal, expected) => {
    const stub = await open({ ...SETTINGS, endings });
    if (refusal) stub.backend.saveSettings.mockRejectedValueOnce(refusal);
    goTo("commands");
    const remove = root.querySelector<HTMLButtonElement>(`button[aria-label="Remove “${removed}”"]`) as HTMLButtonElement;
    if (focused) remove.focus();
    remove.click();
    await settle();
    const active = document.activeElement as HTMLElement;
    expect(active.getAttribute("aria-label") ?? (active === document.body ? "BODY" : active.textContent)).toBe(expected);
    expect(rowOf("Ending phrases").querySelector(".note:not([hidden])")?.textContent ?? null).toBe(refusal);
  });

  const docs = { phrase: "open the docs", action: "open_url", destination: "https://example.com", label: null } as const;
  const news = { phrase: "open the news", action: "open_url", destination: "https://news.example", label: null } as const;
  it.each([
    ["the first script", [docs, news], 1, true, null, "Remove script 1", ["open the news"]],
    ["the last script", [docs, news], 2, true, null, "Remove script 1", ["open the docs"]],
    ["the only script", [docs], 1, true, null, "New script phrase", []],
    ["a script, refused", [docs, news], 2, true, "Thalvi could not apply it.", "Remove script 2", ["open the docs", "open the news"]],
    ["a script while nothing has focus", [docs, news], 2, false, null, "BODY", ["open the docs"]],
  ] as const)("after removing %s, focus is on its neighbour's Remove, else New script phrase", async (_case, scripts, removed, focused, refusal, expected, left) => {
    const stub = await open({ ...SETTINGS, scripts: [...scripts] });
    if (refusal) stub.backend.saveSettings.mockRejectedValueOnce(refusal);
    goTo("commands");
    const remove = root.querySelector<HTMLButtonElement>(`button[aria-label="Remove script ${String(removed)}"]`) as HTMLButtonElement;
    if (focused) remove.focus();
    remove.click();
    await settle();
    const active = document.activeElement as HTMLElement;
    expect(active.getAttribute("aria-label") ?? (active === document.body ? "BODY" : active.textContent)).toBe(expected);
    expect([...root.querySelectorAll<HTMLInputElement>('input[aria-label^="Phrase of script"]')].map((input) => input.value)).toEqual(left);
    expect(rowOf("Scripts").querySelector(".note:not([hidden])")?.textContent ?? null).toBe(refusal);
  });

  it("a script whose Remove gains focus while another script's removal saves keeps it", async () => {
    const mail = { ...news, phrase: "open the mail" };
    const stub = await open({ ...SETTINGS, scripts: [docs, news, mail] });
    let release = (): void => undefined;
    stub.backend.saveSettings.mockReturnValueOnce(new Promise<void>((resolve) => { release = resolve; }));
    goTo("commands");
    const remove = (n: number): HTMLButtonElement => root.querySelector<HTMLButtonElement>(`button[aria-label="Remove script ${String(n)}"]`) as HTMLButtonElement;
    remove(1).focus();
    remove(1).click();
    await settle();
    remove(2).focus();
    release();
    await settle();
    const active = document.activeElement as HTMLElement;
    expect(active.getAttribute("aria-label")).toBe("Remove script 1");
    expect(active.closest(".card")?.querySelector<HTMLInputElement>('input[aria-label^="Phrase of script"]')?.value).toBe("open the news");
  });
});

describe("the wake word picker", () => {
  async function openCapture(settings: Settings = SETTINGS) {
    const stub = await open(settings);
    goTo("listening");
    return stub;
  }
  const WITH_WAKE = "Listening for the wake words and the hold keys.";
  const KEYBOARD = "Listening for the hold keys only.";
  const PAUSED = "Paused: neither the hold keys nor the wake words start anything.";

  // (saved switch, paused, clicked, the desktop refuses with, switch after, sent, status text, notice)
  const switches: [boolean, boolean, boolean, string | null, boolean, boolean | null, string, string][] = [
    [true, false, true, null, false, false, KEYBOARD, ""],
    [false, false, true, null, true, true, WITH_WAKE, ""],
    [true, true, true, null, false, false, PAUSED, ""],
    [true, false, true, "Thalvi could not apply it.", true, false, WITH_WAKE, "Thalvi could not apply it."],
    [false, false, false, null, false, null, KEYBOARD, ""],
  ];

  it.each(switches)("wake words saved %j, paused %j, clicked %j, refused with %j: shows %j, sends %j, says %j", async (saved, paused, clicked, refusal, after, sent, text, notice) => {
    const stub = await openCapture({ ...SETTINGS, wake: { ...SETTINGS.wake, enabled: saved } });
    if (paused) stub.emit({ state: "paused" });
    if (refusal !== null) stub.backend.saveSettings.mockRejectedValue(refusal);
    const input = control("Wake words", "input");
    expect(input.checked).toBe(saved);
    if (clicked) flip(input);
    await settle();

    expect(input.checked).toBe(after);
    expect(stub.backend.saveSettings.mock.calls.map(([patch]) => patch)).toEqual(sent === null ? [] : [{ wake: { enabled: sent } }]);
    expect(rowOf("Wake words").querySelector("small.unit[role=status]")?.textContent).toBe(text);
    expect(rowOf("Wake words").querySelector(".note:not([hidden])")?.textContent ?? "").toBe(notice);
  });

  const badge = (label: string): string => rowOf(label).querySelector(".badge")?.textContent ?? "";
  const problem = (label: string): string =>
    rowOf(label).querySelector<HTMLElement>(".note:not([hidden])")?.textContent ?? "";

  it("shows catalog phrases and downloads the selected dictation head", async () => {
    const stub = await openCapture();
    expect(badge("Wake model (dictation)")).toBe("Ready");
    const dictation = control("Wake model (dictation)", "select");
    expect(Array.from(dictation.options).map((option) => option.textContent)).toEqual([
      "Hey Thalvi — Ready",
      "Hey Mycroft — Not downloaded",
      "Hey Jarvis — Not downloaded",
      "Custom model…",
    ]);
    let finish = (): void => { throw new Error("download has not started"); };
    stub.backend.downloadWakeModel.mockImplementationOnce(() => new Promise<void>((resolve) => { finish = resolve; }));
    choose(dictation, "hey_mycroft");
    await settle();
    within(rowOf("Wake model (dictation)"), "Download").click();
    expect(badge("Wake model (dictation)")).toBe("Downloading…");
    finish();
    await settle();
    expect(stub.backend.downloadWakeModel).toHaveBeenCalledWith("hey_mycroft");
    expect(badge("Wake model (dictation)")).toBe("Ready");
  });

  it("shows a dictation model downloaded while the window stayed open", async () => {
    const settings = {
      ...SETTINGS,
      wake: { ...SETTINGS.wake, capture: { model: "hey_jarvis", threshold: 0.5 } },
    };
    const stub = await openCapture(settings);
    expect(badge("Wake model (dictation)")).toBe("Not downloaded");
    stub.backend.listWakeModels.mockResolvedValueOnce(
      WAKE_MODELS_INFO.map((info) => ({ ...info, cached: info.cached || info.name === "hey_jarvis" })),
    );
    stub.emit({ state: "loading", notice: "Downloading the wake word model." });
    await settle();
    expect(badge("Wake model (dictation)")).toBe("Ready");
    expect(Array.from(control("Wake model (dictation)", "select").options).map((option) => option.textContent)).toEqual([
      "Hey Thalvi — Ready",
      "Hey Mycroft — Not downloaded",
      "Hey Jarvis — Ready",
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

  it("shows catalog choices in the dictation picker", async () => {
    await openCapture();
    const dictation = control("Wake model (dictation)", "select");
    expect(Array.from(dictation.options).filter((option) => option.disabled)).toEqual([]);
  });

  it("moves the threshold to the chosen catalog phrase's default and saves it", async () => {
    const stub = await openCapture({ ...SETTINGS, wake: { ...SETTINGS.wake, capture: { model: "hey_thalvi", threshold: 0.9 } } });
    const shown = (): string => rowOf("Threshold (dictation)").querySelector("output")?.textContent ?? "";
    expect(shown()).toBe("0.90");
    choose(control("Wake model (dictation)", "select"), "hey_jarvis");
    expect(shown()).toBe("0.50");
    choose(control("Wake model (dictation)", "select"), "hey_thalvi");
    expect(shown()).toBe("0.70");
    await settle();
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
    await settle();
    await settle();
    expect(stub.backend.validateWakeModels).toHaveBeenCalledWith("/models/custom.onnx");
    expect(sent(stub).wake.capture.model).toBe("/models/custom.onnx");
  });

  it("shows an invalid custom file at once and does not save it", async () => {
    const stub = await openCapture();
    stub.backend.pickWakeModelFile.mockResolvedValue("/models/bad.onnx");
    choose(control("Wake model (dictation)", "select"), "__custom");
    within(rowOf("Wake model (dictation)"), "Browse…").click();
    await settle();
    expect(problem("Wake model (dictation)")).toBe("The wake model /models/bad.onnx could not be loaded: not a wake model.");
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
  });

  it("preserves the saved conversation wake choice when other settings are saved", async () => {
    const conversation = { model: "hey_jarvis", threshold: 0.65 };
    const stub = await openCapture({ ...SETTINGS, wake: { ...SETTINGS.wake, conversation } });
    type(control("Quiet before paste", "input"), "1.5");
    await settle();
    await settle();
    expect(sent(stub).wake.conversation).toEqual(conversation);
  });

  it("shows a saved custom file by name when Settings opens", async () => {
    await openCapture({
      ...SETTINGS,
      wake: { capture: { model: "/models/mine.onnx", threshold: 0.5 }, conversation: { model: "hey_mycroft", threshold: 0.5 }, enabled: true },
    });
    expect(control("Wake model (dictation)", "select").value).toBe("__custom");
    expect(rowOf("Wake model (dictation)").textContent).toContain("mine.onnx");
  });
});

describe("a custom speech model folder", () => {
  async function openLocal() {
    const stub = await open(local());
    goTo("recognition");
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
    await settle();
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
    goTo("recognition");
    await editLocal();
    await saveServer();
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
    expect(stub.backend.checkSpeechEntry).toHaveBeenCalledTimes(1);
  });
});

describe("the word list", () => {
  const FAVOURED = "Names and terms, added one at a time.";
  const VULKAN_TITLE = "Whisper Turbo · Vulkan GPU (experimental)";
  const cant = (name: string): string => `${name} can't use the word list`;

  async function openWordList(phrases?: string[]) {
    const stub = await open({ ...SETTINGS, stt: { entries: [device()], ...(phrases ? { phrases } : {}) } });
    goTo("recognition");
    return stub;
  }

  function chips(): string[] {
    return Array.from(rowOf("Word list").querySelectorAll(".chip > span")).map((chip) => chip.textContent);
  }

  function wordHint(): string {
    return rowOf("Word list").querySelector(".what small")?.textContent ?? "";
  }

  function listedModels() {
    return [
      { ...NEMOTRON },
      { ...UNIFIED },
      { ...NEMOTRON, name: "parakeet_tdt_v3", title: "Parakeet TDT" },
      { ...WHISPER },
      { ...WHISPER, name: "whisper_turbo_vulkan", title: VULKAN_TITLE },
    ];
  }

  async function add(word: string, by: "plus" | "enter" = "plus"): Promise<void> {
    const box = rowOf("Word list").querySelector('input[aria-label="New word"]') as HTMLInputElement;
    type(box, word);
    if (by === "enter") box.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    else (rowOf("Word list").querySelector('button[aria-label="Add the word"]') as HTMLButtonElement).click();
    await settle();
  }

  it("keeps the list available, edits terms, and saves them locally", async () => {
    const stub = await openWordList();
    expect(rowOf("Word list").hidden).toBe(false);
    await add("  Cybrex2 ");
    await add("Thalvi", "enter");
    expect(chips()).toEqual(["Cybrex2", "Thalvi"]);
    await settle();
    await settle();
    expect(sent(stub).stt.phrases).toEqual(["Cybrex2", "Thalvi"]);
  });

  it("shows saved terms and removes them", async () => {
    const stub = await openWordList(["Cybrex2", "Thalvi"]);
    expect(chips()).toEqual(["Cybrex2", "Thalvi"]);
    (rowOf("Word list").querySelector('button[aria-label="Remove Cybrex2"]') as HTMLButtonElement).click();
    await settle();
    expect(chips()).toEqual(["Thalvi"]);
    await settle();
    await settle();
    expect(sent(stub).stt.phrases).toEqual(["Thalvi"]);
  });

  it("offers the list to direct recognizers that announce phrase boosting", async () => {
    const stub = stubBackend({ ...SETTINGS, stt: { entries: [{ ...SERVER, active: true }] } });
    stub.backend.wordListSupport.mockResolvedValue(false);
    mountSettings(root, stub.backend);
    await settle();
    expect(stub.backend.wordListSupport).toHaveBeenCalledWith(expect.objectContaining({ kind: "realtime" }));
    expect(rowOf("Word list").classList.contains("off")).toBe(false);
    expect(wordHint()).toBe(cant("Realtime speech server"));
  });

  it.each([
    ["Whisper Turbo on the CPU", "whisper_turbo_cpu", cant("On this computer")],
    ["Whisper Turbo on the Vulkan GPU", "whisper_turbo_vulkan", cant("On this computer")],
    ["Nemotron", "nemotron_160ms", FAVOURED],
    ["Parakeet", "parakeet_tdt_v3", FAVOURED],
    ["no model", undefined, FAVOURED],
    ["a model folder", "/models/mine", FAVOURED],
  ] as const)("with %s active, the word list hint says whether that recognizer uses it", async (_name, model, expected) => {
    const stub = stubBackend({ ...SETTINGS, stt: { entries: [device(model)] } });
    stub.backend.listSpeechModels.mockResolvedValue(listedModels());
    mountSettings(root, stub.backend);
    await settle();
    expect(wordHint()).toBe(expected);
    expect(rowOf("Word list").hidden).toBe(false);
    expect(rowOf("Word list").classList.contains("off")).toBe(false);
    expect((rowOf("Word list").querySelector('input[aria-label="New word"]') as HTMLInputElement).disabled).toBe(false);
  });

  it("updates the hint when Use switches between a recognizer that ignores the list and one that favours it", async () => {
    const stub = stubBackend({
      ...SETTINGS,
      stt: {
        entries: [device("whisper_turbo_cpu"), { ...device("nemotron_160ms", false), id: "nemotron", name: "Nemotron Streaming" }],
      },
    });
    stub.backend.listSpeechModels.mockResolvedValue(listedModels());
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    expect(wordHint()).toBe(cant("On this computer"));
    use("Nemotron Streaming");
    await settle();
    expect(wordHint()).toBe(FAVOURED);
    use("On this computer");
    await settle();
    expect(wordHint()).toBe(cant("On this computer"));
    expect(stub.backend.wordListSupport).not.toHaveBeenCalled();
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
  kind: "extra",
  id: "openai",
  name: "OpenAI",
  active: true,
  address: "https://api.openai.com/v1",
  model: "whisper-1",
  api: "openai",
  has_key: true,
};
const META: SpeechEntry = {
  kind: "extra",
  id: "meta",
  name: "Meta",
  active: false,
  address: "https://meta.example/v1",
  model: "asr",
  api: "meta",
};
const GATEWAY: SpeechEntry = {
  kind: "open_ai",
  id: "gateway",
  name: "LLM Gateway",
  active: false,
  address: "https://gw.example/v1",
  model: "whisper-1",
  request: "transcriptions",
  language: "",
};

const WRITING = { url: "http://127.0.0.1:8000/v1", model: "glimmer" };

/** A realtime recognizer in use, another recognizer, the on-device model, and two extra recognizers, with a writing endpoint set. */
function several(): Settings {
  return { ...SETTINGS, writing: WRITING, stt: { entries: [{ ...SERVER }, { ...CYBREX }, device(undefined, false), { ...OPENAI }, { ...META }] } };
}

function entryNamed(name: string): HTMLElement {
  const found = Array.from(root.querySelectorAll<HTMLElement>(".trow.server")).find((entry) => entry.querySelector(".phrase")?.textContent === name);
  if (!found) throw new Error(`no entry ${name}`);
  return found;
}

const names = (list: HTMLElement): (string | null)[] => Array.from(list.querySelectorAll(".trow.server .phrase")).map((phrase) => phrase.textContent);
const recognition = (): HTMLElement => root.querySelector("section[data-page=recognition]") as HTMLElement;
const listOf = (kind: SpeechEntry["kind"]): HTMLElement => recognition().querySelector(`.table[data-kind="${kind}"]`) as HTMLElement;
const tickOf = (name: string): HTMLInputElement => entryNamed(name).querySelector("input[type=checkbox]") as HTMLInputElement;
const editorEl = (): HTMLElement => root.querySelector(".editor") as HTMLElement;
const editButton = (name: string): HTMLButtonElement => root.querySelector(`button[aria-label="Edit ${name}"]`) as HTMLButtonElement;
const summaryText = (): string => recognition().querySelector(".next-dictation")?.textContent ?? "";
const extrasText = (): string => recognition().querySelector(".extras")?.textContent ?? "";
const needsChat = (): HTMLElement => recognition().querySelector(".needs-chat") as HTMLElement;

/** What a row shows at its right edge: Live, or its Extra switch and whether it is on. */
function roleOf(name: string): string {
  const row = entryNamed(name);
  const tick = row.querySelector<HTMLInputElement>("input[type=checkbox]");
  if (tick) return `${row.querySelector(".extra label")?.textContent ?? ""} ${tick.checked ? "on" : "off"}`;
  return row.querySelector(".badge")?.textContent ?? "";
}

describe("the Recognition page", () => {
  async function openSeveral() {
    const stub = await open(several());
    goTo("recognition");
    return stub;
  }

  it("shows the Live recognizer with the next dictation and the extras, then the four groups by kind, then the word list", async () => {
    await openSeveral();
    const boxes = Array.from(recognition().querySelectorAll<HTMLElement>(".grp")).map((box) => {
      const head = box.querySelector<HTMLElement>(".row") as HTMLElement;
      return [
        head.querySelector(".what > :first-child")?.textContent,
        head.querySelector(".what small")?.textContent,
        head.querySelector(".ctl button[aria-label^='Add a']")?.getAttribute("aria-label") ?? null,
        box.querySelector(".none")?.textContent ?? null,
      ];
    });
    expect(boxes).toEqual([
      ["Live recognizer", "Shows your words and hears ending phrases.", null, null],
      ["Streaming servers", "Words appear while you speak.", "Add a streaming server", null],
      ["After-you-stop servers", "Sends each capture once you stop.", "Add an after-you-stop server", "None yet."],
      ["On this computer", "A downloaded model; audio stays here.", "Add an on-device model", null],
      ["Extra-only servers", "Only check the live text; need a chat model.", "Add an extra-only server", null],
      ["Word list", "Names and terms, added one at a time.", null, null],
    ]);
    const live = recognition().querySelector(".grp") as HTMLElement;
    expect(Array.from(live.children).filter((node) => !(node as HTMLElement).hidden).map((node) => node.className)).toEqual(["row", "next-dictation", "next-dictation extras"]);
    expect(summaryText()).toBe("Next dictation: Realtime speech server · text arrives as you speak · Ready");
    expect(extrasText()).toBe("Extra: OpenAI. The chat model merges its text with the live text.");
    for (const box of recognition().querySelectorAll<HTMLElement>(".grp .what small")) expect(box.textContent.length).toBeLessThanOrEqual(HINT_BUDGET);
    expect(recognition().querySelectorAll("select")).toHaveLength(1);
    expect(recognition().textContent).not.toContain("Live streams.");
  });

  it.each([
    ["On this computer", "On this computer"],
    ["ON THIS Computer", "ON THIS Computer"],
    [" on this computer ", " on this computer "],
    ["Laptop model", "Laptop model (on this computer)"],
  ])("lists every live-kind entry in the Live recognizer drop-down by name and group, unless the name says the group (%j), with the one in use chosen", async (name, label) => {
    await open({ ...several(), stt: { entries: [{ ...SERVER }, { ...GATEWAY }, { ...OPENAI }, { ...device(undefined, false), name }] } });
    goTo("recognition");
    expect(Array.from(liveSelect().options).map((option) => [option.value, option.textContent])).toEqual([
      ["direct", "Realtime speech server (streaming)"],
      ["gateway", "LLM Gateway (after you stop)"],
      ["on-device", label],
    ]);
    expect(liveSelect().value).toBe("direct");
  });

  it.each([
    ["Realtime speech server", "127.0.0.1:8012", "Live"],
    ["Cybrex2", "100.64.0.2:8012 · key saved", "Extra off"],
    ["On this computer", "Nemotron Streaming", "Extra off"],
    ["LLM Gateway", "gw.example", "Extra off"],
    ["OpenAI", "api.openai.com · key saved", "Extra on"],
    ["Meta", "meta.example", "Extra off"],
  ])("shows %s as one flat row: name, where it runs, its role and Edit", async (name, where, role) => {
    await open({ ...several(), stt: { entries: [...several().stt.entries, { ...GATEWAY }] } });
    goTo("recognition");
    const row = entryNamed(name);
    expect(row.querySelector(".unit")?.textContent).toBe(where);
    expect(roleOf(name)).toBe(role);
    expect(Array.from(row.querySelectorAll("button")).map((node) => [node.textContent, node.getAttribute("aria-label")])).toEqual([["Edit", `Edit ${name}`]]);
    expect(row.querySelectorAll(".badge").length).toBe(role === "Live" ? 1 : 0);
    expect(row.querySelector("details")).toBeNull();
    const tick = row.querySelector<HTMLInputElement>("input[type=checkbox]");
    if (tick) {
      expect(tick.getAttribute("aria-label")).toBe(`Extra: ${name}`);
      expect((row.querySelector(".extra label") as HTMLLabelElement).htmlFor).toBe(tick.id);
    }
  });

  it("keeps every row in config order, in its kind's group, when the Live recognizer changes", async () => {
    await openSeveral();
    const order = (): (string | null)[][] => (["realtime", "open_ai", "on_device", "extra"] as const).map((kind) => names(listOf(kind)));
    const before = order();
    expect(before).toEqual([["Realtime speech server", "Cybrex2"], [], ["On this computer"], ["OpenAI", "Meta"]]);
    use("Cybrex2");
    await settle();
    expect(order()).toEqual(before);
    expect([roleOf("Cybrex2"), roleOf("Realtime speech server")]).toEqual(["Live", "Extra off"]);
    use("On this computer");
    await settle();
    expect(order()).toEqual(before);
  });

  it("switches the Live recognizer from the drop-down, applying only that, and keeps focus on it", async () => {
    const stub = await open({ ...several(), stt: { entries: several().stt.entries.map((entry) => (entry.id === "cybrex2" ? { ...entry, extra: true } : entry)) } });
    goTo("recognition");
    liveSelect().focus();
    use("Cybrex2");
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    expect(sent(stub).stt.entries.map((entry) => [entry.name, entry.active, "extra" in entry])).toEqual([
      ["Realtime speech server", false, false],
      ["Cybrex2", true, false],
      ["On this computer", false, false],
      ["OpenAI", true, false],
      ["Meta", false, false],
    ]);
    expect(document.activeElement).toBe(liveSelect());
    expect(summaryText()).toBe("Next dictation: Cybrex2 · text arrives as you speak · Ready");
  });

  const where = (text: string): string => {
    const found = Array.from(recognition().querySelectorAll<HTMLElement>(".applied, .note")).find((node) => !node.hidden && node.textContent === text);
    if (!found) return "nowhere";
    if (found.closest(".editor")) return "form";
    if (found.closest(".trow.server")) return `row ${found.closest(".trow.server")?.querySelector(".phrase")?.textContent ?? ""}`;
    return found.closest(".row")?.getAttribute("data-label") ?? "elsewhere";
  };
  const REFUSED = "The settings could not be saved.";
  it.each([
    ["choosing a Live recognizer", false, async () => { use("Cybrex2"); await settle(); }, APPLIED_TEXT, "live recognizer"],
    ["a refused Live recognizer", true, async () => { use("Cybrex2"); await settle(); }, REFUSED, "live recognizer"],
    ["an Extra switch", false, async () => { flip(tickOf("Meta")); await settle(); }, APPLIED_TEXT, "row Meta"],
    ["a refused Extra switch", true, async () => { flip(tickOf("Meta")); await settle(); }, REFUSED, "row Meta"],
    ["a saved edit", false, async () => { edit("Cybrex2"); type(control("Model", "input"), "other"); await saveServer(); }, APPLIED_TEXT, "row Cybrex2"],
    ["a refused edit", true, async () => { edit("Cybrex2"); type(control("Model", "input"), "other"); await saveServer(); }, REFUSED, "form"],
    ["a refused removal", true, async () => { edit("Meta"); within(editorEl(), "Remove").click(); await settle(); }, REFUSED, "form"],
  ] as const)("shows how %s went in the form or on its row", async (_case, refuse, act, text, expected) => {
    const stub = await openSeveral();
    if (refuse) stub.backend.saveSettings.mockRejectedValueOnce(REFUSED);
    await act();
    expect(where(text)).toBe(expected);
  });

  const liveChoice = async () => { use("Cybrex2"); await settle(); };
  it.each([
    ["a Live choice", "an Extra switch", liveChoice, async () => { flip(tickOf("Meta")); await settle(); }, () => entryNamed("Meta")],
    ["a Live choice", "a removal", liveChoice, async () => { edit("Meta"); within(editorEl(), "Remove").click(); await settle(); }, () => listOf("extra").parentElement],
    ["a Live choice", "a saved edit", liveChoice, async () => { edit("Cybrex2"); type(control("Model", "input"), "other"); await saveServer(); }, () => entryNamed("Cybrex2")],
    ["an Extra switch", "another Extra switch", async () => { flip(tickOf("On this computer")); await settle(); }, async () => { flip(tickOf("Meta")); await settle(); }, () => entryNamed("Meta")],
  ] as const)("after %s, shows only the Applied. of %s", async (_first, _then, before, act, owner) => {
    await openSeveral();
    await before();
    await act();
    const shown = Array.from(recognition().querySelectorAll<HTMLElement>(".applied")).filter((node) => node.textContent === APPLIED_TEXT);
    expect(shown).toHaveLength(1);
    expect(owner()?.contains(shown[0] ?? null)).toBe(true);
  });

  it.each([
    ["realtime", "Realtime transcription (WebSocket)", ["Name", "Address", "API key", "Model"]],
    ["open_ai", "OpenAI transcriptions", ["Name", "Address", "API key", "Model", "Request", "Language"]],
    ["on_device", "On this computer 2", ["Name", "Show models for", "Speech model"]],
    ["extra", "Extra recognizer", ["Name", "Address", "API key", "Model", "API"]],
  ] as const)("opens the %s form at the top of its group with a short default name, its explanation and fields, then Cancel and Save", async (kind, name, fields) => {
    await openSeveral();
    await addServer(kind);
    expect(root.querySelectorAll(".editor:not([hidden])")).toHaveLength(1);
    expect(editorEl().parentElement).toBe(listOf(kind));
    expect(listOf(kind).firstElementChild).toBe(editorEl());
    expect(editorEl().querySelector("h4")?.textContent).toBe(ADD[kind]);
    expect(editorEl().querySelector("h4 + p")?.textContent).toMatch(/^[A-Z].{20,}\.$/);
    expect(Array.from(editorEl().querySelectorAll<HTMLElement>(".row")).map((row) => row.querySelector(".what > :first-child")?.textContent)).toEqual(fields);
    expect(control("Name", "input").value).toBe(name);
    expect(Array.from(editorEl().querySelectorAll(".actions button")).map((node) => node.textContent)).toEqual(["Cancel", "Save"]);
    expect(editorEl().querySelector("details.fold[data-fold^='recognition/server/']")).toBeNull();
  });

  it.each([
    ["realtime", [["Name", "How this server is listed when something fails."], ["Address", "The server's ws:// or wss:// address. Thalvi's own recognizer uses port 8012."], ["API key", "Only if the server needs one. Saved, never shown."], ["Model", "The model the server should use. Thalvi's own recognizer accepts any name."]]],
    ["open_ai", [["Name", "How this server is listed when something fails."], ["Address", "Where each capture is sent for transcription."], ["API key", "Saved in the config. Never shown again."], ["Model", "The model named in each request."], ["Request", "Transcriptions, or Chat with audio."], ["Language", "A two-letter code such as de. Empty detects it."]]],
    ["on_device", [["Name", "How this server is listed when something fails."], ["Show models for", "Filters this list by each card's languages."], ["Speech model", "Downloads on first use. A pick applies next."]]],
    ["extra", [["Name", "How this server is listed when something fails."], ["Address", "Where the extra transcription is sent."], ["API key", "Saved in the config. Never shown again."], ["Model", "The model named in each request."], ["API", "Which transcription API the endpoint speaks."]]],
  ] as const)("explains each field of the %s form under its label", async (kind, hints) => {
    await openSeveral();
    await addServer(kind);
    expect(Array.from(editorEl().querySelectorAll<HTMLElement>(".row")).map((row) => [row.querySelector(".what > :first-child")?.textContent, row.querySelector(".what > small")?.textContent ?? null])).toEqual(hints);
  });

  it.each([
    ["explanation", () => editorEl().querySelector("h4 + p")?.textContent, "Words appear while you speak. Connect Thalvi's own recognizer, which runs on a computer with an NVIDIA GPU, or another server with the same WebSocket protocol for realtime transcription."],
    ["default name", () => control("Name", "input").value, "Realtime transcription (WebSocket)"],
    ["Address hint", () => rowOf("Address").querySelector(".what > small")?.textContent, "The server's ws:// or wss:// address. Thalvi's own recognizer uses port 8012."],
    ["API key hint", () => rowOf("API key").querySelector(".what > small")?.textContent, "Only if the server needs one. Saved, never shown."],
    ["Model hint", () => rowOf("Model").querySelector(".what > small")?.textContent, "The model the server should use. Thalvi's own recognizer accepts any name."],
  ] as const)("says what a streaming server can be and how to fill it in: the %s", async (_what, shown, text) => {
    await openSeveral();
    await addServer("realtime");
    expect(shown()).toBe(text);
  });

  it("numbers a default name that is taken", async () => {
    await open({ ...several(), stt: { entries: [{ ...SERVER, name: "Realtime transcription (WebSocket)" }, { ...META, name: "Extra recognizer" }] } });
    goTo("recognition");
    await addServer("realtime");
    expect(control("Name", "input").value).toBe("Realtime transcription (WebSocket) 2");
    await addServer("extra");
    expect(control("Name", "input").value).toBe("Extra recognizer 2");
  });

  it("opens Edit in place of its row, one form at a time, with Remove before Cancel and Save", async () => {
    await openSeveral();
    edit("Cybrex2");
    await settle();
    expect(names(listOf("realtime"))).toEqual(["Realtime speech server"]);
    expect(listOf("realtime").children[1]).toBe(editorEl());
    expect(editorEl().querySelector("h4")?.textContent).toBe("Editing “Cybrex2”");
    expect(Array.from(editorEl().querySelectorAll<HTMLElement>(".actions button")).filter((node) => node.closest("[hidden]") === null).map((node) => node.textContent)).toEqual(["Remove", "Cancel", "Save"]);
    await addServer("extra");
    expect(root.querySelectorAll(".editor")).toHaveLength(1);
    expect(names(listOf("realtime"))).toEqual(["Realtime speech server", "Cybrex2"]);
    expect(editButton("Cybrex2").getAttribute("aria-expanded")).toBe("false");
  });

  it.each([
    ["realtime", "Cancel", "the +"],
    ["realtime", "Save", "the new entry's Edit"],
    ["extra", "Cancel", "the +"],
    ["extra", "Save", "the new entry's Edit"],
    ["edit", "Cancel", "its Edit"],
    ["edit", "Save", "its Edit"],
  ] as const)("returns focus from a %s form closed with %s to %s", async (opener, close, expected) => {
    const stub = await openSeveral();
    if (opener === "edit") editButton("Cybrex2").click();
    else plus(opener).click();
    await settle();
    expect(document.activeElement).toBe(control("Name", "input"));
    const name = control("Name", "input").value;
    if (opener === "realtime") type(control("Address", "input"), "ws://127.0.0.1:8013");
    if (opener === "extra") type(control("Address", "input"), "http://127.0.0.1:9000/v1");
    if (opener === "edit") type(control("Model", "input"), "other");
    within(editorEl(), close).click();
    await settle();
    expect(editorEl().hidden).toBe(true);
    expect(document.activeElement).toBe(expected === "the +" ? plus(opener) : editButton(name));
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(close === "Save" ? 1 : 0);
  });

  it("saves a new entry once the app accepts it, and changes nothing on Cancel", async () => {
    const stub = await openSeveral();
    await addServer("open_ai");
    type(control("Name", "input"), "Local Whisper");
    type(control("Address", "input"), "http://127.0.0.1:9000/v1");
    type(control("Model", "input"), "whisper-large");
    type(control("API key", "input"), "local-secret");
    choose(control("Request", "select"), "chat");
    type(control("Language", "input"), "de");
    await saveServer();
    expect(sent(stub).stt.entries.at(-1)).toMatchObject({ kind: "open_ai", name: "Local Whisper", active: false, api_key: "local-secret", request: "chat", language: "de" });
    expect(sent(stub).stt.entries.at(-1)).not.toHaveProperty("extra");
    expect(names(listOf("open_ai"))).toEqual(["Local Whisper"]);
    expect(entryNamed("Local Whisper").querySelector(".unit")?.textContent).toBe("127.0.0.1:9000 · key saved");
    edit("Local Whisper");
    type(control("Name", "input"), "Dropped");
    within(editorEl(), "Cancel").click();
    await settle();
    expect(names(listOf("open_ai"))).toEqual(["Local Whisper"]);
    expect(stub.applied()).toHaveLength(1);
  });

  it("clears a saved key on Save, keeping focus in the key field once Clear saved key hides", async () => {
    const stub = await openSeveral();
    edit("Cybrex2");
    await settle();
    const clear = within(editorEl(), "Clear saved key");
    clear.focus();
    clear.click();
    await settle();
    expect(clear.hidden).toBe(true);
    expect(document.activeElement).toBe(control("API key", "input"));
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    await saveServer();
    expect(sent(stub).stt.entries.find((entry) => entry.id === CYBREX.id)).toMatchObject({ api_key: "" });
  });

  it.each([
    ["a saved key", "Cybrex2", true],
    ["no key", "Meta", false],
  ] as const)("removes an entry with %s from its form, asking first only when a key would be lost", async (_case, name, asks) => {
    const stub = await openSeveral();
    edit(name);
    await settle();
    within(editorEl(), "Remove").click();
    await settle();
    if (asks) {
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      expect(editorEl().querySelector('[role="group"]')?.getAttribute("aria-label")).toBe(`Remove ${name} and its saved key?`);
      within(editorEl(), "Yes, remove").click();
      await settle();
    }
    expect(sent(stub).stt.entries.map((entry) => entry.name)).not.toContain(name);
    expect(editorEl().hidden).toBe(true);
  });

  it("disables Remove on the live entry and says why on screen", async () => {
    await openSeveral();
    edit("Realtime speech server");
    await settle();
    const remove = within(editorEl(), "Remove");
    expect(remove.disabled).toBe(true);
    const reason = document.getElementById(remove.getAttribute("aria-describedby") ?? "");
    expect(reason?.textContent).toBe("Choose another Live recognizer first.");
    expect(reason?.closest(".editor")).toBe(editorEl());
    expect(reason?.hidden).toBe(false);
  });

  it.each([
    ["the next row's Edit", "Cybrex2", [{ ...SERVER }, { ...CYBREX, has_key: false }, { ...CYBREX, id: "third", name: "Third", has_key: false }], "Edit Third"],
    ["the group's + when it is now empty", "Meta", [{ ...SERVER }, { ...META }], "Add an extra-only server"],
  ] as const)("moves focus after Remove to %s", async (_case, name, entries, expected) => {
    await open({ ...several(), stt: { entries: [...entries] } });
    goTo("recognition");
    edit(name);
    await settle();
    within(editorEl(), "Remove").focus();
    within(editorEl(), "Remove").click();
    await settle();
    expect(document.activeElement?.getAttribute("aria-label")).toBe(expected);
  });
});

describe("Extra switches", () => {
  async function openSeveral(settings: Settings = several()) {
    const stub = await open(settings);
    goTo("recognition");
    return stub;
  }
  const withExtras = (writing: Settings["writing"], extras: readonly string[] = []): Settings => ({
    ...several(),
    writing,
    stt: { entries: several().stt.entries.map((entry) => (extras.includes(entry.name) ? { ...entry, extra: true } : entry)) },
  });
  const savedEntry = (stub: Awaited<ReturnType<typeof open>>, name: string): SpeechEntry | undefined =>
    stub.applied().at(-1)?.stt.entries.find((entry) => entry.name === name);

  it.each([
    ["an extra-only server", "Meta", { active: true }, { active: false }],
    ["a streaming server not in use", "Cybrex2", { active: false, extra: true }, { active: false }],
    ["the on-device model not in use", "On this computer", { active: false, extra: true }, { active: false }],
  ] as const)("turns %s on and off at once, changing only its own flag, and keeps focus on the switch", async (_case, name, on, off) => {
    const stub = await openSeveral();
    const others = (settings: Settings | undefined) => settings?.stt.entries.filter((entry) => entry.name !== name);
    tickOf(name).focus();
    flip(tickOf(name));
    await settle();
    expect(savedEntry(stub, name)).toMatchObject(on);
    expect(others(stub.applied().at(-1))).toEqual(several().stt.entries.filter((entry) => entry.name !== name));
    expect(document.activeElement).toBe(tickOf(name));
    flip(tickOf(name));
    await settle();
    expect(savedEntry(stub, name)).toMatchObject(off);
    expect(savedEntry(stub, name)).not.toHaveProperty("extra");
    expect(others(stub.applied().at(-1))).toEqual(several().stt.entries.filter((entry) => entry.name !== name));
  });

  it("shows a switch that was refused as still off", async () => {
    const stub = await openSeveral();
    stub.backend.saveSettings.mockRejectedValueOnce("The settings could not be saved.");
    flip(tickOf("Meta"));
    await settle();
    expect(tickOf("Meta").checked).toBe(false);
  });

  it.each([
    ["no chat endpoint and none on", null, [], [false, false], "Extra: none"],
    ["a chat endpoint and none on", WRITING, [], [false, false], "Extra: none"],
    ["one on", WRITING, [], [true, false], "Extra: OpenAI. The chat model merges its text with the live text."],
    ["several on", WRITING, [], [true, true], "Extra: OpenAI, Meta. The chat model merges their texts with the live text."],
    ["an on-device one on", WRITING, ["On this computer"], [false, false], "Extra: On this computer. The chat model merges its text with the live text. Each on-device extra loads its own model, which adds memory and processing."],
  ] as const)("sums up the extras with %s", async (_case, writing, live, [openai, meta], text) => {
    await openSeveral({ ...withExtras(writing, live), stt: { entries: withExtras(writing, live).stt.entries.map((entry) => (entry.id === "openai" ? { ...entry, active: openai } : entry.id === "meta" ? { ...entry, active: meta } : entry)) } });
    expect(extrasText()).toBe(text);
  });

  it.each([
    ["no chat endpoint", null, true],
    ["a chat endpoint", WRITING, false],
  ] as const)("with %s, locks every Extra switch that is off and points it to the one line that says why", async (_case, writing, locked) => {
    await openSeveral(withExtras(writing, ["On this computer"]));
    expect(needsChat().hidden).toBe(!locked);
    expect(needsChat().textContent).toBe("Extra switches need a chat model. Go to the endpoint setting");
    expect(recognition().querySelectorAll(".needs-chat")).toHaveLength(1);
    expect(recognition().querySelector(".note.why")).toBeNull();
    for (const name of ["Cybrex2", "Meta", "OpenAI", "On this computer"]) {
      const offAndLocked = locked && !tickOf(name).checked;
      expect([name, tickOf(name).disabled, tickOf(name).getAttribute("aria-describedby")]).toEqual([name, offAndLocked, offAndLocked ? needsChat().id : null]);
    }
  });

  it("lets an Extra that is on be turned off without a chat endpoint", async () => {
    const stub = await openSeveral(withExtras(null, ["On this computer"]));
    flip(tickOf("On this computer"));
    await settle();
    expect(savedEntry(stub, "On this computer")).not.toHaveProperty("extra");
    expect(tickOf("On this computer").disabled).toBe(true);
  });

  it("follows the chat endpoint right away, and leads to it", async () => {
    await openSeveral(withExtras(null));
    expect(tickOf("Meta").disabled).toBe(true);
    button("Go to the endpoint setting").click();
    expect(root.querySelector('nav [aria-current="page"]')?.getAttribute("data-page")).toBe("chat");
    expect(document.activeElement).toBe(control("Writing endpoint address", "input"));
    type(control("Writing endpoint address", "input"), "http://127.0.0.1:8000/v1");
    type(control("Writing model", "input"), "glimmer");
    await settle();
    expect(needsChat().hidden).toBe(true);
    expect(tickOf("Meta").disabled).toBe(false);
  });

  it.each([["open", false], ["collapsed", true]] as const)("leads to the endpoint address with its details %s", async (_case, collapse) => {
    await open({ ...several(), writing: null });
    goTo("chat");
    const fold = rowOf("Writing endpoint address").closest("details.fold");
    if (!(fold instanceof HTMLDetailsElement)) throw new Error("no details around the endpoint address");
    if (collapse) fold.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(fold.open).toBe(!collapse);
    goTo("recognition");
    button("Go to the endpoint setting").click();
    expect(fold.open).toBe(true);
    expect(document.activeElement).toBe(control("Writing endpoint address", "input"));
  });

  it("gives the entry that becomes Live no switch, clears its own extra, and gives the one replaced a switch that is off", async () => {
    const stub = await openSeveral(withExtras(WRITING, ["Cybrex2"]));
    expect(roleOf("Cybrex2")).toBe("Extra on");
    use("Cybrex2");
    await settle();
    expect(savedEntry(stub, "Cybrex2")).toMatchObject({ active: true });
    expect(savedEntry(stub, "Cybrex2")).not.toHaveProperty("extra");
    expect([roleOf("Cybrex2"), roleOf("Realtime speech server")]).toEqual(["Live", "Extra off"]);
    expect(extrasText()).toBe("Extra: OpenAI. The chat model merges its text with the live text.");
  });
});

/** High risk: #558. An extra switched on by mistake sends the user's audio to another server unnoticed. */
describe("roles on the Recognition page against the fake backend", () => {
  type Step =
    | { kind: "live"; at: number }
    | { kind: "extra"; at: number }
    | { kind: "add"; entry: SpeechEntry["kind"] }
    | { kind: "remove"; at: number };
  const step: fc.Arbitrary<Step> = fc.oneof(
    fc.record({ kind: fc.constant("live" as const), at: fc.nat(9) }),
    fc.record({ kind: fc.constant("extra" as const), at: fc.nat(9) }),
    fc.record({ kind: fc.constant("add" as const), entry: fc.constantFrom<SpeechEntry["kind"]>("realtime", "on_device", "extra") }),
    fc.record({ kind: fc.constant("remove" as const), at: fc.nat(9) }),
  );
  const flags = (entry: SpeechEntry): [boolean, boolean] => [entry.active, entry.kind !== "extra" && entry.extra === true];
  const byId = (settings: Settings): Map<string, SpeechEntry> => new Map(settings.stt.entries.map((entry) => [entry.id, entry]));
  const START: Settings = { ...SETTINGS, writing: WRITING, stt: { entries: [{ ...SERVER }, { ...CYBREX, has_key: false }, device(undefined, false), { ...META }] } };

  async function act(backend: ReturnType<typeof fakeBackend>, next: Step): Promise<string | null> {
    const rows = Array.from(recognition().querySelectorAll<HTMLElement>(".trow.server"));
    const picked = rows[("at" in next ? next.at : 0) % Math.max(rows.length, 1)];
    const id = picked?.dataset["id"] ?? null;
    switch (next.kind) {
      case "live": {
        const live = Array.from(liveSelect().options);
        const option = live[next.at % live.length] as HTMLOptionElement;
        choose(liveSelect(), option.value);
        await settle();
        return option.value;
      }
      case "extra": {
        const tick = picked?.querySelector<HTMLInputElement>("input[type=checkbox]");
        if (!tick || tick.disabled) return null;
        flip(tick);
        await settle();
        return id;
      }
      case "add": {
        await addServer(next.entry);
        if (next.entry === "realtime") type(control("Address", "input"), "ws://127.0.0.1:8013");
        if (next.entry !== "on_device") type(control("Model", "input"), "m");
        if (next.entry === "extra") type(control("Address", "input"), "http://127.0.0.1:9000/v1");
        await saveServer();
        await settle();
        const ids = (await backend.getSettings()).stt.entries.map((entry) => entry.id);
        return ids.at(-1) ?? null;
      }
      case "remove": {
        if (id === null) return null;
        edit(picked?.querySelector(".phrase")?.textContent ?? "");
        await settle();
        const remove = within(editorEl(), "Remove");
        if (remove.disabled) {
          within(editorEl(), "Cancel").click();
          return null;
        }
        remove.click();
        await settle();
        const sure = Array.from(editorEl().querySelectorAll("button")).find((node) => node.textContent === "Yes, remove" && !node.closest("[hidden]"));
        sure?.click();
        await settle();
        return id;
      }
    }
  }

  it("changes only the chosen entry's flags, turns nothing else on, and keeps exactly one live entry", async () => {
    vi.useFakeTimers();
    await fc.assert(
      fc.asyncProperty(fc.array(step, { minLength: 1, maxLength: 8 }), async (steps) => {
        document.body.innerHTML = '<div id="settings"></div>';
        root = document.getElementById("settings") as HTMLElement;
        const backend = fakeBackend(new URLSearchParams("at=0"));
        await backend.saveSettings(settingsPatch(await backend.getSettings(), START));
        mountSettings(root, backend);
        await settle();
        goTo("recognition");
        for (const next of steps) {
          const before = byId(await backend.getSettings());
          const target = await act(backend, next);
          const after = await backend.getSettings();
          const live = after.stt.entries.filter((entry) => entry.kind !== "extra" && entry.active);
          expect(live, JSON.stringify(steps)).toHaveLength(1);
          for (const entry of after.stt.entries) {
            const was = before.get(entry.id);
            if (was === undefined) {
              expect([next.kind, entry.id, flags(entry)]).toEqual(["add", target, [false, false]]);
              continue;
            }
            if (entry.id === target && next.kind === "live") {
              expect(flags(entry)).toEqual([true, false]);
              continue;
            }
            if (entry.id === target && next.kind === "extra") {
              expect(flags(entry)).toEqual(entry.kind === "extra" ? [!was.active, false] : [false, !flags(was)[1]]);
              continue;
            }
            if (next.kind === "live" && entry.kind !== "extra") {
              expect(flags(entry)).toEqual([false, flags(was)[1]]);
              continue;
            }
            expect(flags(entry), JSON.stringify(steps)).toEqual(flags(was));
          }
          if (next.kind === "remove" && target !== null) expect(after.stt.entries.some((entry) => entry.id === target)).toBe(false);
        }
      }),
      { numRuns: 40, seed: 558 },
    );
  });
});

describe("the fake backend's rule for extra recognizers", () => {
  const writing = { url: "http://127.0.0.1:8000/v1", model: "glimmer" };
  /** Sends the patch from what the backend holds to `next`, as a window would. */
  const put = async (backend: ReturnType<typeof fakeBackend>, next: Settings): Promise<void> => {
    await backend.saveSettings(settingsPatch(await backend.getSettings(), next));
  };
  const settings = (endpoint: Settings["writing"], on: boolean): Settings => ({ ...SETTINGS, writing: endpoint, stt: { entries: [{ ...SERVER }, { ...OPENAI, active: on }] } });

  it.each([
    ["no writing model was set", null, "Extra recognizers need a writing model. Set one up under Settings → Chat model → Writing first, or switch the extras off."],
    ["the writing model is removed", writing, "Switch off the extra recognizers before removing the writing model."],
  ] as const)("refuses a switched-on extra when %s", async (_case, before, message) => {
    const backend = fakeBackend(new URLSearchParams());
    await put(backend, settings(before, before !== null));
    await expect(put(backend, settings(null, true))).rejects.toThrow(message);
    await expect(put(backend, settings(null, false))).resolves.toBeUndefined();
  });

  it.each([
    ["a live extra needs a writing model", null, { ...SERVER }, { ...CYBREX, extra: true }, "Extra recognizers need a writing model. Set one up under Settings → Chat model → Writing first, or switch the extras off."],
    ["the entry in use cannot be an extra", writing, { ...SERVER, extra: true }, { ...CYBREX }, 'Speech server "Realtime speech server" is the one in use, so it cannot also be an extra recognizer. Switch it off as an extra first.'],
  ] as const)("refuses a live entry's extra: %s", async (_case, endpoint, first, second, message) => {
    const backend = fakeBackend(new URLSearchParams());
    await expect(put(backend, { ...SETTINGS, writing: endpoint, stt: { entries: [first, second] } })).rejects.toThrow(message);
    await expect(put(backend, { ...SETTINGS, writing, stt: { entries: [{ ...SERVER }, { ...CYBREX, extra: true }] } })).resolves.toBeUndefined();
  });
});

describe("the fake backend's address check", () => {
  it.each([
    ["realtime", { ...SERVER, address: "wss://asr.example" }, { ...SERVER, address: "https://asr.example" }, "ws:// or wss://"],
    ["open_ai", { ...GATEWAY, address: "http://127.0.0.1:9000/v1" }, { ...GATEWAY, address: "ws://127.0.0.1:9000/v1" }, "http:// or https://"],
    ["extra", { ...META, address: "http://127.0.0.1:9000/v1" }, { ...META, address: "wss://meta.example/v1" }, "http:// or https://"],
  ] as const)("accepts the addresses the desktop takes for kind %s and refuses another", async (_kind, good, bad, wanted) => {
    const backend = fakeBackend(new URLSearchParams());
    await expect(backend.checkSpeechEntry(good)).resolves.toBeUndefined();
    await expect(backend.checkSpeechEntry(bad)).rejects.toThrow(`the address must start with ${wanted}.`);
  });
});

describe("the Recognition page against the fake backend", () => {
  it("never calls a streaming server Meta realtime, in the form or in the saved row", async () => {
    mountSettings(root, fakeBackend(new URLSearchParams()));
    await settle();
    goTo("recognition");
    const shown = (): string => [recognition().textContent, ...Array.from(recognition().querySelectorAll("input"), (input) => input.value)].join("\n");
    await addServer("realtime");
    expect(shown()).not.toContain("Meta realtime");
    type(control("Address", "input"), "ws://127.0.0.1:8012");
    type(control("Model", "input"), "nemotron");
    await saveServer();
    expect(names(listOf("realtime"))).toContain("Realtime transcription (WebSocket) 2");
    expect(shown()).not.toContain("Meta realtime");
  });
});

describe("the attached speech editor", () => {
  async function openSeveral() {
    const stub = await open(several());
    goTo("recognition");
    return stub;
  }

  it("keeps the focused editor field, caret and draft untouched by status and download refreshes", async () => {
    const stub = await openSeveral();
    edit("On this computer");
    await settle();
    const name = editorEl().querySelector("input") as HTMLInputElement;
    name.focus();
    typeIn(name, "My server");
    name.setSelectionRange(3, 3);
    const events = vi.fn();
    for (const event of ["blur", "focus", "change"]) name.addEventListener(event, events);
    const saves = stub.backend.saveSettings.mock.calls.length;

    for (const trigger of [
      () => { stub.emit({ state: "paused" }); },
      () => { stub.speechProgress({ name: "nemotron_160ms", percent: 100, status: "done", message: null, causes: [] }); },
    ]) {
      trigger();
      await settle();
      expect.soft(document.activeElement).toBe(name);
      expect.soft(name.value).toBe("My server");
      expect.soft([name.selectionStart, name.selectionEnd]).toEqual([3, 3]);
      expect.soft(events).not.toHaveBeenCalled();
      expect.soft(stub.backend.saveSettings).toHaveBeenCalledTimes(saves);
    }
  });

  it.each(["row button", "search box", "body"] as const)("keeps focus on the %s after a status refresh", async (where) => {
    const stub = await openSeveral();
    const focused = where === "row button" ? editButton("On this computer")
      : where === "search box" ? root.querySelector<HTMLInputElement>('input[aria-label="Search settings"]') as HTMLInputElement
      : document.body;
    focused.focus();
    expect(document.activeElement).toBe(focused);
    stub.emit({ state: "paused" });
    await settle();
    expect(document.activeElement).toBe(where === "row button" ? editButton("On this computer") : focused);
  });
});

describe("the next dictation summary and state words", () => {

  it.each([
    ["listening", { state: "listening" }, "Ready"],
    ["starting", { state: "starting", target: null }, "Ready"],
    ["capturing", { state: "capturing", target: null, committed: "", partial: null, notice: null }, "Ready"],
    ["pending", { state: "pending", target: null, route: "paste", text: "hi", waitingForQuiet: false, pauseMs: 0, copies: false }, "Ready"],
    ["writing", { state: "writing", target: null, text: "" }, "Ready"],
    ["thinking", { state: "thinking", target: null }, "Ready"],
    ["merging", { state: "merging", target: null, route: "paste" }, "Merging…"],
    ["working", { state: "working", target: null, route: "paste", cancellable: true }, "Ready"],
    ["done", { state: "done", target: null, route: "paste", notice: "Pasted" }, "Ready"],
    ["loading", { state: "loading", notice: "Loading the speech model." }, "Loading the speech model."],
    ["paused", { state: "paused" }, "Paused — resume to dictate"],
    ["failed", { state: "failed", notice: "n", headline: "Can't reach speech server — Settings", causes: [], kind: "other" }, "Can't reach speech server — Settings"],
  ] as const)("shows %s as the summary state pushed through onStatus", async (_name, status, state) => {
    const stub = await open(several());
    goTo("recognition");
    await settle();
    stub.emit(status as never);
    await settle();
    expect(summaryText()).toBe(`Next dictation: Realtime speech server · text arrives as you speak · ${state}`);
  });

  it("shows the on-device model title and follows the entry once its edit is applied", async () => {
    const stub = await open(local());
    goTo("recognition");
    await settle();
    expect(summaryText()).toBe("Next dictation: On this computer · Nemotron Streaming · Ready");
    await editLocal();
    choose(control("Speech model", "select"), "unified_160ms");
    expect(summaryText()).toBe("Next dictation: On this computer · Nemotron Streaming · Ready");
    await saveServer();
    expect(summaryText()).toBe("Next dictation: On this computer · Nemotron Unified · Ready");
    expect(stub.backend.saveSettings).toHaveBeenCalled();
  });

  describe("the summary's actions while the recognizer is being prepared", () => {
    const LOADING: Status = { state: "loading", notice: "Downloading the speech model — 42%" };
    const FAILED: Status = { state: "failed", notice: "n", headline: "Download failed", causes: [], kind: "other" };
    const actions = (): string[] => {
      const row = root.querySelector(".next-dictation + div");
      return row === null || (row as HTMLElement).hidden
        ? []
        : Array.from(row.querySelectorAll("button"))
            .filter((candidate) => !candidate.hidden)
            .map((candidate) => candidate.textContent);
    };

    it.each([
      ["on this computer, loading", local, LOADING, ["Use a speech server instead"]],
      ["on this computer, failed", local, FAILED, ["Use a speech server instead", "Retry"]],
      ["your own model folder, failed", () => local("/models/mine"), FAILED, ["Use a speech server instead"]],
      ["on this computer, listening", local, { state: "listening" }, []],
      ["on this computer, paused", local, { state: "paused" }, []],
      ["a server, loading", several, LOADING, []],
      ["a server, failed", several, FAILED, []],
    ] as [string, () => Settings, Status, string[]][])("offers the right actions for %s", async (_name, settings, status, expected) => {
      const stub = await open(settings());
      goTo("recognition");
      await settle();
      stub.emit(status);
      await settle();
      expect(actions()).toEqual(expected);
    });

    it("opens the add editor for Use a speech server instead and switches nothing", async () => {
      const stub = await open(local());
      goTo("recognition");
      stub.emit(LOADING);
      await settle();
      button("Use a speech server instead").click();
      expect(editorEl().hidden).toBe(false);
      expect(editorEl().parentElement).toBe(listOf("realtime"));
      expect(editorEl().querySelector("h4")?.textContent).toBe("Add a streaming server");
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      expect(stub.backend.downloadSpeechModel).not.toHaveBeenCalled();
    });

    it("retries through the model download alone, or says why it failed again", async () => {
      const stub = await open(local());
      goTo("recognition");
      stub.emit(FAILED);
      await settle();
      button("Retry").click();
      await settle();
      expect(stub.backend.downloadSpeechModel).toHaveBeenCalledExactlyOnceWith("nemotron_160ms");
      expect(stub.backend.setListening).not.toHaveBeenCalled();

      stub.backend.downloadSpeechModel.mockRejectedValueOnce(new Error("The download of Nemotron Streaming failed: offline."));
      button("Retry").click();
      await settle();
      expect(root.querySelector(".next-dictation + div .note")?.textContent).toBe("The download of Nemotron Streaming failed: offline.");
      expect(stub.backend.setListening).not.toHaveBeenCalled();
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    });
  });

  it("follows Use to a server entry", async () => {
    const stub = await open(several());
    goTo("recognition");
    use("Cybrex2");
    await settle();
    expect(summaryText()).toBe("Next dictation: Cybrex2 · text arrives as you speak · Ready");
    expect(stub.backend.saveSettings).toHaveBeenCalled();
  });

  it("labels the model filter and collapses model details", async () => {
    await open(local());
    goTo("recognition");
    await editLocal();
    expect(rowOf("Show models for").textContent).toContain("Filters this list by each card's languages.");
    expect(control("Show models for", "select").getAttribute("aria-label")).toBe("Show models for");
    const cardEl = rowOf("Speech model").querySelector<HTMLElement>(".model:not([hidden])") as HTMLElement;
    const cardBadges = Array.from(cardEl.querySelectorAll(".badge")).map((badge) => badge.textContent);
    expect(cardBadges).toContain("Selected");
    expect(cardBadges).toContain("Not downloaded");
    expect(cardEl.textContent).toContain("Nemotron Streaming");
    const details = cardEl.querySelector("details") as HTMLDetailsElement;
    expect(details.open).toBe(false);
    expect(details.querySelector("summary")?.textContent).toBe("Details");
    expect(details.textContent).toContain("nemotron_160ms");
    expect(details.textContent).toContain("NVIDIA Open Model License");
    expect(details.textContent).toContain(NEMOTRON.source);
  });
});

describe("the speech model catalog", () => {
  async function openCatalog(settings: Settings = local()) {
    const stub = await open(settings);
    goTo("recognition");
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
  const languages = (): HTMLSelectElement => control("Show models for", "select");
  const listed = (select: HTMLSelectElement): (string | null)[][] =>
    Array.from(select.options).map((option) => [option.value, option.textContent]);
  const OWN = ["__folder", "Your own model folder"];
  const STREAMING = [NEMOTRON.name, "Nemotron Streaming (English, 662 MB)"];
  const UNIFIED_OPTION = [UNIFIED.name, "Nemotron Unified (25 languages, 647 MB)"];

  it("offers each model and an own folder in labelled drop-downs, with the default picked", async () => {
    await openCatalog();
    expect(languages().getAttribute("aria-label")).toBe("Show models for");
    expect(rowOf("Show models for").querySelector("label")?.getAttribute("for")).toBe(languages().id);
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
    goTo("recognition");
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

  it("moves the selection to the first model of a language the selected one lacks, applied with the editor", async () => {
    const stub = await openCatalog();
    choose(languages(), "Dutch");
    expect(picker().value).toBe("unified_160ms");
    expect(card().querySelector("details")?.textContent).toContain("OpenMDW-1.1");
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    await saveServer();
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
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
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
    goTo("recognition");
    await editLocal();
    expect(listed(languages())).toEqual([["", "All languages"]]);
    expect(listed(picker())).toEqual([OWN]);
    choose(picker(), "__folder");
    type(control("Speech model", "input"), "/models/mine");
    await saveServer();
    await settle();
    await settle();
    expect(deviceOf(sent(stub)).model).toBe("/models/mine");
  });

  it("shows the picked model's description, size, license, languages, state and source", async () => {
    await openCatalog();
    expect(rowOf("Speech model").querySelectorAll(".model:not([hidden])")).toHaveLength(1);
    expect(card().textContent).toContain(NEMOTRON.description);
    expect(text(".facts")).toBe("Languages: English.");
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
    goTo("recognition");
    await editLocal();
    expect(text(".unit")).toBe(shown);
  });

  it("marks a downloaded model and offers no download", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...NEMOTRON, cached: true }]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    await editLocal();
    expect(text(".badge")).toBe("Downloaded");
    expect(download().hidden).toBe(true);
  });

  it("names the hardware an on-device entry runs on in its row, and for a folder of the user's own says nothing", async () => {
    const stub = stubBackend(local());
    stub.backend.speechDevice.mockResolvedValue("Runs on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)");
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    const row = (): HTMLElement | null => root.querySelector('.table[data-kind="on_device"] .trow.server');
    expect(stub.backend.speechDevice).toHaveBeenCalledWith("nemotron_160ms");
    expect(row()?.querySelector(".unit")?.textContent).toBe("Nemotron Streaming");
    expect(row()?.querySelector(".device")?.textContent).toBe("Runs on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)");

    // The model loads behind the window: when the app's state changes, the row asks again.
    stub.backend.speechDevice.mockResolvedValue("Runs on the CPU: no usable GPU");
    stub.emit({ state: "loading", notice: "Loading the speech model." });
    stub.emit({ state: "listening" });
    await settle();
    expect(row()?.querySelector(".device")?.textContent).toBe("Runs on the CPU: no usable GPU");

    // A download finishes without a state change, so its end makes the row ask again too.
    stub.backend.speechDevice.mockResolvedValue("Runs on the CPU. The GPU model is downloaded now and is used the next time the speech model loads.");
    stub.speechProgress({ name: "nemotron_160ms", percent: 100, status: "done", message: null, causes: [] });
    await settle();
    expect(row()?.querySelector(".device")?.textContent).toBe("Runs on the CPU. The GPU model is downloaded now and is used the next time the speech model loads.");

    root.replaceChildren();
    const folder = stubBackend(local("/models/mine"));
    mountSettings(root, folder.backend);
    await settle();
    goTo("recognition");
    expect(folder.backend.speechDevice).not.toHaveBeenCalled();
    expect(row()?.querySelector(".device")).toBeNull();
  });

  it("names the hardware each picked model runs on and asks again once its download is done", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...NEMOTRON }]);
    stub.backend.speechDevice.mockResolvedValue("Will run on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)");
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    await editLocal();
    expect(stub.backend.speechDevice).toHaveBeenCalledWith("nemotron_160ms");
    expect(text(".device")).toBe("Will run on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)");

    stub.backend.speechDevice.mockResolvedValue("Runs on the CPU: no usable GPU");
    download().click();
    await settle();
    expect(text(".device")).toBe("Runs on the CPU: no usable GPU");
  });

  it("offers the GPU model's download, with its size, to a model that runs on the CPU because it is missing, and says what the next load does after", async () => {
    const stub = stubBackend(local("unified_160ms"));
    stub.backend.listSpeechModels.mockResolvedValueOnce([{ ...UNIFIED, size: 1_287_000_000 }]);
    stub.backend.speechDevice.mockResolvedValue("Runs on the CPU: the GPU model isn't downloaded");
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    await editLocal();
    expect(text(".unit")).toBe("1.3 GB");
    expect(text(".badge")).toBe("Not downloaded");
    expect(text(".device")).toBe("Runs on the CPU: the GPU model isn't downloaded");
    expect(download().hidden).toBe(false);

    stub.backend.speechDevice.mockResolvedValue("Runs on the CPU. The GPU model is downloaded now and is used the next time the speech model loads.");
    download().click();
    await settle();
    expect(stub.backend.downloadSpeechModel).toHaveBeenCalledWith("unified_160ms");
    expect(text(".badge")).toBe("Downloaded");
    expect(download().hidden).toBe(true);
    expect(text(".device")).toBe("Runs on the CPU. The GPU model is downloaded now and is used the next time the speech model loads.");
  });

  it("shows the download's progress and flips the badge when it is done", async () => {
    const stub = await openCatalog();
    let finish: () => void = () => undefined;
    stub.backend.downloadSpeechModel.mockReturnValueOnce(new Promise<void>((resolve) => { finish = resolve; }));
    download().click();
    expect(stub.backend.downloadSpeechModel).toHaveBeenCalledWith("nemotron_160ms");
    expect(download().getAttribute("aria-disabled")).toBe("true");
    expect(text("[role=status]")).toBe("Checking files…");

    stub.speechProgress({ name: "nemotron_160ms", percent: 40, status: "downloading", message: null, causes: [] });
    expect(text("[role=status]")).toBe("Downloading… 40%");
    expect(meter().value).toBe(40);
    expect(meter().hidden).toBe(false);
    expect(text(".badge")).toBe("Not downloaded");

    stub.speechProgress({ name: "nemotron_160ms", percent: 100, status: "done", message: null, causes: [] });
    finish();
    await settle();
    expect(text(".badge")).toBe("Downloaded");
    expect(meter().hidden).toBe(true);
    expect(text("[role=status]")).toBe("");
    expect(download().hidden).toBe(true);
  });

  it.each([
    ["stays on Download", "done", "the model picker"],
    ["moves to Details", "done", "Details"],
    ["stays on Download", "failed", "Download"],
  ] as const)("keeps focus on the busy Download; when the user %s and the download ends %s, focus is on %s", async (move, end, expected) => {
    const stub = await openCatalog();
    let finish: (failed: boolean) => void = () => undefined;
    stub.backend.downloadSpeechModel.mockReturnValueOnce(new Promise<void>((resolve, reject) => {
      finish = (failed) => { if (failed) reject(new Error("offline")); else resolve(); };
    }));
    const start = download();
    const folder = Array.from(rowOf("Speech model").querySelectorAll("summary")).find((summary) => summary.textContent === "Details · your own model folder");
    start.focus();
    start.click();
    stub.speechProgress({ name: "nemotron_160ms", percent: 40, status: "downloading", message: null, causes: [] });
    expect(document.activeElement).toBe(start);
    expect(start.getAttribute("aria-disabled")).toBe("true");
    start.click();
    expect(stub.backend.downloadSpeechModel).toHaveBeenCalledTimes(1);

    if (move === "moves to Details") folder?.focus();
    if (end === "done") stub.speechProgress({ name: "nemotron_160ms", percent: 100, status: "done", message: null, causes: [] });
    finish(end === "failed");
    await settle();
    expect(document.activeElement).toBe(expected === "the model picker" ? picker() : expected === "Details" ? folder : start);
    expect(start.hasAttribute("aria-disabled")).toBe(false);
  });

  it("says plainly why a download failed and lets the user try again", async () => {
    const stub = await openCatalog();
    stub.backend.downloadSpeechModel.mockRejectedValueOnce("Could not download the speech model encoder. Check the internet connection and try again.");
    download().click();
    await settle();
    expect(alert()).toBe("Could not download the speech model encoder. Check the internet connection and try again.");
    expect(text(".badge")).toBe("Not downloaded");
    expect(meter().hidden).toBe(true);
    expect(download().hasAttribute("aria-disabled")).toBe(false);

    download().click();
    expect(alert()).toBeUndefined();
    await settle();
    expect(text(".badge")).toBe("Downloaded");
  });

  it("copies the report of a failed download with its causes, and only after a failure", async () => {
    const stub = await openCatalog();
    const copy = (): HTMLButtonElement | undefined => [...card().querySelectorAll("button")].find((candidate) => candidate.textContent.startsWith("Cop"));
    expect(copy()?.hidden).toBe(true);

    const message = "Could not download the speech model encoder: could not connect to huggingface.co. Check the internet connection and try again.";
    stub.backend.downloadSpeechModel.mockRejectedValueOnce(new DownloadFailure(message, ["error sending request", "connection refused"]));
    download().click();
    await settle();
    expect(alert()).toBe(message);
    expect(copy()?.hidden).toBe(false);

    stub.backend.copyReport.mockRejectedValueOnce("No clipboard tool is installed.");
    copy()?.click();
    await settle();
    expect(card().textContent).toContain("Could not copy: No clipboard tool is installed.");
    expect(copy()?.textContent).toBe("Copy report");

    copy()?.click();
    await settle();
    expect(stub.backend.copyReport).toHaveBeenLastCalledWith(message, ["error sending request", "connection refused"]);
    expect(copy()?.textContent).toBe("Copied");

    download().click();
    expect(copy()?.hidden).toBe(true);
  });

  it("shows the reason from a failed event too", async () => {
    const stub = await openCatalog();
    stub.speechProgress({ name: "nemotron_160ms", percent: 0, status: "failed", message: "The download did not match its checksum.", causes: [] });
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
    goTo("recognition");
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
      `Languages: ${UNIFIED.languages.join(", ")}. The language is detected automatically.`,
    );
    expect(text(".unit")).toBe("647 MB");
    await saveServer();
    await settle();
    await settle();
    expect(stub.backend.validateSpeechModelFolder).not.toHaveBeenCalled();
    expect(deviceOf(sent(stub)).model).toBe("unified_160ms");
  });

  it("keeps showing the download of a model while another one is picked", async () => {
    const stub = await openCatalog();
    stub.speechProgress({ name: "unified_160ms", percent: 40, status: "downloading", message: null, causes: [] });
    expect(rowOf("Speech model").querySelector(".badge")?.textContent).toBe("Not downloaded");
    choose(picker(), "unified_160ms");
    expect(text("[role=status]")).toBe("Downloading… 40%");
    expect(meter().value).toBe(40);
  });

  it("shows a saved model, and a saved folder as the own-folder entry", async () => {
    await openCatalog(local("unified_160ms"));
    expect(picker().value).toBe("unified_160ms");
    expect(card().querySelector("details")?.textContent).toContain("OpenMDW-1.1");
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
  const reason = (label: string): string => rowOf(label).querySelector<HTMLElement>(".note:not([hidden])")?.textContent ?? "";

  it("applies the endpoint once its address and model are both set, with its key masked, without calling it", async () => {
    const stub = await open();
    goTo("chat");
    expect(sent_writing_fields()).toEqual(["", "", ""]);
    type(control("Writing endpoint address", "input"), endpoint.url);
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    expect(reason("Writing endpoint address")).toBe("The address and the model of the writing endpoint go together: fill in both.");
    type(control("Writing model", "input"), endpoint.model);
    type(control("Writing API key", "input"), endpoint.api_key);
    expect(control("Writing API key", "input").type).toBe("password");
    const calls = () => Object.entries(stub.backend).map(([name, call]) => [name, call.mock.calls.length]);
    const before = calls();
    await settle();
    expect(sent(stub).writing).toEqual(endpoint);
    const grew = calls()
      .filter(([, count], index) => count !== before[index]?.[1])
      .map(([name]) => name);
    expect(grew).toEqual(["saveSettings"]);
  });

  it("shows a saved endpoint and removes it once both its address and model are emptied", async () => {
    const stub = await open({ ...SETTINGS, writing: { url: endpoint.url, model: endpoint.model, has_key: true } });
    goTo("chat");
    expect(sent_writing_fields()).toEqual([endpoint.url, endpoint.model, ""]);
    expect(control("Writing API key", "input").placeholder).toBe("A key is saved");
    type(control("Writing endpoint address", "input"), "");
    await settle();
    expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    type(control("Writing model", "input"), "");
    await settle();
    expect(sent(stub).writing).toBeNull();
  });

  it("keeps the saved writing key unless its field is edited or it is cleared", async () => {
    const writing = { url: endpoint.url, model: endpoint.model, has_key: true };
    const stub = await open({ ...SETTINGS, writing });
    goTo("chat");
    type(control("Writing model", "input"), "updated");
    await settle();
    expect(sent(stub).writing).toEqual({ ...writing, model: "updated" });
    const clear = button("Clear saved writing key");
    clear.focus();
    clear.click();
    await settle();
    expect(stub.applied()[1]?.writing).toEqual({ ...writing, model: "updated", api_key: "" });
    expect(clear.hidden).toBe(true);
    expect(document.activeElement).toBe(control("Writing API key", "input"));
  });

  describe("prompt", () => {
    const fallback = SETTINGS.writing_default_prompt;
    const area = (): HTMLTextAreaElement => rowOf("Writing prompt").querySelector("textarea") as HTMLTextAreaElement;
    const restore = (): HTMLButtonElement => button("Restore default prompt");
    const saved = { url: endpoint.url, model: endpoint.model, has_key: true };

    it("shows the default, applies an edit with the endpoint, and restores the default", async () => {
      const stub = await open({ ...SETTINGS, writing: saved });
      goTo("recognition");
      expect(area().value).toBe(fallback);
      expect(restore().disabled).toBe(true);

      typeIn(area(), "Write short emails.\nNo greeting.");
      await settle();
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      area().dispatchEvent(new Event("change", { bubbles: true }));
      await settle();
      expect(restore().disabled).toBe(false);
      expect(sent(stub).writing).toEqual({ ...saved, prompt: "Write short emails.\nNo greeting." });

      restore().click();
      await settle();
      expect(area().value).toBe(fallback);
      expect(restore().disabled).toBe(true);
      expect(stub.applied()[1]?.writing).toEqual(saved);
    });

    it("shows a saved prompt when Settings is reopened and keeps it through other edits", async () => {
      const writing = { ...saved, prompt: "Be brief." };
      const stub = await open({ ...SETTINGS, writing });
      goTo("chat");
      expect(area().value).toBe("Be brief.");
      expect(restore().disabled).toBe(false);
      type(control("Writing model", "input"), "updated");
      await settle();
      expect(sent(stub).writing).toEqual({ ...writing, model: "updated" });
    });

    it("applies no prompt while the endpoint is empty", async () => {
      const stub = await open();
      goTo("chat");
      type(area(), "Be brief.");
      await settle();
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      expect(reason("Writing prompt")).toBe("Set the address and the model of the writing endpoint first.");
    });

    it("shows the app's refusal of a blank prompt under it", async () => {
      const withEndpoint = await open({ ...SETTINGS, writing: saved });
      goTo("chat");
      withEndpoint.backend.saveSettings.mockRejectedValue("writing.prompt must not be blank. Restore the default to start over.");
      type(area(), "  ");
      await settle();
      expect(reason("Writing prompt")).toBe("writing.prompt must not be blank. Restore the default to start over.");
      expect(area().value).toBe("  ");
    });
  });

  describe("clean up dictation", () => {
    const toggle = (): HTMLInputElement => control("Clean up dictation", "input");

    it("is disabled until a writing endpoint is set, then stores the switch only when on", async () => {
      const stub = await open();
      goTo("chat");
      expect(toggle().disabled).toBe(true);
      expect(rowOf("Clean up dictation").querySelector(".note.why:not([hidden])")?.textContent).toBe("Set a writing endpoint first");
      toggle().click();
      await settle();
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();

      type(control("Writing endpoint address", "input"), endpoint.url);
      type(control("Writing model", "input"), endpoint.model);
      await settle();
      expect(toggle().disabled).toBe(false);
      expect(rowOf("Clean up dictation").querySelector(".note.why:not([hidden])")).toBeNull();
      toggle().click();
      await settle();
      expect(sent(stub).writing).toMatchObject({ clean_dictation: true });

      toggle().click();
      await settle();
      expect(sent(stub).writing).not.toHaveProperty("clean_dictation");
    });
  });

  describe("opening phrases", () => {
    const saved = { url: endpoint.url, model: endpoint.model, has_key: true };
    const rows = (): HTMLElement => rowOf("Opening phrases");
    const addField = (): HTMLInputElement => rows().querySelector<HTMLInputElement>(".add input") as HTMLInputElement;
    const add = async (phrase: string): Promise<void> => {
      type(addField(), phrase);
      (rows().querySelector(".add button") as HTMLElement).click();
      await settle();
    };
    const notice = (): string => rows().querySelector(".note:not([hidden])")?.textContent ?? "";
    const phrases = (): (string | null)[] => Array.from(rows().querySelectorAll(".phrase:not(.add *)"), (node) => node.textContent);
    const promptOf = (phrase: string): HTMLTextAreaElement =>
      rows().querySelector(`textarea[aria-label="Prompt for “${phrase}”"]`) as HTMLTextAreaElement;

    it("adds a rule that starts as a copy of the Writing prompt and applies an edit of it", async () => {
      const stub = await open({ ...SETTINGS, writing: { ...saved, prompt: "Be brief." } });
      goTo("recognition");
      await add("  Write me an E-mail, ");
      expect(phrases()).toEqual(["“write me an e mail”"]);
      expect(promptOf("write me an e mail").value).toBe("Be brief.");
      expect(addField().value).toBe("");
      type(promptOf("write me an e mail"), "Greeting, body, sign-off.");
      await add("write a message");
      expect(promptOf("write a message").value).toBe("Be brief.");
      expect(sent(stub).writing).toEqual({
        ...saved,
        prompt: "Be brief.",
        openings: [
          { phrase: "write me an e mail", prompt: "Greeting, body, sign-off." },
          { phrase: "write a message", prompt: "Be brief." },
        ],
      });
    });

    it("shows saved rules, removes one, and sends no list once the last is gone", async () => {
      const openings = [
        { phrase: "write me an email", prompt: "Email." },
        { phrase: "write a message", prompt: "Message." },
      ];
      const stub = await open({ ...SETTINGS, writing: { ...saved, openings } });
      goTo("recognition");
      expect(phrases()).toEqual(["“write me an email”", "“write a message”"]);
      expect(promptOf("write a message").value).toBe("Message.");
      (rows().querySelector('button[aria-label="Remove “write me an email”"]') as HTMLElement).click();
      await settle();
      expect(sent(stub).writing).toEqual({ ...saved, openings: [openings[1]] });
      (rows().querySelector('button[aria-label="Remove “write a message”"]') as HTMLElement).click();
      await settle();
      expect(stub.applied()[1]?.writing).toEqual(saved);
    });

    it("refuses a phrase that is already there, ignoring case and punctuation, and an empty one", async () => {
      const stub = await open({ ...SETTINGS, writing: { ...saved, openings: [{ phrase: "write me an email", prompt: "Email." }] } });
      goTo("recognition");
      await add("Write me an email!");
      expect(notice()).toBe("“write me an email” is already in the list.");
      expect(addField().value).toBe("Write me an email!");
      expect(phrases()).toEqual(["“write me an email”"]);
      expect(promptOf("write me an email").value).toBe("Email.");
      await add(" ?! ");
      expect(notice()).toBe("Type the phrase to add first.");
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    });

    it("adds no rule while the endpoint is empty", async () => {
      const stub = await open();
      goTo("chat");
      await add("write me an email");
      expect(notice()).toBe("Set the writing endpoint first: the address and the model.");
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    });

    it("shows the app's refusal of a blank rule prompt under it", async () => {
      const withEndpoint = await open({ ...SETTINGS, writing: { ...saved, openings: [{ phrase: "write me an email", prompt: "Email." }] } });
      goTo("chat");
      withEndpoint.backend.saveSettings.mockRejectedValue("writing.openings has a blank prompt for “write me an email”.");
      type(promptOf("write me an email"), "  ");
      await settle();
      expect(notice()).toBe("writing.openings has a blank prompt for “write me an email”.");
      expect(promptOf("write me an email").value).toBe("  ");
    });
  });

  describe("app styles", () => {
    const saved = { url: endpoint.url, model: endpoint.model, has_key: true };
    const style = { app: "firefox", prompt: "Polite email." };
    const rows = (): HTMLElement => rowOf("App styles");
    const field = (label: string): HTMLInputElement | HTMLTextAreaElement =>
      rows().querySelector(`[aria-label="${label}"]`) as HTMLInputElement | HTMLTextAreaElement;
    const notice = (): string => rows().querySelector(".ctl > .stack > .note:not([hidden])")?.textContent ?? "";

    it.each([undefined, "Be brief."])("adds by name with a copy of the Writing prompt (%s)", async (prompt) => {
      const writing = { ...saved, ...(prompt === undefined ? {} : { prompt }) };
      const stub = await open({ ...SETTINGS, writing });
      goTo("chat");
      within(rows(), "Add by name").click();
      typeIn(field("New app"), "  Slack.exe  ");
      within(rows(), "Add").click();
      await settle();
      expect(sent(stub).writing).toEqual({ ...writing, apps: [{ app: "Slack.exe", prompt: prompt ?? SETTINGS.writing_default_prompt }] });
      expect(field("Prompt for app “Slack.exe”").value).toBe(prompt ?? SETTINGS.writing_default_prompt);
      expect(field("Title contains for “Slack.exe”").getAttribute("placeholder")).toBe("—");
      expect(field("New app").value).toBe("");
      expect(document.activeElement).toBe(within(rows(), "Add by name"));
      expect(rows().textContent).toContain("A prompt for the app you write into.");
    });

    it.each(["Gmail", ""])("offers the current app's title and lets it be cleared before adding (%s)", async (title) => {
      const stub = await open({ ...SETTINGS, writing: saved });
      stub.backend.getCurrentApp.mockResolvedValue({ app: "firefox", title: "Gmail" });
      goTo("chat");
      within(rows(), "Add the app you're in now").click();
      await settle();
      expect(stub.backend.getCurrentApp).toHaveBeenCalledOnce();
      expect(field("New app").value).toBe("firefox");
      expect(field("New title contains (optional)").value).toBe("Gmail");
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      typeIn(field("New title contains (optional)"), title);
      within(rows(), "Add").click();
      await settle();
      expect(sent(stub).writing?.apps).toEqual([{ app: "firefox", ...(title === "" ? {} : { title }), prompt: SETTINGS.writing_default_prompt }]);
    });

    it.each([
      ["App", undefined, "kitty", { app: "kitty" }],
      ["Prompt for app", undefined, "One-line commit message.", { prompt: "One-line commit message." }],
      ["Title contains", undefined, "  Gmail  ", { title: "Gmail" }],
      ["Title contains", "Gmail", "  ", {}],
    ] as const)("saves an edit of %s from title %s to %s, keeping the other style", async (label, title, value, changed) => {
      const apps = [{ ...style, ...(title === undefined ? {} : { title }) }, { ...style, title: "Work", prompt: "Work email." }];
      const stub = await open({ ...SETTINGS, writing: { ...saved, apps } });
      goTo("chat");
      type(field(`${label}${label === "Prompt for app" ? "" : " for"} “firefox”`), value);
      await settle();
      expect(sent(stub).writing?.apps).toEqual([{ ...style, ...changed }, apps[1]]);
    });

    it("removes only the chosen style and drops apps from settings when the last is gone", async () => {
      const apps = [style, { ...style, title: "Work", prompt: "Work email." }];
      const openings = [{ phrase: "write an email", prompt: "Email." }];
      const stub = await open({ ...SETTINGS, writing: { ...saved, apps, openings } });
      goTo("chat");
      const remove = (): HTMLButtonElement => rows().querySelector('button[aria-label="Remove app style for “firefox”"]') as HTMLButtonElement;
      remove().click();
      await settle();
      expect(sent(stub).writing).toEqual({ ...saved, apps: [apps[1]], openings });
      remove().click();
      await settle();
      expect(sent(stub).writing).toEqual({ ...saved, openings });
      expect(stub.backend.saveSettings).toHaveBeenLastCalledWith({ writing: { apps: null } });
      expect(rows().querySelector("textarea")).toBeNull();
    });

    it.each([
      [saved, "  ", "Type the app name to add first."],
      [null, "firefox", "Set the writing endpoint first: the address and the model."],
    ])("refuses an add with endpoint %s and app %s", async (writing, app, message) => {
      const stub = await open({ ...SETTINGS, writing });
      goTo("chat");
      within(rows(), "Add by name").click();
      typeIn(field("New app"), app);
      within(rows(), "Add").click();
      await settle();
      expect(notice()).toBe(message);
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      expect(field("New app").value).toBe(app);
    });

    it.each(["add", "prompt", "remove"])("shows a refused %s in the section and keeps the saved list", async (action) => {
      const stub = await open({ ...SETTINGS, writing: { ...saved, apps: [style] } });
      const refusal = "Thalvi could not save the app style: the config file is read-only.";
      stub.backend.saveSettings.mockRejectedValue(refusal);
      goTo("chat");
      if (action === "add") {
        within(rows(), "Add by name").click();
        typeIn(field("New app"), "kitty");
        within(rows(), "Add").click();
      } else if (action === "prompt") {
        type(field("Prompt for app “firefox”"), "Be brief.");
      } else {
        within(rows(), "Remove").click();
      }
      await settle();
      expect(notice()).toBe(refusal);
      expect(stub.applied()).toEqual([]);
      expect(field("App for “firefox”").value).toBe("firefox");
      expect(rows().querySelector(".applied")?.textContent ?? "").toBe("");
    });

    it.each([false, true])("explains an unknown current app or a lookup failure (failure %s)", async (failure) => {
      const stub = await open({ ...SETTINGS, writing: saved });
      if (failure) stub.backend.getCurrentApp.mockRejectedValue("Thalvi could not read the current app.");
      goTo("chat");
      within(rows(), "Add the app you're in now").click();
      await settle();
      expect(notice()).toBe(failure ? "Thalvi could not read the current app." : "Thalvi doesn't know which app you were in yet. Choose Add by name, or write into the app once and try again.");
      expect(field("New app").closest("[hidden]")).not.toBeNull();
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    });

    it("shows GNOME's reason and the existing app styles read-only", async () => {
      const apps = [style, { app: "kitty", title: "Thalvi", prompt: "One-line commit message." }];
      const stub = stubBackend({ ...SETTINGS, writing: { ...saved, apps } });
      const reason = "GNOME doesn't say which app you're in, so app styles don't apply here.";
      stub.backend.getAppStylesUnavailable.mockResolvedValue(reason);
      mountSettings(root, stub.backend);
      await settle();
      goTo("chat");
      expect(rows().textContent).toContain(reason);
      expect(rows().textContent).toContain("firefox");
      expect(rows().textContent).toContain("Title contains: —");
      expect(rows().textContent).toContain("Title contains: Thalvi");
      expect(Array.from(rows().querySelectorAll("textarea"), (area) => [area.value, area.readOnly])).toEqual(apps.map((app) => [app.prompt, true]));
      expect(rows().querySelector("input, button")).toBeNull();
      expect(stub.backend.getCurrentApp).not.toHaveBeenCalled();
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    });

    it.each([
      { apps: [style] },
      { openings: [{ phrase: "write an email", prompt: "Email." }] },
    ])("refuses to clear an endpoint that still has rules (%s)", async (rules) => {
      const stub = await open({ ...SETTINGS, writing: { ...saved, ...rules } });
      goTo("chat");
      type(control("Writing endpoint address", "input"), "");
      await settle();
      type(control("Writing model", "input"), "");
      await settle();
      expect(reason("Writing model")).toBe("Set the address and the model of the writing endpoint first.");
      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
    });
  });

  function sent_writing_fields(): string[] {
    return ["Writing endpoint address", "Writing model", "Writing API key"].map((label) => control(label, "input").value);
  }
});

describe("folded settings", () => {
  const foldOf = (label: string): HTMLDetailsElement => {
    const row = rowOf(label);
    const fold = row.closest("details.fold") ?? row.parentElement?.querySelector<HTMLDetailsElement>(":scope > details.fold");
    if (!(fold instanceof HTMLDetailsElement)) throw new Error(`no details around ${label}`);
    return fold;
  };
  const summaryOf = (label: string): string => foldOf(label).querySelector("summary")?.textContent ?? "";

  it.each([
    ["a default threshold stays closed with no summary", { ...SETTINGS, wake: { ...SETTINGS.wake, capture: { model: "hey_thalvi", threshold: 0.7 } } }, "Threshold (dictation)", false, "Details"],
    ["a changed threshold stays closed and names the value", { ...SETTINGS, wake: { ...SETTINGS.wake, capture: { model: "hey_thalvi", threshold: 0.6 } } }, "Threshold (dictation)", false, "Details · threshold 0.6"],
    ["a default capture limit stays closed", { ...SETTINGS }, "Stop after silence", false, "Details"],
    ["a changed capture limit stays closed and names it", { ...SETTINGS, capture_limits: { idle: 90, max: 300 } }, "Stop after silence", false, "Details · silence 90"],
    ["default quiet timings stay closed", { ...SETTINGS }, "Quiet before paste", false, "Details"],
    ["a changed quiet timing stays closed and names it", { ...SETTINGS, ending_pause: { paste: 1.2, submit: 2.5, task: 2.5 } }, "Quiet before paste", false, "Details · paste 1.2"],
  ])("%s", async (_name, settings, label, isOpen, summary) => {
    await open(settings);
    goTo(label.startsWith("Quiet") ? "commands" : "listening");
    const fold = foldOf(label);
    expect(fold.open).toBe(isOpen);
    expect(fold.querySelector("summary")?.textContent).toBe(summary);
    expect(rowOf(label).hidden).toBe(false);
  });

  it.each([
    [0.6000000238418579, "0.6"],
    [0.55, "0.55"],
    [0.7, "0.7"],
  ])("names a threshold of %s as %s, the way its slider shows it", async (value, shown) => {
    expect(shownNumber(value)).toBe(shown);
    await open({ ...SETTINGS, wake: { ...SETTINGS.wake, capture: { model: "hey_jarvis", threshold: value } } });
    goTo("listening");
    expect(summaryOf("Threshold (dictation)")).toBe(`Details · threshold ${shown}`);
  });

  it("shows a group whose rows all sit inside a fold", async () => {
    await open();
    const hide = settingsLooksCss.match(/\.grp:not\(:has\(.+?\)\)\)/)?.[0];
    if (hide === undefined) throw new Error("no group hiding rule");
    for (const title of ["Capture limits", "Quiet before acting"]) {
      const heading = Array.from(root.querySelectorAll("h4")).find((node) => node.textContent === title);
      const group = heading?.closest(".grp");
      if (!(group instanceof HTMLElement)) throw new Error(`no group ${title}`);
      expect(group.matches(hide), title).toBe(false);
    }
  });

  it("opens the details of a number the field itself refuses, so the invalid value stays visible", async () => {
    await open();
    goTo("listening");
    const fold = foldOf("Stop after silence");
    expect(fold.open).toBe(false);
    fold.open = true;
    type(control("Stop after silence", "input"), "5");
    await settle();
    expect(control("Stop after silence", "input").getAttribute("aria-invalid")).toBe("true");
    expect(fold.open).toBe(true);
    expect(rowOf("Stop after silence").querySelector(".note:not([hidden])")?.textContent).toBe("Enter a number from 10 to 600 s.");
  });

  it.each([
    ["an unset writing endpoint stays open", null, true, "Details"],
    ["a configured writing endpoint closes and names the address, never the key", { url: "http://127.0.0.1:8000/v1", model: "writer", has_key: true, api_key: "secret-value" }, false, "Details · http://127.0.0.1:8000/v1 · writer · key set"],
  ])("%s", async (_name, writing, isOpen, summary) => {
    await open({ ...SETTINGS, writing });
    goTo("chat");
    const fold = root.querySelector<HTMLDetailsElement>('details[data-fold="chat/writing/endpoint"]');
    if (!fold) throw new Error("no writing details");
    expect(fold.open).toBe(isOpen);
    expect(fold.querySelector("summary")?.textContent).toBe(summary);
    expect(fold.textContent).not.toContain("secret-value");
    expect(control("Writing API key", "input").value).toBe("");
  });

  it("folds an app's window classes and a script label, naming the class and the label", async () => {
    await open();
    goTo("commands");
    const app = root.querySelector<HTMLDetailsElement>('details[data-fold="commands/apps/slack/match"]');
    const script = root.querySelector<HTMLDetailsElement>('details[data-fold="commands/scripts/0/label"]');
    if (!app || !script) throw new Error("no app or script details");
    expect(app.open).toBe(false);
    expect(app.querySelector("summary")?.textContent).toBe("Details · slack · Slack");
    expect(script.open).toBe(false);
    expect(script.querySelector("summary")?.textContent).toBe("Details · docs");
  });

  it("keeps every hint inside one line at the narrow width", async () => {
    const stub = stubBackend(local());
    stub.backend.listSpeechModels.mockResolvedValue([
      { ...WHISPER, title: "Whisper Turbo · Vulkan GPU (experimental)" },
    ]);
    mountSettings(root, stub.backend);
    await settle();
    goTo("recognition");
    edit("On this computer");
    await settle();
    const hints = Array.from(root.querySelectorAll<HTMLElement>(".what small, .kind-help")).map((node) => node.textContent || "");
    const over = hints.filter((hint) => hint.length > HINT_BUDGET);
    expect(over).toEqual([]);
  });

  it("remembers a disclosure the user opened across a settings reload", async () => {
    const stub = await open();
    goTo("listening");
    const before = foldOf("Stop after silence");
    before.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(before.dataset["fold"]).toBe("listening/capture-limits/limits");
    await Promise.resolve();
    stub.changedElsewhere();
    await settle();
    goTo("listening");
    expect(foldOf("Stop after silence").open).toBe(true);
    expect(summaryOf("Stop after silence")).toBe("Details");
  });
});

describe("search", () => {
  it.each([
    ["listening", "Stop after silence"],
    ["recognition", "Word list"],
    ["commands", "Scripts"],
    ["chat", "Opening phrases"],
    ["overlay", "Show the draft"],
    ["history", "Recent dictations"],
    ["app", "Update channel"],
  ])("finds a setting of the %s page, and shows that page", async (page, label) => {
    await open();
    type(root.querySelector<HTMLInputElement>(".search") as HTMLInputElement, label);
    const visible = Array.from(root.querySelectorAll<HTMLElement>(".row")).filter((row) => !row.hidden && !row.closest("[hidden]"));
    expect(visible.map((row) => [row.closest("section")?.dataset["page"], row.dataset["label"]])).toEqual([[page, label.toLowerCase()]]);
  });

  it.each([
    ["speech server", ["Streaming servers", "After-you-stop servers", "Extra-only servers"]],
    ["extra recognizer", ["Extra-only servers"]],
    ["word list", ["Word list"]],
    ["live recognizer", ["Live recognizer"]],
    ["streaming servers", ["Streaming servers"]],
    ["after-you-stop servers", ["After-you-stop servers"]],
    ["on this computer", ["On this computer"]],
    ["extra-only servers", ["Extra-only servers"]],
  ])("finds the Recognition page by %s", async (term, titles) => {
    await open();
    type(root.querySelector<HTMLInputElement>(".search") as HTMLInputElement, term);
    const visible = Array.from(root.querySelectorAll<HTMLElement>(".row")).filter((row) => !row.hidden && !row.closest("[hidden]"));
    expect(visible.map((row) => [row.closest("section")?.dataset["page"], row.querySelector(".what > :first-child")?.textContent])).toEqual(titles.map((title) => ["recognition", title]));
  });

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
    expect(root.querySelector<HTMLElement>("section[data-page=recognition]")?.hidden).toBe(false);
    expect(root.querySelector<HTMLElement>("section[data-page=listening]")?.hidden).toBe(true);
  });

  it("finds a folded setting and a nested app field, opens their details, and closes only what the search opened", async () => {
    await open();
    const search = root.querySelector<HTMLInputElement>(".search") as HTMLInputElement;
    const limits = root.querySelector<HTMLDetailsElement>('details[data-fold="listening/capture-limits/limits"]');
    const app = root.querySelector<HTMLDetailsElement>('details[data-fold="commands/apps/slack/match"]');
    if (!limits || !app) throw new Error("missing details");
    expect(limits.open).toBe(false);

    type(search, "stop after silence");
    expect(limits.open).toBe(true);
    expect(limits.closest("section")?.hidden).toBe(false);
    expect(rowOf("Stop after silence").hidden).toBe(false);
    const limitsGroup = limits.closest(".grp");
    expect(limitsGroup?.matches(".grp:not(:has(.row:not([hidden])))")).toBe(false);

    type(search, "window classes");
    expect(app.open).toBe(true);
    expect(app.closest("section")?.hidden).toBe(false);
    const classes = root.querySelector<HTMLElement>('[aria-label="Window classes of slack"]');
    expect(classes?.closest("[hidden]")).toBeNull();

    app.open = false;
    // WebKit fires toggle asynchronously after the programmatic open, which must not count as a touch.
    await new Promise((resolve) => { setTimeout(resolve, 0); });
    limits.dispatchEvent(new Event("toggle", { bubbles: true }));
    type(search, "");
    expect(limits.open).toBe(false);
    expect(app.open).toBe(false);
    expect(root.querySelector<HTMLElement>("section[data-page=recognition]")?.hidden).toBe(false);
  });

  it("keeps a disclosure the user opened during a search after the search is cleared", async () => {
    await open();
    const search = root.querySelector<HTMLInputElement>(".search") as HTMLInputElement;
    type(search, "quiet before paste");
    const fold = root.querySelector<HTMLDetailsElement>('details[data-fold="commands/quiet/timings"]');
    if (!fold) throw new Error("no timings details");
    expect(fold.open).toBe(true);
    fold.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(fold.open).toBe(false);
    await Promise.resolve();
    fold.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(fold.open).toBe(true);
    await Promise.resolve();
    type(search, "");
    goTo("commands");
    expect(root.querySelector<HTMLDetailsElement>('details[data-fold="commands/quiet/timings"]')?.open).toBe(true);
  });

  it("keeps a disclosure open across page navigation and an entry re-render", async () => {
    const stub = await open(local());
    goTo("listening");
    const limits = root.querySelector<HTMLDetailsElement>('details[data-fold="listening/capture-limits/limits"]');
    if (!limits) throw new Error("no limits");
    limits.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
    goTo("recognition");
    goTo("listening");
    expect(root.querySelector<HTMLDetailsElement>('details[data-fold="listening/capture-limits/limits"]')?.open).toBe(true);

    const search = root.querySelector<HTMLInputElement>(".search");
    if (search && search.value !== "") type(search, "");
    goTo("recognition");
    await settle();
    edit("On this computer");
    await settle();
    const card = root.querySelector<HTMLDetailsElement>("details.model-details");
    if (!card) throw new Error("no model details");
    card.querySelector("summary")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await Promise.resolve();
    const id = card.dataset["fold"];
    stub.emit({ state: "loading", notice: "Loading." });
    await settle();
    within(editorEl(), "Cancel").click();
    edit("On this computer");
    await settle();
    const again = root.querySelector<HTMLDetailsElement>(`details[data-fold="${id ?? ""}"]`);
    expect(again?.open).toBe(true);
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
    const hold = rowOf("Dictate key");
    expect(hold.querySelector("button")).toBeNull();
    expect(hold.textContent).toContain("GNOME Wayland does not tell Thalvi when a key is let go.");
    expect(hold.closest("section")?.dataset["page"]).toBe("listening");
    const write = rowOf("Write key");
    expect(write.querySelector("button")).toBeNull();
    expect(write.textContent).toContain("GNOME Wayland does not tell Thalvi when a key is let go.");
  });

  describe("hold keys", () => {
    function press(code: string, modifiers: { ctrlKey?: boolean; shiftKey?: boolean; altKey?: boolean; metaKey?: boolean } = {}): void {
      window.dispatchEvent(new KeyboardEvent("keydown", { code, bubbles: true, cancelable: true, ...modifiers }));
    }

    const SAME = "The write key must differ from the dictate key.";
    const BOUND = "F8 is already bound in Hyprland. Pick another key or remove that bind. The keys that were set before still work.";
    const KEPT = "The keys that were set before still work.";
    const MODIFIER = `Hyprland can bind only F1 to F12 without a modifier for the hold keys, not Alt+KeyQ. ${KEPT}`;
    const NEEDS = 'The key "KeyY" needs a modifier, or typing it would start a capture.';
    const UNKNOWN = 'The key "Tab" is not a key name such as Alt+KeyZ: modifiers Ctrl, Shift, Alt or Win, then a key such as KeyZ, Digit1, F9 or Space, joined by +.';

    const TAB = "Tab can't be used as a hold key. Use a letter, a digit, Space or F1 to F12, alone or with Ctrl, Shift, Alt or Win.";

    // (row, keys pressed, what the desktop refuses with, key sent, key shown, restore disabled, notice shown under the row,
    // whether the other row was refused first, which must leave nothing under it once this row records)
    const cases: [string, string[][], string | null, string | null, string, boolean, string, boolean][] = [
      ["Dictate key", [["KeyY", "ctrl", "shift"]], null, "Ctrl+Shift+KeyY", "Ctrl+Shift+Y", false, "", false],
      ["Write key", [["F8"]], null, "F8", "F8", false, "", false],
      ["Dictate key", [["AltLeft", "alt"], ["Digit1", "alt"]], null, "Alt+Digit1", "Alt+1", false, "", false],
      ["Dictate key", [["Escape"], ["KeyY", "ctrl"]], null, null, "Alt+Z", true, "", false],
      ["Dictate key", [["F8"]], BOUND, "F8", "Alt+Z", true, BOUND, false],
      ["Write key", [["KeyZ", "alt"]], SAME, "Alt+KeyZ", "Alt+X", true, SAME, false],
      ["Dictate key", [["KeyQ", "alt"]], MODIFIER, "Alt+KeyQ", "Alt+Z", true, MODIFIER.replace("Alt+KeyQ", "Alt+Q"), false],
      ["Dictate key", [["KeyY"]], NEEDS, "KeyY", "Alt+Z", true, NEEDS.replace("KeyY", "Y"), false],
      ["Write key", [["Tab"]], UNKNOWN, "Tab", "Alt+X", true, TAB, false],
      ["Write key", [["Tab"]], UNKNOWN, "Tab", "Alt+X", true, TAB, true],
      ["Write key", [["F8"]], null, "F8", "F8", false, "", true],
      // AltGr alone: Windows sends the left Ctrl it adds first, and both flags with the right Alt.
      ["Dictate key", [["ControlLeft", "ctrl"], ["AltRight", "ctrl", "alt"]], null, "AltRight", "AltGr", false, "", false],
      ["Write key", [["AltRight", "alt"]], null, "AltRight", "AltGr", false, "", false],
      ["Write key", [["ControlLeft", "ctrl"], ["Escape"], ["AltRight", "ctrl", "alt"]], null, null, "Alt+X", true, "", false],
      ["Dictate key", [["AltRight", "alt"]], MODIFIER.replace("Alt+KeyQ", "AltRight"), "AltRight", "Alt+Z", true, MODIFIER.replace("Alt+KeyQ", "AltGr"), false],
    ];

    it.each(cases)("%s: keys %j, desktop says %j: sends %j, shows %j", async (label, keys, backendSays, sent, shown, restoreDisabled, notice, otherRefused) => {
      const stub = await open();
      goTo("listening");
      const other = rowOf(label === "Dictate key" ? "Write key" : "Dictate key");
      if (otherRefused) {
        stub.backend.saveSettings.mockRejectedValueOnce(BOUND);
        within(other, "Change…").click();
        press("F6");
        await settle();
        expect(other.querySelector(".note")?.textContent).toBe(BOUND);
      }
      if (backendSays !== null) stub.backend.saveSettings.mockRejectedValue(backendSays);
      const row = rowOf(label);
      within(row, "Change…").click();
      expect(row.querySelector("kbd")?.textContent).toBe("Press the new key. Escape cancels.");
      expect(within(row, "Restore default").disabled).toBe(true);
      expect(other.querySelector(".note")?.textContent).toBe("");
      for (const [code, ...held] of keys) {
        press(code ?? "", { ctrlKey: held.includes("ctrl"), shiftKey: held.includes("shift"), altKey: held.includes("alt") });
        await settle();
      }

      const field = label === "Dictate key" ? "hold_key" : "write_key";
      expect(stub.backend.saveSettings.mock.calls.map(([patch]) => patch).slice(otherRefused ? 1 : 0)).toEqual(sent === null ? [] : [{ [field]: sent }]);
      expect(row.querySelector("kbd")?.textContent).toBe(shown);
      expect(within(row, "Restore default").disabled).toBe(restoreDisabled);
      expect(within(row, "Change…").disabled).toBe(false);
      expect(document.activeElement).toBe(within(row, "Change…"));
      expect(row.querySelector(".note")?.textContent ?? "").toBe(notice);
      expect(other.querySelector(".note")?.textContent).toBe("");
      expect(row.querySelector(".applied")?.textContent).toBe(notice === "" && sent !== null ? APPLIED_TEXT : "");
    });

    it.each([
      ["Dictate key", "search box", "Alt+Z"],
      ["Write key", "search box", "Alt+X"],
      ["Dictate key", "Escape", "Alt+Z"],
      ["Write key", "Escape", "Alt+X"],
    ])("%s: recording keeps focus on Change…, announces the prompt, and leaving it by %s keeps %s", async (label, leave, kept) => {
      const stub = await open();
      goTo("listening");
      const row = rowOf(label);
      const change = within(row, "Change…");
      change.click();
      expect(document.activeElement).toBe(change);
      expect(change.disabled).toBe(false);
      expect(change.getAttribute("aria-disabled")).toBe("true");
      const prompt = Array.from(row.querySelectorAll("*")).find((node) => node.textContent === "Press the new key. Escape cancels.");
      expect(prompt?.closest('[aria-live="polite"], [role="status"]')).not.toBeNull();
      change.click();
      expect(change.getAttribute("aria-disabled")).toBe("true");

      if (leave === "search box") {
        const search = root.querySelector<HTMLInputElement>('input[aria-label="Search settings"]') as HTMLInputElement;
        search.focus();
        const heard: KeyboardEvent[] = [];
        search.addEventListener("keydown", (event) => heard.push(event));
        const typed = new KeyboardEvent("keydown", { code: "KeyQ", key: "q", bubbles: true, cancelable: true });
        search.dispatchEvent(typed);
        expect(heard).toEqual([typed]);
        expect(typed.defaultPrevented).toBe(false);
        expect(document.activeElement).toBe(search);
      } else {
        press("Escape");
        expect(document.activeElement).toBe(change);
      }
      await settle();

      expect(stub.backend.saveSettings).not.toHaveBeenCalled();
      expect(row.querySelector("kbd")?.textContent).toBe(kept);
      expect(change.hasAttribute("aria-disabled")).toBe(false);
      expect(within(row, "Restore default").disabled).toBe(true);
    });

    // (row, field, desktop refuses with, key shown after, focus after)
    it.each([
      ["Dictate key", "hold_key", null, "Alt+Z", "Change…"],
      ["Write key", "write_key", null, "Alt+X", "Change…"],
      ["Dictate key", "hold_key", BOUND, "Ctrl+F9", "Restore default"],
      ["Write key", "write_key", BOUND, "Ctrl+F9", "Restore default"],
    ] as const)("%s (%s): Restore default from Ctrl+F9, desktop says %j: shows %s, focus on %s", async (label, field, backendSays, shown, focused) => {
      const stub = await open({ ...SETTINGS, [field]: "Ctrl+F9" });
      goTo("listening");
      if (backendSays !== null) stub.backend.saveSettings.mockRejectedValue(backendSays);
      const row = rowOf(label);
      expect(row.querySelector("kbd")?.textContent).toBe("Ctrl+F9");
      const restore = within(row, "Restore default");
      restore.focus();
      restore.click();
      await settle();

      expect(stub.backend.saveSettings.mock.calls.map(([patch]) => patch)).toEqual([{ [field]: SETTINGS[field] }]);
      expect(row.querySelector("kbd")?.textContent).toBe(shown);
      expect(restore.disabled).toBe(backendSays === null);
      expect(document.activeElement).toBe(within(row, focused));
      expect(row.querySelector(".note")?.textContent ?? "").toBe(backendSays ?? "");
    });
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

  it("changes the device while listening and pauses and resumes from the Listening page only", async () => {
    const stub = await open();
    const pageOf = (label: string): string | null => rowOf(label).closest("section")?.getAttribute("data-page") ?? null;
    expect(pageOf("Listening")).toBe("listening");
    expect(root.querySelectorAll('.row[data-label="listening"]')).toHaveLength(1);
    expect(root.querySelectorAll('section[data-page="app"] .row[data-label="listening"]')).toHaveLength(0);

    goTo("listening");
    const toggle = control("Listening", "input");
    const picker = control("Microphone", "select");
    expect(picker.disabled).toBe(false);
    choose(picker, "USB Microphone");
    await settle();
    expect(sent(stub).microphone).toBe("USB Microphone");
    expect(rowOf("Microphone").querySelector(".applied")?.textContent).toBe(APPLIED_TEXT);
    expect(stub.backend.setListening).not.toHaveBeenCalled();

    flip(toggle);
    expect(stub.backend.setListening).toHaveBeenLastCalledWith(false);
    stub.emit({ state: "paused" });
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
    const options = Array.from(control("Microphone", "select").options).map((option) => option.textContent);
    expect(options).toEqual(["System default", "USB Microphone", "Built-in", "Old Mic (not found)"]);
  });

  it("says which microphone is open instead when the chosen one could not be opened", async () => {
    const stub = await open();
    goTo("listening");
    const refusal = (): string => rowOf("Microphone").querySelector<HTMLElement>(".note:not([hidden])")?.textContent ?? "";
    choose(control("Microphone", "select"), "USB Microphone");
    await settle();
    stub.audio({ selected: "USB Microphone", active: "Built-in", paused: false, problem: "USB Microphone is not connected." });
    expect(refusal()).toBe("USB Microphone is not connected. Built-in is open instead.");
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("Built-in");
    stub.audio({ selected: "USB Microphone", active: "USB Microphone", paused: false, problem: null });
    expect(refusal()).toBe("");
  });

  it.each<[string, string | null, string, string]>([
    ["the system default", null, "Default Audio Device", "System default (Default Audio Device)"],
    ["a named device", "USB Microphone", "USB Microphone", "USB Microphone"],
  ])("names the open microphone when %s is chosen", async (_chosen, selected, active, shown) => {
    const stub = await open({ ...SETTINGS, microphone: selected });
    goTo("listening");
    stub.audio({ selected, active, paused: false, problem: null });
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe(shown);
  });

  it("lays the microphone picker out like the wake model row", async () => {
    await open();
    goTo("listening");
    const microphone = rowOf("Microphone");
    const wake = rowOf("Wake model (dictation)");
    expect(microphone.classList.contains("wide")).toBe(wake.classList.contains("wide"));
    expect(microphone.querySelector(".stack.wide .trow.pick select")).not.toBeNull();
    expect(microphone.querySelector(".what small")?.textContent).toBe(
      "Opens now, or after the capture that is running.",
    );
  });

  it("shows the active microphone separately from the chosen one and stays paused after choosing while paused", async () => {
    const stub = await open();
    goTo("listening");
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("System default (Built-in)");
    stub.emit({ state: "paused" });
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("None — no microphone is open");
    choose(control("Microphone", "select"), "USB Microphone");
    await settle();
    await settle();
    expect(sent(stub).microphone).toBe("USB Microphone");
    expect(control("Microphone", "select").value).toBe("USB Microphone");
    expect(rowOf("Active microphone").querySelector("output")?.textContent).toBe("None — no microphone is open");
    expect(control("Listening", "input").checked).toBe(false);
  });

  it("shows a failed apply plainly", async () => {
    const stub = await open();
    stub.emit({ state: "failed", notice: "The saved microphone could not be opened.", headline: "Failed — Settings", causes: [], kind: "other" });
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
    goTo("listening");
    stub.level(0.5);
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.500)");
    stub.emit({ state: "paused" });
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.000)");
    stub.level(0.6);
    expect((root.querySelector(".meter b") as HTMLElement).style.transform).toBe("scaleX(0.000)");
  });

  it.each([false, true])("uses audio state for listening controls when a failure occurs (paused=%s)", async (paused) => {
    const stub = await open();
    stub.audio({ selected: null, active: paused ? null : "Built-in", paused, problem: null });
    stub.emit({ state: "failed", notice: "The last operation failed.", headline: "Failed — Settings", causes: [], kind: "other" });
    expect(control("Listening", "input").checked).toBe(!paused);
  });

  it("retains a runtime problem through an applied change and clears it on recovery", async () => {
    const stub = await open();
    stub.emit({ state: "failed", notice: "The chosen microphone is unavailable.", headline: "Failed — Settings", causes: [], kind: "other" });
    goTo("commands");
    type(control("Quiet before paste", "input"), "1.5");
    await settle();
    expect(root.querySelector(".err")?.textContent).toBe("The chosen microphone is unavailable.");
    stub.emit({ state: "listening" });
    expect(root.querySelector(".err")?.textContent).toBe("");
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
    expect(rowOf(label).textContent).not.toContain("THALVI_RECORD");

    flip(control(label, "input"));
    await settle();
    const expected = structuredClone(SETTINGS);
    expected.recordings[kind] = turnedOn;
    expect(sent(stub)).toEqual(expected);
  });

  it("shows the version", async () => {
    await open();
    expect(rowOf("Version").querySelector("output")?.textContent).toBe("1.2.3");
  });
});

describe("updates", () => {
  const state = (phase: UpdateState["phase"], notes: string | null = null, notice: string | null = null): UpdateState => ({ phase, notes, notice });
  const status = () => rowOf("Check for updates").querySelector("output");
  const inRow = (name: string) => within(rowOf("Check for updates"), name);
  const free = [false, null];
  const busy = [false, "true"];
  const off = [true, null];

  async function openOn(version: string, channel: Settings["update_channel"] = "stable") {
    const stub = stubBackend({ ...structuredClone(SETTINGS), update_channel: channel });
    stub.backend.appVersion.mockResolvedValue(version);
    mountSettings(root, stub.backend);
    await settle();
    return stub;
  }

  it("offers Stable first and sends a Development choice with Save", async () => {
    const stub = await open();
    const channel = control("Update channel", "select");
    expect(Array.from(channel.options).map((option) => [option.value, option.textContent])).toEqual([
      ["stable", "Stable"],
      ["development", "Development"],
    ]);
    expect(channel.value).toBe("stable");

    choose(channel, "development");
    await settle();
    expect(sent(stub).update_channel).toBe("development");
  });

  it.each([
    [state({ state: "idle" }), "Thalvi has not checked for updates yet. It checks by itself shortly after it starts.", "Check for updates", free, "note ok"],
    [state({ state: "checking" }), "Checking for updates…", "Check for updates", busy, "note ok"],
    [state({ state: "upToDate" }), "Thalvi 1.2.3 is up to date.", "Check for updates", free, "note ok"],
    [state({ state: "downloading", version: "1.3.0", percent: 40 }, "https://example.com/1.3.0"), "Downloading 1.3.0 — 40%…", "Check for updates", busy, "note ok"],
    [state({ state: "downloading", version: "1.3.0", percent: null }), "Downloading 1.3.0…", "Check for updates", busy, "note ok"],
    [state({ state: "ready", version: "1.3.0" }, "https://example.com/1.3.0"), "1.3.0 is downloaded. Thalvi installs it and restarts when it is idle: not during a dictation.", "Check for updates", free, "note ok"],
    [state({ state: "installing", version: "1.3.0" }), "Installing 1.3.0. Thalvi restarts in a moment.", "Check for updates", busy, "note ok"],
    [state({ state: "failed", message: "Thalvi could not reach the update server." }), "Thalvi could not reach the update server.", "Retry", free, "note"],
    [state({ state: "unsupported", message: "Run Thalvi as the AppImage to update it." }), "Run Thalvi as the AppImage to update it.", "Check for updates", off, "note"],
  ])("shows the update's state in plain words: %j", async (next, text, label, availability, tone) => {
    const stub = await open();
    stub.update(next);
    expect(status()?.textContent).toBe(text);
    expect(status()?.className).toBe(tone);
    expect([inRow(label).disabled, inRow(label).getAttribute("aria-disabled")]).toEqual(availability);
  });

  it.each([
    state({ state: "checking" }),
    state({ state: "downloading", version: "1.3.0", percent: 40 }),
    state({ state: "installing", version: "1.3.0" }),
  ])("keeps focus on the busy button, ignores its presses, and retries from it after a failure: %j", async (working) => {
    const stub = await open();
    goTo("app");
    const check = inRow("Check for updates");
    check.focus();
    expect(document.activeElement).toBe(check);
    check.click();
    stub.update(working);
    expect(document.activeElement).toBe(check);
    expect(check.getAttribute("aria-disabled")).toBe("true");
    check.click();
    expect(stub.backend.checkForUpdates).toHaveBeenCalledTimes(1);

    stub.update(state({ state: "failed", message: "Thalvi could not reach the update server." }));
    expect(document.activeElement).toBe(inRow("Retry"));
    expect(check.hasAttribute("aria-disabled")).toBe(false);
    inRow("Retry").click();
    expect(stub.backend.checkForUpdates).toHaveBeenCalledTimes(2);
  });

  it("tells a failed check apart from being up to date", async () => {
    const stub = await open();
    stub.update(state({ state: "upToDate" }));
    const upToDate = [status()?.textContent, status()?.className];
    stub.update(state({ state: "failed", message: "Thalvi could not reach the update server." }));
    expect([status()?.textContent, status()?.className]).not.toEqual(upToDate);
    expect(inRow("Retry")).toBeDefined();
  });

  it("shows the state it was opened in, then follows it", async () => {
    const stub = stubBackend();
    stub.update(state({ state: "ready", version: "1.3.0" }));
    mountSettings(root, stub.backend);
    await settle();
    expect(status()?.textContent).toContain("1.3.0 is downloaded");
    stub.update(state({ state: "upToDate" }));
    expect(status()?.textContent).toBe("Thalvi 1.2.3 is up to date.");
  });

  it("checks on a click, and says why it could not ask", async () => {
    const stub = await open();
    inRow("Check for updates").click();
    expect(stub.backend.checkForUpdates).toHaveBeenCalledTimes(1);

    stub.backend.checkForUpdates.mockRejectedValueOnce(new Error("Thalvi is shutting down."));
    inRow("Check for updates").click();
    await settle();
    expect(rowOf("Check for updates").textContent).toContain("Thalvi is shutting down.");
  });

  it("offers the release notes only when the update has some, and says why they would not open", async () => {
    const stub = await open();
    expect(inRow("Release notes").hidden).toBe(true);
    stub.update(state({ state: "ready", version: "1.3.0" }, "https://example.com/1.3.0"));
    expect(inRow("Release notes").hidden).toBe(false);

    inRow("Release notes").click();
    expect(stub.backend.openReleaseNotes).toHaveBeenCalledTimes(1);
    stub.backend.openReleaseNotes.mockRejectedValueOnce(new Error("No browser could be started."));
    inRow("Release notes").click();
    await settle();
    expect(rowOf("Check for updates").textContent).toContain("No browser could be started.");

    stub.update(state({ state: "upToDate" }));
    expect(inRow("Release notes").hidden).toBe(true);
  });

  it("says once what the last update did, without hiding the state", async () => {
    const stub = await open();
    stub.update(state({ state: "idle" }, null, "Updated to 1.2.3."));
    const notes = Array.from(rowOf("Check for updates").querySelectorAll("p")).map((paragraph) => [paragraph.textContent, paragraph.hidden]);
    expect(notes).toContainEqual(["Updated to 1.2.3.", false]);
    expect(status()?.textContent).toContain("has not checked");
  });

  it.each([
    ["0.2.0-dev.3", "stable", true],
    ["0.2.0-dev.3", "development", false],
    ["0.2.0-dev.3+abc", "stable", true],
    ["1.2.3", "stable", false],
    ["1.2.3+build-5", "stable", false],
  ] as const)("on %s with %s chosen, the note that Thalvi stays on its build is shown: %s", async (version, channel, shown) => {
    await openOn(version, channel);
    const note = Array.from(rowOf("Update channel").querySelectorAll("p")).find((paragraph) => paragraph.textContent.includes("stays on this build"));
    expect(note !== undefined && !note.hidden).toBe(shown);
    if (shown) expect(note?.textContent).toBe(`You are on the development build ${version}. On Stable, Thalvi stays on this build until a newer stable release is published.`);
  });

  it("shows the note as soon as Stable is applied on a development build", async () => {
    await openOn("0.2.0-dev.3", "development");
    const note = () => Array.from(rowOf("Update channel").querySelectorAll("p")).find((paragraph) => !paragraph.hidden);
    expect(note()).toBeUndefined();
    choose(control("Update channel", "select"), "stable");
    await settle();
    expect(note()?.textContent).toContain("stays on this build");
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
    confirmIn(cards()[1] as HTMLElement, "Delete", "Yes, delete");
    await settle();
    expect(stub.backend.deleteHistoryEntry).toHaveBeenCalledExactlyOnceWith(20);
    expect(cards().map((card) => shown(card).text)).toEqual(["Remind me to call.", "Never mind this one"]);
  });

  it.each([
    ["the first of three", ENTRIES, 0, null, 1],
    ["the middle of three", ENTRIES, 1, null, 2],
    ["the last of three", ENTRIES, 2, null, 1],
    ["the only entry", ENTRIES.slice(0, 1), 0, null, "Nothing has been kept yet."],
    ["an entry, refused", ENTRIES, 1, "The history could not be written.", 1],
  ] as const)("after deleting %s, focus is on its neighbour's Delete, else the empty note", async (_case, list, deleted, refusal, expected) => {
    const stub = await openHistory(list, list, list.filter((_, index) => index !== deleted));
    goTo("history");
    await settle();
    if (refusal) stub.backend.deleteHistoryEntry.mockRejectedValueOnce(refusal);
    const labels = cards().map((card) => within(card, "Delete").getAttribute("aria-label"));
    confirmIn(cards()[deleted] as HTMLElement, "Delete", "Yes, delete");
    await settle();
    const active = document.activeElement as HTMLElement;
    expect(active.getAttribute("aria-label") ?? active.textContent).toBe(typeof expected === "number" ? labels[expected] : expected);
  });

  it("keeps the list and says why when an entry cannot be deleted", async () => {
    const stub = await openHistory(ENTRIES);
    stub.backend.deleteHistoryEntry.mockRejectedValueOnce("The history could not be written.");
    confirmIn(cards()[0] as HTMLElement, "Delete", "Yes, delete");
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
    expect(rowOf("Keep a history").textContent).toContain("What you dictate and where it went.");

    flip(control("Keep a history", "input"));
    type(control("Entries to keep", "input"), "50");
    await settle();
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
  const buttons = (): string[] => Array.from(section().querySelectorAll("button")).filter(shownInSection).map((candidate) => candidate.textContent);
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
    confirmIn(section(), "Discard", "Yes, discard");
    await settle();
    expect(stub.backend.discardDraft).toHaveBeenCalledOnce();
    expect(area().value).toBe("Waiting.");
    expect(notes()).not.toContain("A newer dictation is waiting. Discard this one to see it.");

    stub.backend.getDraft.mockResolvedValueOnce(null);
    confirmIn(section(), "Discard", "Yes, discard");
    await settle();
    expect(notes()[0]).toContain("Nothing to recover yet.");
  });

  it.each([
    ["nothing is left", null, null, "Nothing to recover yet. The last dictation appears here, also when it failed or you cancelled it."],
    ["a newer draft waits", { ...DRAFT, id: 9, original: "Waiting.", text: "Waiting." }, null, "Discard"],
    ["the discard is refused", null, "The draft could not be removed.", "Discard"],
  ] as const)("after Discard when %s, focus is on the empty note, else on Discard", async (_case, next, refusal, expected) => {
    const stub = await openDraft({ ...DRAFT, newerWaiting: next !== null });
    goTo("history");
    await settle();
    if (refusal) stub.backend.discardDraft.mockRejectedValueOnce(refusal);
    else stub.backend.getDraft.mockResolvedValueOnce(next);
    confirmIn(section(), "Discard", "Yes, discard");
    await settle();
    expect(document.activeElement?.textContent).toBe(expected);
  });

  it.each([
    ["Delete", () => root.querySelector<HTMLElement>(".entry") ?? root, "Delete this entry? This cannot be undone.", "Yes, delete", /^Delete the entry from .*\d/, "Nothing has been kept yet."],
    ["Discard", section, "Discard the last dictation? This cannot be undone.", "Yes, discard", /^Discard$/, "Nothing to recover yet. The last dictation appears here, also when it failed or you cancelled it."],
  ] as const)("%s asks first: Keep changes nothing and returns to it, the yes acts once", async (action, scope, question, yes, name, after) => {
    const stub = stubBackend();
    let entries: HistoryEntry[] = [{ id: 30, timestamp_ms: Date.UTC(2026, 10, 3, 9, 30), text: "Remind me to call.", target: "Slack", outcome: "accepted" }];
    let draft: Draft | null = DRAFT;
    stub.backend.getHistory.mockImplementation(() => Promise.resolve(structuredClone(entries)));
    stub.backend.deleteHistoryEntry.mockImplementation(() => { entries = []; return Promise.resolve(); });
    stub.backend.getDraft.mockImplementation(() => Promise.resolve(structuredClone(draft)));
    stub.backend.discardDraft.mockImplementation(() => { draft = null; return Promise.resolve(); });
    mountSettings(root, stub.backend);
    await settle();
    goTo("history");
    await settle();
    const sent = action === "Delete" ? stub.backend.deleteHistoryEntry : stub.backend.discardDraft;
    const ask = within(scope(), action);
    expect(ask.getAttribute("aria-label") ?? ask.textContent).toMatch(name);
    const asking = (): HTMLElement | null => scope().querySelector<HTMLElement>(`[role="group"][aria-label="${question}"]`);

    ask.focus();
    ask.click();
    await settle();
    expect(sent).not.toHaveBeenCalled();
    expect(ask.hidden).toBe(true);
    expect(asking()?.hidden).toBe(false);
    expect(asking()?.textContent).toContain(question);
    expect(document.activeElement?.textContent).toBe("Keep");

    within(scope(), "Keep").click();
    await settle();
    expect(sent).not.toHaveBeenCalled();
    expect(ask.hidden).toBe(false);
    expect(asking()?.hidden).toBe(true);
    expect(document.activeElement).toBe(ask);

    ask.click();
    within(scope(), yes).click();
    await settle();
    expect(sent).toHaveBeenCalledOnce();
    expect(document.activeElement?.textContent).toBe(after);
  });

  it("opens on the Recognition page, where a first start leaves it", async () => {
    await openDraft();
    expect(root.querySelector('nav [aria-current="page"]')?.getAttribute("data-page")).toBe("recognition");
    expect(root.querySelector<HTMLElement>('section[data-page="recognition"]')?.hidden).toBe(false);
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

describe("attached editor contrast", () => {
  it.each(["gunmetal", "holo"])("uses a dark surface for the %s look", async (look) => {
    const style = document.head.appendChild(document.createElement("style"));
    style.textContent = settingsCss + settingsLooksCss;
    try {
      await open();
      root.dataset["look"] = look;
      goTo("recognition");
      (root.querySelector('.trow.server button[aria-label^="Edit "]') as HTMLButtonElement).click();
      const editor = root.querySelector<HTMLElement>(".table > .editor");
      if (!editor) throw new Error("no attached editor");
      expect(getComputedStyle(editor).backgroundColor).toBe("rgb(16, 20, 24)");
    } finally {
      style.remove();
    }
  });
});

describe("switch colours", () => {
  it.each(["neon", "holo", "gunmetal"])("draws an off switch in %s without the error colour, and an on switch keeps its on colour", (look) => {
    const style = document.head.appendChild(document.createElement("style"));
    style.textContent = settingsCss + settingsLooksCss;
    try {
      const rules = Array.from(style.sheet?.cssRules ?? []).filter((rule): rule is CSSStyleRule => rule instanceof CSSStyleRule);
      const applies = (selector: string): boolean => selector.includes('input[type="checkbox"]') && (!selector.includes("data-look") || selector.includes(`data-look="${look}"`));
      const switchRules = rules.filter((rule) => rule.selectorText.split(/,(?![^(]*\))/).some(applies));
      const off = switchRules.filter((rule) => !rule.selectorText.includes(":checked"));
      expect(off.length).toBeGreaterThan(0);
      for (const rule of off) expect(rule.style.cssText, rule.selectorText).not.toMatch(/--bad|--warn/);
      if (look === "gunmetal") {
        const on = switchRules.filter((rule) => rule.selectorText.includes(":checked"));
        expect(on.map((rule) => rule.style.cssText).join(" ")).toContain("--ok");
      }
    } finally {
      style.remove();
    }
  });
});

describe("the failure bar", () => {
  // jsdom does not load the app's stylesheet, and what shows depends on it.
  beforeEach(() => {
    document.head.appendChild(document.createElement("style")).textContent = settingsCss;
  });
  afterEach(() => {
    document.head.querySelector("style")?.remove();
  });

  it("holds nothing but a failure: it is hidden until one comes and is gone when it recovers", async () => {
    const stub = await open();
    const bar = root.querySelector<HTMLElement>("footer.bar") as HTMLElement;
    expect(getComputedStyle(bar).display).toBe("none");
    stub.emit({ state: "failed", notice: "Could not download the wake model.", headline: "Failed — Settings", causes: [], kind: "other" });
    expect(getComputedStyle(bar).display).not.toBe("none");
    expect(getComputedStyle(root.querySelector(".err") as HTMLElement).display).not.toBe("none");
    expect(getComputedStyle(root.querySelector(".msg") as HTMLElement).display).toBe("none");
    expect(Array.from(bar.querySelectorAll("button"), (candidate) => candidate.textContent)).toEqual(["Copy", "×"]);
    stub.emit({ state: "listening" });
    expect(getComputedStyle(bar).display).toBe("none");
  });
});

describe("local chat model", () => {
  it("preserves the manual endpoint and prompt while switching to the local model and back", async () => {
    const writing = { url: "http://127.0.0.1:8181/v1", model: "saved", has_key: true, prompt: "Keep my words." };
    const stub = await open({ ...SETTINGS, writing });
    goTo("chat");
    choose(control("Chat model source", "select"), "local");
    await settle();
    expect(sent(stub).writing).toEqual({ ...writing, local: true });
    expect(rowOf("Writing endpoint address").classList.contains("off")).toBe(true);
    expect(rowOf("Local chat model").classList.contains("off")).toBe(false);
    expect(rowOf("Local chat model").textContent).toContain("Apache-2.0");
    expect(rowOf("Local chat model").textContent).toContain("English, German, Spanish");
    choose(control("Chat model source", "select"), "manual");
    await settle();
    expect(sent(stub).writing).toEqual({ ...writing, local: false });
    expect(rowOf("Writing endpoint address").classList.contains("off")).toBe(false);
    expect(rowOf("Local chat model").classList.contains("off")).toBe(true);
    expect(control("Writing endpoint address", "input").value).toBe(writing.url);
  });

  it("installs with checksum progress and asks before removing the file", async () => {
    const stub = await open({ ...SETTINGS, writing: { local: true } });
    goTo("chat");
    expect(rowOf("Local chat model").textContent).toContain("Qwen3 4B");
    stub.backend.downloadChatModel.mockImplementation(() => {
      const progress = stub.backend.onChatModelProgress.mock.calls[0]?.[0];
      progress?.({ name: "chat", percent: 42, status: "downloading", message: null, causes: [] });
      expect(rowOf("Local chat model").textContent).toContain("checking checksums… 42%");
      stub.backend.getChatModel.mockResolvedValue({ title: "Qwen3 4B", size: 2533261953, license: "Apache-2.0", languages: ["English", "German", "Spanish"], build: "vulkan", build_label: "Vulkan", model_installed: true, runtime_installed: true });
      return Promise.resolve();
    });
    button("Download chat model").click();
    await settle();
    expect(stub.backend.downloadChatModel).toHaveBeenCalledWith("vulkan");
    button("Remove chat model").click();
    expect(stub.backend.removeChatModel).not.toHaveBeenCalled();
    button("Keep model").click();
    expect(stub.backend.removeChatModel).not.toHaveBeenCalled();
    button("Remove chat model").click();
    button("Yes, delete model").click();
    await settle();
    expect(stub.backend.removeChatModel).toHaveBeenCalledOnce();
  });
});
