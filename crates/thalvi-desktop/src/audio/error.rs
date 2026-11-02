//! What can go wrong with the microphone, in words the user can act on.

use std::fmt;

use cpal::ErrorKind;

/// A microphone problem; `Display` is the plain notice shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// No input device exists.
    NoDevice,
    /// The microphone chosen in the settings is not connected.
    DeviceMissing(String),
    /// The system refused access to the microphone.
    PermissionDenied,
    /// Another app holds the microphone.
    Busy,
    /// The microphone exists but could not be opened.
    OpenFailed(cpal::Error),
    /// The microphone was unplugged or disabled while capturing.
    DeviceLost,
    /// The default microphone changed while capturing.
    DeviceChanged,
    /// The engine stopped reading for ten seconds and the queued audio overflowed.
    Dropped,
    /// The stream failed for another reason.
    StreamFailed(cpal::Error),
    /// The microphone was paused or closed.
    Closed,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoDevice => {
                "No microphone was found. Plug one in or set a default input in the system sound settings."
            }
            Self::DeviceMissing(name) => {
                return write!(
                    f,
                    "The microphone {name:?} is not connected. Plug it in or choose another one in Thalvi's settings."
                );
            }
            Self::PermissionDenied => {
                "The microphone is blocked. Allow Thalvi to use it in the system privacy settings."
            }
            Self::Busy => "The microphone is in use by another app.",
            Self::OpenFailed(_) => {
                "The microphone could not be opened. Check the device and its permission."
            }
            Self::DeviceLost => "The microphone was disconnected. Capture stopped without delivery.",
            Self::DeviceChanged => {
                "The default microphone changed. Capture stopped without delivery."
            }
            Self::Dropped => {
                "Thalvi could not keep up with the microphone audio. Capture stopped without delivery."
            }
            Self::StreamFailed(_) => "The microphone stopped working. Capture stopped without delivery.",
            Self::Closed => "The microphone is off.",
        })
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenFailed(error) | Self::StreamFailed(error) => Some(error),
            _ => None,
        }
    }
}

impl AudioError {
    /// Maps a failure to open the stream.
    pub(super) fn from_open(error: cpal::Error) -> Self {
        match error.kind() {
            ErrorKind::DeviceNotAvailable => Self::NoDevice,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::DeviceBusy => Self::Busy,
            _ => Self::OpenFailed(error),
        }
    }

    /// Maps an error reported by a running stream; `None` means capture is unaffected.
    pub(super) fn from_stream(error: cpal::Error) -> Option<Self> {
        match error.kind() {
            ErrorKind::DeviceNotAvailable => Some(Self::DeviceLost),
            ErrorKind::DeviceChanged | ErrorKind::StreamInvalidated => Some(Self::DeviceChanged),
            // A discontinuity is a gap in the audio, not the end of the stream.
            ErrorKind::Xrun | ErrorKind::RealtimeDenied => None,
            _ => Some(Self::StreamFailed(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_failures_name_the_cause() {
        let cases = [
            (ErrorKind::DeviceNotAvailable, AudioError::NoDevice),
            (ErrorKind::PermissionDenied, AudioError::PermissionDenied),
            (ErrorKind::DeviceBusy, AudioError::Busy),
        ];
        for (kind, expected) in cases {
            assert_eq!(AudioError::from_open(kind.into()), expected, "{kind:?}");
        }
        let other = cpal::Error::new(ErrorKind::UnsupportedConfig);
        assert_eq!(
            AudioError::from_open(other.clone()),
            AudioError::OpenFailed(other)
        );
    }

    #[test]
    fn running_stream_errors_stop_capture_unless_harmless() {
        let cases = [
            (ErrorKind::DeviceNotAvailable, Some(AudioError::DeviceLost)),
            (ErrorKind::DeviceChanged, Some(AudioError::DeviceChanged)),
            (
                ErrorKind::StreamInvalidated,
                Some(AudioError::DeviceChanged),
            ),
            (ErrorKind::Xrun, None),
            (ErrorKind::RealtimeDenied, None),
        ];
        for (kind, expected) in cases {
            assert_eq!(AudioError::from_stream(kind.into()), expected, "{kind:?}");
        }
        let other = cpal::Error::new(ErrorKind::BackendError);
        assert_eq!(
            AudioError::from_stream(other.clone()),
            Some(AudioError::StreamFailed(other))
        );
    }

    #[test]
    fn a_missing_chosen_microphone_is_named_in_the_notice() {
        assert_eq!(
            AudioError::DeviceMissing("USB Mic".to_owned()).to_string(),
            "The microphone \"USB Mic\" is not connected. Plug it in or choose another one in Thalvi's settings."
        );
    }

    #[test]
    fn notices_are_plain_words_without_backend_detail() {
        let detail = cpal::Error::with_message(ErrorKind::BackendError, "snd_pcm_readi EIO");
        for error in [
            AudioError::OpenFailed(detail.clone()),
            AudioError::StreamFailed(detail),
        ] {
            assert!(!error.to_string().contains("snd_pcm"), "{error}");
            assert!(std::error::Error::source(&error).is_some());
        }
    }
}
