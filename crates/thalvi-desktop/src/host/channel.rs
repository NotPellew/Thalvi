//! The request/reply socket: the running app answers one line per connection, to its owner alone.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::net::sockopt::socket_peercred;

use super::line::{LineError, MAX_REPLY, MAX_REQUEST, read_line, refusal};
use crate::host::{HostError, RequestServer};

const SOCKET: &str = "thalvi.sock";

/// How long a request is waited for once a client has connected.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a reply may take to be written.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a request that is too long is read and dropped, so that its sender can read the refusal.
const MAX_DRAIN: u64 = 16 * 1024 * 1024;

/// How many requests are answered at once; a client past it is told to try again.
const MAX_CONNECTIONS: usize = 8;

fn unsupported(text: impl Into<String>) -> HostError {
    HostError::Unsupported(text.into())
}

/// The session's runtime folder, which systemd makes readable by the user alone.
fn runtime_dir() -> Result<PathBuf, HostError> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| {
            unsupported(
                "The desktop session has no private runtime folder, so Thalvi cannot be controlled from outside.",
            )
        })
}

pub(super) fn serve(
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<RequestServer, HostError> {
    serve_in(
        &runtime_dir()?,
        rustix::process::geteuid().as_raw(),
        READ_TIMEOUT,
        handler,
    )
}

pub(super) fn ask(request: &str, timeout: Duration) -> Result<Option<String>, HostError> {
    ask_in(&runtime_dir()?, request, timeout)
}

/// Serves in `dir` instead of the session's runtime folder, for tests that keep their own.
pub(super) fn serve_at(
    dir: &Path,
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<RequestServer, HostError> {
    serve_in(
        dir,
        rustix::process::geteuid().as_raw(),
        READ_TIMEOUT,
        handler,
    )
}

/// Asks in `dir` instead of the session's runtime folder.
pub(super) fn ask_at(
    dir: &Path,
    request: &str,
    timeout: Duration,
) -> Result<Option<String>, HostError> {
    ask_in(dir, request, timeout)
}

fn serve_in(
    dir: &Path,
    owner: u32,
    read_timeout: Duration,
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<RequestServer, HostError> {
    let mode = std::fs::metadata(dir)
        .map_err(|error| unsupported(format!("The runtime folder could not be read: {error}.")))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(unsupported(
            "The runtime folder is open to other users, so Thalvi cannot be controlled from outside.",
        ));
    }
    let path = dir.join(SOCKET);
    if UnixStream::connect(&path).is_ok() {
        return Err(unsupported("Another Thalvi is already running."));
    }
    // Nothing answers, so a file left here is from a Thalvi that did not clean up.
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(unsupported(format!(
            "The old socket could not be removed: {error}."
        )));
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| unsupported(format!("The socket could not be made: {error}.")))?;
    let private = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let served = private.map_err(|error| error.to_string()).and_then(|()| {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let handler: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(handler);
        let busy = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("thalvi-requests".to_owned())
            .spawn(move || accept(&listener, &stopped, owner, read_timeout, &handler, &busy))
            .map(|thread| (stop, thread))
            .map_err(|error| error.to_string())
    });
    match served {
        Ok((stop, thread)) => Ok(RequestServer::new(Serving {
            path,
            stop,
            thread: Some(thread),
        })),
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            Err(unsupported(format!(
                "The socket could not be served: {error}."
            )))
        }
    }
}

fn accept(
    listener: &UnixListener,
    stopped: &AtomicBool,
    owner: u32,
    read_timeout: Duration,
    handler: &Arc<dyn Fn(&str) -> String + Send + Sync>,
    busy: &Arc<AtomicUsize>,
) {
    for connection in listener.incoming() {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!("The socket stopped answering: {error}");
                break;
            }
        };
        // Only the owner may ask: the folder already keeps other users out, so this is a second lock.
        if socket_peercred(&connection).map_or(true, |peer| peer.uid.as_raw() != owner) {
            continue;
        }
        let Some(slot) = Slot::take(busy) else {
            let _ = reply(
                &connection,
                &refusal("Thalvi is busy. Try again in a moment."),
            );
            continue;
        };
        let handler = Arc::clone(handler);
        let spawned = std::thread::Builder::new()
            .name("thalvi-request".to_owned())
            .spawn(move || {
                let _slot = slot;
                let _ = connection.set_read_timeout(Some(read_timeout));
                let read = read_line(&connection, MAX_REQUEST);
                let answer = match &read {
                    Ok(request) => handler(request),
                    Err(LineError::TooLong) => refusal("That request is too long."),
                    Err(LineError::NotText) => refusal("That request is not text."),
                    Err(LineError::Io(_)) => return,
                };
                let _ = reply(&connection, &answer);
                if read == Err(LineError::TooLong) {
                    // A client still sending would otherwise be reset and never see the reply.
                    let _ = std::io::copy(&mut (&connection).take(MAX_DRAIN), &mut std::io::sink());
                }
            });
        if spawned.is_err() {
            tracing::warn!("A request could not be answered: no thread to run it");
        }
    }
}

/// One of the requests answered at once, given back when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(busy: &Arc<AtomicUsize>) -> Option<Self> {
        if busy.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            busy.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(busy)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn reply(mut connection: &UnixStream, answer: &str) -> std::io::Result<()> {
    connection.set_write_timeout(Some(WRITE_TIMEOUT))?;
    writeln!(connection, "{answer}")
}

/// Stops the thread and removes the socket when dropped.
struct Serving {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the thread out of `accept`.
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn ask_in(dir: &Path, request: &str, timeout: Duration) -> Result<Option<String>, HostError> {
    let stream = match UnixStream::connect(dir.join(SOCKET)) {
        Ok(stream) => stream,
        // No socket, or a stale one that nothing listens on.
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(unsupported(format!("Thalvi could not be asked: {error}.")));
        }
    };
    let failed = |error: std::io::Error| unsupported(format!("Thalvi did not answer: {error}."));
    stream.set_read_timeout(Some(timeout)).map_err(failed)?;
    stream.set_write_timeout(Some(timeout)).map_err(failed)?;
    writeln!(&stream, "{request}").map_err(failed)?;
    match read_line(&stream, MAX_REPLY) {
        Ok(reply) if reply.is_empty() => Err(unsupported(
            "Thalvi closed the connection without answering.",
        )),
        Ok(reply) => Ok(Some(reply)),
        Err(LineError::Io(kind)) => Err(failed(kind.into())),
        Err(_) => Err(unsupported(
            "Thalvi answered with something that is not a reply.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;

    use super::*;

    const SHORT: Duration = Duration::from_millis(500);

    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// A runtime folder like the session's: the user's alone.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str, mode: u32) -> Self {
            let dir =
                std::env::temp_dir().join(format!("thalvi-sock-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, Permissions::from_mode(mode)).unwrap();
            Self(dir)
        }

        fn socket(&self) -> PathBuf {
            self.0.join(SOCKET)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ask(dir: &Dir, request: &str) -> Result<Option<String>, HostError> {
        ask_in(&dir.0, request, SHORT)
    }

    fn echo(request: &str) -> String {
        format!("heard {request}")
    }

    #[test]
    fn a_client_is_answered_for_its_request_until_the_app_stops_serving() {
        let dir = Dir::new("serve", 0o700);
        let server = serve_in(&dir.0, me(), SHORT, echo).unwrap();
        assert_eq!(ask(&dir, "state").unwrap(), Some("heard state".to_owned()));
        assert_eq!(ask(&dir, "pause").unwrap(), Some("heard pause".to_owned()));
        drop(server);
        assert_eq!(ask(&dir, "state"), Ok(None));
        assert!(!dir.socket().exists(), "the socket is removed");
    }

    #[test]
    fn nothing_answers_when_no_app_runs_or_it_died_leaving_its_socket() {
        let dir = Dir::new("stale", 0o700);
        assert_eq!(ask(&dir, "state"), Ok(None), "no socket");
        drop(UnixListener::bind(dir.socket()).unwrap());
        assert!(dir.socket().exists());
        assert_eq!(ask(&dir, "state"), Ok(None), "a socket nothing listens on");
        let server = serve_in(&dir.0, me(), SHORT, echo).unwrap();
        assert_eq!(
            ask(&dir, "state").unwrap(),
            Some("heard state".to_owned()),
            "a new app replaces the stale file"
        );
        drop(server);
    }

    #[test]
    fn a_second_app_leaves_a_live_socket_alone() {
        let dir = Dir::new("live", 0o700);
        let _first = serve_in(&dir.0, me(), SHORT, |_| "first".to_owned()).unwrap();
        let second = serve_in(&dir.0, me(), SHORT, |_| "second".to_owned());
        assert_eq!(
            second.err(),
            Some(unsupported("Another Thalvi is already running."))
        );
        assert_eq!(ask(&dir, "state").unwrap(), Some("first".to_owned()));
    }

    #[test]
    fn only_the_owner_gets_in_by_folder_socket_and_peer() {
        let open = Dir::new("open", 0o755);
        let refused = serve_in(&open.0, me(), SHORT, echo);
        assert_eq!(
            refused.err(),
            Some(unsupported(
                "The runtime folder is open to other users, so Thalvi cannot be controlled from outside."
            ))
        );
        assert!(!open.socket().exists());

        let dir = Dir::new("modes", 0o700);
        let called = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&called);
        let server = serve_in(&dir.0, me() + 1, SHORT, move |_| {
            seen.store(true, Ordering::SeqCst);
            "answered".to_owned()
        })
        .unwrap();
        let mode = std::fs::metadata(dir.socket())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the socket is the user's alone");
        // A connection from another user is closed without being read or answered.
        assert!(
            matches!(ask(&dir, "state"), Err(HostError::Unsupported(_))),
            "no answer"
        );
        assert!(!called.load(Ordering::SeqCst), "the handler never ran");
        drop(server);
    }

    #[test]
    fn a_request_is_read_within_its_bound_and_a_silent_client_is_given_up_on() {
        let dir = Dir::new("bound", 0o700);
        let server = serve_in(&dir.0, me(), SHORT, |request| request.len().to_string()).unwrap();
        let exactly = "a".repeat(MAX_REQUEST);
        assert_eq!(
            ask_in(&dir.0, &exactly, Duration::from_secs(5)).unwrap(),
            Some(MAX_REQUEST.to_string()),
            "at the limit"
        );
        let past = "a".repeat(MAX_REQUEST + 1);
        let refused = ask_in(&dir.0, &past, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            refused,
            refusal("That request is too long."),
            "just past it"
        );
        let far = "a".repeat(MAX_REQUEST * 4);
        let refused_far = ask_in(&dir.0, &far, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            refused_far,
            refusal("That request is too long."),
            "a sender far past it still reads the refusal"
        );
        // Connected but silent: the app stops waiting and the next client is still answered.
        let mut silent = UnixStream::connect(dir.socket()).unwrap();
        let mut closed = Vec::new();
        silent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        silent.read_to_end(&mut closed).unwrap();
        assert!(closed.is_empty(), "no answer for no request");
        assert_eq!(ask(&dir, "ok").unwrap(), Some("2".to_owned()));
        drop(server);
    }

    #[test]
    fn a_reply_past_its_bound_is_not_taken_for_an_answer() {
        let dir = Dir::new("reply", 0o700);
        let listener = UnixListener::bind(dir.socket()).unwrap();
        let replies = std::thread::spawn(move || {
            for reply in [vec![b'a'; MAX_REPLY], vec![b'a'; MAX_REPLY + 1], Vec::new()] {
                let (mut connection, _) = listener.accept().unwrap();
                let _ = read_line(&connection, MAX_REQUEST);
                let _ = connection.write_all(&reply);
            }
        });
        let long = |dir: &Dir| ask_in(&dir.0, "state", Duration::from_secs(5));
        assert_eq!(
            long(&dir).unwrap().map(|reply| reply.len()),
            Some(MAX_REPLY)
        );
        assert_eq!(
            long(&dir),
            Err(unsupported(
                "Thalvi answered with something that is not a reply."
            )),
            "just past it"
        );
        assert_eq!(
            long(&dir),
            Err(unsupported(
                "Thalvi closed the connection without answering."
            )),
        );
        replies.join().unwrap();
    }
}
