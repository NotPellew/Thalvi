//! The Windows request/reply pipe: a named pipe only the current user may open, never over the network.

#![expect(
    unsafe_code,
    reason = "raw Win32 pipe calls; each block says why it is sound"
)]

use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree,
};
use ::windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use ::windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use ::windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use ::windows::Win32::System::IO::CancelIoEx;
use ::windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use ::windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use ::windows::core::{BOOL, HRESULT, PCWSTR, PWSTR};

use crate::host::line::{LineError, MAX_REPLY, MAX_REQUEST, read_line, refusal};
use crate::host::{HostError, RequestServer};

/// How long a request is waited for once a client has connected.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a reply may take to be written.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a request that is too long is read and dropped, so that its sender can read the refusal.
const MAX_DRAIN: u64 = 16 * 1024 * 1024;

/// How many requests are answered at once; a client past it is told to try again.
const MAX_CONNECTIONS: usize = 8;

const BUFFER: u32 = 64 * 1024;

type Handler = Arc<dyn Fn(&str) -> String + Send + Sync>;

fn unsupported(text: impl Into<String>) -> HostError {
    HostError::Unsupported(text.into())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// The pipe of one user; `instance` tells apart the pipes of tests.
fn pipe_name(sid: &str, instance: &str) -> String {
    format!(r"\\.\pipe\thalvi-{sid}{instance}")
}

/// A descriptor that lets `sid` alone open the pipe, and nobody else, administrators included.
fn pipe_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

/// Closes a kernel handle when dropped.
struct Closing(HANDLE);

impl Drop for Closing {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful open call and is closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The user who runs `process`, as text such as `S-1-5-21-…`.
fn sid_of(process: HANDLE) -> Result<String, String> {
    let mut token = HANDLE::default();
    // SAFETY: `process` is a live process handle and `token` is a valid out pointer.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
        .map_err(|error| error.to_string())?;
    let token = Closing(token);

    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 only asks for the size, which is written to `needed`.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &raw mut needed) };
    // u64 units keep the buffer aligned for the pointer inside TOKEN_USER.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    if buffer.is_empty() {
        return Err("Windows did not say who runs Thalvi.".to_owned());
    }
    // SAFETY: `buffer` holds at least `needed` writable bytes.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            needed,
            &raw mut needed,
        )
    }
    .map_err(|error| error.to_string())?;
    // SAFETY: Windows filled `buffer` with a TOKEN_USER whose SID points into it, and the buffer
    // outlives this read.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = PWSTR::null();
    // SAFETY: `sid` is valid, and `text` receives a string that is freed below.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.map_err(|error| error.to_string())?;
    // SAFETY: on success `text` is a NUL-terminated string allocated by Windows.
    let sid = unsafe { text.to_string() };
    // SAFETY: the string was allocated with LocalAlloc and is not used again.
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    sid.map_err(|error| error.to_string())
}

fn own_sid() -> Result<String, HostError> {
    // SAFETY: the pseudo handle for the current process needs no closing.
    sid_of(unsafe { GetCurrentProcess() })
        .map_err(|error| unsupported(format!("Windows did not say who runs Thalvi: {error}")))
}

/// A security descriptor built from SDDL text, freed when dropped.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn of(sddl: &str) -> Result<Self, String> {
        let text = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `text` is NUL-terminated and `descriptor` is a valid out pointer.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .map_err(|error| error.to_string())?;
        Ok(Self(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: Windows allocated the descriptor with LocalAlloc and it is freed once, here.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

/// Makes one instance of the pipe, open to the user `sid` alone and to no client from the network.
fn create_instance(name: &[u16], sid: &str, first: bool) -> Result<File, String> {
    let descriptor = Descriptor::of(&pipe_sddl(sid))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0.0,
        bInheritHandle: BOOL(0),
    };
    let mut open = PIPE_ACCESS_DUPLEX;
    if first {
        // Another process that made the pipe first would otherwise be joined, not refused.
        open |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: `name` is NUL-terminated and `attributes` and its descriptor outlive the call, which
    // copies them.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            open,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            Some(&raw const attributes),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(::windows::core::Error::from_thread().to_string());
    }
    // SAFETY: `handle` is a new pipe handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn raw(pipe: &File) -> HANDLE {
    HANDLE(pipe.as_raw_handle())
}

/// Runs `work`, which uses `pipe`, and cancels its pending input and output once `limit` passes.
fn within<T>(pipe: &File, limit: Duration, work: impl FnOnce() -> T) -> T {
    let (done, waiting) = mpsc::channel::<()>();
    let handle = raw(pipe).0 as usize;
    let watchdog = std::thread::spawn(move || {
        if waiting.recv_timeout(limit) == Err(RecvTimeoutError::Timeout) {
            // SAFETY: the caller keeps the pipe open until this thread is joined.
            let _ = unsafe { CancelIoEx(HANDLE(handle as *mut c_void), None) };
        }
    });
    let result = work();
    drop(done);
    let _ = watchdog.join();
    result
}

pub(super) fn serve(handler: Handler) -> Result<RequestServer, HostError> {
    serve_at("", handler)
}

fn serve_at(instance: &str, handler: Handler) -> Result<RequestServer, HostError> {
    let sid = own_sid()?;
    let name = wide(&pipe_name(&sid, instance));
    let first = create_instance(&name, &sid, true).map_err(|error| {
        unsupported(format!(
            "The pipe could not be made, or another Thalvi is already running: {error}"
        ))
    })?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let woken = name.clone();
    let thread = std::thread::Builder::new()
        .name("thalvi-requests".to_owned())
        .spawn(move || accept(first, &name, &sid, &stopped, &handler))
        .map_err(|error| unsupported(format!("The pipe could not be served: {error}.")))?;
    Ok(RequestServer::new(Serving {
        name: woken,
        stop,
        thread: Some(thread),
    }))
}

fn accept(first: File, name: &[u16], sid: &str, stopped: &AtomicBool, handler: &Handler) {
    let busy = Arc::new(AtomicUsize::new(0));
    let mut waiting = first;
    loop {
        // SAFETY: `waiting` is a live pipe handle opened without overlapped I/O.
        let connected = unsafe { ConnectNamedPipe(raw(&waiting), None) };
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        // A client that connected before this call is a client all the same.
        if let Err(error) = connected
            && error.code() != HRESULT::from_win32(ERROR_PIPE_CONNECTED.0)
        {
            tracing::warn!("The pipe stopped answering: {error}");
            break;
        }
        let next = match create_instance(name, sid, false) {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!("The pipe stopped answering: {error}");
                break;
            }
        };
        let connection = std::mem::replace(&mut waiting, next);
        let handler = Arc::clone(handler);
        let slot = Slot::take(&busy);
        let spawned = std::thread::Builder::new()
            .name("thalvi-request".to_owned())
            .spawn(move || answer(&connection, slot, &handler));
        if spawned.is_err() {
            tracing::warn!("A request could not be answered: no thread to run it");
        }
    }
}

/// One of the requests answered at once, given back when dropped; none when all are taken.
struct Slot(Option<Arc<AtomicUsize>>);

impl Slot {
    fn take(busy: &Arc<AtomicUsize>) -> Self {
        if busy.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            busy.fetch_sub(1, Ordering::SeqCst);
            return Self(None);
        }
        Self(Some(Arc::clone(busy)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(busy) = &self.0 {
            busy.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn answer(connection: &File, slot: Slot, handler: &Handler) {
    let mut too_long = false;
    let reply = if slot.0.is_none() {
        refusal("Thalvi is busy. Try again in a moment.")
    } else {
        match within(connection, READ_TIMEOUT, || {
            read_line(connection, MAX_REQUEST)
        }) {
            Ok(request) => handler(&request),
            Err(LineError::TooLong) => {
                too_long = true;
                refusal("That request is too long.")
            }
            Err(LineError::NotText) => refusal("That request is not text."),
            Err(LineError::Io(_)) => String::new(),
        }
    };
    if !reply.is_empty() {
        within(connection, WRITE_TIMEOUT, || {
            let mut pipe = connection;
            let _ = writeln!(pipe, "{reply}");
        });
    }
    if too_long {
        // A client still sending would otherwise be cut off and never see the reply.
        within(connection, READ_TIMEOUT, || {
            let _ = std::io::copy(&mut connection.take(MAX_DRAIN), &mut std::io::sink());
        });
    }
    // SAFETY: the pipe is open; this waits until the client has read what was written.
    let _ = unsafe { FlushFileBuffers(raw(connection)) };
    // SAFETY: the pipe is open and this instance serves no one else.
    let _ = unsafe { DisconnectNamedPipe(raw(connection)) };
    drop(slot);
}

/// Stops the thread and lets the pipe go when dropped.
struct Serving {
    name: Vec<u16>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the thread out of `ConnectNamedPipe`.
        drop(open(&self.name));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open(name: &[u16]) -> Result<File, ::windows::core::Error> {
    // SAFETY: `name` is NUL-terminated. The security flags keep a server from acting as this user.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    }?;
    // SAFETY: `handle` is a new file handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

/// Fails unless the pipe is served by a process of the user `sid`, so that a program of another
/// user that made the pipe first is never handed a request.
fn check_server(pipe: &File, sid: &str) -> Result<(), String> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a live client handle and `pid` is a valid out pointer.
    unsafe { GetNamedPipeServerProcessId(raw(pipe), &raw mut pid) }
        .map_err(|error| error.to_string())?;
    // SAFETY: OpenProcess takes plain values; the handle it returns is owned by `Closing`.
    let process = Closing(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|error| error.to_string())?,
    );
    if sid_of(process.0)? == sid {
        Ok(())
    } else {
        Err("Another user runs the program that answers.".to_owned())
    }
}

pub(super) fn ask(request: &str, timeout: Duration) -> Result<Option<String>, HostError> {
    ask_at("", request, timeout)
}

fn ask_at(instance: &str, request: &str, timeout: Duration) -> Result<Option<String>, HostError> {
    let sid = own_sid()?;
    let name = wide(&pipe_name(&sid, instance));
    let started = Instant::now();
    let pipe = loop {
        match open(&name) {
            Ok(pipe) => break pipe,
            Err(error) if error.code() == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) => {
                return Ok(None);
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_BUSY.0) => {
                let left = timeout
                    .checked_sub(started.elapsed())
                    .filter(|left| !left.is_zero())
                    .ok_or_else(|| unsupported("Thalvi did not answer: it is busy."))?;
                // SAFETY: `name` is NUL-terminated.
                let _ = unsafe {
                    WaitNamedPipeW(
                        PCWSTR(name.as_ptr()),
                        u32::try_from(left.as_millis()).unwrap_or(u32::MAX),
                    )
                };
            }
            Err(error) => {
                return Err(unsupported(format!("Thalvi could not be asked: {error}.")));
            }
        }
    };
    check_server(&pipe, &sid)
        .map_err(|error| unsupported(format!("Thalvi could not be asked: {error}")))?;
    let failed = |error: std::io::Error| unsupported(format!("Thalvi did not answer: {error}."));
    let left = timeout.saturating_sub(started.elapsed());
    within(&pipe, left, || {
        let mut writer = &pipe;
        writeln!(writer, "{request}").map_err(failed)?;
        match read_line(&pipe, MAX_REPLY) {
            Ok(reply) if reply.is_empty() => Err(unsupported(
                "Thalvi closed the connection without answering.",
            )),
            Ok(reply) => Ok(Some(reply)),
            Err(LineError::Io(kind)) => Err(failed(kind.into())),
            Err(_) => Err(unsupported(
                "Thalvi answered with something that is not a reply.",
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use ::windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use ::windows::Win32::Security::DACL_SECURITY_INFORMATION;

    use super::*;

    const SHORT: Duration = Duration::from_secs(5);

    fn instance(name: &str) -> String {
        format!("-test-{name}-{}", std::process::id())
    }

    #[test]
    fn the_pipe_admits_the_current_user_alone() {
        let sid = own_sid().unwrap();
        let descriptor = Descriptor::of(&pipe_sddl(&sid)).unwrap();
        let mut text = PWSTR::null();
        // SAFETY: the descriptor is valid and `text` receives a string that is freed below.
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &raw mut text,
                None,
            )
        }
        .unwrap();
        // SAFETY: on success `text` is a NUL-terminated string allocated by Windows.
        let sddl = unsafe { text.to_string() }.unwrap();
        // SAFETY: the string was allocated with LocalAlloc and is not used again.
        let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        assert_eq!(
            sddl,
            format!("D:P(A;;GA;;;{sid})"),
            "protected, with one entry for the user and none for anyone else"
        );
        assert!(
            pipe_name(&sid, "").starts_with(r"\\.\pipe\"),
            "a local pipe, whose name holds the user's SID"
        );
        assert!(pipe_name(&sid, "").ends_with(&sid));
    }

    #[test]
    fn a_client_is_answered_until_the_app_stops_serving_and_no_second_app_takes_the_pipe() {
        let instance = instance("serve");
        let server = serve_at(&instance, Arc::new(|request| format!("heard {request}"))).unwrap();
        assert_eq!(
            ask_at(&instance, "state", SHORT).unwrap(),
            Some("heard state".to_owned())
        );
        assert_eq!(
            ask_at(&instance, "pause", SHORT).unwrap(),
            Some("heard pause".to_owned())
        );
        let second = serve_at(&instance, Arc::new(|_| "second".to_owned()));
        assert!(second.is_err(), "the pipe is already served");
        drop(server);
        assert_eq!(ask_at(&instance, "state", SHORT), Ok(None));
    }

    #[test]
    fn a_request_is_read_within_its_bound() {
        let instance = instance("bound");
        let _server = serve_at(&instance, Arc::new(|request| request.len().to_string())).unwrap();
        assert_eq!(
            ask_at(&instance, &"a".repeat(MAX_REQUEST), SHORT).unwrap(),
            Some(MAX_REQUEST.to_string()),
            "at the limit"
        );
        assert_eq!(
            ask_at(&instance, &"a".repeat(MAX_REQUEST + 1), SHORT).unwrap(),
            Some(refusal("That request is too long.")),
            "just past it"
        );
    }
}
