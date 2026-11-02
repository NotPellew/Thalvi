//! Windows host: Win32 window identity, `SendInput` typing and the clipboard.

use super::{App, Host, HostError, TypedText, WebUrl};

/// The Windows desktop.
pub struct Windows;

impl Windows {
    /// # Errors
    /// Why Thalvi cannot drive this desktop.
    pub fn detect() -> Result<Self, HostError> {
        Err(unsupported())
    }
}

fn unsupported() -> HostError {
    HostError::Unsupported("Typing into Windows apps is not built yet.".to_owned())
}

impl Host for Windows {
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
