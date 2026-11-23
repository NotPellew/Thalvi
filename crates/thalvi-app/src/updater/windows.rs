//! Whether a Windows install can replace itself: the per-user installer's folder always can.

use tauri::AppHandle;

/// The per-user installer's folder is its user's to write, so the app always updates itself.
pub fn unsupported(_: &AppHandle) -> Option<String> {
    None
}
