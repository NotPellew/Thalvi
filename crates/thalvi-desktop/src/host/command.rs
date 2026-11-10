//! Runs desktop tools as child processes with a time limit; the seam the Linux hosts are tested through.

use std::io::{ErrorKind, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::HostError;

/// How long one tool may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(8);

/// How much of a tool's stderr is kept; the rest is read and dropped so the tool never blocks.
const STDERR_LIMIT: u64 = 4096;

/// How long a failed tool's stderr may take to arrive once the tool has exited.
const STDERR_GRACE: Duration = Duration::from_millis(200);

/// Longest reason shown to the user, in characters.
const REASON_CHARS: usize = 200;

/// Why a tool did not run to success. `Display` is the plain notice for the user.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("Desktop tool {0} is not installed.")]
    NotInstalled(String),
    #[error("Desktop tool {0} took too long.")]
    TimedOut(String),
    #[error("Desktop tool {program} failed{}", reason_suffix(.reason.as_deref()))]
    Failed {
        program: String,
        /// The first line the tool wrote to stderr, when it wrote one.
        reason: Option<String>,
    },
}

fn reason_suffix(reason: Option<&str>) -> String {
    reason.map_or_else(|| ".".to_owned(), |reason| format!(": {reason}."))
}

/// A tool failure before any input was sent: a missing tool means this desktop lacks it.
pub fn tool(error: &CommandError) -> HostError {
    match error {
        CommandError::NotInstalled(_) => HostError::Unsupported(error.to_string()),
        CommandError::TimedOut(_) | CommandError::Failed { .. } => {
            HostError::NothingTyped(error.to_string())
        }
    }
}

/// Puts `text` on the Wayland clipboard; the one place the Linux hosts write it.
///
/// # Errors
/// Why `wl-copy` did not run to success.
pub fn wl_copy(run: &impl CommandRunner, text: &str) -> Result<(), HostError> {
    // An empty copy would clear the clipboard instead of keeping anything.
    if text.is_empty() {
        return Ok(());
    }
    run.run(
        "wl-copy",
        &["--type", "text/plain;charset=utf-8"],
        Some(text),
    )
    .map(|_| ())
    .map_err(|error| tool(&error))
}

/// Runs one desktop tool to completion.
pub trait CommandRunner: Send + Sync {
    /// Runs `program` with `args` and `stdin` as its input, and returns what it wrote to stdout.
    ///
    /// # Errors
    /// Fails when the tool is missing, exits unsuccessfully, or does not finish in time.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, CommandError>;
}

#[cfg(test)]
pub(super) mod fakes;

/// Runs the real tools.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, CommandError> {
        run_within(TIMEOUT, program, args, stdin)
    }
}

fn run_within(
    timeout: Duration,
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<String, CommandError> {
    let failed = |reason| CommandError::Failed {
        program: program.to_owned(),
        reason,
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                CommandError::NotInstalled(program.to_owned())
            } else {
                failed(None)
            }
        })?;
    // The text is written from its own thread so a full pipe cannot outlast the time limit.
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_owned();
        std::thread::spawn(move || pipe.write_all(text.as_bytes()));
    }
    let mut stdout = child.stdout.take().ok_or_else(|| failed(None))?;
    let stderr = child.stderr.take().map(|mut pipe| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = (&mut pipe).take(STDERR_LIMIT).read_to_end(&mut kept);
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            let _ = sender.send(kept);
        });
        receiver
    });
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = String::new();
        let read = stdout.read_to_string(&mut output).map(|_| output);
        let _ = sender.send(read);
    });
    let Ok(read) = receiver.recv_timeout(timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CommandError::TimedOut(program.to_owned()));
    };
    let status = child.wait().map_err(|_| failed(None))?;
    match read {
        Ok(output) if status.success() => Ok(output),
        _ => Err(failed(stderr.and_then(|receiver| reason(&receiver)))),
    }
}

/// The first non-empty stderr line, cut to a length fit for a notice.
fn reason(stderr: &mpsc::Receiver<Vec<u8>>) -> Option<String> {
    let kept = stderr.recv_timeout(STDERR_GRACE).ok()?;
    let text = String::from_utf8_lossy(&kept);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line.trim_end_matches('.');
    Some(line.chars().take(REASON_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn a_tool_gets_its_input_on_stdin_and_returns_its_output() {
        // Far larger than two pipe buffers, so the write cannot finish before the tool is read from.
        let text = "café 👋\n".repeat(400_000);
        assert_eq!(run_within(LONG, "cat", &[], Some(&text)), Ok(text));
        assert_eq!(
            run_within(LONG, "echo", &["-n", "$(not run)"], None),
            Ok("$(not run)".to_owned())
        );
    }

    #[test]
    fn a_failing_tool_says_why_in_its_first_stderr_line_without_blocking_or_leaking_input() {
        let failure = run_within(
            LONG,
            "sh",
            &[
                "-c",
                // More than the kept limit on stderr, so the tool must not block on a full pipe.
                "printf '\\n  snap-confine cannot continue.  \\nsecond line\\n' >&2; \\
                 head -c 200000 /dev/zero | tr '\\0' x >&2; exit 3",
            ],
            Some("secret dictation"),
        )
        .expect_err("the tool exits unsuccessfully");
        assert_eq!(
            failure.to_string(),
            "Desktop tool sh failed: snap-confine cannot continue."
        );
        assert!(!failure.to_string().contains("secret dictation"));
    }

    #[test]
    fn missing_failing_and_stuck_tools_are_told_apart() {
        assert_eq!(
            run_within(LONG, "thalvi-no-such-tool", &[], None),
            Err(CommandError::NotInstalled("thalvi-no-such-tool".to_owned()))
        );
        assert_eq!(
            run_within(LONG, "false", &[], None),
            Err(CommandError::Failed {
                program: "false".to_owned(),
                reason: None,
            })
        );
        assert_eq!(
            run_within(Duration::from_millis(100), "sleep", &["60"], None),
            Err(CommandError::TimedOut("sleep".to_owned()))
        );
    }
}
