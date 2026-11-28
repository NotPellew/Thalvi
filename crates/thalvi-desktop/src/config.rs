//! Desktop configuration, validated once at load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::capture::{CaptureLimits, EndingPauses, Endings};
use crate::cues::Cues;
use crate::entries::{Entries, EntryError, RawStt};
use crate::history::HistoryConfig;
use crate::hold::{Action, KeyBind, KeyMode, KeyName};
use crate::host::{App, Desktop, HostError, Modifier, Shortcut, WebUrl};
use crate::metrics::RecordKinds;
use crate::phrases::WordList;
use crate::recognizer::SpeechSource;
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
    #[error("The desktop config at {path} cannot use these keys: {source}")]
    UnbindableKeys {
        path: PathBuf,
        #[source]
        source: HostError,
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
    /// Which builds the app updates itself to.
    pub update_channel: UpdateChannel,
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
    /// The keys Settings → Keys lists, in order: each an action, how its key works, and the key.
    /// The desktop's default rows unless the config names others.
    pub keys: Vec<KeyBind>,
    /// The chat endpoint that writes dictation out; without one, asking for it says where to set it up.
    pub writing: Option<Writing>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDesktopConfig {
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
    /// Read as JSON so the learning switch can be added without changing the speech entries'
    /// reader; the entries and their strictness are validated in `build`.
    #[serde(default)]
    stt: serde_json::Value,
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
    update_channel: UpdateChannel,
    #[serde(default)]
    recordings: RecordKinds,
    #[serde(default)]
    history: HistoryConfig,
    #[serde(default)]
    start_at_login: bool,
    #[serde(default)]
    show_in_menu: bool,
    /// Read as written, validated in `build`.
    #[serde(default)]
    keys: Option<Vec<KeyBind>>,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
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

/// Which builds the app keeps itself on: `Stable` unless the user picks the development builds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Development,
}

impl UpdateChannel {
    /// The name the config file and the metrics use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Development => "development",
        }
    }
}

impl TryFrom<String> for UpdateChannel {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        match name.as_str() {
            "stable" => Ok(Self::Stable),
            "development" => Ok(Self::Development),
            _ => Err("update_channel must be stable or development.".to_owned()),
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
type KeyDefaults = (KeyName, KeyName);

/// The key rows a desktop starts with: its hold key and write key, both in Hold mode.
fn default_keys((hold, write): &KeyDefaults) -> Vec<KeyBind> {
    vec![
        KeyBind::new(Action::Dictate, KeyMode::Hold, hold.clone()),
        KeyBind::new(Action::Write, KeyMode::Hold, write.clone()),
    ]
}

/// The rows to store: none when they are the desktop's default, which a config then follows if it
/// changes.
fn stored_keys<'a>(keys: &'a [KeyBind], defaults: &KeyDefaults) -> Option<&'a [KeyBind]> {
    (keys != default_keys(defaults)).then_some(keys)
}

/// Why key rows cannot be used, in the words Settings shows: each key only once, and each action
/// with a row that can start it and one that can end it.
fn validate_keys(keys: &[KeyBind]) -> Result<(), String> {
    let mut known: Vec<&KeyName> = Vec::new();
    for bind in keys {
        if known.contains(&&bind.key) {
            return Err(format!(
                "The key {} is bound more than once. Give each keybind its own key.",
                bind.key
            ));
        }
        known.push(&bind.key);
    }
    for action in Action::ALL {
        let rows: Vec<&KeyBind> = keys.iter().filter(|bind| bind.action == action).collect();
        if rows.is_empty() {
            continue;
        }
        if !rows.iter().any(|bind| bind.mode.starts()) {
            return Err(format!(
                "The {} keys can end a capture but none can start one. Give a row Hold or Toggle mode, or add a Start key.",
                action.label()
            ));
        }
        if !rows.iter().any(|bind| bind.mode.ends()) {
            return Err(format!(
                "The {} keys can start a capture but none can end it. Give a row Hold or Toggle mode, or add a Stop key.",
                action.label()
            ));
        }
    }
    Ok(())
}

impl TryFrom<RawDesktopConfig> for DesktopConfig {
    type Error = String;

    fn try_from(raw: RawDesktopConfig) -> Result<Self, String> {
        build(raw, &Desktop::default_hold_keys())
    }
}

fn build(raw: RawDesktopConfig, defaults: &KeyDefaults) -> Result<DesktopConfig, String> {
    let keys = raw.keys.unwrap_or_else(|| default_keys(defaults));
    validate_keys(&keys)?;
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
    let mut stt = match raw.stt {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map,
        other => return Err(format!("stt must be an object, not {other}.")),
    };
    let learn_from_corrections = match stt.remove("learn_from_corrections") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(on)) => on,
        Some(_) => return Err("stt.learn_from_corrections must be true or false.".to_owned()),
    };
    let raw_stt: RawStt = serde_json::from_value(serde_json::Value::Object(stt))
        .map_err(|error: serde_json::Error| error.to_string())?;
    let (entries, phrases) = raw_stt
        .entries()
        .map_err(|error: EntryError| error.to_string())?;
    Ok(DesktopConfig {
        wake: raw.wake,
        cues: raw.cues,
        ending_pause: raw.ending_pause,
        capture_limits: raw.capture_limits,
        endings: raw.endings,
        stt: SttConfig {
            entries,
            phrases,
            learn_from_corrections,
        },
        apps,
        scripts,
        overlay: raw.overlay,
        microphone,
        look: raw.look,
        update_channel: raw.update_channel,
        recordings: raw.recordings,
        history: raw.history,
        start_at_login: raw.start_at_login,
        show_in_menu: raw.show_in_menu,
        keys,
        // Numbers as digits with no endpoint is the default, so it is not kept.
        writing: raw
            .writing
            .filter(|writing| writing.has_endpoint() || !writing.numbers_as_digits()),
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
    FocusApp { name: String, app: App },
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
    /// The speech servers: the live one dictation uses.
    pub entries: Entries,
    /// Spellings recognition favours, on this computer and on a recognizer that boosts phrases.
    pub phrases: WordList,
    /// Whether Thalvi learns from a cleanup of the text it pasted. Off by default, so the file
    /// keeps the switch only when it is on.
    pub learn_from_corrections: bool,
}

impl SttConfig {
    /// Whether Thalvi watches a field after a paste to learn from its cleanup.
    #[must_use]
    pub fn learn_from_corrections(&self) -> bool {
        self.learn_from_corrections
    }
}

impl Serialize for SttConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct FileStt<'a> {
            entries: &'a Entries,
            #[serde(skip_serializing_if = "WordList::is_empty")]
            phrases: &'a WordList,
            #[serde(skip_serializing_if = "Option::is_none")]
            learn_from_corrections: Option<bool>,
        }
        FileStt {
            entries: &self.entries,
            phrases: &self.phrases,
            learn_from_corrections: self.learn_from_corrections.then_some(true),
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
    update_channel: UpdateChannel,
    recordings: RecordKinds,
    history: HistoryConfig,
    start_at_login: bool,
    show_in_menu: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    keys: Option<&'a [KeyBind]>,
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
    /// The file form for a desktop whose default keys are `defaults`: the key rows are in it only
    /// when they differ.
    fn serialize_for<S: Serializer>(
        &self,
        serializer: S,
        defaults: &KeyDefaults,
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
            update_channel: self.update_channel,
            recordings: self.recordings,
            history: self.history,
            start_at_login: self.start_at_login,
            show_in_menu: self.show_in_menu,
            keys: stored_keys(&self.keys, defaults),
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
        let invalid = |source| ConfigError::InvalidFile {
            path: path.to_owned(),
            source,
        };
        let raw: RawDesktopConfig = serde_json::from_str(&json).map_err(invalid)?;
        let config =
            Self::try_from(raw).map_err(|message| invalid(serde::de::Error::custom(message)))?;
        Desktop::check_hold_keys(&config.keys).map_err(|source| ConfigError::UnbindableKeys {
            path: path.to_owned(),
            source,
        })?;
        Ok(config)
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
    use crate::entries::LiveKind;
    use crate::models::{CatalogHead, CatalogSpeech};
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
        assert_eq!(config.wake, WakeConfig::default());
        assert_eq!(config.cues, Cues::default());
        assert_eq!(config.endings, Endings::default());
        assert!(!config.start_at_login);
        assert!(!config.show_in_menu);
        assert_eq!(config.keys, default_keys(&Desktop::default_hold_keys()));
        assert_eq!(config.writing, None);
    }

    const WINDOWS: fn() -> KeyDefaults = || (KeyName::alt_z(), KeyName::alt_x());
    const HYPRLAND: fn() -> KeyDefaults = || (KeyName::f9(), KeyName::f10());

    /// The config `json` as a desktop with `defaults` loads it.
    fn loaded(json: &str, defaults: &KeyDefaults) -> Result<DesktopConfig, String> {
        build(
            serde_json::from_str(json).map_err(|error| error.to_string())?,
            defaults,
        )
    }

    #[test]
    fn wake_words_are_on_unless_the_file_turns_them_off_and_only_off_is_stored() {
        for (json, enabled) in [
            ("{}", true),
            (r#"{"wake": {}}"#, true),
            (r#"{"wake": {"enabled": true}}"#, true),
            (r#"{"wake": {"enabled": false}}"#, false),
            (
                r#"{"wake": {"capture": {"model": "hey_jarvis"}, "enabled": false}}"#,
                false,
            ),
        ] {
            let config = DesktopConfig::from_json_str(json).unwrap();
            assert_eq!(config.wake.enabled(), enabled, "{json}");
            let written = serde_json::to_value(&config).unwrap();
            assert_eq!(
                written["wake"].get("enabled"),
                (!enabled).then_some(&serde_json::json!(false)),
                "{json}"
            );
            let again = DesktopConfig::from_json_str(&written.to_string()).unwrap();
            assert_eq!(again.wake, config.wake, "{json}");
        }
        assert!(DesktopConfig::from_json_str(r#"{"wake": {"enabled": "no"}}"#).is_err());
    }

    /// The rows `config` lists, as `"action mode key"`.
    fn rows(config: &DesktopConfig) -> Vec<String> {
        config
            .keys
            .iter()
            .map(|bind| format!("{} {} {}", bind.action.name(), bind.mode.name(), bind.key))
            .collect()
    }

    #[test]
    fn the_keys_are_the_desktops_defaults_and_are_stored_only_when_they_differ() {
        // (desktop, stored in the file, rows in effect, stored again)
        type Case = (
            fn() -> KeyDefaults,
            &'static str,
            &'static [&'static str],
            bool,
        );
        let cases: [Case; 4] = [
            (
                HYPRLAND,
                "{}",
                &["dictate hold F9", "write hold F10"],
                false,
            ),
            (
                WINDOWS,
                "{}",
                &["dictate hold Alt+KeyZ", "write hold Alt+KeyX"],
                false,
            ),
            (
                HYPRLAND,
                r#"{"keys": [{"action": "dictate", "mode": "toggle", "key": "F7"}]}"#,
                &["dictate toggle F7"],
                true,
            ),
            (
                HYPRLAND,
                r#"{"keys": [
                    {"action": "dictate", "mode": "hold", "key": "F9"},
                    {"action": "write", "mode": "hold", "key": "F10"}
                ]}"#,
                &["dictate hold F9", "write hold F10"],
                false,
            ),
        ];
        for (desktop, json, in_effect, stored) in cases {
            let defaults = desktop();
            let config = loaded(json, &defaults).unwrap();
            assert_eq!(rows(&config), in_effect, "{json}");
            let written = config
                .serialize_for(serde_json::value::Serializer, &defaults)
                .unwrap();
            assert_eq!(written.get("keys").is_some(), stored, "{json}");
            assert_eq!(loaded(&written.to_string(), &defaults).unwrap(), config);
        }
    }

    #[test]
    fn every_action_needs_a_key_that_starts_it_and_one_that_ends_it() {
        let keys = |json: &str| loaded(json, &HYPRLAND());
        for (json, expected) in [
            (
                r#"{"keys": [{"action": "dictate", "mode": "stop", "key": "F7"}]}"#,
                "The Dictate keys can end a capture but none can start one. Give a row Hold or Toggle mode, or add a Start key.",
            ),
            (
                r#"{"keys": [{"action": "write", "mode": "start", "key": "F7"}]}"#,
                "The Write keys can start a capture but none can end it. Give a row Hold or Toggle mode, or add a Stop key.",
            ),
        ] {
            let error = keys(json).unwrap_err();
            assert!(error.contains(expected), "{json}: {error}");
        }
        for good in [
            r#"{"keys": [{"action": "dictate", "mode": "start", "key": "F7"},
                         {"action": "dictate", "mode": "stop", "key": "F8"}]}"#,
            r#"{"keys": [{"action": "dictate", "mode": "toggle", "key": "F7"}]}"#,
            r#"{"keys": []}"#,
        ] {
            let config = keys(good).unwrap_or_else(|error| panic!("{good}: {error}"));
            assert_eq!(
                loaded(&serde_json::to_string(&config).unwrap(), &HYPRLAND()).unwrap(),
                config
            );
        }
    }

    #[test]
    fn key_rows_refuse_a_key_twice_and_a_mode_that_is_no_mode() {
        let error = loaded(
            r#"{"keys": [{"action": "dictate", "mode": "hold", "key": "F7"},
                         {"action": "write", "mode": "toggle", "key": "F7"}]}"#,
            &HYPRLAND(),
        )
        .unwrap_err();
        assert!(error.contains("F7 is bound more than once"), "{error}");
        let error = loaded(
            r#"{"keys": [{"action": "dictate", "mode": "strt", "key": "F7"}]}"#,
            &HYPRLAND(),
        )
        .unwrap_err();
        assert!(
            error.contains("mode must be hold, toggle, start or stop"),
            "{error}"
        );
        let error = loaded(
            r#"{"keys": [{"action": "listen", "mode": "hold", "key": "F7"}]}"#,
            &HYPRLAND(),
        )
        .unwrap_err();
        assert!(error.contains("unknown variant `listen`"), "{error}");
        let error = loaded(
            r#"{"keys": [{"action": "dictate", "mode": "hold", "key": "KeyZ"}]}"#,
            &HYPRLAND(),
        )
        .unwrap_err();
        assert!(error.contains("needs a modifier"), "{error}");
    }

    #[test]
    fn the_writing_endpoint_is_absent_by_default_and_refused_when_it_cannot_be_used() {
        let writing = |json: &str| parse(json).map(|config| config.writing);
        assert_eq!(writing(r#"{"writing": null}"#).unwrap(), None);
        assert_eq!(
            writing(r#"{"writing": {"numbers_as_digits": true}}"#).unwrap(),
            None,
            "on with no endpoint is the default and is not kept"
        );
        assert_eq!(writing(r#"{"writing": {}}"#).unwrap(), None);
        let off = writing(r#"{"writing": {"numbers_as_digits": false}}"#)
            .unwrap()
            .unwrap();
        assert!(!off.numbers_as_digits());
        assert!(!off.has_endpoint(), "the switch needs no endpoint");
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
                "writing.model must be",
            ),
            (
                r#"{"writing": {"url": "http://x", "prompt": "Be brief."}}"#,
                "writing.model must be",
            ),
            (
                r#"{"writing": {"prompt": " \n"}}"#,
                "writing.prompt must not be blank",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "tone": 1}}"#,
                "unknown field `tone`",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "openings": [{"phrase": "?!", "prompt": "p"}]}}"#,
                "writing.openings needs a phrase",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "openings": [{"phrase": "a b", "prompt": " "}]}}"#,
                "writing.openings has a blank prompt for “a b”",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "openings": [{"phrase": "A b", "prompt": "p"}, {"phrase": "a, B!", "prompt": "q"}]}}"#,
                "writing.openings has “a b” twice.",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "apps": [{"app": " ", "prompt": "p"}]}}"#,
                "writing.apps has a blank app.",
            ),
            (
                r#"{"writing": {"url": "http://x", "model": "m", "apps": [{"app": "kitty", "prompt": " \n"}]}}"#,
                "writing.apps has a blank prompt.",
            ),
            (
                r#"{"writing": {"clean_dictation": true}}"#,
                "writing.clean_dictation needs a writing endpoint",
            ),
            (
                r#"{"writing": {"prompt": "Be brief."}}"#,
                "writing.prompt needs a writing endpoint",
            ),
            (
                r#"{"writing": {"openings": [{"phrase": "a b", "prompt": "p"}]}}"#,
                "writing.openings needs a writing endpoint",
            ),
            (
                r#"{"writing": {"apps": [{"app": "kitty", "prompt": "p"}]}}"#,
                "writing.apps needs a writing endpoint",
            ),
            (
                r#"{"writing": {"chat_build": "cpu"}}"#,
                "writing.chat_build needs a writing endpoint",
            ),
            (
                r#"{"writing": {"api_key": "k"}}"#,
                "writing.api_key needs a writing endpoint",
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
        let cleaned =
            parse(r#"{"writing": {"url": "http://x", "model": "m", "clean_dictation": true}}"#)
                .unwrap();
        assert!(cleaned.writing.as_ref().unwrap().clean_dictation());
        let stored_flag = serde_json::to_value(&cleaned).unwrap();
        assert_eq!(stored_flag["writing"]["clean_dictation"], true);
        let off = parse(r#"{"writing": {"url": "http://x", "model": "m"}}"#).unwrap();
        assert!(!off.writing.as_ref().unwrap().clean_dictation());
        assert!(
            serde_json::to_value(&off).unwrap()["writing"]
                .get("clean_dictation")
                .is_none()
        );
        assert_eq!(stored(DEFAULT_PROMPT), (DEFAULT_PROMPT.to_owned(), None));
        assert_eq!(
            stored("  Be brief.\n"),
            ("Be brief.".to_owned(), Some(json!("Be brief.")))
        );
    }

    #[test]
    fn writing_rules_round_trip_normalized_and_empty_lists_are_not_written() {
        let endpoint = json!({"url": "http://x", "model": "m"});
        for (field, input, normalized) in [
            (
                "openings",
                json!([{"phrase": "Write me an e-mail", "prompt": " Short. "}]),
                json!([{"phrase": "write me an e mail", "prompt": "Short."}]),
            ),
            (
                "openings",
                json!([{"phrase": "Never mindİ", "prompt": "p"}]),
                json!([{"phrase": "never mindi", "prompt": "p"}]),
            ),
            (
                "apps",
                json!([
                    {"app": " kitty ", "prompt": " One line. "},
                    {"app": " firefox ", "title": " Gmail ", "prompt": " Polite. "},
                    {"app": " editor ", "title": " ", "prompt": " Clear. "}
                ]),
                json!([
                    {"app": "kitty", "prompt": "One line."},
                    {"app": "firefox", "title": "Gmail", "prompt": "Polite."},
                    {"app": "editor", "prompt": "Clear."}
                ]),
            ),
        ] {
            let written = |rules: serde_json::Value| {
                let mut writing = endpoint.clone();
                writing[field] = rules;
                let config = parse(&json!({"writing": writing}).to_string()).unwrap();
                let written = serde_json::to_value(&config).unwrap();
                assert_eq!(parse(&written.to_string()).unwrap(), config);
                written["writing"].clone()
            };
            assert_eq!(written(input)[field], normalized, "{field}");
            assert!(written(json!([])).get(field).is_none(), "{field}");
            let plain = parse(&json!({"writing": endpoint}).to_string()).unwrap();
            assert!(
                serde_json::to_value(&plain).unwrap()["writing"]
                    .get(field)
                    .is_none(),
                "{field}"
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
            LiveKind::OnDevice(SpeechChoice::Catalog(CatalogSpeech::ALL[0]))
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
                     "destination": " https://github.com/NotPellew/Thalvi "}
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

    fn rejected_settings() -> Vec<(String, &'static str)> {
        const HISTORY_LIMIT: &str =
            "history limit must be a whole number of entries from 1 to 1000";
        let valid_endings = r#""paste": "paste", "write": "write", "send": "submit", "do it": "task",
            "scratch": "scratch", "cancel": "cancel""#;
        vec![
            (r#"{"discord": {}}"#.to_owned(), "unknown field `discord`"),
            (
                r#"{"server": "http://127.0.0.1:8080"}"#.to_owned(),
                "unknown field `server`",
            ),
            (
                r#"{"server": "ws://127.0.0.1:8080/session"}"#.to_owned(),
                "unknown field `server`",
            ),
            (
                r#"{"server": "ws://127.0.0.1:8080"}"#.to_owned(),
                "unknown field `server`",
            ),
            (r#"{"hold_key": "F8"}"#.to_owned(), "unknown field `hold_key`"),
            (
                r#"{"write_key": "F10"}"#.to_owned(),
                "unknown field `write_key`",
            ),
            (
                r#"{"stt": {"engine": "local", "model": "nemotron_160ms"}}"#.to_owned(),
                "unknown field `engine`",
            ),
            (
                r#"{"stt": {"secondary": {"enabled": true, "url": "http://127.0.0.1:9/v1", "model": "m"}}}"#.to_owned(),
                "unknown field `secondary`",
            ),
            (
                r#"{"stt": {"merge": true}}"#.to_owned(),
                "unknown field `merge`",
            ),
            (
                r#"{"stt": {"entries": [{"kind": "thalvi_server", "id": "s", "name": "S", "active": true}]}}"#.to_owned(),
                "unknown variant `thalvi_server`",
            ),
            (
                r#"{"stt": {"entries": [{"kind": "on_device", "id": "d", "name": "D", "active": true}, {"kind": "extra", "id": "x", "name": "X", "active": false}]}}"#.to_owned(),
                "unknown variant `extra`",
            ),
            (
                r#"{"stt": {"entries": [{"kind": "on_device", "id": "d", "name": "D", "active": true}, {"kind": "second_pass", "id": "x", "name": "X", "active": false}]}}"#.to_owned(),
                "unknown variant `second_pass`",
            ),
            (
                r#"{"stt": {"entries": [{"kind": "on_device", "id": "d", "name": "D", "active": true, "extra": true}]}}"#.to_owned(),
                "unknown field `extra`",
            ),
            (
                r#"{"stt": {"entries": [{"kind": "on_device", "id": "d", "name": "D", "active": true, "final_pass": true}]}}"#.to_owned(),
                "unknown field `final_pass`",
            ),
            (
                r#"{"stt": {"primary": {}}}"#.to_owned(),
                "unknown field `primary`",
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
            (
                r#"{"stt": {"model": "nemotron_160ms"}}"#.to_owned(),
                "unknown field `model`",
            ),
            (
                r#"{"stt": {"engine": "local", "model": "whisper"}}"#.to_owned(),
                "unknown field `engine`",
            ),
            (
                r#"{"stt": {"engine": "cloud"}}"#.to_owned(),
                "unknown field `engine`",
            ),
            (r#"{"history": {"limit": 0}}"#.to_owned(), HISTORY_LIMIT),
            (r#"{"history": {"limit": 1001}}"#.to_owned(), HISTORY_LIMIT),
            (r#"{"history": {"limit": -1}}"#.to_owned(), "invalid value"),
            (r#"{"history": {"enabled": "yes"}}"#.to_owned(), "boolean"),
            (
                r#"{"history": {"audio": true}}"#.to_owned(),
                "unknown field `audio`",
            ),
            (
                r#"{"themes": {"version": 1}}"#.to_owned(),
                "unknown field `themes`",
            ),
        ]
    }

    fn rejected_apps_and_scripts() -> Vec<(String, &'static str)> {
        vec![
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
                script(
                    r#""phrase": "x", "action": "discord_message", "destination": "https://discord.com/channels/1/2""#,
                ),
                "unknown variant `discord_message`",
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
        ]
    }

    #[test]
    fn invalid_settings_are_rejected_in_plain_words() {
        for (json, expected) in rejected_settings()
            .into_iter()
            .chain(rejected_apps_and_scripts())
        {
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
    fn the_config_serializes_to_the_file_form_it_was_read_from() {
        let full = r#"{
            "stt": {"entries": [{"kind": "on_device", "id": "d", "name": "Device",
                                 "active": true, "model": "/models/speech"}]},
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
                 "label": "the editor"}
            ],
            "overlay": {"show": false, "draft": true},
            "microphone": " USB Mic ",
            "look": "gunmetal",
            "update_channel": "development",
            "recordings": {"metrics": true, "text": false, "audio": true},
            "history": {"enabled": false, "limit": 50},
            "start_at_login": true,
            "show_in_menu": true,
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
    fn look_defaults_to_gunmetal_rejects_unknown_names_and_round_trips_byte_for_byte() {
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
        let original = "{\n  \"look\": \"holo\"\n}";
        let config = parse(original).unwrap();
        assert_eq!(config.look, Look::Holo);
        let written = serde_json::to_string_pretty(&config).unwrap();
        assert!(
            !written.contains("\"themes\""),
            "a save adds no themes: {written}"
        );
        let again = parse(&written).unwrap();
        assert_eq!(serde_json::to_string_pretty(&again).unwrap(), written);
    }

    #[test]
    fn the_update_channel_defaults_to_stable_and_rejects_unknown_names() {
        assert_eq!(parse("{}").unwrap().update_channel, UpdateChannel::Stable);
        for (name, channel) in [
            ("stable", UpdateChannel::Stable),
            ("development", UpdateChannel::Development),
        ] {
            let config = parse(&format!(r#"{{"update_channel": "{name}"}}"#)).unwrap();
            assert_eq!(config.update_channel, channel);
            assert_eq!(channel.name(), name, "the name the metrics use");
        }
        for json in [
            r#"{"update_channel": "nightly"}"#,
            r#"{"update_channel": "Stable"}"#,
        ] {
            let error = parse(json).unwrap_err().to_string();
            assert!(
                error.contains("update_channel must be stable or development."),
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
