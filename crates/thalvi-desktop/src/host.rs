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

use crate::learn::{FieldLimit, FieldProbe};
use crate::report::UserError;

pub use line::{MAX_REQUEST, refusal};

/// The title of the status overlay window, by which a desktop recognises it.
pub const OVERLAY_TITLE: &str = "Thalvi overlay";

/// The host for the desktop Thalvi runs on, and its status overlay's own surface and waker.
#[cfg(target_os = "linux")]
pub use linux::{Desktop, OverlayPlan, OverlaySurface, OverlayWaker, Session, status_announcer};
/// The host for the desktop Thalvi runs on, and its status overlay's own surface and waker.
#[cfg(target_os = "windows")]
pub use windows::{Desktop, OverlayPlan, OverlaySurface, OverlayWaker, status_announcer};

/// One overlay frame: `width` × `height` pixels of premultiplied BGRA, row by row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// What ended a wait on the overlay surface.
#[derive(Clone, Debug, PartialEq)]
pub enum OverlayEvent {
    /// Its [`OverlayWaker`] was called.
    Woken,
    /// The pointer pressed the overlay here, in points from its top-left corner.
    Press { x: f32, y: f32 },
    /// The wait reached its deadline.
    Due,
    /// The surface's connection or protocol failed; the overlay is no longer on screen.
    Lost(String),
    /// A show is still waiting for the compositor to place the surface.
    Pending,
}

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
    fn headline(&self) -> String {
        match self {
            Self::Unsupported(_) => "Desktop can't do that — Settings",
            Self::NothingTyped(_) => "Nothing was pasted — Settings",
            Self::MaybePartial(_) => "Some text may have been pasted",
        }
        .to_owned()
    }
}

/// The notice and headline an overlay failure shows in the tray and Settings.
#[must_use]
pub fn overlay_failure(reason: &str) -> (&'static str, String) {
    (
        "Status overlay unavailable",
        format!("{reason} The status overlay stays off until it recovers."),
    )
}

/// Text to paste: line breaks (as `\n`) and tabs, but no other control character, so pasting a
/// single line can only insert text and never act in a terminal.
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

/// Where Thalvi's status goes for the desktop's screen reader.
pub trait StatusAnnouncer: Send + Sync {
    /// Says `announcement` once, without taking focus from the window the user is in.
    fn announce(&self, announcement: &str);
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
    /// The desktop refused the keys for now; the watch binds them when the desktop reloads its config.
    Refused { reason: HostError, watch: HoldWatch },
}

/// Whether the desktop can currently report the hold keys, after their initial setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldReport {
    /// The keys cannot be used; the notice says why.
    Problem(String),
    /// The keys work again after the desktop reloaded its config.
    Working,
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

/// The program a window belongs to, and its title as read once when the window was bound.
/// `Display` is the app name only: the title stays out of notices, history, metrics and recordings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowApp {
    app: String,
    title: String,
}

impl WindowApp {
    /// `app` is the Hyprland class or the Windows executable name.
    #[must_use]
    pub fn new(app: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            title: title.into(),
        }
    }

    /// The app name a writing style matches.
    #[must_use]
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The window title a writing style may require.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
}

impl fmt::Display for WindowApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.app)
    }
}

/// Why a desktop cannot match a writing style to the focused app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMatchLimit {
    /// Mutter tells no app which window is focused, so an app style has nothing to match.
    Gnome,
}

impl AppMatchLimit {
    /// The sentence Settings shows on the App styles section.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Gnome => "GNOME doesn't say which app you're in, so app styles don't apply here.",
        }
    }
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

    /// The app and title of a bound window.
    ///
    /// # Errors
    /// Why this desktop cannot name the focused app.
    fn writing_target(&self, target: &Self::Target) -> Result<WindowApp, AppMatchLimit>;

    /// The most recently focused window outside this process; `None` when none can be read.
    ///
    /// # Errors
    /// Why this desktop cannot name an app.
    fn recent_app(&self) -> Result<Option<WindowApp>, AppMatchLimit>;

    /// Whether a click on the overlay reaches it; where it does not, the overlay shows no buttons.
    fn overlay_takes_clicks(&self) -> bool;

    /// Whether this desktop can only put dictation on the clipboard, never paste it, so the
    /// target names no window and the user pastes it themselves.
    fn copies_only(&self) -> bool;

    /// Whether `target` is a terminal that the paste shortcut pastes into, where a pasted line
    /// break can run a command.
    fn is_terminal(&self, target: &Self::Target) -> bool;

    /// Arms a watch for the field the next paste will reach. Call just before `deliver`; return
    /// at once and do no I/O here, so the paste never waits for a field.
    ///
    /// # Errors
    /// Why this desktop cannot read that app's field.
    fn watch_field(&self, target: &Self::Target) -> Result<Box<dyn FieldProbe>, FieldLimit>;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_app_displays_only_the_app_name() {
        let window = WindowApp::new("firefox", "Private window title");
        assert_eq!(window.app(), "firefox");
        assert_eq!(window.title(), "Private window title");
        assert_eq!(window.to_string(), "firefox");
    }

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
