//! GNOME Wayland host: the clipboard and `gio`, since Mutter shows apps no focused window and takes no typed keys.

use std::fmt;
use std::path::Path;

use super::command::{CommandRunner, SystemRunner, tool, wl_copy};
use super::{App, Delivered, HoldSource, Host, HostError, TypedText, WebUrl};
use crate::hold::HoldKey;

/// Where GNOME dictation goes: there is no window to bind, so the text waits on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clipboard;

impl fmt::Display for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the clipboard")
    }
}

/// The GNOME Wayland desktop.
pub struct Gnome<R = SystemRunner> {
    run: R,
}

impl Gnome {
    pub(super) fn system() -> Self {
        Self { run: SystemRunner }
    }

    /// GNOME cannot tell an app when a key is let go, so a held key cannot dictate.
    ///
    /// # Errors
    /// Never fails.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same signature as the other hosts, whose hook can fail"
    )]
    pub fn watch_hold_key(
        _key: &HoldKey,
        _on_change: impl Fn(bool) + Send + 'static,
    ) -> Result<HoldSource, HostError> {
        Ok(HoldSource::Unavailable(
            "GNOME Wayland does not tell Thalvi when a key is let go, so holding a key to dictate is unavailable. The wake word still works.",
        ))
    }
}

impl<R: CommandRunner> Gnome<R> {
    fn gio_open(&self, what: &str) -> Result<(), HostError> {
        self.run
            .run("gio", &["open", what], None)
            .map(|_| ())
            .map_err(|error| tool(&error))
    }
}

impl<R: CommandRunner> Host for Gnome<R> {
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
        text: &TypedText,
        _enter: bool,
    ) -> Result<Delivered, HostError> {
        if text.as_str().is_empty() {
            return Err(HostError::Unsupported(
                "There is no text to copy, and Thalvi cannot press Enter on GNOME Wayland."
                    .to_owned(),
            ));
        }
        wl_copy(&self.run, text.as_str()).map(|()| Delivered::OnClipboard)
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        wl_copy(&self.run, text)
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

    // Wayland apps cannot place their windows, so Mutter chooses where the overlay goes.
    fn prepare_overlay(&self, _width: f64, _height: f64) -> Result<f64, HostError> {
        Ok(0.0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::command::{CommandError, fakes::FakeRunner};
    use super::*;

    fn host(
        reply: impl Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync + 'static,
    ) -> Gnome<FakeRunner> {
        Gnome {
            run: FakeRunner::new(reply),
        }
    }

    fn typed(text: &str) -> TypedText {
        TypedText::new(text).unwrap()
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
        assert_eq!(
            host.run.commands(),
            [
                "wl-copy --type text/plain;charset=utf-8 <café 👋 --help>",
                "wl-copy --type text/plain;charset=utf-8 <café 👋 --help>",
            ]
        );
    }

    #[test]
    fn a_clipboard_that_cannot_be_set_or_text_that_is_empty_is_a_typed_reason() {
        let missing = host(|_, _| Err(CommandError::NotInstalled("wl-copy".to_owned())));
        assert_eq!(
            missing.deliver(&Clipboard, &typed("hi"), false),
            Err(HostError::Unsupported(
                "Desktop tool wl-copy is not installed.".to_owned()
            ))
        );
        let nothing = host(|_, _| Ok(String::new()));
        assert_eq!(
            nothing.deliver(&Clipboard, &typed(""), true),
            Err(HostError::Unsupported(
                "There is no text to copy, and Thalvi cannot press Enter on GNOME Wayland."
                    .to_owned()
            ))
        );
        assert_eq!(nothing.run.commands(), Vec::<String>::new());
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
        let source = Gnome::watch_hold_key(&HoldKey::default(), |_| {});
        assert!(matches!(source, Ok(HoldSource::Unavailable(reason)) if reason.contains("let go")));
    }
}
