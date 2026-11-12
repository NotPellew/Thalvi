//! Windows host: window identity, the clipboard and `SendInput` key presses, over a small Win32 seam.

mod pipe;
mod win32;

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{
    App, Delivered, HoldSource, Host, HostError, MAYBE_PASTED, Modifier, PasteText, RequestServer,
    WebUrl,
};
use crate::hold::{HoldKey, HoldKind};
pub use win32::Real;

/// A window handle as a plain number, so targets stay `Send`.
pub type Hwnd = isize;

/// Which process owns a window. A reused handle or process ID never matches the old one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pid: u32,
    /// Process creation time as a `FILETIME`.
    started: u64,
    executable: String,
}

/// What Windows says about a window's process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    identity: Identity,
    /// The mandatory integrity level (RID); `None` when Windows refused to say.
    integrity: Option<u32>,
}

/// One keyboard event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// A virtual-key code.
    key: u16,
    release: bool,
}

/// The Win32 calls the host rules depend on. The real and the test implementation are the only two.
pub trait Win32: Send + Sync {
    fn foreground(&self) -> Option<Hwnd>;
    /// `None` when the window no longer exists or its process cannot be read.
    fn process(&self, window: Hwnd) -> Option<Process>;
    /// Visible top-level windows, most recently active first.
    fn top_level_windows(&self) -> Vec<Hwnd>;
    /// Asks Windows once to bring `window` to the front; Windows may refuse.
    fn focus(&self, window: Hwnd);
    fn own_integrity(&self) -> Option<u32>;
    /// How many events Windows accepted.
    fn send_input(&self, events: &[KeyEvent]) -> usize;
    fn set_clipboard(&self, text: &str) -> bool;
    fn open_url(&self, url: &str) -> bool;
    /// The top of the primary monitor's work area, below a taskbar docked there, in logical pixels.
    fn work_area_top(&self) -> Option<f64>;
}

/// A window as bound at wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    window: Hwnd,
    identity: Identity,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.identity.executable)
    }
}

/// The Windows desktop.
pub struct Windows<A = Real> {
    api: A,
    own_integrity: u32,
}

impl Windows {
    /// # Errors
    /// Why Thalvi cannot drive this desktop.
    pub fn detect() -> Result<Self, HostError> {
        Self::new(Real)
    }

    /// Answers each request on a named pipe only the current user may open, and never over the
    /// network, with `handler(line)` until the guard is dropped.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when the pipe cannot be made or another app serves it.
    pub fn serve_requests(
        handler: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<RequestServer, HostError> {
        pipe::serve(Arc::new(handler))
    }

    /// The keys a config without hold keys gets on this desktop: the hold key, then the write key.
    #[must_use]
    pub fn default_hold_keys() -> (HoldKey, HoldKey) {
        (HoldKey::alt_z(), HoldKey::alt_x())
    }

    /// Refuses the keys this desktop can never bind, without binding anything; Windows takes
    /// every key a hold key can be, and refuses one only when the hook is installed.
    ///
    /// # Errors
    /// Never.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the same signature as the other hosts, which refuse keys they can't bind"
    )]
    pub fn check_hold_keys(_keys: &[(HoldKind, HoldKey)]) -> Result<(), HostError> {
        Ok(())
    }

    /// Reports each press and release of `keys` to `on_change` until the guard is dropped, and
    /// keeps them from the focused app. `on_change` runs inside the keyboard hook: it must not block.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when Windows refuses the keyboard hook.
    pub fn watch_hold_keys(
        keys: &[(HoldKind, HoldKey)],
        on_change: impl Fn(HoldKind, bool) + Send + 'static,
        _on_problem: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<HoldSource, HostError> {
        win32::watch_hold_keys(keys.to_vec(), Box::new(on_change)).map(HoldSource::Watching)
    }

    /// The running app's reply to `request`, waiting up to `timeout`; `None` when no app answers.
    ///
    /// # Errors
    /// [`HostError::Unsupported`] when the answer could not be asked or read.
    pub fn send_request(request: &str, timeout: Duration) -> Result<Option<String>, HostError> {
        pipe::ask(request, timeout)
    }

    /// Shows `message` in an error box titled `title` and waits until it is closed. Needs no
    /// detected desktop, so a failure to detect one can be shown.
    pub fn show_error(title: &str, message: &str) {
        win32::message_box(title, message);
    }

    /// Shows or hides the overlay window `window` without activating it, which Tauri's `show`
    /// would.
    pub fn set_overlay_visible(window: Hwnd, visible: bool) {
        win32::set_visible_unfocused(window, visible);
    }
}

impl<A: Win32> Windows<A> {
    fn new(api: A) -> Result<Self, HostError> {
        let own_integrity = api.own_integrity().ok_or_else(|| {
            HostError::Unsupported(
                "Thalvi could not read its own permission level from Windows.".to_owned(),
            )
        })?;
        Ok(Self { api, own_integrity })
    }

    fn live_process(&self, target: &Target) -> Result<Process, HostError> {
        self.api
            .process(target.window)
            .filter(|process| process.identity == target.identity)
            .ok_or_else(|| nothing_typed("The window closed. Nothing was pasted."))
    }

    fn check_input_reaches(&self, process: &Process) -> Result<(), HostError> {
        match process.integrity {
            Some(level) if level <= self.own_integrity => Ok(()),
            Some(_) => Err(nothing_typed(
                "That window runs with more permissions than Thalvi, such as an administrator \
                 window, and Windows drops keys sent to it. Nothing was pasted.",
            )),
            None => Err(nothing_typed(
                "Thalvi could not check whether that window runs as an administrator. Nothing \
                 was pasted.",
            )),
        }
    }

    /// One focus attempt when `window` is not in front, then a check that it is.
    fn in_front(&self, window: Hwnd) -> bool {
        if self.api.foreground() == Some(window) {
            return true;
        }
        self.api.focus(window);
        self.api.foreground() == Some(window)
    }

    fn send(&self, events: &[KeyEvent], partial: &str) -> Result<(), HostError> {
        if events.is_empty() {
            return Ok(());
        }
        match self.api.send_input(events) {
            0 => Err(HostError::NothingTyped(
                "Windows refused the input. Nothing was pasted.".to_owned(),
            )),
            accepted if accepted == events.len() => Ok(()),
            accepted => {
                // A key left down would stay pressed for every program, so let go of what was pressed.
                let stuck = releases(&events[..accepted]);
                if !stuck.is_empty() && self.api.send_input(&stuck) < stuck.len() {
                    return Err(HostError::MaybePartial(format!(
                        "{partial} A key Thalvi pressed may still be held down; tap it to let go."
                    )));
                }
                Err(HostError::MaybePartial(partial.to_owned()))
            }
        }
    }
}

impl<A: Win32> Host for Windows<A> {
    type Target = Target;

    fn overlay_takes_clicks(&self) -> bool {
        true
    }

    fn copies_only(&self) -> bool {
        false
    }

    fn capture_target(&self) -> Result<Target, HostError> {
        let window = self.api.foreground().ok_or_else(|| {
            nothing_typed("No window is in front, so Thalvi has nowhere to paste.")
        })?;
        let process = self.api.process(window).ok_or_else(|| {
            nothing_typed("Thalvi could not tell which program the window in front belongs to.")
        })?;
        Ok(Target {
            window,
            identity: process.identity,
        })
    }

    fn deliver(
        &self,
        target: &Target,
        text: &PasteText,
        enter: bool,
    ) -> Result<Delivered, HostError> {
        // Copied first, so the text is on the clipboard whatever happens to the window.
        let paste = !text.as_str().is_empty();
        if paste {
            self.copy(text.as_str())?;
        }
        let process = self.live_process(target)?;
        self.check_input_reaches(&process)?;
        if !self.in_front(target.window) {
            return Err(nothing_typed("The window changed. Nothing was pasted."));
        }
        self.send(&paste_events(paste, enter), MAYBE_PASTED)
            .map(|()| Delivered::InWindow)
    }

    fn copy(&self, text: &str) -> Result<(), HostError> {
        // Windows text uses CRLF line breaks, and some programs show a lone LF as one line.
        let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
        if self.api.set_clipboard(&text) {
            Ok(())
        } else {
            Err(nothing_typed(
                "Windows would not let Thalvi use the clipboard.",
            ))
        }
    }

    fn focus_app(&self, app: &App) -> Result<Target, HostError> {
        if app.executables.is_empty() {
            return Err(HostError::Unsupported(
                "This app has no Windows program name set, so Thalvi cannot switch to it."
                    .to_owned(),
            ));
        }
        let shortcut = app
            .shortcut
            .as_ref()
            .map(|shortcut| shortcut_events(&shortcut.modifiers, &shortcut.key))
            .transpose()?;
        let (window, process) = self
            .api
            .top_level_windows()
            .into_iter()
            .find_map(|window| {
                let process = self.api.process(window)?;
                app.executables
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&process.identity.executable))
                    .then_some((window, process))
            })
            .ok_or_else(|| {
                nothing_typed("That app is not open, so Thalvi could not switch to it.")
            })?;
        if shortcut.is_some() {
            self.check_input_reaches(&process)?;
        }
        if !self.in_front(window) {
            return Err(nothing_typed(
                "Windows did not let Thalvi switch to that app. Nothing was pressed.",
            ));
        }
        if let Some(events) = shortcut {
            self.send(
                &events,
                "Pressing the shortcut stopped partway. Some keys may have been pressed.",
            )?;
        }
        Ok(Target {
            window,
            identity: process.identity,
        })
    }

    fn open_url(&self, url: &WebUrl) -> Result<(), HostError> {
        if self.api.open_url(url.as_str()) {
            Ok(())
        } else {
            Err(nothing_typed("Windows could not open the web address."))
        }
    }

    fn open_folder(&self, path: &Path) -> Result<(), HostError> {
        // Opening a folder with the shell's "open" verb shows it in Explorer.
        if self.api.open_url(&path.to_string_lossy()) {
            Ok(())
        } else {
            Err(nothing_typed("Windows could not open the folder."))
        }
    }

    fn prepare_overlay(&self, _width: f64, _height: f64) -> Result<f64, HostError> {
        // The app shows the window with `set_overlay_visible`, because tao's `show` activates it
        // despite WS_EX_NOACTIVATE; it sizes and positions it below the taskbar with the top
        // returned here.
        self.api.work_area_top().ok_or_else(|| {
            HostError::Unsupported("Windows did not say where the taskbar is.".to_owned())
        })
    }
}

fn nothing_typed(notice: &str) -> HostError {
    HostError::NothingTyped(notice.to_owned())
}

const VK_RETURN: u16 = 0x0D;
const VK_CONTROL: u16 = 0x11;
const VK_V: u16 = 0x56;

fn tap(key: u16) -> [KeyEvent; 2] {
    [false, true].map(|release| KeyEvent { key, release })
}

/// Ctrl+V when `paste`, then Enter when `enter`.
fn paste_events(paste: bool, enter: bool) -> Vec<KeyEvent> {
    let ctrl = |release| KeyEvent {
        key: VK_CONTROL,
        release,
    };
    let mut events = Vec::new();
    if paste {
        events.push(ctrl(false));
        events.extend(tap(VK_V));
        events.push(ctrl(true));
    }
    if enter {
        events.extend(tap(VK_RETURN));
    }
    events
}

/// Key-ups for every key that `sent` pressed and did not release, the last pressed first.
fn releases(sent: &[KeyEvent]) -> Vec<KeyEvent> {
    let mut held: Vec<u16> = Vec::new();
    for event in sent {
        if event.release {
            held.retain(|key| *key != event.key);
        } else {
            held.push(event.key);
        }
    }
    held.into_iter()
        .rev()
        .map(|key| KeyEvent { key, release: true })
        .collect()
}

fn shortcut_events(modifiers: &[Modifier], key: &str) -> Result<Vec<KeyEvent>, HostError> {
    let code = virtual_key(key).ok_or_else(|| {
        HostError::Unsupported(format!(
            "Windows has no key called {key}, so the shortcut was not pressed."
        ))
    })?;
    let held = modifiers.iter().map(|modifier| match modifier {
        Modifier::Shift => 0x10,
        Modifier::Ctrl => VK_CONTROL,
        Modifier::Alt => 0x12,
        Modifier::Logo => 0x5B,
    });
    let down = |key| KeyEvent {
        key,
        release: false,
    };
    let up = |key| KeyEvent { key, release: true };
    Ok(held
        .clone()
        .map(down)
        .chain(tap(code))
        .chain(held.rev().map(up))
        .collect())
}

/// Letters, digits, function keys and the common named keys; `None` for any other name.
fn virtual_key(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    match lower.as_bytes() {
        [letter @ b'a'..=b'z'] => Some(u16::from(letter.to_ascii_uppercase())),
        [digit @ b'0'..=b'9'] => Some(u16::from(*digit)),
        _ => match lower.as_str() {
            "return" | "enter" => Some(VK_RETURN),
            "tab" => Some(0x09),
            "escape" => Some(0x1B),
            "space" => Some(0x20),
            "backspace" => Some(0x08),
            function => {
                let number: u16 = function.strip_prefix('f')?.parse().ok()?;
                (1..=24).contains(&number).then_some(0x6F + number)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Shortcut;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const MEDIUM: u32 = 0x2000;
    const HIGH: u32 = 0x3000;
    const NOTEPAD: Hwnd = 10;
    const EDITOR: Hwnd = 20;
    const OTHER: Hwnd = 30;

    fn identity(pid: u32, executable: &str) -> Identity {
        Identity {
            pid,
            started: 1_000 + u64::from(pid),
            executable: executable.to_owned(),
        }
    }

    fn process(pid: u32, executable: &str, integrity: Option<u32>) -> Process {
        Process {
            identity: identity(pid, executable),
            integrity,
        }
    }

    #[derive(Default)]
    struct State {
        foreground: Option<Hwnd>,
        processes: HashMap<Hwnd, Process>,
        stack: Vec<Hwnd>,
        /// Window that `focus` brings to the front; `None` when Windows refuses.
        focus_grants: Option<Hwnd>,
        accepts: Option<usize>,
        /// How many events of a batch after the first Windows accepts; `accepts` when unset.
        later_accepts: Option<usize>,
        focused: Vec<Hwnd>,
        sent: Vec<Vec<KeyEvent>>,
        clipboard: Vec<String>,
        clipboard_open: bool,
        opened: Vec<String>,
        browser_open: bool,
        work_area_top: Option<f64>,
    }

    struct Fake(Mutex<State>);

    impl Fake {
        fn desktop() -> Self {
            Self(Mutex::new(State {
                foreground: Some(NOTEPAD),
                processes: HashMap::from([
                    (NOTEPAD, process(1, "notepad.exe", Some(MEDIUM))),
                    (EDITOR, process(2, "T3 Code.exe", Some(MEDIUM))),
                    (OTHER, process(3, "T3 Code.exe", Some(MEDIUM))),
                ]),
                stack: vec![NOTEPAD, OTHER, EDITOR],
                clipboard_open: true,
                browser_open: true,
                work_area_top: Some(0.0),
                ..State::default()
            }))
        }

        fn with(&self, change: impl FnOnce(&mut State)) {
            change(&mut self.0.lock().unwrap());
        }

        fn read<T>(&self, read: impl FnOnce(&State) -> T) -> T {
            read(&self.0.lock().unwrap())
        }
    }

    impl Win32 for Fake {
        fn foreground(&self) -> Option<Hwnd> {
            self.read(|state| state.foreground)
        }
        fn process(&self, window: Hwnd) -> Option<Process> {
            self.read(|state| state.processes.get(&window).cloned())
        }
        fn top_level_windows(&self) -> Vec<Hwnd> {
            self.read(|state| state.stack.clone())
        }
        fn focus(&self, window: Hwnd) {
            self.with(|state| {
                state.focused.push(window);
                if state.focus_grants == Some(window) {
                    state.foreground = Some(window);
                }
            });
        }
        fn own_integrity(&self) -> Option<u32> {
            Some(MEDIUM)
        }
        fn send_input(&self, events: &[KeyEvent]) -> usize {
            let mut state = self.0.lock().unwrap();
            let limit = if state.sent.is_empty() {
                state.accepts
            } else {
                state.later_accepts.or(state.accepts)
            };
            state.sent.push(events.to_vec());
            limit.map_or(events.len(), |n| n.min(events.len()))
        }
        fn set_clipboard(&self, text: &str) -> bool {
            let mut state = self.0.lock().unwrap();
            if state.clipboard_open {
                state.clipboard.push(text.to_owned());
            }
            state.clipboard_open
        }
        fn open_url(&self, url: &str) -> bool {
            let mut state = self.0.lock().unwrap();
            if state.browser_open {
                state.opened.push(url.to_owned());
            }
            state.browser_open
        }
        fn work_area_top(&self) -> Option<f64> {
            self.read(|state| state.work_area_top)
        }
    }

    fn windows(fake: Fake) -> Windows<Fake> {
        Windows::new(fake).unwrap()
    }

    fn text(text: &str) -> PasteText {
        PasteText::new(text).unwrap()
    }

    fn ctrl_v() -> Vec<KeyEvent> {
        [
            vec![KeyEvent {
                key: VK_CONTROL,
                release: false,
            }],
            virtual_tap(VK_V).to_vec(),
            vec![KeyEvent {
                key: VK_CONTROL,
                release: true,
            }],
        ]
        .concat()
    }

    fn virtual_tap(code: u16) -> [KeyEvent; 2] {
        tap(code)
    }

    fn app(executables: &[&str], shortcut: Option<Shortcut>) -> App {
        App {
            classes: Vec::new(),
            executables: executables.iter().map(|name| (*name).to_owned()).collect(),
            shortcut,
        }
    }

    fn nothing_typed_text(error: HostError) -> String {
        match error {
            HostError::NothingTyped(notice) => notice,
            other => panic!("expected NothingTyped, got {other:?}"),
        }
    }

    #[test]
    fn capture_binds_the_foreground_window_or_says_why_not() {
        let desktop = windows(Fake::desktop());
        let target = desktop.capture_target().unwrap();
        assert_eq!(target.window, NOTEPAD);
        assert_eq!(target.identity, identity(1, "notepad.exe"));

        desktop.api.with(|state| state.foreground = None);
        assert_eq!(
            nothing_typed_text(desktop.capture_target().unwrap_err()),
            "No window is in front, so Thalvi has nowhere to paste."
        );

        desktop.api.with(|state| state.foreground = Some(999));
        assert_eq!(
            nothing_typed_text(desktop.capture_target().unwrap_err()),
            "Thalvi could not tell which program the window in front belongs to."
        );
    }

    #[test]
    fn unreadable_own_permissions_make_the_desktop_unsupported() {
        struct Blind(Fake);
        impl Win32 for Blind {
            fn foreground(&self) -> Option<Hwnd> {
                self.0.foreground()
            }
            fn process(&self, window: Hwnd) -> Option<Process> {
                self.0.process(window)
            }
            fn top_level_windows(&self) -> Vec<Hwnd> {
                self.0.top_level_windows()
            }
            fn focus(&self, window: Hwnd) {
                self.0.focus(window);
            }
            fn own_integrity(&self) -> Option<u32> {
                None
            }
            fn send_input(&self, events: &[KeyEvent]) -> usize {
                self.0.send_input(events)
            }
            fn set_clipboard(&self, text: &str) -> bool {
                self.0.set_clipboard(text)
            }
            fn open_url(&self, url: &str) -> bool {
                self.0.open_url(url)
            }
            fn work_area_top(&self) -> Option<f64> {
                self.0.work_area_top()
            }
        }
        assert!(matches!(
            Windows::new(Blind(Fake::desktop())),
            Err(HostError::Unsupported(_))
        ));
    }

    #[test]
    fn the_overlay_hangs_from_the_top_of_the_work_area_below_a_taskbar() {
        let desktop = windows(Fake::desktop());
        assert_eq!(desktop.prepare_overlay(460.0, 200.0), Ok(0.0));

        desktop.api.with(|state| state.work_area_top = Some(32.0));
        assert_eq!(desktop.prepare_overlay(460.0, 200.0), Ok(32.0));

        desktop.api.with(|state| state.work_area_top = None);
        assert_eq!(
            desktop.prepare_overlay(460.0, 200.0),
            Err(HostError::Unsupported(
                "Windows did not say where the taskbar is.".to_owned()
            ))
        );
    }

    #[test]
    fn pasting_into_the_bound_foreground_sets_the_clipboard_with_crlf_and_sends_one_batch() {
        let return_key = virtual_tap(VK_RETURN).to_vec();
        for (words, enter, copied, events) in [
            (
                "aé👋\nline",
                true,
                Some("aé👋\r\nline"),
                [ctrl_v(), return_key.clone()].concat(),
            ),
            ("hi", false, Some("hi"), ctrl_v()),
            ("", true, None, return_key),
            ("", false, None, Vec::new()),
        ] {
            let desktop = windows(Fake::desktop());
            let target = desktop.capture_target().unwrap();

            desktop.deliver(&target, &text(words), enter).unwrap();

            let sent = desktop.api.read(|state| state.sent.clone());
            let expected = if events.is_empty() {
                vec![]
            } else {
                vec![events]
            };
            assert_eq!(sent, expected, "{words:?} enter={enter}");
            assert_eq!(
                desktop.api.read(|state| state.clipboard.clone()),
                copied.map(str::to_owned).into_iter().collect::<Vec<_>>(),
                "{words:?}"
            );
            assert!(desktop.api.read(|state| state.focused.is_empty()));
        }
    }

    #[test]
    fn a_changed_foreground_gets_one_focus_attempt_and_pastes_only_if_it_works() {
        for (grants, pasted) in [(Some(NOTEPAD), true), (None, false)] {
            let desktop = windows(Fake::desktop());
            let target = desktop.capture_target().unwrap();
            desktop.api.with(|state| {
                state.foreground = Some(EDITOR);
                state.focus_grants = grants;
            });

            let result = desktop.deliver(&target, &text("hi"), false);

            assert_eq!(desktop.api.read(|state| state.focused.clone()), [NOTEPAD]);
            assert_eq!(
                desktop.api.read(|state| state.sent.len()),
                usize::from(pasted)
            );
            assert_eq!(desktop.api.read(|state| state.clipboard.clone()), ["hi"]);
            if pasted {
                result.unwrap();
            } else {
                assert_eq!(
                    nothing_typed_text(result.unwrap_err()),
                    "The window changed. Nothing was pasted."
                );
            }
        }
    }

    #[test]
    fn a_closed_window_or_reused_handle_or_reused_pid_never_matches() {
        let reused = [
            ("closed", None),
            (
                "handle reused by another program",
                Some(process(7, "calc.exe", Some(MEDIUM))),
            ),
            (
                "same pid started later",
                Some(Process {
                    identity: Identity {
                        started: 5_000,
                        ..identity(1, "notepad.exe")
                    },
                    integrity: Some(MEDIUM),
                }),
            ),
        ];
        for (case, now) in reused {
            let desktop = windows(Fake::desktop());
            let target = desktop.capture_target().unwrap();
            desktop.api.with(|state| {
                match now {
                    Some(process) => state.processes.insert(NOTEPAD, process),
                    None => state.processes.remove(&NOTEPAD),
                };
            });

            let notice =
                nothing_typed_text(desktop.deliver(&target, &text("hi"), true).unwrap_err());

            assert_eq!(notice, "The window closed. Nothing was pasted.", "{case}");
            assert_eq!(desktop.api.read(|state| state.clipboard.clone()), ["hi"]);
            assert!(
                desktop
                    .api
                    .read(|state| state.sent.is_empty() && state.focused.is_empty()),
                "{case}"
            );
        }
    }

    #[test]
    fn a_target_with_more_permissions_than_thalvi_is_refused_before_any_input() {
        for (integrity, allowed) in [
            (Some(MEDIUM), true),
            (Some(0x1000), true),
            (Some(HIGH), false),
            (None, false),
        ] {
            let desktop = windows(Fake::desktop());
            desktop.api.with(|state| {
                state.processes.get_mut(&NOTEPAD).unwrap().integrity = integrity;
                state.foreground = Some(EDITOR);
                state.focus_grants = Some(NOTEPAD);
            });
            let target = Target {
                window: NOTEPAD,
                identity: identity(1, "notepad.exe"),
            };

            let result = desktop.deliver(&target, &text("hi"), false);

            assert_eq!(result.is_ok(), allowed, "{integrity:?}");
            if !allowed {
                assert!(
                    nothing_typed_text(result.unwrap_err()).contains("administrator"),
                    "{integrity:?}"
                );
                assert!(
                    desktop
                        .api
                        .read(|state| state.sent.is_empty() && state.focused.is_empty())
                );
            }
        }
    }

    #[test]
    fn a_paste_that_windows_cut_short_is_reported_once_and_releases_the_keys_it_pressed() {
        let key_up = |key| KeyEvent { key, release: true };
        // The batch is Ctrl down, V down, V up, Ctrl up, Return down, Return up.
        for (accepted, released) in [
            (1, vec![key_up(VK_CONTROL)]),
            (2, vec![key_up(VK_V), key_up(VK_CONTROL)]),
            (3, vec![key_up(VK_CONTROL)]),
            (4, vec![]),
            (5, vec![key_up(VK_RETURN)]),
        ] {
            let desktop = windows(Fake::desktop());
            let target = desktop.capture_target().unwrap();
            desktop.api.with(|state| state.accepts = Some(accepted));

            let error = desktop.deliver(&target, &text("hello"), true).unwrap_err();

            assert!(
                matches!(&error, HostError::MaybePartial(notice) if notice == MAYBE_PASTED),
                "{accepted}: {error:?}"
            );
            let sent = desktop.api.read(|state| state.sent.clone());
            assert_eq!(
                sent.len(),
                1 + usize::from(!released.is_empty()),
                "{accepted}"
            );
            assert_eq!(
                sent.get(1).cloned().unwrap_or_default(),
                released,
                "{accepted}"
            );
        }

        // Windows refusing the key-ups too is told, since Ctrl would stay down.
        let desktop = windows(Fake::desktop());
        let target = desktop.capture_target().unwrap();
        desktop.api.with(|state| {
            state.accepts = Some(2);
            state.later_accepts = Some(1);
        });
        let error = desktop.deliver(&target, &text("hello"), true).unwrap_err();
        assert!(
            matches!(&error, HostError::MaybePartial(notice) if notice.starts_with(MAYBE_PASTED) && notice.contains("still be held down")),
            "{error:?}"
        );
        assert_eq!(desktop.api.read(|state| state.sent.len()), 2);

        let desktop = windows(Fake::desktop());
        let target = desktop.capture_target().unwrap();
        desktop.api.with(|state| state.accepts = Some(0));
        let error = desktop.deliver(&target, &text("hello"), true).unwrap_err();
        assert!(matches!(error, HostError::NothingTyped(_)), "{error:?}");
        assert_eq!(desktop.api.read(|state| state.sent.len()), 1);
    }

    #[test]
    fn switching_to_an_app_takes_its_most_recent_window_and_presses_the_shortcut() {
        let desktop = windows(Fake::desktop());
        desktop.api.with(|state| state.focus_grants = Some(OTHER));
        let shortcut = Shortcut {
            modifiers: vec![Modifier::Ctrl, Modifier::Shift],
            key: "k".to_owned(),
        };

        let target = desktop
            .focus_app(&app(&["t3 code.EXE"], Some(shortcut)))
            .unwrap();

        assert_eq!(desktop.api.read(|state| state.focused.clone()), [OTHER]);
        assert_eq!(target.window, OTHER);
        assert_eq!(target.identity, identity(3, "T3 Code.exe"));
        let expected: Vec<KeyEvent> = [
            tap(0x11)[0],
            tap(0x10)[0],
            virtual_tap(0x4B)[0],
            virtual_tap(0x4B)[1],
            tap(0x10)[1],
            tap(0x11)[1],
        ]
        .to_vec();
        assert_eq!(desktop.api.read(|state| state.sent.clone()), [expected]);
    }

    #[test]
    fn switching_to_an_app_that_is_already_in_front_presses_only_the_shortcut() {
        let desktop = windows(Fake::desktop());
        let shortcut = Shortcut {
            modifiers: Vec::new(),
            key: "Return".to_owned(),
        };

        desktop
            .focus_app(&app(&["notepad.exe"], Some(shortcut)))
            .unwrap();

        assert!(desktop.api.read(|state| state.focused.is_empty()));
        assert_eq!(
            desktop.api.read(|state| state.sent.clone()),
            [virtual_tap(VK_RETURN).to_vec()]
        );
    }

    #[test]
    fn switching_fails_in_plain_words_and_presses_nothing() {
        let shortcut = || {
            Some(Shortcut {
                modifiers: Vec::new(),
                key: "k".to_owned(),
            })
        };
        let cases = [
            (
                "no executables",
                app(&[], None),
                None,
                "no Windows program name",
            ),
            (
                "not running",
                app(&["slack.exe"], shortcut()),
                None,
                "not open",
            ),
            (
                "unknown key",
                app(
                    &["T3 Code.exe"],
                    Some(Shortcut {
                        modifiers: Vec::new(),
                        key: "Hyper_L".to_owned(),
                    }),
                ),
                None,
                "no key called Hyper_L",
            ),
            (
                "focus refused",
                app(&["T3 Code.exe"], shortcut()),
                None,
                "did not let Thalvi switch",
            ),
            (
                "elevated app",
                app(&["T3 Code.exe"], shortcut()),
                Some(HIGH),
                "administrator",
            ),
        ];
        for (case, app, integrity, expected) in cases {
            let desktop = windows(Fake::desktop());
            if let Some(level) = integrity {
                desktop.api.with(|state| {
                    for window in [EDITOR, OTHER] {
                        state.processes.get_mut(&window).unwrap().integrity = Some(level);
                    }
                });
            }

            let error = desktop.focus_app(&app).unwrap_err();

            let notice = match error {
                HostError::Unsupported(notice) | HostError::NothingTyped(notice) => notice,
                HostError::MaybePartial(notice) => panic!("{case}: {notice}"),
            };
            assert!(notice.contains(expected), "{case}: {notice}");
            assert!(desktop.api.read(|state| state.sent.is_empty()), "{case}");
            if case != "focus refused" {
                assert!(desktop.api.read(|state| state.focused.is_empty()), "{case}");
            }
        }
    }

    #[test]
    fn a_shortcut_cut_short_is_reported_as_maybe_partial() {
        let desktop = windows(Fake::desktop());
        desktop.api.with(|state| state.accepts = Some(1));
        let shortcut = Shortcut {
            modifiers: vec![Modifier::Alt],
            key: "F4".to_owned(),
        };

        let error = desktop
            .focus_app(&app(&["notepad.exe"], Some(shortcut)))
            .unwrap_err();

        assert!(matches!(error, HostError::MaybePartial(_)));
    }

    #[test]
    fn key_names_map_to_virtual_keys() {
        for (name, code) in [
            ("k", Some(0x4B)),
            ("K", Some(0x4B)),
            ("7", Some(0x37)),
            ("F1", Some(0x70)),
            ("f24", Some(0x87)),
            ("F25", None),
            ("F0", None),
            ("Return", Some(0x0D)),
            ("tab", Some(0x09)),
            ("Escape", Some(0x1B)),
            ("space", Some(0x20)),
            ("BackSpace", Some(0x08)),
            ("Hyper_L", None),
        ] {
            assert_eq!(virtual_key(name), code, "{name}");
        }
        assert_eq!(
            shortcut_events(&[Modifier::Alt, Modifier::Logo], "x")
                .unwrap()
                .len(),
            6
        );
    }

    #[test]
    fn the_clipboard_and_the_browser_report_what_windows_refused() {
        let desktop = windows(Fake::desktop());
        let url = WebUrl::parse("https://github.com/NotPellew/Thalvi").unwrap();

        desktop.copy("kept").unwrap();
        desktop.open_url(&url).unwrap();
        desktop
            .open_folder(Path::new(r"C:\Users\me\thalvi"))
            .unwrap();
        assert_eq!(desktop.api.read(|state| state.clipboard.clone()), ["kept"]);
        assert_eq!(
            desktop.api.read(|state| state.opened.clone()),
            ["https://github.com/NotPellew/Thalvi", r"C:\Users\me\thalvi"]
        );

        desktop.api.with(|state| {
            state.clipboard_open = false;
            state.browser_open = false;
        });
        assert_eq!(
            nothing_typed_text(desktop.copy("lost").unwrap_err()),
            "Windows would not let Thalvi use the clipboard."
        );
        let target = desktop.capture_target().unwrap();
        assert_eq!(
            nothing_typed_text(desktop.deliver(&target, &text("lost"), true).unwrap_err()),
            "Windows would not let Thalvi use the clipboard."
        );
        assert!(desktop.api.read(|state| state.sent.is_empty()));
        assert_eq!(
            nothing_typed_text(desktop.open_url(&url).unwrap_err()),
            "Windows could not open the web address."
        );
        assert_eq!(
            nothing_typed_text(desktop.open_folder(Path::new("x")).unwrap_err()),
            "Windows could not open the folder."
        );
    }

    #[test]
    fn the_real_process_lookup_reads_this_test_process() {
        let me = win32::process_by_pid(std::process::id()).unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            me.identity.executable,
            exe.file_name().unwrap().to_string_lossy()
        );
        assert!(me.identity.started > 0);
        assert_eq!(me.integrity, Real.own_integrity());
        assert!(me.integrity.is_some());
    }
}
