//! Hyprland host: hyprctl, wtype and wl-copy.

use super::{App, Host, HostError, TypedText, WebUrl};

/// The Hyprland desktop.
pub struct Hyprland;

impl Hyprland {
    /// # Errors
    /// Why Thalvi cannot drive this desktop.
    pub fn detect() -> Result<Self, HostError> {
        Err(unsupported())
    }
}

fn unsupported() -> HostError {
    HostError::Unsupported("Hyprland support is not built yet.".to_owned())
}

impl Host for Hyprland {
    type Target = ();

    fn capture_target(&self) -> Result<(), HostError> {
        Err(unsupported())
    }

    fn deliver(&self, _target: &(), _text: &TypedText, _enter: bool) -> Result<(), HostError> {
        Err(unsupported())
    }

    fn copy(&self, _text: &str) -> Result<(), HostError> {
        Err(unsupported())
    }

    fn focus_app(&self, _app: &App) -> Result<(), HostError> {
        Err(unsupported())
    }

    fn open_url(&self, _url: &WebUrl) -> Result<(), HostError> {
        Err(unsupported())
    }
}
