//! GNOME Wayland host: the app's own X11 GTK clipboard and `gio`, since Mutter shows apps no focused window and takes no typed keys.

mod toolkit;

pub use self::toolkit::keep_unfocused;
use self::toolkit::{Gtk, Toolkit};

use std::fmt;
use std::path::Path;

use super::command::{CommandRunner, SystemRunner, tool};
use super::{App, Delivered, HoldSource, Host, HostError, PasteText, WebUrl};
use crate::hold::{HoldChange, HoldKey, HoldKind};

/// Where GNOME dictation goes: there is no window to bind, so the text waits on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clipboard;

impl fmt::Display for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the clipboard")
    }
}

/// The GNOME Wayland desktop.
pub struct Gnome<R = SystemRunner, T = Gtk> {
    run: R,
    toolkit: T,
}

impl Gnome {
    pub(super) fn system() -> Self {
        Self {
            run: SystemRunner,
            toolkit: Gtk,
        }
    }

    /// GNOME cannot tell an app when a key is let go, so a held key cannot dictate.
    ///
    /// # Errors
    /// Never fails.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same signature as the other hosts, whose hook can fail"
    )]
    pub fn watch_hold_keys(
        _keys: &[(HoldKind, HoldKey)],
        _on_change: impl Fn(HoldKind, HoldChange) + Send + 'static,
        _on_problem: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<HoldSource, HostError> {
        Ok(HoldSource::Unavailable(
            "GNOME Wayland does not tell Thalvi when a key is let go, so holding a key to dictate or write is unavailable. The wake word still works.",
        ))
    }
}

impl<R: CommandRunner, T: Toolkit> Gnome<R, T> {
    fn gio_open(&self, what: &str) -> Result<(), HostError> {
        self.run
            .run("gio", &["open", what], None)
            .map(|_| ())
            .map_err(|error| tool(&error))
    }
}

impl<R: CommandRunner, T: Toolkit> Host for Gnome<R, T> {
    type Target = Clipboard;

    // The overlay is an ordinary window here, and a click on it only focuses it.
    fn overlay_takes_clicks(&self) -> bool {
        true
    }

    fn copies_only(&self) -> bool {
        true
    }

    fn capture_target(&self) -> Result<Clipboard, HostError> {
        Ok(Clipboard)
    }

    fn deliver(
        &self,
        _target: &Clipboard,
        text: &PasteText,
        _enter: bool,
    ) -> Result<Delivered, HostError> {
        if text.as_str().is_empty() {
            return Err(HostError::Unsupported(
                "There is no text to copy, and Thalvi cannot press Enter on GNOME Wayland."
                    .to_owned(),
            ));
        }
        self.toolkit
            .set_clipboard(text.as_str())
            .map(|()| Delivered::OnClipboard)
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        // An empty copy would clear the clipboard instead of keeping anything.
        if text.is_empty() {
            return Ok(());
        }
        self.toolkit.set_clipboard(text)
    }

    fn focus_app(&self, _app: &App) -> Result<Clipboard, HostError> {
        Err(HostError::Unsupported(
            "Switching apps is unavailable on GNOME Wayland.".to_owned(),
        ))
    }

    fn open_url(&self, url: &WebUrl) -> Result<(), HostError> {
        self.gio_open(url.as_str())
    }

    fn open_folder(&self, path: &Path) -> Result<(), HostError> {
        self.gio_open(&path.to_string_lossy())
    }

    // The app runs on the X11 backend here, where a window can be placed below the panel.
    fn prepare_overlay(&self, _width: f64, _height: f64) -> Result<f64, HostError> {
        self.toolkit.work_area_top()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::super::command::{CommandError, fakes::FakeRunner};
    use super::*;

    /// A clipboard that keeps what it is given, or refuses it, and a fixed work area.
    #[derive(Default)]
    struct FakeToolkit {
        clipboard: Mutex<Vec<String>>,
        refuse: Option<HostError>,
    }

    impl Toolkit for FakeToolkit {
        fn set_clipboard(&self, text: &str) -> Result<(), HostError> {
            if let Some(error) = &self.refuse {
                return Err(error.clone());
            }
            self.clipboard.lock().unwrap().push(text.to_owned());
            Ok(())
        }

        fn work_area_top(&self) -> Result<f64, HostError> {
            Ok(32.0)
        }
    }

    fn host_with(
        toolkit: FakeToolkit,
        reply: impl Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync + 'static,
    ) -> Gnome<FakeRunner, FakeToolkit> {
        Gnome {
            run: FakeRunner::new(reply),
            toolkit,
        }
    }

    fn host(
        reply: impl Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync + 'static,
    ) -> Gnome<FakeRunner, FakeToolkit> {
        host_with(FakeToolkit::default(), reply)
    }

    fn clipboard(host: &Gnome<FakeRunner, FakeToolkit>) -> Vec<String> {
        host.toolkit.clipboard.lock().unwrap().clone()
    }

    fn typed(text: &str) -> PasteText {
        PasteText::new(text).unwrap()
    }

    #[test]
    fn dictation_waits_on_the_clipboard_without_any_key_or_focus_command() {
        let host = host(|_, _| Ok(String::new()));
        assert_eq!(host.capture_target(), Ok(Clipboard));
        assert_eq!(Clipboard.to_string(), "the clipboard");
        for enter in [false, true] {
            assert_eq!(
                host.deliver(&Clipboard, &typed("café 👋 --help"), enter),
                Ok(Delivered::OnClipboard)
            );
        }
        assert_eq!(clipboard(&host), ["café 👋 --help", "café 👋 --help"]);
        assert_eq!(
            host.run.commands(),
            Vec::<String>::new(),
            "no tool runs: no wl-copy, no key, no focus"
        );
    }

    #[test]
    fn copying_sets_the_clipboard_and_empty_text_leaves_it_alone() {
        let host = host(|_, _| Ok(String::new()));
        host.copy("kept").unwrap();
        host.copy("").unwrap();
        assert_eq!(clipboard(&host), ["kept"]);
    }

    #[test]
    fn the_overlay_goes_below_the_panel_at_the_work_areas_top() {
        let host = host(|_, _| Ok(String::new()));
        assert_eq!(host.prepare_overlay(460.0, 200.0), Ok(32.0));
    }

    #[test]
    fn a_clipboard_that_cannot_be_set_or_text_that_is_empty_is_a_typed_reason() {
        let refused = HostError::NothingTyped("The clipboard did not answer.".to_owned());
        let refusing = host_with(
            FakeToolkit {
                refuse: Some(refused.clone()),
                ..FakeToolkit::default()
            },
            |_, _| Ok(String::new()),
        );
        assert_eq!(
            refusing.deliver(&Clipboard, &typed("hi"), false),
            Err(refused)
        );
        let nothing = host(|_, _| Ok(String::new()));
        assert_eq!(
            nothing.deliver(&Clipboard, &typed(""), true),
            Err(HostError::Unsupported(
                "There is no text to copy, and Thalvi cannot press Enter on GNOME Wayland."
                    .to_owned()
            ))
        );
        assert_eq!(clipboard(&nothing), Vec::<String>::new());
    }

    #[test]
    fn switching_apps_is_refused_in_plain_words_and_starts_nothing() {
        let host = host(|_, _| Ok(String::new()));
        let app = App {
            classes: vec!["Code".to_owned()],
            executables: Vec::new(),
            shortcut: None,
        };
        assert_eq!(
            host.focus_app(&app),
            Err(HostError::Unsupported(
                "Switching apps is unavailable on GNOME Wayland.".to_owned()
            ))
        );
        assert_eq!(host.run.commands(), Vec::<String>::new());
    }

    #[test]
    fn links_and_folders_open_through_gio_and_a_missing_gio_is_unsupported() {
        let host_ok = host(|_, _| Ok(String::new()));
        host_ok
            .open_url(&WebUrl::parse("https://github.com/NotPellew/Thalvi").unwrap())
            .unwrap();
        host_ok
            .open_folder(&PathBuf::from("/home/me/My Notes"))
            .unwrap();
        assert_eq!(
            host_ok.run.commands(),
            [
                "gio open https://github.com/NotPellew/Thalvi",
                "gio open /home/me/My Notes"
            ]
        );
        let missing = host(|_, _| Err(CommandError::NotInstalled("gio".to_owned())));
        assert_eq!(
            missing.open_folder(Path::new("/tmp")),
            Err(HostError::Unsupported(
                "Desktop tool gio is not installed.".to_owned()
            ))
        );
    }

    #[test]
    fn the_hold_key_is_no_failure_on_gnome_but_says_why_it_does_nothing() {
        let source =
            Gnome::watch_hold_keys(&[(HoldKind::Dictate, HoldKey::alt_z())], |_, _| {}, drop);
        assert!(matches!(source, Ok(HoldSource::Unavailable(reason)) if reason.contains("let go")));
    }
}
