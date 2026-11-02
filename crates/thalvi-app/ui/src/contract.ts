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
  | { state: "failed"; notice: string }
  | { state: "loading"; notice: string }; // the speech model is being fetched or loaded

export interface Level {
  level: number;
}

export type Ending = "paste" | "submit" | "task" | "scratch" | "cancel";
export type ScriptActionKind = "open_url" | "focus_app" | "discord_message";
export type SecondaryApi = "openai" | "meta";
export type SttEngine = "server" | "local";

export interface HeadSettings {
  model: string;
  threshold: number;
}

export interface SecondarySettings {
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

/** The desktop config file with every field filled, plus the read-only `recordings`. */
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
  recordings: { metrics: boolean; text: boolean; audio: boolean };
}

/** The Tauri commands and events, behind one interface so tests inject a fake. */
export interface Backend {
  getStatus(): Promise<Status>;
  onStatus(listener: (status: Status) => void): void;
  onLevel(listener: (level: number) => void): void;
  getSettings(): Promise<Settings>;
  saveSettings(settings: Settings): Promise<void>;
  setListening(on: boolean): Promise<void>;
  cancelCapture(): Promise<void>;
  listMicrophones(): Promise<string[]>;
  openRecordings(): Promise<void>;
  appVersion(): Promise<string>;
}

export function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return "Something went wrong.";
}
