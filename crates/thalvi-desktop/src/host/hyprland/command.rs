//! Runs desktop tools as child processes with a time limit; the seam the Hyprland host is tested through.

use std::io::{ErrorKind, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long one tool may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(8);

/// Why a tool did not run to success. `Display` is the plain notice for the user.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("Desktop tool {0} is not installed.")]
    NotInstalled(String),
    #[error("Desktop tool {0} took too long.")]
    TimedOut(String),
    #[error("Desktop tool {0} failed.")]
    Failed(String),
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
    let failed = || CommandError::Failed(program.to_owned());
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                CommandError::NotInstalled(program.to_owned())
            } else {
                failed()
            }
        })?;
    // The text is written from its own thread so a full pipe cannot outlast the time limit.
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_owned();
        std::thread::spawn(move || pipe.write_all(text.as_bytes()));
    }
    let mut stdout = child.stdout.take().ok_or_else(failed)?;
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
    let status = child.wait().map_err(|_| failed())?;
    match read {
        Ok(output) if status.success() => Ok(output),
        _ => Err(failed()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn a_tool_gets_its_input_on_stdin_and_returns_its_output() {
        // Larger than a pipe buffer, so the write cannot finish before the tool reads.
        let text = "café 👋\n".repeat(30_000);
        assert_eq!(run_within(LONG, "cat", &[], Some(&text)), Ok(text));
        assert_eq!(
            run_within(LONG, "echo", &["-n", "$(not run)"], None),
            Ok("$(not run)".to_owned())
        );
    }

    #[test]
    fn missing_failing_and_stuck_tools_are_told_apart() {
        assert_eq!(
            run_within(LONG, "thalvi-no-such-tool", &[], None),
            Err(CommandError::NotInstalled("thalvi-no-such-tool".to_owned()))
        );
        assert_eq!(
            run_within(LONG, "false", &[], None),
            Err(CommandError::Failed("false".to_owned()))
        );
        assert_eq!(
            run_within(Duration::from_millis(100), "sleep", &["60"], None),
            Err(CommandError::TimedOut("sleep".to_owned()))
        );
    }
}
