//! Desktop configuration, validated once at load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::capture::{CaptureLimits, EndingPauses, Endings};
use crate::cues::Cues;
use crate::entries::{Entries, EntryError, RawStt};
use crate::history::HistoryConfig;
use crate::hold::HoldKey;
use crate::host::{App, Desktop, Modifier, Shortcut, WebUrl};
use crate::metrics::RecordKinds;
use crate::phrases::WordList;
use crate::recognizer::{ServerUrl, SpeechSource};
use crate::wake::WakeConfig;
use crate::writing::Writing;

/// Why the desktop config could not be used. Messages are shown to the user as written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("The desktop config {path} could not be read: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("The desktop config is not valid: {0}")]
    Invalid(#[from] serde_json::Error),
    #[error(
        "The desktop config at {path} is not valid: {source}. Fix or remove the file, then start Thalvi again."
    )]
    InvalidFile {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// What the desktop app is configured to do. Every setting is optional in the file and has
/// the default shown in `Default` of its type; unknown settings are rejected.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawDesktopConfig")]
pub struct DesktopConfig {
    pub wake: WakeConfig,
    pub cues: Cues,
    pub ending_pause: EndingPauses,
    pub capture_limits: CaptureLimits,
    pub endings: Endings,
    pub stt: SttConfig,
    pub apps: BTreeMap<String, App>,
    /// Phrases the task ending runs, in file order.
    pub scripts: Vec<Script>,
    pub overlay: OverlayConfig,
    /// The input device to open; `None` is the system default.
    pub microphone: Option<String>,
    pub look: Look,
    /// What is recorded on this computer; nothing unless the user turns it on.
    pub recordings: RecordKinds,
    /// The dictation history kept on this computer; on unless the user turns it off.
    pub history: HistoryConfig,
    /// Whether the user wants Thalvi to start at login. The app restores the OS entry from this
    /// on start, because a Windows upgrade removes it.
    pub start_at_login: bool,
    /// Whether the user wants Thalvi in the app menu. The app rewrites the `AppImage`'s menu
    /// entry from this on start, because the `AppImage` may have moved.
    pub show_in_menu: bool,
    /// The key held to dictate, as in effect: the desktop's default unless the config names another.
    pub hold_key: HoldKey,
    /// The key held to dictate and have the words written out as proper text, as in effect like
    /// `hold_key`.
    pub write_key: HoldKey,
    /// The chat endpoint that writes dictation out; without one, asking for it says where to set it up.
    pub writing: Option<Writing>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDesktopConfig {
    /// The address a config written before the speech server list kept; read only to migrate it.
    #[serde(default)]
    server: Option<ServerUrl>,
    #[serde(default)]
    wake: WakeConfig,
    #[serde(default, deserialize_with = "cues")]
    cues: Cues,
    #[serde(default)]
    ending_pause: EndingPauses,
    #[serde(default)]
    capture_limits: CaptureLimits,
    #[serde(default)]
    endings: Endings,
    #[serde(default)]
    stt: RawStt,
    #[serde(default)]
    apps: BTreeMap<String, RawApp>,
    #[serde(default)]
    scripts: Vec<RawScript>,
    #[serde(default)]
    overlay: OverlayConfig,
    #[serde(default)]
    microphone: Option<String>,
    #[serde(default)]
    look: Look,
    #[serde(default)]
    recordings: RecordKinds,
    #[serde(default)]
    history: HistoryConfig,
    #[serde(default)]
    start_at_login: bool,
    #[serde(default)]
    show_in_menu: bool,
    #[serde(default)]
    hold_key: Option<HoldKey>,
    #[serde(default)]
    write_key: Option<HoldKey>,
    #[serde(default)]
    writing: Option<Writing>,
}

impl DesktopConfig {
    /// The active live entry, as errors and notices name it.
    #[must_use]
    pub fn speech_source(&self) -> SpeechSource {
        self.stt.entries.active_live().source()
    }
}

/// How the overlay and the settings window look; `Gunmetal` unless the user picks another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", rename_all = "lowercase")]
pub enum Look {
    Neon,
    Holo,
    #[default]
    Gunmetal,
}

impl TryFrom<String> for Look {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        match name.as_str() {
            "neon" => Ok(Self::Neon),
            "holo" => Ok(Self::Holo),
            "gunmetal" => Ok(Self::Gunmetal),
            _ => Err("look must be gunmetal, holo or neon.".to_owned()),
        }
    }
}

/// What the status overlay shows; both are on by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayConfig {
    /// Show the overlay while a capture is under way.
    #[serde(default = "on")]
    pub show: bool,
    /// Show the dictated text in the overlay.
    #[serde(default = "on")]
    pub draft: bool,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            show: true,
            draft: true,
        }
    }
}

/// The hold and write keys the running desktop starts with.
type KeyDefaults = (HoldKey, HoldKey);

/// The key in effect for a stored one: the default when none is stored, or when it is the value
/// every config stored before defaults were set per desktop.
fn in_effect(
    stored: Option<HoldKey>,
    formerly_always_stored: &HoldKey,
    default: &HoldKey,
) -> HoldKey {
    stored
        .filter(|key| key != formerly_always_stored)
        .unwrap_or_else(|| default.clone())
}

/// The key to store: none when it is the default, which a config then follows if it changes.
fn to_store<'a>(key: &'a HoldKey, default: &HoldKey) -> Option<&'a HoldKey> {
    (key != default).then_some(key)
}

impl TryFrom<RawDesktopConfig> for DesktopConfig {
    type Error = String;

    fn try_from(raw: RawDesktopConfig) -> Result<Self, String> {
        build(raw, &Desktop::default_hold_keys())
    }
}

fn build(
    raw: RawDesktopConfig,
    (hold_default, write_default): &KeyDefaults,
) -> Result<DesktopConfig, String> {
    let hold_key = in_effect(raw.hold_key, &HoldKey::alt_z(), hold_default);
    let write_key = in_effect(raw.write_key, &HoldKey::alt_x(), write_default);
    let apps = raw
        .apps
        .into_iter()
        .map(|(name, app)| app.validate(&name).map(|app| (name, app)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut scripts: Vec<Script> = Vec::new();
    for script in raw.scripts {
        let script = script.validate(&apps)?;
        if scripts.iter().any(|known| known.phrase == script.phrase) {
            return Err("Script phrases must be distinct.".to_owned());
        }
        scripts.push(script);
    }
    let microphone = raw
        .microphone
        .map(|name| nonempty(&name, "microphone"))
        .transpose()?;
    if write_key == hold_key {
        return Err("The write key must differ from the hold key.".to_owned());
    }
    let (entries, phrases) = raw
        .stt
        .entries(raw.server)
        .map_err(|error: EntryError| error.to_string())?;
    Ok(DesktopConfig {
        wake: raw.wake,
        cues: raw.cues,
        ending_pause: raw.ending_pause,
        capture_limits: raw.capture_limits,
        endings: raw.endings,
        stt: SttConfig { entries, phrases },
        apps,
        scripts,
        overlay: raw.overlay,
        microphone,
        look: raw.look,
        recordings: raw.recordings,
        history: raw.history,
        start_at_login: raw.start_at_login,
        show_in_menu: raw.show_in_menu,
        hold_key,
        write_key,
        writing: raw.writing,
    })
}

/// A spoken phrase and the action it runs, as configured under `scripts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    /// Lowercase with single spaces; a spoken task matches it after the same normalizing.
    pub phrase: String,
    pub action: ScriptAction,
    /// What notices call the action.
    pub label: String,
}

/// What a script does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptAction {
    OpenUrl(WebUrl),
    FocusApp {
        name: String,
        app: App,
    },
    /// The phrase is a prefix: the words after it are the message. Normalized channel URL.
    DiscordMessage {
        destination: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    #[serde(default)]
    classes: Vec<String>,
    #[serde(default)]
    executables: Vec<String>,
    #[serde(default)]
    input_keys: Vec<String>,
}

impl RawApp {
    fn validate(self, name: &str) -> Result<App, String> {
        if self.classes.is_empty() && self.executables.is_empty() {
            return Err(format!("apps.{name} needs window classes or executables."));
        }
        let names = |values: Vec<String>, what: &str| {
            values
                .iter()
                .map(|value| nonempty(value, &format!("apps.{name} {what}")))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(App {
            classes: names(self.classes, "window class")?,
            executables: names(self.executables, "executable")?,
            shortcut: shortcut(&self.input_keys, name)?,
        })
    }
}

fn shortcut(keys: &[String], app: &str) -> Result<Option<Shortcut>, String> {
    let keys = keys
        .iter()
        .map(|key| nonempty(key, &format!("apps.{app} key")))
        .collect::<Result<Vec<_>, _>>()?;
    let Some((key, modifiers)) = keys.split_last() else {
        return Ok(None);
    };
    let modifiers = modifiers
        .iter()
        .map(|modifier| match modifier.to_lowercase().as_str() {
            "ctrl" => Some(Modifier::Ctrl),
            "shift" => Some(Modifier::Shift),
            "alt" => Some(Modifier::Alt),
            "logo" => Some(Modifier::Logo),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    match modifiers {
        Some(modifiers) if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            Ok(Some(Shortcut {
                modifiers,
                key: key.clone(),
            }))
        }
        _ => Err("Input keys must be modifiers followed by one named key.".to_owned()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScript {
    phrase: String,
    action: RawAction,
    destination: String,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawAction {
    OpenUrl,
    FocusApp,
    DiscordMessage,
}

impl RawScript {
    fn validate(self, apps: &BTreeMap<String, App>) -> Result<Script, String> {
        let phrase = nonempty(&self.phrase, "script phrase")?
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let destination = nonempty(&self.destination, "script destination")?;
        let action = match self.action {
            RawAction::OpenUrl => {
                let invalid = || {
                    "A script URL must be an http or https address with a host and no credentials."
                        .to_owned()
                };
                let parsed = reqwest::Url::parse(&destination).map_err(|_| invalid())?;
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(invalid());
                }
                ScriptAction::OpenUrl(WebUrl::parse(&destination).map_err(|_| invalid())?)
            }
            RawAction::FocusApp => ScriptAction::FocusApp {
                app: apps
                    .get(&destination)
                    .cloned()
                    .ok_or_else(|| format!("No app named {destination:?} is configured."))?,
                name: destination,
            },
            RawAction::DiscordMessage => ScriptAction::DiscordMessage {
                destination: discord_channel(&destination)?,
            },
        };
        let label = match self.label {
            Some(label) => nonempty(&label, "script label")?,
            None => phrase.clone(),
        };
        Ok(Script {
            phrase,
            action,
            label,
        })
    }
}

fn discord_channel(destination: &str) -> Result<String, String> {
    let invalid = || "Discord destination must be a Discord channel or DM URL.".to_owned();
    let url = reqwest::Url::parse(destination).map_err(|_| invalid())?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let segments: Vec<&str> = url.path_segments().ok_or_else(invalid)?.collect();
    let ["channels", server, channel] = segments[..] else {
        return Err(invalid());
    };
    if !matches!(url.scheme(), "https" | "discord")
        || url.host_str() != Some("discord.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(server == "@me" || digits(server))
        || !digits(channel)
    {
        return Err(invalid());
    }
    Ok(format!("https://discord.com/channels/{server}/{channel}"))
}

fn nonempty(value: &str, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} must be nonempty text."));
    }
    Ok(value.to_owned())
}

/// Speech-to-text settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SttConfig {
    /// The speech servers: the live one dictation uses, and the second passes that may replace its text.
    pub entries: Entries,
    /// Spellings recognition favours, on this computer and on a recognizer that boosts phrases.
    pub phrases: WordList,
}

impl Serialize for SttConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct FileStt<'a> {
            entries: &'a Entries,
            #[serde(skip_serializing_if = "WordList::is_empty")]
            phrases: &'a WordList,
        }
        FileStt {
            entries: &self.entries,
            phrases: &self.phrases,
        }
        .serialize(serializer)
    }
}

/// A config in the form of its file; `record` is never part of it.
#[derive(Serialize)]
struct FileForm<'a> {
    wake: &'a WakeConfig,
    cues: &'a Cues,
    ending_pause: &'a EndingPauses,
    capture_limits: &'a CaptureLimits,
    endings: &'a Endings,
    stt: &'a SttConfig,
    apps: BTreeMap<&'a str, FileApp<'a>>,
    scripts: Vec<FileScript<'a>>,
    overlay: &'a OverlayConfig,
    microphone: Option<&'a str>,
    look: Look,
    recordings: RecordKinds,
    history: HistoryConfig,
    start_at_login: bool,
    show_in_menu: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hold_key: Option<&'a HoldKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    write_key: Option<&'a HoldKey>,
    writing: &'a Option<Writing>,
}

#[derive(Serialize)]
struct FileApp<'a> {
    classes: &'a [String],
    executables: &'a [String],
    input_keys: Vec<String>,
}

impl<'a> From<&'a App> for FileApp<'a> {
    fn from(app: &'a App) -> Self {
        let keys = app.shortcut.iter().flat_map(|shortcut| {
            shortcut
                .modifiers
                .iter()
                .map(|modifier| {
                    match modifier {
                        Modifier::Ctrl => "ctrl",
                        Modifier::Shift => "shift",
                        Modifier::Alt => "alt",
                        Modifier::Logo => "logo",
                    }
                    .to_owned()
                })
                .chain(std::iter::once(shortcut.key.clone()))
        });
        Self {
            classes: &app.classes,
            executables: &app.executables,
            input_keys: keys.collect(),
        }
    }
}

#[derive(Serialize)]
struct FileScript<'a> {
    phrase: &'a str,
    action: &'static str,
    destination: &'a str,
    label: &'a str,
}

impl Serialize for DesktopConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.serialize_for(serializer, &Desktop::default_hold_keys())
    }
}

impl DesktopConfig {
    /// The file form for a desktop whose default keys are `defaults`: the hold and write keys are
    /// in it only when they differ.
    fn serialize_for<S: Serializer>(
        &self,
        serializer: S,
        (hold_default, write_default): &KeyDefaults,
    ) -> Result<S::Ok, S::Error> {
        let scripts = self
            .scripts
            .iter()
            .map(|script| {
                let (action, destination) = match &script.action {
                    ScriptAction::OpenUrl(url) => ("open_url", url.as_str()),
                    ScriptAction::FocusApp { name, app } => {
                        if self.apps.get(name) != Some(app) {
                            return Err(serde::ser::Error::custom("script app is not configured"));
                        }
                        ("focus_app", name.as_str())
                    }
                    ScriptAction::DiscordMessage { destination } => {
                        ("discord_message", destination.as_str())
                    }
                };
                Ok(FileScript {
                    phrase: &script.phrase,
                    action,
                    destination,
                    label: &script.label,
                })
            })
            .collect::<Result<Vec<_>, S::Error>>()?;
        FileForm {
            wake: &self.wake,
            cues: &self.cues,
            ending_pause: &self.ending_pause,
            capture_limits: &self.capture_limits,
            endings: &self.endings,
            stt: &self.stt,
            apps: self
                .apps
                .iter()
                .map(|(name, app)| (name.as_str(), FileApp::from(app)))
                .collect(),
            scripts,
            overlay: &self.overlay,
            microphone: self.microphone.as_deref(),
            look: self.look,
            recordings: self.recordings,
            history: self.history,
            start_at_login: self.start_at_login,
            show_in_menu: self.show_in_menu,
            hold_key: to_store(&self.hold_key, hold_default),
            write_key: to_store(&self.write_key, write_default),
            writing: &self.writing,
        }
        .serialize(serializer)
    }
}

impl DesktopConfig {
    /// Reads and validates the config at `path`.
    ///
    /// # Errors
    /// Fails when the file cannot be read or a setting is unknown or out of range.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let json = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_str(&json).map_err(|source| ConfigError::InvalidFile {
            path: path.to_owned(),
            source,
        })
    }

    /// Validates config `json`.
    ///
    /// # Errors
    /// Fails when a setting is unknown or out of range.
    pub fn from_json_str(json: &str) -> Result<Self, ConfigError> {
        Ok(serde_json::from_str(json)?)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per cue, named as in the config"
)]
struct RawCues {
    #[serde(default = "on")]
    wake: bool,
    #[serde(default = "on")]
    provisional: bool,
    #[serde(default = "on")]
    cancelled: bool,
    #[serde(default = "on")]
    accepted: bool,
    #[serde(default = "on")]
    error: bool,
}

fn on() -> bool {
    true
}

fn cues<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Cues, D::Error> {
    let raw = RawCues::deserialize(deserializer)?;
    Ok(Cues {
        wake: raw.wake,
        provisional: raw.provisional,
        cancelled: raw.cancelled,
        accepted: raw.accepted,
        error: raw.error,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::capture::{CaptureRoute, Ending};
    use crate::cues::Cue;
    use crate::entries::{LiveKind, SecondPassEntry};
    use crate::models::{CatalogHead, CatalogSpeech};
    use crate::secondary::{SecondaryApi, SecondaryStt};
    use crate::transducer::SpeechChoice;
    use crate::wake::HeadModel;
    use crate::writing::DEFAULT_PROMPT;

    fn parse(json: &str) -> Result<DesktopConfig, ConfigError> {
        DesktopConfig::from_json_str(json)
    }

    #[test]
    fn an_empty_config_uses_the_defaults() {
        let config = parse("{}").unwrap();
        assert_eq!(config.stt, SttConfig::default());
        assert_eq!(
            config.stt.entries.active_live().kind,
            LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms))
        );
        assert_eq!(config.stt.entries.second_pass(), []);
        assert_eq!(config.wake, WakeConfig::default());
        assert_eq!(config.cues, Cues::default());
        assert_eq!(config.ending_pause, EndingPauses::default());
        assert_eq!(config.capture_limits, CaptureLimits::default());
        assert_eq!(config.endings, Endings::default());
        assert_eq!(config.recordings, RecordKinds::default());
        assert_eq!(
            (config.history.enabled(), config.history.limit()),
            (true, 200)
        );
        assert!(!config.start_at_login);
        assert!(!config.show_in_menu);
        assert_eq!(
            (config.hold_key, config.write_key),
            Desktop::default_hold_keys()
        );
        assert_eq!(config.writing, None);
    }

    const WINDOWS: fn() -> KeyDefaults = || (HoldKey::alt_z(), HoldKey::alt_x());
    const HYPRLAND: fn() -> KeyDefaults = || (HoldKey::f9(), HoldKey::f10());

    /// The config `json` as a desktop with `defaults` loads it.
    fn loaded(json: &str, defaults: &KeyDefaults) -> Result<DesktopConfig, String> {
        build(
            serde_json::from_str(json).map_err(|error| error.to_string())?,
            defaults,
        )
    }

    #[test]
    fn the_write_key_is_checked_once_and_must_differ_from_the_hold_key() {
        let key = |json: &str| loaded(json, &WINDOWS()).map(|config| config.write_key.to_string());
        assert_eq!(key(r#"{"write_key": "Ctrl+KeyY"}"#).unwrap(), "Ctrl+KeyY");
        let error = key(r#"{"write_key": "KeyX"}"#).unwrap_err();
        assert!(error.contains("needs a modifier"), "{error}");
        for json in [
            r#"{"write_key": "Alt+KeyZ"}"#,
            r#"{"hold_key": "Alt+KeyX"}"#,
            r#"{"hold_key": "F9", "write_key": "F9"}"#,
        ] {
            let error = key(json).unwrap_err();
            assert!(
                error.contains("must differ from the hold key"),
                "{json}: {error}"
            );
        }
        assert_eq!(
            key(r#"{"hold_key": "Alt+KeyX", "write_key": "Alt+KeyZ"}"#).unwrap(),
            "Alt+KeyZ",
            "the keys may swap places"
        );
        let error = loaded(r#"{"hold_key": "F10"}"#, &HYPRLAND()).unwrap_err();
        assert!(error.contains("must differ from the hold key"), "{error}");
    }

    #[test]
    fn the_hold_keys_are_the_desktops_defaults_and_are_stored_only_when_they_differ() {
        // (desktop, stored in the file, in effect, stored again)
        type Case = (
            fn() -> KeyDefaults,
            &'static str,
            (&'static str, &'static str),
            (Option<&'static str>, Option<&'static str>),
        );
        let cases: [Case; 9] = [
            (HYPRLAND, "{}", ("F9", "F10"), (None, None)),
            (WINDOWS, "{}", ("Alt+KeyZ", "Alt+KeyX"), (None, None)),
            (
                HYPRLAND,
                r#"{"hold_key": "Alt+KeyZ", "write_key": "Alt+KeyX"}"#,
                ("F9", "F10"),
                (None, None),
            ),
            (
                WINDOWS,
                r#"{"hold_key": "Alt+KeyZ", "write_key": "Alt+KeyX"}"#,
                ("Alt+KeyZ", "Alt+KeyX"),
                (None, None),
            ),
            (
                HYPRLAND,
                r#"{"hold_key": "F9", "write_key": "F10"}"#,
                ("F9", "F10"),
                (None, None),
            ),
            (
                HYPRLAND,
                r#"{"hold_key": "F8"}"#,
                ("F8", "F10"),
                (Some("F8"), None),
            ),
            (
                WINDOWS,
                r#"{"write_key": "F10"}"#,
                ("Alt+KeyZ", "F10"),
                (None, Some("F10")),
            ),
            (
                WINDOWS,
                r#"{"hold_key": "F9", "write_key": "Ctrl+F10"}"#,
                ("F9", "Ctrl+F10"),
                (Some("F9"), Some("Ctrl+F10")),
            ),
            (
                HYPRLAND,
                r#"{"hold_key": null, "write_key": "F11"}"#,
                ("F9", "F11"),
                (None, Some("F11")),
            ),
        ];
        for (desktop, json, in_effect, stored) in cases {
            let defaults = desktop();
            let config = loaded(json, &defaults).unwrap();
            assert_eq!(
                (config.hold_key.to_string(), config.write_key.to_string()),
                (in_effect.0.to_owned(), in_effect.1.to_owned()),
                "{json}"
            );
            let written = config
                .serialize_for(serde_json::value::Serializer, &defaults)
                .unwrap();
            assert_eq!(
                (
                    written.get("hold_key").and_then(|key| key.as_str()),
                    written.get("write_key").and_then(|key| key.as_str())
                ),
                stored,
                "{json}"
            );
            assert_eq!(loaded(&written.to_string(), &defaults).unwrap(), config);
        }
    }

    #[test]
    fn the_writing_endpoint_is_absent_by_default_and_refused_when_it_cannot_be_used() {
        let writing = |json: &str| parse(json).map(|config| config.writing);
        assert_eq!(writing(r#"{"writing": null}"#).unwrap(), None);
        let endpoint = writing(r#"{"writing": {"url": "http://127.0.0.1:9/v1/", "model": " m "}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(endpoint.model(), "m");
        assert_eq!(
            endpoint.prompt(),
            DEFAULT_PROMPT,
            "no prompt means the default"
        );
        for (json, expected) in [
            (
                r#"{"writing": {"url": "http://x", "model": "m", "prompt": " \n"}}"#,
                "writing.prompt must not be blank",
            ),
            (
                r#"{"writing": {"url": "ftp://x", "model": "m"}}"#,
                "writing.url must be",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": " "}}"#,
                "writing.model must be",
            ),
            (
                r#"{"writing": {"url": "http://x"}}"#,
                "missing field `model`",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "tone": 1}}"#,
                "unknown field `tone`",
            ),
        ] {
            let error = writing(json).unwrap_err().to_string();
            assert!(error.contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn a_writing_prompt_is_kept_only_when_it_differs_from_the_default() {
        let stored = |prompt: &str| {
            let json = json!({"writing": {"url": "http://x", "model": "m", "prompt": prompt}});
            let config = parse(&json.to_string()).unwrap();
            let written = serde_json::to_value(&config).unwrap();
            assert_eq!(parse(&written.to_string()).unwrap(), config);
            (
                config.writing.unwrap().prompt().to_owned(),
                written["writing"].get("prompt").cloned(),
            )
        };
        assert_eq!(stored(DEFAULT_PROMPT), (DEFAULT_PROMPT.to_owned(), None));
        assert_eq!(
            stored("  Be brief.\n"),
            ("Be brief.".to_owned(), Some(json!("Be brief.")))
        );
    }

    #[test]
    fn the_hold_key_is_checked_once_when_the_config_loads() {
        let key = |json: &str| loaded(json, &WINDOWS()).map(|config| config.hold_key.to_string());
        assert_eq!(key(r#"{"hold_key": "Ctrl+KeyY"}"#).unwrap(), "Ctrl+KeyY");
        let error = key(r#"{"hold_key": "KeyZ"}"#).unwrap_err();
        assert!(error.contains("needs a modifier"), "{error}");
        let error = key(r#"{"hold_key": "Alt+Hyper"}"#).unwrap_err();
        assert!(error.contains("not a key name"), "{error}");
    }

    #[test]
    fn secondary_is_off_by_default_and_round_trips_off_on_and_default() {
        let endpoint =
            r#""url": "http://127.0.0.1:9/v1", "model": "m", "api_key": "k", "api": "meta""#;
        for (json, expected) in [
            ("{}".to_owned(), None),
            (
                format!(r#"{{"stt": {{"secondary": {{{endpoint}}}}}}}"#),
                Some(false),
            ),
            (
                format!(r#"{{"stt": {{"secondary": {{"enabled": false, {endpoint}}}}}}}"#),
                Some(false),
            ),
            (
                format!(r#"{{"stt": {{"secondary": {{"enabled": true, {endpoint}}}}}}}"#),
                Some(true),
            ),
        ] {
            let config = parse(&json).unwrap();
            assert_eq!(
                config
                    .stt
                    .entries
                    .second_pass()
                    .first()
                    .map(SecondPassEntry::ticked),
                expected,
                "{json}"
            );
            let written = serde_json::to_string(&config).unwrap();
            let again = parse(&written).unwrap();
            assert_eq!(again.stt.entries, config.stt.entries, "{written}");
            if expected == Some(false) {
                assert!(written.contains(r#""active":false"#), "{written}");
                assert!(written.contains("http://127.0.0.1:9/v1"), "{written}");
                assert!(written.contains(r#""api_key":"k""#), "{written}");
            }
            if expected == Some(true) {
                assert!(written.contains(r#""active":true"#), "{written}");
            }
        }
        let off_blank = parse(
            r#"{"stt": {"secondary": {"enabled": false, "url": "", "model": "", "api_key": "k"}}}"#,
        )
        .unwrap();
        assert_eq!(
            off_blank
                .stt
                .entries
                .second_pass()
                .first()
                .map(SecondPassEntry::ticked),
            Some(false)
        );
        let on_blank =
            parse(r#"{"stt": {"secondary": {"enabled": true, "url": "", "model": ""}}}"#)
                .unwrap_err()
                .to_string();
        assert!(on_blank.contains("stt.secondary.url"), "{on_blank}");
        for (json, expected) in [
            (
                r#"{"stt": {"secondary": {"enabled": true, "url": "ftp://x", "model": "m"}}}"#,
                "stt.secondary.url must be an http or https URL",
            ),
            (
                r#"{"stt": {"secondary": {"enabled": true, "url": "http://x", "model": " "}}}"#,
                "stt.secondary.model must be nonempty",
            ),
        ] {
            let error = parse(json).unwrap_err().to_string();
            assert!(error.contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn dictation_is_recognized_on_the_server_unless_a_local_model_is_chosen() {
        for (json, kind) in [
            (
                r#"{"stt": {"engine": "server"}}"#,
                LiveKind::ThalviServer(ServerUrl::default()),
            ),
            (
                r#"{"stt": {"engine": "local"}}"#,
                LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms)),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "nemotron_160ms"}}"#,
                LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms)),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "unified_160ms"}}"#,
                LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::Unified160ms)),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "parakeet_tdt_v3"}}"#,
                LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::ParakeetTdtV3)),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "/models/mine"}}"#,
                LiveKind::OnDevice(SpeechChoice::Folder("/models/mine".into())),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "C:\\models\\mine"}}"#,
                LiveKind::OnDevice(SpeechChoice::Folder("C:\\models\\mine".into())),
            ),
        ] {
            assert_eq!(
                parse(json).unwrap().stt.entries.active_live().kind,
                kind,
                "{json}"
            );
        }
    }

    #[test]
    fn the_word_list_round_trips_trimmed_and_a_refused_one_names_the_limit() {
        let config = parse(r#"{"stt": {"phrases": [" Cybrex2 ", "Thalvi"]}}"#).unwrap();
        assert_eq!(config.stt.phrases.phrases(), ["Cybrex2", "Thalvi"]);
        let written = serde_json::to_string(&config).unwrap();
        assert_eq!(parse(&written).unwrap().stt.phrases, config.stt.phrases);
        assert!(
            !serde_json::to_string(&parse("{}").unwrap())
                .unwrap()
                .contains("phrases")
        );
        let error = parse(r#"{"stt": {"phrases": [""]}}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("1 to 40 characters"), "{error}");
    }

    #[test]
    fn every_setting_is_read() {
        let config = DesktopConfig::from_json_str(
            r#"{
                "server": "wss://thalvi.example:9000/",
                "stt": {"secondary": {"url": "http://127.0.0.1:9/v1/", "model": " m ", "api": "meta"}},
                "wake": {"capture": {"model": "hey_jarvis", "threshold": 0.7}},
                "cues": {"wake": false, "error": false},
                "ending_pause": {"paste": 1, "submit": 3},
                "capture_limits": {"idle": 90},
                "endings": {
                    "Send It": "submit", "paste": "paste", "write it": "write", "do it": "task",
                    "scratch": "scratch", "cancel": "cancel"
                },
                "recordings": {"metrics": true, "audio": true}
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.stt.entries.active_live().kind,
            LiveKind::ThalviServer(ServerUrl::parse("wss://thalvi.example:9000").unwrap())
        );
        assert_eq!(
            config
                .stt
                .entries
                .second_pass()
                .first()
                .map(|entry| &entry.settings),
            Some(
                &SecondaryStt::new(
                    "http://127.0.0.1:9/v1",
                    "m",
                    String::new(),
                    SecondaryApi::Meta,
                    false
                )
                .unwrap()
            )
        );
        assert_eq!(
            config.wake.capture().model,
            HeadModel::Catalog(CatalogHead::HeyJarvis)
        );
        assert!((config.wake.capture().threshold.get() - 0.7).abs() < f32::EPSILON);
        assert_eq!(
            config.cues,
            Cues {
                wake: false,
                error: false,
                ..Cues::default()
            }
        );
        assert_eq!(
            [
                CaptureRoute::Paste,
                CaptureRoute::Submit,
                CaptureRoute::Task
            ]
            .map(|route| config.ending_pause.get(route)),
            [
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_millis(2500)
            ]
        );
        assert_eq!(
            [config.capture_limits.idle(), config.capture_limits.max()],
            [Duration::from_secs(90), Duration::from_secs(300)]
        );
        assert_eq!(
            config.endings,
            Endings::try_from(BTreeMap::from([
                ("send it".to_owned(), Ending::Submit),
                ("paste".to_owned(), Ending::Paste),
                ("write it".to_owned(), Ending::Write),
                ("do it".to_owned(), Ending::Task),
                ("scratch".to_owned(), Ending::Scratch),
                ("cancel".to_owned(), Ending::Cancel),
            ]))
            .unwrap()
        );
        assert_eq!(
            config.recordings,
            RecordKinds {
                metrics: true,
                text: false,
                audio: true
            }
        );
    }

    #[test]
    fn each_cue_switch_turns_off_only_its_own_cue() {
        let all = [
            Cue::Wake,
            Cue::Provisional,
            Cue::Cancelled,
            Cue::Accepted,
            Cue::Error,
        ];
        for (key, off) in [
            ("wake", Cue::Wake),
            ("provisional", Cue::Provisional),
            ("cancelled", Cue::Cancelled),
            ("accepted", Cue::Accepted),
            ("error", Cue::Error),
        ] {
            let config = parse(&format!(r#"{{"cues": {{"{key}": false}}}}"#)).unwrap();
            for cue in all {
                assert_eq!(config.cues.enabled(cue), cue != off, "{key} {cue:?}");
            }
        }
    }

    fn script(fields: &str) -> String {
        format!(r#"{{"apps": {{"t3": {{"classes": ["a"]}}}}, "scripts": [{{{fields}}}]}}"#)
    }

    #[test]
    fn saving_a_script_preserves_its_named_app_when_two_apps_have_the_same_settings() {
        let config = parse(
            r#"{"apps": {"a": {"classes": ["terminal"]}, "b": {"classes": ["terminal"]}},
                "scripts": [{"phrase": "switch", "action": "focus_app", "destination": "b"}]}"#,
        )
        .unwrap();
        let mut saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["scripts"][0]["destination"], "b");
        saved["apps"]["a"]["classes"] = serde_json::json!(["other-terminal"]);
        let reloaded = parse(&saved.to_string()).unwrap();
        assert_eq!(reloaded.scripts, config.scripts);
    }

    #[test]
    fn apps_and_scripts_are_read_into_typed_actions() {
        let config = parse(
            r#"{
                "apps": {
                    "t3": {"classes": [" T3 Code "], "executables": ["T3 Code.exe"],
                           "input_keys": ["Ctrl", "SHIFT", "k"]},
                    "term": {"executables": ["wt.exe"]}
                },
                "scripts": [
                    {"phrase": "  Switch   To T3 ", "action": "focus_app", "destination": " t3 ",
                     "label": " T3 Code "},
                    {"phrase": "project", "action": "open_url",
                     "destination": " https://github.com/NotPellew/Thalvi "},
                    {"phrase": "message the demo saying", "action": "discord_message",
                     "destination": "discord://discord.com/channels/@me/42"}
                ]
            }"#,
        )
        .unwrap();
        let t3 = App {
            classes: vec!["T3 Code".to_owned()],
            executables: vec!["T3 Code.exe".to_owned()],
            shortcut: Some(Shortcut {
                modifiers: vec![Modifier::Ctrl, Modifier::Shift],
                key: "k".to_owned(),
            }),
        };
        assert_eq!(
            config.scripts,
            [
                Script {
                    phrase: "switch to t3".to_owned(),
                    action: ScriptAction::FocusApp {
                        name: "t3".to_owned(),
                        app: t3
                    },
                    label: "T3 Code".to_owned(),
                },
                Script {
                    phrase: "project".to_owned(),
                    action: ScriptAction::OpenUrl(
                        WebUrl::parse("https://github.com/NotPellew/Thalvi").unwrap()
                    ),
                    label: "project".to_owned(),
                },
                Script {
                    phrase: "message the demo saying".to_owned(),
                    action: ScriptAction::DiscordMessage {
                        destination: "https://discord.com/channels/@me/42".to_owned()
                    },
                    label: "message the demo saying".to_owned(),
                },
            ]
        );
        assert_eq!(parse("{}").unwrap().scripts, []);
    }

    #[test]
    fn an_app_without_input_keys_has_no_shortcut_and_one_key_needs_no_modifier() {
        for (keys, expected) in [
            ("[]", None),
            (
                r#"["Return"]"#,
                Some(Shortcut {
                    modifiers: Vec::new(),
                    key: "Return".to_owned(),
                }),
            ),
        ] {
            let config = parse(&format!(
                r#"{{"apps": {{"a": {{"classes": ["a"], "input_keys": {keys}}}}},
                    "scripts": [{{"phrase": "x", "action": "focus_app", "destination": "a"}}]}}"#
            ))
            .unwrap();
            assert_eq!(
                config.scripts[0].action,
                ScriptAction::FocusApp {
                    name: "a".to_owned(),
                    app: App {
                        classes: vec!["a".to_owned()],
                        executables: Vec::new(),
                        shortcut: expected,
                    },
                }
            );
        }
    }

    #[test]
    fn discord_destinations_must_be_channel_or_dm_urls() {
        for (destination, normalized) in [
            (
                "https://discord.com/channels/123/456",
                Some("https://discord.com/channels/123/456"),
            ),
            (
                "discord://discord.com/channels/@me/456",
                Some("https://discord.com/channels/@me/456"),
            ),
            ("https://discord.com/channels/123/456?x=1", None),
            ("https://discord.com/channels/123/456#top", None),
            ("https://discord.com/channels/123/456/789", None),
            ("https://discord.com/channels/abc/456", None),
            ("https://discord.com/channels/@me/", None),
            ("https://example.com/channels/123/456", None),
            ("https://discord.com:8443/channels/123/456", None),
            ("https://u@discord.com/channels/123/456", None),
            ("http://discord.com/channels/123/456", None),
            ("not a url", None),
        ] {
            let result = parse(&script(&format!(
                r#""phrase": "x", "action": "discord_message", "destination": "{destination}""#
            )));
            match normalized {
                Some(normalized) => assert_eq!(
                    result.unwrap().scripts[0].action,
                    ScriptAction::DiscordMessage {
                        destination: normalized.to_owned()
                    },
                    "{destination}"
                ),
                None => assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("Discord destination must be a Discord channel or DM URL."),
                    "{destination}"
                ),
            }
        }
    }

    #[test]
    fn speech_recognition_settings_that_cannot_work_are_rejected_in_plain_words() {
        for (json, expected) in [
            (
                r#"{"stt": {"model": "nemotron_160ms"}}"#,
                "stt.model only applies when stt.engine is local.",
            ),
            (
                r#"{"stt": {"engine": "local", "model": "whisper"}}"#,
                "Unknown speech model \"whisper\". Use nemotron_160ms, unified_160ms, parakeet_tdt_v3 or the path of a folder",
            ),
            (r#"{"stt": {"engine": "cloud"}}"#, "unknown variant `cloud`"),
        ] {
            let error = parse(json).unwrap_err();
            assert!(error.to_string().contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn invalid_settings_are_rejected_in_plain_words() {
        let valid_endings = r#""paste": "paste", "write": "write", "send": "submit", "do it": "task",
            "scratch": "scratch", "cancel": "cancel""#;
        for (json, expected) in [
            (r#"{"discord": {}}"#.to_owned(), "unknown field `discord`"),
            (
                r#"{"server": "http://127.0.0.1:8080"}"#.to_owned(),
                "server must be a ws or wss URL",
            ),
            (
                r#"{"server": "ws://127.0.0.1:8080/session"}"#.to_owned(),
                "server root",
            ),
            (
                r#"{"stt": {"primary": {}}}"#.to_owned(),
                "unknown field `primary`",
            ),
            (
                r#"{"stt": {"secondary": {"url": "http://x", "model": "m", "api": "azure"}}}"#
                    .to_owned(),
                "unknown variant `azure`",
            ),
            (
                r#"{"metrics": "m.jsonl"}"#.to_owned(),
                "unknown field `metrics`",
            ),
            (
                r#"{"record": ["audio"]}"#.to_owned(),
                "unknown field `record`",
            ),
            (
                r#"{"wake": {"capture_model": "x.onnx"}}"#.to_owned(),
                "unknown field `capture_model`",
            ),
            (
                r#"{"wake": {"capture": {"model": "hey_mycroft"}}}"#.to_owned(),
                "wake models must be different",
            ),
            (
                r#"{"wake": {"conversation": {"model": "hey_jarvis", "threshold": 0}}}"#.to_owned(),
                "A wake threshold must be above 0",
            ),
            (r#"{"cues": {"wake": 1}}"#.to_owned(), "invalid type"),
            (r#"{"cues": {"wake": null}}"#.to_owned(), "invalid type"),
            (
                r#"{"cues": {"chime": true}}"#.to_owned(),
                "unknown field `chime`",
            ),
            (
                r#"{"ending_pause": {"submit": 5.5}}"#.to_owned(),
                "submit pause must be a number of seconds from 0 to 5",
            ),
            (
                r#"{"ending_pause": {"paste": -1}}"#.to_owned(),
                "paste pause must be a number of seconds from 0 to 5",
            ),
            (r#"{"ending_pause": 2.5}"#.to_owned(), "invalid type"),
            (r#"{"ending_pause": true}"#.to_owned(), "invalid type"),
            (
                r#"{"capture_limits": {"idle": 9}}"#.to_owned(),
                "idle limit must be a whole number of seconds from 10 to 600",
            ),
            (
                r#"{"capture_limits": {"max": 3601}}"#.to_owned(),
                "max limit must be a whole number of seconds from 60 to 3600",
            ),
            (r#"{"capture_limits": 60}"#.to_owned(), "invalid type"),
            (
                r#"{"capture_limits": {"min": 1}}"#.to_owned(),
                "unknown field `min`",
            ),
            (
                r#"{"endings": {"enter": "submit"}}"#.to_owned(),
                "Configure paste, write, submit, task, scratch, and cancel",
            ),
            (
                format!(r#"{{"endings": {{{valid_endings}, "Do  It": "task"}}}}"#),
                "Ending phrases must be distinct",
            ),
            (
                format!(r#"{{"endings": {{{valid_endings}, " ": "task"}}}}"#),
                "must not be empty",
            ),
            (
                format!(r#"{{"endings": {{{valid_endings}, "x": "enter"}}}}"#),
                "unknown variant `enter`",
            ),
        ] {
            let error = parse(&json).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{json}");
            assert!(error.to_string().contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn invalid_apps_and_scripts_are_rejected_in_plain_words() {
        for (json, expected) in [
            (
                r#"{"apps": {"t3": {"classes": ["a"], "keys": []}}}"#.to_owned(),
                "unknown field `keys`",
            ),
            (
                r#"{"apps": {"t3": {}}}"#.to_owned(),
                "apps.t3 needs window classes or executables.",
            ),
            (
                r#"{"apps": {"t3": {"classes": [" "]}}}"#.to_owned(),
                "apps.t3 window class must be nonempty text.",
            ),
            (
                r#"{"apps": {"t3": {"executables": [""]}}}"#.to_owned(),
                "apps.t3 executable must be nonempty text.",
            ),
            (
                r#"{"apps": {"t3": {"classes": ["a"], "input_keys": ["ctrl", " "]}}}"#.to_owned(),
                "apps.t3 key must be nonempty text.",
            ),
            (
                r#"{"apps": {"t3": {"classes": ["a"], "input_keys": ["meta", "l"]}}}"#.to_owned(),
                "Input keys must be modifiers followed by one named key.",
            ),
            (
                r#"{"apps": {"t3": {"classes": ["a"], "input_keys": ["ctrl", "k l"]}}}"#.to_owned(),
                "Input keys must be modifiers followed by one named key.",
            ),
            (
                r#"{"apps": {"t3": {"classes": ["a"], "input_keys": ["ctrl", "-"]}}}"#.to_owned(),
                "Input keys must be modifiers followed by one named key.",
            ),
            (
                script(r#""phrase": "x", "action": "open_url", "destination": "ftp://x.com""#),
                "A script URL must be an http or https address",
            ),
            (
                script(
                    r#""phrase": "x", "action": "open_url", "destination": "https://u:p@x.com""#,
                ),
                "A script URL must be an http or https address",
            ),
            (
                script(r#""phrase": "x", "action": "focus_app", "destination": "nope""#),
                "No app named \"nope\" is configured.",
            ),
            (
                script(r#""phrase": "x", "action": "run", "destination": "x""#),
                "unknown variant `run`",
            ),
            (
                script(r#""phrase": " ", "action": "open_url", "destination": "https://x.com""#),
                "script phrase must be nonempty text.",
            ),
            (
                script(
                    r#""phrase": "x", "action": "open_url", "destination": "https://x.com", "label": " ""#,
                ),
                "script label must be nonempty text.",
            ),
            (
                script(
                    r#""phrase": "x", "action": "open_url", "destination": "https://x.com", "color": 1"#,
                ),
                "unknown field `color`",
            ),
            (
                script(r#""phrase": "x", "action": "open_url""#),
                "missing field `destination`",
            ),
            (
                r#"{"scripts": [
                    {"phrase": "Open  Site", "action": "open_url", "destination": "https://x.com"},
                    {"phrase": "open site", "action": "open_url", "destination": "https://y.com"}
                ]}"#
                .to_owned(),
                "Script phrases must be distinct.",
            ),
        ] {
            let error = parse(&json).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{json}");
            assert!(error.to_string().contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn recordings_are_off_unless_the_file_turns_them_on_and_name_only_known_kinds() {
        for (json, expected) in [
            ("{}", Ok((false, false, false))),
            (r#"{"recordings": {}}"#, Ok((false, false, false))),
            (
                r#"{"recordings": {"text": true}}"#,
                Ok((false, true, false)),
            ),
            (
                r#"{"recordings": {"metrics": true, "text": true, "audio": true}}"#,
                Ok((true, true, true)),
            ),
            (r#"{"recordings": {"speech": true}}"#, Err("speech")),
            (r#"{"recordings": {"audio": "yes"}}"#, Err("boolean")),
        ] {
            let result = parse(json).map(|config| {
                (
                    config.recordings.metrics,
                    config.recordings.text,
                    config.recordings.audio,
                )
            });
            match (result, expected) {
                (Ok(kinds), Ok(expected)) => assert_eq!(kinds, expected, "{json}"),
                (Err(error), Err(expected)) => {
                    assert!(error.to_string().contains(expected), "{json}: {error}");
                }
                (other, expected) => panic!("{json}: {other:?} vs {expected:?}"),
            }
        }
    }

    #[test]
    fn history_is_on_with_200_entries_unless_the_file_says_otherwise() {
        for (json, expected) in [
            ("{}", (true, 200)),
            (r#"{"history": {}}"#, (true, 200)),
            (r#"{"history": {"enabled": false}}"#, (false, 200)),
            (r#"{"history": {"limit": 1}}"#, (true, 1)),
            (
                r#"{"history": {"enabled": false, "limit": 1000}}"#,
                (false, 1000),
            ),
        ] {
            let config = parse(json).unwrap();
            assert_eq!(
                (config.history.enabled(), config.history.limit()),
                expected,
                "{json}"
            );
            let written = serde_json::to_string(&config).unwrap();
            assert!(
                written.contains(&format!(
                    r#""history":{{"enabled":{},"limit":{}}}"#,
                    expected.0, expected.1
                )),
                "{written}"
            );
        }
    }

    #[test]
    fn history_settings_that_cannot_work_are_rejected_in_plain_words() {
        let limit = "history limit must be a whole number of entries from 1 to 1000";
        for (json, expected) in [
            (r#"{"history": {"limit": 0}}"#, limit),
            (r#"{"history": {"limit": 1001}}"#, limit),
            (r#"{"history": {"limit": -1}}"#, "invalid value"),
            (r#"{"history": {"enabled": "yes"}}"#, "boolean"),
            (r#"{"history": {"audio": true}}"#, "unknown field `audio`"),
        ] {
            let error = parse(json).unwrap_err();
            assert!(matches!(error, ConfigError::Invalid(_)), "{json}");
            assert!(error.to_string().contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn the_config_serializes_to_the_file_form_it_was_read_from() {
        let full = r#"{
            "server": "wss://thalvi.example:9000/",
            "stt": {"engine": "local", "model": "/models/speech",
                    "secondary": {"enabled": true, "url": "http://127.0.0.1:9/v1/", "model": "m", "api_key": "k", "api": "meta"}},
            "wake": {"capture": {"model": "/models/me.onnx", "threshold": 0.7},
                     "conversation": {"model": "hey_jarvis"}},
            "cues": {"wake": false, "error": false},
            "ending_pause": {"paste": 1, "submit": 3, "task": 2.5},
            "capture_limits": {"idle": 90, "max": 120},
            "endings": {"Send It": "submit", "paste": "paste", "write it": "write", "do it": "task",
                        "scratch": "scratch", "cancel": "cancel"},
            "apps": {"editor": {"classes": ["Code"], "executables": ["Code.exe"],
                                "input_keys": ["ctrl", "Shift", "p"]},
                     "term": {"classes": ["kitty"]}},
            "scripts": [
                {"phrase": "open site", "action": "open_url", "destination": "https://y.com/a"},
                {"phrase": "switch to editor", "action": "focus_app", "destination": "editor",
                 "label": "the editor"},
                {"phrase": "message demo saying", "action": "discord_message",
                 "destination": "https://discord.com/channels/1/2"}
            ],
            "overlay": {"show": false, "draft": true},
            "microphone": " USB Mic ",
            "look": "gunmetal",
            "recordings": {"metrics": true, "text": false, "audio": true},
            "history": {"enabled": false, "limit": 50},
            "start_at_login": true,
            "show_in_menu": true,
            "hold_key": "Ctrl+Shift+F9",
            "write_key": "Ctrl+F10",
            "writing": {"url": "http://127.0.0.1:9/v1", "model": "m", "api_key": "k"}
        }"#;
        for json in ["{}", full] {
            let config = parse(json).unwrap();
            let written = serde_json::to_string(&config).unwrap();
            assert_eq!(parse(&written).unwrap(), config, "{written}");
        }
    }

    #[test]
    fn overlay_and_microphone_default_and_are_validated() {
        let config = parse("{}").unwrap();
        assert_eq!(
            config.overlay,
            OverlayConfig {
                show: true,
                draft: true
            }
        );
        assert_eq!(config.microphone, None);

        let config = parse(r#"{"overlay": {"draft": false}, "microphone": " USB Mic "}"#).unwrap();
        assert_eq!(
            config.overlay,
            OverlayConfig {
                show: true,
                draft: false
            }
        );
        assert_eq!(config.microphone.as_deref(), Some("USB Mic"));

        for (json, expected) in [
            (
                r#"{"microphone": "  "}"#,
                "microphone must be nonempty text.",
            ),
            (r#"{"overlay": {"show": "yes"}}"#, "invalid type"),
            (r#"{"overlay": {"size": 3}}"#, "unknown field `size`"),
        ] {
            let error = parse(json).unwrap_err();
            assert!(error.to_string().contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn look_defaults_to_gunmetal_and_rejects_unknown_names() {
        assert_eq!(parse("{}").unwrap().look, Look::Gunmetal);
        for (name, look) in [
            ("neon", Look::Neon),
            ("holo", Look::Holo),
            ("gunmetal", Look::Gunmetal),
        ] {
            let config = parse(&format!(r#"{{"look": "{name}"}}"#)).unwrap();
            assert_eq!(config.look, look);
        }
        for json in [r#"{"look": "classic"}"#, r#"{"look": "Holo"}"#] {
            let error = parse(json).unwrap_err().to_string();
            assert!(
                error.contains("look must be gunmetal, holo or neon."),
                "{json}: {error}"
            );
        }
    }

    #[test]
    fn load_reads_a_file_and_names_one_it_cannot_read_or_understand() {
        let dir = std::env::temp_dir().join(format!("thalvi-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"ending_pause": {"task": 2}}"#).unwrap();

        let loaded = DesktopConfig::load(&path).unwrap();
        let missing = DesktopConfig::load(&dir.join("missing.json")).unwrap_err();
        let broken_path = dir.join("broken.json");
        std::fs::write(&broken_path, "{1: 2}").unwrap();
        let broken = DesktopConfig::load(&broken_path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            loaded.ending_pause.get(CaptureRoute::Task),
            Duration::from_secs(2)
        );
        assert!(matches!(missing, ConfigError::Read { .. }));
        assert!(missing.to_string().contains("missing.json"));
        assert_eq!(
            broken.to_string(),
            format!(
                "The desktop config at {} is not valid: key must be a string at line 1 column 2. \
                 Fix or remove the file, then start Thalvi again.",
                broken_path.display()
            )
        );
    }
}
