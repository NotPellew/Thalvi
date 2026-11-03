//! Hyprland host: hyprctl, wtype and wl-copy.

mod command;
mod state;

use std::fmt;
use std::path::Path;

use serde::Deserialize;

use self::command::{CommandError, CommandRunner, SystemRunner};
use super::{
    App, Host, HostError, Modifier, OVERLAY_TITLE, Shortcut, StateServer, TypedText, WebUrl,
};

/// The window as Hyprland identifies it; the pid tells a reused address from the original window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    address: String,
    pid: u32,
    class: String,
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.class)
    }
}

/// The Hyprland desktop.
pub struct Hyprland<R = SystemRunner> {
    run: R,
}

impl Hyprland {
    /// # Errors
    /// [`HostError::Unsupported`] naming this desktop unless Hyprland is running.
    pub fn detect() -> Result<Self, HostError> {
        detect_in(
            std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().as_deref(),
            std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        )
        .map(|()| Self { run: SystemRunner })
    }

    /// Answers each query on the user's private runtime folder with `state()` until the guard is
    /// dropped.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when no private folder is available or another app answers.
    pub fn serve_state(
        state: impl Fn() -> &'static str + Send + 'static,
    ) -> Result<StateServer, HostError> {
        state::serve(state)
    }

    /// The running app's state word; `None` when no app answers.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when the answer could not be asked or read.
    pub fn query_state() -> Result<Option<String>, HostError> {
        state::query()
    }
}

fn detect_in(signature: Option<&str>, desktop: Option<&str>) -> Result<(), HostError> {
    if signature.is_some_and(|signature| !signature.is_empty()) {
        return Ok(());
    }
    let name = desktop.filter(|name| !name.is_empty()).unwrap_or("unknown");
    Err(HostError::Unsupported(format!(
        "Thalvi supports Hyprland and Windows for now; this desktop is {name}."
    )))
}

/// The default browsers Thalvi can ask for a new tab, by `.desktop` file, and their commands.
const BROWSERS: [(&str, &str); 6] = [
    ("firefox_firefox.desktop", "/snap/bin/firefox"),
    ("firefox.desktop", "firefox"),
    ("chromium_chromium.desktop", "/snap/bin/chromium"),
    ("chromium.desktop", "chromium"),
    ("google-chrome.desktop", "google-chrome"),
    ("google-chrome-stable.desktop", "google-chrome-stable"),
];

const NO_FOCUSED_WINDOW: &str = "The desktop did not identify the focused window.";
const INVALID_WINDOW_LIST: &str = "The desktop returned an invalid window list.";
const INVALID_MONITOR_LIST: &str = "The desktop returned an invalid monitor list.";
const NOTHING_TYPED: &str = " Nothing was typed.";

/// One window as `hyprctl -j` prints it; any field may be absent, as for no focused window.
#[derive(Deserialize)]
struct Row {
    address: Option<String>,
    pid: Option<i64>,
    class: Option<String>,
    #[serde(rename = "focusHistoryID")]
    focus_history: Option<i64>,
}

impl Row {
    fn window(self) -> Option<(i64, Window)> {
        let address = self.address.filter(|address| {
            address
                .strip_prefix("0x")
                .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        })?;
        let pid = u32::try_from(self.pid?).ok().filter(|pid| *pid > 0)?;
        let class = self.class.filter(|class| !class.is_empty())?;
        // Windows that were never focused have no history; they rank last.
        let history = self.focus_history.filter(|id| *id >= 0).unwrap_or(i64::MAX);
        Some((
            history,
            Window {
                address,
                pid,
                class,
            },
        ))
    }
}

/// One monitor as `hyprctl -j monitors` prints it.
#[derive(Deserialize)]
struct Monitor {
    #[serde(default)]
    focused: bool,
    /// Space panels reserve, in logical pixels, in the order left, top, right, bottom.
    reserved: [i64; 4],
}

/// A tool failure before any input was sent: a missing tool means this desktop lacks it.
fn tool(error: &CommandError) -> HostError {
    match error {
        CommandError::NotInstalled(_) => HostError::Unsupported(error.to_string()),
        CommandError::TimedOut(_) | CommandError::Failed { .. } => {
            HostError::NothingTyped(error.to_string())
        }
    }
}

/// States on a failure that nothing reached a window.
fn nothing_typed(error: HostError) -> HostError {
    match error {
        HostError::Unsupported(message) => HostError::Unsupported(message + NOTHING_TYPED),
        HostError::NothingTyped(message) => HostError::NothingTyped(message + NOTHING_TYPED),
        partial @ HostError::MaybePartial(_) => partial,
    }
}

fn key_arguments(shortcut: &Shortcut) -> Vec<&str> {
    let mut args = Vec::new();
    for modifier in &shortcut.modifiers {
        args.extend([
            "-M",
            match modifier {
                Modifier::Ctrl => "ctrl",
                Modifier::Shift => "shift",
                Modifier::Alt => "alt",
                Modifier::Logo => "logo",
            },
        ]);
    }
    args.extend(["-k", &shortcut.key]);
    args
}

impl<R: CommandRunner> Hyprland<R> {
    fn json(&self, query: &str) -> Result<String, HostError> {
        self.run
            .run("hyprctl", &["-j", query], None)
            .map_err(|error| tool(&error))
    }

    fn active(&self) -> Result<Window, HostError> {
        serde_json::from_str::<Row>(&self.json("activewindow")?)
            .ok()
            .and_then(Row::window)
            .map(|(_, window)| window)
            .ok_or_else(|| HostError::NothingTyped(NO_FOCUSED_WINDOW.to_owned()))
    }

    /// Every window, most recently focused first.
    fn windows(&self) -> Result<Vec<Window>, HostError> {
        let invalid = || HostError::NothingTyped(INVALID_WINDOW_LIST.to_owned());
        let rows: Vec<Row> = serde_json::from_str(&self.json("clients")?).map_err(|_| invalid())?;
        let mut windows = rows
            .into_iter()
            .map(|row| row.window().ok_or_else(invalid))
            .collect::<Result<Vec<_>, _>>()?;
        windows.sort_by_key(|(history, _)| *history);
        Ok(windows.into_iter().map(|(_, window)| window).collect())
    }

    /// Focuses `target` by address, then checks that it really is the focused window.
    fn focus(&self, target: &Window) -> Result<(), HostError> {
        if !self.windows()?.contains(target) {
            return Err(HostError::NothingTyped(
                "The original window closed.".to_owned(),
            ));
        }
        self.run
            .run(
                "hyprctl",
                &[
                    "dispatch",
                    "focuswindow",
                    &format!("address:{}", target.address),
                ],
                None,
            )
            .map_err(|error| tool(&error))?;
        if self.active()? != *target {
            return Err(HostError::NothingTyped(
                "The target could not be focused.".to_owned(),
            ));
        }
        Ok(())
    }
}

impl<R: CommandRunner> Host for Hyprland<R> {
    type Target = Window;

    fn capture_target(&self) -> Result<Window, HostError> {
        self.active()
    }

    fn deliver(&self, target: &Window, text: &TypedText, enter: bool) -> Result<(), HostError> {
        self.focus(target).map_err(nothing_typed)?;
        let mut args = vec!["-"];
        if enter {
            args.extend(["-k", "Return"]);
        }
        self.run
            .run("wtype", &args, Some(text.as_str()))
            .map(|_| ())
            .map_err(|error| match error {
                CommandError::NotInstalled(_) => nothing_typed(tool(&error)),
                CommandError::TimedOut(_) | CommandError::Failed { .. } => HostError::MaybePartial(
                    "Typing failed or stopped partway. Check the target before retrying."
                        .to_owned(),
                ),
            })
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        // An empty copy would clear the clipboard instead of keeping anything.
        if text.is_empty() {
            return Ok(());
        }
        self.run
            .run(
                "wl-copy",
                &["--type", "text/plain;charset=utf-8"],
                Some(text),
            )
            .map(|_| ())
            .map_err(|error| tool(&error))
    }

    fn focus_app(&self, app: &App) -> Result<Window, HostError> {
        let classes: Vec<String> = app
            .classes
            .iter()
            .map(|class| class.to_lowercase())
            .collect();
        let selected = self
            .windows()?
            .into_iter()
            .find(|window| classes.contains(&window.class.to_lowercase()))
            .ok_or_else(|| HostError::NothingTyped("No window of that app is open.".to_owned()))?;
        self.focus(&selected)?;
        if let Some(shortcut) = &app.shortcut {
            self.run
                .run("wtype", &key_arguments(shortcut), None)
                .map_err(|error| match error {
                    CommandError::NotInstalled(_) => tool(&error),
                    CommandError::TimedOut(_) | CommandError::Failed { .. } => {
                        HostError::MaybePartial(
                            "The app's shortcut may have been pressed only partly. Check the app."
                                .to_owned(),
                        )
                    }
                })?;
        }
        Ok(selected)
    }

    fn open_url(&self, url: &WebUrl) -> Result<(), HostError> {
        let default = self
            .run
            .run("xdg-settings", &["get", "default-web-browser"], None)
            .map_err(|error| tool(&error))?;
        let (_, browser) = BROWSERS
            .iter()
            .find(|(desktop_file, _)| *desktop_file == default.trim())
            .ok_or_else(|| {
                HostError::Unsupported(
                    "The default browser does not support this new-tab action yet.".to_owned(),
                )
            })?;
        self.run
            .run(browser, &["--new-tab", url.as_str()], None)
            .map(|_| ())
            .map_err(|error| tool(&error))
    }

    fn open_folder(&self, path: &Path) -> Result<(), HostError> {
        self.run
            .run("xdg-open", &[&path.to_string_lossy()], None)
            .map(|_| ())
            .map_err(|error| tool(&error))
    }

    fn prepare_overlay(&self, width: f64, height: f64) -> Result<f64, HostError> {
        let monitors: Vec<Monitor> = serde_json::from_str(&self.json("monitors")?)
            .map_err(|_| HostError::NothingTyped(INVALID_MONITOR_LIST.to_owned()))?;
        // The window opens on the focused monitor. A rule added later wins over earlier ones, and
        // Hyprland applies `size` and `move` each time the window maps, so a hidden overlay appears
        // at its new size and place; a client cannot resize its own floating window afterwards.
        let top = monitors
            .iter()
            .find(|monitor| monitor.focused)
            .or(monitors.first())
            .map_or(0, |monitor| monitor.reserved[1].max(0));
        let rule = format!(
            "match:title ^({OVERLAY_TITLE})$, float on, pin on, no_initial_focus on, no_focus on, \
             size {width} {height}, move ((monitor_w*0.5)-({width}*0.5)) {top}, border_size 0, \
             no_shadow on, no_anim on"
        );
        self.run
            .run("hyprctl", &["keyword", "windowrule", &rule], None)
            .map_err(|error| tool(&error))?;
        Ok(f64::from(i32::try_from(top).unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    type Reply = Box<dyn Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync>;

    /// Records every command and answers with `reply`.
    struct FakeRunner {
        calls: Mutex<Vec<(Vec<String>, Option<String>)>>,
        reply: Reply,
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            stdin: Option<&str>,
        ) -> Result<String, CommandError> {
            let command: Vec<String> = std::iter::once(program)
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect();
            let reply = (self.reply)(&command, stdin);
            self.calls
                .lock()
                .unwrap()
                .push((command, stdin.map(str::to_owned)));
            reply
        }
    }

    fn host(
        reply: impl Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync + 'static,
    ) -> Hyprland<FakeRunner> {
        Hyprland {
            run: FakeRunner {
                calls: Mutex::default(),
                reply: Box::new(reply),
            },
        }
    }

    fn commands(host: &Hyprland<FakeRunner>) -> Vec<String> {
        host.run
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(argv, stdin)| match stdin {
                Some(stdin) => format!("{} <{stdin}>", argv.join(" ")),
                None => argv.join(" "),
            })
            .collect()
    }

    fn row(address: &str, pid: i64, class: &str, history: i64) -> String {
        format!(
            r#"{{"address": "{address}", "mapped": true, "title": "TITLE", "pid": {pid},
                "class": "{class}", "focusHistoryID": {history}}}"#
        )
    }

    fn window(address: &str, pid: u32, class: &str) -> Window {
        Window {
            address: address.to_owned(),
            pid,
            class: class.to_owned(),
        }
    }

    /// A desktop with `windows` open, `active` focused, and focus following `focuswindow`.
    fn desktop(
        windows: Vec<String>,
        active: &'static str,
        focus_lands_on: &'static str,
    ) -> Hyprland<FakeRunner> {
        let focused = Mutex::new(active.to_owned());
        host(move |argv, _| {
            let args: Vec<&str> = argv.iter().map(String::as_str).collect();
            match args[..] {
                ["hyprctl", "-j", "clients"] => Ok(format!("[{}]", windows.join(","))),
                ["hyprctl", "-j", "activewindow"] => {
                    let focused = focused.lock().unwrap();
                    Ok(windows
                        .iter()
                        .find(|window| window.contains(&format!("\"{}\"", *focused)))
                        .cloned()
                        .unwrap_or_else(|| "{}".to_owned()))
                }
                ["hyprctl", "dispatch", "focuswindow", _] => {
                    *focused.lock().unwrap() = focus_lands_on.to_owned();
                    Ok("ok".to_owned())
                }
                _ => Ok(String::new()),
            }
        })
    }

    fn typed(text: &str) -> TypedText {
        TypedText::new(text).unwrap()
    }

    #[test]
    fn only_a_hyprland_session_is_detected_and_other_desktops_are_named() {
        for (signature, desktop, expected) in [
            (Some("abc"), None, Ok(())),
            (Some("abc"), Some("GNOME"), Ok(())),
            (
                None,
                Some("GNOME"),
                Err("Thalvi supports Hyprland and Windows for now; this desktop is GNOME."),
            ),
            (
                Some(""),
                Some("KDE"),
                Err("Thalvi supports Hyprland and Windows for now; this desktop is KDE."),
            ),
            (
                None,
                None,
                Err("Thalvi supports Hyprland and Windows for now; this desktop is unknown."),
            ),
        ] {
            let result = detect_in(signature, desktop);
            match expected {
                Ok(()) => assert_eq!(result, Ok(()), "{signature:?} {desktop:?}"),
                Err(message) => assert_eq!(
                    result,
                    Err(HostError::Unsupported(message.to_owned())),
                    "{signature:?} {desktop:?}"
                ),
            }
        }
    }

    #[test]
    fn the_focused_window_is_bound_by_address_pid_and_class() {
        let host = desktop(vec![row("0x1f", 42, "kitty", 0)], "0x1f", "0x1f");
        assert_eq!(host.capture_target(), Ok(window("0x1f", 42, "kitty")));
        assert_eq!(window("0x1f", 42, "kitty").to_string(), "kitty");
        assert_eq!(commands(&host), ["hyprctl -j activewindow"]);
    }

    #[test]
    fn an_unidentifiable_focus_is_refused_in_plain_words() {
        for (json, expected) in [
            ("{}", "The desktop did not identify the focused window."),
            (
                r#"{"address": "main", "pid": 1, "class": "a"}"#,
                "The desktop did not identify the focused window.",
            ),
            (
                r#"{"address": "0x", "pid": 1, "class": "a"}"#,
                "The desktop did not identify the focused window.",
            ),
            (
                r#"{"address": "0x1", "pid": 0, "class": "a"}"#,
                "The desktop did not identify the focused window.",
            ),
            (
                r#"{"address": "0x1", "pid": -3, "class": "a"}"#,
                "The desktop did not identify the focused window.",
            ),
            (
                r#"{"address": "0x1", "pid": 1, "class": ""}"#,
                "The desktop did not identify the focused window.",
            ),
            (
                r#"{"address": "0x1", "pid": "1", "class": "a"}"#,
                "The desktop did not identify the focused window.",
            ),
            (
                "not json",
                "The desktop did not identify the focused window.",
            ),
        ] {
            let json = json.to_owned();
            let host = host(move |_, _| Ok(json.clone()));
            assert_eq!(
                host.capture_target(),
                Err(HostError::NothingTyped(expected.to_owned()))
            );
        }
    }

    #[test]
    fn missing_or_failing_tools_are_told_apart() {
        for (error, expected) in [
            (
                CommandError::NotInstalled("hyprctl".to_owned()),
                HostError::Unsupported("Desktop tool hyprctl is not installed.".to_owned()),
            ),
            (
                CommandError::TimedOut("hyprctl".to_owned()),
                HostError::NothingTyped("Desktop tool hyprctl took too long.".to_owned()),
            ),
            (
                CommandError::Failed {
                    program: "hyprctl".to_owned(),
                    reason: None,
                },
                HostError::NothingTyped("Desktop tool hyprctl failed.".to_owned()),
            ),
        ] {
            let host = host(move |_, _| Err(error.clone()));
            assert_eq!(host.capture_target(), Err(expected));
        }
    }

    #[test]
    fn delivery_refocuses_the_original_window_and_sends_the_text_on_stdin_only() {
        let text = "--help $(not-a-command) `nor-this` café 👋";
        for (enter, wtype) in [
            (false, format!("wtype - <{text}>")),
            (true, format!("wtype - -k Return <{text}>")),
        ] {
            let host = desktop(
                vec![row("0x1", 1, "browser", 0), row("0x2", 2, "terminal", 1)],
                "0x1",
                "0x2",
            );
            assert_eq!(
                host.deliver(&window("0x2", 2, "terminal"), &typed(text), enter),
                Ok(())
            );
            assert_eq!(
                commands(&host),
                [
                    "hyprctl -j clients",
                    "hyprctl dispatch focuswindow address:0x2",
                    "hyprctl -j activewindow",
                    wtype.as_str(),
                ]
            );
        }
    }

    #[test]
    fn nothing_is_typed_when_the_target_is_gone_reused_or_not_focusable() {
        for (name, windows, lands_on, message) in [
            (
                "closed",
                vec![row("0x1", 1, "browser", 0)],
                "0x1",
                "The original window closed. Nothing was typed.",
            ),
            (
                "address reused by another process",
                vec![row("0x2", 99, "terminal", 0)],
                "0x2",
                "The original window closed. Nothing was typed.",
            ),
            (
                "focus did not move",
                vec![row("0x1", 1, "browser", 0), row("0x2", 2, "terminal", 1)],
                "0x1",
                "The target could not be focused. Nothing was typed.",
            ),
        ] {
            let host = desktop(windows, "0x1", lands_on);
            assert_eq!(
                host.deliver(&window("0x2", 2, "terminal"), &typed("hi"), true),
                Err(HostError::NothingTyped(message.to_owned())),
                "{name}"
            );
            assert!(
                commands(&host)
                    .iter()
                    .all(|command| !command.starts_with("wtype")),
                "{name}"
            );
        }
    }

    #[test]
    fn a_typing_tool_that_fails_after_focus_may_have_typed_part_of_the_text() {
        for (error, expected) in [
            (
                CommandError::Failed {
                    program: "wtype".to_owned(),
                    reason: None,
                },
                HostError::MaybePartial(
                    "Typing failed or stopped partway. Check the target before retrying."
                        .to_owned(),
                ),
            ),
            (
                CommandError::TimedOut("wtype".to_owned()),
                HostError::MaybePartial(
                    "Typing failed or stopped partway. Check the target before retrying."
                        .to_owned(),
                ),
            ),
            (
                CommandError::NotInstalled("wtype".to_owned()),
                HostError::Unsupported(
                    "Desktop tool wtype is not installed. Nothing was typed.".to_owned(),
                ),
            ),
        ] {
            let host = host({
                let error = error.clone();
                move |argv, _| match argv[0].as_str() {
                    "wtype" => Err(error.clone()),
                    _ if argv.last().is_some_and(|arg| arg == "clients") => {
                        Ok(format!("[{}]", row("0x1", 1, "a", 0)))
                    }
                    _ if argv.last().is_some_and(|arg| arg == "activewindow") => {
                        Ok(row("0x1", 1, "a", 0))
                    }
                    _ => Ok(String::new()),
                }
            });
            assert_eq!(
                host.deliver(&window("0x1", 1, "a"), &typed("hi"), false),
                Err(expected),
                "{error:?}"
            );
        }
    }

    #[test]
    fn the_clipboard_gets_the_text_on_stdin_and_empty_text_leaves_it_alone() {
        let host = host(|_, _| Ok(String::new()));
        assert_eq!(host.copy("keep café"), Ok(()));
        assert_eq!(host.copy(""), Ok(()));
        assert_eq!(
            commands(&host),
            ["wl-copy --type text/plain;charset=utf-8 <keep café>"]
        );

        let broken = self::host(|_, _| Err(CommandError::NotInstalled("wl-copy".to_owned())));
        assert_eq!(
            broken.copy("x"),
            Err(HostError::Unsupported(
                "Desktop tool wl-copy is not installed.".to_owned()
            ))
        );
    }

    #[test]
    fn focusing_an_app_picks_its_most_recently_used_window_by_class_ignoring_case() {
        let app = |classes: &[&str], shortcut: Option<Shortcut>| App {
            classes: classes.iter().map(|class| (*class).to_owned()).collect(),
            executables: Vec::new(),
            shortcut,
        };
        let windows = || {
            vec![
                row("0x1", 1, "Slack", 3),
                row("0x2", 2, "Code", 2),
                row("0x3", 3, "code", 1),
                row("0x4", 4, "code", -1),
            ]
        };
        let host = desktop(windows(), "0x1", "0x3");
        assert_eq!(
            host.focus_app(&app(&["CODE"], None)),
            Ok(window("0x3", 3, "code"))
        );
        assert_eq!(
            commands(&host),
            [
                "hyprctl -j clients",
                "hyprctl -j clients",
                "hyprctl dispatch focuswindow address:0x3",
                "hyprctl -j activewindow",
            ]
        );

        let host = desktop(windows(), "0x1", "0x1");
        assert_eq!(
            host.focus_app(&app(&["terminal"], None)),
            Err(HostError::NothingTyped(
                "No window of that app is open.".to_owned()
            ))
        );
        assert_eq!(commands(&host), ["hyprctl -j clients"]);
    }

    #[test]
    fn an_apps_shortcut_is_pressed_only_after_focus_is_verified() {
        let app = App {
            classes: vec!["code".to_owned()],
            executables: Vec::new(),
            shortcut: Some(Shortcut {
                modifiers: vec![
                    Modifier::Ctrl,
                    Modifier::Shift,
                    Modifier::Alt,
                    Modifier::Logo,
                ],
                key: "Return".to_owned(),
            }),
        };
        let host = desktop(vec![row("0x1", 1, "code", 0)], "0x1", "0x1");
        assert_eq!(host.focus_app(&app), Ok(window("0x1", 1, "code")));
        assert_eq!(
            commands(&host).last().map(String::as_str),
            Some("wtype -M ctrl -M shift -M alt -M logo -k Return")
        );

        let host = desktop(
            vec![row("0x1", 1, "code", 0), row("0x2", 2, "slack", 1)],
            "0x2",
            "0x2",
        );
        assert_eq!(
            host.focus_app(&app),
            Err(HostError::NothingTyped(
                "The target could not be focused.".to_owned()
            ))
        );
        assert!(
            commands(&host)
                .iter()
                .all(|command| !command.starts_with("wtype"))
        );
    }

    #[test]
    fn a_folder_opens_in_the_file_manager() {
        let ready = host(|_, _| Ok(String::new()));
        assert_eq!(
            ready.open_folder(Path::new("/home/me/.local/share/thalvi")),
            Ok(())
        );
        assert_eq!(commands(&ready), ["xdg-open /home/me/.local/share/thalvi"]);

        let missing = host(|_, _| Err(CommandError::NotInstalled("xdg-open".to_owned())));
        assert_eq!(
            missing.open_folder(Path::new("/x")),
            Err(HostError::Unsupported(
                "Desktop tool xdg-open is not installed.".to_owned()
            ))
        );
    }

    #[test]
    fn the_overlay_window_is_pinned_below_the_panel_without_taking_focus() {
        let monitor = |focused: bool, top: i64| {
            format!(r#"{{"name": "M", "focused": {focused}, "reserved": [0, {top}, 0, 0]}}"#)
        };
        let cases = [
            ("no panel", format!("[{}]", monitor(true, 0)), 0),
            ("a 40 px bar", format!("[{}]", monitor(true, 40)), 40),
            (
                "the focused monitor's bar, not another's",
                format!("[{}, {}]", monitor(false, 0), monitor(true, 40)),
                40,
            ),
        ];
        for (name, monitors, top) in cases {
            let ready = host(move |argv, _| {
                Ok(if argv.iter().any(|arg| arg == "monitors") {
                    monitors.clone()
                } else {
                    "ok".to_owned()
                })
            });
            assert_eq!(
                ready.prepare_overlay(460.0, 200.0),
                Ok(f64::from(top)),
                "{name}"
            );
            assert_eq!(
                commands(&ready),
                [
                    "hyprctl -j monitors".to_owned(),
                    format!(
                        "hyprctl keyword windowrule match:title ^(Thalvi overlay)$, float on, \
                         pin on, no_initial_focus on, no_focus on, \
                         size 460 200, move ((monitor_w*0.5)-(460*0.5)) {top}, border_size 0, \
                         no_shadow on, no_anim on"
                    )
                ],
                "{name}"
            );
        }

        let garbled = host(|_, _| Ok("not json".to_owned()));
        assert_eq!(
            garbled.prepare_overlay(460.0, 200.0),
            Err(HostError::NothingTyped(
                "The desktop returned an invalid monitor list.".to_owned()
            ))
        );
        let missing = host(|_, _| Err(CommandError::NotInstalled("hyprctl".to_owned())));
        assert_eq!(
            missing.prepare_overlay(460.0, 200.0),
            Err(HostError::Unsupported(
                "Desktop tool hyprctl is not installed.".to_owned()
            ))
        );
    }

    #[test]
    fn urls_open_in_a_new_tab_of_a_supported_default_browser() {
        let url = WebUrl::parse("https://example.com/Case?q=Value").unwrap();
        for (default, command) in [
            ("firefox_firefox.desktop\n", "/snap/bin/firefox"),
            ("firefox.desktop", "firefox"),
            ("chromium_chromium.desktop", "/snap/bin/chromium"),
            ("chromium.desktop", "chromium"),
            ("google-chrome.desktop", "google-chrome"),
            ("google-chrome-stable.desktop", "google-chrome-stable"),
        ] {
            let host = host(move |argv, _| {
                Ok(if argv[0] == "xdg-settings" {
                    default.to_owned()
                } else {
                    String::new()
                })
            });
            assert_eq!(host.open_url(&url), Ok(()), "{default}");
            assert_eq!(
                commands(&host),
                [
                    "xdg-settings get default-web-browser".to_owned(),
                    format!("{command} --new-tab https://example.com/Case?q=Value"),
                ],
                "{default}"
            );
        }

        let host = host(|_, _| Ok("other.desktop".to_owned()));
        assert_eq!(
            host.open_url(&url),
            Err(HostError::Unsupported(
                "The default browser does not support this new-tab action yet.".to_owned()
            ))
        );
        assert_eq!(commands(&host), ["xdg-settings get default-web-browser"]);
    }

    #[test]
    fn hyprctl_window_lists_rank_by_focus_history_and_refuse_bad_rows() {
        let host = desktop(
            vec![
                row("0x1", 1, "a", 5),
                r#"{"address": "0x2", "pid": 2, "class": "never"}"#.to_owned(),
                row("0x3", 3, "b", 0),
                row("0x4", 4, "c", -1),
            ],
            "0x3",
            "0x3",
        );
        let order: Vec<_> = host
            .windows()
            .unwrap()
            .into_iter()
            .map(|window| window.address)
            .collect();
        assert_eq!(order, ["0x3", "0x1", "0x2", "0x4"]);

        for json in ["{}", "[{\"address\": \"0x1\"}]", "nope"] {
            let host = self::host(move |_, _| Ok(json.to_owned()));
            assert_eq!(
                host.windows(),
                Err(HostError::NothingTyped(
                    "The desktop returned an invalid window list.".to_owned()
                )),
                "{json}"
            );
        }
    }
}
