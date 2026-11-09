//! Hyprland host: hyprctl, wtype and wl-copy.

use std::fmt;
use std::path::Path;

use serde::Deserialize;
use xkbcommon::xkb::{self, Keycode, Keysym};

use super::command::{CommandError, CommandRunner, SystemRunner, tool, wl_copy};
use super::{
    App, Delivered, HoldSource, Host, HostError, MAYBE_PASTED, Modifier, OVERLAY_TITLE, PasteText,
    Shortcut, WebUrl,
};
use crate::hold::{HoldKey, HoldKind};

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
    pub(super) fn system() -> Self {
        Self { run: SystemRunner }
    }

    /// The compositor reports the key, through the binds in `hyprland.conf` that run the
    /// `hold-start` and `hold-end` commands, so there is nothing to watch.
    ///
    /// # Errors
    /// Never fails.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same signature as the Windows host, whose hook can fail"
    )]
    pub fn watch_hold_keys(
        _keys: &[(HoldKind, HoldKey)],
        _on_change: impl Fn(HoldKind, bool) + Send + 'static,
    ) -> Result<HoldSource, HostError> {
        Ok(HoldSource::Binds(
            "On Hyprland the hold and write keys are binds in hyprland.conf: see integrations/hyprland/hyprland-binds.conf.",
        ))
    }
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
const NOTHING_TYPED: &str = " Nothing was pasted.";
const ENTER_FAILED: &str = "The text was pasted, but Thalvi may not have pressed Enter. Check the window before trying again.";

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

/// The keyboard as `hyprctl -j devices` prints it: the names its keymap is compiled from.
#[derive(Deserialize)]
struct Keyboard {
    rules: String,
    model: String,
    layout: String,
    variant: String,
    options: String,
    #[serde(default)]
    main: bool,
}

#[derive(Deserialize)]
struct Devices {
    keyboards: Vec<Keyboard>,
}

/// An option as `hyprctl -j getoption` prints it.
#[derive(Deserialize)]
struct StringOption {
    #[serde(default)]
    str: String,
    set: bool,
}

impl StringOption {
    /// The text the user gave, if any; Hyprland prints `[[EMPTY]]` for an option nobody set.
    fn value(self) -> Option<String> {
        Some(self.str).filter(|value| self.set && !value.is_empty() && value != "[[EMPTY]]")
    }
}

impl Keyboard {
    fn keymap(&self, context: &xkb::Context) -> Option<xkb::Keymap> {
        xkb::Keymap::new_from_names(
            context,
            &self.rules,
            &self.model,
            &self.layout,
            &self.variant,
            Some(self.options.clone()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    }
}

/// The keymap in a `kb_file`, which the layout names do not describe; `None` when the file is
/// not an absolute or home path, cannot be read, or is not a keymap.
fn keymap_file(context: &xkb::Context, path: &str) -> Option<xkb::Keymap> {
    let path = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => Path::new(path).to_path_buf(),
    };
    if !path.is_absolute() {
        return None;
    }
    xkb::Keymap::new_from_string(
        context,
        std::fs::read_to_string(path).ok()?,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
}

/// The keycode that types `v` in `keymap`: in its first layout, since `sendshortcut` sends the
/// key with that layout's group whichever one is active, else in the first other layout that has
/// one, since a program matches Ctrl shortcuts against a Latin layout.
fn v_keycode(keymap: &xkb::Keymap) -> Option<u32> {
    (0..keymap.num_layouts()).find_map(|layout| {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find(|code| {
            keymap
                .key_get_syms_by_level(Keycode::new(*code), layout, 0)
                .contains(&Keysym::v)
        })
    })
}

/// States on a failure that nothing reached a window.
fn nothing_typed(error: HostError) -> HostError {
    match error {
        HostError::Unsupported(message) => HostError::Unsupported(message + NOTHING_TYPED),
        HostError::NothingTyped(message) => HostError::NothingTyped(message + NOTHING_TYPED),
        partial @ HostError::MaybePartial(_) => partial,
    }
}

/// Window classes of terminals, compared ignoring case; Ctrl+V sends these a raw `^V`, so they
/// paste with Ctrl+Shift+V.
const TERMINAL_CLASSES: [&str; 14] = [
    "kitty",
    "foot",
    "footclient",
    "alacritty",
    "org.wezfurlong.wezterm",
    "com.mitchellh.ghostty",
    "org.gnome.console",
    "org.gnome.terminal",
    "gnome-terminal-server",
    "org.kde.konsole",
    "konsole",
    "xterm",
    "org.gnome.ptyxis",
    "ptyxis",
];

/// The modifiers of the paste shortcut for `target`.
fn paste_modifiers(target: &Window) -> &'static str {
    if TERMINAL_CLASSES.contains(&target.class.to_lowercase().as_str()) {
        "CTRL SHIFT"
    } else {
        "CTRL"
    }
}

/// A failure of a key tool after the window was focused: some keys may have been pressed.
fn key_failure(error: &CommandError) -> HostError {
    match error {
        CommandError::NotInstalled(_) => nothing_typed(tool(error)),
        CommandError::TimedOut(_) | CommandError::Failed { .. } => {
            HostError::MaybePartial(MAYBE_PASTED.to_owned())
        }
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

    /// The keymap the main keyboard types with: the `kb_file` when one is set, else its layout
    /// names; `None` when Hyprland does not say or the keymap cannot be rebuilt, so a wrong key is
    /// never guessed from names that do not describe it.
    fn keymap(&self) -> Option<xkb::Keymap> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let option = self
            .run
            .run("hyprctl", &["-j", "getoption", "input:kb_file"], None)
            .ok()?;
        if let Some(path) = serde_json::from_str::<StringOption>(&option).ok()?.value() {
            return keymap_file(&context, &path);
        }
        let mut keyboards = serde_json::from_str::<Devices>(&self.json("devices").ok()?)
            .ok()?
            .keyboards;
        keyboards.sort_by_key(|keyboard| !keyboard.main);
        keyboards.first()?.keymap(&context)
    }

    /// How `sendshortcut` names the V key. Hyprland resolves a key name once per name, against the
    /// keyboard that last sent a key, so a layout change leaves the old key and a virtual keyboard
    /// such as wtype's fails it with "key not found"; a keycode from the keymap now in use does not.
    /// With no keymap to read, the name is left to Hyprland: the worst it does is refuse the key.
    fn v_key(&self) -> String {
        self.keymap()
            .as_ref()
            .and_then(v_keycode)
            .map_or_else(|| "v".to_owned(), |code| format!("code:{code}"))
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

    // Hyprland routes no pointer input to a `no_focus` window, and the overlay must stay one.
    fn overlay_takes_clicks(&self) -> bool {
        false
    }

    fn copies_only(&self) -> bool {
        false
    }

    fn capture_target(&self) -> Result<Window, HostError> {
        self.active()
    }

    fn deliver(
        &self,
        target: &Window,
        text: &PasteText,
        enter: bool,
    ) -> Result<Delivered, HostError> {
        let paste = !text.as_str().is_empty();
        // Copied first, so the text is on the clipboard whatever happens to the window.
        if paste {
            wl_copy(&self.run, text.as_str()).map_err(nothing_typed)?;
        }
        self.focus(target).map_err(nothing_typed)?;
        if paste {
            // The compositor sends the keys with its own keymap; GTK 3 ignores a Ctrl shortcut
            // that wtype sends with its own.
            let shortcut = format!(
                "{},{},address:{}",
                paste_modifiers(target),
                self.v_key(),
                target.address
            );
            let reply = self
                .run
                .run("hyprctl", &["dispatch", "sendshortcut", &shortcut], None)
                .map_err(|error| key_failure(&error))?;
            if reply.trim() != "ok" {
                return Err(HostError::NothingTyped(format!(
                    "The desktop would not press the paste keys: {}. Nothing was pasted.",
                    reply.trim().trim_end_matches('.')
                )));
            }
        }
        if enter {
            // Terminal agents read an Enter that arrives right after a paste as a new line.
            let pause: &[&str] = if paste { &["-s", "250"] } else { &[] };
            let keys: Vec<&str> = pause.iter().copied().chain(["-k", "Return"]).collect();
            self.run.run("wtype", &keys, None).map_err(|error| {
                if paste {
                    // The paste went through, so the text is in the window even if Enter is not.
                    HostError::MaybePartial(ENTER_FAILED.to_owned())
                } else {
                    key_failure(&error)
                }
            })?;
        }
        Ok(Delivered::InWindow)
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        wl_copy(&self.run, text)
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
        let top = monitors
            .iter()
            .find(|monitor| monitor.focused)
            .or(monitors.first())
            .map_or(0, |monitor| monitor.reserved[1].max(0));
        // Named rules can be restored after a config reload without accumulating one per capture.
        let rules = [
            ("match:title", format!("^({OVERLAY_TITLE})$")),
            ("float", "on".to_owned()),
            ("pin", "on".to_owned()),
            ("no_initial_focus", "on".to_owned()),
            ("no_focus", "on".to_owned()),
            ("size", format!("{width} {height}")),
            ("move", format!("((monitor_w*0.5)-({width}*0.5)) {top}")),
            ("border_size", "0".to_owned()),
            ("no_shadow", "on".to_owned()),
            ("no_anim", "on".to_owned()),
            ("no_blur", "on".to_owned()),
        ];
        let batch = rules
            .iter()
            .map(|(name, value)| format!("keyword windowrule[thalvi-overlay]:{name} {value}"))
            .collect::<Vec<_>>()
            .join("; ");
        let response = self
            .run
            .run("hyprctl", &["--batch", &batch], None)
            .map_err(|error| tool(&error))?;
        let replies: Vec<_> = response
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if replies.len() != rules.len() || replies.iter().any(|reply| *reply != "ok") {
            return Err(HostError::NothingTyped(
                "The desktop could not prepare the status overlay.".to_owned(),
            ));
        }
        Ok(f64::from(i32::try_from(top).unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_hold_key_is_no_failure_on_hyprland_where_binds_report_it() {
        let source =
            Hyprland::watch_hold_keys(&[(HoldKind::Dictate, HoldKey::default())], |_, _| {});
        assert!(
            matches!(source, Ok(HoldSource::Binds(reason)) if reason.contains("hyprland.conf"))
        );
    }

    use std::sync::Mutex;

    use super::super::command::fakes::FakeRunner;
    use super::*;

    fn host(
        reply: impl Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync + 'static,
    ) -> Hyprland<FakeRunner> {
        Hyprland {
            run: FakeRunner::new(reply),
        }
    }

    fn commands(host: &Hyprland<FakeRunner>) -> Vec<String> {
        host.run.commands()
    }

    fn row(address: &str, pid: i64, class: &str, history: i64) -> String {
        format!(
            r#"{{"address": "{address}", "mapped": true, "title": "TITLE", "pid": {pid},
                "class": "{class}", "focusHistoryID": {history}}}"#
        )
    }

    /// `hyprctl -j getoption input:kb_file` when nobody set it.
    const UNSET: &str = r#"{"option": "input:kb_file", "str": "[[EMPTY]]", "set": false }"#;

    fn kb_file(path: &str) -> String {
        format!(r#"{{"option": "input:kb_file", "str": "{path}", "set": true }}"#)
    }

    /// A `kb_file` of the given layout, written where the host can read it; returns its path.
    fn keymap_file(name: &str, layout: &str, variant: &str) -> String {
        let keymap = xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            "",
            "",
            layout,
            variant,
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let path =
            std::env::temp_dir().join(format!("thalvi-hyprland-{}-{name}.xkb", std::process::id()));
        std::fs::write(&path, keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// `hyprctl -j devices` for keyboards given as (layout, variant, active index, main).
    fn keyboards(keyboards: &[(&str, &str, u32, bool)]) -> String {
        let rows: Vec<String> = keyboards
            .iter()
            .map(|(layout, variant, index, main)| {
                format!(
                    r#"{{"rules": "", "model": "", "layout": "{layout}", "variant": "{variant}",
                        "options": "", "active_layout_index": {index}, "main": {main}}}"#
                )
            })
            .collect();
        format!(r#"{{"keyboards": [{}], "mice": []}}"#, rows.join(","))
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
                ["hyprctl", "-j", "getoption", "input:kb_file"] => Ok(UNSET.to_owned()),
                ["hyprctl", "-j", "devices"] => Ok(keyboards(&[("us", "", 0, true)])),
                ["hyprctl", "dispatch", "sendshortcut", _] => Ok("ok\n".to_owned()),
                _ => Ok(String::new()),
            }
        })
    }

    fn paste(text: &str) -> PasteText {
        PasteText::new(text).unwrap()
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
    fn delivery_copies_then_refocuses_the_original_window_and_presses_the_paste_shortcut() {
        let text = "--help $(not-a-command) `nor-this` café 👋\nsecond line";
        let enter = "wtype -s 250 -k Return";
        for (class, submit, shortcut) in [
            ("browser", false, "CTRL"),
            ("browser", true, "CTRL"),
            ("kitty", false, "CTRL SHIFT"),
            ("Alacritty", true, "CTRL SHIFT"),
            ("org.gnome.Console", false, "CTRL SHIFT"),
        ] {
            let host = desktop(
                vec![row("0x1", 1, "other", 0), row("0x2", 2, class, 1)],
                "0x1",
                "0x2",
            );
            assert_eq!(
                host.deliver(&window("0x2", 2, class), &paste(text), submit),
                Ok(Delivered::InWindow)
            );
            let mut expected = vec![
                format!("wl-copy --type text/plain;charset=utf-8 <{text}>"),
                "hyprctl -j clients".to_owned(),
                "hyprctl dispatch focuswindow address:0x2".to_owned(),
                "hyprctl -j activewindow".to_owned(),
                "hyprctl -j getoption input:kb_file".to_owned(),
                "hyprctl -j devices".to_owned(),
                format!("hyprctl dispatch sendshortcut {shortcut},code:55,address:0x2"),
            ];
            expected.extend(submit.then(|| enter.to_owned()));
            assert_eq!(commands(&host), expected, "{class} submit={submit}");
        }
    }

    type Reply = Result<String, CommandError>;

    #[test]
    fn the_paste_key_is_the_one_that_types_v_in_the_layout_now_in_use() {
        let dvorak_file = keymap_file("dvorak", "us", "dvorak");
        let garbage =
            std::env::temp_dir().join(format!("thalvi-hyprland-{}-bad.xkb", std::process::id()));
        std::fs::write(&garbage, "not a keymap").unwrap();
        let garbage = garbage.to_string_lossy().into_owned();
        let us = || Ok(keyboards(&[("us", "", 0, true)]));
        let unset = || Ok(UNSET.to_owned());
        let failing = || Err(CommandError::TimedOut("hyprctl".to_owned()));
        let cases: Vec<(&str, Reply, Reply, &str)> = vec![
            ("us", unset(), us(), "code:55"),
            (
                "de",
                unset(),
                Ok(keyboards(&[("de", "", 0, true)])),
                "code:55",
            ),
            (
                "dvorak, where V sits elsewhere",
                unset(),
                Ok(keyboards(&[("us", "dvorak", 0, true)])),
                "code:60",
            ),
            (
                "the first layout, though another is active",
                unset(),
                Ok(keyboards(&[("us,de", "dvorak,", 1, true)])),
                "code:60",
            ),
            (
                "a first layout without a V falls back to one that has it",
                unset(),
                Ok(keyboards(&[("ru,us", "", 0, true)])),
                "code:55",
            ),
            (
                "the main keyboard, not a virtual one",
                unset(),
                Ok(keyboards(&[
                    ("de", "", 0, false),
                    ("us", "dvorak", 0, true),
                ])),
                "code:60",
            ),
            (
                "a kb_file, which the layout names do not describe",
                Ok(kb_file(&dvorak_file)),
                us(),
                "code:60",
            ),
            (
                "an option set to nothing is no kb_file",
                Ok(kb_file("")),
                us(),
                "code:55",
            ),
            (
                "a kb_file that is missing",
                Ok(kb_file("/no/such/keymap.xkb")),
                us(),
                "v",
            ),
            (
                "a kb_file that is no keymap",
                Ok(kb_file(&garbage)),
                us(),
                "v",
            ),
            (
                "a kb_file relative to the config",
                Ok(kb_file("keymap.xkb")),
                us(),
                "v",
            ),
            ("an option Hyprland does not answer", failing(), us(), "v"),
            ("an unreadable option", Ok("nope".to_owned()), us(), "v"),
            (
                "an unknown layout is left to Hyprland",
                unset(),
                Ok(keyboards(&[("no-such-layout", "", 0, true)])),
                "v",
            ),
            ("no keyboards", unset(), Ok(keyboards(&[])), "v"),
            ("not json", unset(), Ok("nope".to_owned()), "v"),
            ("devices failing", unset(), failing(), "v"),
        ];
        for (name, option, devices, expected) in cases {
            let host = host(move |argv, _| {
                if argv.iter().any(|arg| arg == "getoption") {
                    option.clone()
                } else {
                    devices.clone()
                }
            });
            assert_eq!(host.v_key(), expected, "{name}");
        }
        std::fs::remove_file(dvorak_file).unwrap();
        std::fs::remove_file(garbage).unwrap();
    }

    #[test]
    fn submitting_nothing_presses_only_enter_and_leaves_the_clipboard_alone() {
        let host = desktop(vec![row("0x1", 1, "kitty", 0)], "0x1", "0x1");
        assert_eq!(
            host.deliver(&window("0x1", 1, "kitty"), &paste(""), true),
            Ok(Delivered::InWindow)
        );
        assert_eq!(
            commands(&host).last().map(String::as_str),
            Some("wtype -k Return")
        );
        assert!(
            commands(&host)
                .iter()
                .all(|command| !command.starts_with("wl-copy") && !command.contains("sendshortcut"))
        );
    }

    #[test]
    fn nothing_is_pasted_when_the_target_is_gone_reused_or_not_focusable() {
        for (name, windows, lands_on, message) in [
            (
                "closed",
                vec![row("0x1", 1, "browser", 0)],
                "0x1",
                "The original window closed. Nothing was pasted.",
            ),
            (
                "address reused by another process",
                vec![row("0x2", 99, "terminal", 0)],
                "0x2",
                "The original window closed. Nothing was pasted.",
            ),
            (
                "focus did not move",
                vec![row("0x1", 1, "browser", 0), row("0x2", 2, "terminal", 1)],
                "0x1",
                "The target could not be focused. Nothing was pasted.",
            ),
        ] {
            let host = desktop(windows, "0x1", lands_on);
            assert_eq!(
                host.deliver(&window("0x2", 2, "terminal"), &paste("hi"), true),
                Err(HostError::NothingTyped(message.to_owned())),
                "{name}"
            );
            // The text is kept on the clipboard, and no key is pressed.
            assert!(
                commands(&host)
                    .iter()
                    .all(|command| !command.starts_with("wtype")
                        && !command.contains("sendshortcut")),
                "{name}"
            );
            assert_eq!(
                commands(&host)[0],
                "wl-copy --type text/plain;charset=utf-8 <hi>",
                "{name}"
            );
        }
    }

    #[test]
    fn a_key_tool_that_fails_after_focus_may_have_pasted_part_of_the_text() {
        let partial = HostError::MaybePartial(
            "Thalvi may or may not have pasted the text. Check the window before trying again."
                .to_owned(),
        );
        let enter_failed = HostError::MaybePartial(
            "The text was pasted, but Thalvi may not have pressed Enter. Check the window before trying again."
                .to_owned(),
        );
        let failed = |program: &str| CommandError::Failed {
            program: program.to_owned(),
            reason: None,
        };
        for (name, program, reply, submit, expected) in [
            (
                "shortcut tool fails",
                "hyprctl",
                Err(failed("hyprctl")),
                false,
                partial.clone(),
            ),
            (
                "shortcut tool stuck",
                "hyprctl",
                Err(CommandError::TimedOut("hyprctl".to_owned())),
                false,
                partial.clone(),
            ),
            (
                "shortcut refused",
                "hyprctl",
                Ok("window not found".to_owned()),
                false,
                HostError::NothingTyped(
                    "The desktop would not press the paste keys: window not found. Nothing was pasted."
                        .to_owned(),
                ),
            ),
            (
                "enter fails after the paste",
                "wtype",
                Err(failed("wtype")),
                true,
                enter_failed.clone(),
            ),
            (
                "enter tool missing after the paste",
                "wtype",
                Err(CommandError::NotInstalled("wtype".to_owned())),
                true,
                enter_failed,
            ),
        ] {
            let host = host(move |argv, _| match argv {
                [tool, ..] if tool == program && (tool == "wtype" || argv[2] == "sendshortcut") => {
                    reply.clone()
                }
                [_, _, query] if query == "clients" => Ok(format!("[{}]", row("0x1", 1, "a", 0))),
                [_, _, query] if query == "activewindow" => Ok(row("0x1", 1, "a", 0)),
                [_, _, _, _] if argv[2] == "sendshortcut" => Ok("ok".to_owned()),
                _ => Ok(String::new()),
            });
            assert_eq!(
                host.deliver(&window("0x1", 1, "a"), &paste("hi"), submit),
                Err(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn a_missing_enter_tool_is_unsupported_when_nothing_was_pasted() {
        let host = host(|argv, _| match argv[0].as_str() {
            "wtype" => Err(CommandError::NotInstalled("wtype".to_owned())),
            _ if argv.last().is_some_and(|arg| arg == "clients") => {
                Ok(format!("[{}]", row("0x1", 1, "a", 0)))
            }
            _ if argv.last().is_some_and(|arg| arg == "activewindow") => Ok(row("0x1", 1, "a", 0)),
            _ => Ok("ok".to_owned()),
        });
        assert_eq!(
            host.deliver(&window("0x1", 1, "a"), &paste(""), true),
            Err(HostError::Unsupported(
                "Desktop tool wtype is not installed. Nothing was pasted.".to_owned()
            ))
        );
    }

    #[test]
    fn nothing_is_pressed_when_the_text_cannot_be_put_on_the_clipboard() {
        let host = host(|argv, _| match argv[0].as_str() {
            "wl-copy" => Err(CommandError::NotInstalled("wl-copy".to_owned())),
            _ => Ok(String::new()),
        });
        assert_eq!(
            host.deliver(&window("0x1", 1, "a"), &paste("hi"), true),
            Err(HostError::Unsupported(
                "Desktop tool wl-copy is not installed. Nothing was pasted.".to_owned()
            ))
        );
        assert_eq!(
            commands(&host),
            ["wl-copy --type text/plain;charset=utf-8 <hi>"]
        );
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
                    "ok\n\n".repeat(11)
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
                        "hyprctl --batch \
                         keyword windowrule[thalvi-overlay]:match:title ^(Thalvi overlay)$; \
                         keyword windowrule[thalvi-overlay]:float on; \
                         keyword windowrule[thalvi-overlay]:pin on; \
                         keyword windowrule[thalvi-overlay]:no_initial_focus on; \
                         keyword windowrule[thalvi-overlay]:no_focus on; \
                         keyword windowrule[thalvi-overlay]:size 460 200; \
                         keyword windowrule[thalvi-overlay]:move ((monitor_w*0.5)-(460*0.5)) {top}; \
                         keyword windowrule[thalvi-overlay]:border_size 0; \
                         keyword windowrule[thalvi-overlay]:no_shadow on; \
                         keyword windowrule[thalvi-overlay]:no_anim on; \
                         keyword windowrule[thalvi-overlay]:no_blur on"
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

        for reply in [
            String::new(),
            "ok\n".repeat(10),
            format!("{}invalid field\n", "ok\n".repeat(10)),
        ] {
            let rejected = host(move |argv, _| {
                Ok(if argv.iter().any(|arg| arg == "monitors") {
                    "[]".to_owned()
                } else {
                    reply.clone()
                })
            });
            assert_eq!(
                rejected.prepare_overlay(460.0, 200.0),
                Err(HostError::NothingTyped(
                    "The desktop could not prepare the status overlay.".to_owned()
                ))
            );
        }
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
