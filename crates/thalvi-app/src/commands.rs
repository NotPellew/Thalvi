//! The commands the UI calls; errors reach it as plain strings.

#![expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands a command its state by value"
)]

use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, State};
use thalvi_desktop::audio;
use thalvi_desktop::draft::DraftView;
use thalvi_desktop::history::{Entry, HistoryError, Store as History};
use thalvi_desktop::host::{Desktop, Host, WebUrl};
use thalvi_desktop::metrics::{Event, Recorder};
use thalvi_desktop::models::{
    CatalogSpeech, ModelError, ModelStore, SpeechModelInfo, SpeechProgress,
};
use thalvi_desktop::report::{Report, System};
use thalvi_desktop::transducer::check_folder;

use crate::app::AppState;
use crate::menu_entry;
use crate::overlay;
use crate::runner::{AudioStatus, Control};
use crate::settings::settings_json;
use crate::startup;
use crate::status::Status;
use crate::switch::{SwitchState, Unavailable};

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> Status {
    state.link.status()
}

#[tauri::command]
pub fn overlay_takes_clicks() -> bool {
    Desktop::OVERLAY_TAKES_CLICKS
}

#[tauri::command]
pub fn get_audio(state: State<'_, AppState>) -> AudioStatus {
    state.link.audio()
}

/// Refuses a microphone change while listening is not paused, before anything is written.
///
/// # Errors
/// The plain refusal when the saved device would change while the microphone may be open.
fn ensure_microphone_change_allowed(
    settings: &Value,
    current: Option<&str>,
    paused: bool,
) -> Result<(), String> {
    let incoming = match settings.get("microphone") {
        Some(Value::String(name)) => Some(name.trim().to_owned()),
        _ => None,
    };
    if incoming.as_deref() != current && !paused {
        return Err(
            "Pause listening before changing the microphone. Turn listening off, then save again."
                .to_owned(),
        );
    }
    Ok(())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<Value, String> {
    settings_json(&state.config())
}

#[tauri::command]
pub fn save_settings(
    settings: Value,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    ensure_microphone_change_allowed(
        &settings,
        state.config().microphone.as_deref(),
        state.link.audio().paused,
    )?;
    let config = state.store.save(settings, &state.config())?;
    let look = state.config().look;
    state.apply(&config);
    if config.look != look {
        // The settings are saved either way; the overlay keeps its old place.
        if let Err(error) = overlay::replace(&app, &state.host()) {
            eprintln!("The overlay could not be placed again for the new look: {error}");
        }
    }
    // The overlay window is hidden and idle, so it re-reads the settings when told.
    let _ = app.emit("thalvi://settings", ());
    Ok(())
}

#[tauri::command]
pub fn set_listening(on: bool, state: State<'_, AppState>) {
    state.link.send(Control::SetListening(on));
}

#[tauri::command]
pub fn cancel_capture(state: State<'_, AppState>) {
    state.link.send(Control::Cancel);
}

/// Hides the overlay's failure; the status stays failed until the next capture starts.
#[tauri::command]
pub fn dismiss_error(app: AppHandle, state: State<'_, AppState>) {
    state.overlay.apply(&app, overlay::Visibility::Hide);
}

/// Puts an error report on the clipboard through the desktop; a web view's own clipboard is refused
/// or ignored on some desktops.
#[tauri::command]
pub async fn copy_report(
    message: String,
    causes: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    copy(&app, &state, &message, &causes).await
}

/// Copies the report for an error to the clipboard through the desktop.
///
/// # Errors
/// Why the clipboard could not be written, in plain words.
pub async fn copy(
    app: &AppHandle,
    state: &AppState,
    message: &str,
    causes: &[String],
) -> Result<(), String> {
    let report = Report::new(
        message,
        causes,
        &app.package_info().version.to_string(),
        System::current(),
        &state.config(),
    );
    let host = state.host();
    tauri::async_runtime::spawn_blocking(move || {
        host.copy(&report.text()).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The report could not be copied: {error}"))?
}

#[tauri::command]
pub async fn list_microphones() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        audio::list_microphones().map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The microphones could not be listed: {error}"))?
}

/// Checks that a folder holds a speech model, before it is saved as `stt.model`.
#[tauri::command]
pub async fn validate_speech_model_folder(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        check_folder(Path::new(path.trim())).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The folder could not be checked: {error}"))?
}

/// The catalog speech models and whether each is downloaded.
#[tauri::command]
pub async fn list_speech_models() -> Result<Vec<SpeechModelInfo>, String> {
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    Ok(store.speech_models().await)
}

/// What `thalvi://speech-model-progress` carries.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct SpeechModelProgress<'a> {
    name: &'a str,
    percent: u8,
    status: &'static str,
    /// Why it failed, in plain words; only with `failed`.
    message: Option<String>,
}

impl<'a> SpeechModelProgress<'a> {
    fn of(name: &'a str, progress: SpeechProgress) -> Self {
        let (percent, status) = match progress {
            SpeechProgress::Verifying => (0, "verifying"),
            SpeechProgress::Downloading { percent } => (percent, "downloading"),
            SpeechProgress::Done => (100, "done"),
        };
        Self {
            name,
            percent,
            status,
            message: None,
        }
    }

    fn failed(name: &'a str, message: String) -> Self {
        Self {
            name,
            percent: 0,
            status: "failed",
            message: Some(message),
        }
    }
}

/// Downloads a catalog speech model, telling the UI how far it is.
#[tauri::command]
pub async fn download_speech_model(
    name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let model = CatalogSpeech::named(&name)
        .ok_or_else(|| format!("{name} is not a speech model Thalvi can download."))?;
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    let started = Instant::now();
    let result = state
        .speech_downloads
        .run(model.name(), &store, &model.assets(), |progress| {
            let _ = app.emit(
                "thalvi://speech-model-progress",
                SpeechModelProgress::of(model.name(), progress),
            );
        })
        .await;
    if matches!(result, Err(ModelError::AlreadyDownloading { .. })) {
        return result.map_err(|error| error.to_string());
    }
    recorder.metric(
        Event::SpeechDownload,
        if result.is_ok() { "ok" } else { "error" },
        Some(started.elapsed()),
        json!({"model": model.name()}),
    );
    result.map_err(|error| {
        let message = error.to_string();
        let _ = app.emit(
            "thalvi://speech-model-progress",
            SpeechModelProgress::failed(model.name(), message.clone()),
        );
        message
    })
}

/// Opens a catalog speech model's page in the browser.
#[tauri::command]
pub async fn open_speech_model_source(
    name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let model = CatalogSpeech::named(&name)
        .ok_or_else(|| format!("{name} is not a speech model Thalvi knows."))?;
    let url = WebUrl::parse(model.source()).map_err(|error| error.to_string())?;
    let host = state.host();
    tauri::async_runtime::spawn_blocking(move || {
        host.open_url(&url).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The page could not be opened: {error}"))?
}

#[tauri::command]
pub async fn open_recordings(state: State<'_, AppState>) -> Result<(), String> {
    let folder = Recorder::dir().map_err(|error| error.to_string())?;
    if !folder.is_dir() {
        return Err(
            "Nothing has been recorded yet. Turn a recording on in the settings.".to_owned(),
        );
    }
    let host = state.host();
    tauri::async_runtime::spawn_blocking(move || {
        host.open_folder(&folder).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The folder could not be opened: {error}"))?
}

/// Runs one history operation off the UI thread; errors reach the UI as plain strings.
async fn with_history<T: Send + 'static>(
    state: &AppState,
    operation: impl FnOnce(&History) -> Result<T, HistoryError> + Send + 'static,
) -> Result<T, String> {
    let history = std::sync::Arc::clone(&state.history);
    tauri::async_runtime::spawn_blocking(move || {
        operation(&history).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The history could not be reached: {error}"))?
}

/// The kept dictations, newest first.
#[tauri::command]
pub async fn get_history(state: State<'_, AppState>) -> Result<Vec<Entry>, String> {
    with_history(&state, History::list).await
}

#[tauri::command]
pub async fn delete_history_entry(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    with_history(&state, move |history| history.delete(id)).await
}

/// Removes every entry, and a damaged history file with them.
#[tauri::command]
pub async fn clear_history(state: State<'_, AppState>) -> Result<(), String> {
    with_history(&state, History::clear).await
}

/// The last dictation, if there is one.
#[tauri::command]
pub fn get_draft(state: State<'_, AppState>) -> Option<DraftView> {
    state.drafts.view()
}

/// Keeps the edited text. Nothing is recognized, copied or typed.
#[tauri::command]
pub fn edit_draft(text: String, state: State<'_, AppState>) -> Result<(), String> {
    state.drafts.edit(text).map_err(|error| error.to_string())
}

/// Puts the edited text on the clipboard.
#[tauri::command]
pub async fn copy_draft(text: String, state: State<'_, AppState>) -> Result<(), String> {
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    state
        .drafts
        .copy(&state.delivery, &recorder, text)
        .await
        .map_err(|error| error.to_string())
}

/// Types the edited text into the window the dictation was bound to, without pressing Enter.
#[tauri::command]
pub async fn paste_draft(text: String, state: State<'_, AppState>) -> Result<(), String> {
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    let capture_active = state.link.status().is_busy();
    state
        .drafts
        .paste(&state.delivery, &recorder, text, capture_active)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn discard_draft(state: State<'_, AppState>) {
    state.drafts.discard();
}

#[tauri::command]
pub fn get_start_at_login(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SwitchState, String> {
    startup::switch(&app, state.login_problem()).map_err(|error| error.to_string())
}

/// Puts the `hyprland.conf` line that starts Thalvi at login on the clipboard.
#[tauri::command]
pub async fn copy_login_line(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let Ok(SwitchState::Unavailable(Unavailable {
        line: Some(line), ..
    })) = startup::switch(&app, None)
    else {
        return Err("There is no line to copy.".to_owned());
    };
    let host = state.host();
    tauri::async_runtime::spawn_blocking(move || {
        host.copy(&line).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The line could not be copied: {error}"))?
}

/// Adds or removes the startup entry and remembers the choice; fails unless the OS then holds it as
/// asked.
#[tauri::command]
pub fn set_start_at_login(
    on: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    startup::set(&app, on, || state.remember_login(on)).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn get_menu_entry(state: State<'_, AppState>) -> Result<SwitchState, String> {
    menu_entry::switch(&menu_entry::System::from_env(), state.menu_problem())
}

/// Adds or removes Thalvi's app menu entry and remembers the choice.
#[tauri::command]
pub fn set_menu_entry(on: bool, state: State<'_, AppState>) -> Result<(), String> {
    menu_entry::set(&menu_entry::System::from_env(), on, || {
        state.remember_menu_entry(on)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn progress_reaches_the_ui_as_name_percent_and_status() {
        for (progress, expected) in [
            (
                SpeechProgress::Verifying,
                json!({"name": "m", "percent": 0, "status": "verifying", "message": null}),
            ),
            (
                SpeechProgress::Downloading { percent: 42 },
                json!({"name": "m", "percent": 42, "status": "downloading", "message": null}),
            ),
            (
                SpeechProgress::Done,
                json!({"name": "m", "percent": 100, "status": "done", "message": null}),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(SpeechModelProgress::of("m", progress)).ok(),
                Some(expected)
            );
        }
        assert_eq!(
            serde_json::to_value(SpeechModelProgress::failed("m", "No network.".to_owned())).ok(),
            Some(json!({"name": "m", "percent": 0, "status": "failed", "message": "No network."}))
        );
    }

    #[test]
    fn a_microphone_change_needs_a_pause_but_other_saves_do_not() {
        let refusal =
            "Pause listening before changing the microphone. Turn listening off, then save again.";
        for (settings, current, paused, expected) in [
            (json!({"microphone": "USB Mic"}), None, false, Err(refusal)),
            (
                json!({"microphone": null}),
                Some("USB Mic"),
                false,
                Err(refusal),
            ),
            (json!({"microphone": "USB Mic"}), None, true, Ok(())),
            (json!({"microphone": null}), Some("USB Mic"), true, Ok(())),
            (
                json!({"microphone": "USB Mic"}),
                Some("USB Mic"),
                false,
                Ok(()),
            ),
            (json!({"microphone": null}), None, false, Ok(())),
            (
                json!({"microphone": " USB Mic "}),
                Some("USB Mic"),
                false,
                Ok(()),
            ),
        ] {
            assert_eq!(
                ensure_microphone_change_allowed(&settings, current, paused),
                expected.map_err(str::to_owned),
                "{settings} current={current:?} paused={paused}"
            );
        }
    }
}
