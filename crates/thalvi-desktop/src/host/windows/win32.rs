//! The real Win32 calls behind the host's seam. The only place `unsafe` is allowed.

#![expect(
    unsafe_code,
    reason = "raw Win32 calls; each block says why it is sound"
)]

use std::ffi::c_void;
use std::mem::size_of;
use std::time::Duration;

use ::windows::Win32::Foundation::{
    CloseHandle, FILETIME, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM,
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
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, SendInput, VIRTUAL_KEY,
};
use ::windows::Win32::UI::Shell::ShellExecuteW;
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetForegroundWindow, GetWindow, GetWindowLongW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE, SW_SHOWNORMAL,
    SetForegroundWindow, ShowWindow, WS_EX_TOOLWINDOW,
};
use ::windows::core::{BOOL, PCWSTR, PWSTR, w};

use super::{Hwnd, Identity, Key, KeyEvent, Process, Win32};

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

/// Who runs process `pid`, or `None` when it is gone or Windows will not say.
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
    let (code, scan, mut flags) = match event.key {
        Key::Unicode(unit) => (0, unit, KEYEVENTF_UNICODE),
        Key::Virtual(code) => (code, 0, KEYBD_EVENT_FLAGS(0)),
    };
    if event.release {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(code),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
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
}
