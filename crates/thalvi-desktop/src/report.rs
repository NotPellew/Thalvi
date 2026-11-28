//! The error report a user copies, and the headline an error shows where its full text does not fit.

use std::error::Error;
use std::fmt::Write as _;

use jiff::Timestamp;

use crate::config::DesktopConfig;

/// A certificate rejection, with advice that never asks the user to bypass verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CertificateProblem {
    #[error(
        "The server's certificate issuer is not trusted. Ask your IT team or server administrator to check the certificate chain and install the approved company CA in this computer's certificate store, then restart Thalvi."
    )]
    UntrustedIssuer,
    #[error(
        "The server's certificate has expired. Check this computer's date and time, or ask the server administrator to renew the certificate."
    )]
    Expired,
    #[error(
        "The server's certificate is not valid yet. Check this computer's date and time, or contact the server administrator."
    )]
    NotValidYet,
    #[error(
        "The server's certificate does not match its address. Check the configured server address, or ask the server administrator to correct the certificate."
    )]
    WrongHost,
    #[error(
        "The server's certificate could not be verified. Contact your IT team or server administrator to check the certificate."
    )]
    Invalid,
}

impl CertificateProblem {
    /// Finds a typed TLS certificate rejection through HTTP, WebSocket and I/O wrappers.
    #[must_use]
    pub fn from_error(error: &(dyn Error + 'static)) -> Option<Self> {
        std::iter::successors(Some(error), |cause| {
            if let Some(io) = cause.downcast_ref::<std::io::Error>() {
                io.get_ref().map(|inner| inner as &(dyn Error + 'static))
            } else {
                (*cause).source()
            }
        })
        .find_map(|cause| {
            let rustls::Error::InvalidCertificate(certificate) =
                cause.downcast_ref::<rustls::Error>()?
            else {
                return None;
            };
            Some(match certificate {
                rustls::CertificateError::UnknownIssuer => Self::UntrustedIssuer,
                rustls::CertificateError::Expired
                | rustls::CertificateError::ExpiredContext { .. } => Self::Expired,
                rustls::CertificateError::NotValidYet
                | rustls::CertificateError::NotValidYetContext { .. } => Self::NotValidYet,
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. } => Self::WrongHost,
                _ => Self::Invalid,
            })
        })
    }
}

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
    /// Names what only this failure knows, such as the model that fell behind, so it stays within
    /// [`MAX_HEADLINE_CHARS`].
    fn headline(&self) -> String;
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
    pub at: Timestamp,
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
        Self::at(Timestamp::now(), message, causes, version, system, config)
    }

    #[must_use]
    pub fn at(
        at: Timestamp,
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
            self.at.strftime("%Y-%m-%d %H:%M:%S UTC"),
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
    use super::*;
    use crate::recognizer::{ConnectFailure, Detail, RecognizerError, SpeechSource};

    #[test]
    fn certificate_causes_are_classified_through_io_without_matching_error_text() {
        for (certificate, expected) in [
            (
                rustls::CertificateError::UnknownIssuer,
                CertificateProblem::UntrustedIssuer,
            ),
            (
                rustls::CertificateError::Expired,
                CertificateProblem::Expired,
            ),
            (
                rustls::CertificateError::NotValidYet,
                CertificateProblem::NotValidYet,
            ),
            (
                rustls::CertificateError::NotValidForName,
                CertificateProblem::WrongHost,
            ),
            (
                rustls::CertificateError::Revoked,
                CertificateProblem::Invalid,
            ),
        ] {
            let error = std::io::Error::other(rustls::Error::InvalidCertificate(certificate));
            assert_eq!(CertificateProblem::from_error(&error), Some(expected));
        }
        let error = std::io::Error::other("invalid certificate: UnknownIssuer");
        assert_eq!(CertificateProblem::from_error(&error), None);
    }

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
                 Settings:\n- Speech recognition: the speech server \"Office\" at ws://127.0.0.1:9\n- Microphone: USB Mic\n",
            ),
            (
                "",
                vec![],
                "{}",
                "\
                 Error: Thalvi could not reach the speech server.\n\n\
                 Time: 2026-11-03 12:34:56 UTC\nThalvi: 1.2.3\nSystem: linux\n\n\
                 Settings:\n- Speech recognition: the speech model \"On this computer\" (nemotron_160ms)\n- Microphone: system default\n",
            ),
        ];
        for (desktop, causes, json, expected) in cases {
            let report = Report::at(
                "2026-11-03T12:34:56Z".parse().unwrap(),
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
            r#"{"writing": {"url": "https://example.com/v1/url-secret", "model": "m", "api_key": "sk-secret-key"}, "stt": {"entries": [{"kind": "realtime", "id": "r", "name": "Office", "active": true, "address": "ws://127.0.0.1:9", "model": "m", "api_key": "sk-secret-key"}]}}"#,
        );
        let report = Report::new("Failed.", &[], "1.2.3", system("GNOME"), &config).text();
        for secret in ["sk-secret-key", "url-secret", "example.com"] {
            assert!(!report.contains(secret), "{secret} in {report}");
        }
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
