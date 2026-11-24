//! The commands the UI calls; errors reach it as plain strings.

#![expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands a command its state by value"
)]

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, State};
use thalvi_desktop::audio;
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::draft::DraftView;
use thalvi_desktop::entries::{self, Realtime};
use thalvi_desktop::history::{Entry, HistoryError, Store as History};
use thalvi_desktop::host::{AppMatchLimit, Desktop, Host, WebUrl, WindowApp};
use thalvi_desktop::metrics::{Event, Recorder};
use thalvi_desktop::models::{
    CatalogHead, ListedSpeech, ModelError, ModelFiles, ModelStore, SpeechDownloads,
    SpeechModelInfo, SpeechProgress, WHISPER_TURBO, WakeModelInfo,
};
use thalvi_desktop::report::{Report, System, causes};
use thalvi_desktop::theme::Style;
use thalvi_desktop::transducer::check_folder;
use thalvi_desktop::wake::validate_wake_model;

use crate::app::AppState;
use crate::menu_entry;
use crate::overlay::{self, Sample, View};
use crate::runner::{AudioStatus, Control};
use crate::settings::{merged, settings_json};
use crate::startup;
use crate::status::Status;
use crate::switch::{SwitchState, Unavailable};
use crate::update::Snapshot;

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> Status {
    state.link.status()
}

/// Whether the next dictation waits for a rebuilt engine, as `thalvi://rebuilding` says when it changes.
#[tauri::command]
pub fn get_rebuilding(state: State<'_, AppState>) -> bool {
    *state.link.rebuilding_updates().borrow()
}

#[tauri::command]
pub fn get_audio(state: State<'_, AppState>) -> AudioStatus {
    state.link.audio()
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<Value, String> {
    settings_json(&state.config())
}

/// The look model resolved for Settings, so the window can wear the look before it is shown.
#[tauri::command]
pub fn get_look_styles(state: State<'_, AppState>) -> Value {
    crate::look_styles::response(&state.config())
}

/// Draws the real overlay for a synthetic state, as framed raw RGBA bytes for the preview canvas.
#[tauri::command]
pub async fn preview_overlay(
    state: String,
    scale: f32,
    app_state: State<'_, AppState>,
) -> Result<tauri::ipc::Response, String> {
    let sample: Sample = state.parse()?;
    let scale = scale.clamp(1.0, 3.0);
    let config = app_state.config();
    let look = config.look;
    let style = Arc::new(Style::resolve(look, config.themes.of(look)));
    let dark = app_state.is_dark();
    let bytes = crate::preview::draw(View::sample(sample), style, dark, scale).await?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Says the settings window has its styles; a show asked for meanwhile now happens.
#[tauri::command]
pub fn settings_ready(app: AppHandle, state: State<'_, AppState>) {
    state.settings_ready.store(true, Ordering::SeqCst);
    if state.settings_wanted.swap(false, Ordering::SeqCst) {
        crate::app::show_settings(&app);
    }
}

/// Saves what Settings changed: `patch` holds only the changed settings, as a JSON merge patch.
#[tauri::command]
pub fn save_settings(patch: Value, state: State<'_, AppState>) -> Result<(), String> {
    save(&state, settings_patch(&patch)).map(drop)
}

/// The form of a Settings save: the patch merged over the config read under the save lock, as an
/// agent's `settings set` is, so it cannot undo a change saved since the window last looked.
pub(crate) fn settings_patch(
    patch: &Value,
) -> impl FnOnce(&DesktopConfig) -> Result<((), Option<Value>), String> + '_ {
    move |current| Ok(((), Some(merged(current, patch)?)))
}

/// Saves and applies what Settings sends, and what an outside program sends in its place.
/// `form` builds the settings form from the config read under the save lock and returns a value
/// for the caller with the form, or `None` to save nothing; the flag tells whether one was saved.
///
/// # Errors
/// The plain reason the settings were refused, in which case nothing changed.
pub fn save<T>(
    state: &AppState,
    form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<Value>), String>,
) -> Result<(T, bool), String> {
    state.save_form(form)
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
    match result {
        Ok(_) => {
            state.wake_head_downloaded(head);
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The local chat download and the runtime selected on this computer.
#[tauri::command]
pub async fn get_chat_model(
    build: Option<thalvi_desktop::chat::Build>,
    state: State<'_, AppState>,
) -> Result<thalvi_desktop::chat::Info, String> {
    chat_model(&state, build).await
}

/// The local chat model's Settings view.
///
/// # Errors
/// Why the model store or hardware check could not be reached.
pub async fn chat_model(
    state: &AppState,
    build: Option<thalvi_desktop::chat::Build>,
) -> Result<thalvi_desktop::chat::Info, String> {
    use thalvi_desktop::chat::{Build, Info};
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    let requested = build.or_else(|| {
        state
            .config()
            .writing
            .as_ref()
            .and_then(thalvi_desktop::writing::Writing::on_computer)
            .flatten()
    });
    tauri::async_runtime::spawn_blocking(move || {
        let preferred = requested.unwrap_or(Build::Vulkan);
        let chosen = if preferred.files().server_path(&store).is_some() {
            Build::installed(&store, requested)
        } else {
            preferred
        };
        Info::of(&store, chosen)
    })
    .await
    .map_err(|error| error.to_string())
}

/// Installs the pinned chat model and prebuilt runtime, with CPU fallback when Vulkan has no GPU.
#[tauri::command]
pub async fn download_chat_model(
    build: thalvi_desktop::chat::Build,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), DownloadFailed> {
    download_chat(&app, &state, build).await
}

/// Installs only in this user's model store and reports the checksum-verified download.
///
/// # Errors
/// A concurrent installation or the first download/unpack failure.
pub async fn download_chat(
    app: &AppHandle,
    state: &AppState,
    build: thalvi_desktop::chat::Build,
) -> Result<(), DownloadFailed> {
    use thalvi_desktop::chat::{self, Build};
    let _install = state.chat_install.try_lock().map_err(|_| {
        DownloadFailed::from("The chat model installation is already changing.".to_owned())
    })?;
    let store = ModelStore::per_user().map_err(|error| DownloadFailed::from(&error))?;
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    let started = Instant::now();
    let emit = |progress| {
        let _ = app.emit(
            "thalvi://chat-model-progress",
            SpeechModelProgress::of("chat", progress),
        );
    };
    let result = async {
        chat::install(&store, &build.files(), |progress| {
            if progress != SpeechProgress::Done {
                emit(progress);
            }
        })
        .await?;
        let checked_store = store.clone();
        let chosen = tauri::async_runtime::spawn_blocking(move || {
            Build::installed(&checked_store, Some(build))
        })
        .await
        .unwrap_or(Build::Cpu);
        if chosen != build {
            chat::install(&store, &chosen.files(), |progress| {
                if progress != SpeechProgress::Done {
                    emit(progress);
                }
            })
            .await?;
        }
        Ok::<(), ModelError>(())
    }
    .await;
    recorder.metric(
        Event::ChatDownload,
        &result
            .as_ref()
            .map_or_else(ModelError::metric_kind, |()| "ok".to_owned()),
        Some(started.elapsed()),
        json!({"model": chat::MODEL.asset.file_name, "build": build.name()}),
    );
    match result {
        Ok(()) => {
            emit(SpeechProgress::Done);
            Ok(())
        }
        Err(error) => {
            let failed = DownloadFailed::from(&error);
            let _ = app.emit(
                "thalvi://chat-model-progress",
                SpeechModelProgress::failed("chat", failed.clone()),
            );
            Err(failed)
        }
    }
}

/// Removes the local model after the Settings window has asked for confirmation.
#[tauri::command]
pub async fn remove_chat_model(state: State<'_, AppState>) -> Result<(), String> {
    let _install = state
        .chat_install
        .try_lock()
        .map_err(|_| "Wait for the chat model installation to finish.".to_owned())?;
    let (reply, removed) = tokio::sync::oneshot::channel();
    state.link.send(Control::RemoveChat(reply));
    tokio::time::timeout(std::time::Duration::from_secs(5), removed)
        .await
        .map_err(|_| "The chat model could not be stopped in time.".to_owned())?
        .map_err(|_| "The engine is no longer running.".to_owned())??;
    Ok(())
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
    let mut models = store
        .speech_models(state.speech_devices.plans().await)
        .await;
    models.extend(store.whisper_models().await);
    Ok(models)
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
    let model = ListedSpeech::named(name)
        .ok_or_else(|| format!("{name} is not a speech model Thalvi knows."))?;
    match model {
        ListedSpeech::Catalog(model) => {
            let store = ModelStore::per_user().map_err(|error| error.to_string())?;
            Ok(state.speech_devices.line(&store, model).await)
        }
        ListedSpeech::Whisper(backend) => Ok(state.speech_devices.whisper_line(backend)),
    }
}

/// What `thalvi://speech-model-progress` carries.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct SpeechModelProgress<'a> {
    name: &'a str,
    percent: u8,
    status: &'static str,
    /// Why it failed, in plain words; only with `failed`.
    message: Option<String>,
    /// What lay behind the failure, outermost first; only with `failed`.
    causes: Vec<String>,
}

/// Why a speech model download failed: what the UI shows and what the report copies.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DownloadFailed {
    pub message: String,
    pub causes: Vec<String>,
}

impl From<String> for DownloadFailed {
    fn from(message: String) -> Self {
        Self {
            message,
            causes: Vec::new(),
        }
    }
}

impl From<&ModelError> for DownloadFailed {
    fn from(error: &ModelError) -> Self {
        Self {
            message: error.to_string(),
            causes: causes(error),
        }
    }
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
            causes: Vec::new(),
        }
    }

    fn failed(name: &'a str, failed: DownloadFailed) -> Self {
        Self {
            name,
            percent: 0,
            status: "failed",
            message: Some(failed.message),
            causes: failed.causes,
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

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Asked {
    Realtime {
        id: String,
        address: String,
        model: String,
        #[serde(default)]
        api_key: Option<String>,
    },
}

/// Whether a directly reached recognizer boosts the word list.
#[tauri::command]
pub async fn word_list_support(entry: Value, state: State<'_, AppState>) -> Result<bool, String> {
    let Asked::Realtime {
        id,
        address,
        model,
        api_key,
    } = serde_json::from_value::<Asked>(entry).map_err(|error| error.to_string())?;
    let config = state.config();
    let key = api_key.unwrap_or_else(|| config.stt.entries.key_of(&id).to_owned());
    let realtime = Realtime::new(&address, &model, key)?;
    let recorder = Recorder::per_user(config.recordings).map_err(|error| error.to_string())?;
    thalvi_desktop::phrases::realtime_boosts(&realtime, &recorder.metrics()).await
}

/// Downloads a catalog speech model, telling the UI how far it is.
#[tauri::command]
pub async fn download_speech_model(
    name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), DownloadFailed> {
    download_speech(&app, &state, &name).await
}

/// Downloads a catalog speech model, telling the UI how far it is.
///
/// # Errors
/// The plain reason the download failed, with what lay behind it.
pub async fn download_speech(
    app: &AppHandle,
    state: &AppState,
    name: &str,
) -> Result<(), DownloadFailed> {
    let model = ListedSpeech::named(name)
        .ok_or_else(|| format!("{name} is not a speech model Thalvi can download."))?;
    let store = ModelStore::per_user().map_err(|error| DownloadFailed::from(&error))?;
    let recorder =
        Recorder::per_user(state.config().recordings).map_err(|error| error.to_string())?;
    let emit = |progress| {
        let _ = app.emit("thalvi://speech-model-progress", progress);
    };
    match model {
        ListedSpeech::Catalog(catalog) => {
            let assets = catalog.assets_on(&state.speech_devices.plans().await(catalog));
            Download {
                name: catalog.name(),
                assets: &assets,
                store: &store,
                downloads: &state.speech_downloads,
                recorder: &recorder,
            }
            .run(emit)
            .await?;
        }
        ListedSpeech::Whisper(backend) => {
            Download {
                name: backend.name(),
                assets: &WHISPER_TURBO,
                store: &store,
                downloads: &state.speech_downloads,
                recorder: &recorder,
            }
            .run(emit)
            .await?;
        }
    }
    state.link.send(Control::ReloadSpeechModel(model));
    Ok(())
}

/// One download of a speech model's files, with what reports it.
struct Download<'a, F: ModelFiles> {
    name: &'static str,
    assets: &'a F,
    store: &'a ModelStore,
    downloads: &'a SpeechDownloads,
    recorder: &'a Recorder,
}

impl<F: ModelFiles> Download<'_, F> {
    /// Fetches the files the store lacks, telling `emit` each step and a failure, and records the outcome.
    async fn run(&self, emit: impl Fn(SpeechModelProgress<'static>)) -> Result<(), DownloadFailed> {
        let started = Instant::now();
        let result = self
            .downloads
            .run(self.name, self.store, self.assets, |progress| {
                emit(SpeechModelProgress::of(self.name, progress));
            })
            .await;
        if let Err(error @ ModelError::AlreadyDownloading { .. }) = &result {
            return Err(error.into());
        }
        self.recorder.metric(
            Event::SpeechDownload,
            &result
                .as_ref()
                .map_or_else(ModelError::metric_kind, |()| "ok".to_owned()),
            Some(started.elapsed()),
            json!({"model": self.name}),
        );
        result.map_err(|error| {
            let failed = DownloadFailed::from(&error);
            emit(SpeechModelProgress::failed(self.name, failed.clone()));
            failed
        })
    }
}

/// Opens a catalog speech model's page in the browser.
#[tauri::command]
pub async fn open_speech_model_source(
    name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let model = ListedSpeech::named(&name)
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

/// Why this desktop cannot match app styles, or `None` where it can.
#[tauri::command(async)]
pub fn get_app_styles_unavailable(state: State<'_, AppState>) -> Option<&'static str> {
    state.host().recent_app().err().map(AppMatchLimit::reason)
}

/// An app offered only to the local Settings window.
#[derive(Serialize)]
pub struct CurrentApp {
    app: String,
    title: String,
}

impl CurrentApp {
    fn of(
        recent: Result<Option<WindowApp>, AppMatchLimit>,
        last: &Mutex<Option<WindowApp>>,
    ) -> Option<Self> {
        let window = recent
            .ok()?
            .or_else(|| last.lock().unwrap_or_else(PoisonError::into_inner).clone())?;
        Some(Self {
            app: window.app().to_owned(),
            title: window.title().to_owned(),
        })
    }
}

/// The most recent app outside Thalvi, falling back to the last bound window.
#[tauri::command(async)]
pub fn get_current_app(state: State<'_, AppState>) -> Option<CurrentApp> {
    CurrentApp::of(state.host().recent_app(), &state.link.last_writing_window)
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
    use thalvi_desktop::models::{Asset, ModelKind, SpeechAssets};

    use super::*;

    #[test]
    fn the_current_app_prefers_recent_focus_falls_back_and_is_absent_on_gnome() {
        let last = WindowApp::new("kitty", "secret-title");
        for (recent, bound, expected) in [
            (
                Ok(Some(WindowApp::new("firefox", "Inbox"))),
                Some(last.clone()),
                json!({"app": "firefox", "title": "Inbox"}),
            ),
            (
                Ok(None),
                Some(last.clone()),
                json!({"app": "kitty", "title": "secret-title"}),
            ),
            (
                Ok(Some(WindowApp::new("editor", ""))),
                None,
                json!({"app": "editor", "title": ""}),
            ),
            (Ok(None), None, Value::Null),
            (Err(AppMatchLimit::Gnome), Some(last), Value::Null),
        ] {
            assert_eq!(
                serde_json::to_value(CurrentApp::of(recent, &Mutex::new(bound))).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn the_defaults_the_ui_restores_are_this_desktops_keys_as_the_settings_name_them() {
        let (hold, write) = Desktop::default_hold_keys();
        assert_eq!(
            get_hold_key_defaults(),
            json!({"hold_key": hold.to_string(), "write_key": write.to_string()})
        );
    }

    #[tokio::test]
    async fn a_download_that_fails_tells_the_ui_how_far_it_got_and_why_and_records_the_kind_of_failure()
     {
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

        let failed = result.unwrap_err();
        assert_eq!(
            failed.message,
            "Could not download the speech model m.onnx: could not connect to 127.0.0.1. Check the internet connection and try again."
        );
        assert!(!failed.causes.is_empty(), "{failed:?}");
        assert!(
            failed.causes.iter().all(|cause| !cause.contains("/m.onnx")),
            "{:?}",
            failed.causes
        );
        assert_eq!(
            *emitted.borrow(),
            [
                SpeechModelProgress::of("m", SpeechProgress::Verifying),
                SpeechModelProgress::failed("m", failed),
            ]
        );
        recorder.finish().await;
        let metrics = std::fs::read_to_string(dir.join("records").join("metrics.jsonl")).unwrap();
        let line: Value = serde_json::from_str(metrics.lines().next().unwrap()).unwrap();
        assert_eq!(
            (&line["event"], &line["outcome"], &line["model"]),
            (
                &json!("desktop_speech_download"),
                &json!("connect"),
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
                json!({"name": "m", "percent": 0, "status": "verifying", "message": null, "causes": []}),
            ),
            (
                SpeechProgress::Downloading { percent: 42 },
                json!({"name": "m", "percent": 42, "status": "downloading", "message": null, "causes": []}),
            ),
            (
                SpeechProgress::Done,
                json!({"name": "m", "percent": 100, "status": "done", "message": null, "causes": []}),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(SpeechModelProgress::of("m", progress)).ok(),
                Some(expected)
            );
        }
        assert_eq!(
            serde_json::to_value(SpeechModelProgress::failed(
                "m",
                DownloadFailed {
                    message: "No network.".to_owned(),
                    causes: vec!["dns error".to_owned()],
                }
            ))
            .ok(),
            Some(
                json!({"name": "m", "percent": 0, "status": "failed", "message": "No network.", "causes": ["dns error"]})
            )
        );
    }
}
