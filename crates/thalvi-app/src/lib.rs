//! Thalvi desktop app: the Tauri tray, overlay and settings window around the capture engine.

mod agent;
mod app;
mod client;
mod commands;
mod control;
mod launch;
mod menu_entry;
mod overlay;
mod runner;
mod settings;
mod startup;
mod status;
mod switch;
mod tray;
mod update;
mod updater;

pub use app::run;
