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
struct SyncedConfig {
    store: Store,
    current: Mutex<DesktopConfig>,
    lock: Mutex<()>,
}

impl SyncedConfig {
    fn new(store: Store, config: DesktopConfig) -> Self {
        Self {
            store,
            current: Mutex::new(config),
            lock: Mutex::new(()),
        }
    }

    fn config(&self) -> DesktopConfig {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, config: &DesktopConfig) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();
    }

    /// Validates and writes a settings form, then applies it, all under the save lock.
    /// `apply` installs the config in the running app, as `AppState::apply` does.
    fn save_form(
        &self,
        settings: Value,
        paused: bool,
        apply: impl FnOnce(&DesktopConfig),
    ) -> Result<(DesktopConfig, bool), String> {
        let _save = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        ensure_microphone_change_allowed(&settings, self.config().microphone.as_deref(), paused)?;
        let current = self.config();
        let config = self.store.save(settings, &current)?;
        let look_changed = config.look != current.look;
        apply(&config);
        Ok((config, look_changed))
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
    fn start(config: Option<std::path::PathBuf>) -> Result<(Self, bool), String> {
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
        let held = watch_hold_keys(&config, &link);
        let state = Self {
            link,
            saves: SyncedConfig::new(store, config.clone()),
            overlay: Overlay::default(),
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

    /// Saves a settings form and takes it into effect, ordered against every other save.
    ///
    /// # Errors
    /// The plain reason the settings were refused, in which case nothing changed.
    pub fn save_form(&self, settings: Value) -> Result<(DesktopConfig, bool), String> {
        self.saves
            .save_form(settings, self.link.audio().paused, |config| {
                self.apply(config);
            })
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
    pub fn apply(&self, config: &DesktopConfig) {
        self.overlay_shown
            .store(config.overlay.show, Ordering::SeqCst);
        let key_changed = {
            let current = self.saves.config();
            let changed =
                (&current.hold_key, &current.write_key) != (&config.hold_key, &config.write_key);
            self.saves.set(config);
            changed
        };
        if key_changed {
            let mut watch = self
                .hold_watch
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // The old hook goes before the new one comes.
            *watch = None;
            *watch = watch_hold_keys(config, &self.link).watch;
        }
        self.link.send(Control::Apply(Box::new(config.clone())));
    }
}

/// What watching the hold keys came to.
#[derive(Default)]
struct Held {
    watch: Option<HoldWatch>,
    /// Why this desktop offers no hold key, for Settings.
    unavailable: Option<&'static str>,
}

/// Starts reporting the hold and write keys to the engine where the desktop lets the app see
/// them. Where binds report them instead, that is only logged; where the desktop offers no hold
/// key, Settings says so and nothing looks like an error; where the desktop refuses, the user is
/// told through the status, and the rest of the app goes on.
fn watch_hold_keys(config: &DesktopConfig, link: &EngineLink) -> Held {
    let input = link.hold_input();
    let keys = [
        (HoldKind::Dictate, config.hold_key.clone()),
        (HoldKind::Write, config.write_key.clone()),
    ];
    held(
        Desktop::watch_hold_keys(&keys, move |kind, down| match kind {
            HoldKind::Dictate => input.set(down),
            HoldKind::Write => input.set_write(down),
        }),
        |notice| link.send(Control::HoldKeyFailed(notice)),
    )
}

fn held(source: Result<HoldSource, HostError>, tell_failure: impl FnOnce(String)) -> Held {
    match source {
        Ok(HoldSource::Hook(watch)) => Held {
            watch: Some(watch),
            unavailable: None,
        },
        Ok(HoldSource::Binds(reason)) => {
            println!("{reason}");
            Held::default()
        }
        Ok(HoldSource::Unavailable(reason)) => {
            println!("{reason}");
            Held {
                watch: None,
                unavailable: Some(reason),
            }
        }
        Err(error) => {
            tell_failure(format!(
                "{error} Dictating by holding a key does not work until Thalvi is restarted or another key is set; the wake word still works."
            ));
            Held::default()
        }
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
    // GTK's backend is fixed when it starts, which the builder below does.
    #[cfg(target_os = "linux")]
    Desktop::prepare_toolkit().map_err(|error| error.to_string())?;
    let app = tauri::Builder::default()
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
            if let Err(message) = setup(app.handle(), config) {
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
fn setup(app: &AppHandle, config: Option<std::path::PathBuf>) -> Result<(), String> {
    // Only the first instance gets here, so a relaunch never reads the config.
    let (state, first_start) = AppState::start(config)?;
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
    let state = app.state::<AppState>();
    match overlay::create(app, &state.host) {
        Ok(_) => {}
        Err(error) => {
            // An overlay the desktop cannot keep unfocused would take focus from the user's app.
            println!("{error} The status overlay stays off.");
            state.overlay_shown.store(false, Ordering::SeqCst);
        }
    }
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
    state.overlay.apply(app, Visibility::of(status, enabled));
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

    fn save(&self, settings: serde_json::Value) -> Result<DesktopConfig, String> {
        let config = commands::save(&self.0, &self.0.state::<AppState>(), settings)?;
        let _ = self.0.emit(CHANGED_ELSEWHERE_EVENT, ());
        Ok(config)
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
                        .save_form(settings, true, move |config| {
                            synced.set(config);
                            applied
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(config.clone());
                        })
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
                    let error = saves.save_form(settings, true, |_| {}).unwrap_err();
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
    fn only_a_refused_hold_key_is_told_as_a_failure() {
        // (what the desktop said, failure told, watching, why unavailable)
        type Case = (
            Result<HoldSource, HostError>,
            Option<&'static str>,
            bool,
            Option<&'static str>,
        );
        let cases: Vec<Case> = vec![
            (Ok(HoldSource::Hook(HoldWatch::new(()))), None, true, None),
            (Ok(HoldSource::Binds("In the config.")), None, false, None),
            (
                Ok(HoldSource::Unavailable("No release events.")),
                None,
                false,
                Some("No release events."),
            ),
            (
                Err(HostError::Unsupported("Hook refused.".to_owned())),
                Some(
                    "Hook refused. Dictating by holding a key does not work until Thalvi is restarted or another key is set; the wake word still works.",
                ),
                false,
                None,
            ),
        ];
        for (source, told, watching, unavailable) in cases {
            let mut failure = None;
            let held = held(source, |notice| failure = Some(notice));
            assert_eq!(failure.as_deref(), told);
            assert_eq!(held.watch.is_some(), watching);
            assert_eq!(held.unavailable, unavailable);
        }
    }
}
