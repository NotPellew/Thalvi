//! The desktop's report that it has shown the full final Submit text.

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PreviewError {
    #[error("The full text did not fit in the Submit preview. Nothing was submitted.")]
    TooLong,
    #[error("The Submit preview could not be shown. Nothing was submitted.")]
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Presentation {
    #[default]
    Waiting,
    Shown,
    Failed(PreviewError),
}

/// A report for one particular final Submit preview.
#[derive(Clone, Debug, Default)]
pub struct SubmitPreview(watch::Sender<Presentation>);

impl SubmitPreview {
    pub(super) fn watch(&self) -> watch::Receiver<Presentation> {
        self.0.subscribe()
    }

    /// The desktop attached a frame with the full text.
    pub fn shown(&self) {
        self.0.send_if_modified(|state| {
            if *state != Presentation::Waiting {
                return false;
            }
            *state = Presentation::Shown;
            true
        });
    }

    /// The full text cannot fit, or its surface could not be presented.
    pub fn failed(&self, error: PreviewError) {
        self.0.send_if_modified(|state| {
            if matches!(state, Presentation::Failed(_)) {
                return false;
            }
            *state = Presentation::Failed(error);
            true
        });
    }
}

impl PartialEq for SubmitPreview {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}

impl Eq for SubmitPreview {}
