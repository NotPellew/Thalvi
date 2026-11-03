// The real backend: Tauri commands and events.

import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { AudioStatus, Backend, Level, SwitchState, Settings, SpeechModelInfo, SpeechModelProgress, Status } from "./contract.ts";

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
    getSettings: () => invoke<Settings>("get_settings"),
    saveSettings: (settings) => invoke<undefined>("save_settings", { settings }),
    setListening: (on) => invoke<undefined>("set_listening", { on }),
    cancelCapture: () => invoke<undefined>("cancel_capture"),
    dismissError: () => invoke<undefined>("dismiss_error"),
    copyReport: (message, causes) => invoke<undefined>("copy_report", { message, causes }),
    listMicrophones: () => invoke<string[]>("list_microphones"),
    openRecordings: () => invoke<undefined>("open_recordings"),
    getStartAtLogin: () => invoke<SwitchState>("get_start_at_login"),
    setStartAtLogin: (on) => invoke<undefined>("set_start_at_login", { on }),
    getMenuEntry: () => invoke<SwitchState>("get_menu_entry"),
    setMenuEntry: (on) => invoke<undefined>("set_menu_entry", { on }),
    copyLoginLine: () => invoke<undefined>("copy_login_line"),
    validateSpeechModelFolder: (path) => invoke<undefined>("validate_speech_model_folder", { path }),
    listSpeechModels: () => invoke<SpeechModelInfo[]>("list_speech_models"),
    downloadSpeechModel: (name) => invoke<undefined>("download_speech_model", { name }),
    onSpeechModelProgress: (listener) => {
      void listen<SpeechModelProgress>("thalvi://speech-model-progress", (event) => {
        listener(event.payload);
      });
    },
    openSpeechModelSource: (name) => invoke<undefined>("open_speech_model_source", { name }),
    appVersion: getVersion,
  };
}
