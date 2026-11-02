//! Wires the engine to the tray, the overlay, the settings window and the commands.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::host::{Desktop, Host};

use crate::commands;
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
}

impl AppState {
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
    let launch = Launch::parse(&args)?;
    let store = Store::new(launch.config, std::env::var("THALVI_RECORD").ok())?;
    let config = store.load()?;
    let link = EngineLink::start(
        config.clone(),
        Desktop::detect().map_err(|e| e.to_string())?,
    )?;
    let state = AppState {
        link,
        store,
        overlay: Overlay::default(),
        host: Arc::new(Desktop::detect().map_err(|e| e.to_string())?),
        overlay_shown: AtomicBool::new(config.overlay.show),
        config: Mutex::new(config),
    };
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            // A relaunch that cannot be read still opens the settings.
            let command = Launch::parse(args.get(1..).unwrap_or_default())
                .map_or(Command::Show, |launch| launch.command);
            act(app, command);
        }))
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_settings,
            commands::save_settings,
            commands::set_listening,
            commands::cancel_capture,
            commands::list_microphones,
            commands::open_recordings,
        ])
        .setup(|app| {
            create_windows(app.handle())?;
            follow_engine(app.handle())?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|error| format!("Thalvi could not start: {error}"))?;
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
    match state.host.prepare_overlay() {
        Ok(()) => {
            overlay::create(app)?;
        }
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
    let tray = Tray::new(app, &status.borrow())?;
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let current = status.borrow_and_update().clone();
            show(&handle, &tray, &current);
            if status.changed().await.is_err() {
                // The engine thread ended without a quit.
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
    let state = app.state::<AppState>();
    match command {
        Command::Show => show_settings(app),
        Command::Toggle => {
            let on = matches!(state.link.status(), Status::Paused | Status::Failed { .. });
            state.link.send(Control::SetListening(on));
        }
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
                app.state::<AppState>().link.quit();
                app.exit(0);
            });
        }
        _ => {}
    }
}
