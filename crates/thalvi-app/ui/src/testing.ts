// Test doubles shared by the page tests.

import { vi } from "vitest";
import type { AudioStatus, Backend, Draft, HistoryEntry, HoldKeyDefaults, LiveEntry, SpeechEntry, SwitchState, Settings, SpeechModelInfo, SpeechModelProgress, Status, UpdateState, WakeModelInfo } from "./contract.ts";

export const SETTINGS: Settings = {
  wake: {
    capture: { model: "hey_thalvi", threshold: 0.5 },
    conversation: { model: "hey_mycroft", threshold: 0.5 },
    enabled: true,
  },
  cues: { wake: true, provisional: true, cancelled: true, accepted: true, error: true },
  ending_pause: { paste: 0.7, submit: 2.5, task: 2.5 },
  capture_limits: { idle: 60, max: 300 },
  endings: {
    "paste that": "paste",
    "write that out": "write",
    submit: "submit",
    "do that": "task",
    "scratch that": "scratch",
    "never mind": "cancel",
  },
  stt: { entries: [{ kind: "realtime", id: "direct", name: "Realtime speech server", active: true, address: "ws://127.0.0.1:8012", model: "nemotron" }] },
  apps: { slack: { classes: ["Slack"], executables: [], input_keys: ["ctrl", "k"] } },
  scripts: [{ phrase: "open the docs", action: "open_url", destination: "https://example.com", label: "docs" }],
  overlay: { show: true, draft: true },
  microphone: null,
  look: "neon",
  update_channel: "stable",
  recordings: { metrics: true, text: false, audio: false },
  hold_key: "Alt+KeyZ",
  write_key: "Alt+KeyX",
  writing: null,
  writing_default_prompt: "Write what I dictated as clear text.",
  history: { enabled: true, limit: 200 },
};

export const WAKE_MODELS_INFO: WakeModelInfo[] = [
  { name: "hey_thalvi", title: "Hey Thalvi", size: 395_406, cached: true, threshold: 0.7 },
  { name: "hey_mycroft", title: "Hey Mycroft", size: 857_691, cached: false, threshold: 0.5 },
  { name: "hey_jarvis", title: "Hey Jarvis", size: 1_271_370, cached: false, threshold: 0.5 },
];

const UNIFIED_LANGUAGES = [
  "Bulgarian",
  "Croatian",
  "Czech",
  "Danish",
  "Dutch",
  "English",
  "Estonian",
  "Finnish",
  "French",
  "German",
  "Greek",
  "Hungarian",
  "Italian",
  "Latvian",
  "Lithuanian",
  "Maltese",
  "Polish",
  "Portuguese",
  "Romanian",
  "Russian",
  "Slovak",
  "Slovenian",
  "Spanish",
  "Swedish",
  "Ukrainian",
];

export const NEMOTRON: SpeechModelInfo = {
  name: "nemotron_160ms",
  title: "Nemotron Streaming",
  description: "NVIDIA's Nemotron Speech Streaming.",
  size: 661_919_416,
  source: "https://huggingface.co/example/nemotron",
  license: "NVIDIA Open Model License",
  languages: ["English"],
  cached: false,
};

export const UNIFIED: SpeechModelInfo = {
  name: "unified_160ms",
  title: "Nemotron Unified",
  description: "NVIDIA's Nemotron Unified ASR.",
  size: 646_518_349,
  source: "https://huggingface.co/example/unified",
  license: "OpenMDW-1.1",
  languages: UNIFIED_LANGUAGES,
  cached: false,
};

/** A backend whose calls are spies and whose events the test emits. */
export function stubBackend(settings: Settings = SETTINGS, initial: Status = { state: "listening" }) {
  const statusListeners: ((status: Status) => void)[] = [];
  const levelListeners: ((level: number) => void)[] = [];
  const audioListeners: ((audio: AudioStatus) => void)[] = [];
  const elsewhereListeners: (() => void)[] = [];
  const speechListeners: ((progress: SpeechModelProgress) => void)[] = [];
  const pageListeners: ((page: string) => void)[] = [];
  const updateListeners: ((state: UpdateState) => void)[] = [];
  let updateState: UpdateState = { phase: { state: "idle" }, notes: null, notice: null };
  let audioState: AudioStatus = {
    selected: settings.microphone,
    active: initial.state === "paused" ? null : (settings.microphone ?? "Built-in"),
    paused: initial.state === "paused",
  };
  const emitAudio = (next: AudioStatus): void => {
    audioState = next;
    for (const listener of audioListeners) listener(next);
  };
  const backend = {
    getStatus: vi.fn(() => Promise.resolve(initial)),
    onStatus: vi.fn((listener: (status: Status) => void) => statusListeners.push(listener)),
    onLevel: vi.fn((listener: (level: number) => void) => levelListeners.push(listener)),
    getAudio: vi.fn(() => Promise.resolve(structuredClone(audioState))),
    onAudio: vi.fn((listener: (audio: AudioStatus) => void) => audioListeners.push(listener)),
    onSettingsChangedElsewhere: vi.fn((listener: () => void) => elsewhereListeners.push(listener)),
    getSettings: vi.fn(() => Promise.resolve(structuredClone(settings))),
    saveSettings: vi.fn<(settings: Settings) => Promise<void>>(() => Promise.resolve()),
    setListening: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    dismissError: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    copyReport: vi.fn<(message: string, causes: string[]) => Promise<void>>(() => Promise.resolve()),
    listMicrophones: vi.fn(() => Promise.resolve(["USB Microphone", "Built-in"])),
    openRecordings: vi.fn(() => Promise.resolve()),
    getHistory: vi.fn<() => Promise<HistoryEntry[]>>(() => Promise.resolve([])),
    deleteHistoryEntry: vi.fn<(id: number) => Promise<void>>(() => Promise.resolve()),
    clearHistory: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    getDraft: vi.fn<() => Promise<Draft | null>>(() => Promise.resolve(null)),
    editDraft: vi.fn<(text: string) => Promise<void>>(() => Promise.resolve()),
    copyText: vi.fn<(text: string) => Promise<void>>(() => Promise.resolve()),
    discardDraft: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    onOpenPage: vi.fn((listener: (page: string) => void) => pageListeners.push(listener)),
    getHoldKeyUnavailable: vi.fn<() => Promise<string | null>>(() => Promise.resolve(null)),
    getHoldKeyDefaults: vi.fn<() => Promise<HoldKeyDefaults>>(() => Promise.resolve({ hold_key: "Alt+KeyZ", write_key: "Alt+KeyX" })),
    getStartAtLogin: vi.fn<() => Promise<SwitchState>>(() => Promise.resolve({ kind: "available", on: false, problem: null })),
    setStartAtLogin: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    getMenuEntry: vi.fn<() => Promise<SwitchState>>(() => Promise.resolve({ kind: "available", on: false, problem: null })),
    setMenuEntry: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    copyLoginLine: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    validateSpeechModelFolder: vi.fn<(path: string) => Promise<void>>(() => Promise.resolve()),
    listWakeModels: vi.fn<() => Promise<WakeModelInfo[]>>(() => Promise.resolve(structuredClone(WAKE_MODELS_INFO))),
    validateWakeModels: vi.fn<(capture: string) => Promise<void>>((capture) => {
      if (!WAKE_MODELS_INFO.some((info) => info.name === capture) && (capture.includes("bad") || capture.includes("missing") || !capture.endsWith(".onnx"))) {
        return Promise.reject(new Error(`The wake model ${capture} could not be loaded: not a wake model.`));
      }
      return Promise.resolve();
    }),
    downloadWakeModel: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    pickWakeModelFile: vi.fn<() => Promise<string | null>>(() => Promise.resolve("/models/custom.onnx")),
    listSpeechModels: vi.fn<() => Promise<SpeechModelInfo[]>>(() => Promise.resolve([{ ...NEMOTRON }, { ...UNIFIED }])),
    downloadSpeechModel: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    onSpeechModelProgress: vi.fn((listener: (progress: SpeechModelProgress) => void) => speechListeners.push(listener)),
    speechDevice: vi.fn<(name: string) => Promise<string>>(() => Promise.resolve("Will run on the CPU: this model runs on the CPU")),
    openSpeechModelSource: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    checkSpeechEntry: vi.fn<(entry: SpeechEntry) => Promise<void>>(() => Promise.resolve()),
    wordListSupport: vi.fn<(entry: LiveEntry) => Promise<boolean>>(() => Promise.resolve(true)),
    appVersion: vi.fn(() => Promise.resolve("1.2.3")),
    getUpdateState: vi.fn<() => Promise<UpdateState>>(() => Promise.resolve(structuredClone(updateState))),
    onUpdate: vi.fn((listener: (state: UpdateState) => void) => updateListeners.push(listener)),
    checkForUpdates: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    openReleaseNotes: vi.fn<() => Promise<void>>(() => Promise.resolve()),
  } satisfies Backend;
  return {
    backend,
    emit: (status: Status) => {
      for (const listener of statusListeners) listener(status);
      const paused = status.state === "paused";
      if ((status.state === "paused" || status.state === "listening" || status.state === "keyboardOnly") && paused !== audioState.paused) {
        emitAudio({
          selected: audioState.selected,
          active: paused ? null : (audioState.selected ?? "Built-in"),
          paused,
        });
      }
    },
    audio: emitAudio,
    audioState: () => structuredClone(audioState),
    level: (level: number) => {
      for (const listener of levelListeners) listener(level);
    },
    speechProgress: (progress: SpeechModelProgress) => {
      for (const listener of speechListeners) listener(progress);
    },
    openPage: (page: string) => {
      for (const listener of pageListeners) listener(page);
    },
    changedElsewhere: () => {
      for (const listener of elsewhereListeners) listener();
    },
    update: (next: UpdateState) => {
      updateState = next;
      for (const listener of updateListeners) listener(structuredClone(next));
    },
  };
}

/** Lets pending promise callbacks run. */
export async function settle(): Promise<void> {
  for (let turn = 0; turn < 20; turn += 1) await Promise.resolve();
}
