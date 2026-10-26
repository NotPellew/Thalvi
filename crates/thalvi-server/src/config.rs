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
    /// Recognizer WebSocket URL.
    pub stt_url: String,
    /// Recognizer model name.
    pub stt_model: String,
    /// Recognizer API key.
    pub stt_api_key: String,
    /// Speech synthesis base URL, without a trailing slash.
    pub tts_base_url: String,
    /// Speech synthesis model name.
    pub tts_model: String,
    /// Speech synthesis voice name.
    pub tts_voice: String,
    /// Speech API key; empty means no Authorization header.
    pub tts_api_key: String,
    /// Which recording kinds are enabled (`THALVI_RECORD`).
    pub record: RecordKinds,
    /// Directory recordings are written under (`THALVI_DATA_DIR`).
    pub data_dir: PathBuf,
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
        let stt_url = required(&lookup, "STT_URL", false)?;
        if !stt_url.starts_with("ws://") && !stt_url.starts_with("wss://") {
            return Err(format!(
                "STT_URL must be a ws:// or wss:// URL: {stt_url:?}"
            ));
        }
        let stt_model = required(&lookup, "STT_MODEL", false)?;
        let stt_api_key = required(&lookup, "STT_API_KEY", true)?;
        let tts_base_url = http_base(&lookup, "TTS_BASE_URL")?;
        let tts_model = required(&lookup, "TTS_MODEL", false)?;
        let tts_voice = required(&lookup, "TTS_VOICE", false)?;
        let tts_api_key = required(&lookup, "TTS_API_KEY", true)?;
        let record = parse_record_kinds(lookup("THALVI_RECORD"))?;
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
            stt_url,
            stt_model,
            stt_api_key,
            tts_base_url,
            tts_model,
            tts_voice,
            tts_api_key,
            record,
            data_dir: PathBuf::from(data_dir),
        })
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
    use super::Config;
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
