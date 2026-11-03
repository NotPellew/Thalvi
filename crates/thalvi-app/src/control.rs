//! What status bars and hotkeys reach: the state word the running app shares and how a bar shows it.

use serde_json::json;
use thalvi_desktop::host::HostError;
use tokio::sync::watch;

use crate::status::{LastError, Status};
use crate::tray::Icon;

/// Every state a bar can show, in the order of the tray icons.
const ICONS: [Icon; 6] = [
    Icon::Paused,
    Icon::Listening,
    Icon::Capturing,
    Icon::Pending,
    Icon::Working,
    Icon::Error,
];

/// The state word shared with a bar, and what a click does in that state.
fn describe(icon: Icon) -> (&'static str, &'static str) {
    match icon {
        Icon::Paused => ("paused", "Thalvi is paused. Click to resume."),
        Icon::Listening => ("listening", "Thalvi is listening. Click to pause."),
        Icon::Capturing => (
            "capturing",
            "Thalvi is taking dictation. Click to cancel it and pause.",
        ),
        Icon::Pending => (
            "pending",
            "Thalvi is about to finish a dictation. Click to cancel it and pause.",
        ),
        Icon::Working => ("working", "Thalvi is working. Click to pause."),
        Icon::Error => ("error", "Thalvi hit a problem. Click to toggle listening."),
    }
}

/// Follows the state word when an error can be copied.
const COPY_ERROR: &str = "copy-error";

/// The answer to a bar's question: the engine's latest published state, never what was heard,
/// and whether `copy-error` has a report to copy.
/// An engine that stopped has no state to confirm, so that reads as an error.
pub fn answer(
    status: watch::Receiver<Status>,
    last_error: LastError,
) -> impl Fn() -> String + Send + 'static {
    move || {
        let word =
            if matches!(*status.borrow(), Status::Loading { .. }) && status.has_changed().is_ok() {
                "loading"
            } else {
                let icon = if status.has_changed().is_err() {
                    Icon::Error
                } else {
                    Icon::of(&status.borrow())
                };
                describe(icon).0
            };
        if last_error.get().is_some() {
            format!("{word} {COPY_ERROR}")
        } else {
            word.to_owned()
        }
    }
}

/// Whether `copy-error` has a report to copy, from the running app's answer. No app is left to the
/// caller's "not running" message.
///
/// # Errors
/// The plain reason when the app cannot be asked or has shown no error.
pub fn check_copy_error(reply: Result<Option<String>, HostError>) -> Result<(), String> {
    match reply {
        Err(error) => Err(error.to_string()),
        Ok(Some(line)) if !line.split(' ').any(|word| word == COPY_ERROR) => {
            Err("Thalvi has shown no error to copy.".to_owned())
        }
        Ok(_) => Ok(()),
    }
}

/// The line a Waybar custom module reads: `alt` picks the icon and `class` the style.
#[must_use]
pub fn bar_json(reply: Result<Option<String>, HostError>) -> String {
    let unavailable = |tooltip: &str| json!({"text": "", "alt": "unavailable", "class": "unavailable", "tooltip": tooltip});
    let line = match reply {
        Ok(None) => unavailable("Thalvi is not running."),
        Err(error) => unavailable(&error.to_string()),
        Ok(Some(line)) if line.split(' ').next() == Some("loading") => json!({
            "text": "", "alt": "loading", "class": "loading",
            "tooltip": "Thalvi is starting. Click to pause."
        }),
        Ok(Some(line)) => ICONS
            .into_iter()
            .map(describe)
            .find(|(known, _)| line.split(' ').next() == Some(*known))
            .map_or_else(
                || unavailable("Thalvi answered with a state this version does not know."),
                |(word, tooltip)| json!({"text": "", "alt": word, "class": word, "tooltip": tooltip}),
            ),
    };
    line.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn bar(reply: Result<Option<String>, HostError>) -> Value {
        serde_json::from_str(&bar_json(reply)).unwrap()
    }

    #[test]
    fn the_bar_shows_the_state_the_app_shares() {
        for icon in ICONS {
            let (word, tooltip) = describe(icon);
            assert_eq!(
                bar(Ok(Some(word.to_owned()))),
                json!({"text": "", "alt": word, "class": word, "tooltip": tooltip}),
            );
        }
    }

    #[test]
    fn the_bar_says_unavailable_without_an_app_and_for_answers_it_cannot_use() {
        let unavailable = |tooltip: &str| json!({"text": "", "alt": "unavailable", "class": "unavailable", "tooltip": tooltip});
        let cases = [
            (Ok(None), unavailable("Thalvi is not running.")),
            (
                Ok(Some("dictating: hello".to_owned())),
                unavailable("Thalvi answered with a state this version does not know."),
            ),
            (
                Err(HostError::Unsupported(
                    "Thalvi did not answer: timed out.".to_owned(),
                )),
                unavailable("Thalvi did not answer: timed out."),
            ),
        ];
        for (reply, expected) in cases {
            assert_eq!(bar(reply), expected);
        }
    }

    #[test]
    fn the_shared_state_follows_the_engine_and_never_the_text() {
        let (sender, receiver) = watch::channel(Status::Paused);
        let state = answer(receiver, LastError::default());
        assert_eq!(state(), "paused");
        sender.send_replace(Status::Capturing {
            target: Some("kitty".to_owned()),
            committed: "my password is".to_owned(),
            partial: Some("hunter2".to_owned()),
        });
        assert_eq!(state(), "capturing");
        sender.send_replace(Status::Failed {
            notice: "http://user:key@host".to_owned(),
            headline: "Failed",
            causes: Vec::new(),
        });
        assert_eq!(state(), "error");
        sender.send_replace(Status::Loading {
            notice: "private path".to_owned(),
        });
        assert_eq!(state(), "loading");
        sender.send_replace(Status::Listening);
        assert_eq!(state(), "listening");
        drop(sender);
        assert_eq!(state(), "error", "an engine that stopped confirms nothing");
    }

    #[test]
    fn copy_error_is_offered_from_the_first_failure_on_and_the_bar_ignores_it() {
        let failed = Status::Failed {
            notice: "No.".to_owned(),
            headline: "No",
            causes: Vec::new(),
        };
        let (sender, receiver) = watch::channel(Status::Listening);
        let last = LastError::default();
        let state = answer(receiver, last.clone());
        assert_eq!(state(), "listening");
        last.record(&failed);
        sender.send_replace(Status::Listening);
        assert_eq!(state(), "listening copy-error");
        for word in ["paused", "loading"] {
            assert_eq!(
                bar(Ok(Some(format!("{word} copy-error")))),
                bar(Ok(Some(word.to_owned())))
            );
        }
    }

    #[test]
    fn copy_error_is_refused_before_the_app_is_told_unless_an_error_can_be_copied() {
        let cases = [
            (Ok(Some("listening copy-error".to_owned())), Ok(())),
            (Ok(Some("error copy-error".to_owned())), Ok(())),
            (
                Ok(Some("listening".to_owned())),
                Err("Thalvi has shown no error to copy.".to_owned()),
            ),
            (Ok(None), Ok(())),
            (
                Err(HostError::Unsupported("No status here.".to_owned())),
                Err("No status here.".to_owned()),
            ),
        ];
        for (reply, expected) in cases {
            assert_eq!(check_copy_error(reply.clone()), expected, "{reply:?}");
        }
    }
}
