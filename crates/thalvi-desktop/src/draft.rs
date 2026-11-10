//! The last dictation, kept in memory so the user can fix it before copying it.

use std::fmt::Display;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use crate::capture::CaptureRoute;
use crate::delivery::{Bound, Finished};
use crate::engine::CaptureOutcome;
use crate::history::{Capture, Outcome};

/// What the last delivery of the draft's text came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftOutcome {
    Delivered,
    /// The user cancelled; nothing was pasted.
    Cancelled,
    Failed,
    /// It is not known whether the paste reached the window.
    MaybePartial,
}

/// There is no draft to edit. Shown to the user as written.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("There is no dictation to work on.")]
pub struct NoDraft;

/// A draft as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftView {
    /// Changes whenever a different dictation becomes the draft, so the UI knows to reload the text.
    pub id: u64,
    pub original: String,
    /// The text as the user edited it; this is what Copy uses.
    pub text: String,
    /// The window the dictation was meant for, when one was bound.
    pub target: Option<String>,
    pub outcome: DraftOutcome,
    /// A newer dictation ended while this one had edits; discarding this one shows it.
    pub newer_waiting: bool,
}

/// One dictation: what was heard, the user's edits and where it was meant to go.
#[derive(Clone, Debug)]
pub struct Draft {
    id: u64,
    original: String,
    text: String,
    target: Option<String>,
    outcome: DraftOutcome,
}

impl Draft {
    /// The draft a finished capture leaves, bound to the window it started in. `None` when no
    /// text was heard, and for tasks, whose text is a command rather than something to paste.
    #[must_use]
    pub fn of<T: Display>(
        outcome: &CaptureOutcome,
        finished: Option<&Finished>,
        bound: &Bound<T>,
    ) -> Option<Self> {
        if matches!(outcome, CaptureOutcome::Accepted(request) if request.route == CaptureRoute::Task)
        {
            return None;
        }
        let capture = Capture::of(outcome, finished, bound.window())?;
        let outcome = match capture.outcome {
            Outcome::Accepted => DraftOutcome::Delivered,
            Outcome::Cancelled => DraftOutcome::Cancelled,
            Outcome::Failed if finished.is_some_and(|finished| finished.maybe_partial) => {
                DraftOutcome::MaybePartial
            }
            Outcome::Failed => DraftOutcome::Failed,
        };
        Some(Self {
            id: 0,
            original: capture.text.clone(),
            text: capture.text,
            target: capture.target,
            outcome,
        })
    }
}

#[derive(Default)]
struct Slot {
    current: Option<Draft>,
    /// The latest dictation that ended while `current` was edited.
    newer: Option<Draft>,
    last_id: u64,
}

/// The one last draft, shared between the engine that offers new dictations and the commands that
/// edit it.
#[derive(Default)]
pub struct Drafts {
    slot: Mutex<Slot>,
}

impl Drafts {
    fn slot(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `draft` the last dictation, unless the current one was edited: then it waits until
    /// the user discards the current one.
    pub fn offer(&self, mut draft: Draft) {
        let mut slot = self.slot();
        slot.last_id += 1;
        draft.id = slot.last_id;
        if slot
            .current
            .as_ref()
            .is_some_and(|current| current.text != current.original)
        {
            slot.newer = Some(draft);
        } else {
            slot.current = Some(draft);
            slot.newer = None;
        }
    }

    #[must_use]
    pub fn view(&self) -> Option<DraftView> {
        let slot = self.slot();
        slot.current.as_ref().map(|draft| DraftView {
            id: draft.id,
            original: draft.original.clone(),
            text: draft.text.clone(),
            target: draft.target.clone(),
            outcome: draft.outcome,
            newer_waiting: slot.newer.is_some(),
        })
    }

    /// Stores the edited `text`; nothing is recognized, copied or sent.
    ///
    /// # Errors
    /// [`NoDraft`] when there is no draft.
    pub fn edit(&self, text: String) -> Result<(), NoDraft> {
        self.slot()
            .current
            .as_mut()
            .map(|draft| draft.text = text)
            .ok_or(NoDraft)
    }

    /// Drops the draft; a newer dictation that waited takes its place.
    pub fn discard(&self) {
        let mut slot = self.slot();
        slot.current = slot.newer.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureRequest;
    use crate::config::DesktopConfig;
    use crate::delivery::{Delivery, fakes::FakeHost};
    use crate::engine::CaptureError;
    use crate::host::HostError;
    use crate::metrics::Recorder;

    fn paste_outcome(text: &str) -> CaptureOutcome {
        CaptureOutcome::Accepted(CaptureRequest {
            route: CaptureRoute::Paste,
            text: text.to_owned(),
        })
    }

    fn cancelled(text: &str) -> CaptureOutcome {
        CaptureOutcome::Cancelled {
            dictated: Some(text.to_owned()),
        }
    }

    /// The draft a capture that ended with `outcome` leaves on `host`, after its delivery.
    async fn left_by(
        host: FakeHost,
        outcome: &CaptureOutcome,
    ) -> (Option<Draft>, Delivery<FakeHost>) {
        let delivery = Delivery::new(host);
        let bound = delivery.bind(&Recorder::disabled()).await;
        let config = DesktopConfig::from_json_str("{}").unwrap();
        let finished = delivery
            .finish(outcome, &bound, &config, &Recorder::disabled())
            .await;
        (Draft::of(outcome, finished.as_ref(), &bound), delivery)
    }

    async fn offered(host: FakeHost, outcome: &CaptureOutcome) -> (Drafts, Delivery<FakeHost>) {
        let (draft, delivery) = left_by(host, outcome).await;
        let drafts = Drafts::default();
        if let Some(draft) = draft {
            drafts.offer(draft);
        }
        (drafts, delivery)
    }

    async fn newer(text: &str) -> Draft {
        left_by(FakeHost::default(), &cancelled(text))
            .await
            .0
            .unwrap()
    }

    #[tokio::test]
    async fn each_way_a_capture_ends_leaves_a_draft_with_the_outcome_observed() {
        let failed = |text: Option<&str>| CaptureOutcome::Failed {
            error: CaptureError::MicStopped,
            recoverable_text: text.map(str::to_owned),
        };
        let deliver_fails = |error| FakeHost::default().failing("deliver", error);
        for (label, host, outcome, expected) in [
            (
                "delivered",
                FakeHost::default(),
                paste_outcome("hello"),
                Some(DraftOutcome::Delivered),
            ),
            (
                "paste refused",
                deliver_fails(HostError::NothingTyped("Gone.".to_owned())),
                paste_outcome("hello"),
                Some(DraftOutcome::Failed),
            ),
            (
                "paste may or may not have happened",
                deliver_fails(HostError::MaybePartial("Partway.".to_owned())),
                paste_outcome("hello"),
                Some(DraftOutcome::MaybePartial),
            ),
            (
                "capture failed with text",
                FakeHost::default(),
                failed(Some("hello")),
                Some(DraftOutcome::Failed),
            ),
            (
                "cancelled with text",
                FakeHost::default(),
                cancelled("hello"),
                Some(DraftOutcome::Cancelled),
            ),
            (
                "cancelled without text",
                FakeHost::default(),
                CaptureOutcome::Cancelled { dictated: None },
                None,
            ),
            (
                "failed without text",
                FakeHost::default(),
                failed(None),
                None,
            ),
            ("blank text", FakeHost::default(), paste_outcome("  "), None),
            (
                "a task",
                FakeHost::default(),
                CaptureOutcome::Accepted(CaptureRequest {
                    route: CaptureRoute::Task,
                    text: "switch to editor".to_owned(),
                }),
                None,
            ),
        ] {
            let view = offered(host, &outcome).await.0.view();
            assert_eq!(view.as_ref().map(|view| view.outcome), expected, "{label}");
            if let Some(view) = view {
                assert_eq!(
                    (
                        view.original.as_str(),
                        view.text.as_str(),
                        view.target.as_deref()
                    ),
                    ("hello", "hello", Some("the editor")),
                    "{label}"
                );
            }
        }
    }

    #[tokio::test]
    async fn editing_keeps_the_original_and_calls_nothing() {
        let (drafts, delivery) = offered(FakeHost::default(), &cancelled("hello wrld")).await;
        let before = delivery.host().calls();
        drafts.edit("hello world".to_owned()).unwrap();
        let view = drafts.view().unwrap();
        assert_eq!(
            (view.original.as_str(), view.text.as_str(), view.outcome),
            ("hello wrld", "hello world", DraftOutcome::Cancelled)
        );
        assert_eq!(delivery.host().calls(), before);
    }

    #[tokio::test]
    async fn a_newer_dictation_waits_while_the_draft_is_edited() {
        let shown = |drafts: &Drafts| drafts.view().map(|view| (view.text, view.newer_waiting));
        let some = |text: &str, waiting| Some((text.to_owned(), waiting));

        // Without edits the newer dictation replaces it, even when the first was never delivered.
        let (drafts, _) = offered(FakeHost::default(), &cancelled("first")).await;
        drafts.offer(newer("second").await);
        assert_eq!(shown(&drafts), some("second", false));

        // With edits it waits; the latest waiting one is shown once the edited draft is discarded.
        drafts.edit("second fixed".to_owned()).unwrap();
        drafts.offer(newer("third").await);
        drafts.offer(newer("fourth").await);
        assert_eq!(shown(&drafts), some("second fixed", true));
        drafts.discard();
        assert_eq!(shown(&drafts), some("fourth", false));
        drafts.discard();
        assert_eq!(shown(&drafts), None);

        // Edits to a delivered dictation count too; reverting them frees the draft.
        let (drafts, _) = offered(FakeHost::default(), &paste_outcome("sent")).await;
        drafts.edit("sent fixed".to_owned()).unwrap();
        drafts.offer(newer("next").await);
        assert_eq!(shown(&drafts), some("sent fixed", true));
        drafts.edit("sent".to_owned()).unwrap();
        drafts.offer(newer("after revert").await);
        assert_eq!(shown(&drafts), some("after revert", false));
    }

    #[tokio::test]
    async fn discard_drops_the_draft() {
        let (drafts, _) = offered(FakeHost::default(), &cancelled("hello")).await;
        drafts.discard();
        assert_eq!(drafts.view(), None);
        assert_eq!(drafts.edit("x".to_owned()), Err(NoDraft));
    }
}
