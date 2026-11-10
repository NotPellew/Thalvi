//! What a launch of the app asks for: a question or control for the running app, or the app itself.

use std::io;
use std::path::PathBuf;

use serde_json::Value;

use crate::agent::{Catalog, Request};
use crate::startup::LOGIN_ARG;

const USAGE: &str = "Usage: thalvi-desktop [toggle | pause | resume | cancel | hold-start | hold-end | write-start | write-end | copy-error | state | status | show [page] | quit | feed <wav-file> | models | model-download <wake|speech> <name> | speech-check <json> | word-list-push | settings get | settings check <json> | settings set <json> | metrics summary | config-file]";

/// The command line after the program name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Ask the running app and print its reply; never starts the app, the microphone or a window.
    Ask(Request),
    /// Open the app, or its settings window when it already runs.
    Open {
        /// What the login entry asks for: start in the tray, and leave a running app as it is.
        login: bool,
        /// A config file to use instead of the per-user one.
        config: Option<PathBuf>,
    },
}

impl Launch {
    /// `stdin` is read when a settings change is given as `-`.
    ///
    /// # Errors
    /// The usage line, or why the settings change is no JSON.
    pub fn parse(
        args: &[String],
        stdin: impl FnOnce() -> io::Result<String>,
    ) -> Result<Self, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let open = |login, config| Ok(Self::Open { login, config });
        match args.as_slice() {
            [] => open(false, None),
            ["status"] => Ok(Self::Ask(Request::Status)),
            ["state"] => Ok(Self::Ask(Request::State)),
            ["toggle"] => Ok(Self::Ask(Request::Toggle)),
            ["pause"] => Ok(Self::Ask(Request::Pause)),
            ["resume"] => Ok(Self::Ask(Request::Resume)),
            ["cancel"] => Ok(Self::Ask(Request::Cancel)),
            ["hold-start"] => Ok(Self::Ask(Request::HoldStart)),
            ["hold-end"] => Ok(Self::Ask(Request::HoldEnd)),
            ["write-start"] => Ok(Self::Ask(Request::WriteStart)),
            ["write-end"] => Ok(Self::Ask(Request::WriteEnd)),
            ["copy-error"] => Ok(Self::Ask(Request::CopyError)),
            ["settings", "get"] => Ok(Self::Ask(Request::SettingsGet)),
            ["settings", "check", change] => Ok(Self::Ask(Request::SettingsCheck {
                settings: change_of(change, stdin)?,
            })),
            ["settings", "set", change] => Ok(Self::Ask(Request::SettingsSet {
                settings: change_of(change, stdin)?,
            })),
            ["metrics", "summary"] => Ok(Self::Ask(Request::MetricsSummary)),
            ["show"] => Ok(Self::Ask(Request::Show { page: None })),
            ["show", page] => Ok(Self::Ask(Request::Show {
                page: Some((*page).to_owned()),
            })),
            ["quit"] => Ok(Self::Ask(Request::Quit)),
            ["models"] => Ok(Self::Ask(Request::Models)),
            ["word-list-push"] => Ok(Self::Ask(Request::WordListPush)),
            ["feed", file] => Ok(Self::Ask(Request::Feed {
                // The running app has its own working folder.
                path: std::path::absolute(file)
                    .map_err(|error| format!("The audio file's path could not be read: {error}."))?
                    .to_string_lossy()
                    .into_owned(),
            })),
            ["model-download", kind, name] => {
                let kind = match *kind {
                    "wake" => Catalog::Wake,
                    "speech" => Catalog::Speech,
                    _ => return Err(USAGE.to_owned()),
                };
                Ok(Self::Ask(Request::ModelDownload {
                    kind,
                    name: (*name).to_owned(),
                }))
            }
            ["speech-check", entry] => Ok(Self::Ask(Request::SpeechCheck {
                entry: change_of(entry, stdin)?,
            })),
            [LOGIN_ARG] => open(true, None),
            [path]
                if !matches!(
                    *path,
                    "settings" | "metrics" | "feed" | "model-download" | "speech-check"
                ) =>
            {
                open(false, Some(PathBuf::from(path)))
            }
            _ => Err(USAGE.to_owned()),
        }
    }
}

/// The JSON change given on the command line, or on standard input for `-`.
fn change_of(argument: &str, stdin: impl FnOnce() -> io::Result<String>) -> Result<Value, String> {
    let text = if argument == "-" {
        stdin().map_err(|error| format!("The settings change could not be read: {error}."))?
    } else {
        argument.to_owned()
    };
    serde_json::from_str(&text).map_err(|_| "The settings change is not valid JSON.".to_owned())
}

/// Whether a second launch, with its full command line, asks the running app to open its settings.
///
/// A line that cannot be read still opens the settings.
#[must_use]
pub fn relaunch_shows_settings(args: &[String]) -> bool {
    !matches!(
        Launch::parse(args.get(1..).unwrap_or_default(), || Ok(String::new())),
        Ok(Launch::Open { login: true, .. } | Launch::Ask(_))
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(args: &[&str]) -> Result<Launch, String> {
        Launch::parse(
            &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
            || Ok(r#"{"look": "holo"}"#.to_owned()),
        )
    }

    #[test]
    fn a_launch_names_a_request_for_the_running_app_or_opens_the_app() {
        let open = |login, config: Option<&str>| {
            Ok(Launch::Open {
                login,
                config: config.map(PathBuf::from),
            })
        };
        let ask = |request| Ok(Launch::Ask(request));
        for (args, expected) in [
            (&[][..], open(false, None)),
            (&["--login"][..], open(true, None)),
            (&["desktop.json"][..], open(false, Some("desktop.json"))),
            (&["toggle"][..], ask(Request::Toggle)),
            (&["pause"][..], ask(Request::Pause)),
            (&["resume"][..], ask(Request::Resume)),
            (&["cancel"][..], ask(Request::Cancel)),
            (&["hold-start"][..], ask(Request::HoldStart)),
            (&["hold-end"][..], ask(Request::HoldEnd)),
            (&["write-start"][..], ask(Request::WriteStart)),
            (&["write-end"][..], ask(Request::WriteEnd)),
            (&["copy-error"][..], ask(Request::CopyError)),
            (&["status"][..], ask(Request::Status)),
            (&["state"][..], ask(Request::State)),
            (&["settings", "get"][..], ask(Request::SettingsGet)),
            (&["metrics", "summary"][..], ask(Request::MetricsSummary)),
            (
                &["settings", "check", r#"{"look":"neon"}"#][..],
                ask(Request::SettingsCheck {
                    settings: json!({"look": "neon"}),
                }),
            ),
            (
                &["settings", "set", r#"{"look":"neon"}"#][..],
                ask(Request::SettingsSet {
                    settings: json!({"look": "neon"}),
                }),
            ),
            (
                &["settings", "set", "-"][..],
                ask(Request::SettingsSet {
                    settings: json!({"look": "holo"}),
                }),
            ),
            (
                &["settings", "set", "{nope"][..],
                Err("The settings change is not valid JSON.".to_owned()),
            ),
            (&["a", "b"][..], Err(USAGE.to_owned())),
            (&["toggle", "x"][..], Err(USAGE.to_owned())),
            (&["settings"][..], Err(USAGE.to_owned())),
            (&["settings", "get", "x"][..], Err(USAGE.to_owned())),
            (&["metrics"][..], Err(USAGE.to_owned())),
        ] {
            assert_eq!(parse(args), expected, "{args:?}");
        }
    }

    #[test]
    fn a_launch_names_the_commands_that_feed_audio_and_set_the_app_up() {
        let ask = |request| Ok(Launch::Ask(request));
        let absolute = std::env::temp_dir()
            .join("hello.wav")
            .to_string_lossy()
            .into_owned();
        for (args, expected) in [
            (&["show"][..], ask(Request::Show { page: None })),
            (
                &["show", "speech"][..],
                ask(Request::Show {
                    page: Some("speech".to_owned()),
                }),
            ),
            (&["quit"][..], ask(Request::Quit)),
            (&["models"][..], ask(Request::Models)),
            (&["word-list-push"][..], ask(Request::WordListPush)),
            (
                &["feed", &absolute][..],
                ask(Request::Feed {
                    path: absolute.clone(),
                }),
            ),
            (
                &["feed", "hello.wav"][..],
                ask(Request::Feed {
                    path: std::path::absolute("hello.wav")
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                }),
            ),
            (
                &["model-download", "speech", "nemotron_160ms"][..],
                ask(Request::ModelDownload {
                    kind: Catalog::Speech,
                    name: "nemotron_160ms".to_owned(),
                }),
            ),
            (
                &["model-download", "wake", "hey_thalvi"][..],
                ask(Request::ModelDownload {
                    kind: Catalog::Wake,
                    name: "hey_thalvi".to_owned(),
                }),
            ),
            (
                &["speech-check", r#"{"kind":"realtime"}"#][..],
                ask(Request::SpeechCheck {
                    entry: json!({"kind": "realtime"}),
                }),
            ),
            (&["model-download", "other", "x"][..], Err(USAGE.to_owned())),
            (&["model-download", "wake"][..], Err(USAGE.to_owned())),
            (&["feed"][..], Err(USAGE.to_owned())),
            (&["quit", "now"][..], Err(USAGE.to_owned())),
        ] {
            assert_eq!(parse(args), expected, "{args:?}");
        }
    }

    #[test]
    fn a_relaunch_opens_settings_unless_it_is_the_login_entry_or_a_request() {
        let shows = |args: &[&str]| {
            relaunch_shows_settings(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        };
        for (args, expected) in [
            (&["thalvi-desktop"][..], true),
            (&["thalvi-desktop", "desktop.json"][..], true),
            (&["thalvi-desktop", "--login"][..], false),
            (&["thalvi-desktop", "toggle"][..], false),
            (&["thalvi-desktop", "a", "b"][..], true),
            (&[][..], true),
        ] {
            assert_eq!(shows(args), expected, "{args:?}");
        }
    }
}
