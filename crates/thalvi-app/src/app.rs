//! Wires the engine to the tray, the overlay, the settings window and the commands.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::delivery::Delivery;
use thalvi_desktop::device::Adapters;
use thalvi_desktop::draft::Drafts;
use thalvi_desktop::history;
use thalvi_desktop::hold::HoldKind;
use thalvi_desktop::host::{Desktop, HoldSource, HoldWatch, HostError, RequestServer, refusal};
use thalvi_desktop::metrics::Recorder;
use thalvi_desktop::models::SpeechDownloads;
use thalvi_desktop::speech_device::SpeechDevices;
use tokio::sync::watch;

use crate::agent::{self, Running, Switch};
use crate::client;
use crate::commands;
use crate::launch::{Launch, relaunch_shows_settings};
use crate::menu_entry::{self, MenuEntry};
use crate::overlay::{self, Overlay, Visibility};
use crate::runner::{AudioStatus, Control, EngineLink};
use serde_json::Value;

use crate::settings::{Store, ensure_microphone_change_allowed};
use crate::startup::{self, APP_NAME, LOGIN_ARG, Registration};
use crate::status::{Failure, FailureKind, LastError, Status};
use crate::tray::{self, CopyItem, Tray};

const SETTINGS: &str = "settings";

/// The settings page that holds the last dictation, and the event that opens a page.
const HISTORY_PAGE: &str = "history";
const OPEN_PAGE_EVENT: &str = "thalvi://open-page";

/// Sent when an agent changes what the open settings window shows; the window's own saves never send it.
const CHANGED_ELSEWHERE_EVENT: &str = "thalvi://settings-elsewhere";

/// The running config and the file it is saved to. One lock orders every save
/// from reading the config through writing the file and applying the result.
pub(crate) struct SyncedConfig {
    pub(crate) store: Store,
    current: Mutex<DesktopConfig>,
    lock: Mutex<()>,
}

impl SyncedConfig {
    pub(crate) fn new(store: Store, config: DesktopConfig) -> Self {
        Self {
            store,
            current: Mutex::new(config),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn config(&self) -> DesktopConfig {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set(&self, config: &DesktopConfig) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();
    }

    /// Builds a settings form from the config read under the save lock, then validates, writes and
    /// applies it, all under that lock. `form` returns the value to hand back and the form to save,
    /// or `None` to save nothing. `take_effect` gets the config in use, the validated one and
    /// `write`, which replaces the file; it writes before it installs the config in the running
    /// app, as `AppState::take_effect` does, and an error from it before writing leaves the file.
    pub(crate) fn save_form<T>(
        &self,
        form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<Value>), String>,
        paused: bool,
        take_effect: impl FnOnce(
            &DesktopConfig,
            &DesktopConfig,
            &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String>,
    ) -> Result<(T, Option<(DesktopConfig, bool)>), String> {
        let _save = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.config();
        let (value, settings) = form(&current)?;
        let Some(settings) = settings else {
            return Ok((value, None));
        };
        ensure_microphone_change_allowed(&settings, current.microphone.as_deref(), paused)?;
        let config = Store::check(settings, &current)?;
        take_effect(&current, &config, &|| self.store.write(&config))?;
        let look_changed = config.look != current.look;
        Ok((value, Some((config, look_changed))))
    }

    fn remember(&self, change: impl FnOnce(&mut DesktopConfig)) -> Result<(), String> {
        let _save = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut config = self.config();
        change(&mut config);
        self.store.save_switches(&config)?;
        self.set(&config);
        Ok(())
    }
}

/// What the commands and the tray menu reach.
pub struct AppState {
    pub link: EngineLink,
    saves: SyncedConfig,
    pub overlay: Overlay,
    pub speech_downloads: SpeechDownloads,
    /// Where the speech model runs, shared with the engine thread that loads it.
    pub speech_devices: Arc<SpeechDevices>,
    /// The dictation history, shared with the engine thread that writes it.
    pub history: Arc<history::Store>,
    /// The last dictation, shared with the engine thread that offers each new one.
    pub drafts: Arc<Drafts>,
    /// Delivers for the engine thread and copies for the commands, so both record the same metrics.
    pub delivery: Arc<Delivery<Desktop>>,
    /// The latest failure shown, for "Copy last error" in the tray and `copy-error`.
    last_error: LastError,
    /// Set once the engine's first status is in; the tray menu and the commands update it.
    tray: OnceLock<Tray>,
    host: Arc<Desktop>,
    overlay_shown: AtomicBool,
    /// Why the login entry could not be put back on start, for the switch to show.
    login_problem: Mutex<Option<String>>,
    /// Why the app menu entry could not be rewritten on start, for the switch to show.
    menu_problem: Mutex<Option<String>>,
    /// Answers requests from outside, such as status bars and agents, until the app quits.
    request_server: Mutex<Option<RequestServer>>,
    /// Reports the hold key where the app sees the keyboard itself; empty elsewhere.
    hold_watch: Mutex<Option<HoldWatch>>,
    hold_unavailable: Option<&'static str>,
}

impl AppState {
    /// Reads the config and starts the engine; also whether this is the first start.
    fn start(config: Option<std::path::PathBuf>, overlay: Overlay) -> Result<(Self, bool), String> {
        let store = Store::new(config)?;
        let (config, first_start) = store.start()?;
        let history = Arc::new(history::Store::per_user());
        let drafts = Arc::new(Drafts::default());
        let delivery = Arc::new(Delivery::new(
            Desktop::detect().map_err(|error| error.to_string())?,
        ));
        let speech_devices = Arc::new(SpeechDevices::new(Adapters::default()));
        let link = EngineLink::start(
            config.clone(),
            Arc::clone(&delivery),
            Arc::clone(&history),
            Arc::clone(&drafts),
            Arc::clone(&speech_devices),
        )?;
        let mut held = watch_hold_keys(&config, &link, false);
        if let Some(notice) = held.refusal.take() {
            link.send(Control::HoldKeyFailed(notice));
        }
        let state = Self {
            link,
            saves: SyncedConfig::new(store, config.clone()),
            overlay,
            speech_downloads: SpeechDownloads::default(),
            speech_devices,
            history,
            drafts,
            delivery,
            last_error: LastError::default(),
            tray: OnceLock::new(),
            host: Arc::new(Desktop::detect().map_err(|error| error.to_string())?),
            overlay_shown: AtomicBool::new(config.overlay.show),
            login_problem: Mutex::new(None),
            menu_problem: Mutex::new(None),
            request_server: Mutex::new(None),
            hold_watch: Mutex::new(held.watch),
            hold_unavailable: held.unavailable,
        };
        Ok((state, first_start))
    }

    #[must_use]
    pub fn config(&self) -> DesktopConfig {
        self.saves.config()
    }

    /// Builds a settings form from the config now in use, saves it and takes it into effect,
    /// ordered against every other save; see `SyncedConfig::save_form`.
    ///
    /// # Errors
    /// The plain reason the settings were refused, in which case nothing changed.
    pub fn save_form<T>(
        &self,
        form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<Value>), String>,
    ) -> Result<(T, Option<(DesktopConfig, bool)>), String> {
        self.saves
            .save_form(form, self.link.audio().paused, |current, config, write| {
                self.take_effect(current, config, write)
            })
    }

    /// Writes `config` and takes it into effect. Changed hold keys are watched before the file is
    /// written, so a desktop that refuses them leaves the old keys in use and the file as it was.
    fn take_effect(
        &self,
        current: &DesktopConfig,
        config: &DesktopConfig,
        write: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        let keys_changed =
            (&current.hold_key, &current.write_key) != (&config.hold_key, &config.write_key);
        let mut watch = self
            .hold_watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        save_keys(
            &mut watch,
            keys_changed,
            |keeping| watch_hold_keys(config, &self.link, keeping),
            write,
        )?;
        drop(watch);
        self.apply(config);
        Ok(())
    }

    /// Why this desktop offers no hold key; `None` where it does.
    #[must_use]
    pub fn hold_key_unavailable(&self) -> Option<&'static str> {
        self.hold_unavailable
    }

    #[must_use]
    pub fn host(&self) -> Arc<Desktop> {
        Arc::clone(&self.host)
    }

    /// Puts the startup entry back if the user wants Thalvi to start at login.
    fn restore_login(&self, registration: &impl Registration) {
        let problem = startup::restore(registration, self.config().start_at_login)
            .err()
            .map(|error| error.to_string());
        *self
            .login_problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = problem;
    }

    /// Why the entry could not be put back on start, if it could not.
    #[must_use]
    pub fn login_problem(&self) -> Option<String> {
        self.login_problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Rewrites the app menu entry if the user wants Thalvi in the menu.
    fn restore_menu_entry(&self, entry: &impl MenuEntry) {
        let problem = menu_entry::restore(entry, self.config().show_in_menu).err();
        *self
            .menu_problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = problem;
    }

    /// Why the entry could not be rewritten on start, if it could not.
    #[must_use]
    pub fn menu_problem(&self) -> Option<String> {
        self.menu_problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Saves the user's choice to start at login. It is written apart from the settings form,
    /// which the switch does not wait for.
    ///
    /// # Errors
    /// Why the config file could not be written.
    pub fn remember_login(&self, on: bool) -> Result<(), String> {
        self.remember(|config| config.start_at_login = on)?;
        *self
            .login_problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    /// Saves the user's choice to have Thalvi in the app menu, apart from the settings form.
    ///
    /// # Errors
    /// Why the config file could not be written.
    pub fn remember_menu_entry(&self, on: bool) -> Result<(), String> {
        self.remember(|config| config.show_in_menu = on)?;
        *self
            .menu_problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    fn remember(&self, change: impl FnOnce(&mut DesktopConfig)) -> Result<(), String> {
        self.saves.remember(change)
    }

    /// Takes `config` into effect: the engine applies it between captures.
    fn apply(&self, config: &DesktopConfig) {
        self.overlay_shown
            .store(config.overlay.show, Ordering::SeqCst);
        self.saves.set(config);
        self.link.send(Control::Apply(Box::new(config.clone())));
    }
}

/// What watching the hold keys came to.
#[derive(Default)]
struct Held {
    watch: Option<HoldWatch>,
    /// Why this desktop offers no hold key, for Settings.
    unavailable: Option<&'static str>,
    /// What the user is told when the desktop refused the keys.
    refusal: Option<String>,
}

/// Starts watching the changed keys before `write` saves them, so that a desktop that refuses
/// them leaves the old keys in use and the file as it was; `start` is told whether there is a
/// watch to keep. Unchanged keys keep their watch.
fn save_keys<T>(
    previous: &mut Option<HoldWatch>,
    keys_changed: bool,
    start: impl FnOnce(bool) -> Held,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let next = if keys_changed {
        let next = start(previous.is_some());
        if let Some(refusal) = next.refusal {
            return Err(refusal);
        }
        Some(next.watch)
    } else {
        None
    };
    let saved = write()?;
    if let Some(watch) = next {
        *previous = watch;
    }
    Ok(saved)
}

/// Starts reporting the hold and write keys to the engine where the desktop lets the app see
/// them, by a hook or binds of its own. Where the desktop offers no hold key, Settings says so and
/// nothing looks like an error; where the desktop refuses, the refusal says so in plain words, and
/// the rest of the app goes on. `keeping` says that the keys in use stay when the desktop refuses.
fn watch_hold_keys(config: &DesktopConfig, link: &EngineLink, keeping: bool) -> Held {
    let input = link.hold_input();
    let keys = [
        (HoldKind::Dictate, config.hold_key.clone()),
        (HoldKind::Write, config.write_key.clone()),
    ];
    held(
        Desktop::watch_hold_keys(
            &keys,
            move |kind, down| match kind {
                HoldKind::Dictate => input.set(down),
                HoldKind::Write => input.set_write(down),
            },
            link.hold_problems(),
        ),
        keeping,
    )
}

fn held(source: Result<HoldSource, HostError>, keeping: bool) -> Held {
    match source {
        Ok(HoldSource::Watching(watch)) => Held {
            watch: Some(watch),
            ..Held::default()
        },
        Ok(HoldSource::Unavailable(reason)) => {
            println!("{reason}");
            Held {
                unavailable: Some(reason),
                ..Held::default()
            }
        }
        Err(error) => Held {
            refusal: Some(if keeping {
                format!("{error} The keys that were set before still work.")
            } else {
                format!(
                    "{error} Dictating by holding a key does not work until Thalvi is restarted or another key is set; the wake word still works."
                )
            }),
            ..Held::default()
        },
    }
}

#[derive(Serialize, Clone)]
struct Level {
    level: f32,
}

pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let launch = Launch::parse(&args, || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map(|_| text)
    });
    let (login, config) = match launch {
        Ok(Launch::Ask(request)) => return client::run(&request),
        Ok(Launch::Open { login, config }) => (login, config),
        Err(message) => {
            println!("{}", refusal(&message));
            return ExitCode::from(2);
        }
    };
    // Recording failures and dropped events are reported only through `tracing`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    match start(login, config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            report_failure(&message);
            ExitCode::FAILURE
        }
    }
}

/// Says why Thalvi could not start: on stderr, and in a message box when no terminal shows it,
/// as for a menu or login launch, or a release Windows build, which has no console.
fn report_failure(message: &str) {
    eprintln!("{message}");
    if !std::io::stderr().is_terminal() {
        Desktop::show_error("Thalvi couldn't start", message);
    }
}

fn start(login: bool, config: Option<std::path::PathBuf>) -> Result<(), String> {
    // An app that already runs opens its settings, or is left as the login entry finds it.
    if if login {
        client::app_runs()
    } else {
        client::opened_running_app()
    } {
        return Ok(());
    }
    let (ui, overlay) = overlay::prepare();
    // Rune's event loop must own the main thread on Linux and Windows; Tauri's runs beside it.
    let tauri = std::thread::Builder::new()
        .name("tauri".to_owned())
        .spawn(move || {
            if let Err(message) = run_tauri(config, overlay) {
                report_failure(&message);
                std::process::exit(1);
            }
        })
        .map_err(|error| error.to_string())?;
    if let Err(error) = ui.try_run() {
        println!("The status overlay could not start: {error}. It stays off.");
    }
    // Tauri ends the process when it quits.
    let _ = tauri.join();
    Ok(())
}

fn run_tauri(config: Option<std::path::PathBuf>, overlay: Overlay) -> Result<(), String> {
    // GTK's backend is fixed when it starts, which the builder below does on this thread.
    #[cfg(target_os = "linux")]
    Desktop::prepare_toolkit().map_err(|error| error.to_string())?;
    let app = tauri::Builder::default()
        .any_thread()
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            if relaunch_shows_settings(&args) {
                show_settings(app);
            }
        }))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name(APP_NAME)
                .arg(LOGIN_ARG)
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_audio,
            commands::overlay_takes_clicks,
            commands::get_settings,
            commands::save_settings,
            commands::set_listening,
            commands::cancel_capture,
            commands::dismiss_error,
            commands::copy_report,
            commands::list_microphones,
            commands::open_recordings,
            commands::get_hold_key_unavailable,
            commands::get_start_at_login,
            commands::set_start_at_login,
            commands::copy_login_line,
            commands::get_menu_entry,
            commands::set_menu_entry,
            commands::validate_speech_model_folder,
            commands::list_wake_models,
            commands::validate_wake_model_choices,
            commands::download_wake_model,
            commands::list_speech_models,
            commands::speech_device,
            commands::download_speech_model,
            commands::open_speech_model_source,
            commands::check_speech_entry,
            commands::word_list_support,
            commands::share_word_list,
            commands::get_history,
            commands::delete_history_entry,
            commands::clear_history,
            commands::get_draft,
            commands::edit_draft,
            commands::copy_text,
            commands::discard_draft,
        ])
        .setup(move |app| {
            // Tauri turns an error from this hook into a panic, so it is reported here instead.
            if let Err(message) = setup(app.handle(), config, overlay) {
                report_failure(&message);
                std::process::exit(1);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|error| error.to_string())?;
    app.run(|_, event| {
        // The tray keeps the app alive when no window is open.
        if let RunEvent::ExitRequested {
            api, code: None, ..
        } = event
        {
            api.prevent_exit();
        }
    });
    Ok(())
}

/// Reads the config and starts the engine, the tray and the windows.
fn setup(
    app: &AppHandle,
    config: Option<std::path::PathBuf>,
    overlay: Overlay,
) -> Result<(), String> {
    // Only the first instance gets here, so a relaunch never reads the config.
    let (state, first_start) = AppState::start(config, overlay)?;
    state.restore_login(app);
    state.restore_menu_entry(&menu_entry::System::from_env());
    app.manage(state);
    serve_requests(app);
    create_windows(app).map_err(|error| error.to_string())?;
    if first_start {
        show_settings(app);
    }
    follow_engine(app).map_err(|error| error.to_string())
}

/// Starts answering requests from outside. The app works without; status bars then report Thalvi
/// as not running, and the commands say so.
fn serve_requests(app: &AppHandle) {
    let handle = app.clone();
    let listening = Desktop::serve_requests(move |line| {
        tauri::async_runtime::block_on(agent::answer(
            &Live(handle.clone()),
            agent::EFFECT_WAIT,
            line,
        ))
    });
    match listening {
        Ok(requests) => {
            *app.state::<AppState>()
                .request_server
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(requests);
        }
        Err(error) => println!("{error} The status bar module and the agent commands stay off."),
    }
}

fn create_windows(app: &AppHandle) -> tauri::Result<()> {
    let settings =
        WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
            .title("Thalvi settings")
            .inner_size(780.0, 680.0)
            .visible(false)
            // Built at start, when a login start may hold the foreground; `show_settings` focuses it.
            .focused(false)
            .build()?;
    let window = settings.clone();
    settings.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = window.hide();
        }
    });
    Ok(())
}

/// Carries the engine's status and level to the tray, the overlay and the UI.
fn follow_engine(app: &AppHandle) -> tauri::Result<()> {
    let state = app.state::<AppState>();
    let mut status = state.link.status_updates();
    let mut level = state.link.level_updates();
    let mut audio = state.link.audio_updates();
    let tray = Tray::new(app, &status.borrow(), state.link.audio().paused)?;
    let _ = state.tray.set(tray);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let current = status.borrow_and_update().clone();
        show(&handle, &current);
        loop {
            let changed = tokio::select! {
                changed = status.changed() => {
                    if changed.is_ok() {
                        let current = status.borrow_and_update().clone();
                        show(&handle, &current);
                    }
                    changed
                }
                changed = audio.changed() => {
                    if changed.is_ok() {
                        let current = audio.borrow_and_update().clone();
                        if let Some(tray) = handle.state::<AppState>().tray.get() {
                            tray.listening(current.paused);
                        }
                        let _ = handle.emit("thalvi://audio", current);
                    }
                    changed
                }
            };
            if changed.is_err() {
                let failed = Status::Failed {
                    kind: FailureKind::Other,
                    notice: "Thalvi stopped unexpectedly. Restart it.".to_owned(),
                    headline: "Thalvi stopped — restart it",
                    causes: Vec::new(),
                };
                show(&handle, &failed);
                break;
            }
        }
    });
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while level.changed().await.is_ok() {
            let level = *level.borrow_and_update();
            let _ = handle.emit("thalvi://level", Level { level });
        }
    });
    Ok(())
}

fn show(app: &AppHandle, status: &Status) {
    let state = app.state::<AppState>();
    state.last_error.record(status);
    if let Some(tray) = state.tray.get() {
        tray.show(status);
    }
    let _ = app.emit("thalvi://status", status);
    let enabled = state.overlay_shown.load(Ordering::SeqCst);
    state.overlay.show(status, Visibility::of(status, enabled));
}

/// Opens the settings window; only an explicit user action calls this.
fn show_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Copies the last error's report; a failure shows on the tray item and in the log.
///
/// # Errors
/// Why there is nothing to copy, or why the clipboard refused it.
async fn copy_last_error(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let failure = state
        .last_error
        .get()
        .ok_or("Thalvi has shown no error to copy.")?;
    let copied = commands::copy(app, &state, &failure.notice, &failure.causes).await;
    let item = match &copied {
        Ok(()) => CopyItem::Ready,
        Err(reason) => {
            eprintln!("The error report could not be copied: {reason}");
            CopyItem::Failed(reason.clone())
        }
    };
    if let Some(tray) = state.tray.get() {
        tray.copy_item(&item);
    }
    copied
}

/// The running app, as the commands from outside see it.
struct Live(AppHandle);

impl Running for Live {
    fn status(&self) -> watch::Receiver<Status> {
        self.0.state::<AppState>().link.status_updates()
    }

    fn audio(&self) -> watch::Receiver<AudioStatus> {
        self.0.state::<AppState>().link.audio_updates()
    }

    fn last_error(&self) -> Option<Failure> {
        self.0.state::<AppState>().last_error.get()
    }

    fn control(&self, control: Control) {
        self.0.state::<AppState>().link.send(control);
    }

    fn hold(&self, kind: HoldKind, down: bool) {
        let input = self.0.state::<AppState>().link.hold_input();
        match kind {
            HoldKind::Dictate => input.set(down),
            HoldKind::Write => input.set_write(down),
        }
    }

    fn show_settings(&self) {
        show_settings(&self.0);
    }

    async fn copy_error(&self) -> Result<(), String> {
        copy_last_error(&self.0).await
    }

    fn config(&self) -> DesktopConfig {
        self.0.state::<AppState>().config()
    }

    fn save<T>(
        &self,
        form: impl FnOnce(&DesktopConfig) -> Result<(T, Option<serde_json::Value>), String>,
    ) -> Result<T, String> {
        let (value, saved) = commands::save(&self.0, &self.0.state::<AppState>(), form)?;
        if saved {
            let _ = self.0.emit(CHANGED_ELSEWHERE_EVENT, ());
        }
        Ok(value)
    }

    async fn share_word_list(&self) -> Result<(), String> {
        commands::share(&self.0.state::<AppState>()).await
    }

    fn set_switch(&self, switch: Switch, on: bool) -> Result<(), String> {
        let state = self.0.state::<AppState>();
        match switch {
            Switch::StartAtLogin => startup::set(&self.0, on, || state.remember_login(on))
                .map_err(|error| error.to_string()),
            Switch::ShowInMenu => menu_entry::set(&menu_entry::System::from_env(), on, || {
                state.remember_menu_entry(on)
            }),
        }?;
        let _ = self.0.emit(CHANGED_ELSEWHERE_EVENT, ());
        Ok(())
    }

    fn recordings(&self) -> Result<std::path::PathBuf, String> {
        Recorder::dir().map_err(|error| error.to_string())
    }
}

/// Handles a tray menu item.
pub fn on_menu(app: &AppHandle, id: &str) {
    match id {
        tray::TOGGLE => app.state::<AppState>().link.send(Control::ToggleListening),
        tray::CANCEL => app.state::<AppState>().link.send(Control::Cancel),
        tray::COPY_ERROR => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let _ = copy_last_error(&app).await;
            });
        }
        tray::SETTINGS => show_settings(app),
        tray::LAST_DICTATION => {
            show_settings(app);
            let _ = app.emit(OPEN_PAGE_EVENT, HISTORY_PAGE);
        }
        tray::QUIT => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<AppState>();
                // Bars read "not running" while the microphone and the recognizer close.
                drop(
                    state
                        .request_server
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take(),
                );
                // Exiting never drops managed state, and Hyprland keeps binds the app added.
                drop(
                    state
                        .hold_watch
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take(),
                );
                state.link.quit();
                app.exit(0);
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    fn writer(
        saves: &Arc<SyncedConfig>,
        applied: &Arc<Mutex<Vec<DesktopConfig>>>,
        line: &Arc<Barrier>,
        body: impl FnOnce(&Arc<SyncedConfig>, &Arc<Mutex<Vec<DesktopConfig>>>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let saves = Arc::clone(saves);
        let applied = Arc::clone(applied);
        let line = Arc::clone(line);
        std::thread::spawn(move || {
            line.wait();
            body(&saves, &applied);
        })
    }

    #[test]
    fn overlapping_saves_leave_the_file_and_the_running_config_agreeing() {
        let dir = std::env::temp_dir().join(format!("thalvi-app-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(Some(dir.join("desktop.json"))).unwrap();
        let start = DesktopConfig::from_json_str("{}").unwrap();
        store.save_switches(&start).unwrap();
        let saves = Arc::new(SyncedConfig::new(store, start));
        let applied: Arc<Mutex<Vec<DesktopConfig>>> = Arc::new(Mutex::new(Vec::new()));
        let line = Arc::new(Barrier::new(5));
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let saves = Arc::clone(&saves);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    saves.store.load().expect("the file is always whole");
                    std::thread::yield_now();
                }
            })
        };
        let form = |look: &'static str| {
            writer(&saves, &applied, &line, move |saves, applied| {
                for _ in 0..50 {
                    let mut settings = crate::settings::settings_json(&saves.config()).unwrap();
                    settings["look"] = serde_json::json!(look);
                    let synced = Arc::clone(saves);
                    saves
                        .save_form(
                            |_| Ok(((), Some(settings))),
                            true,
                            move |_, config, write| {
                                write()?;
                                synced.set(config);
                                applied
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .push(config.clone());
                                Ok(())
                            },
                        )
                        .unwrap();
                }
            })
        };
        let handles = [
            form("holo"),
            form("neon"),
            writer(&saves, &applied, &line, |saves, _| {
                for round in 0..50 {
                    saves
                        .remember(|c| c.start_at_login = round % 2 == 0)
                        .unwrap();
                }
                saves.remember(|c| c.start_at_login = true).unwrap();
            }),
            writer(&saves, &applied, &line, |saves, _| {
                for _ in 0..50 {
                    let mut settings = crate::settings::settings_json(&saves.config()).unwrap();
                    settings["ending_pause"] = serde_json::json!({"paste": 9});
                    let error = saves
                        .save_form(|_| Ok(((), Some(settings))), true, |_, _, write| write())
                        .unwrap_err();
                    assert_eq!(
                        error,
                        "The paste pause must be a number of seconds from 0 to 5."
                    );
                }
            }),
        ];
        line.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        let (running, file) = (saves.config(), saves.store.load().unwrap());
        let applied = applied.lock().unwrap_or_else(PoisonError::into_inner);
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(file, running, "the running config matches the file");
        assert_eq!(applied.last(), Some(&running), "the engine got it last");
        assert!(file.start_at_login, "the last switch save is kept");
        assert!(
            matches!(
                file.look,
                thalvi_desktop::config::Look::Holo | thalvi_desktop::config::Look::Neon
            ),
            "the file holds a reported form: {:?}",
            file.look
        );
        assert_eq!(leftovers, ["desktop.json"], "no temporary file is left");
    }

    #[test]
    fn a_save_the_file_system_refuses_is_reported_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("thalvi-app-refused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("block"), b"not a directory").unwrap();
        let store = Store::new(Some(dir.join("block").join("desktop.json"))).unwrap();
        let start = DesktopConfig::from_json_str("{}").unwrap();
        let saves = SyncedConfig::new(store, start.clone());
        let mut settings = crate::settings::settings_json(&start).unwrap();
        settings["look"] = serde_json::json!("holo");
        let mut applied = false;

        let form = saves
            .save_form(
                |_| Ok(((), Some(settings))),
                true,
                |_, _, write| {
                    write()?;
                    applied = true;
                    Ok(())
                },
            )
            .unwrap_err();
        let switch = saves
            .remember(|c| c.start_at_login = !start.start_at_login)
            .unwrap_err();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);

        for error in [form, switch] {
            assert!(
                error.starts_with("The settings could not be saved to "),
                "{error}"
            );
        }
        assert!(!applied, "a refused form is not applied");
        assert_eq!(saves.config(), start, "the running config is unchanged");
        assert_eq!(leftovers, ["block"], "nothing is written");
    }

    #[test]
    fn a_change_refused_before_writing_leaves_the_file_and_the_running_config() {
        let dir = std::env::temp_dir().join(format!("thalvi-app-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desktop.json");
        let start = DesktopConfig::from_json_str("{}").unwrap();
        let saves = SyncedConfig::new(Store::new(Some(path.clone())).unwrap(), start.clone());
        let mut settings = crate::settings::settings_json(&start).unwrap();
        settings["hold_key"] = serde_json::json!("F7");

        let error = saves
            .save_form(
                |_| Ok(((), Some(settings))),
                true,
                |_, _, _| Err("F7 is already bound in Hyprland.".to_owned()),
            )
            .unwrap_err();
        let written = path.exists();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(error, "F7 is already bound in Hyprland.");
        assert!(!written, "nothing is written");
        assert_eq!(saves.config(), start, "the running config is unchanged");
    }

    #[test]
    fn a_save_says_whether_the_look_changed() {
        let dir = std::env::temp_dir().join(format!("thalvi-app-look-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let start = DesktopConfig::from_json_str("{}").unwrap();
        let saves = SyncedConfig::new(
            Store::new(Some(dir.join("desktop.json"))).unwrap(),
            start.clone(),
        );
        let changed = |look: &str| {
            let mut settings = crate::settings::settings_json(&saves.config()).unwrap();
            settings["look"] = serde_json::json!(look);
            let ((), result) = saves
                .save_form(
                    |_| Ok(((), Some(settings))),
                    true,
                    |_, config, write| {
                        write()?;
                        saves.set(config);
                        Ok(())
                    },
                )
                .unwrap();
            result.map(|(_, look_changed)| look_changed)
        };

        let results = [changed("holo"), changed("holo")];
        let _ = std::fs::remove_dir_all(&dir);

        assert_ne!(start.look, thalvi_desktop::config::Look::Holo);
        assert_eq!(results, [Some(true), Some(false)]);
    }

    #[test]
    fn only_a_refused_hold_key_is_told_as_a_refusal() {
        const RESTART: &str = "Hook refused. Dictating by holding a key does not work until Thalvi is restarted or another key is set; the wake word still works.";
        // (what the desktop said, whether keys are in use, refusal told, watching, why unavailable)
        type Case = (
            Result<HoldSource, HostError>,
            bool,
            Option<&'static str>,
            bool,
            Option<&'static str>,
        );
        let refused = || Err(HostError::Unsupported("Hook refused.".to_owned()));
        let cases: Vec<Case> = vec![
            (
                Ok(HoldSource::Watching(HoldWatch::new(()))),
                false,
                None,
                true,
                None,
            ),
            (
                Ok(HoldSource::Unavailable("No release events.")),
                false,
                None,
                false,
                Some("No release events."),
            ),
            (refused(), false, Some(RESTART), false, None),
            (
                refused(),
                true,
                Some("Hook refused. The keys that were set before still work."),
                false,
                None,
            ),
        ];
        for (source, keeping, told, watching, unavailable) in cases {
            let held = held(source, keeping);
            assert_eq!(held.refusal.as_deref(), told);
            assert_eq!(held.watch.is_some(), watching);
            assert_eq!(held.unavailable, unavailable);
        }
    }

    /// A watch that logs its name when it is dropped.
    struct Named(Arc<Mutex<Vec<&'static str>>>, &'static str);

    impl Drop for Named {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(self.1);
        }
    }

    #[test]
    fn a_refused_key_change_is_not_saved_and_an_accepted_one_is_saved_with_its_watch() {
        // (keys changed, refusal, write fails, result, writes, watches started, dropped in the end)
        type Case = (
            bool,
            Option<&'static str>,
            bool,
            Result<(), &'static str>,
            usize,
            usize,
            &'static [&'static str],
        );
        let cases: [Case; 4] = [
            (
                true,
                Some("Hyprland refused F7."),
                false,
                Err("Hyprland refused F7."),
                0,
                1,
                &["old"],
            ),
            (true, None, false, Ok(()), 1, 1, &["old", "new"]),
            (false, None, false, Ok(()), 1, 0, &["old"]),
            (
                true,
                None,
                true,
                Err("The settings could not be saved."),
                1,
                1,
                &["new", "old"],
            ),
        ];
        for (changed, refusal, write_fails, expected, writes, starts, dropped) in cases {
            let log = Arc::new(Mutex::new(Vec::new()));
            let mut watch = Some(HoldWatch::new(Named(Arc::clone(&log), "old")));
            let (mut started, mut written) = (0, 0);

            let result = save_keys(
                &mut watch,
                changed,
                |keeping| {
                    assert!(keeping, "the old watch is in place when the new starts");
                    assert!(log.lock().unwrap().is_empty(), "the old go only after");
                    started += 1;
                    Held {
                        watch: refusal
                            .is_none()
                            .then(|| HoldWatch::new(Named(Arc::clone(&log), "new"))),
                        refusal: refusal.map(str::to_owned),
                        ..Held::default()
                    }
                },
                || {
                    written += 1;
                    if write_fails {
                        Err("The settings could not be saved.".to_owned())
                    } else {
                        Ok(())
                    }
                },
            );

            let case = format!("{changed} {refusal:?} {write_fails}");
            assert_eq!(result, expected.map_err(str::to_owned), "{case}");
            assert_eq!((written, started), (writes, starts), "{case}");
            assert!(watch.is_some(), "{case}");
            drop(watch);
            assert_eq!(*log.lock().unwrap(), dropped, "{case}");
        }
    }
}
