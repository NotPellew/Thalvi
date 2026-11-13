//! The error report a user copies, and the headline an error shows where its full text does not fit.

use std::error::Error;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};

use crate::config::DesktopConfig;

/// The messages of every error behind `error`, outermost first, without `error` itself.
#[must_use]
pub fn causes(error: &(dyn Error + 'static)) -> Vec<String> {
    std::iter::successors(error.source(), |cause| (*cause).source())
        .map(ToString::to_string)
        .collect()
}

/// The most characters a headline has, so the overlay's narrowest look shows it without scrolling.
pub const MAX_HEADLINE_CHARS: usize = 36;

/// An error the user is shown: `Display` is the full text for Settings and the report, and
/// `headline` a line short enough for the overlay to show whole.
pub trait UserError: Error {
    fn headline(&self) -> &'static str;
}

/// The computer Thalvi runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct System {
    pub os: String,
    /// The desktop session, such as `Hyprland` or `GNOME`; empty when the OS does not name one.
    pub desktop: String,
}

impl System {
    #[must_use]
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        }
    }
}

/// Everything a copied error report says. Settings come from a fixed list, so a key or other
/// secret in the config can never reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub message: String,
    pub causes: Vec<String>,
    pub at: DateTime<Utc>,
    pub version: String,
    pub system: System,
    settings: Vec<(&'static str, String)>,
}

impl Report {
    #[must_use]
    pub fn new(
        message: &str,
        causes: &[String],
        version: &str,
        system: System,
        config: &DesktopConfig,
    ) -> Self {
        Self::at(Utc::now(), message, causes, version, system, config)
    }

    #[must_use]
    pub fn at(
        at: DateTime<Utc>,
        message: &str,
        causes: &[String],
        version: &str,
        system: System,
        config: &DesktopConfig,
    ) -> Self {
        let mut settings = vec![("Speech recognition", config.speech_source().to_string())];
        settings.push((
            "Microphone",
            config
                .microphone
                .clone()
                .unwrap_or_else(|| "system default".to_owned()),
        ));
        let second = config.stt.entries.ticked().next().is_some();
        settings.push((
            "Extra recognizers",
            if second { "on" } else { "off" }.to_owned(),
        ));
        Self {
            message: message.to_owned(),
            causes: causes.to_vec(),
            at,
            version: version.to_owned(),
            system,
            settings,
        }
    }

    /// The text for the clipboard.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("Error: {}\n", self.message);
        if !self.causes.is_empty() {
            text.push_str("\nTechnical cause:\n");
            for cause in &self.causes {
                let _ = writeln!(text, "- {cause}");
            }
        }
        let _ = write!(
            text,
            "\nTime: {}\nThalvi: {}\nSystem: {}{}{}\n\nSettings:\n",
            self.at.format("%Y-%m-%d %H:%M:%S UTC"),
            self.version,
            self.system.os,
            if self.system.desktop.is_empty() {
                ""
            } else {
                ", "
            },
            self.system.desktop,
        );
        for (name, value) in &self.settings {
            let _ = writeln!(text, "- {name}: {value}");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;
    use crate::recognizer::{ConnectFailure, Detail, RecognizerError, SpeechSource};

    fn config(json: &str) -> DesktopConfig {
        DesktopConfig::from_json_str(json).unwrap()
    }

    fn system(desktop: &str) -> System {
        System {
            os: "linux".to_owned(),
            desktop: desktop.to_owned(),
        }
    }

    #[test]
    fn the_report_carries_the_message_causes_time_version_system_and_settings() {
        let cases = [
            (
                "Hyprland",
                vec!["connection refused (os error 111)".to_owned()],
                r#"{"stt":{"entries":[{"kind":"realtime","id":"r","name":"Office","active":true,"address":"ws://127.0.0.1:9","model":"m"}]},"microphone":"USB Mic"}"#,
                "\
                 Error: Thalvi could not reach the speech server.\n\n\
                 Technical cause:\n- connection refused (os error 111)\n\n\
                 Time: 2026-11-03 12:34:56 UTC\nThalvi: 1.2.3\nSystem: linux, Hyprland\n\n\
                 Settings:\n- Speech recognition: the speech server \"Office\" at ws://127.0.0.1:9\n- Microphone: USB Mic\n- Extra recognizers: off\n",
            ),
            (
                "",
                vec![],
                r#"{"stt": {"engine": "local"}}"#,
                "\
                 Error: Thalvi could not reach the speech server.\n\n\
                 Time: 2026-11-03 12:34:56 UTC\nThalvi: 1.2.3\nSystem: linux\n\n\
                 Settings:\n- Speech recognition: the speech model \"On this computer\" (nemotron_160ms)\n- Microphone: system default\n- Extra recognizers: off\n",
            ),
        ];
        for (desktop, causes, json, expected) in cases {
            let report = Report::at(
                Utc.with_ymd_and_hms(2026, 11, 3, 12, 34, 56).unwrap(),
                "Thalvi could not reach the speech server.",
                &causes,
                "1.2.3",
                system(desktop),
                &config(json),
            );
            assert_eq!(report.text(), expected, "{json}");
        }
    }

    #[test]
    fn a_configured_key_or_endpoint_never_reaches_the_report() {
        let config = config(
            r#"{"writing": {"url": "http://writing.invalid/v1", "model": "m"}, "stt": {"secondary": {"enabled": true, "url": "https://example.com/v1/url-secret", "model": "m", "api_key": "sk-secret-key", "api": "openai"}}}"#,
        );
        let report = Report::new("Failed.", &[], "1.2.3", system("GNOME"), &config).text();
        for secret in ["sk-secret-key", "url-secret", "example.com"] {
            assert!(!report.contains(secret), "{secret} in {report}");
        }
        assert!(report.contains("- Extra recognizers: on"), "{report}");
    }

    #[test]
    fn the_cause_chain_lists_each_source_below_the_error_itself() {
        let error = RecognizerError::Unreachable {
            speech: SpeechSource::direct("Office", "ws://127.0.0.1:8080".to_owned()),
            reason: ConnectFailure::Refused,
            detail: Detail("IO error: Connection refused (os error 111)".to_owned()),
        };
        assert_eq!(
            causes(&error),
            ["IO error: Connection refused (os error 111)"]
        );
        assert_eq!(
            causes(&RecognizerError::said("No model.")),
            Vec::<String>::new()
        );
    }
}
