//! The state socket: the running app tells each program that connects its state line, for status bars.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::host::{HostError, StateServer};

const SOCKET: &str = "thalvi-state.sock";

/// The longest reply that can be a state word and its newline.
const MAX_REPLY: usize = 32;

/// How long an answer or a question waits for the other side.
const TIMEOUT: Duration = Duration::from_millis(500);

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
                "The desktop session has no private runtime folder, so the status is not shared.",
            )
        })
}

pub fn serve(state: impl Fn() -> String + Send + 'static) -> Result<StateServer, HostError> {
    serve_in(&runtime_dir()?, state)
}

pub fn query() -> Result<Option<String>, HostError> {
    query_in(&runtime_dir()?, TIMEOUT)
}

fn serve_in(
    dir: &Path,
    state: impl Fn() -> String + Send + 'static,
) -> Result<StateServer, HostError> {
    let mode = std::fs::metadata(dir)
        .map_err(|error| unsupported(format!("The runtime folder could not be read: {error}.")))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(unsupported(
            "The runtime folder is open to other users, so the status is not shared.",
        ));
    }
    let path = dir.join(SOCKET);
    if UnixStream::connect(&path).is_ok() {
        return Err(unsupported("Another Thalvi already shares its status."));
    }
    // Nothing answers, so a file left here is from a Thalvi that did not clean up.
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(unsupported(format!(
            "The old status socket could not be removed: {error}."
        )));
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| unsupported(format!("The status socket could not be made: {error}.")))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("thalvi-state".to_owned())
        .spawn(move || {
            for connection in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                match connection {
                    Ok(mut connection) => {
                        let _ = connection.set_write_timeout(Some(TIMEOUT));
                        let _ = writeln!(connection, "{}", state());
                    }
                    Err(error) => {
                        tracing::warn!("The status socket stopped answering: {error}");
                        break;
                    }
                }
            }
        })
        .map_err(|error| {
            let _ = std::fs::remove_file(&path);
            unsupported(format!("The status socket could not be served: {error}."))
        })?;
    Ok(StateServer::new(Serving {
        path,
        stop,
        thread: Some(thread),
    }))
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

fn query_in(dir: &Path, timeout: Duration) -> Result<Option<String>, HostError> {
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
    let mut reply = Vec::new();
    stream
        .take(MAX_REPLY as u64 + 1)
        .read_to_end(&mut reply)
        .map_err(failed)?;
    if reply.len() > MAX_REPLY {
        return Err(unsupported(
            "Thalvi answered with something that is not a state.",
        ));
    }
    Ok(Some(String::from_utf8_lossy(&reply).trim_end().to_owned()))
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::sync::Mutex;

    use super::*;

    const SHORT: Duration = Duration::from_millis(200);

    /// A runtime folder like the session's: the user's alone.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str, mode: u32) -> Self {
            let dir =
                std::env::temp_dir().join(format!("thalvi-state-{name}-{}", std::process::id()));
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

    fn ask(dir: &Dir) -> Result<Option<String>, HostError> {
        query_in(&dir.0, SHORT)
    }

    #[test]
    fn a_client_is_told_the_current_state_until_the_app_stops_serving() {
        let dir = Dir::new("serve", 0o700);
        let current = Arc::new(Mutex::new("listening"));
        let shared = Arc::clone(&current);
        let server = serve_in(&dir.0, move || (*shared.lock().unwrap()).to_owned()).unwrap();
        assert_eq!(ask(&dir).unwrap(), Some("listening".to_owned()));
        *current.lock().unwrap() = "paused";
        assert_eq!(ask(&dir).unwrap(), Some("paused".to_owned()));
        drop(server);
        assert_eq!(ask(&dir), Ok(None));
        assert!(!dir.socket().exists(), "the socket is removed");
    }

    #[test]
    fn nothing_answers_when_no_app_runs_or_it_died_leaving_its_socket() {
        let dir = Dir::new("stale", 0o700);
        assert_eq!(ask(&dir), Ok(None), "no socket");
        drop(UnixListener::bind(dir.socket()).unwrap());
        assert!(dir.socket().exists());
        assert_eq!(ask(&dir), Ok(None), "a socket nothing listens on");
        let server = serve_in(&dir.0, || "listening".to_owned()).unwrap();
        assert_eq!(
            ask(&dir).unwrap(),
            Some("listening".to_owned()),
            "a new app replaces the stale file"
        );
        drop(server);
    }

    #[test]
    fn a_second_app_leaves_a_live_socket_alone() {
        let dir = Dir::new("live", 0o700);
        let _first = serve_in(&dir.0, || "listening".to_owned()).unwrap();
        let second = serve_in(&dir.0, || "paused".to_owned());
        assert_eq!(
            second.err(),
            Some(unsupported("Another Thalvi already shares its status."))
        );
        assert_eq!(ask(&dir).unwrap(), Some("listening".to_owned()));
    }

    #[test]
    fn a_runtime_folder_open_to_other_users_shares_nothing() {
        let dir = Dir::new("open", 0o755);
        let served = serve_in(&dir.0, || "listening".to_owned());
        assert_eq!(
            served.err(),
            Some(unsupported(
                "The runtime folder is open to other users, so the status is not shared."
            ))
        );
        assert!(!dir.socket().exists());
    }

    #[test]
    fn a_reply_is_a_short_word_and_a_silent_app_is_given_up_on() {
        let dir = Dir::new("reply", 0o700);
        let listener = UnixListener::bind(dir.socket()).unwrap();
        let replies = std::thread::spawn(move || {
            let mut held = Vec::new();
            for reply in [vec![b'a'; MAX_REPLY], vec![b'a'; MAX_REPLY + 1], Vec::new()] {
                let (mut connection, _) = listener.accept().unwrap();
                connection.write_all(&reply).unwrap();
                if reply.is_empty() {
                    // Silent but connected: the client must stop waiting by itself.
                    held.push(connection);
                }
            }
            held
        });
        assert_eq!(
            ask(&dir).unwrap(),
            Some("a".repeat(MAX_REPLY)),
            "at the limit"
        );
        assert_eq!(
            ask(&dir),
            Err(unsupported(
                "Thalvi answered with something that is not a state."
            )),
            "just past it"
        );
        assert!(
            matches!(ask(&dir), Err(HostError::Unsupported(text)) if text.starts_with("Thalvi did not answer")),
            "silence"
        );
        drop(replies.join().unwrap());
    }
}
