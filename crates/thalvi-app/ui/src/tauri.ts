// The real backend: Tauri commands and events.

import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type { AudioStatus, Backend, Draft, HistoryEntry, Level, SwitchState, Settings, SpeechModelInfo, SpeechModelProgress, Status, WakeModelInfo } from "./contract.ts";

export function tauriBackend(): Backend {
  return {
    getStatus: () => invoke<Status>("get_status"),
    onStatus: (listener) => {
      void listen<Status>("thalvi://status", (event) => {
        listener(event.payload);
      });
    },
    onLevel: (listener) => {
      void listen<Level>("thalvi://level", (event) => {
        listener(event.payload.level);
      });
    },
    getAudio: () => invoke<AudioStatus>("get_audio"),
    onAudio: (listener) => {
      void listen<AudioStatus>("thalvi://audio", (event) => {
        listener(event.payload);
      });
    },
    onSettingsSaved: (listener) => {
      void listen("thalvi://settings", () => {
        listener();
      });
    },
    onSettingsChangedElsewhere: (listener) => {
      void listen("thalvi://settings-elsewhere", () => {
        listener();
      });
    },
    getSettings: () => invoke<Settings>("get_settings"),
    saveSettings: (settings) => invoke<undefined>("save_settings", { settings }),
    setListening: (on) => invoke<undefined>("set_listening", { on }),
    cancelCapture: () => invoke<undefined>("cancel_capture"),
    overlayTakesClicks: () => invoke<boolean>("overlay_takes_clicks"),
    dismissError: () => invoke<undefined>("dismiss_error"),
    copyReport: (message, causes) => invoke<undefined>("copy_report", { message, causes }),
    listMicrophones: () => invoke<string[]>("list_microphones"),
    openRecordings: () => invoke<undefined>("open_recordings"),
    getHistory: () => invoke<HistoryEntry[]>("get_history"),
    deleteHistoryEntry: (id) => invoke<undefined>("delete_history_entry", { id }),
    clearHistory: () => invoke<undefined>("clear_history"),
    getDraft: () => invoke<Draft | null>("get_draft"),
    editDraft: (text) => invoke<undefined>("edit_draft", { text }),
    copyText: (text) => invoke<undefined>("copy_text", { text }),
    discardDraft: () => invoke<undefined>("discard_draft"),
    onOpenPage: (listener) => {
      void listen<string>("thalvi://open-page", (event) => {
        listener(event.payload);
      });
    },
    getHoldKeyUnavailable: () => invoke<string | null>("get_hold_key_unavailable"),
    getStartAtLogin: () => invoke<SwitchState>("get_start_at_login"),
    setStartAtLogin: (on) => invoke<undefined>("set_start_at_login", { on }),
    getMenuEntry: () => invoke<SwitchState>("get_menu_entry"),
    setMenuEntry: (on) => invoke<undefined>("set_menu_entry", { on }),
    copyLoginLine: () => invoke<undefined>("copy_login_line"),
    validateSpeechModelFolder: (path) => invoke<undefined>("validate_speech_model_folder", { path }),
    listWakeModels: () => invoke<WakeModelInfo[]>("list_wake_models"),
    validateWakeModels: (capture) => invoke<undefined>("validate_wake_model_choices", { capture }),
    downloadWakeModel: (name) => invoke<undefined>("download_wake_model", { name }),
    pickWakeModelFile: async () => {
      const picked = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "Wake model", extensions: ["onnx"] }],
      });
      return typeof picked === "string" ? picked : null;
    },
    listSpeechModels: () => invoke<SpeechModelInfo[]>("list_speech_models"),
    downloadSpeechModel: (name) => invoke<undefined>("download_speech_model", { name }),
    onSpeechModelProgress: (listener) => {
      void listen<SpeechModelProgress>("thalvi://speech-model-progress", (event) => {
        listener(event.payload);
      });
    },
    speechDevice: (name) => invoke<string>("speech_device", { name }),
    openSpeechModelSource: (name) => invoke<undefined>("open_speech_model_source", { name }),
    checkSpeechEntry: (entry) => invoke<undefined>("check_speech_entry", { entry }),
    wordListSupport: (entry) => invoke<boolean>("word_list_support", { entry }),
    shareWordList: () => invoke<undefined>("share_word_list"),
    appVersion: getVersion,
  };
}
