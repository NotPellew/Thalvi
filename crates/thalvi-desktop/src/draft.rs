//! The last dictation, kept in memory so the user can fix it, copy it or deliberately paste it again.

use std::sync::{Mutex, PoisonError};

use serde::Serialize;

use crate::capture::CaptureRoute;
use crate::delivery::{Bound, Delivery, Finished};
use crate::engine::CaptureOutcome;
use crate::history::{Capture, Outcome};
use crate::host::{Host, HostError};
use crate::metrics::Recorder;

/// What the last delivery of the draft's text came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftOutcome {
    Delivered,
    /// The user cancelled; nothing was typed and nothing is sent unless the user asks.
    Cancelled,
    Failed,
    /// Typing stopped partway: some of the text may already be in the window.
    MaybePartial,
}

/// Why a draft action was refused. Messages are shown to the user as written.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DraftError {
    #[error("There is no dictation to work on.")]
    NoDraft,
    #[error("There is no text. Type some, or discard this draft.")]
    Empty,
    #[error(
        "No window was bound when you dictated this, so there is nowhere to paste it. Copy it instead."
    )]
    NoTarget,
    #[error("Thalvi is capturing or delivering right now. Try again when it has finished.")]
    CaptureActive,
    #[error("The text is already being pasted.")]
    Pasting,
    #[error("{0}")]
    Host(HostError),
}

/// A draft as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftView {
    /// Changes whenever a different dictation becomes the draft, so the UI knows to reload the text.
    pub id: u64,
    pub original: String,
    /// The text as the user edited it; this is what Copy and Paste use.
    pub text: String,
    /// The window Paste would type into.
    pub target: Option<String>,
    pub outcome: DraftOutcome,
    /// A newer dictation ended while this one had unsent edits; discarding this one shows it.
    pub newer_waiting: bool,
    pub pasting: bool,
}

/// One dictation: what was heard, the user's edits and where it was meant to go.
#[derive(Clone, Debug)]
pub struct Draft<T> {
    id: u64,
    original: String,
    text: String,
    target: Option<T>,
    outcome: DraftOutcome,
    /// What the user last pasted, or the original when it was delivered.
    sent: Option<String>,
}

impl<T: Clone + std::fmt::Display> Draft<T> {
    /// The draft a finished capture leaves, bound to the window it started in. `None` when no
    /// text was heard, and for tasks, whose text is a command rather than something to type.
    #[must_use]
    pub fn of(
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
            sent: (outcome == DraftOutcome::Delivered).then(|| capture.text.clone()),
            original: capture.text.clone(),
            text: capture.text,
            target: bound.target().cloned(),
            outcome,
        })
    }

    fn has_unsent_edits(&self) -> bool {
        self.text != self.original && self.sent.as_deref() != Some(self.text.as_str())
    }
}

struct Slot<T> {
    current: Option<Draft<T>>,
    /// The latest dictation that ended while `current` could not be replaced.
    newer: Option<Draft<T>>,
    pasting: bool,
    last_id: u64,
}

/// The one last draft, shared between the engine that offers new dictations and the commands that
/// edit it.
pub struct Drafts<T> {
    slot: Mutex<Slot<T>>,
}

impl<T> Default for Drafts<T> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(Slot {
                current: None,
                newer: None,
                pasting: false,
                last_id: 0,
            }),
        }
    }
}

impl<T: Clone + std::fmt::Display> Drafts<T> {
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot<T>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `draft` the last dictation, unless the current one has unsent edits or is being
    /// pasted: then it waits until the user discards the current one.
    pub fn offer(&self, mut draft: Draft<T>) {
        let mut slot = self.slot();
        slot.last_id += 1;
        draft.id = slot.last_id;
        let holds = slot.pasting || slot.current.as_ref().is_some_and(Draft::has_unsent_edits);
        if holds {
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
            target: draft.target.as_ref().map(ToString::to_string),
            outcome: draft.outcome,
            newer_waiting: slot.newer.is_some(),
            pasting: slot.pasting,
        })
    }

    /// Stores the edited `text`; nothing is recognized, copied or sent.
    ///
    /// # Errors
    /// [`DraftError::NoDraft`] when there is no draft.
    pub fn edit(&self, text: String) -> Result<(), DraftError> {
        self.slot()
            .current
            .as_mut()
            .map(|draft| draft.text = text)
            .ok_or(DraftError::NoDraft)
    }

    /// Drops the draft; a newer dictation that waited takes its place.
    pub fn discard(&self) {
        let mut slot = self.slot();
        slot.current = slot.newer.take();
    }

    /// Puts the edited `text` on the clipboard.
    ///
    /// # Errors
    /// Why there is nothing to copy, or why the clipboard could not be set.
    pub async fn copy<H: Host<Target = T> + 'static>(
        &self,
        delivery: &Delivery<H>,
        recorder: &Recorder,
        text: String,
    ) -> Result<(), DraftError> {
        self.edit(text.clone())?;
        if text.trim().is_empty() {
            return Err(DraftError::Empty);
        }
        delivery
            .copy_text(recorder, &text)
            .await
            .map_err(DraftError::Host)
    }

    /// Types the edited `text` into the window the draft is bound to, without pressing Enter. The
    /// host re-verifies that window; one paste runs at a time, and none while a capture is active.
    ///
    /// # Errors
    /// Why the paste was refused, or why the host could not type the text; the text stays.
    pub async fn paste<H: Host<Target = T> + 'static>(
        &self,
        delivery: &Delivery<H>,
        recorder: &Recorder,
        text: String,
        capture_active: bool,
    ) -> Result<(), DraftError> {
        let (id, target) = self.begin_paste(&text, capture_active)?;
        let result = delivery.type_text(recorder, target, &text).await;
        self.finish_paste(id, text, &result);
        result.map_err(DraftError::Host)
    }

    fn begin_paste(&self, text: &str, capture_active: bool) -> Result<(u64, T), DraftError> {
        let mut slot = self.slot();
        if slot.current.is_none() {
            return Err(DraftError::NoDraft);
        }
        if capture_active {
            return Err(DraftError::CaptureActive);
        }
        if slot.pasting {
            return Err(DraftError::Pasting);
        }
        let Some(draft) = slot.current.as_mut() else {
            return Err(DraftError::NoDraft);
        };
        text.clone_into(&mut draft.text);
        if text.trim().is_empty() {
            return Err(DraftError::Empty);
        }
        let target = draft.target.clone().ok_or(DraftError::NoTarget)?;
        let id = draft.id;
        slot.pasting = true;
        Ok((id, target))
    }

    fn finish_paste(&self, id: u64, text: String, result: &Result<(), HostError>) {
        let mut slot = self.slot();
        slot.pasting = false;
        let Some(draft) = slot.current.as_mut().filter(|draft| draft.id == id) else {
            return;
        };
        match result {
            Ok(()) => {
                draft.sent = Some(text);
                draft.outcome = DraftOutcome::Delivered;
            }
            Err(HostError::MaybePartial(_)) => draft.outcome = DraftOutcome::MaybePartial,
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureRequest;
    use crate::config::DesktopConfig;
    use crate::delivery::fakes::{FakeHost, FakeWindow};
    use crate::engine::CaptureError;

    const EDITOR: &str = "the editor";

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
    ) -> (Option<Draft<FakeWindow>>, Delivery<FakeHost>) {
        let delivery = Delivery::new(host);
        let bound = delivery.bind(&Recorder::disabled()).await;
        let config = DesktopConfig::from_json_str("{}").unwrap();
        let finished = delivery
            .finish(outcome, &bound, &config, &Recorder::disabled())
            .await;
        (Draft::of(outcome, finished.as_ref(), &bound), delivery)
    }

    async fn offered(
        host: FakeHost,
        outcome: &CaptureOutcome,
    ) -> (Drafts<FakeWindow>, Delivery<FakeHost>) {
        let (draft, delivery) = left_by(host, outcome).await;
        let drafts = Drafts::default();
        if let Some(draft) = draft {
            drafts.offer(draft);
        }
        (drafts, delivery)
    }

    async fn newer(text: &str) -> Draft<FakeWindow> {
        left_by(FakeHost::default(), &cancelled(text))
            .await
            .0
            .unwrap()
    }

    async fn paste(
        drafts: &Drafts<FakeWindow>,
        delivery: &Delivery<FakeHost>,
        text: &str,
    ) -> Result<(), DraftError> {
        drafts
            .paste(delivery, &Recorder::disabled(), text.to_owned(), false)
            .await
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
                "typing refused",
                deliver_fails(HostError::NothingTyped("Gone.".to_owned())),
                paste_outcome("hello"),
                Some(DraftOutcome::Failed),
            ),
            (
                "typing stopped partway",
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
                    ("hello", "hello", Some(EDITOR)),
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
    async fn paste_types_the_edit_into_the_bound_window_and_nothing_else() {
        let failing = |error| FakeHost::default().failing("deliver", error);
        let closed =
            HostError::NothingTyped("The original window closed. Nothing was typed.".to_owned());
        let partway = HostError::MaybePartial("Typing stopped partway.".to_owned());
        for (label, host, text, expected, outcome, typed) in [
            (
                "delivered",
                FakeHost::default(),
                "fixed",
                Ok(()),
                DraftOutcome::Delivered,
                vec!["deliver:the editor:\"fixed\":enter=false"],
            ),
            (
                "target closed or changed",
                failing(closed.clone()),
                "fixed",
                Err(DraftError::Host(closed)),
                DraftOutcome::Cancelled,
                vec!["deliver:the editor:\"fixed\":enter=false"],
            ),
            (
                "typing stopped partway",
                failing(partway.clone()),
                "fixed",
                Err(DraftError::Host(partway)),
                DraftOutcome::MaybePartial,
                vec!["deliver:the editor:\"fixed\":enter=false"],
            ),
            (
                "a line break could press Enter",
                FakeHost::default(),
                "one\ntwo",
                Err(DraftError::Host(HostError::NothingTyped(
                    "The text contains control characters that could press keys. Nothing was typed.".to_owned(),
                ))),
                DraftOutcome::Cancelled,
                vec![],
            ),
        ] {
            let (drafts, delivery) = offered(host, &cancelled("wrong")).await;
            let before = delivery.host().calls().len();
            assert_eq!(paste(&drafts, &delivery, text).await, expected, "{label}");
            assert_eq!(delivery.host().calls()[before..], typed, "{label}");
            let view = drafts.view().unwrap();
            assert_eq!((view.outcome, view.text.as_str(), view.pasting), (outcome, text, false), "{label}");
        }
    }

    #[tokio::test]
    async fn an_empty_edit_is_refused_by_paste_and_copy() {
        let (drafts, delivery) = offered(FakeHost::default(), &cancelled("hello")).await;
        let before = delivery.host().calls();
        for text in ["", "  \n "] {
            assert_eq!(
                paste(&drafts, &delivery, text).await,
                Err(DraftError::Empty),
                "paste {text:?}"
            );
            assert_eq!(
                drafts
                    .copy(&delivery, &Recorder::disabled(), text.to_owned())
                    .await,
                Err(DraftError::Empty),
                "copy {text:?}"
            );
        }
        assert_eq!(delivery.host().calls(), before);
    }

    #[tokio::test]
    async fn copy_puts_the_edit_on_the_clipboard_and_a_failure_keeps_the_text() {
        let denied = HostError::Unsupported("The clipboard is locked.".to_owned());
        for (host, expected, copied) in [
            (FakeHost::default(), Ok(()), vec!["copy:\"fixed\""]),
            (
                FakeHost::default().failing("copy", denied.clone()),
                Err(DraftError::Host(denied)),
                vec!["copy:\"fixed\""],
            ),
        ] {
            let (drafts, delivery) = offered(host, &cancelled("wrong")).await;
            let before = delivery.host().calls().len();
            let result = drafts
                .copy(&delivery, &Recorder::disabled(), "fixed".to_owned())
                .await;
            assert_eq!(result, expected);
            assert_eq!(delivery.host().calls()[before..], copied);
            assert_eq!(drafts.view().unwrap().text, "fixed");
        }
    }

    #[tokio::test]
    async fn a_draft_without_a_bound_window_can_be_copied_but_not_pasted() {
        let host = FakeHost::default().failing(
            "capture_target",
            HostError::Unsupported("No window.".to_owned()),
        );
        let (drafts, delivery) = offered(host, &cancelled("hello")).await;
        assert_eq!(drafts.view().unwrap().target, None);
        assert_eq!(
            paste(&drafts, &delivery, "hello").await,
            Err(DraftError::NoTarget)
        );
        assert_eq!(
            drafts
                .copy(&delivery, &Recorder::disabled(), "hello".to_owned())
                .await,
            Ok(())
        );
        assert!(
            !delivery
                .host()
                .calls()
                .iter()
                .any(|call| call.starts_with("deliver"))
        );
    }

    #[tokio::test]
    async fn only_one_paste_runs_and_none_during_a_capture() {
        let (drafts, delivery) = offered(FakeHost::default(), &cancelled("hello")).await;
        let before = delivery.host().calls().len();
        let (first, second) = tokio::join!(
            paste(&drafts, &delivery, "hello"),
            paste(&drafts, &delivery, "hello")
        );
        assert_eq!((first, second), (Ok(()), Err(DraftError::Pasting)));
        assert_eq!(
            delivery.host().calls()[before..],
            ["deliver:the editor:\"hello\":enter=false"]
        );

        let before = delivery.host().calls().len();
        let busy = drafts
            .paste(&delivery, &Recorder::disabled(), "hello".to_owned(), true)
            .await;
        assert_eq!(busy, Err(DraftError::CaptureActive));
        assert_eq!(delivery.host().calls().len(), before);
    }

    #[tokio::test]
    async fn a_newer_dictation_waits_while_the_draft_has_unsent_edits() {
        let shown =
            |drafts: &Drafts<FakeWindow>| drafts.view().map(|view| (view.text, view.newer_waiting));
        let some = |text: &str, waiting| Some((text.to_owned(), waiting));

        // Without edits the newer dictation replaces it, even when the first was never sent.
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

        // Edits to a delivered dictation are unsent too; reverting them frees the draft.
        let (drafts, delivery) = offered(FakeHost::default(), &paste_outcome("sent")).await;
        drafts.edit("sent fixed".to_owned()).unwrap();
        drafts.offer(newer("next").await);
        assert_eq!(shown(&drafts), some("sent fixed", true));
        drafts.edit("sent".to_owned()).unwrap();
        drafts.offer(newer("after revert").await);
        assert_eq!(shown(&drafts), some("after revert", false));

        // Edits that were pasted are sent, so the next dictation replaces them.
        drafts.edit("after revert fixed".to_owned()).unwrap();
        paste(&drafts, &delivery, "after revert fixed")
            .await
            .unwrap();
        drafts.offer(newer("last").await);
        assert_eq!(shown(&drafts), some("last", false));
    }

    #[tokio::test]
    async fn discard_drops_the_draft() {
        let (drafts, _) = offered(FakeHost::default(), &cancelled("hello")).await;
        drafts.discard();
        assert_eq!(drafts.view(), None);
        assert_eq!(drafts.edit("x".to_owned()), Err(DraftError::NoDraft));
    }
}
