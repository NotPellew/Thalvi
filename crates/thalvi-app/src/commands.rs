//! The commands the UI calls; errors reach it as plain strings.

#![expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands a command its state by value"
)]

use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use thalvi_desktop::audio;
use thalvi_desktop::host::Host;
use thalvi_desktop::metrics::Recorder;

use crate::app::AppState;
use crate::overlay;
use crate::runner::Control;
use crate::settings::settings_json;
use crate::status::Status;

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> Status {
    state.link.status()
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
    let config = state.store.save(settings)?;
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

#[tauri::command]
pub async fn list_microphones() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        audio::list_microphones().map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The microphones could not be listed: {error}"))?
}

#[tauri::command]
pub async fn open_recordings(state: State<'_, AppState>) -> Result<(), String> {
    let folder = Recorder::user_dir().map_err(|error| error.to_string())?;
    if !folder.is_dir() {
        return Err(
            "Nothing has been recorded yet. Start Thalvi with THALVI_RECORD set to record."
                .to_owned(),
        );
    }
    let host = state.host();
    tauri::async_runtime::spawn_blocking(move || {
        host.open_folder(&folder).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The folder could not be opened: {error}"))?
}
