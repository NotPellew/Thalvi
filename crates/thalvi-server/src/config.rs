//! Owns server configuration loaded and validated once at startup.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::recorder::RecordKinds;

/// Validated server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Address the HTTP/WebSocket server binds to.
    pub addr: SocketAddr,
    /// Directory holding the built web client.
    pub web_dir: PathBuf,
    /// Chat Completions base URL, without a trailing slash.
    pub llm_base_url: String,
    /// Chat model name.
    pub llm_model: String,
    /// Chat API key; empty means no Authorization header.
    pub llm_api_key: String,
    /// `reasoning_effort` sent with each request; `None` leaves the server default.
    pub llm_reasoning_effort: Option<String>,
    /// Background model for conversation titles; `None` keeps first-line titles.
    pub bg_llm: Option<BgLlm>,
    /// Recognizer WebSocket URL.
    pub stt_url: String,
    /// Recognizer model name.
    pub stt_model: String,
    /// Recognizer API key.
    pub stt_api_key: String,
    /// Speech synthesis base URL, without a trailing slash.
    pub tts_base_url: String,
    /// Speech protocol `tts_base_url` speaks, with its own settings.
    pub tts_api: TtsApi,
    /// Speech synthesis voice name; the default every browser starts with.
    pub tts_voice: String,
    /// Voices offered in the app (`TTS_VOICES`), the default first.
    pub tts_voices: Vec<String>,
    /// Which recording kinds are enabled (`THALVI_RECORD`).
    pub record: RecordKinds,
    /// Directory recordings are written under (`THALVI_DATA_DIR`).
    pub data_dir: PathBuf,
    /// Whether the spoken acknowledgement is enabled (`THALVI_ACK`).
    pub ack_enabled: bool,
    /// Whether voice turns reach the chat model as audio (`LLM_AUDIO_INPUT`).
    pub llm_audio_input: bool,
    /// Age in days after which sessions are retired (`THALVI_RETIRE_DAYS`); `None` is off.
    pub retire_days: Option<u64>,
}

/// The background model's Chat Completions endpoint (`BG_LLM_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgLlm {
    /// Chat Completions base URL, without a trailing slash.
    pub base_url: String,
    /// Chat model name.
    pub model: String,
    /// Chat API key; empty means no Authorization header.
    pub api_key: String,
}

/// Speech synthesis protocol (`TTS_API`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsApi {
    /// `OpenAI` `/audio/speech` streaming raw PCM, as Kokoro-FastAPI serves it.
    OpenAi {
        /// Speech synthesis model name.
        model: String,
        /// Speech API key; empty means no Authorization header.
        api_key: String,
    },
    /// Qwen3-TTS server's framed `/audio/speech/pcm-stream`.
    Qwen3,
}

impl Config {
    /// Loads configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns a plain message naming the missing or invalid variable.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Loads configuration using `lookup` to resolve variable names.
    ///
    /// # Errors
    ///
    /// Returns a plain message naming the missing or invalid variable.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let addr_raw = required(&lookup, "THALVI_ADDR", false)?;
        let addr = addr_raw
            .parse::<SocketAddr>()
            .map_err(|_| format!("THALVI_ADDR is not a valid socket address: {addr_raw:?}"))?;
        let web_dir = required(&lookup, "THALVI_WEB_DIR", false)?;
        let llm_base_url = http_base(&lookup, "LLM_BASE_URL")?;
        let llm_model = required(&lookup, "LLM_MODEL", false)?;
        let llm_api_key = required(&lookup, "LLM_API_KEY", true)?;
        let llm_reasoning_effort = lookup("LLM_REASONING_EFFORT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let bg_llm = parse_bg_llm(&lookup)?;
        let stt_url = required(&lookup, "STT_URL", false)?;
        if !stt_url.starts_with("ws://") && !stt_url.starts_with("wss://") {
            return Err(format!(
                "STT_URL must be a ws:// or wss:// URL: {stt_url:?}"
            ));
        }
        let stt_model = required(&lookup, "STT_MODEL", false)?;
        let stt_api_key = required(&lookup, "STT_API_KEY", true)?;
        let tts_base_url = http_base(&lookup, "TTS_BASE_URL")?;
        let tts_api = match required(&lookup, "TTS_API", false)?.trim() {
            "openai" => TtsApi::OpenAi {
                model: required(&lookup, "TTS_MODEL", false)?,
                api_key: required(&lookup, "TTS_API_KEY", true)?,
            },
            "qwen3" => TtsApi::Qwen3,
            other => {
                return Err(format!("TTS_API must be openai or qwen3: {other:?}"));
            }
        };
        let tts_voice = required(&lookup, "TTS_VOICE", false)?;
        let tts_voices = parse_voices(&tts_voice, lookup("TTS_VOICES").as_deref())?;
        let ack_enabled = parse_on_off("THALVI_ACK", lookup("THALVI_ACK").as_deref())?;
        let llm_audio_input =
            parse_on_off("LLM_AUDIO_INPUT", lookup("LLM_AUDIO_INPUT").as_deref())?;
        let record = parse_record_kinds(lookup("THALVI_RECORD"))?;
        let retire_days = parse_retire_days(lookup("THALVI_RETIRE_DAYS").as_deref())?;
        let data_dir = lookup("THALVI_DATA_DIR")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "data".to_string());
        Ok(Self {
            addr,
            web_dir: PathBuf::from(web_dir),
            llm_base_url,
            llm_model,
            llm_api_key,
            llm_reasoning_effort,
            bg_llm,
            stt_url,
            stt_model,
            stt_api_key,
            tts_base_url,
            tts_api,
            tts_voice,
            tts_voices,
            record,
            data_dir: PathBuf::from(data_dir),
            ack_enabled,
            llm_audio_input,
            retire_days,
        })
    }
}

fn parse_bg_llm(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<BgLlm>, String> {
    const NAMES: [&str; 3] = ["BG_LLM_BASE_URL", "BG_LLM_MODEL", "BG_LLM_API_KEY"];
    let missing: Vec<&str> = NAMES
        .into_iter()
        .filter(|name| lookup(name).is_none())
        .collect();
    if missing.len() == NAMES.len() {
        return Ok(None);
    }
    if !missing.is_empty() {
        return Err(format!(
            "BG_LLM_BASE_URL, BG_LLM_MODEL and BG_LLM_API_KEY must be set together; not set: {}",
            missing.join(", ")
        ));
    }
    Ok(Some(BgLlm {
        base_url: http_base(lookup, "BG_LLM_BASE_URL")?,
        model: required(lookup, "BG_LLM_MODEL", false)?,
        api_key: required(lookup, "BG_LLM_API_KEY", true)?,
    }))
}

fn parse_voices(default: &str, raw: Option<&str>) -> Result<Vec<String>, String> {
    let mut voices = vec![default.to_string()];
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(voices);
    };
    let mut listed: Vec<&str> = Vec::new();
    for name in raw.split(',').map(str::trim) {
        if name.is_empty() {
            return Err(format!(
                "TTS_VOICES must not contain an empty name: {raw:?}"
            ));
        }
        if listed.contains(&name) {
            return Err(format!("TTS_VOICES lists {name:?} twice"));
        }
        listed.push(name);
    }
    voices.extend(
        listed
            .into_iter()
            .filter(|name| *name != default)
            .map(str::to_string),
    );
    Ok(voices)
}

fn parse_retire_days(raw: Option<&str>) -> Result<Option<u64>, String> {
    match raw.map(str::trim) {
        None | Some("" | "off") => Ok(None),
        Some(value) => match value.parse::<u64>() {
            Ok(days) if days > 0 => Ok(Some(days)),
            _ => Err(format!(
                "THALVI_RETIRE_DAYS must be off or a positive number of days: {value:?}"
            )),
        },
    }
}

fn parse_on_off(name: &str, raw: Option<&str>) -> Result<bool, String> {
    match raw.map(str::trim) {
        None | Some("" | "off") => Ok(false),
        Some("on") => Ok(true),
        Some(other) => Err(format!(
            "{name} must be on or off: {other:?} (expected on, off, or unset)"
        )),
    }
}

fn parse_record_kinds(raw: Option<String>) -> Result<RecordKinds, String> {
    let mut kinds = RecordKinds::default();
    let Some(raw) = raw else {
        return Ok(kinds);
    };
    for value in raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match value {
            "metrics" => kinds.metrics = true,
            "text" => kinds.text = true,
            "audio" => kinds.audio = true,
            other => {
                return Err(format!(
                    "THALVI_RECORD contains an unknown value: {other:?} (expected a comma-separated subset of metrics, text, audio)"
                ));
            }
        }
    }
    Ok(kinds)
}

fn required(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &str,
    allow_empty: bool,
) -> Result<String, String> {
    match lookup(name) {
        Some(value) if allow_empty || !value.trim().is_empty() => Ok(value),
        Some(_) => Err(format!("{name} must not be empty")),
        None => Err(format!("{name} is not set")),
    }
}

fn http_base(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    let value = required(lookup, name, false)?;
    if !value.starts_with("http://") && !value.starts_with("https://") {
        return Err(format!(
            "{name} must be an http:// or https:// URL: {value:?}"
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::{BgLlm, Config, TtsApi};
    use std::collections::HashMap;

    fn full() -> HashMap<&'static str, String> {
        [
            ("THALVI_ADDR", "127.0.0.1:8080"),
            ("THALVI_WEB_DIR", "web/dist"),
            ("LLM_BASE_URL", "http://127.0.0.1:1234/v1/"),
            ("LLM_MODEL", "m"),
            ("LLM_API_KEY", ""),
            ("STT_URL", "wss://example.invalid/asr"),
            ("STT_MODEL", "s"),
            ("STT_API_KEY", ""),
            ("TTS_API", "openai"),
            ("TTS_BASE_URL", "http://127.0.0.1:8880/v1"),
            ("TTS_MODEL", "kokoro"),
            ("TTS_VOICE", "af_heart"),
            ("TTS_API_KEY", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect()
    }

    #[test]
    fn valid_config_loads_and_trims_base_urls() {
        let vars = full();
        let config = Config::from_lookup(|name| vars.get(name).cloned());
        assert_eq!(
            config.map(|c| (c.llm_base_url, c.addr.to_string(), c.llm_reasoning_effort)),
            Ok((
                "http://127.0.0.1:1234/v1".to_string(),
                "127.0.0.1:8080".to_string(),
                None
            ))
        );
    }

    #[test]
    fn blank_reasoning_effort_leaves_the_server_default() {
        for (value, expected) in [("", None), (" minimal ", Some("minimal".to_string()))] {
            let mut vars = full();
            vars.insert("LLM_REASONING_EFFORT", value.to_string());
            let config = Config::from_lookup(|name| vars.get(name).cloned());
            assert_eq!(config.map(|c| c.llm_reasoning_effort), Ok(expected));
        }
    }

    #[test]
    fn background_model_needs_all_three_variables_or_none() {
        let all = [
            ("BG_LLM_BASE_URL", "https://bg.invalid/v1/"),
            ("BG_LLM_MODEL", "title-model"),
            ("BG_LLM_API_KEY", "secret"),
        ];
        let cases = [
            (0..0, Ok(false)),
            (0..3, Ok(true)),
            (0..2, Err("BG_LLM_API_KEY")),
            (1..3, Err("BG_LLM_BASE_URL")),
        ];
        for (given, expected) in cases {
            let mut vars = full();
            vars.extend(
                all[given]
                    .iter()
                    .map(|(name, value)| (*name, (*value).to_string())),
            );
            let result = Config::from_lookup(|name| vars.get(name).cloned()).map(|c| c.bg_llm);
            match expected {
                Ok(true) => assert_eq!(
                    result,
                    Ok(Some(BgLlm {
                        base_url: "https://bg.invalid/v1".to_string(),
                        model: "title-model".to_string(),
                        api_key: "secret".to_string(),
                    }))
                ),
                Ok(false) => assert_eq!(result, Ok(None)),
                Err(name) => {
                    let err = result.unwrap_err();
                    assert!(
                        err.contains(name) && err.contains("together"),
                        "error {err:?} should name {name} and say they go together"
                    );
                }
            }
        }
    }

    #[test]
    fn background_model_url_must_be_http() {
        let mut vars = full();
        vars.extend([
            ("BG_LLM_BASE_URL", "ftp://x".to_string()),
            ("BG_LLM_MODEL", "m".to_string()),
            ("BG_LLM_API_KEY", String::new()),
        ]);
        let err = Config::from_lookup(|name| vars.get(name).cloned()).unwrap_err();
        assert!(err.contains("BG_LLM_BASE_URL"), "{err}");
    }

    #[test]
    fn tts_api_selects_the_protocol_and_its_settings() {
        let openai = TtsApi::OpenAi {
            model: "kokoro".to_string(),
            api_key: String::new(),
        };
        let cases = [
            (Some("openai"), vec![], Ok(openai)),
            (
                Some("qwen3"),
                vec!["TTS_MODEL", "TTS_API_KEY"],
                Ok(TtsApi::Qwen3),
            ),
            (Some("openai"), vec!["TTS_MODEL"], Err("TTS_MODEL")),
            (Some("kokoro"), vec![], Err("TTS_API")),
            (None, vec![], Err("TTS_API")),
        ];
        for (value, unset, expected) in cases {
            let mut vars = full();
            match value {
                Some(value) => vars.insert("TTS_API", value.to_string()),
                None => vars.remove("TTS_API"),
            };
            for name in unset {
                vars.remove(name);
            }
            let result = Config::from_lookup(|name| vars.get(name).cloned()).map(|c| c.tts_api);
            match expected {
                Ok(api) => assert_eq!(result, Ok(api), "value={value:?}"),
                Err(name) => {
                    let err = result.unwrap_err();
                    assert!(err.contains(name), "error {err:?} should name {name}");
                }
            }
        }
    }

    #[test]
    fn voices_default_to_tts_voice_and_keep_it_first() {
        type Expected<'a> = Result<Vec<&'a str>, &'a str>;
        let cases: [(Option<&str>, Expected); 7] = [
            (None, Ok(vec!["af_heart"])),
            (Some("  "), Ok(vec!["af_heart"])),
            (
                Some("sohee, af_bella"),
                Ok(vec!["af_heart", "sohee", "af_bella"]),
            ),
            (Some("af_bella,af_heart"), Ok(vec!["af_heart", "af_bella"])),
            (Some("sohee,,af_bella"), Err("empty name")),
            (Some("sohee,"), Err("empty name")),
            (Some("sohee, sohee"), Err("twice")),
        ];
        for (value, expected) in cases {
            let mut vars = full();
            if let Some(value) = value {
                vars.insert("TTS_VOICES", value.to_string());
            }
            let result = Config::from_lookup(|name| vars.get(name).cloned()).map(|c| c.tts_voices);
            match expected {
                Ok(names) => assert_eq!(
                    result,
                    Ok(names.into_iter().map(str::to_string).collect()),
                    "value={value:?}"
                ),
                Err(part) => {
                    let err = result.unwrap_err();
                    assert!(
                        err.contains("TTS_VOICES") && err.contains(part),
                        "error {err:?} should name TTS_VOICES and say {part:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn retirement_is_off_unless_given_a_positive_number_of_days() {
        for (value, expected) in [
            (None, Ok(None)),
            (Some(""), Ok(None)),
            (Some("off"), Ok(None)),
            (Some("90"), Ok(Some(90))),
            (Some("0"), Err(())),
            (Some("-1"), Err(())),
            (Some("often"), Err(())),
        ] {
            let mut vars = full();
            if let Some(value) = value {
                vars.insert("THALVI_RETIRE_DAYS", value.to_string());
            }
            let result = Config::from_lookup(|name| vars.get(name).cloned()).map(|c| c.retire_days);
            if let Ok(days) = expected {
                assert_eq!(result, Ok(days), "value={value:?}");
            } else {
                let error = result.unwrap_err();
                assert!(error.contains("THALVI_RETIRE_DAYS"), "{error}");
            }
        }
    }

    #[test]
    fn on_off_switches_parse_on_off_unset_and_reject_unknown_values() {
        type Switch = fn(&Config) -> bool;
        let switches: [(&str, Switch); 2] = [
            ("THALVI_ACK", |c| c.ack_enabled),
            ("LLM_AUDIO_INPUT", |c| c.llm_audio_input),
        ];
        let cases: Vec<(Option<&str>, Result<bool, ()>)> = vec![
            (None, Ok(false)),
            (Some(""), Ok(false)),
            (Some("off"), Ok(false)),
            (Some("on"), Ok(true)),
            (Some("yes"), Err(())),
        ];
        for (name, read) in switches {
            for (value, expected) in &cases {
                let mut vars = full();
                if let Some(value) = value {
                    vars.insert(name, value.to_string());
                }
                let result = Config::from_lookup(|key| vars.get(key).cloned()).map(|c| read(&c));
                if let Ok(expected) = expected {
                    assert_eq!(result, Ok(*expected), "{name}={value:?}");
                } else {
                    let err = result.unwrap_err();
                    assert!(
                        err.contains(name) && err.contains("on") && err.contains("off"),
                        "error should name {name} and expected values: {err}"
                    );
                }
            }
        }
    }

    #[test]
    fn record_kinds_parse_subsets_and_reject_unknown_values() {
        use crate::recorder::RecordKinds;
        let none = RecordKinds::default();
        let cases: Vec<(Option<&str>, Result<RecordKinds, ()>)> = vec![
            (None, Ok(none)),
            (Some(""), Ok(none)),
            (
                Some("metrics"),
                Ok(RecordKinds {
                    metrics: true,
                    ..none
                }),
            ),
            (
                Some("text, audio"),
                Ok(RecordKinds {
                    text: true,
                    audio: true,
                    ..none
                }),
            ),
            (
                Some("metrics,text,audio"),
                Ok(RecordKinds {
                    metrics: true,
                    text: true,
                    audio: true,
                }),
            ),
            (Some("video"), Err(())),
            (Some("metrics,video"), Err(())),
        ];
        for (value, expected) in cases {
            let mut vars = full();
            if let Some(value) = value {
                vars.insert("THALVI_RECORD", value.to_string());
            }
            let result = Config::from_lookup(|name| vars.get(name).cloned()).map(|c| c.record);
            if let Ok(kinds) = expected {
                assert_eq!(result, Ok(kinds), "value={value:?}");
            } else {
                let err = result.unwrap_err();
                assert!(
                    err.contains("THALVI_RECORD") && err.contains("video"),
                    "error should name the unknown value: {err}"
                );
            }
        }
    }

    #[test]
    fn missing_or_bad_variables_are_named() {
        let cases: Vec<(&str, Option<&str>)> = vec![
            ("THALVI_ADDR", None),
            ("THALVI_ADDR", Some("not-an-address")),
            ("LLM_BASE_URL", Some("ftp://x")),
            ("STT_URL", Some("https://x")),
            ("TTS_VOICE", Some("")),
        ];
        for (name, value) in cases {
            let mut vars = full();
            match value {
                Some(v) => {
                    vars.insert(name, v.to_string());
                }
                None => {
                    vars.remove(name);
                }
            }
            let err = Config::from_lookup(|n| vars.get(n).cloned())
                .map(|_| ())
                .unwrap_err();
            assert!(err.contains(name), "error {err:?} should name {name}");
        }
    }
}
