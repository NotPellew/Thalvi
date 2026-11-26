//! Spoken commands into `tasks::decide`: the first line is what was said, each further line a script.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thalvi_desktop::config::{Script, ScriptAction};
use thalvi_desktop::host::{App, WebUrl};
use thalvi_desktop::tasks::decide;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let (spoken, scripts) = input.split_once('\n').unwrap_or((input, ""));
    let scripts: Vec<Script> = scripts.lines().filter_map(script).collect();
    let _ = decide(spoken, &scripts);
});

/// `open`, `focus` or `message`, a space, then the phrase, normalized as the config does.
fn script(line: &str) -> Option<Script> {
    let (kind, phrase) = line.split_once(' ')?;
    let phrase = phrase
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if phrase.is_empty() {
        return None;
    }
    let action = match kind {
        "open" => ScriptAction::OpenUrl(WebUrl::parse("https://example.com").ok()?),
        "focus" => ScriptAction::FocusApp {
            name: "app".to_owned(),
            app: App {
                classes: vec!["app".to_owned()],
                executables: Vec::new(),
                shortcut: None,
            },
        },
        "message" => ScriptAction::DiscordMessage {
            destination: "https://discord.com/channels/@me/42".to_owned(),
        },
        _ => return None,
    };
    Some(Script {
        label: phrase.clone(),
        phrase,
        action,
    })
}
