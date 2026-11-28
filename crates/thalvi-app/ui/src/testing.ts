// Test doubles shared by the page tests.

import { vi } from "vitest";
import { OVERLAY_PREVIEW_STATES, type AudioStatus, type Backend, type ChatBuild, type ChatModelInfo, type CurrentApp, type Draft, type HistoryEntry, type KeyDefaults, type Learning, type LiveEntry, type OverlayPreviewState, type SpeechEntry, type SwitchState, type Settings, type SpeechModelInfo, type SpeechModelProgress, type Status, type UpdateState, type WakeModelInfo } from "./contract.ts";
import { applyPatch, type SettingsPatch } from "./patch.ts";

export const CHAT_MODEL: ChatModelInfo = {
  title: "Qwen3 4B · Q4_K_M", size: 2533261953, license: "Apache-2.0",
  languages: ["English", "German", "Spanish"], build: "vulkan", build_label: "Vulkan",
  model_installed: false, runtime_installed: false,
};

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
  stt: { entries: [{ kind: "realtime", id: "direct", name: "Realtime speech server", active: true, address: "ws://127.0.0.1:8012", model: "nemotron" }], learn_from_corrections: false },
  apps: { slack: { classes: ["Slack"], executables: [], input_keys: ["ctrl", "k"] } },
  scripts: [{ phrase: "open the docs", action: "open_url", destination: "https://example.com", label: "docs" }],
  overlay: { show: true, draft: true },
  microphone: null,
  look: "neon",
  update_channel: "stable",
  recordings: { metrics: true, text: false, audio: false },
  keys: [
    { action: "dictate", mode: "hold", key: "Alt+KeyZ" },
    { action: "write", mode: "hold", key: "Alt+KeyX" },
  ],
  writing: null,
  writing_default_prompt: "Write what I dictated as clear text.",
  history: { enabled: true, limit: 200 },
};

export const WAKE_MODELS_INFO: WakeModelInfo[] = [
  { name: "hey_thalvi", title: "Hey Thalvi", size: 395_406, cached: true, threshold: 0.7 },
  { name: "hey_mycroft", title: "Hey Mycroft", size: 857_691, cached: false, threshold: 0.5 },
  { name: "hey_jarvis", title: "Hey Jarvis", size: 1_271_370, cached: false, threshold: 0.5 },
];

/** A tiny image whose pixels differ per state, so a preview can be checked byte for byte. */
export function previewBuffer(state: OverlayPreviewState): ArrayBuffer {
  const width = 2;
  const height = 2;
  const bytes = new Uint8ClampedArray(8 + width * height * 4);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, width, true);
  view.setUint32(4, height, true);
  const shade = OVERLAY_PREVIEW_STATES.indexOf(state) + 1;
  for (let at = 8; at < bytes.length; at += 4) {
    bytes[at] = shade * 40;
    bytes[at + 1] = 255 - shade * 40;
    bytes[at + 2] = shade * 20;
    bytes[at + 3] = 255;
  }
  return bytes.buffer;
}

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
  word_list: true,
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
  word_list: true,
};

export const WHISPER: SpeechModelInfo = {
  name: "whisper_turbo_cpu",
  title: "Whisper Turbo · CPU (experimental)",
  description: "Experimental. OpenAI's Whisper Turbo on the CPU.",
  size: 574_926_293,
  source: "https://huggingface.co/ggerganov/whisper.cpp",
  license: "MIT",
  languages: ["English", "German"],
  cached: false,
  word_list: false,
};

/** A backend whose calls are spies and whose events the test emits. */
export function stubBackend(settings: Settings = SETTINGS, initial: Status = { state: "listening" }) {
  const statusListeners: ((status: Status) => void)[] = [];
  const levelListeners: ((level: number) => void)[] = [];
  const audioListeners: ((audio: AudioStatus) => void)[] = [];
  const rebuildingListeners: ((rebuilding: boolean) => void)[] = [];
  const elsewhereListeners: (() => void)[] = [];
  const schemeListeners: ((dark: boolean) => void)[] = [];
  let dark = false;
  const speechListeners: ((progress: SpeechModelProgress) => void)[] = [];
  const pageListeners: ((page: string) => void)[] = [];
  const updateListeners: ((state: UpdateState) => void)[] = [];
  let updateState: UpdateState = { phase: { state: "idle" }, notes: null, notice: null };
  let audioState: AudioStatus = {
    selected: settings.microphone,
    active: initial.state === "paused" ? null : (settings.microphone ?? "Built-in"),
    paused: initial.state === "paused",
    problem: null,
  };
  const emitAudio = (next: AudioStatus): void => {
    audioState = next;
    for (const listener of audioListeners) listener(next);
  };
  // What the desktop holds after each patch it accepted.
  let held = structuredClone(settings);
  const applied: Settings[] = [];
  const learningListeners: ((learning: Learning) => void)[] = [];
  let learning: Learning = { suggestions: [], limit: null };
  const tellLearning = (): void => {
    for (const listener of learningListeners) listener(structuredClone(learning));
  };
  const backend = {
    getStatus: vi.fn(() => Promise.resolve(initial)),
    onStatus: vi.fn((listener: (status: Status) => void) => statusListeners.push(listener)),
    onLevel: vi.fn((listener: (level: number) => void) => levelListeners.push(listener)),
    getAudio: vi.fn(() => Promise.resolve(structuredClone(audioState))),
    onAudio: vi.fn((listener: (audio: AudioStatus) => void) => audioListeners.push(listener)),
    getRebuilding: vi.fn(() => Promise.resolve(false)),
    onRebuilding: vi.fn((listener: (rebuilding: boolean) => void) => rebuildingListeners.push(listener)),
    onSettingsChangedElsewhere: vi.fn((listener: () => void) => elsewhereListeners.push(listener)),
    getSettings: vi.fn(() => Promise.resolve(structuredClone(settings))),
    setDark: vi.fn<(value: boolean) => Promise<void>>(() => Promise.resolve()),
    onThemeChanged: vi.fn((listener: (value: boolean) => void) => {
      schemeListeners.push(listener);
      listener(dark);
    }),
    previewOverlay: vi.fn<(state: OverlayPreviewState, scale: number) => Promise<ArrayBuffer>>((state) => Promise.resolve(previewBuffer(state))),
    settingsReady: vi.fn(() => Promise.resolve()),
    saveSettings: vi.fn<(patch: SettingsPatch) => Promise<void>>((patch) => {
      held = Object.assign({ writing: null, microphone: null }, applyPatch(held, patch));
      applied.push(structuredClone(held));
      return Promise.resolve();
    }),
    setListening: vi.fn<(on: boolean) => Promise<void>>(() => Promise.resolve()),
    dismissError: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    copyReport: vi.fn<(message: string, causes: string[]) => Promise<void>>(() => Promise.resolve()),
    listMicrophones: vi.fn(() => Promise.resolve(["USB Microphone", "Built-in"])),
    openRecordings: vi.fn(() => Promise.resolve()),
    getHistory: vi.fn<() => Promise<HistoryEntry[]>>(() => Promise.resolve([])),
    deleteHistoryEntry: vi.fn<(id: number) => Promise<void>>(() => Promise.resolve()),
    clearHistory: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    getDraft: vi.fn<() => Promise<Draft | null>>(() => Promise.resolve(null)),
    getLearning: vi.fn<() => Promise<Learning>>(() => Promise.resolve(structuredClone(learning))),
    acceptSuggestion: vi.fn<(word: string) => Promise<void>>((word) => {
      learning = { ...learning, suggestions: learning.suggestions.filter((suggestion) => suggestion.word !== word) };
      settings = { ...structuredClone(settings), stt: { ...settings.stt, phrases: [...(settings.stt.phrases ?? []), word] } };
      tellLearning();
      return Promise.resolve();
    }),
    dismissSuggestion: vi.fn<(word: string) => Promise<void>>((word) => {
      learning = { ...learning, suggestions: learning.suggestions.filter((suggestion) => suggestion.word !== word) };
      tellLearning();
      return Promise.resolve();
    }),
    onLearningChanged: vi.fn((listener: (learning: Learning) => void) => { learningListeners.push(listener); }),
    editDraft: vi.fn<(text: string) => Promise<void>>(() => Promise.resolve()),
    copyText: vi.fn<(text: string) => Promise<void>>(() => Promise.resolve()),
    discardDraft: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    onOpenPage: vi.fn((listener: (page: string) => void) => pageListeners.push(listener)),
    getHoldKeyUnavailable: vi.fn<() => Promise<string | null>>(() => Promise.resolve(null)),
    getAppStylesUnavailable: vi.fn<() => Promise<string | null>>(() => Promise.resolve(null)),
    getCurrentApp: vi.fn<() => Promise<CurrentApp | null>>(() => Promise.resolve(null)),
    getKeyDefaults: vi.fn<() => Promise<KeyDefaults>>(() =>
      Promise.resolve({
        keys: [
          { action: "dictate", mode: "hold", key: "Alt+KeyZ" },
          { action: "write", mode: "hold", key: "Alt+KeyX" },
        ],
      }),
    ),
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
    getChatModel: vi.fn<(build?: ChatBuild) => Promise<ChatModelInfo>>((build) => Promise.resolve({ ...CHAT_MODEL, ...(build ? { build, build_label: build === "cpu" ? "CPU" : "Vulkan" } : {}) })),
    downloadChatModel: vi.fn<(build: ChatBuild) => Promise<void>>(() => Promise.resolve()),
    removeChatModel: vi.fn<() => Promise<void>>(() => Promise.resolve()),
    onChatModelProgress: vi.fn<(listener: (progress: SpeechModelProgress) => void) => void>(),
    listSpeechModels: vi.fn<() => Promise<SpeechModelInfo[]>>(() => Promise.resolve([{ ...NEMOTRON }, { ...UNIFIED }])),
    downloadSpeechModel: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    useLighterModel: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
    cancelSpeechDownload: vi.fn<(name: string) => Promise<void>>(() => Promise.resolve()),
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
    /** The settings after each change the desktop accepted, in order. */
    applied: () => applied,
    emit: (status: Status) => {
      for (const listener of statusListeners) listener(status);
      const paused = status.state === "paused";
      if ((status.state === "paused" || status.state === "listening" || status.state === "keyboardOnly") && paused !== audioState.paused) {
        emitAudio({
          selected: audioState.selected,
          active: paused ? null : (audioState.selected ?? "Built-in"),
          paused,
          problem: null,
        });
      }
    },
    audio: emitAudio,
    rebuilding: (rebuilding: boolean) => {
      for (const listener of rebuildingListeners) listener(rebuilding);
    },
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
    schemeChanged: (value: boolean) => {
      dark = value;
      for (const listener of schemeListeners) listener(value);
    },
    learningChanged: (next: Learning) => {
      learning = structuredClone(next);
      tellLearning();
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
