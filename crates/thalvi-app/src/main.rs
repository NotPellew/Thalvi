//! Starts the Thalvi desktop app.

// Release builds launched from the Start menu or at login must not open a console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    thalvi_app::run()
}
