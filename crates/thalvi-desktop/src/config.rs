//! Desktop configuration, validated once at load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::capture::{CaptureLimits, EndingPauses, Endings};
use crate::cues::Cues;
use crate::host::{App, Modifier, Shortcut, WebUrl};
use crate::metrics::RecordKinds;
use crate::models::CatalogSpeech;
use crate::recognizer::ServerUrl;
use crate::secondary::SecondaryStt;
use crate::transducer::SpeechChoice;
use crate::wake::WakeConfig;

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
        "THALVI_RECORD contains an unknown value: {0:?}. Use a comma-separated list of metrics, text and audio."
    )]
    Record(String),
}

/// What the desktop app is configured to do. Every setting is optional in the file and has
/// the default shown in `Default` of its type; unknown settings are rejected.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawDesktopConfig")]
pub struct DesktopConfig {
    pub server: ServerUrl,
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
    /// From `THALVI_RECORD`, never from the file.
    pub record: RecordKinds,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDesktopConfig {
    #[serde(default)]
    server: ServerUrl,
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
    stt: SttConfig,
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
}

/// How the overlay and the settings window look; `Classic` is the plain pill and window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", rename_all = "lowercase")]
pub enum Look {
    #[default]
    Classic,
    Holo,
    Gunmetal,
}

impl TryFrom<String> for Look {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        match name.as_str() {
            "classic" => Ok(Self::Classic),
            "holo" => Ok(Self::Holo),
            "gunmetal" => Ok(Self::Gunmetal),
            _ => Err("look must be classic, holo or gunmetal.".to_owned()),
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

impl TryFrom<RawDesktopConfig> for DesktopConfig {
    type Error = String;

    fn try_from(raw: RawDesktopConfig) -> Result<Self, String> {
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
        Ok(Self {
            server: raw.server,
            wake: raw.wake,
            cues: raw.cues,
            ending_pause: raw.ending_pause,
            capture_limits: raw.capture_limits,
            endings: raw.endings,
            stt: raw.stt,
            apps,
            scripts,
            overlay: raw.overlay,
            microphone,
            look: raw.look,
            record: RecordKinds::default(),
        })
    }
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
    FocusApp(App),
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
            RawAction::FocusApp => ScriptAction::FocusApp(
                apps.get(&destination)
                    .cloned()
                    .ok_or_else(|| format!("No app named {destination:?} is configured."))?,
            ),
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

/// Where dictation is recognized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SttEngine {
    /// Streams audio to the Thalvi server's `/transcription` route.
    #[default]
    Server,
    /// Recognizes on this machine; opens no server connection.
    Local(SpeechChoice),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum EngineName {
    Server,
    Local,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStt {
    #[serde(default)]
    secondary: Option<SecondaryStt>,
    #[serde(default)]
    engine: Option<EngineName>,
    #[serde(default)]
    model: Option<String>,
}

impl TryFrom<RawStt> for SttConfig {
    type Error = String;

    fn try_from(raw: RawStt) -> Result<Self, String> {
        let engine = match (raw.engine, raw.model) {
            (None | Some(EngineName::Server), None) => SttEngine::Server,
            (None | Some(EngineName::Server), Some(_)) => {
                return Err("stt.model only applies when stt.engine is local.".to_owned());
            }
            (Some(EngineName::Local), None) => {
                SttEngine::Local(SpeechChoice::Catalog(CatalogSpeech::ALL[0]))
            }
            (Some(EngineName::Local), Some(model)) => {
                let choice = if let Some(catalog) = CatalogSpeech::ALL
                    .iter()
                    .find(|catalog| catalog.name() == model)
                {
                    SpeechChoice::Catalog(*catalog)
                } else if model.contains(['/', '\\']) {
                    SpeechChoice::Folder(PathBuf::from(model))
                } else {
                    return Err(format!(
                        "Unknown speech model \"{model}\". Use {} or the path of a folder holding encoder.int8.onnx, decoder.int8.onnx, joiner.int8.onnx and tokens.txt.",
                        CatalogSpeech::ALL.map(CatalogSpeech::name).join(", ")
                    ));
                };
                SttEngine::Local(choice)
            }
        };
        Ok(Self {
            secondary: raw.secondary,
            engine,
        })
    }
}

/// Speech-to-text settings.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(try_from = "RawStt")]
pub struct SttConfig {
    /// Re-transcribes each accepted capture on this endpoint when set.
    pub secondary: Option<SecondaryStt>,
    pub engine: SttEngine,
}

impl Serialize for SttConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct FileStt<'a> {
            secondary: &'a Option<SecondaryStt>,
            engine: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            model: Option<String>,
        }
        let (engine, model) = match &self.engine {
            SttEngine::Server => ("server", None),
            SttEngine::Local(choice) => ("local", Some(choice.label())),
        };
        FileStt {
            secondary: &self.secondary,
            engine,
            model,
        }
        .serialize(serializer)
    }
}

/// A config in the form of its file; `record` is never part of it.
#[derive(Serialize)]
struct FileForm<'a> {
    server: &'a ServerUrl,
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
        let scripts = self
            .scripts
            .iter()
            .map(|script| {
                let (action, destination) = match &script.action {
                    ScriptAction::OpenUrl(url) => ("open_url", url.as_str()),
                    ScriptAction::FocusApp(app) => {
                        let name = self
                            .apps
                            .iter()
                            .find_map(|(name, known)| (known == app).then_some(name))
                            .ok_or_else(|| {
                                serde::ser::Error::custom("script app is not configured")
                            })?;
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
            server: &self.server,
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
        }
        .serialize(serializer)
    }
}

impl DesktopConfig {
    /// Reads and validates the config at `path`; `record` is the value of `THALVI_RECORD`.
    ///
    /// # Errors
    /// Fails when the file cannot be read, a setting is unknown or out of range, or `record`
    /// names an unknown kind.
    pub fn load(path: &Path, record: Option<&str>) -> Result<Self, ConfigError> {
        let json = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_json_str(&json, record)
    }

    /// Validates config `json`; `record` is the value of `THALVI_RECORD`.
    ///
    /// # Errors
    /// Fails when a setting is unknown or out of range, or `record` names an unknown kind.
    pub fn from_json_str(json: &str, record: Option<&str>) -> Result<Self, ConfigError> {
        let mut config: Self = serde_json::from_str(json)?;
        config.record = record_kinds(record)?;
        Ok(config)
    }
}

fn record_kinds(raw: Option<&str>) -> Result<RecordKinds, ConfigError> {
    let mut kinds = RecordKinds::default();
    let values = raw
        .into_iter()
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    for value in values {
        match value {
            "metrics" => kinds.metrics = true,
            "text" => kinds.text = true,
            "audio" => kinds.audio = true,
            other => return Err(ConfigError::Record(other.to_owned())),
        }
    }
    Ok(kinds)
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

    use super::*;
    use crate::capture::{CaptureRoute, Ending};
    use crate::cues::Cue;
    use crate::models::CatalogHead;
    use crate::secondary::SecondaryApi;
    use crate::wake::HeadModel;

    fn parse(json: &str) -> Result<DesktopConfig, ConfigError> {
        DesktopConfig::from_json_str(json, None)
    }

    #[test]
    fn an_empty_config_uses_the_defaults() {
        let config = parse("{}").unwrap();
        assert_eq!(
            config.server,
            ServerUrl::parse("ws://127.0.0.1:8080").unwrap()
        );
        assert_eq!(config.stt, SttConfig::default());
        assert_eq!(config.stt.secondary, None);
        assert_eq!(config.wake, WakeConfig::default());
        assert_eq!(config.cues, Cues::default());
        assert_eq!(config.ending_pause, EndingPauses::default());
        assert_eq!(config.capture_limits, CaptureLimits::default());
        assert_eq!(config.endings, Endings::default());
        assert_eq!(config.record, RecordKinds::default());
    }

    #[test]
    fn dictation_is_recognized_on_the_server_unless_a_local_model_is_chosen() {
        for (json, engine) in [
            (r#"{"stt": {"engine": "server"}}"#, SttEngine::Server),
            (
                r#"{"stt": {"engine": "local"}}"#,
                SttEngine::Local(SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms)),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "nemotron_160ms"}}"#,
                SttEngine::Local(SpeechChoice::Catalog(CatalogSpeech::Nemotron160ms)),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "/models/mine"}}"#,
                SttEngine::Local(SpeechChoice::Folder("/models/mine".into())),
            ),
            (
                r#"{"stt": {"engine": "local", "model": "C:\\models\\mine"}}"#,
                SttEngine::Local(SpeechChoice::Folder("C:\\models\\mine".into())),
            ),
        ] {
            assert_eq!(parse(json).unwrap().stt.engine, engine, "{json}");
        }
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
                    "Send It": "submit", "paste": "paste", "do it": "task",
                    "scratch": "scratch", "cancel": "cancel"
                }
            }"#,
            Some("metrics, audio"),
        )
        .unwrap();
        assert_eq!(
            config.server,
            ServerUrl::parse("wss://thalvi.example:9000").unwrap()
        );
        assert_eq!(
            config.stt.secondary,
            Some(
                SecondaryStt::new(
                    "http://127.0.0.1:9/v1",
                    "m",
                    String::new(),
                    SecondaryApi::Meta
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
                ("do it".to_owned(), Ending::Task),
                ("scratch".to_owned(), Ending::Scratch),
                ("cancel".to_owned(), Ending::Cancel),
            ]))
            .unwrap()
        );
        assert_eq!(
            config.record,
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
                    action: ScriptAction::FocusApp(t3),
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
                ScriptAction::FocusApp(App {
                    classes: vec!["a".to_owned()],
                    executables: Vec::new(),
                    shortcut: expected,
                })
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
                "Unknown speech model \"whisper\". Use nemotron_160ms or the path of a folder",
            ),
            (r#"{"stt": {"engine": "cloud"}}"#, "unknown variant `cloud`"),
        ] {
            let error = parse(json).unwrap_err();
            assert!(error.to_string().contains(expected), "{json}: {error}");
        }
    }

    #[test]
    fn invalid_settings_are_rejected_in_plain_words() {
        let valid_endings = r#""paste": "paste", "send": "submit", "do it": "task",
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
                r#"{"stt": {"secondary": {"url": "ftp://x", "model": "m"}}}"#.to_owned(),
                "stt.secondary.url must be an http or https URL",
            ),
            (
                r#"{"stt": {"secondary": {"url": "http://x", "model": " "}}}"#.to_owned(),
                "stt.secondary.model must be nonempty",
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
                "Configure paste, submit, task, scratch, and cancel",
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
    fn record_kinds_come_from_a_comma_separated_list() {
        for (raw, expected) in [
            (None, Ok((false, false, false))),
            (Some(""), Ok((false, false, false))),
            (Some("text"), Ok((false, true, false))),
            (Some(" metrics , audio,"), Ok((true, false, true))),
            (Some("metrics,text,audio"), Ok((true, true, true))),
            (Some("metrics,speech"), Err("speech")),
            (Some("on"), Err("on")),
        ] {
            let result = DesktopConfig::from_json_str("{}", raw).map(|config| {
                (
                    config.record.metrics,
                    config.record.text,
                    config.record.audio,
                )
            });
            match (result, expected) {
                (Ok(kinds), Ok(expected)) => assert_eq!(kinds, expected, "{raw:?}"),
                (Err(ConfigError::Record(value)), Err(expected)) => {
                    assert_eq!(value, expected, "{raw:?}");
                }
                (other, expected) => panic!("{raw:?}: {other:?} vs {expected:?}"),
            }
        }
    }

    #[test]
    fn the_config_serializes_to_the_file_form_it_was_read_from() {
        let full = r#"{
            "server": "wss://thalvi.example:9000/",
            "stt": {"engine": "local", "model": "/models/speech",
                    "secondary": {"url": "http://127.0.0.1:9/v1/", "model": "m", "api_key": "k", "api": "meta"}},
            "wake": {"capture": {"model": "/models/me.onnx", "threshold": 0.7},
                     "conversation": {"model": "hey_jarvis"}},
            "cues": {"wake": false, "error": false},
            "ending_pause": {"paste": 1, "submit": 3, "task": 2.5},
            "capture_limits": {"idle": 90, "max": 120},
            "endings": {"Send It": "submit", "paste": "paste", "do it": "task",
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
            "look": "gunmetal"
        }"#;
        for json in ["{}", full] {
            let config = DesktopConfig::from_json_str(json, Some("metrics,audio")).unwrap();
            let written = serde_json::to_string(&config).unwrap();
            let again = DesktopConfig::from_json_str(&written, None).unwrap();
            assert_eq!(
                again,
                DesktopConfig {
                    record: RecordKinds::default(),
                    ..config
                },
                "{written}"
            );
            assert!(!written.contains("record"), "{written}");
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
    fn look_defaults_to_classic_and_rejects_unknown_names() {
        assert_eq!(parse("{}").unwrap().look, Look::Classic);
        for (name, look) in [
            ("classic", Look::Classic),
            ("holo", Look::Holo),
            ("gunmetal", Look::Gunmetal),
        ] {
            let config = parse(&format!(r#"{{"look": "{name}"}}"#)).unwrap();
            assert_eq!(config.look, look);
        }
        for json in [r#"{"look": "neon"}"#, r#"{"look": "Holo"}"#] {
            let error = parse(json).unwrap_err().to_string();
            assert!(
                error.contains("look must be classic, holo or gunmetal."),
                "{json}: {error}"
            );
        }
    }

    #[test]
    fn load_reads_a_file_and_names_one_it_cannot_read() {
        let dir = std::env::temp_dir().join(format!("thalvi-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"ending_pause": {"task": 2}}"#).unwrap();

        let loaded = DesktopConfig::load(&path, None).unwrap();
        let missing = DesktopConfig::load(&dir.join("missing.json"), None).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            loaded.ending_pause.get(CaptureRoute::Task),
            Duration::from_secs(2)
        );
        assert!(matches!(missing, ConfigError::Read { .. }));
        assert!(missing.to_string().contains("missing.json"));
    }
}
