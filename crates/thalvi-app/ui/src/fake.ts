// Dev-only fake backend for `npm run dev` with `?fake`; `?at=<n>` pins the overlay to the n-th status of the script and `?look=holo` or `?look=gunmetal` picks the look.

import type { AudioStatus, Backend, Settings, SpeechModelInfo, SpeechModelProgress, Status } from "./contract.ts";
import { LOOKS } from "./look.ts";

const SETTINGS: Settings = {
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
    "finish dictation": "paste",
    submit: "submit",
    "do that": "task",
    "scratch that": "scratch",
    "never mind": "cancel",
  },
  stt: { engine: "server", secondary: null },
  apps: { slack: { classes: ["Slack"], executables: ["slack.exe"], input_keys: [] } },
  scripts: [{ phrase: "open the docs", action: "open_url", destination: "https://example.com/docs", label: "docs" }],
  overlay: { show: true, draft: true },
  microphone: null,
  look: "gunmetal",
  recordings: { metrics: true, text: false, audio: false },
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
    { state: "pending", target: slack, route: "submit", text: SENTENCE, waitingForQuiet: true, pauseMs: 2500 },
    { state: "pending", target: slack, route: "submit", text: SENTENCE, waitingForQuiet: false, pauseMs: 2500 },
    { state: "working", target: slack, route: "submit", cancellable: false },
    { state: "done", target: slack, route: "submit", notice: "Sent" },
    { state: "paused" },
    { state: "listening" },
    { state: "starting", target: slack },
    { state: "capturing", target: slack, committed: "Remind me to call", partial: null },
    { state: "pending", target: slack, route: "paste", text: "Remind me to call", waitingForQuiet: false, pauseMs: 700 },
    { state: "done", target: slack, route: "paste", notice: "Pasted" },
    { state: "starting", target: null },
    { state: "failed", notice: "The speech server is not reachable. Check the server address in settings." },
    { state: "loading", notice: "Loading the speech model." },
    { state: "starting", target: "Firefox" },
    { state: "capturing", target: "Firefox", committed: "", partial: null },
    { state: "pending", target: "Firefox", route: "task", text: "open the calendar", waitingForQuiet: false, pauseMs: 2500 },
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
  const speechModels: SpeechModelInfo[] = [
    {
      name: "nemotron_160ms",
      title: "Nemotron Speech Streaming 0.6B",
      description: "NVIDIA's English streaming recognizer, int8, answering every 160 ms. Runs on the CPU.",
      size: 661_919_416,
      source: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-speech-streaming-en-0.6b-160ms-int8-2026-04-25",
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
      if (!/^wss?:\/\//.test(next.server)) {
        return Promise.reject(new Error("The desktop config is not valid: the server address must start with ws:// or wss://."));
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
    listMicrophones: () => Promise.resolve([...microphones]),
    openRecordings: () => Promise.resolve(),
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
    appVersion: () => Promise.resolve("0.1.0 (fake)"),
  };
}
