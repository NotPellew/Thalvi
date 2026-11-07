// Test doubles shared by the page tests.

import { vi } from "vitest";
import type { AudioStatus, Backend, Draft, HistoryEntry, SwitchState, Settings, SpeechModelInfo, SpeechModelProgress, Status, WakeModelInfo } from "./contract.ts";
import type { Clock } from "./overlay.ts";

export const SETTINGS: Settings = {
  server: "ws://127.0.0.1:8080",
  wake: {
    capture: { model: "hey_thalvi", threshold: 0.5 },
    conversation: { model: "hey_mycroft", threshold: 0.5 },
  },
  cues: { wake: true, provisional: true, cancelled: true, accepted: true, error: true },
  ending_pause: { paste: 0.7, submit: 2.5, task: 2.5 },
  capture_limits: { idle: 60, max: 300 },
  endings: {
    "paste that": "paste",
    submit: "submit",
    "do that": "task",
    "scratch that": "scratch",
    "never mind": "cancel",
  },
  stt: { engine: "server", secondary: null },
  apps: { slack: { classes: ["Slack"], executables: [], input_keys: ["ctrl", "k"] } },
  scripts: [{ phrase: "open the docs", action: "open_url", destination: "https://example.com", label: "docs" }],
  overlay: { show: true, draft: true },
  microphone: null,
  look: "neon",
  recordings: { metrics: true, text: false, audio: false },
  hold_key: "Alt+KeyZ",
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
  const savedListeners: (() => void)[] = [];
  const speechListeners: ((progress: SpeechModelProgress) => void)[] = [];
  const pageListeners: ((page: string) => void)[] = [];
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
    onSettingsSaved: vi.fn((listener: () => void) => savedListeners.push(listener)),
    getSettings: vi.fn(() => Promise.resolve(structuredClone(settings))),
    saveSettings: vi.fn<(settings: Settings) => Promise<void>>(() => Promise.resolve()),
    setListening: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    cancelCapture: vi.fn(() => Promise.resolve()),
    overlayTakesClicks: vi.fn(() => Promise.resolve(true)),
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
    getStartAtLogin: vi.fn<() => Promise<SwitchState>>(() => Promise.resolve({ kind: "available", on: false, problem: null })),
    setStartAtLogin: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    getMenuEntry: vi.fn<() => Promise<SwitchState>>(() => Promise.resolve({ kind: "available", on: false, problem: null })),
    setMenuEntry: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    copyLoginLine: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    validateSpeechModelFolder: vi.fn<(path: string) => Promise<void>>(() => Promise.resolve()),
    listWakeModels: vi.fn<() => Promise<WakeModelInfo[]>>(() => Promise.resolve(structuredClone(WAKE_MODELS_INFO))),
    validateWakeModels: vi.fn<(capture: string, conversation: string) => Promise<void>>((capture, conversation) => {
      if (capture === conversation) return Promise.reject(new Error("The capture and conversation wake models must be different."));
      for (const model of [capture, conversation]) {
        if (!WAKE_MODELS_INFO.some((info) => info.name === model) && (model.includes("bad") || model.includes("missing") || !model.endsWith(".onnx"))) {
          return Promise.reject(new Error(`The wake model ${model} could not be loaded: not a wake model.`));
        }
      }
      return Promise.resolve();
    }),
    downloadWakeModel: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    pickWakeModelFile: vi.fn<() => Promise<string | null>>(() => Promise.resolve("/models/custom.onnx")),
    listSpeechModels: vi.fn<() => Promise<SpeechModelInfo[]>>(() => Promise.resolve([{ ...NEMOTRON }, { ...UNIFIED }])),
    downloadSpeechModel: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    onSpeechModelProgress: vi.fn((listener: (progress: SpeechModelProgress) => void) => speechListeners.push(listener)),
    openSpeechModelSource: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    wordListSupport: vi.fn<(server: string) => Promise<boolean>>(() => Promise.resolve(false)),
    shareWordList: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    appVersion: vi.fn(() => Promise.resolve("1.2.3")),
  } satisfies Backend;
  return {
    backend,
    emit: (status: Status) => {
      for (const listener of statusListeners) listener(status);
      const paused = status.state === "paused";
      if ((status.state === "paused" || status.state === "listening") && paused !== audioState.paused) {
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
    saved: () => {
      for (const listener of savedListeners) listener();
    },
  };
}

/** Lets pending promise callbacks run. */
export async function settle(): Promise<void> {
  for (let turn = 0; turn < 20; turn += 1) await Promise.resolve();
}

/** A clock the test advances by hand; each advance runs the frames queued so far once. */
export function manualClock(): Clock & { advance(ms: number): void } {
  let time = 1000;
  let queued: (() => void)[] = [];
  return {
    now: () => time,
    frame: (callback) => queued.push(callback),
    advance: (ms) => {
      time += ms;
      const run = queued;
      queued = [];
      for (const callback of run) callback();
    },
  };
}
