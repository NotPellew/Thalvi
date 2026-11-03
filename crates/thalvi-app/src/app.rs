//! Wires the engine to the tray, the overlay, the settings window and the commands.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::host::{Desktop, StateServer};
use thalvi_desktop::models::SpeechDownloads;

use crate::commands;
use crate::control;
use crate::launch::{Command, Launch, relaunch_command};
use crate::menu_entry::{self, MenuEntry};
use crate::overlay::{self, Overlay, Visibility};
use crate::runner::{Control, EngineLink};
use crate::settings::Store;
use crate::startup::{self, APP_NAME, LOGIN_ARG, Registration};
use crate::status::{LastError, Status};
use crate::tray::{self, CopyItem, Tray};

const SETTINGS: &str = "settings";

/// What the commands and the tray menu reach.
pub struct AppState {
    pub link: EngineLink,
    pub store: Store,
    pub overlay: Overlay,
    pub speech_downloads: SpeechDownloads,
    /// The latest failure shown, for "Copy last error" in the tray and `copy-error`.
    last_error: LastError,
    /// Set once the engine's first status is in; the tray menu and the commands update it.
    tray: OnceLock<Tray>,
    host: Arc<Desktop>,
    config: Mutex<DesktopConfig>,
    overlay_shown: AtomicBool,
    /// Why the login entry could not be put back on start, for the switch to show.
    login_problem: Mutex<Option<String>>,
    /// Why the app menu entry could not be rewritten on start, for the switch to show.
    menu_problem: Mutex<Option<String>>,
    /// Answers status bars until the app quits.
    state_server: Mutex<Option<StateServer>>,
}

impl AppState {
    /// Reads the config and starts the engine.
    fn start(config: Option<std::path::PathBuf>) -> Result<Self, String> {
        let store = Store::new(config)?;
        let config = store.load()?;
        let link = EngineLink::start(
            config.clone(),
            Desktop::detect().map_err(|error| error.to_string())?,
        )?;
        let last_error = LastError::default();
        let state_server = match Desktop::serve_state(control::answer(
            link.status_updates(),
            last_error.clone(),
        )) {
            Ok(server) => Some(server),
            Err(error) => {
                // The app works without a status bar; the bar then reports Thalvi as not running.
                println!("{error} The status bar module stays off.");
                None
            }
        };
        Ok(Self {
            link,
            store,
            overlay: Overlay::default(),
            speech_downloads: SpeechDownloads::default(),
            last_error,
            tray: OnceLock::new(),
            host: Arc::new(Desktop::detect().map_err(|error| error.to_string())?),
            overlay_shown: AtomicBool::new(config.overlay.show),
            login_problem: Mutex::new(None),
            menu_problem: Mutex::new(None),
            config: Mutex::new(config),
            state_server: Mutex::new(state_server),
        })
    }

    #[must_use]
    pub fn config(&self) -> DesktopConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        let mut config = self.config();
        change(&mut config);
        self.store.save_switches(&config)?;
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config;
        Ok(())
    }

    /// Takes `config` into effect: the engine applies it between captures.
    pub fn apply(&self, config: &DesktopConfig) {
        self.overlay_shown
            .store(config.overlay.show, Ordering::SeqCst);
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config.clone();
        self.link.send(Control::Apply(Box::new(config.clone())));
    }
}

#[derive(Serialize, Clone)]
struct Level {
    level: f32,
}

pub fn run() -> ExitCode {
    // Recording failures and dropped events are reported only through `tracing`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    match start() {
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

fn start() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, config) = match Launch::parse(&args)? {
        Launch::Status => {
            println!("{}", control::bar_json(Desktop::query_state()));
            return Ok(());
        }
        Launch::Run { command, config } => {
            if command == Command::CopyError {
                control::check_copy_error(Desktop::query_state())?;
            }
            (command, config)
        }
    };
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            act(app, relaunch_command(&args));
        }))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name(APP_NAME)
                .arg(LOGIN_ARG)
                .build(),
        )
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
            commands::get_start_at_login,
            commands::set_start_at_login,
            commands::copy_login_line,
            commands::get_menu_entry,
            commands::set_menu_entry,
            commands::validate_speech_model_folder,
            commands::list_speech_models,
            commands::download_speech_model,
            commands::open_speech_model_source,
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
    // A relaunch has already been handed to the running app by now, so reaching this point means
    // no app runs; a control command must not start the engine, the microphone or any window.
    command.first_start()?;
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
    let state = AppState::start(config)?;
    state.restore_login(app);
    state.restore_menu_entry(&menu_entry::System::from_env());
    app.manage(state);
    create_windows(app).map_err(|error| error.to_string())?;
    follow_engine(app).map_err(|error| error.to_string())
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
        loop {
            let current = status.borrow_and_update().clone();
            show(&handle, &current);
            let changed = tokio::select! {
                changed = status.changed() => changed,
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

fn act(app: &AppHandle, command: Command) {
    // A relaunch can arrive while the first instance is still starting.
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match command {
        Command::Show => show_settings(app),
        Command::Login => {}
        Command::Toggle => state.link.send(Control::ToggleListening),
        Command::Pause => state.link.send(Control::SetListening(false)),
        Command::Resume => state.link.send(Control::SetListening(true)),
        Command::Cancel => state.link.send(Control::Cancel),
        Command::HoldStart => state.link.hold_input().set(true),
        Command::HoldEnd => state.link.hold_input().set(false),
        Command::CopyError => copy_last_error(app),
    }
}

/// Copies the last error's report; a failure shows on the tray item and in the log.
fn copy_last_error(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let Some(failure) = state.last_error.get() else {
            return;
        };
        let item = match commands::copy(&app, &state, &failure.notice, &failure.causes).await {
            Ok(()) => CopyItem::Ready,
            Err(reason) => {
                eprintln!("The error report could not be copied: {reason}");
                CopyItem::Failed(reason)
            }
        };
        if let Some(tray) = state.tray.get() {
            tray.copy_item(&item);
        }
    });
}

/// Handles a tray menu item.
pub fn on_menu(app: &AppHandle, id: &str) {
    match id {
        tray::TOGGLE => act(app, Command::Toggle),
        tray::CANCEL => act(app, Command::Cancel),
        tray::COPY_ERROR => act(app, Command::CopyError),
        tray::SETTINGS => act(app, Command::Show),
        tray::QUIT => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<AppState>();
                // Bars read "not running" while the microphone and the recognizer close.
                drop(
                    state
                        .state_server
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
