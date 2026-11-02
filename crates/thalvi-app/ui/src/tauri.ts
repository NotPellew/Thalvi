// The real backend: Tauri commands and events.

import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Backend, Level, Settings, Status } from "./contract.ts";

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
    getSettings: () => invoke<Settings>("get_settings"),
    saveSettings: (settings) => invoke<undefined>("save_settings", { settings }),
    setListening: (on) => invoke<undefined>("set_listening", { on }),
    cancelCapture: () => invoke<undefined>("cancel_capture"),
    listMicrophones: () => invoke<string[]>("list_microphones"),
    openRecordings: () => invoke<undefined>("open_recordings"),
    appVersion: getVersion,
  };
}
