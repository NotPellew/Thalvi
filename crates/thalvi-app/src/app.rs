//! Wires the engine to the tray, the overlay, the settings window and the commands.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::host::{Desktop, StateServer};

use crate::commands;
use crate::control;
use crate::launch::{Command, Launch};
use crate::overlay::{self, Overlay, Visibility};
use crate::runner::{Control, EngineLink};
use crate::settings::Store;
use crate::status::Status;
use crate::tray::{self, Tray};

const SETTINGS: &str = "settings";

/// What the commands and the tray menu reach.
pub struct AppState {
    pub link: EngineLink,
    pub store: Store,
    pub overlay: Overlay,
    host: Arc<Desktop>,
    config: Mutex<DesktopConfig>,
    overlay_shown: AtomicBool,
    /// Answers status bars until the app quits.
    state_server: Mutex<Option<StateServer>>,
}

impl AppState {
    /// Reads the config and starts the engine.
    fn start(config: Option<std::path::PathBuf>) -> Result<Self, String> {
        let store = Store::new(config, std::env::var("THALVI_RECORD").ok())?;
        let config = store.load()?;
        let link = EngineLink::start(
            config.clone(),
            Desktop::detect().map_err(|error| error.to_string())?,
        )?;
        let state_server = match Desktop::serve_state(control::answer(link.status_updates())) {
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
            host: Arc::new(Desktop::detect().map_err(|error| error.to_string())?),
            overlay_shown: AtomicBool::new(config.overlay.show),
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
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, config) = match Launch::parse(&args)? {
        Launch::Status => {
            println!("{}", control::bar_json(Desktop::query_state()));
            return Ok(());
        }
        Launch::Run { command, config } => (command, config),
    };
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            // A relaunch that cannot be read still opens the settings.
            let command = match Launch::parse(args.get(1..).unwrap_or_default()) {
                Ok(Launch::Run { command, .. }) => command,
                _ => Command::Show,
            };
            act(app, command);
        }))
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_audio,
            commands::get_settings,
            commands::save_settings,
            commands::set_listening,
            commands::cancel_capture,
            commands::list_microphones,
            commands::open_recordings,
            commands::validate_speech_model_folder,
        ])
        .setup(move |app| {
            // Only the first instance gets here, so a relaunch never reads the config.
            app.manage(AppState::start(config)?);
            create_windows(app.handle())?;
            follow_engine(app.handle())?;
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
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let current = status.borrow_and_update().clone();
            show(&handle, &tray, &current);
            let changed = tokio::select! {
                changed = status.changed() => changed,
                changed = audio.changed() => {
                    if changed.is_ok() {
                        let current = audio.borrow_and_update().clone();
                        tray.listening(current.paused);
                        let _ = handle.emit("thalvi://audio", current);
                    }
                    changed
                }
            };
            if changed.is_err() {
                let failed = Status::Failed {
                    notice: "Thalvi stopped unexpectedly. Restart it.".to_owned(),
                };
                show(&handle, &tray, &failed);
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

fn show(app: &AppHandle, tray: &Tray, status: &Status) {
    tray.show(status);
    let _ = app.emit("thalvi://status", status);
    let state = app.state::<AppState>();
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
        Command::Toggle => state.link.send(Control::ToggleListening),
        Command::Pause => state.link.send(Control::SetListening(false)),
        Command::Resume => state.link.send(Control::SetListening(true)),
        Command::Cancel => state.link.send(Control::Cancel),
    }
}

/// Handles a tray menu item.
pub fn on_menu(app: &AppHandle, id: &str) {
    match id {
        tray::TOGGLE => act(app, Command::Toggle),
        tray::CANCEL => act(app, Command::Cancel),
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
