//! What status bars and hotkeys reach: the state word the running app shares and how a bar shows it.

use serde_json::{Value, json};
use thalvi_desktop::host::HostError;
use tokio::sync::watch;

use crate::status::Status;
use crate::tray::Icon;

/// Every state a bar can show, in the order of the tray icons.
const ICONS: [Icon; 7] = [
    Icon::Paused,
    Icon::Listening,
    Icon::Keyboard,
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
        Icon::Keyboard => (
            "keyboard",
            "Thalvi is listening for the hold keys only. Click to pause.",
        ),
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

/// The state word for a bar: the engine's latest published state, never what was heard.
/// An engine that stopped has no state to confirm, so that reads as an error.
pub fn word(status: &watch::Receiver<Status>) -> &'static str {
    if matches!(*status.borrow(), Status::Loading { .. }) && status.has_changed().is_ok() {
        return "loading";
    }
    let icon = if status.has_changed().is_err() {
        Icon::Error
    } else {
        Icon::of(&status.borrow())
    };
    describe(icon).0
}

/// The line a Waybar custom module reads: `alt` picks the icon and `class` the style. `reply` is
/// the running app's answer to a `status` request.
#[must_use]
pub fn bar_json(reply: Result<Option<String>, HostError>) -> String {
    let unavailable = |tooltip: &str| json!({"text": "", "alt": "unavailable", "class": "unavailable", "tooltip": tooltip});
    let line = match reply {
        Ok(None) => unavailable("Thalvi is not running."),
        Err(error) => unavailable(&error.to_string()),
        Ok(Some(line)) => {
            let reply: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let state = reply["state"].as_str();
            if state == Some("loading") {
                json!({
                    "text": "", "alt": "loading", "class": "loading",
                    "tooltip": "Thalvi is starting. Click to pause."
                })
            } else {
                ICONS
                    .into_iter()
                    .map(describe)
                    .find(|(known, _)| state == Some(*known))
                    .map_or_else(
                        || unavailable("Thalvi answered with a state this version does not know."),
                        |(word, tooltip)| json!({"text": "", "alt": word, "class": word, "tooltip": tooltip}),
                    )
            }
        }
    };
    line.to_string()
}

#[cfg(test)]
mod tests {
    use crate::status::FailureKind;
    use serde_json::json;

    use super::*;

    fn bar(reply: Result<Option<String>, HostError>) -> Value {
        serde_json::from_str(&bar_json(reply)).unwrap()
    }

    /// The bar for what the running app answers a `status` request.
    fn answered(word: &str, copy_error: bool) -> Value {
        bar(Ok(Some(
            json!({"ok": true, "state": word, "copy_error": copy_error}).to_string(),
        )))
    }

    #[test]
    fn the_bar_shows_the_state_the_app_shares() {
        for icon in ICONS {
            let (word, tooltip) = describe(icon);
            assert_eq!(
                answered(word, false),
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
        let state = || word(&receiver);
        assert_eq!(state(), "paused");
        sender.send_replace(Status::Capturing {
            target: Some("kitty".to_owned()),
            committed: "my password is".to_owned(),
            partial: Some("hunter2".to_owned()),
            notice: None,
        });
        assert_eq!(state(), "capturing");
        sender.send_replace(Status::Failed {
            kind: FailureKind::Other,
            notice: "http://user:key@host".to_owned(),
            headline: "Failed",
            causes: Vec::new(),
        });
        assert_eq!(state(), "error");
        sender.send_replace(Status::Loading {
            notice: "private path".to_owned(),
            downloading: false,
        });
        assert_eq!(state(), "loading");
        sender.send_replace(Status::Listening);
        assert_eq!(state(), "listening");
        sender.send_replace(Status::KeyboardOnly);
        assert_eq!(state(), "keyboard");
        drop(sender);
        assert_eq!(state(), "error", "an engine that stopped confirms nothing");
    }

    #[test]
    fn the_bar_ignores_whether_an_error_can_be_copied() {
        for word in ["paused", "loading", "listening", "keyboard"] {
            assert_eq!(answered(word, true), answered(word, false));
        }
    }
}
