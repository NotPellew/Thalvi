//! The Linux desktop host: picks Hyprland or GNOME Wayland at startup and forwards every call to it.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use super::gnome::{self, Clipboard, Gnome};
use super::hyprland::{Hyprland, Window};
use super::{
    App, Delivered, HoldSource, Host, HostError, PasteText, RequestServer, WebUrl, channel,
};
use crate::hold::{HoldKey, HoldKind};

/// The desktop session Thalvi runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    Hyprland,
    Gnome,
}

impl Session {
    /// # Errors
    /// [`HostError::Unsupported`] naming this desktop unless it is Hyprland or GNOME Wayland.
    pub fn detect() -> Result<Self, HostError> {
        let var = |name| std::env::var(name).ok();
        Self::of(
            var("HYPRLAND_INSTANCE_SIGNATURE").as_deref(),
            var("XDG_CURRENT_DESKTOP").as_deref(),
            var("XDG_SESSION_TYPE").as_deref(),
        )
    }

    fn of(
        signature: Option<&str>,
        desktop: Option<&str>,
        session_type: Option<&str>,
    ) -> Result<Self, HostError> {
        if present(signature).is_some() {
            return Ok(Self::Hyprland);
        }
        let name = present(desktop).unwrap_or("unknown");
        if name.contains("GNOME") {
            match present(session_type) {
                Some("wayland") => return Ok(Self::Gnome),
                // A GNOME session that is not Wayland must not read as supported.
                Some(other) => {
                    return Err(unsupported(&format!("{name} ({other})")));
                }
                None => {}
            }
        }
        Err(unsupported(name))
    }
}

fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn unsupported(desktop: &str) -> HostError {
    HostError::Unsupported(format!(
        "Thalvi supports Hyprland, GNOME and Windows; this desktop is {desktop}."
    ))
}

/// The Linux desktop Thalvi runs on.
pub enum Desktop {
    Hyprland(Hyprland),
    Gnome(Gnome),
}

/// What a capture is bound to: the window on Hyprland, the clipboard on GNOME.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Window(Window),
    Clipboard(Clipboard),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Window(window) => window.fmt(f),
            Self::Clipboard(clipboard) => clipboard.fmt(f),
        }
    }
}

/// Where Thalvi cannot use `XWayland` on GNOME, and so cannot open a window.
fn no_xwayland() -> HostError {
    HostError::Unsupported(
        "Thalvi opens its windows through XWayland on GNOME and could not reach it. Install XWayland (on Ubuntu, the xwayland package) and log in again, then start Thalvi."
            .to_owned(),
    )
}

/// `GDK_BACKEND` overrides the allowed backends, so one that leaves out X11 is named instead.
fn backend_excludes_x11(backend: &str) -> HostError {
    HostError::Unsupported(format!(
        "GDK_BACKEND={backend} keeps Thalvi off XWayland, which it needs on GNOME so its windows never take focus. Unset GDK_BACKEND for Thalvi, then start it."
    ))
}

/// On GNOME, GTK runs on the X11 backend, since Mutter hands a Wayland window focus when it
/// appears and takes no placement from it; `init` then proves `XWayland` answers.
fn prepare_toolkit_for(
    session: Option<Session>,
    backend: Option<&str>,
    allow_backends: impl FnOnce(&str),
    init: impl FnOnce() -> bool,
) -> Result<(), HostError> {
    match session {
        Some(Session::Gnome) => {
            if let Some(backend) = backend.filter(|backend| {
                !backend.is_empty() && !backend.split(',').any(|b| matches!(b.trim(), "x11" | "*"))
            }) {
                return Err(backend_excludes_x11(backend));
            }
            allow_backends("x11");
            init().then_some(()).ok_or_else(no_xwayland)
        }
        Some(Session::Hyprland) | None => Ok(()),
    }
}

impl Desktop {
    /// Chooses GTK's windowing backend for this desktop. Call before anything starts GTK,
    /// including [`Self::show_error`]; a desktop Thalvi does not support is reported by
    /// [`Self::detect`] instead.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when GNOME's `XWayland` cannot be reached.
    pub fn prepare_toolkit() -> Result<(), HostError> {
        prepare_toolkit_for(
            Session::detect().ok(),
            std::env::var("GDK_BACKEND").ok().as_deref(),
            gtk::gdk::set_allowed_backends,
            || gtk::init().is_ok(),
        )
    }

    /// Settles a window's platform hints before it first appears, so it never takes focus.
    pub fn settle_overlay_window(&self, window: &gtk::ApplicationWindow) {
        match self {
            // Hyprland's window rules from `prepare_overlay` do this.
            Self::Hyprland(_) => {}
            Self::Gnome(_) => gnome::keep_unfocused(window),
        }
    }

    /// # Errors
    /// [`HostError::Unsupported`] naming this desktop unless it is Hyprland or GNOME Wayland.
    pub fn detect() -> Result<Self, HostError> {
        Ok(match Session::detect()? {
            Session::Hyprland => Self::Hyprland(Hyprland::system()),
            Session::Gnome => Self::Gnome(Gnome::system()),
        })
    }

    /// Answers each request on a socket in the user's private runtime folder with `handler(line)`
    /// until the guard is dropped. Only the user's own programs are answered.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when no private folder is available or another app answers.
    pub fn serve_requests(
        handler: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<RequestServer, HostError> {
        channel::serve(handler)
    }

    /// [`Self::serve_requests`] on a socket in `dir`, which only the user may open.
    ///
    /// # Errors
    /// As [`Self::serve_requests`], and when `dir` is open to other users.
    #[doc(hidden)]
    pub fn serve_requests_in(
        dir: &Path,
        handler: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<RequestServer, HostError> {
        channel::serve_at(dir, handler)
    }

    /// [`Self::send_request`] to the socket in `dir`.
    ///
    /// # Errors
    /// As [`Self::send_request`].
    #[doc(hidden)]
    pub fn send_request_in(
        dir: &Path,
        request: &str,
        timeout: Duration,
    ) -> Result<Option<String>, HostError> {
        channel::ask_at(dir, request, timeout)
    }

    /// The keys a config without hold keys gets on this desktop: the hold key, then the write key.
    /// GNOME offers no hold key, so these are Hyprland's.
    #[must_use]
    pub fn default_hold_keys() -> (HoldKey, HoldKey) {
        Hyprland::default_hold_keys()
    }

    /// Reports the hold keys as the desktop allows: through binds on Hyprland, not at all on GNOME.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when this desktop cannot detect a key's release, or is none
    /// Thalvi supports.
    pub fn watch_hold_keys(
        keys: &[(HoldKind, HoldKey)],
        on_change: impl Fn(HoldKind, bool) + Send + 'static,
    ) -> Result<HoldSource, HostError> {
        match Session::detect()? {
            Session::Hyprland => Hyprland::watch_hold_keys(keys, on_change),
            Session::Gnome => Gnome::watch_hold_keys(keys, on_change),
        }
    }

    /// The running app's reply to `request`, waiting up to `timeout` for each step; `None` when no
    /// app answers.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when the answer could not be asked or read.
    pub fn send_request(request: &str, timeout: Duration) -> Result<Option<String>, HostError> {
        channel::ask(request, timeout)
    }

    /// Shows `message` in an error box titled `title` and waits until it is closed. Needs no
    /// detected desktop, so a failure to detect one can be shown. Does nothing without a display.
    pub fn show_error(title: &str, message: &str) {
        use gtk::prelude::Cast;

        if gtk::init().is_err() {
            return;
        }
        let dialog = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Error,
            gtk::ButtonsType::Ok,
            message,
        );
        gtk::prelude::GtkWindowExt::set_title(&dialog, title);
        // So the user can copy the message, such as a path, out of the box.
        if let Ok(area) =
            gtk::prelude::MessageDialogExt::message_area(&dialog).downcast::<gtk::Container>()
        {
            for child in gtk::prelude::ContainerExt::children(&area) {
                if let Ok(label) = child.downcast::<gtk::Label>() {
                    gtk::prelude::LabelExt::set_selectable(&label, true);
                }
            }
        }
        gtk::prelude::DialogExt::run(&dialog);
    }
}

/// A target from the other desktop, which the app never hands to this one.
fn foreign() -> HostError {
    HostError::NothingTyped("The target belongs to another desktop. Nothing was pasted.".to_owned())
}

impl Host for Desktop {
    type Target = Target;

    fn overlay_takes_clicks(&self) -> bool {
        match self {
            Self::Hyprland(host) => host.overlay_takes_clicks(),
            Self::Gnome(host) => host.overlay_takes_clicks(),
        }
    }

    fn copies_only(&self) -> bool {
        match self {
            Self::Hyprland(host) => host.copies_only(),
            Self::Gnome(host) => host.copies_only(),
        }
    }

    fn capture_target(&self) -> Result<Target, HostError> {
        match self {
            Self::Hyprland(host) => host.capture_target().map(Target::Window),
            Self::Gnome(host) => host.capture_target().map(Target::Clipboard),
        }
    }

    fn deliver(
        &self,
        target: &Target,
        text: &PasteText,
        enter: bool,
    ) -> Result<Delivered, HostError> {
        match (self, target) {
            (Self::Hyprland(host), Target::Window(window)) => host.deliver(window, text, enter),
            (Self::Gnome(host), Target::Clipboard(clipboard)) => {
                host.deliver(clipboard, text, enter)
            }
            _ => Err(foreign()),
        }
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        match self {
            Self::Hyprland(host) => host.copy(text),
            Self::Gnome(host) => host.copy(text),
        }
    }

    fn focus_app(&self, app: &App) -> Result<Target, HostError> {
        match self {
            Self::Hyprland(host) => host.focus_app(app).map(Target::Window),
            Self::Gnome(host) => host.focus_app(app).map(Target::Clipboard),
        }
    }

    fn open_url(&self, url: &WebUrl) -> Result<(), HostError> {
        match self {
            Self::Hyprland(host) => host.open_url(url),
            Self::Gnome(host) => host.open_url(url),
        }
    }

    fn open_folder(&self, path: &Path) -> Result<(), HostError> {
        match self {
            Self::Hyprland(host) => host.open_folder(path),
            Self::Gnome(host) => host.open_folder(path),
        }
    }

    fn prepare_overlay(&self, width: f64, height: f64) -> Result<f64, HostError> {
        match self {
            Self::Hyprland(host) => host.prepare_overlay(width, height),
            Self::Gnome(host) => host.prepare_overlay(width, height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_is_hyprland_or_gnome_wayland_and_other_desktops_are_named() {
        let says = |desktop: &str| {
            Err(HostError::Unsupported(format!(
                "Thalvi supports Hyprland, GNOME and Windows; this desktop is {desktop}."
            )))
        };
        for (signature, desktop, session_type, expected) in [
            (Some("abc"), None, None, Ok(Session::Hyprland)),
            (
                Some("abc"),
                Some("ubuntu:GNOME"),
                Some("wayland"),
                Ok(Session::Hyprland),
            ),
            (
                None,
                Some("ubuntu:GNOME"),
                Some("wayland"),
                Ok(Session::Gnome),
            ),
            (Some(""), Some("GNOME"), Some("wayland"), Ok(Session::Gnome)),
            (
                None,
                Some("ubuntu:GNOME"),
                Some("x11"),
                says("ubuntu:GNOME (x11)"),
            ),
            (None, Some("ubuntu:GNOME"), None, says("ubuntu:GNOME")),
            (None, Some("KDE"), Some("wayland"), says("KDE")),
            (Some(""), None, None, says("unknown")),
        ] {
            assert_eq!(
                Session::of(signature, desktop, session_type),
                expected,
                "{signature:?} {desktop:?} {session_type:?}"
            );
        }
    }

    #[test]
    fn only_gnome_runs_gtk_on_x11_and_a_missing_xwayland_is_a_plain_error() {
        // (session, GDK_BACKEND, whether XWayland answers, backends allowed, result)
        for (session, backend, xwayland, allowed, result) in [
            (Some(Session::Gnome), None, true, Some("x11"), Ok(())),
            (
                Some(Session::Gnome),
                Some("x11,wayland"),
                true,
                Some("x11"),
                Ok(()),
            ),
            (Some(Session::Gnome), Some(""), true, Some("x11"), Ok(())),
            (
                Some(Session::Gnome),
                None,
                false,
                Some("x11"),
                Err(no_xwayland()),
            ),
            (
                Some(Session::Gnome),
                Some("wayland"),
                true,
                None,
                Err(backend_excludes_x11("wayland")),
            ),
            (
                Some(Session::Hyprland),
                Some("wayland"),
                false,
                None,
                Ok(()),
            ),
            (None, None, false, None, Ok(())),
        ] {
            let mut chosen = None;
            let outcome = prepare_toolkit_for(
                session,
                backend,
                |backends| chosen = Some(backends.to_owned()),
                || xwayland,
            );
            assert_eq!(chosen.as_deref(), allowed, "{session:?} {backend:?}");
            assert_eq!(outcome, result, "{session:?} {backend:?} {xwayland}");
        }
    }

    #[test]
    fn a_target_names_its_window_or_the_clipboard_and_belongs_to_one_desktop() {
        let desktop = Desktop::Hyprland(Hyprland::system());
        let text = PasteText::new("hi").unwrap();
        assert_eq!(Target::Clipboard(Clipboard).to_string(), "the clipboard");
        assert_eq!(
            desktop.deliver(&Target::Clipboard(Clipboard), &text, false),
            Err(foreign())
        );
        let desktop = Desktop::Gnome(Gnome::system());
        assert_eq!(desktop.capture_target(), Ok(Target::Clipboard(Clipboard)));
    }
}
