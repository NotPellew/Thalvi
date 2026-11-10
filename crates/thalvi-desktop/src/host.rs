//! The desktop host interface: bind the window a capture starts in, paste into it, and run desktop actions.

#[cfg(target_os = "linux")]
mod channel;
#[cfg(target_os = "linux")]
mod command;
#[cfg(target_os = "linux")]
mod gnome;
#[cfg(target_os = "linux")]
mod hyprland;
mod line;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::report::UserError;

pub use line::{MAX_REQUEST, refusal};

/// The title of the status overlay window, by which a desktop recognises it.
pub const OVERLAY_TITLE: &str = "Thalvi overlay";

/// The host for the desktop Thalvi runs on.
#[cfg(target_os = "linux")]
pub use linux::{Desktop, Session};
/// The host for the desktop Thalvi runs on.
#[cfg(target_os = "windows")]
pub type Desktop = windows::Windows;

/// Where `deliver` left the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivered {
    /// Pasted into the target window.
    InWindow,
    /// Put on the clipboard for the user to paste; the desktop lets nothing paste for them.
    OnClipboard,
}

/// What a failed paste says when it is not known whether the text reached the window.
pub(crate) const MAYBE_PASTED: &str =
    "Thalvi may or may not have pasted the text. Check the window before trying again.";

/// How long submit waits between paste and Enter on desktops that need it.
#[cfg_attr(
    not(any(target_os = "windows", test)),
    expect(dead_code, reason = "the Windows host waits through this constant")
)]
pub(crate) const ENTER_SUBMIT_WAIT: Duration = Duration::from_millis(250);

/// The `wtype -s` argument matching [`ENTER_SUBMIT_WAIT`].
#[cfg_attr(
    not(target_os = "linux"),
    expect(dead_code, reason = "Hyprland passes this to wtype")
)]
pub(crate) const ENTER_SUBMIT_WAIT_MS: &str = "250";

/// What submit says when the text was pasted but Enter did not go through.
pub(crate) const ENTER_FAILED: &str = "The text was pasted, but Thalvi may not have pressed Enter. Check the window before trying again.";

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
            Self::NothingTyped(_) => "Nothing was pasted — Settings",
            Self::MaybePartial(_) => "Some text may have been pasted",
        }
    }
}

/// Text to paste: line breaks (as `\n`) and tabs, but no other control character, so pasting it
/// can only insert text and never act in a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasteText(String);

impl PasteText {
    /// Line endings of any style become `\n`.
    ///
    /// # Errors
    /// [`HostError::NothingTyped`] when `text` holds a C0 or C1 control character other than a
    /// line break or tab.
    pub fn new(text: &str) -> Result<Self, HostError> {
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err(HostError::NothingTyped(
                "The text contains control characters that could act in the window. Nothing was pasted."
                    .to_owned(),
            ));
        }
        Ok(Self(text.replace("\r\n", "\n").replace('\r', "\n")))
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

/// Keeps a desktop answering requests from other programs of the same user, such as a status bar or
/// an agent; dropping it stops. A request and its reply are each one line of JSON.
pub struct RequestServer {
    _stop_on_drop: Box<dyn Send + Sync>,
}

impl RequestServer {
    /// Wraps whatever stops the answering when dropped.
    pub fn new(stop_on_drop: impl Send + Sync + 'static) -> Self {
        Self {
            _stop_on_drop: Box::new(stop_on_drop),
        }
    }
}

/// Keeps a desktop reporting the hold key's presses and releases; dropping it stops, and lets the
/// key go if it was held.
pub struct HoldWatch {
    _stop_on_drop: Box<dyn Send>,
}

impl HoldWatch {
    /// Wraps whatever stops the watching when dropped.
    pub fn new(stop_on_drop: impl Send + 'static) -> Self {
        Self {
            _stop_on_drop: Box::new(stop_on_drop),
        }
    }
}

/// How a desktop reports the hold key.
pub enum HoldSource {
    /// The desktop reports the key until this is dropped, to the app's keyboard hook or to binds
    /// it added to the compositor.
    Watching(HoldWatch),
    /// The desktop gives the app no way to see the key, as `reason` says: not a failure, since
    /// the user did nothing wrong, but the key does nothing and Settings says why.
    Unavailable(&'static str),
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
    fn overlay_takes_clicks(&self) -> bool;

    /// Whether this desktop can only put dictation on the clipboard, never paste it, so the
    /// target names no window and the user pastes it themselves.
    fn copies_only(&self) -> bool;

    /// Binds the focused window.
    ///
    /// # Errors
    /// Why no window could be bound.
    fn capture_target(&self) -> Result<Self::Target, HostError>;

    /// Puts `text` on the clipboard and pastes it into `target`, then presses Enter when `enter`,
    /// only if `target` is still the window that will receive the input; a desktop that cannot
    /// paste leaves `text` on the clipboard and presses nothing. The text stays on the clipboard.
    ///
    /// # Errors
    /// [`HostError::NothingTyped`] when the target closed, lost focus or refuses input;
    /// [`HostError::MaybePartial`] when it is not known whether the paste or Enter reached the window.
    fn deliver(
        &self,
        target: &Self::Target,
        text: &PasteText,
        enter: bool,
    ) -> Result<Delivered, HostError>;

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
    /// first appears and before each remap, since compositor config reloads can discard the rules.
    ///
    /// # Errors
    /// Why the desktop could not be told; the overlay must then stay hidden.
    fn prepare_overlay(&self, width: f64, height: f64) -> Result<f64, HostError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_text_keeps_line_breaks_and_tabs_and_refuses_other_control_characters() {
        for (text, pasted) in [
            ("äöüß é 👋 plain words", Some("äöüß é 👋 plain words")),
            ("", Some("")),
            ("line\nbreak\ttab", Some("line\nbreak\ttab")),
            ("dos\r\nold mac\rlone\n", Some("dos\nold mac\nlone\n")),
            ("escape\u{1b}[A", None),
            ("bell\u{7}", None),
            ("delete\u{7f}", None),
            ("next line\u{85}", None),
        ] {
            assert_eq!(
                PasteText::new(text).ok().as_ref().map(PasteText::as_str),
                pasted,
                "{text:?}"
            );
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
