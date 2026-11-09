//! The real Win32 calls behind the host's seam. The only place `unsafe` is allowed.

#![expect(
    unsafe_code,
    reason = "raw Win32 calls; each block says why it is sound"
)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::thread::JoinHandle;
use std::time::Duration;

use ::windows::Win32::Foundation::{
    CloseHandle, FILETIME, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use ::windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use ::windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use ::windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use ::windows::Win32::System::Ole::CF_UNICODETEXT;
use ::windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use ::windows::Win32::UI::HiDpi::GetDpiForSystem;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use ::windows::Win32::UI::Shell::ShellExecuteW;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, EnumWindows, GW_OWNER, GWL_EXSTYLE, GetForegroundWindow, GetMessageW,
    GetWindow, GetWindowLongW, GetWindowThreadProcessId, HC_ACTION, IsIconic, IsWindow,
    IsWindowVisible, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MB_ICONERROR, MB_OK, MSG,
    MessageBoxW, PostThreadMessageW, SPI_GETWORKAREA, SW_HIDE, SW_RESTORE, SW_SHOWNOACTIVATE,
    SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetForegroundWindow, SetWindowsHookExW,
    ShowWindow, SystemParametersInfoW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WS_EX_TOOLWINDOW,
};
use ::windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

use super::{Hwnd, Identity, KeyEvent, Process, Win32};
use crate::hold::{self, HoldHooks, HoldKey, HoldKind, Verdict};
use crate::host::{HoldWatch, HostError};

/// The live Windows desktop.
pub struct Real;

fn handle(window: Hwnd) -> HWND {
    HWND(window as *mut c_void)
}

/// Closes a kernel handle when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open call and is closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Shows `window` without activating it, or hides it.
pub(super) fn set_visible_unfocused(window: Hwnd, visible: bool) {
    let command = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
    // SAFETY: ShowWindow only acts on a window handle; a stale handle makes it fail harmlessly.
    let _ = unsafe { ShowWindow(handle(window), command) };
}

/// Who runs process `pid`, or `None` when it is gone or Windows will not say.
/// Shows `message` in an error box titled `title` and waits until it is closed.
pub(super) fn message_box(title: &str, message: &str) {
    // SAFETY: both texts are `HSTRING`s, null-terminated and alive for the whole call, and no
    // owner window is given.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from(title),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub(super) fn process_by_pid(pid: u32) -> Option<Process> {
    // SAFETY: OpenProcess takes plain values; the handle it returns is owned by `Owned`.
    let process =
        Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);

    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: the handle is a live process handle and the four pointers are distinct, valid FILETIMEs.
    unsafe { GetProcessTimes(process.0, created, exited, kernel, user) }.ok()?;
    let started = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);

    let mut path = [0u16; 1024];
    let mut length = u32::try_from(path.len()).ok()?;
    // SAFETY: `path` has `length` writable units and `length` is updated to the units written.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &raw mut length,
        )
    }
    .ok()?;
    let path = String::from_utf16_lossy(path.get(..usize::try_from(length).ok()?)?);
    let executable = path.rsplit('\\').next()?.to_owned();

    Some(Process {
        identity: Identity {
            pid,
            started,
            executable,
        },
        integrity: integrity_of(process.0),
    })
}

/// The mandatory integrity level (RID) of a process, or `None` when Windows will not say.
fn integrity_of(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    // SAFETY: `process` is a live process handle and `token` is a valid out pointer.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }.ok()?;
    let token = Owned(token);

    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 only asks for the size, which is written to `needed`.
    let _ = unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &raw mut needed) };
    // u64 units keep the buffer aligned for the pointer inside TOKEN_MANDATORY_LABEL.
    let mut buffer = vec![0u64; usize::try_from(needed).ok()?.div_ceil(size_of::<u64>())];
    if buffer.is_empty() {
        return None;
    }
    // SAFETY: `buffer` holds at least `needed` writable bytes.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            needed,
            &raw mut needed,
        )
    }
    .ok()?;
    // SAFETY: Windows filled `buffer` with a TOKEN_MANDATORY_LABEL whose SID points into it, and
    // the buffer outlives these reads.
    unsafe {
        let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sub_authorities = *GetSidSubAuthorityCount(label.Label.Sid);
        let last = u32::from(sub_authorities.checked_sub(1)?);
        Some(*GetSidSubAuthority(label.Label.Sid, last))
    }
}

/// Whether the user sees `window` as an app window, not a tool window or a dialog's owner chain.
fn is_app_window(window: HWND) -> bool {
    // SAFETY: these calls only read state of a window handle; a stale handle makes them fail.
    unsafe {
        let extended = GetWindowLongW(window, GWL_EXSTYLE).cast_unsigned();
        IsWindowVisible(window).as_bool()
            && extended & WS_EX_TOOLWINDOW.0 == 0
            && GetWindow(window, GW_OWNER).is_err()
    }
}

unsafe extern "system" fn collect_window(window: HWND, windows: LPARAM) -> BOOL {
    if is_app_window(window) {
        // SAFETY: `windows` is the `&mut Vec<Hwnd>` that `top_level_windows` passed to EnumWindows,
        // which stays alive and unaliased for the whole enumeration.
        let windows = unsafe { &mut *(windows.0 as *mut Vec<Hwnd>) };
        windows.push(window.0 as Hwnd);
    }
    BOOL::from(true)
}

fn input(event: KeyEvent) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if event.release {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(event.key),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A virtual key that no key sends, tapped between a swallowed hold key and the release of its Alt
/// or Win so the focused app does not take that release for a lone press that opens its menu.
const MASK_KEY: u16 = 0xE8;

type OnChange = Box<dyn Fn(HoldKind, bool) + Send>;

/// What the keyboard hook procedure works with; it runs on the thread that installed the hook.
struct Rules {
    hooks: HoldHooks,
    on_change: OnChange,
}

thread_local! {
    static RULES: RefCell<Option<Rules>> = const { RefCell::new(None) };
}

fn virtual_input(code: u16, release: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if release {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(code),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Taps the unused key, then lets the release of `modifier` through behind it; says how many of
/// the events went in.
fn mask_release(modifier: u16, extended: bool) -> usize {
    let inputs = [
        virtual_input(MASK_KEY, false, false),
        virtual_input(MASK_KEY, true, false),
        virtual_input(modifier, true, extended),
    ];
    let Ok(size) = i32::try_from(size_of::<INPUT>()) else {
        return 0;
    };
    // SAFETY: `inputs` is a valid slice of INPUT structures and `size` is the size of one. The hook
    // skips these events, which are marked as injected.
    unsafe { SendInput(&inputs, size) as usize }
}

/// Whether the keyboard holds `vk` down right now.
fn physically_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState takes a plain key code.
    unsafe { GetAsyncKeyState(i32::from(vk)) }.cast_unsigned() & 0x8000 != 0
}

/// The low-level keyboard hook: keeps the hold key from the focused app, passes every other key.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION.cast_signed() {
        // SAFETY: for HC_ACTION, `lparam` points to a KBDLLHOOKSTRUCT that lives until this
        // procedure returns.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let message = u32::try_from(wparam.0).unwrap_or(0);
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if down || matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
            let event = hold::KeyEvent {
                vk: u16::try_from(info.vkCode).unwrap_or(0),
                scan_code: u16::try_from(info.scanCode).unwrap_or(0),
                extended,
                down,
                injected: info.flags.0 & LLKHF_INJECTED.0 != 0,
            };
            let decision = RULES.with(|rules| {
                let mut rules = rules.try_borrow_mut().ok()?;
                let rules = rules.as_mut()?;
                let (verdict, changes) = rules.hooks.on_event(event, physically_down);
                for (kind, down) in changes {
                    (rules.on_change)(kind, down);
                }
                Some(verdict)
            });
            match decision {
                Some(Verdict::Swallow) => return LRESULT(1),
                Some(Verdict::Mask) => {
                    if Verdict::after_mask(mask_release(event.vk, extended)) == Verdict::Swallow {
                        return LRESULT(1);
                    }
                }
                Some(Verdict::Pass) | None => {}
            }
        }
    }
    // SAFETY: the arguments are the ones this procedure was called with.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Ends the hook thread's message loop when dropped, and waits for the hook to be removed.
struct HookThread {
    id: u32,
    join: Option<JoinHandle<()>>,
}

impl Drop for HookThread {
    fn drop(&mut self) {
        // SAFETY: plain values; the thread runs a message loop that ends on WM_QUIT.
        let _ = unsafe { PostThreadMessageW(self.id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Starts the keyboard hook for `keys` on a thread of its own, reporting each change to `on_change`.
pub(super) fn watch_hold_keys(
    keys: Vec<(HoldKind, HoldKey)>,
    on_change: OnChange,
) -> Result<HoldWatch, HostError> {
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let join = std::thread::Builder::new()
        .name("thalvi-hold-key".to_owned())
        .spawn(move || {
            RULES.with(|rules| {
                *rules.borrow_mut() = Some(Rules {
                    hooks: HoldHooks::new(keys),
                    on_change,
                });
            });
            // SAFETY: `keyboard_hook` has the signature the hook requires and, as a low-level
            // hook, needs no module handle.
            let installed =
                unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) };
            let hook = match installed {
                Ok(hook) => hook,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };
            // SAFETY: no arguments.
            let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
            let mut message = MSG::default();
            // Windows calls the hook from inside this wait. GetMessageW returns 0 on WM_QUIT.
            // SAFETY: `message` is a valid MSG.
            while unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0 > 0 {}
            // SAFETY: `hook` came from the successful call above and is removed once.
            let _ = unsafe { UnhookWindowsHookEx(hook) };
            RULES.with(|rules| {
                if let Some(rules) = rules.borrow_mut().take() {
                    // A key still held when watching ends must not stay down.
                    (rules.on_change)(HoldKind::Dictate, false);
                    (rules.on_change)(HoldKind::Write, false);
                }
            });
        })
        .map_err(|error| refused(&error.to_string()))?;
    match ready.recv() {
        Ok(Ok(id)) => Ok(HoldWatch::new(HookThread {
            id,
            join: Some(join),
        })),
        Ok(Err(reason)) => Err(refused(&reason)),
        Err(_) => Err(refused("the keyboard hook thread stopped")),
    }
}

fn refused(reason: &str) -> HostError {
    HostError::Unsupported(format!(
        "Windows refused the keyboard hook for the dictation key: {reason}"
    ))
}

/// Holds the clipboard open; closes it when dropped.
struct Clipboard;

impl Clipboard {
    fn open() -> Option<Self> {
        // Another program, such as a clipboard manager, may hold it for a moment.
        for _ in 0..10 {
            // SAFETY: a null owner window is allowed; success is paired with CloseClipboard in Drop.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Some(Self);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: this process opened the clipboard in `open`.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Hands the open clipboard a copy of `bytes` as data in `format`.
fn set_data(format: u32, bytes: &[u8]) -> bool {
    if format == 0 {
        return false;
    }
    // SAFETY: the block is allocated for `bytes.len()` bytes, locked while copied into, and either
    // owned by the clipboard after SetClipboardData succeeds or freed here when it fails.
    unsafe {
        let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else {
            return false;
        };
        let destination = GlobalLock(memory);
        if destination.is_null() {
            let _ = GlobalFree(Some(memory));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(memory);
        let accepted = SetClipboardData(format, Some(HANDLE(memory.0))).is_ok();
        if !accepted {
            let _ = GlobalFree(Some(HGLOBAL(memory.0)));
        }
        accepted
    }
}

impl Win32 for Real {
    fn foreground(&self) -> Option<Hwnd> {
        // SAFETY: GetForegroundWindow takes no arguments.
        let window = unsafe { GetForegroundWindow() };
        (!window.0.is_null()).then_some(window.0 as Hwnd)
    }

    fn process(&self, window: Hwnd) -> Option<Process> {
        let window = handle(window);
        let mut pid = 0u32;
        // SAFETY: both calls tolerate a stale handle; `pid` is a valid out pointer.
        let owner = unsafe {
            IsWindow(Some(window)).as_bool()
                && GetWindowThreadProcessId(window, Some(&raw mut pid)) != 0
        };
        if owner && pid != 0 {
            process_by_pid(pid)
        } else {
            None
        }
    }

    fn top_level_windows(&self) -> Vec<Hwnd> {
        let mut windows = Vec::new();
        // SAFETY: the callback only reads `windows` through the pointer, which outlives the call.
        let _ = unsafe { EnumWindows(Some(collect_window), LPARAM(&raw mut windows as isize)) };
        windows
    }

    fn focus(&self, window: Hwnd) {
        let window = handle(window);
        // SAFETY: both calls only act on a window handle; a stale handle makes them fail harmlessly.
        unsafe {
            if IsIconic(window).as_bool() {
                let _ = ShowWindow(window, SW_RESTORE);
            }
            let _ = SetForegroundWindow(window);
        }
    }

    fn own_integrity(&self) -> Option<u32> {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        integrity_of(unsafe { GetCurrentProcess() })
    }

    fn send_input(&self, events: &[KeyEvent]) -> usize {
        let inputs: Vec<INPUT> = events.iter().copied().map(input).collect();
        let Ok(size) = i32::try_from(size_of::<INPUT>()) else {
            return 0;
        };
        // SAFETY: `inputs` is a valid slice of INPUT structures and `size` is the size of one.
        unsafe { SendInput(&inputs, size) as usize }
    }

    fn set_clipboard(&self, text: &str) -> bool {
        let Some(_clipboard) = Clipboard::open() else {
            return false;
        };
        // SAFETY: the clipboard is open, as EmptyClipboard requires.
        if unsafe { EmptyClipboard() }.is_err() {
            return false;
        }
        let unit = 0u32.to_le_bytes();
        let utf16: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        // SAFETY: each name is a valid NUL-terminated wide string.
        let marks = unsafe {
            [
                RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
                RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
                RegisterClipboardFormatW(w!("CanUploadToCloudClipboard")),
            ]
        };
        marks.into_iter().all(|mark| set_data(mark, &unit))
            && set_data(u32::from(CF_UNICODETEXT.0), &utf16)
    }

    fn open_url(&self, url: &str) -> bool {
        let wide: Vec<u16> = url.encode_utf16().chain([0]).collect();
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecuteW reports success as a value above 32.
        result.0 as usize > 32
    }

    fn work_area_top(&self) -> Option<f64> {
        let mut area = RECT::default();
        // SAFETY: SPI_GETWORKAREA fills the RECT that `area` points to, which outlives the call.
        unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&raw mut area).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .ok()?;
        // SAFETY: GetDpiForSystem takes no arguments.
        let dpi = unsafe { GetDpiForSystem() };
        Some(f64::from(area.top) * 96.0 / f64::from(dpi.max(96)))
    }
}

#[cfg(test)]
mod tests {
    use ::windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_POPUP,
    };

    use super::*;

    #[test]
    fn a_non_activating_window_shows_without_becoming_active() {
        // SAFETY: STATIC is a system window class, so no module handle or registration is needed.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("Thalvi test"),
                WS_POPUP,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("the test window is created");
        let window_id = window.0 as Hwnd;
        // SAFETY: the calls only read the state of the window this test owns.
        let visible = || unsafe { IsWindowVisible(window).as_bool() };
        // SAFETY: GetActiveWindow takes no arguments.
        let active = || unsafe { GetActiveWindow() } == window;

        set_visible_unfocused(window_id, true);
        let shown = (visible(), active());
        set_visible_unfocused(window_id, false);
        let hidden = visible();
        // SAFETY: the window belongs to this thread and is not used afterwards.
        unsafe { DestroyWindow(window) }.expect("the test window is destroyed");

        assert_eq!((shown, hidden), ((true, false), false));
    }
}
