// The real backend: Tauri commands and events.

import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { DownloadFailure, errorText } from "./contract.ts";
import type { AudioStatus, Backend, ChatModelInfo, CurrentApp, Draft, HistoryEntry, KeyDefaults, Level, LookStyles, OverlayPreviewState, SwitchState, Settings, SpeechModelInfo, SpeechModelProgress, Status, UpdateState, WakeModelInfo } from "./contract.ts";

/** The rejection of `download_speech_model`, which carries `{ message, causes }`, as a typed error. */
function downloadFailure(rejection: unknown): DownloadFailure {
  if (typeof rejection === "object" && rejection !== null && "message" in rejection && typeof rejection.message === "string" && "causes" in rejection && Array.isArray(rejection.causes)) {
    return new DownloadFailure(rejection.message, rejection.causes.filter((cause): cause is string => typeof cause === "string"));
  }
  return new DownloadFailure(errorText(rejection), []);
}

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
    getRebuilding: () => invoke<boolean>("get_rebuilding"),
    onRebuilding: (listener) => {
      void listen<boolean>("thalvi://rebuilding", (event) => {
        listener(event.payload);
      });
    },
    onSettingsChangedElsewhere: (listener) => {
      void listen("thalvi://settings-elsewhere", () => {
        listener();
      });
    },
    getSettings: () => invoke<Settings>("get_settings"),
    getLookStyles: () => invoke<LookStyles>("get_look_styles"),
    previewOverlay: (state: OverlayPreviewState, scale: number) => invoke<ArrayBuffer>("preview_overlay", { state, scale }),
    settingsReady: () => invoke<undefined>("settings_ready"),
    saveSettings: (patch) => invoke<undefined>("save_settings", { patch }),
    setListening: (on) => invoke<undefined>("set_listening", { on }),
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
    getAppStylesUnavailable: () => invoke<string | null>("get_app_styles_unavailable"),
    getCurrentApp: () => invoke<CurrentApp | null>("get_current_app"),
    getKeyDefaults: () => invoke<KeyDefaults>("get_key_defaults"),
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
    getChatModel: (build) => invoke<ChatModelInfo>("get_chat_model", { build }),
    downloadChatModel: (build) => invoke<undefined>("download_chat_model", { build }).catch((rejection: unknown) => { throw downloadFailure(rejection); }),
    removeChatModel: () => invoke<undefined>("remove_chat_model"),
    onChatModelProgress: (listener) => { void listen<SpeechModelProgress>("thalvi://chat-model-progress", (event) => { listener(event.payload); }); },
    listSpeechModels: () => invoke<SpeechModelInfo[]>("list_speech_models"),
    downloadSpeechModel: (name) =>
      invoke<undefined>("download_speech_model", { name }).catch((rejection: unknown) => {
        throw downloadFailure(rejection);
      }),
    useLighterModel: (name) =>
      invoke<undefined>("use_lighter_model", { name }).catch((rejection: unknown) => {
        throw downloadFailure(rejection);
      }),
    cancelSpeechDownload: (name) => invoke<undefined>("cancel_speech_download", { name }),
    onSpeechModelProgress: (listener) => {
      void listen<SpeechModelProgress>("thalvi://speech-model-progress", (event) => {
        listener(event.payload);
      });
    },
    speechDevice: (name) => invoke<string>("speech_device", { name }),
    openSpeechModelSource: (name) => invoke<undefined>("open_speech_model_source", { name }),
    checkSpeechEntry: (entry) => invoke<undefined>("check_speech_entry", { entry }),
    wordListSupport: (entry) => invoke<boolean>("word_list_support", { entry }),
    appVersion: getVersion,
    getUpdateState: () => invoke<UpdateState>("get_update_state"),
    onUpdate: (listener) => {
      void listen<UpdateState>("thalvi://update", (event) => {
        listener(event.payload);
      });
    },
    checkForUpdates: () => invoke<undefined>("check_for_updates"),
    openReleaseNotes: () => invoke<undefined>("open_release_notes"),
  };
}
