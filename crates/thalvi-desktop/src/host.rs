//! The desktop host interface: bind the window a capture starts in, type into it, and run desktop actions.

#[cfg(target_os = "linux")]
mod hyprland;
#[cfg(target_os = "windows")]
mod windows;

use std::fmt;
use std::path::Path;

use crate::report::UserError;

/// The title of the status overlay window, by which a desktop recognises it.
pub const OVERLAY_TITLE: &str = "Thalvi overlay";

/// The host for the desktop Thalvi runs on.
#[cfg(target_os = "linux")]
pub type Desktop = hyprland::Hyprland;
/// The host for the desktop Thalvi runs on.
#[cfg(target_os = "windows")]
pub type Desktop = windows::Windows;

/// Why a host action did not complete. The text is the plain notice shown to the user.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    /// This desktop cannot do it.
    #[error("{0}")]
    Unsupported(String),
    /// It failed before any input reached a window.
    #[error("{0}")]
    NothingTyped(String),
    /// Some input may have reached a window.
    #[error("{0}")]
    MaybePartial(String),
}

impl UserError for HostError {
    fn headline(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "Desktop can't do that — Settings",
            Self::NothingTyped(_) => "Nothing was typed — Settings",
            Self::MaybePartial(_) => "Some input may have been typed",
        }
    }
}

/// Text that holds no control characters, so typing it can only insert characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedText(String);

impl TypedText {
    /// # Errors
    /// [`HostError::NothingTyped`] when `text` holds a C0 or C1 control character.
    pub fn new(text: &str) -> Result<Self, HostError> {
        if text.chars().any(char::is_control) {
            return Err(HostError::NothingTyped(
                "The text contains control characters that could press keys. Nothing was typed."
                    .to_owned(),
            ));
        }
        Ok(Self(text.to_owned()))
    }

    /// The checked text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An `http` or `https` address, checked before it reaches the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebUrl(reqwest::Url);

impl WebUrl {
    /// # Errors
    /// [`HostError::NothingTyped`] unless `url` parses as `http` or `https` with a host.
    pub fn parse(url: &str) -> Result<Self, HostError> {
        reqwest::Url::parse(url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
            .map(Self)
            .ok_or_else(|| HostError::NothingTyped("That is not a web address.".to_owned()))
    }

    /// The address as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Keeps a desktop answering state queries from other programs, such as a status bar; dropping it
/// stops. An answer is one short line the app chooses, never anything the user said.
pub struct StateServer {
    _stop_on_drop: Box<dyn Send + Sync>,
}

impl StateServer {
    /// Wraps whatever stops the answering when dropped.
    pub fn new(stop_on_drop: impl Send + Sync + 'static) -> Self {
        Self {
            _stop_on_drop: Box::new(stop_on_drop),
        }
    }
}

/// A key held while the last key of a [`Shortcut`] is pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Logo,
}

/// A key chord such as `ctrl+k`, pressed after an app is focused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub modifiers: Vec<Modifier>,
    /// One key name made of ASCII letters, digits and `_`, such as `k` or `Return`.
    pub key: String,
}

/// A configured app and how each desktop recognises its windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    /// Window classes on Linux desktops.
    pub classes: Vec<String>,
    /// Executable names on Windows, such as `T3 Code.exe`.
    pub executables: Vec<String>,
    /// Pressed once the app is focused.
    pub shortcut: Option<Shortcut>,
}

/// What a desktop can do for Thalvi. Calls block; run them off the capture loop.
///
/// Every method returns a typed reason when this desktop cannot do it, never a silent no-op.
pub trait Host: Send + Sync {
    /// A window as bound at wake. Equal targets are the same live window, never a reused handle;
    /// `Display` names it in notices, without its title.
    type Target: Clone + PartialEq + fmt::Debug + fmt::Display + Send + 'static;

    /// Whether a click on the overlay reaches it; where it does not, the overlay shows no buttons.
    const OVERLAY_TAKES_CLICKS: bool;

    /// Binds the focused window.
    ///
    /// # Errors
    /// Why no window could be bound.
    fn capture_target(&self) -> Result<Self::Target, HostError>;

    /// Types `text` into `target`, then Enter when `enter`, only if `target` is still the window
    /// that will receive the input.
    ///
    /// # Errors
    /// [`HostError::NothingTyped`] when the target closed, lost focus or refuses input;
    /// [`HostError::MaybePartial`] when typing stopped partway.
    fn deliver(
        &self,
        target: &Self::Target,
        text: &TypedText,
        enter: bool,
    ) -> Result<(), HostError>;

    /// Puts `text` on the clipboard.
    ///
    /// # Errors
    /// Why the clipboard could not be set.
    fn copy(&self, text: &str) -> Result<(), HostError>;

    /// Focuses the most recently used window of `app`, then presses its shortcut.
    ///
    /// # Errors
    /// Why no window of `app` could be focused.
    fn focus_app(&self, app: &App) -> Result<Self::Target, HostError>;

    /// Opens `url` in the default browser.
    ///
    /// # Errors
    /// Why the browser could not be asked.
    fn open_url(&self, url: &WebUrl) -> Result<(), HostError>;

    /// Opens the folder `path` in the file manager.
    ///
    /// # Errors
    /// Why the file manager could not be asked.
    fn open_folder(&self, path: &Path) -> Result<(), HostError>;

    /// Makes the window titled [`OVERLAY_TITLE`] float, `width` by `height` logical pixels, at the
    /// top centre of the screen area windows may use, above other windows, without ever taking
    /// focus; returns that area's top in logical pixels, below any panel. Call before the window
    /// first appears and again to re-place it after its size changed.
    ///
    /// # Errors
    /// Why the desktop could not be told; the overlay must then stay hidden.
    fn prepare_overlay(&self, width: f64, height: f64) -> Result<f64, HostError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_refuses_anything_that_could_press_a_key() {
        for (text, typable) in [
            ("äöüß é 👋 plain words", true),
            ("", true),
            ("line\nbreak", false),
            ("tab\there", false),
            ("escape\u{1b}[A", false),
            ("delete\u{7f}", false),
            ("next line\u{85}", false),
        ] {
            assert_eq!(TypedText::new(text).is_ok(), typable, "{text:?}");
        }
    }

    #[test]
    fn web_urls_must_be_http_or_https_with_a_host() {
        for (url, valid) in [
            ("https://github.com/NotPellew/Thalvi", true),
            ("http://127.0.0.1:8080/", true),
            ("file:///etc/passwd", false),
            ("javascript:alert(1)", false),
            ("github.com", false),
        ] {
            assert_eq!(WebUrl::parse(url).is_ok(), valid, "{url}");
        }
    }
}
