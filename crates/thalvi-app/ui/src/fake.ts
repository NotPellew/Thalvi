// Dev-only fake backend for `npm run dev` with `?fake`; `?at=<n>` pins the overlay to the n-th status of the script.

import type { Backend, Settings, Status } from "./contract.ts";

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
  stt: { secondary: null },
  apps: { slack: { classes: ["Slack"], executables: ["slack.exe"], input_keys: [] } },
  scripts: [{ phrase: "open the docs", action: "open_url", destination: "https://example.com/docs", label: "docs" }],
  overlay: { show: true, draft: true },
  microphone: null,
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
    { state: "working", target: slack, route: "submit" },
    { state: "done", target: slack, route: "submit", notice: "Sent" },
    { state: "paused" },
    { state: "listening" },
    { state: "starting", target: slack },
    { state: "capturing", target: slack, committed: "Remind me to call", partial: null },
    { state: "pending", target: slack, route: "paste", text: "Remind me to call", waitingForQuiet: false, pauseMs: 700 },
    { state: "done", target: slack, route: "paste", notice: "Pasted" },
    { state: "starting", target: null },
    { state: "failed", notice: "The speech server is not reachable. Check the server address in settings." },
  ];
}

export function fakeBackend(params: URLSearchParams): Backend {
  const statuses = script();
  const pinned = params.get("at");
  let index = 0;
  let status: Status = { state: "listening" };
  let level = 0;
  const statusListeners: ((status: Status) => void)[] = [];
  const levelListeners: ((level: number) => void)[] = [];
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
    level = status.state === "starting" || status.state === "capturing" || status.state === "pending"
      ? Math.max(0, Math.min(1, level * 0.5 + Math.random() * 0.7))
      : 0;
    for (const listener of levelListeners) listener(level);
  }, 80);

  let saved = structuredClone(SETTINGS);
  return {
    getStatus: () => Promise.resolve(status),
    onStatus: (listener) => statusListeners.push(listener),
    onLevel: (listener) => levelListeners.push(listener),
    getSettings: () => Promise.resolve(structuredClone(saved)),
    saveSettings: (next) => {
      if (!/^wss?:\/\//.test(next.server)) {
        return Promise.reject(new Error("The desktop config is not valid: the server address must start with ws:// or wss://."));
      }
      saved = structuredClone(next);
      return Promise.resolve();
    },
    setListening: (on) => {
      emit(on ? { state: "listening" } : { state: "paused" });
      return Promise.resolve();
    },
    cancelCapture: () => {
      emit({ state: "listening" });
      return Promise.resolve();
    },
    listMicrophones: () => Promise.resolve(["Built-in Audio Analog Stereo", "USB Microphone"]),
    openRecordings: () => Promise.resolve(),
    appVersion: () => Promise.resolve("0.1.0 (fake)"),
  };
}
