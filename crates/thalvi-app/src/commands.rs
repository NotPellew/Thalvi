//! The commands the UI calls; errors reach it as plain strings.

#![expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands a command its state by value"
)]

use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, State};
use thalvi_desktop::audio;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::draft::DraftView;
use thalvi_desktop::entries::{self, LiveKind, Realtime};
use thalvi_desktop::history::{Entry, HistoryError, Store as History};
use thalvi_desktop::host::{Desktop, Host, WebUrl};
use thalvi_desktop::metrics::{Event, Recorder};
use thalvi_desktop::models::{
    CatalogHead, CatalogSpeech, ModelError, ModelStore, SpeechAssets, SpeechDownloads,
    SpeechModelInfo, SpeechProgress, WakeModelInfo,
};
use thalvi_desktop::phrases;
use thalvi_desktop::recognizer::ServerUrl;
use thalvi_desktop::report::{Report, System};
use thalvi_desktop::transducer::check_folder;
use thalvi_desktop::wake::validate_wake_model;

use crate::app::AppState;
use crate::menu_entry;
use crate::overlay;
use crate::runner::{AudioStatus, Control};
use crate::settings::settings_json;
use crate::startup;
use crate::status::Status;
use crate::switch::{SwitchState, Unavailable};
use crate::update::Snapshot;

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> Status {
    state.link.status()
}

#[tauri::command]
pub fn overlay_takes_clicks(state: State<'_, AppState>) -> bool {
    state.host().overlay_takes_clicks()
}

#[tauri::command]
pub fn get_audio(state: State<'_, AppState>) -> AudioStatus {
    state.link.audio()
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
    save(&app, &state, |_| Ok(((), Some(settings)))).map(drop)
}

/// Saves and applies what Settings sends, and what an outside program sends in its place.
/// `form` builds the settings form from the config read under the save lock and returns a value
/// for the caller with the form, or `None` to save nothing; the flag tells whether one was saved.
///
/// # Errors
/// The plain reason the settings were refused, in which case nothing changed.
pub fn save<T>(
    app: &AppHandle,
    state: &AppState,
    form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<Value>), String>,
) -> Result<(T, bool), String> {
    let (value, saved) = state.save_form(form)?;
    if saved.is_none() {
        return Ok((value, false));
    }
    // The overlay window is hidden and idle, so it re-reads the settings when told.
    let _ = app.emit("thalvi://settings", ());
    Ok((value, true))
}

/// What the update is doing, for a window that opens after the events were sent.
#[tauri::command]
pub fn get_update_state(state: State<'_, AppState>) -> Snapshot {
    state.updates.snapshot()
}

/// Checks for an update now, or joins the check that is running; the answer comes as the update's
/// state changes.
#[tauri::command]
pub fn check_for_updates(state: State<'_, AppState>) {
    state.updates.check_now();
}

/// Opens the release page of the update being fetched or waiting.
#[tauri::command]
pub async fn open_release_notes(state: State<'_, AppState>) -> Result<(), String> {
    let notes = state
        .updates
        .snapshot()
        .notes
        .ok_or("There are no release notes to open yet.")?;
    let url = WebUrl::parse(&notes).map_err(|error| error.to_string())?;
    let host = state.host();
    tauri::async_runtime::spawn_blocking(move || {
        host.open_url(&url).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The page could not be opened: {error}"))?
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
pub fn dismiss_error(state: State<'_, AppState>) {
    state.overlay.apply(overlay::Visibility::Hide);
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

/// The catalog wake heads and whether each is downloaded.
#[tauri::command]
pub async fn list_wake_models() -> Result<Vec<WakeModelInfo>, String> {
    wake_models().await
}

/// The catalog wake heads and whether each is downloaded.
///
/// # Errors
/// Why the model store cannot be reached.
pub async fn wake_models() -> Result<Vec<WakeModelInfo>, String> {
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    Ok(store.wake_models().await)
}

/// Checks both chosen wake models the way listening will use them, before they are saved.
///
/// # Errors
/// The plain reason a choice cannot be used.
#[tauri::command]
pub async fn validate_wake_model_choices(capture: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        validate_wake_model(&capture).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("The wake models could not be checked: {error}"))?
}

/// Downloads one catalog wake head, so its badge can show it ready before first use.
///
/// # Errors
/// The plain reason the download failed.
#[tauri::command]
pub async fn download_wake_model(name: String, state: State<'_, AppState>) -> Result<(), String> {
    download_wake(&state, &name).await
}

/// Downloads one catalog wake head, as Settings does.
///
/// # Errors
/// The plain reason the download failed.
pub async fn download_wake(state: &AppState, name: &str) -> Result<(), String> {
    let head = CatalogHead::named(name)
        .ok_or_else(|| format!("{name} is not a wake model Thalvi can download."))?;
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    let started = Instant::now();
    let result = store.fetch(&head.asset()).await;
    recorder.metric(
        Event::WakeLoad,
        if result.is_ok() { "ok" } else { "error" },
        Some(started.elapsed()),
        json!({"model": head.name(), "source": "settings_download"}),
    );
    result.map(|_| ()).map_err(|error| error.to_string())
}

/// The catalog speech models and whether each is downloaded; the files counted are those the model loads here.
#[tauri::command]
pub async fn list_speech_models(
    state: State<'_, AppState>,
) -> Result<Vec<SpeechModelInfo>, String> {
    speech_models(&state).await
}

/// The catalog speech models and whether each is downloaded; the files counted are those the model loads here.
///
/// # Errors
/// Why the model store cannot be reached.
pub async fn speech_models(state: &AppState) -> Result<Vec<SpeechModelInfo>, String> {
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    Ok(store
        .speech_models(state.speech_devices.plans().await)
        .await)
}

/// What the Speech section says about the hardware a catalog model runs on: where it is running if the engine
/// has it loaded, else where it will load.
#[tauri::command]
pub async fn speech_device(name: String, state: State<'_, AppState>) -> Result<String, String> {
    speech_device_line(&state, &name).await
}

/// What the Speech section says about the hardware a catalog model runs on.
///
/// # Errors
/// That the model is not in the catalog, or why the model store cannot be reached.
pub async fn speech_device_line(state: &AppState, name: &str) -> Result<String, String> {
    let model = CatalogSpeech::named(name)
        .ok_or_else(|| format!("{name} is not a speech model Thalvi knows."))?;
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    Ok(state.speech_devices.line(&store, model).await)
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

/// Checks one speech server entry where it is added or edited, before it joins the list.
///
/// # Errors
/// The plain reason the entry cannot be used.
#[tauri::command]
pub fn check_speech_entry(entry: Value) -> Result<(), String> {
    entries::check(&entry)
}

/// The live entries whose recognizer can be asked about a word list.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Asked {
    Realtime {
        id: String,
        address: String,
        model: String,
        /// Absent when the saved key is meant.
        #[serde(default)]
        api_key: Option<String>,
    },
    ThalviServer {
        address: String,
    },
}

/// Whether the recognizer behind a live entry boosts a word list, so Settings can offer one.
#[tauri::command]
pub async fn word_list_support(entry: Value, state: State<'_, AppState>) -> Result<bool, String> {
    let asked = serde_json::from_value::<Asked>(entry).map_err(|error| error.to_string())?;
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    match asked {
        Asked::ThalviServer { address } => {
            let server = ServerUrl::parse(&address).map_err(|error| error.to_string())?;
            phrases::server_boosts(&reqwest::Client::new(), &server, &recorder.metrics()).await
        }
        Asked::Realtime {
            id,
            address,
            model,
            api_key,
        } => {
            let key = api_key.unwrap_or_else(|| state.config().stt.entries.key_of(&id).to_owned());
            let realtime = Realtime::new(&address, &model, key)?;
            phrases::realtime_boosts(&realtime, &recorder.metrics()).await
        }
    }
}

/// Saves the stored word list on the active Thalvi server, which gives it to its recognizer; a
/// recognizer reached directly is given the list at each connection instead.
#[tauri::command]
pub async fn share_word_list(state: State<'_, AppState>) -> Result<(), String> {
    share(&state).await
}

/// Gives the active Thalvi server the stored word list; any other recognizer needs none.
///
/// # Errors
/// The plain reason the list could not be saved there.
pub async fn share(state: &AppState) -> Result<(), String> {
    let config = state.config();
    let LiveKind::ThalviServer(server) = &config.stt.entries.active_live().kind else {
        return Ok(());
    };
    let recorder = Recorder::per_user(config.recordings).map_err(|error| error.to_string())?;
    phrases::share(
        &reqwest::Client::new(),
        server,
        &config.stt.phrases,
        &recorder.metrics(),
    )
    .await
}

/// Downloads a catalog speech model, telling the UI how far it is.
#[tauri::command]
pub async fn download_speech_model(
    name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    download_speech(&app, &state, &name).await
}

/// Downloads a catalog speech model, telling the UI how far it is.
///
/// # Errors
/// The plain reason the download failed.
pub async fn download_speech(app: &AppHandle, state: &AppState, name: &str) -> Result<(), String> {
    let model = CatalogSpeech::named(name)
        .ok_or_else(|| format!("{name} is not a speech model Thalvi can download."))?;
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    let assets = model.assets_on(&state.speech_devices.plans().await(model));
    let download = Download {
        name: model.name(),
        assets: &assets,
        store: &store,
        downloads: &state.speech_downloads,
        recorder: &recorder,
    };
    download
        .run(|progress| {
            let _ = app.emit("thalvi://speech-model-progress", progress);
        })
        .await
}

/// One download of a catalog speech model's files, with what reports it.
struct Download<'a> {
    name: &'static str,
    assets: &'a SpeechAssets<'a>,
    store: &'a ModelStore,
    downloads: &'a SpeechDownloads,
    recorder: &'a Recorder,
}

impl Download<'_> {
    /// Fetches the files the store lacks, telling `emit` each step and a failure, and records the outcome.
    async fn run(&self, emit: impl Fn(SpeechModelProgress<'static>)) -> Result<(), String> {
        let started = Instant::now();
        let result = self
            .downloads
            .run(self.name, self.store, self.assets, |progress| {
                emit(SpeechModelProgress::of(self.name, progress));
            })
            .await;
        if matches!(result, Err(ModelError::AlreadyDownloading { .. })) {
            return result.map_err(|error| error.to_string());
        }
        self.recorder.metric(
            Event::SpeechDownload,
            if result.is_ok() { "ok" } else { "error" },
            Some(started.elapsed()),
            json!({"model": self.name}),
        );
        result.map_err(|error| {
            let message = error.to_string();
            emit(SpeechModelProgress::failed(self.name, message.clone()));
            message
        })
    }
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

/// Keeps the edited text. Nothing is recognized, copied or pasted.
#[tauri::command]
pub fn edit_draft(text: String, state: State<'_, AppState>) -> Result<(), String> {
    state.drafts.edit(text).map_err(|error| error.to_string())
}

/// Puts `text` on the clipboard through the desktop.
#[tauri::command]
pub async fn copy_text(text: String, state: State<'_, AppState>) -> Result<(), String> {
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    state
        .delivery
        .copy_text(&recorder, &text)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn discard_draft(state: State<'_, AppState>) {
    state.drafts.discard();
}

/// Why this desktop offers no hold key, or `None` where it does.
#[tauri::command]
pub fn get_hold_key_unavailable(state: State<'_, AppState>) -> Option<&'static str> {
    state.hold_key_unavailable()
}

/// The hold and write keys this desktop starts with, named as in the settings.
#[tauri::command]
pub fn get_hold_key_defaults() -> Value {
    let (hold, write) = Desktop::default_hold_keys();
    json!({"hold_key": hold.to_string(), "write_key": write.to_string()})
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
    use std::cell::RefCell;

    use serde_json::json;
    use thalvi_desktop::metrics::RecordKinds;
    use thalvi_desktop::models::{Asset, ModelKind};

    use super::*;

    #[test]
    fn the_defaults_the_ui_restores_are_this_desktops_keys_as_the_settings_name_them() {
        let (hold, write) = Desktop::default_hold_keys();
        assert_eq!(
            get_hold_key_defaults(),
            json!({"hold_key": hold.to_string(), "write_key": write.to_string()})
        );
    }

    #[tokio::test]
    async fn a_download_that_fails_tells_the_ui_how_far_it_got_and_why_and_records_the_error() {
        let dir = std::env::temp_dir().join(format!("thalvi-download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A port nothing listens on, so the download fails at once without a network.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/m.onnx");
        let file = Asset {
            kind: ModelKind::Speech,
            file_name: "m.onnx",
            url: &url,
            size: 4,
            sha256: "00",
        };
        let assets = SpeechAssets {
            encoder: file,
            encoder_data: None,
            decoder: file,
            joiner: file,
            tokens: file,
        };
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                ..RecordKinds::default()
            },
            dir.join("records"),
        )
        .unwrap();
        let store = ModelStore::in_dir(dir.join("models"));
        let downloads = SpeechDownloads::default();
        let emitted = RefCell::new(Vec::new());

        let result = Download {
            name: "m",
            assets: &assets,
            store: &store,
            downloads: &downloads,
            recorder: &recorder,
        }
        .run(|progress| emitted.borrow_mut().push(progress))
        .await;

        let message = result.unwrap_err();
        assert!(
            message.starts_with("Could not download the speech model m.onnx"),
            "{message}"
        );
        assert_eq!(
            *emitted.borrow(),
            [
                SpeechModelProgress::of("m", SpeechProgress::Verifying),
                SpeechModelProgress::failed("m", message),
            ]
        );
        recorder.finish().await;
        let metrics = std::fs::read_to_string(dir.join("records").join("metrics.jsonl")).unwrap();
        let line: Value = serde_json::from_str(metrics.lines().next().unwrap()).unwrap();
        assert_eq!(
            (&line["event"], &line["outcome"], &line["model"]),
            (
                &json!("desktop_speech_download"),
                &json!("error"),
                &json!("m")
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
}
