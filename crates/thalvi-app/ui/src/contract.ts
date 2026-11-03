// The Rust app <-> UI contract; mirrors crates/thalvi-app/src/status.rs and the desktop config.

export type Route = "paste" | "submit" | "task";

export type Status =
  | { state: "paused" }
  | { state: "listening" }
  | { state: "starting"; target: string | null }
  | { state: "capturing"; target: string | null; committed: string; partial: string | null }
  | {
      state: "pending";
      target: string | null;
      route: Route;
      text: string;
      waitingForQuiet: boolean;
      pauseMs: number;
    }
  | { state: "working"; target: string | null; route: Route; cancellable: boolean } // cancellable until delivery starts
  | { state: "done"; target: string | null; route: Route; notice: string }
  | { state: "failed"; notice: string; headline: string; causes: string[] } // headline: notice in one short line for the overlay; causes: the technical chain behind it, for the error report
  | { state: "loading"; notice: string }; // the speech model is being fetched or loaded

export interface Level {
  level: number;
}

/** The saved microphone choice, the device actually open, and whether listening is paused. */
export interface AudioStatus {
  selected: string | null;
  active: string | null;
  paused: boolean;
}

export type Ending = "paste" | "submit" | "task" | "scratch" | "cancel";
export type ScriptActionKind = "open_url" | "focus_app" | "discord_message";
export type SecondaryApi = "openai" | "meta";
export type SttEngine = "server" | "local";
export type Look = "gunmetal" | "holo" | "neon";

export interface HeadSettings {
  model: string;
  threshold: number;
}

export interface SecondarySettings {
  enabled: boolean;
  url: string;
  model: string;
  api: SecondaryApi;
  api_key: string;
}

export interface AppSettings {
  classes: string[];
  executables: string[];
  input_keys: string[];
}

export interface ScriptSettings {
  phrase: string;
  action: ScriptActionKind;
  destination: string;
  label?: string | null;
}

/** The desktop config file with every field filled. */
export interface Settings {
  server: string;
  wake: { capture: HeadSettings; conversation: HeadSettings };
  cues: { wake: boolean; provisional: boolean; cancelled: boolean; accepted: boolean; error: boolean };
  ending_pause: { paste: number; submit: number; task: number };
  capture_limits: { idle: number; max: number };
  endings: Record<string, Ending>;
  stt: {
    engine: SttEngine;
    /** A catalog name or a model folder; only with the local engine, and its default when absent. */
    model?: string;
    secondary?: SecondarySettings | null;
  };
  apps: Record<string, AppSettings>;
  scripts: ScriptSettings[];
  overlay: { show: boolean; draft: boolean };
  microphone: string | null;
  look: Look;
  recordings: { metrics: boolean; text: boolean; audio: boolean };
  /** The key held to dictate, such as `Alt+KeyZ`; only Windows reads it. */
  hold_key: string;
  /** Whether finished captures are kept on this computer, and how many. */
  history: { enabled: boolean; limit: number };
}

export type HistoryOutcome = "accepted" | "cancelled" | "failed";

/** One kept dictation; `target` is the window it was meant for, when there was one. */
export interface HistoryEntry {
  id: number;
  timestamp_ms: number;
  text: string;
  target: string | null;
  outcome: HistoryOutcome;
}

/** A speech model Thalvi can download, as the Rust catalog describes it. */
export interface SpeechModelInfo {
  name: string;
  title: string;
  description: string;
  /** Bytes to download in all. */
  size: number;
  /** The model's page. */
  source: string;
  /** Every file is on disk and matches its checksum. */
  cached: boolean;
}

/** One step of a speech model download; `message` is the plain reason, only with `failed`. */
export interface SpeechModelProgress {
  name: string;
  percent: number;
  status: "verifying" | "downloading" | "done" | "failed";
  message: string | null;
}

/** The Tauri commands and events, behind one interface so tests inject a fake. */
export interface Backend {
  getStatus(): Promise<Status>;
  onStatus(listener: (status: Status) => void): void;
  onLevel(listener: (level: number) => void): void;
  getAudio(): Promise<AudioStatus>;
  onAudio(listener: (audio: AudioStatus) => void): void;
  /** Settings were saved, from any window. */
  onSettingsSaved(listener: () => void): void;
  getSettings(): Promise<Settings>;
  saveSettings(settings: Settings): Promise<void>;
  setListening(on: boolean): Promise<void>;
  cancelCapture(): Promise<void>;
  /** Whether a click on the overlay reaches it; Hyprland gives a window that never takes focus no clicks. */
  overlayTakesClicks(): Promise<boolean>;
  /** The user dismissed the latest error; the overlay stops showing it. */
  dismissError(): Promise<void>;
  /** Puts an error report for `message` on the clipboard; rejects with the plain reason it could not. */
  copyReport(message: string, causes: string[]): Promise<void>;
  listMicrophones(): Promise<string[]>;
  openRecordings(): Promise<void>;
  /** The kept dictations, newest first; rejects with the plain reason the history cannot be read. */
  getHistory(): Promise<HistoryEntry[]>;
  deleteHistoryEntry(id: number): Promise<void>;
  /** Removes every entry, and a damaged history file with them. */
  clearHistory(): Promise<void>;
  /** Whether the OS starts Thalvi at login, read from the OS, and why Thalvi could not put its entry back on start. */
  getStartAtLogin(): Promise<SwitchState>;
  /** Adds or removes the startup entry; rejects unless the OS then holds it as asked. */
  setStartAtLogin(on: boolean): Promise<void>;
  /** Whether Thalvi is in the app menu, or why it offers no switch for that. */
  getMenuEntry(): Promise<SwitchState>;
  /** Adds or removes the app menu entry; rejects with the plain reason it could not. */
  setMenuEntry(on: boolean): Promise<void>;
  /** Puts the line that starts Thalvi at login on the clipboard, when the state carries one. */
  copyLoginLine(): Promise<void>;
  /** Resolves when the folder holds a speech model; rejects with the plain reason it does not. */
  validateSpeechModelFolder(path: string): Promise<void>;
  listSpeechModels(): Promise<SpeechModelInfo[]>;
  /** Resolves when the model is on disk and verified; rejects with the plain reason it is not. */
  downloadSpeechModel(name: string): Promise<void>;
  onSpeechModelProgress(listener: (progress: SpeechModelProgress) => void): void;
  /** Opens the model's page in the browser. */
  openSpeechModelSource(name: string): Promise<void>;
  appVersion(): Promise<string>;
}

/** An OS setting Thalvi can switch, or the reason it offers no switch and a line to do it by hand. */
export type SwitchState =
  | { kind: "available"; on: boolean; problem: string | null }
  | { kind: "unavailable"; reason: string; line: string | null };

export function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return "Something went wrong.";
}
