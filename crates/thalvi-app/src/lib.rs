//! Thalvi desktop app: the Tauri tray, overlay and settings window around the capture engine.

mod app;
mod commands;
mod launch;
mod overlay;
mod runner;
mod settings;
mod status;
mod tray;

pub use app::run;
