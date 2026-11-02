//! Scripted task decisions and their execution through the desktop host.

use crate::config::{DesktopConfig, ScriptAction};
use crate::host::{App, Host, HostError, WebUrl};

/// What the task ending asked for, as a typed action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Task {
    OpenUrl {
        url: WebUrl,
        label: String,
    },
    FocusApp {
        app: App,
        label: String,
    },
    DiscordMessage {
        destination: String,
        message: String,
        label: String,
    },
}

/// Why no task was chosen. `Display` is the plain notice for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("That task isn't configured yet.")]
pub struct NotConfigured;

/// The refusal for every Discord message: live Discord actions are switched off.
const DISCORD_OFF: &str = "Discord messages are switched off for now. Nothing was typed or sent.";

/// Chooses the task `text` asks for: a configured phrase, else `open <host>`.
///
/// # Errors
/// [`NotConfigured`] when the text matches no script and is not a valid `open` request.
pub fn decide(text: &str, config: &DesktopConfig) -> Result<Task, NotConfigured> {
    let spoken = normalize(text);
    for script in &config.scripts {
        match &script.action {
            ScriptAction::DiscordMessage { destination } => {
                let prefix = format!("{} ", script.phrase);
                if let Some(rest) = strip_prefix_ignore_case(text, &prefix)
                    && !rest.trim().is_empty()
                {
                    return Ok(Task::DiscordMessage {
                        destination: destination.clone(),
                        message: rest.trim().to_owned(),
                        label: script.label.clone(),
                    });
                }
            }
            ScriptAction::OpenUrl(url) if spoken == script.phrase => {
                return Ok(Task::OpenUrl {
                    url: url.clone(),
                    label: script.label.clone(),
                });
            }
            ScriptAction::FocusApp { app, .. } if spoken == script.phrase => {
                return Ok(Task::FocusApp {
                    app: app.clone(),
                    label: script.label.clone(),
                });
            }
            ScriptAction::OpenUrl(_) | ScriptAction::FocusApp { .. } => {}
        }
    }
    spoken_url(text).ok_or(NotConfigured)
}

/// Runs `task` on the desktop and returns the notice to show.
///
/// # Errors
/// Why the host could not do it; Discord messages are always refused, before any host call.
pub fn execute(task: &Task, host: &impl Host) -> Result<String, HostError> {
    match task {
        Task::DiscordMessage { .. } => Err(HostError::Unsupported(DISCORD_OFF.to_owned())),
        Task::OpenUrl { url, label } => {
            host.open_url(url)?;
            Ok(format!("Asked the browser to open {label}."))
        }
        Task::FocusApp { app, label } => {
            host.focus_app(app)?;
            Ok(format!("Focused {label}."))
        }
    }
}

/// Lowercase, whitespace collapsed, without sentence punctuation around it.
fn normalize(text: &str) -> String {
    text.to_lowercase()
        .trim_matches([' ', '.', '!', '?', '\t', '\r', '\n'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The rest of `text` after `prefix`, matched ignoring case. `prefix` is already lowercase.
fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let mut wanted = prefix.chars();
    for (index, character) in text.char_indices() {
        for lower in character.to_lowercase() {
            match wanted.next() {
                Some(expected) if expected == lower => {}
                _ => return None,
            }
        }
        if wanted.as_str().is_empty() {
            return Some(&text[index + character.len_utf8()..]);
        }
    }
    None
}

/// `open <host>` with a plain web address: letters, digits and hyphens in dotted labels.
fn spoken_url(text: &str) -> Option<Task> {
    let text = text.trim();
    let rest = text
        .get(4..)
        .filter(|_| text[..4].eq_ignore_ascii_case("open"))?;
    let value = rest.trim_start();
    if value.len() == rest.len() {
        return None;
    }
    // Recognizers end the sentence before "do that" with punctuation no host ends with.
    let value = value.trim_end_matches(['.', ',', '!', '?']);
    if value.is_empty() {
        return None;
    }
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    if candidate
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
        || has_bad_percent(&candidate)
    {
        return None;
    }
    let authority = candidate
        .split_once("://")?
        .1
        .split(['/', '?', '#'])
        .next()?;
    let parsed = reqwest::Url::parse(&candidate).ok()?;
    let host = parsed.host_str()?;
    let plain_host = authority.is_ascii()
        && !authority.contains('%')
        && host.contains('.')
        && host.split('.').all(|label| {
            let bytes = label.as_bytes();
            bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        });
    if !plain_host
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port() == Some(0)
    {
        return None;
    }
    Some(Task::OpenUrl {
        url: WebUrl::parse(&candidate).ok()?,
        label: value.to_owned(),
    })
}

fn has_bad_percent(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| {
        *byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::fakes::FakeHost;

    fn config() -> DesktopConfig {
        DesktopConfig::from_json_str(
            r#"{
                "apps": {"editor": {"classes": ["Code"], "input_keys": ["ctrl", "l"]}},
                "scripts": [
                    {"phrase": "Open  the project page", "action": "open_url",
                     "destination": "https://github.com/NotPellew/Thalvi", "label": "the project page"},
                    {"phrase": "switch to editor", "action": "focus_app", "destination": "editor"},
                    {"phrase": "message the demo channel saying", "action": "discord_message",
                     "destination": "https://discord.com/channels/1/2", "label": "Demo"}
                ]
            }"#,
            None,
        )
        .unwrap()
    }

    fn open(url: &str, label: &str) -> Task {
        Task::OpenUrl {
            url: WebUrl::parse(url).unwrap(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn configured_phrases_match_ignoring_case_spacing_and_sentence_punctuation() {
        let config = config();
        for spoken in [
            "open the project page",
            "  Open the PROJECT   page. ",
            "open the project page?!",
        ] {
            assert_eq!(
                decide(spoken, &config),
                Ok(open(
                    "https://github.com/NotPellew/Thalvi",
                    "the project page"
                )),
                "{spoken}"
            );
        }
        let focus = decide("Switch to Editor.", &config).unwrap();
        let Task::FocusApp { app, label } = focus else {
            panic!("expected a focus task");
        };
        assert_eq!(
            (app.classes, label.as_str()),
            (vec!["Code".to_owned()], "switch to editor")
        );
    }

    #[test]
    fn spoken_open_requests_follow_the_url_rule() {
        let config = config();
        for (spoken, expected) in [
            (
                "open google.com",
                Some(("https://google.com/", "google.com")),
            ),
            (
                "OPEN https://Example.com/Case?q=Value",
                Some((
                    "https://example.com/Case?q=Value",
                    "https://Example.com/Case?q=Value",
                )),
            ),
            (
                "open http://127.0.0.1:8080/x",
                Some(("http://127.0.0.1:8080/x", "http://127.0.0.1:8080/x")),
            ),
            (
                "open sub-domain.example.co.uk/a%20b",
                Some((
                    "https://sub-domain.example.co.uk/a%20b",
                    "sub-domain.example.co.uk/a%20b",
                )),
            ),
            ("open javascript:alert(1)", None),
            ("open https://user@example.com", None),
            ("open https://user:pw@example.com", None),
            ("open example.com extra", None),
            ("open example.com;$(bad)", None),
            ("open https://example.com:bad", None),
            ("open https://example.com:0", None),
            ("open https://example.com/%zz", None),
            ("open https://example.com/%4", None),
            ("open https://example.com\\evil", None),
            ("open https://ex%41mple.com", None),
            ("open https://exämple.com", None),
            ("open ftp://example.com", None),
            ("open localhost", None),
            (
                "Open google.com.",
                Some(("https://google.com/", "google.com")),
            ),
            (
                "open example.com/a?",
                Some(("https://example.com/a", "example.com/a")),
            ),
            ("open .", None),
            ("open -example.com", None),
            ("open exa..mple.com", None),
            ("open [::1]", None),
            ("open", None),
            ("opengoogle.com", None),
            ("opens example.com", None),
            ("open\u{7f}example.com", None),
        ] {
            let expected = expected.map(|(url, label)| open(url, label));
            assert_eq!(decide(spoken, &config).ok(), expected, "{spoken:?}");
        }
    }

    #[test]
    fn discord_phrases_are_prefixes_whose_rest_is_the_message() {
        let config = config();
        let message = |text: &str| Task::DiscordMessage {
            destination: "https://discord.com/channels/1/2".to_owned(),
            message: text.to_owned(),
            label: "Demo".to_owned(),
        };
        assert_eq!(
            decide("Message the demo channel saying  Hello There ", &config),
            Ok(message("Hello There"))
        );
        for spoken in [
            "message the demo channel saying",
            "message the demo channel saying   ",
            "message the demo channel",
        ] {
            assert_eq!(decide(spoken, &config), Err(NotConfigured), "{spoken:?}");
        }
    }

    #[test]
    fn execute_asks_the_host_and_refuses_discord_before_any_host_call() {
        let host = FakeHost::default();
        assert_eq!(
            execute(&open("https://example.com/", "example.com"), &host).as_deref(),
            Ok("Asked the browser to open example.com.")
        );
        let focus = decide("switch to editor", &config()).unwrap();
        assert_eq!(
            execute(&focus, &host).as_deref(),
            Ok("Focused switch to editor.")
        );
        let discord = decide("message the demo channel saying hi", &config()).unwrap();
        assert!(matches!(
            execute(&discord, &host),
            Err(HostError::Unsupported(_))
        ));
        assert_eq!(
            host.calls(),
            ["open_url:https://example.com/", "focus_app:Code"]
        );
    }
}
