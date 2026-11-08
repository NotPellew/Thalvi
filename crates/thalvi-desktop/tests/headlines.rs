//! Every error the overlay can show has a headline that fits the narrowest look without scrolling.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::time::Duration;

use thalvi_desktop::audio::AudioError;
use thalvi_desktop::capture::CaptureFullError;
use thalvi_desktop::engine::{CaptureError, ListenError};
use thalvi_desktop::host::HostError;
use thalvi_desktop::metrics::RecorderError;
use thalvi_desktop::models::{ModelError, ModelKind};
use thalvi_desktop::recognizer::{
    ConnectFailure, Detail, RecognizerError, ServerUrl, SpeechSource,
};
use thalvi_desktop::report::{MAX_HEADLINE_CHARS, UserError};
use thalvi_desktop::tasks::NotConfigured;
use thalvi_desktop::wake::WakeError;

type Cases = Vec<(Box<dyn UserError>, &'static str)>;

fn device(kind: cpal::ErrorKind) -> cpal::Error {
    cpal::Error::with_message(kind, "device")
}

fn download() -> reqwest::Error {
    reqwest::Client::new()
        .get("not a url")
        .build()
        .expect_err("a relative URL is refused")
}

fn office() -> SpeechSource {
    SpeechSource::thalvi_server("Office", &ServerUrl::default())
}

fn unreachable() -> RecognizerError {
    RecognizerError::Unreachable {
        speech: office(),
        reason: ConnectFailure::Refused,
        detail: Detail("refused".to_owned()),
    }
}

fn recognition() -> Cases {
    let server = office;
    let local = || SpeechSource::on_device("On this computer", "nemotron".to_owned());
    let seconds = Duration::from_secs(30);
    vec![
        (
            Box::new(unreachable()),
            "Can't reach speech server — Settings",
        ),
        (
            Box::new(RecognizerError::Dropped { speech: office() }),
            "Speech server hung up — Settings",
        ),
        (
            Box::new(RecognizerError::NotUnderstood { speech: office() }),
            "Bad speech server reply — Settings",
        ),
        (
            Box::new(RecognizerError::said("No model.")),
            "Speech recognition failed — Settings",
        ),
        (
            Box::new(RecognizerError::from(ModelError::Download {
                kind: ModelKind::Speech,
                file: "f".to_owned(),
                source: download(),
            })),
            "Download failed — check internet",
        ),
        (
            Box::new(CaptureError::Recognizer(unreachable())),
            "Can't reach speech server — Settings",
        ),
        (
            Box::new(CaptureError::NotReady {
                waited: seconds,
                speech: server(),
            }),
            "Speech server not ready — Settings",
        ),
        (
            Box::new(CaptureError::NotReady {
                waited: seconds,
                speech: local(),
            }),
            "Speech model not ready — Settings",
        ),
        (
            Box::new(CaptureError::Audio(AudioError::Busy)),
            "Microphone is used by another app",
        ),
        (
            Box::new(CaptureError::MicStopped),
            "Microphone stopped mid-capture",
        ),
        (Box::new(CaptureError::NoEnding(seconds)), "No words heard"),
        (
            Box::new(CaptureError::HoldTooShort),
            "Key released too soon",
        ),
        (Box::new(CaptureError::NothingHeard), "No words heard"),
        (
            Box::new(CaptureError::TooLong(seconds)),
            "Capture too long — not delivered",
        ),
        (
            Box::new(CaptureError::Full(CaptureFullError)),
            "Capture full — finish or cancel",
        ),
    ]
}

fn microphone() -> Cases {
    vec![
        (
            Box::new(AudioError::NoDevice),
            "No microphone found — plug one in",
        ),
        (
            Box::new(AudioError::DeviceMissing("USB".to_owned())),
            "Microphone not connected — Settings",
        ),
        (
            Box::new(AudioError::PermissionDenied),
            "Microphone blocked by the system",
        ),
        (
            Box::new(AudioError::OpenFailed(device(
                cpal::ErrorKind::DeviceNotAvailable,
            ))),
            "Can't open microphone — Settings",
        ),
        (Box::new(AudioError::DeviceLost), "Microphone disconnected"),
        (
            Box::new(AudioError::DeviceChanged),
            "Default microphone changed",
        ),
        (
            Box::new(AudioError::Dropped),
            "Can't keep up with the microphone",
        ),
        (
            Box::new(AudioError::StreamFailed(device(
                cpal::ErrorKind::DeviceNotAvailable,
            ))),
            "Microphone stopped working",
        ),
        (Box::new(AudioError::Closed), "Microphone is off"),
        (
            Box::new(ListenError::Audio(AudioError::Busy)),
            "Microphone is used by another app",
        ),
        (
            Box::new(ListenError::Wake(WakeError::Inference("x".to_owned()))),
            "Wake word detection failed",
        ),
    ]
}

fn everything_else() -> Cases {
    vec![
        (
            Box::new(WakeError::Model(ModelError::NoDataDir)),
            "No folder to keep models in",
        ),
        (
            Box::new(WakeError::Runtime("x".to_owned())),
            "Wake word engine won't start",
        ),
        (
            Box::new(WakeError::Load {
                model: "m".to_owned(),
                reason: "r".to_owned(),
            }),
            "Wake model can't be loaded",
        ),
        (
            Box::new(WakeError::Head {
                model: "m".to_owned(),
                reason: "r",
            }),
            "Wake model can't be used",
        ),
        (
            Box::new(ModelError::Download {
                kind: ModelKind::Speech,
                file: "f".to_owned(),
                source: download(),
            }),
            "Download failed — check internet",
        ),
        (
            Box::new(ModelError::Checksum {
                kind: ModelKind::Wake,
                file: "f".to_owned(),
            }),
            "Downloaded model was damaged",
        ),
        (
            Box::new(ModelError::Replaced {
                kind: ModelKind::Wake,
                file: "f".to_owned(),
                cause: Box::new(ModelError::NoDataDir),
            }),
            "Stored model was damaged",
        ),
        (
            Box::new(ModelError::AlreadyDownloading {
                name: "n".to_owned(),
            }),
            "Model is already downloading",
        ),
        (
            Box::new(ModelError::Io {
                kind: ModelKind::Wake,
                file: "f".to_owned(),
                dir: "/tmp".into(),
                source: std::io::Error::other("full"),
            }),
            "Can't save the model file",
        ),
        (
            Box::new(RecorderError::NoDataDir),
            "No folder to record into",
        ),
        (
            Box::new(RecorderError::Writer(std::io::Error::other("full"))),
            "Recording can't start",
        ),
        (
            Box::new(HostError::Unsupported("x".to_owned())),
            "Desktop can't do that — Settings",
        ),
        (
            Box::new(HostError::NothingTyped("x".to_owned())),
            "Nothing was typed — Settings",
        ),
        (
            Box::new(HostError::MaybePartial("x".to_owned())),
            "Some input may have been typed",
        ),
        (Box::new(NotConfigured), "That task isn't configured yet"),
    ]
}

#[test]
fn each_error_kind_has_a_headline_that_fits_the_overlay() {
    let cases = recognition()
        .into_iter()
        .chain(microphone())
        .chain(everything_else());
    for (error, headline) in cases {
        assert_eq!(error.headline(), headline, "{error}");
        assert!(
            headline.chars().count() <= MAX_HEADLINE_CHARS,
            "{headline} is longer than {MAX_HEADLINE_CHARS} characters"
        );
    }
}
