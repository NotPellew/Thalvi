//! The Linux desktop host: picks Hyprland or GNOME Wayland at startup and forwards every call to it.

use std::fmt;
use std::path::Path;

use super::gnome::{Clipboard, Gnome};
use super::hyprland::{Hyprland, Window};
use super::{App, Delivered, HoldSource, Host, HostError, StateServer, TypedText, WebUrl, state};
use crate::hold::HoldKey;

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

impl Desktop {
    /// # Errors
    /// [`HostError::Unsupported`] naming this desktop unless it is Hyprland or GNOME Wayland.
    pub fn detect() -> Result<Self, HostError> {
        Ok(match Session::detect()? {
            Session::Hyprland => Self::Hyprland(Hyprland::system()),
            Session::Gnome => Self::Gnome(Gnome::system()),
        })
    }

    /// Answers each query on the user's private runtime folder with `state()` until the guard is
    /// dropped.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when no private folder is available or another app answers.
    pub fn serve_state(
        state: impl Fn() -> String + Send + 'static,
    ) -> Result<StateServer, HostError> {
        state::serve(state)
    }

    /// Reports the hold key as the desktop allows: through binds on Hyprland, not at all on GNOME.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when this desktop cannot detect a key's release, or is none
    /// Thalvi supports.
    pub fn watch_hold_key(
        key: &HoldKey,
        on_change: impl Fn(bool) + Send + 'static,
    ) -> Result<HoldSource, HostError> {
        match Session::detect()? {
            Session::Hyprland => Hyprland::watch_hold_key(key, on_change),
            Session::Gnome => Gnome::watch_hold_key(key, on_change),
        }
    }

    /// The running app's state line; `None` when no app answers.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when the answer could not be asked or read.
    pub fn query_state() -> Result<Option<String>, HostError> {
        state::query()
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
    HostError::NothingTyped("The target belongs to another desktop. Nothing was typed.".to_owned())
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
        text: &TypedText,
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
    fn a_target_names_its_window_or_the_clipboard_and_belongs_to_one_desktop() {
        let desktop = Desktop::Hyprland(Hyprland::system());
        let text = TypedText::new("hi").unwrap();
        assert_eq!(Target::Clipboard(Clipboard).to_string(), "the clipboard");
        assert_eq!(
            desktop.deliver(&Target::Clipboard(Clipboard), &text, false),
            Err(foreign())
        );
        let desktop = Desktop::Gnome(Gnome::system());
        assert_eq!(desktop.capture_target(), Ok(Target::Clipboard(Clipboard)));
    }
}
