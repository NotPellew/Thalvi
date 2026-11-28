// The Rust app <-> UI contract; mirrors crates/thalvi-app/src/status.rs and the desktop config.

import type { SettingsPatch } from "./patch.ts";

export type Route = "paste" | "write" | "submit" | "task";

/** What a failure stopped: only `delivery` means a capture's text did not reach its destination. */
export type FailureKind = "delivery" | "recognizer" | "other";

export type Status =
  | { state: "paused" }
  | { state: "listening" }
  | { state: "keyboardOnly" } // listening with wake words off: only the hold keys start a capture
  | { state: "starting"; target: string | null }
  | {
      state: "capturing";
      target: string | null;
      committed: string;
      partial: string | null;
      /** Something to tell the user while the capture goes on; it holds no dictated words and goes with the next words. */
      notice: string | null;
    }
  | {
      state: "pending";
      target: string | null;
      route: Route;
      text: string;
      waitingForQuiet: boolean;
      pauseMs: number;
      /** The desktop can only copy dictation to the clipboard, so a paste or submit ending is a copy. */
      copies: boolean;
    }
  | { state: "writing"; target: string | null; text: string } // the writing model is at work; text is what has come in so far; cancellable until the paste
  | { state: "thinking"; target: string | null } // the writing model is reasoning and has written no text yet; cancellable
  | { state: "working"; target: string | null; route: Route; cancellable: boolean } // cancellable until delivery starts
  | { state: "transcribing"; target: string | null; route: Route; cancellable: boolean }
  | { state: "finishing"; target: string | null; route: Route; cancellable: boolean } // a released hold whose recognizer is catching up; cancellable
  | { state: "cleaningUp"; target: string | null } // a dictation is being cleaned before it is pasted; cancellable
  | { state: "done"; target: string | null; route: Route; notice: string }
  | { state: "failed"; notice: string; headline: string; causes: string[]; kind: FailureKind; offer: LighterModel | null } // headline: notice in one short line for the overlay; causes: the technical chain behind it, for the error report; offer: a lighter on-device model to switch to after a cant-keep-up failure
  | { state: "loading"; notice: string }; // the speech model is being fetched or loaded

/** A lighter on-device model offered after this computer could not keep up. */
export interface LighterModel {
  /** The catalog name the settings store. */
  name: string;
  /** What Settings calls it. */
  title: string;
}

export interface Level {
  level: number;
}

/** The saved microphone choice, the device actually open, and whether listening is paused. */
export interface AudioStatus {
  selected: string | null;
  active: string | null;
  paused: boolean;
  /** Why the selected device could not be opened while `active` stays open; null otherwise. */
  problem: string | null;
}

export type Ending = "paste" | "write" | "submit" | "task" | "scratch" | "cancel";
export type ScriptActionKind = "open_url" | "focus_app" | "discord_message";
export type SecondaryApi = "openai" | "meta";
/** How a live OpenAI-compatible entry sends each capture. */
export type OpenAiRequest = "transcriptions" | "chat";
export type Look = "gunmetal" | "holo" | "neon";
/** An overlay state the renderer can draw a preview for. */
export type OverlayPreviewState = "live" | "quiet" | "working" | "pending" | "done" | "failed";
export const OVERLAY_PREVIEW_STATES: readonly OverlayPreviewState[] = ["live", "quiet", "working", "pending", "done", "failed"];
/** Which builds Thalvi keeps itself updated to. */
export type UpdateChannel = "stable" | "development";

export interface HeadSettings {
  model: string;
  threshold: number;
}

interface EntryBase {
  /** Never shown; the key saved for an entry belongs to its id. */
  id: string;
  name: string;
  /** The one live entry in use. */
  active: boolean;
}

type LiveBase = EntryBase;

/** A recognizer the desktop connects to itself, speaking Meta's realtime ASR protocol. */
export interface RealtimeEntry extends LiveBase {
  kind: "realtime";
  address: string;
  model: string;
  /** Present only when the user replaced or cleared the key; absent keeps the saved one. */
  api_key?: string;
  /** Whether a key is saved; the key itself never reaches the window. */
  has_key?: boolean;
}

/** The speech model on this computer. */
export interface OnDeviceEntry extends LiveBase {
  kind: "on_device";
  /** A catalog name or a model folder; its default when absent. */
  model?: string;
}

/** A buffered OpenAI-compatible transcription endpoint used for live dictation. */
export interface OpenAiEntry extends LiveBase {
  kind: "open_ai";
  address: string;
  model: string;
  request: OpenAiRequest;
  /** A two-letter ISO 639-1 code, or empty to let the server detect it. */
  language: string;
  api_key?: string;
  has_key?: boolean;
}

/** A Writing prompt for dictations that begin with `phrase`, which the desktop stores normalized. */
export interface WritingOpening {
  phrase: string;
  prompt: string;
}

export interface WritingAppStyle {
  app: string;
  title?: string;
  prompt: string;
}

export interface CurrentApp {
  app: string;
  title: string;
}

export type ChatBuild = "cpu" | "vulkan";
export interface ChatModelInfo {
  title: string;
  size: number;
  license: string;
  languages: string[];
  build: ChatBuild;
  build_label: string;
  model_installed: boolean;
  runtime_installed: boolean;
}

export interface WritingSettings {
  url?: string;
  model?: string;
  local?: boolean;
  chat_build?: ChatBuild;
  api_key?: string;
  has_key?: boolean;
  /** Set only when it differs from `Settings.writing_default_prompt`; Settings shows the prompt in effect. */
  prompt?: string;
  /** Absent when there are none. */
  openings?: WritingOpening[];
  /** Absent when there are none. */
  apps?: WritingAppStyle[];
  /** Present only when dictation is cleaned before it is pasted. */
  clean_dictation?: boolean;
  /** Whether a delivered dictation gets its number words as digits; absent with no writing settings (the default, on). */
  numbers_as_digits?: boolean;
}
export type LiveEntry = RealtimeEntry | OnDeviceEntry | OpenAiEntry;
export type SpeechEntry = LiveEntry;

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
  /** `enabled` is the Wake words switch: off, only the hold keys start a capture. */
  wake: { capture: HeadSettings; conversation: HeadSettings; enabled: boolean };
  cues: { wake: boolean; provisional: boolean; cancelled: boolean; accepted: boolean; error: boolean };
  ending_pause: { paste: number; submit: number; task: number };
  capture_limits: { idle: number; max: number };
  endings: Record<string, Ending>;
  stt: {
    /** Live entries, exactly one active. */
    entries: SpeechEntry[];
    /** Spellings recognition favours; absent when empty. */
    phrases?: string[];
    /** Whether Thalvi watches a field after a paste. */
    learn_from_corrections: boolean;
  };
  apps: Record<string, AppSettings>;
  scripts: ScriptSettings[];
  overlay: { show: boolean; draft: boolean };
  microphone: string | null;
  look: Look;
  update_channel: UpdateChannel;
  recordings: { metrics: boolean; text: boolean; audio: boolean };
  /** The keys Settings → Keys lists, in order. */
  keys: KeyBind[];
  /** The chat endpoint that writes dictation out; null until one is set up. */
  writing: WritingSettings | null;
  /** The writing prompt used until the user sets their own; read only. */
  writing_default_prompt: string;
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
  /** The user's cleanup of the pasted span; absent when none was kept. */
  corrected?: string;
}

export type DraftOutcome = "delivered" | "cancelled" | "failed" | "maybePartial";

/** The last dictation, held in memory; `text` is the user's edit and `original` what was heard. */
export interface Draft {
  /** Changes when a different dictation becomes the draft, so the text is reloaded only then. */
  id: number;
  original: string;
  text: string;
  /** The window the dictation was meant for, when one was bound. */
  target: string | null;
  outcome: DraftOutcome;
  /** A newer dictation ended while this one had unsent edits; discarding this one shows it. */
  newerWaiting: boolean;
  /** The words as recognized, when a cleanup pasted something else. */
  raw?: string | null;
  /** The user's cleanup of the pasted span, which Copy uses without their own edits. */
  corrected: string | null;
}

/** A name the user corrected, with what Thalvi wrote. */
export interface Suggestion {
  word: string;
  heard: string;
}

/** The in-memory suggestions and why the last field could not be watched. */
export interface Learning {
  suggestions: Suggestion[];
  limit: string | null;
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
  license: string;
  /** The languages it recognizes. */
  languages: string[];
  /** Every file is on disk and matches its checksum. */
  cached: boolean;
  /** Whether recognition favours the word list. */
  word_list: boolean;
}

/** A wake head Thalvi can download, as the Rust catalog describes it. */
export interface WakeModelInfo {
  name: string;
  title: string;
  /** Bytes to download. */
  size: number;
  /** The file is on disk at its pinned size. */
  cached: boolean;
  /** The threshold the head starts at. */
  threshold: number;
}

/** One step of a speech model download; `message` is the plain reason and `causes` what lay behind it, only with `failed`. */
export interface SpeechModelProgress {
  name: string;
  percent: number;
  status: "verifying" | "downloading" | "done" | "failed";
  message: string | null;
  causes: string[];
}

/** Why a speech model download failed: the plain reason, and the causes a copied report adds. */
export class DownloadFailure extends Error {
  constructor(
    message: string,
    readonly causes: string[],
  ) {
    super(message);
  }
}

/** What the update is doing; mirrors `Phase` in crates/thalvi-app/src/update.rs. */
export type UpdatePhase =
  | { state: "idle" } // no check has run yet
  | { state: "checking" }
  | { state: "upToDate" }
  | { state: "downloading"; version: string; percent: number | null }
  | { state: "ready"; version: string } // downloaded and verified; installs once Thalvi is idle
  | { state: "installing"; version: string }
  | { state: "failed"; message: string } // the installed version is as it was
  | { state: "unsupported"; message: string }; // this install cannot update itself

export interface UpdateState {
  phase: UpdatePhase;
  /** The release page of the version being fetched or waiting. */
  notes: string | null;
  /** What happened at this start, such as "Updated to 0.2.0." */
  notice: string | null;
}

/** The Tauri commands and events, behind one interface so tests inject a fake. */
export interface Backend {
  getStatus(): Promise<Status>;
  onStatus(listener: (status: Status) => void): void;
  onLevel(listener: (level: number) => void): void;
  getAudio(): Promise<AudioStatus>;
  onAudio(listener: (audio: AudioStatus) => void): void;
  /** Whether a saved change rebuilds the engine and the next dictation waits for it, even while a capture runs on the old one. */
  getRebuilding(): Promise<boolean>;
  onRebuilding(listener: (rebuilding: boolean) => void): void;
  /** An agent changed the saved settings or a switch the window reads from the OS; the window's own saves never fire this. */
  onSettingsChangedElsewhere(listener: () => void): void;
  getSettings(): Promise<Settings>;
  /** Straight-alpha RGBA of one overlay state: width and height as u32 LE, then width * height * 4 bytes. */
  previewOverlay(state: OverlayPreviewState, scale: number): Promise<ArrayBuffer>;
  /** Says this window has its styles, so a show asked for meanwhile can happen. */
  settingsReady(): Promise<void>;
  /** Applies a merge patch of the settings that changed: it is merged into the config the desktop holds when it runs, so it can't undo a change saved since. Rejects with the plain reason it was refused. */
  saveSettings(patch: SettingsPatch): Promise<void>;
  setListening(on: boolean): Promise<void>;
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
  /** The last dictation, or null when there is none. */
  getDraft(): Promise<Draft | null>;
  /** The pending name suggestions and why the last field could not be watched. */
  getLearning(): Promise<Learning>;
  /** Adds a suggested name to the word list through the normal settings save. */
  acceptSuggestion(word: string): Promise<void>;
  dismissSuggestion(word: string): Promise<void>;
  /** Fires when a suggestion or the field limit changes. */
  onLearningChanged(listener: (learning: Learning) => void): void;
  /** Keeps the edited text; nothing is recognized, copied or typed. */
  editDraft(text: string): Promise<void>;
  /** Puts the text on the clipboard through the desktop; rejects with the plain reason it could not. */
  copyText(text: string): Promise<void>;
  discardDraft(): Promise<void>;
  /** The tray asked for the settings page with this id. */
  onOpenPage(listener: (page: string) => void): void;
  /** Why this desktop offers no hold key to dictate with, or null where it does. */
  getHoldKeyUnavailable(): Promise<string | null>;
  /** Why this desktop cannot match app styles, or null where it can. */
  getAppStylesUnavailable(): Promise<string | null>;
  /** The app focused before Settings, or the last bound target; null when neither is known. */
  getCurrentApp(): Promise<CurrentApp | null>;
  /** The keys this desktop starts with, which Settings restores. */
  getKeyDefaults(): Promise<KeyDefaults>;
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
  listWakeModels(): Promise<WakeModelInfo[]>;
  /** Resolves when the dictation wake choice can be used; rejects with the plain reason it cannot. */
  validateWakeModels(capture: string): Promise<void>;
  /** Resolves when the catalog wake head is on disk and verified; rejects with the plain reason it is not. */
  downloadWakeModel(name: string): Promise<void>;
  /** The `.onnx` file the user picked, or null when they cancelled. */
  pickWakeModelFile(): Promise<string | null>;
  getChatModel(build?: ChatBuild): Promise<ChatModelInfo>;
  downloadChatModel(build: ChatBuild): Promise<void>;
  removeChatModel(): Promise<void>;
  onChatModelProgress(listener: (progress: SpeechModelProgress) => void): void;
  listSpeechModels(): Promise<SpeechModelInfo[]>;
  /** Resolves when the model is on disk and verified; rejects with a `DownloadFailure`. */
  downloadSpeechModel(name: string): Promise<void>;
  /** Downloads the lighter model a failure offers and makes it the model in use; resolves the same way. */
  useLighterModel(name: string): Promise<void>;
  /** Stops a speech model download that is running; files already downloaded stay. */
  cancelSpeechDownload(name: string): Promise<void>;
  onSpeechModelProgress(listener: (progress: SpeechModelProgress) => void): void;
  /** Where the model runs, or will run when it loads, in plain words: the GPU, or the CPU and why. */
  speechDevice(name: string): Promise<string>;
  /** Opens the model's page in the browser. */
  openSpeechModelSource(name: string): Promise<void>;
  /** Resolves when the entry's fields can be used; rejects with the plain reason they cannot. */
  checkSpeechEntry(entry: SpeechEntry): Promise<void>;
  /** Whether a direct realtime recognizer boosts the word list. */
  wordListSupport(entry: LiveEntry): Promise<boolean>;
  appVersion(): Promise<string>;
  getUpdateState(): Promise<UpdateState>;
  onUpdate(listener: (state: UpdateState) => void): void;
  /** Checks now, or joins the check that is running; the answer arrives as the update's state changes. */
  checkForUpdates(): Promise<void>;
  /** Opens the release page of the update being fetched or waiting; rejects with the plain reason it could not. */
  openReleaseNotes(): Promise<void>;
}

/** The key rows a desktop starts with, named as in `Settings`. */
export interface KeyDefaults {
  keys: KeyBind[];
}

/** What a keybind starts: the words pasted as dictated, or written out first. */
export type KeyAction = "dictate" | "write";

/** How a keybind works: held, pressed to start or end, or only starting or only ending. */
export type KeyMode = "hold" | "toggle" | "start" | "stop";

/** One row of Settings → Keys. */
export interface KeyBind {
  action: KeyAction;
  mode: KeyMode;
  /** The physical key, such as `Alt+KeyZ` or `F9`. */
  key: string;
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
