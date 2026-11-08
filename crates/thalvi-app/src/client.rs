//! What the command line prints for a request to the running app: its JSON reply, and an exit code.

use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;
use thalvi_desktop::host::{Desktop, HostError, refusal};

use crate::agent::Request;
use crate::control;

/// How long a status bar waits for the state before it shows Thalvi as unavailable.
const BAR_WAIT: Duration = Duration::from_millis(500);

/// How long a command waits for the app, which may wait for a control to show its effect.
const COMMAND_WAIT: Duration = Duration::from_secs(10);

const NOT_RUNNING: &str =
    "Thalvi is not running, so there is nothing to control. Start Thalvi first.";

/// Asks the running app, prints one line and says whether the request was granted.
#[must_use]
pub fn run(request: &Request) -> ExitCode {
    if *request == Request::Status {
        let reply = Desktop::send_request(&request.line(), BAR_WAIT);
        println!("{}", control::bar_json(reply));
        return ExitCode::SUCCESS;
    }
    let (line, granted) = outcome(Desktop::send_request(&request.line(), COMMAND_WAIT));
    println!("{line}");
    if granted {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Whether an app answers a request to open its settings.
#[must_use]
pub fn opened_running_app() -> bool {
    matches!(
        Desktop::send_request(&Request::Show.line(), COMMAND_WAIT),
        Ok(Some(_))
    )
}

/// Whether an app is running at all, for a launch that only wants one to exist.
#[must_use]
pub fn app_runs() -> bool {
    matches!(
        Desktop::send_request(&Request::State.line(), BAR_WAIT),
        Ok(Some(_))
    )
}

/// The line to print, and whether it is a reply that granted the request.
fn outcome(reply: Result<Option<String>, HostError>) -> (String, bool) {
    match reply {
        Ok(Some(line)) => {
            let granted = serde_json::from_str::<Value>(&line)
                .is_ok_and(|reply| reply["ok"] == Value::Bool(true));
            (line, granted)
        }
        Ok(None) => (refusal(NOT_RUNNING), false),
        Err(error) => (refusal(&error.to_string()), false),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_reply_is_printed_as_it_came_and_only_an_ok_one_succeeds() {
        let granted = json!({"ok": true, "took_effect": true}).to_string();
        let refused = json!({"ok": false, "error": "No."}).to_string();
        for (reply, expected) in [
            (Ok(Some(granted.clone())), (granted, true)),
            (Ok(Some(refused.clone())), (refused, false)),
            (
                Ok(Some("not json".to_owned())),
                ("not json".to_owned(), false),
            ),
            (Ok(None), (refusal(NOT_RUNNING), false)),
            (
                Err(HostError::Unsupported(
                    "Thalvi did not answer: timed out.".to_owned(),
                )),
                (refusal("Thalvi did not answer: timed out."), false),
            ),
        ] {
            assert_eq!(outcome(reply.clone()), expected, "{reply:?}");
        }
    }
}
