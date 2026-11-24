//! Whether a Linux session runs the login entry, and the advice Hyprland needs instead.

use thalvi_desktop::host::Session;

use crate::switch::Unavailable;

/// The plugin's entry is an XDG autostart file, which the user's system startup list governs.
pub fn turned_off_by_user() -> bool {
    false
}

/// Why this session would never run the entry, when it would not.
pub fn unavailable() -> Option<Unavailable> {
    unavailable_on(Session::detect().ok(), hyprland::unavailable)
}

/// GNOME runs XDG autostart entries itself, so its login switch is the plain OS entry; only
/// Hyprland needs advice. `hyprland` is asked only on Hyprland.
fn unavailable_on(
    session: Option<Session>,
    hyprland: impl FnOnce() -> Option<Unavailable>,
) -> Option<Unavailable> {
    match session {
        Some(Session::Hyprland) => hyprland(),
        Some(Session::Gnome) | None => None,
    }
}

/// Hyprland runs only the `exec-once` lines of `hyprland.conf`. A session manager such as uwsm
/// runs XDG autostart entries by starting systemd's `xdg-desktop-autostart.target`; that target
/// being up is what tells an entry will be read.
mod hyprland {
    use std::path::PathBuf;
    use std::process::Command;

    use crate::startup::LOGIN_ARG;
    use crate::switch::Unavailable;

    pub fn unavailable() -> Option<Unavailable> {
        if autostart_runs(&target_state()) {
            return None;
        }
        Some(no_autostart(launcher().as_deref()))
    }

    /// Empty when `systemctl` is missing or fails, as on a session without a systemd user manager.
    fn target_state() -> String {
        Command::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=ActiveState",
                "--value",
                "xdg-desktop-autostart.target",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    }

    /// `activating` counts: an entry that starts Thalvi at login runs while the target is still coming up.
    fn autostart_runs(active_state: &str) -> bool {
        matches!(active_state, "active" | "activating")
    }

    /// The `AppImage` when there is one, since the executable then sits in a temporary mount.
    fn launcher() -> Option<String> {
        std::env::var_os("APPIMAGE")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .and_then(|path| path.to_str().map(str::to_owned))
    }

    fn no_autostart(launcher: Option<&str>) -> Unavailable {
        let (reason, line) = match launcher {
            Some(path) => (
                "Hyprland does not start login entries by itself, so a switch here would do nothing. To start Thalvi at login, add this line to hyprland.conf:",
                Some(exec_once(path)),
            ),
            None => (
                "Hyprland does not start login entries by itself, so a switch here would do nothing. To start Thalvi at login, add an exec-once line to hyprland.conf that runs Thalvi with --login.",
                None,
            ),
        };
        Unavailable {
            reason: reason.to_owned(),
            line,
        }
    }

    /// `exec-once` runs its value with `sh -c`, and `hyprland.conf` reads `#` as a comment unless doubled.
    fn exec_once(path: &str) -> String {
        let safe = !path.is_empty()
            && path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-./:@%+=,".contains(&byte));
        let quoted = if safe {
            path.to_owned()
        } else {
            format!("'{}'", path.replace('\'', r"'\''"))
        };
        format!("exec-once = {quoted} {LOGIN_ARG}").replace('#', "##")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_a_started_autostart_target_means_login_entries_run() {
            for (state, runs) in [
                ("active", true),
                ("activating", true),
                ("inactive", false),
                ("failed", false),
                ("", false),
            ] {
                assert_eq!(autostart_runs(state), runs, "{state:?}");
            }
        }

        #[test]
        fn the_line_for_hyprland_conf_quotes_what_the_shell_and_the_config_would_misread() {
            for (path, line) in [
                (
                    "/home/me/Thalvi.AppImage",
                    "exec-once = /home/me/Thalvi.AppImage --login",
                ),
                (
                    "/home/me/My Apps/Thalvi.AppImage",
                    "exec-once = '/home/me/My Apps/Thalvi.AppImage' --login",
                ),
                (
                    "/home/me/it's $HOME;x/thalvi",
                    r"exec-once = '/home/me/it'\''s $HOME;x/thalvi' --login",
                ),
                (
                    "/home/me/#1/thalvi",
                    "exec-once = '/home/me/##1/thalvi' --login",
                ),
            ] {
                assert_eq!(exec_once(path), line, "{path}");
            }
        }

        #[test]
        fn the_note_carries_the_line_unless_the_path_is_unknown() {
            let known = no_autostart(Some("/opt/thalvi"));
            assert_eq!(
                known.line.as_deref(),
                Some("exec-once = /opt/thalvi --login")
            );
            let unknown = no_autostart(None);
            assert_eq!(unknown.line, None);
            assert!(unknown.reason.contains("exec-once"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hyprland_gets_login_advice_and_gnome_keeps_the_plain_os_entry() {
        let advice = || {
            Some(Unavailable {
                reason: "Hyprland advice".to_owned(),
                line: None,
            })
        };
        for (session, shown) in [
            (Some(Session::Hyprland), true),
            (Some(Session::Gnome), false),
            (None, false),
        ] {
            let mut asked = false;
            let result = unavailable_on(session, || {
                asked = true;
                advice()
            });
            assert_eq!(result.is_some(), shown, "{session:?}");
            assert_eq!(asked, shown, "{session:?}: Hyprland alone is asked");
        }
    }
}
