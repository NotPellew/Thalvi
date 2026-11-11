//! The Linux desktop host: picks Hyprland or GNOME Wayland at startup and forwards every call to it.

mod layer;
mod xwayland;

use std::fmt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::gnome::{self, Clipboard, Gnome};
use super::hyprland::{Hyprland, Window};
use super::{
    App, Delivered, HoldSource, Host, HostError, OverlayEvent, OverlayFrame, PasteText,
    RequestServer, WebUrl, channel,
};
use crate::hold::{HoldChange, HoldKey, HoldKind};

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

    /// Where the status overlay's surface goes. Call on the toolkit's thread.
    ///
    /// # Errors
    /// Why GNOME did not say where its top panel ends.
    pub fn overlay_plan(&self) -> Result<OverlayPlan, HostError> {
        match self {
            // The compositor keeps a layer surface clear of the bars.
            Self::Hyprland(_) => Ok(OverlayPlan::Layer),
            Self::Gnome(host) => Ok(OverlayPlan::Xwayland {
                top: host.prepare_overlay(0.0, 0.0)?,
            }),
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

    /// Refuses the keys this desktop can never bind, without binding anything; a key that is
    /// merely taken is found only when the keys are watched.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] saying why a key cannot be used.
    pub fn check_hold_keys(keys: &[(HoldKind, HoldKey)]) -> Result<(), HostError> {
        match Session::detect() {
            Ok(Session::Hyprland) => Hyprland::check_hold_keys(keys),
            Ok(Session::Gnome) | Err(_) => Ok(()),
        }
    }

    /// Reports the hold keys as the desktop allows: through binds Thalvi adds to Hyprland, not at
    /// all on GNOME. Problems found after this returns go to `on_problem`.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when this desktop cannot detect a key's release, or is none
    /// Thalvi supports.
    pub fn watch_hold_keys(
        keys: &[(HoldKind, HoldKey)],
        on_change: impl Fn(HoldKind, HoldChange) + Send + 'static,
        on_problem: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<HoldSource, HostError> {
        match Session::detect()? {
            Session::Hyprland => Hyprland::watch_hold_keys(keys, on_change, on_problem),
            Session::Gnome => Gnome::watch_hold_keys(keys, on_change, on_problem),
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

/// Where the status overlay's surface goes: worked out on the toolkit's thread, opened on the
/// overlay's own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OverlayPlan {
    /// A layer-shell surface on the overlay layer, hanging from the top edge.
    Layer,
    /// An override-redirect `XWayland` window `top` logical pixels below the screen's top edge.
    Xwayland { top: f64 },
}

impl OverlayPlan {
    /// Opens the hidden surface, `width` × `height` points; the calling thread then owns it.
    ///
    /// # Errors
    /// Why the desktop gave no surface; the overlay then stays off.
    pub fn open(
        self,
        width: f32,
        height: f32,
        takes_clicks: bool,
    ) -> Result<OverlaySurface, HostError> {
        let (sender, events) = mpsc::channel();
        let backing = match self {
            Self::Layer => Backing::Layer(Box::new(layer::Layer::open(width, height)?)),
            Self::Xwayland { top } => Backing::Xwayland(xwayland::Window::open(
                (width, height),
                top,
                takes_clicks,
                sender.clone(),
            )?),
        };
        Ok(OverlaySurface {
            backing,
            events,
            waker: OverlayWaker(sender),
        })
    }
}

/// Wakes the overlay's thread from any thread.
#[derive(Clone, Debug)]
pub struct OverlayWaker(mpsc::Sender<OverlayEvent>);

impl OverlayWaker {
    pub fn wake(&self) {
        let _ = self.0.send(OverlayEvent::Woken);
    }
}

/// The status overlay's surface: hidden until shown, never taking the keyboard.
pub struct OverlaySurface {
    backing: Backing,
    events: mpsc::Receiver<OverlayEvent>,
    waker: OverlayWaker,
}

enum Backing {
    Layer(Box<layer::Layer>),
    Xwayland(xwayland::Window),
}

impl OverlaySurface {
    #[must_use]
    pub fn waker(&self) -> OverlayWaker {
        self.waker.clone()
    }

    /// Physical pixels per point; frames are drawn at this scale.
    #[must_use]
    pub fn scale(&self) -> f32 {
        match &self.backing {
            Backing::Layer(layer) => layer.scale(),
            Backing::Xwayland(window) => window.scale(),
        }
    }

    /// Shows `frame` on a surface that was hidden.
    ///
    /// # Errors
    /// Why the desktop did not show it.
    pub fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        match &mut self.backing {
            Backing::Layer(layer) => layer.show(frame),
            Backing::Xwayland(window) => window.show(frame),
        }
    }

    /// Replaces what a shown surface shows.
    ///
    /// # Errors
    /// Why the desktop did not take the frame.
    pub fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        match &mut self.backing {
            Backing::Layer(layer) => layer.present(frame),
            Backing::Xwayland(window) => window.present(frame),
        }
    }

    pub fn hide(&mut self) {
        match &mut self.backing {
            Backing::Layer(layer) => layer.hide(),
            Backing::Xwayland(window) => window.hide(),
        }
    }

    /// Waits for a press, a wake or `deadline`; with no deadline, for one of the first two.
    pub fn wait(&mut self, deadline: Option<Instant>) -> OverlayEvent {
        match deadline {
            Some(at) => self
                .events
                .recv_timeout(at.saturating_duration_since(Instant::now()))
                .unwrap_or(OverlayEvent::Due),
            None => self.events.recv().unwrap_or(OverlayEvent::Due),
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
