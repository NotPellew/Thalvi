//! Desktop configuration, validated once at load.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::capture::{Endings, PauseDuration};
use crate::cues::Cues;
use crate::metrics::RecordKinds;
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
#[serde(deny_unknown_fields)]
pub struct DesktopConfig {
    #[serde(default)]
    pub wake: WakeConfig,
    #[serde(default, deserialize_with = "cues")]
    pub cues: Cues,
    #[serde(default)]
    pub ending_pause: PauseDuration,
    #[serde(default)]
    pub endings: Endings,
    /// From `THALVI_RECORD`, never from the file.
    #[serde(skip)]
    pub record: RecordKinds,
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
    use crate::capture::Ending;
    use crate::models::CatalogHead;
    use crate::wake::HeadModel;

    fn parse(json: &str) -> Result<DesktopConfig, ConfigError> {
        DesktopConfig::from_json_str(json, None)
    }

    #[test]
    fn an_empty_config_uses_the_defaults() {
        let config = parse("{}").unwrap();
        assert_eq!(config.wake, WakeConfig::default());
        assert_eq!(config.cues, Cues::default());
        assert_eq!(config.ending_pause.get(), Duration::from_millis(2500));
        assert_eq!(config.endings, Endings::default());
        assert_eq!(config.record, RecordKinds::default());
    }

    #[test]
    fn every_setting_is_read() {
        let config = DesktopConfig::from_json_str(
            r#"{
                "wake": {"capture": {"model": "hey_jarvis", "threshold": 0.7}},
                "cues": {"wake": false, "error": false},
                "ending_pause": 3,
                "endings": {
                    "Send It": "submit", "paste": "paste", "do it": "task",
                    "scratch": "scratch", "cancel": "cancel"
                }
            }"#,
            Some("metrics, audio"),
        )
        .unwrap();
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
        assert_eq!(config.ending_pause.get(), Duration::from_secs(3));
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
    fn invalid_settings_are_rejected_in_plain_words() {
        let valid_endings = r#""paste": "paste", "send": "submit", "do it": "task",
            "scratch": "scratch", "cancel": "cancel""#;
        for (json, expected) in [
            (r#"{"apps": {}}"#.to_owned(), "unknown field `apps`"),
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
            (r#"{"ending_pause": 1.9}"#.to_owned(), "from 2 to 3"),
            (r#"{"ending_pause": 3.1}"#.to_owned(), "from 2 to 3"),
            (r#"{"ending_pause": "2.5"}"#.to_owned(), "invalid type"),
            (r#"{"ending_pause": true}"#.to_owned(), "invalid type"),
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
    fn load_reads_a_file_and_names_one_it_cannot_read() {
        let dir = std::env::temp_dir().join(format!("thalvi-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"ending_pause": 2}"#).unwrap();

        let loaded = DesktopConfig::load(&path, None).unwrap();
        let missing = DesktopConfig::load(&dir.join("missing.json"), None).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(loaded.ending_pause.get(), Duration::from_secs(2));
        assert!(matches!(missing, ConfigError::Read { .. }));
        assert!(missing.to_string().contains("missing.json"));
    }
}
