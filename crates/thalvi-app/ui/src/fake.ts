// Dev-only fake backend for `npm run dev` with `?fake`; `?at=<n>` pins the overlay to the n-th status of the script and `?look=holo` or `?look=gunmetal` picks the look.

import type { AudioStatus, Backend, Draft, HistoryEntry, Settings, SpeechModelInfo, SpeechModelProgress, Status, WakeModelInfo } from "./contract.ts";
import { LOOKS } from "./look.ts";

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

const SETTINGS: Settings = {
  wake: {
    capture: { model: "hey_thalvi", threshold: 0.7 },
    conversation: { model: "hey_mycroft", threshold: 0.5 },
  },
  cues: { wake: true, provisional: true, cancelled: true, accepted: true, error: true },
  ending_pause: { paste: 0.7, submit: 2.5, task: 2.5 },
  capture_limits: { idle: 60, max: 300 },
  endings: {
    "paste that": "paste",
    "finish dictation": "paste",
    submit: "submit",
    "do that": "task",
    "scratch that": "scratch",
    "never mind": "cancel",
  },
  stt: { entries: [{ kind: "thalvi_server", id: "thalvi-server", name: "Thalvi server", active: true, address: "ws://127.0.0.1:8080" }] },
  apps: { slack: { classes: ["Slack"], executables: ["slack.exe"], input_keys: [] } },
  scripts: [{ phrase: "open the docs", action: "open_url", destination: "https://example.com/docs", label: "docs" }],
  overlay: { show: true, draft: true },
  microphone: null,
  look: "gunmetal",
  recordings: { metrics: true, text: false, audio: false },
  hold_key: "Alt+KeyZ",
  history: { enabled: true, limit: 200 },
};

const SENTENCE = "Hey, I'm running a few minutes late, can we push the call to half past?";

function script(): Status[] {
  const words = SENTENCE.split(" ");
  const slack = "Slack";
  return [
    { state: "listening" },
    { state: "starting", target: slack },
    { state: "capturing", target: slack, committed: "", partial: words.slice(0, 3).join(" ") },
    { state: "capturing", target: slack, committed: words.slice(0, 7).join(" "), partial: words.slice(7, 10).join(" ") },
    { state: "capturing", target: slack, committed: SENTENCE.repeat(3), partial: "and bring the" },
    { state: "pending", target: slack, route: "submit", text: SENTENCE, waitingForQuiet: true, pauseMs: 2500, copies: false },
    { state: "pending", target: slack, route: "submit", text: SENTENCE, waitingForQuiet: false, pauseMs: 2500, copies: false },
    { state: "working", target: slack, route: "submit", cancellable: false },
    { state: "done", target: slack, route: "submit", notice: "Sent" },
    { state: "paused" },
    { state: "listening" },
    { state: "starting", target: slack },
    { state: "capturing", target: slack, committed: "Remind me to call", partial: null },
    { state: "pending", target: slack, route: "paste", text: "Remind me to call", waitingForQuiet: false, pauseMs: 700, copies: false },
    { state: "done", target: slack, route: "paste", notice: "Pasted" },
    { state: "starting", target: null },
    { state: "failed", notice: "Thalvi could not reach the speech server at ws://127.0.0.1:8080 (connection refused). Start the Thalvi server, or fix Settings → Speech → Server address.", headline: "Can't reach speech server — Settings", causes: [], kind: "other" },
    { state: "loading", notice: "Loading the speech model." },
    { state: "starting", target: "Firefox" },
    { state: "capturing", target: "Firefox", committed: "", partial: null },
    { state: "pending", target: "Firefox", route: "task", text: "open the calendar", waitingForQuiet: false, pauseMs: 2500, copies: false },
    { state: "working", target: "Firefox", route: "task", cancellable: true },
    { state: "done", target: "Firefox", route: "task", notice: "Opened the calendar" },
  ];
}

export function fakeBackend(params: URLSearchParams): Backend {
  const statuses = script();
  const pinned = params.get("at");
  let index = 0;
  let status: Status = { state: "listening" };
  let level = 0;
  const requested = params.get("look");
  const look = LOOKS.find(([name]) => name === requested)?.[0] ?? "gunmetal";
  const savedListeners: (() => void)[] = [];
  const statusListeners: ((status: Status) => void)[] = [];
  const levelListeners: ((level: number) => void)[] = [];
  const audioListeners: ((audio: AudioStatus) => void)[] = [];
  const speechListeners: ((progress: SpeechModelProgress) => void)[] = [];
  const wakeModels: WakeModelInfo[] = [
    { name: "hey_thalvi", title: "Hey Thalvi", size: 395_406, cached: true, threshold: 0.7 },
    { name: "hey_mycroft", title: "Hey Mycroft", size: 857_691, cached: false, threshold: 0.5 },
    { name: "hey_jarvis", title: "Hey Jarvis", size: 1_271_370, cached: false, threshold: 0.5 },
  ];
  const speechModels: SpeechModelInfo[] = [
    {
      name: "nemotron_160ms",
      title: "Nemotron Streaming",
      description: "From NVIDIA: the lightest on the CPU, and text appears soonest. Mostly without punctuation. The default.",
      size: 661_919_416,
      source: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25",
      license: "NVIDIA Open Model License",
      languages: ["English"],
      cached: false,
    },
    {
      name: "unified_160ms",
      title: "Nemotron Unified",
      description: "From NVIDIA: fewest word errors in our tests, with punctuation and capitals. It reads the last six seconds again every 160 ms, so it needs a fast CPU and text appears about half a second later. Exported by Thalvi.",
      size: 646_518_349,
      source: "https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b",
      license: "OpenMDW-1.1",
      languages: UNIFIED_LANGUAGES,
      cached: false,
    },
  ];
  const speak = (progress: SpeechModelProgress): void => {
    for (const listener of speechListeners) listener(progress);
  };
  const microphones = ["Built-in Audio Analog Stereo", "USB Microphone"];
  let audioState: AudioStatus = { selected: null, active: "Built-in Audio Analog Stereo", paused: false };
  const emitAudio = (next: AudioStatus): void => {
    audioState = next;
    for (const listener of audioListeners) listener(next);
  };
  const activeFor = (selected: string | null): string | null => {
    if (selected === null) return "Built-in Audio Analog Stereo";
    return microphones.includes(selected) ? selected : null;
  };
  const emit = (next: Status): void => {
    status = next;
    for (const listener of statusListeners) listener(next);
  };

  if (pinned !== null) {
    status = statuses[Number(pinned)] ?? status;
  } else {
    setInterval(() => {
      index = (index + 1) % statuses.length;
      const next = statuses[index];
      if (next) emit(next);
    }, 2200);
  }
  setInterval(() => {
    level = status.state === "listening" || status.state === "starting" || status.state === "capturing" || status.state === "pending"
      ? Math.max(0, Math.min(1, level * 0.5 + Math.random() * 0.7))
      : 0;
    for (const listener of levelListeners) listener(level);
  }, 80);

  const now = Date.now();
  let history: HistoryEntry[] = [
    { id: 3, timestamp_ms: now - 60_000, text: "Remind me to call the dentist tomorrow morning.", target: "Slack", outcome: "accepted" },
    { id: 2, timestamp_ms: now - 3_600_000, text: "Thalvi could not reach the window that was open when you started.", target: null, outcome: "failed" },
    { id: 1, timestamp_ms: now - 86_400_000, text: "Hey, I'm running a few minutes late", target: "Firefox", outcome: "cancelled" },
  ];
  let draft: Draft | null = {
    id: 1,
    original: "Remind me to call the dentist tomorow morning.",
    text: "Remind me to call the dentist tomorow morning.",
    target: "Slack",
    outcome: "failed",
    newerWaiting: false,
  };
  let startAtLogin = false;
  let menuEntry = false;
  const hyprland = params.has("hyprland");
  const installer = params.has("installer");
  let saved = { ...structuredClone(SETTINGS), look };
  return {
    getStatus: () => Promise.resolve(status),
    onStatus: (listener) => statusListeners.push(listener),
    onLevel: (listener) => levelListeners.push(listener),
    getAudio: () => Promise.resolve(structuredClone(audioState)),
    onAudio: (listener) => audioListeners.push(listener),
    onSettingsSaved: (listener) => savedListeners.push(listener),
    getSettings: () => Promise.resolve(structuredClone(saved)),
    saveSettings: (next) => {
      if (next.stt.entries.filter((entry) => entry.kind !== "second_pass" && entry.active).length !== 1) {
        return Promise.reject(new Error("The desktop config is not valid: exactly one live speech server must be active."));
      }
      if (next.microphone !== saved.microphone && !audioState.paused) {
        return Promise.reject(new Error("Pause listening before changing the microphone. Turn listening off, then save again."));
      }
      saved = structuredClone(next);
      emitAudio({ selected: next.microphone, active: audioState.paused ? null : activeFor(next.microphone), paused: audioState.paused });
      for (const listener of savedListeners) listener();
      return Promise.resolve();
    },
    setListening: (on) => {
      emitAudio({ selected: audioState.selected, active: on ? activeFor(audioState.selected) : null, paused: !on });
      emit(on ? { state: "listening" } : { state: "paused" });
      return Promise.resolve();
    },
    cancelCapture: () => {
      emit({ state: "listening" });
      return Promise.resolve();
    },
    overlayTakesClicks: () => Promise.resolve(true),
    dismissError: () => Promise.resolve(),
    copyReport: () => Promise.resolve(),
    listMicrophones: () => Promise.resolve([...microphones]),
    openRecordings: () => Promise.resolve(),
    getHistory: () => Promise.resolve(structuredClone(history)),
    deleteHistoryEntry: (id) => {
      history = history.filter((entry) => entry.id !== id);
      return Promise.resolve();
    },
    clearHistory: () => {
      history = [];
      return Promise.resolve();
    },
    getDraft: () => Promise.resolve(structuredClone(draft)),
    editDraft: (text) => {
      if (draft) draft.text = text;
      return Promise.resolve();
    },
    copyText: () => Promise.resolve(),
    discardDraft: () => {
      draft = null;
      return Promise.resolve();
    },
    onOpenPage: () => undefined,
    getHoldKeyUnavailable: () => Promise.resolve(null),
    getStartAtLogin: () =>
      Promise.resolve(
        hyprland
          ? {
              kind: "unavailable",
              reason: "Hyprland does not start login entries by itself, so a switch here would do nothing. To start Thalvi at login, add this line to hyprland.conf:",
              line: "exec-once = '/home/me/My Apps/Thalvi.AppImage' --login",
            }
          : { kind: "available", on: startAtLogin, problem: null },
      ),
    setStartAtLogin: (on) => {
      startAtLogin = on;
      return Promise.resolve();
    },
    getMenuEntry: () =>
      Promise.resolve(
        installer
          ? { kind: "unavailable", reason: "Added by the installer; uninstalling removes it.", line: null }
          : { kind: "available", on: menuEntry, problem: null },
      ),
    setMenuEntry: (on) => {
      menuEntry = on;
      return Promise.resolve();
    },
    copyLoginLine: () => Promise.resolve(),
    validateSpeechModelFolder: (path) =>
      path.includes("missing") ? Promise.reject(new Error("Missing required file: tokens.txt")) : Promise.resolve(),
    listWakeModels: () => Promise.resolve(structuredClone(wakeModels)),
    validateWakeModels: (capture, conversation) => {
      if (capture === conversation) return Promise.reject(new Error("The capture and conversation wake models must be different."));
      for (const model of [capture, conversation]) {
        if (!wakeModels.some((info) => info.name === model) && (model.includes("bad") || model.includes("missing") || !model.endsWith(".onnx"))) {
          return Promise.reject(new Error(`The wake model ${model} could not be loaded: not a wake model.`));
        }
      }
      return Promise.resolve();
    },
    downloadWakeModel: (name) => {
      const model = wakeModels.find((candidate) => candidate.name === name);
      if (model) model.cached = true;
      return Promise.resolve();
    },
    pickWakeModelFile: () => Promise.resolve("/models/custom.onnx"),
    listSpeechModels: () => Promise.resolve(structuredClone(speechModels)),
    downloadSpeechModel: (name) =>
      new Promise((resolve) => {
        speak({ name, percent: 0, status: "verifying", message: null });
        let percent = 0;
        const timer = setInterval(() => {
          percent += 10;
          if (percent < 100) {
            speak({ name, percent, status: "downloading", message: null });
            return;
          }
          clearInterval(timer);
          const model = speechModels.find((candidate) => candidate.name === name);
          if (model) model.cached = true;
          speak({ name, percent: 100, status: "done", message: null });
          resolve();
        }, 400);
      }),
    onSpeechModelProgress: (listener) => speechListeners.push(listener),
    openSpeechModelSource: () => Promise.resolve(),
    checkSpeechEntry: (entry) => {
      const wanted = entry.kind === "second_pass" ? /^https?:\/\// : /^wss?:\/\//;
      return "address" in entry && !wanted.test(entry.address)
        ? Promise.reject(new Error(`Speech server "${entry.name}": the address must start with ${entry.kind === "second_pass" ? "http:// or https://" : "ws:// or wss://"}.`))
        : Promise.resolve();
    },
    wordListSupport: () => Promise.resolve(true),
    shareWordList: () => Promise.resolve(),
    appVersion: () => Promise.resolve("0.1.0 (fake)"),
  };
}
