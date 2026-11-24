//! Thalvi desktop app: the Tauri tray, overlay and settings window around the capture engine.

mod agent;
mod app;
mod client;
mod commands;
mod control;
mod launch;
mod look_styles;
mod menu_entry;
mod overlay;
mod preview;
mod runner;
mod settings;
mod startup;
mod status;
mod switch;
mod tray;
mod update;
mod updater;
mod watch;

pub use app::run;
pub use overlay::{View, render_rgba};
pub use status::{FailureKind, Status};
