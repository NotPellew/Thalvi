//! What Thalvi says to a screen reader: the status changes after the microphone stops taking text.

use std::sync::Arc;

use thalvi_desktop::capture::CaptureRoute;
use thalvi_desktop::host::StatusAnnouncer;

use crate::status::{FailureKind, Status};

/// Announces status changes to the desktop's screen reader, in the order they are published.
pub(crate) struct Announcements {
    announcer: Option<Arc<dyn StatusAnnouncer>>,
}

impl Announcements {
    pub(crate) fn new(announcer: Option<Arc<dyn StatusAnnouncer>>) -> Self {
        Self { announcer }
    }

    pub(crate) fn for_change(
        &self,
        previous: &Status,
        next: &Status,
    ) -> Option<(&dyn StatusAnnouncer, String)> {
        Some((self.announcer.as_deref()?, announcement(previous, next)?))
    }
}

/// The words for a change in status, or none while the microphone is still taking text.
fn announcement(previous: &Status, next: &Status) -> Option<String> {
    match next {
        Status::Paused => Some("Paused".to_owned()),
        Status::Finishing { .. } | Status::Transcribing { .. }
            if !matches!(
                previous,
                Status::Finishing { .. } | Status::Transcribing { .. }
            ) =>
        {
            Some("Finishing".to_owned())
        }
        Status::Writing { .. } | Status::Thinking { .. }
            if !matches!(previous, Status::Writing { .. } | Status::Thinking { .. }) =>
        {
            Some("Writing".to_owned())
        }
        Status::CleaningUp { .. } => Some("Cleaning up".to_owned()),
        Status::Done { route, target, .. } => Some(delivered(*route, target.as_deref())),
        Status::Failed {
            notice,
            headline,
            kind,
            ..
        } => Some(failed(previous, *kind, headline, notice)),
        Status::Listening | Status::KeyboardOnly | Status::Loading { .. }
            if matches!(previous, Status::Paused) =>
        {
            Some("Resumed".to_owned())
        }
        _ => None,
    }
}

/// Where a delivered capture's words went: the window by name, or the notice that says they are
/// on the clipboard instead.
fn delivered(route: CaptureRoute, target: Option<&str>) -> String {
    match (route, target) {
        (CaptureRoute::Task, _) => "Task completed".to_owned(),
        (_, Some(target)) => format!("Sent to {target}"),
        (CaptureRoute::Submit, None) => "Copied — press Ctrl+V then Enter".to_owned(),
        (_, None) => "Copied — press Ctrl+V to paste".to_owned(),
    }
}

/// A failure's short headline and its plain cause; the clipboard-recovery notice carries the
/// dictated text after a line break, which a screen reader must never say.
fn failed(previous: &Status, kind: FailureKind, headline: &str, notice: &str) -> String {
    if kind == FailureKind::Recognizer {
        // A recognizer's own diagnostics may include words it heard.
        return format!("{headline}. See Recognition in Settings for details.");
    }
    if matches!(
        previous,
        Status::Working {
            route: CaptureRoute::Task,
            ..
        }
    ) {
        // A task tool can return a spoken address in its diagnostics.
        return format!("{headline}. The task could not complete.");
    }
    if kind == FailureKind::Delivery && headline == "No words heard" {
        return "Not sent: no words heard".to_owned();
    }
    let cause = notice.lines().next().unwrap_or_default();
    let cause = cause.strip_suffix(" Your text follows:").unwrap_or(cause);
    format!("{headline}. {cause}")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Mutex, PoisonError};

    use thalvi_desktop::engine::SubmitPreview;

    use super::*;
    use crate::status::{FailureKind, STARTING_NOTICE};

    /// A screen reader that records what it hears instead of speaking.
    #[derive(Clone, Default)]
    pub(crate) struct Spoken(Arc<Mutex<Vec<String>>>);

    impl Spoken {
        pub(crate) fn said(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl StatusAnnouncer for Spoken {
        fn announce(&self, announcement: &str) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(announcement.to_owned());
        }
    }

    const SECRET: &str = "the words the user dictated";

    fn loading() -> Status {
        Status::Loading {
            notice: STARTING_NOTICE.to_owned(),
            downloading: false,
        }
    }

    fn starting() -> Status {
        Status::Starting {
            target: Some("Editor".to_owned()),
            loading: false,
        }
    }

    fn capturing() -> Status {
        Status::Capturing {
            target: Some("Editor".to_owned()),
            committed: SECRET.to_owned(),
            partial: Some(SECRET.to_owned()),
            notice: None,
        }
    }

    fn pending() -> Status {
        Status::Pending {
            target: Some("Editor".to_owned()),
            route: CaptureRoute::Paste,
            text: SECRET.to_owned(),
            waiting_for_quiet: false,
            pause_ms: 700,
            copies: false,
            final_confirmation: None,
        }
    }

    fn finishing() -> Status {
        Status::Finishing {
            target: Some("Editor".to_owned()),
            route: CaptureRoute::Paste,
            cancellable: true,
        }
    }

    fn transcribing() -> Status {
        Status::Transcribing {
            target: Some("Editor".to_owned()),
            route: CaptureRoute::Paste,
            cancellable: true,
        }
    }

    fn writing() -> Status {
        Status::Writing {
            target: Some("Editor".to_owned()),
            text: SECRET.to_owned(),
        }
    }

    fn thinking() -> Status {
        Status::Thinking {
            target: Some("Editor".to_owned()),
        }
    }

    fn cleaning_up() -> Status {
        Status::CleaningUp {
            target: Some("Editor".to_owned()),
        }
    }

    fn working() -> Status {
        Status::Working {
            target: Some("Editor".to_owned()),
            route: CaptureRoute::Paste,
            cancellable: false,
        }
    }

    fn done(target: Option<&str>, notice: &str) -> Status {
        Status::Done {
            target: target.map(str::to_owned),
            route: CaptureRoute::Paste,
            notice: notice.to_owned(),
            wake_words: true,
        }
    }

    fn failed(headline: &str, notice: &str) -> Status {
        Status::Failed {
            notice: notice.to_owned(),
            headline: headline.to_owned(),
            causes: Vec::new(),
            kind: FailureKind::Delivery,
            offer: None,
        }
    }

    fn spoken_turnaround(
        announcements: &Announcements,
        spoken: &Spoken,
        statuses: &[Status],
    ) -> Vec<String> {
        let mut previous = loading();
        for next in statuses {
            if *next != previous
                && let Some((announcer, words)) = announcements.for_change(&previous, next)
            {
                announcer.announce(&words);
            }
            previous = next.clone();
        }
        spoken.said()
    }

    #[test]
    fn the_status_sequence_announces_each_post_release_change_once_and_never_the_dictated_text() {
        let spoken = Spoken::default();
        let announcements = Announcements::new(Some(Arc::new(spoken.clone())));
        let statuses = [
            loading(),
            Status::Listening,
            starting(),
            capturing(),
            pending(),
            finishing(),
            transcribing(),
            finishing(),
            thinking(),
            writing(),
            thinking(),
            Status::Writing {
                target: Some("Editor".to_owned()),
                text: format!("{SECRET}, more"),
            },
            working(),
            done(Some("kitty"), "Delivered."),
            done(Some("kitty"), "Delivered."),
            Status::Paused,
            loading(),
            Status::Listening,
            starting(),
            capturing(),
            pending(),
            finishing(),
            cleaning_up(),
            working(),
            done(None, "Copied \u{2014} press Ctrl+V to paste"),
            starting(),
            capturing(),
            finishing(),
            failed(
                "Clipboard failed \u{2014} text in Settings",
                &format!("Clipboard recovery failed: locked. Your text follows:\n{SECRET}"),
            ),
            Status::Listening,
            finishing(),
            failed(
                "No words heard",
                "No words were heard while the key was held. Nothing was pasted.",
            ),
            failed("Hold key unavailable", "Hyprland refused F9."),
            Status::Done {
                target: Some(SECRET.to_owned()),
                route: CaptureRoute::Task,
                notice: format!("Asked the browser to open {SECRET}."),
                wake_words: true,
            },
            Status::Working {
                target: None,
                route: CaptureRoute::Task,
                cancellable: false,
            },
            failed("Could not open the address", SECRET),
            Status::Failed {
                notice: format!("The speech server reported: {SECRET}"),
                headline: "Speech recognition failed — Settings".to_owned(),
                causes: Vec::new(),
                kind: FailureKind::Recognizer,
                offer: None,
            },
            Status::Paused,
            loading(),
            Status::Listening,
        ];
        let said = spoken_turnaround(&announcements, &spoken, &statuses);
        assert_eq!(
            said,
            [
                "Finishing",
                "Writing",
                "Sent to kitty",
                "Paused",
                "Resumed",
                "Finishing",
                "Cleaning up",
                "Copied \u{2014} press Ctrl+V to paste",
                "Finishing",
                "Clipboard failed \u{2014} text in Settings. Clipboard recovery failed: locked.",
                "Finishing",
                "Not sent: no words heard",
                "Hold key unavailable. Hyprland refused F9.",
                "Task completed",
                "Could not open the address. The task could not complete.",
                "Speech recognition failed — Settings. See Recognition in Settings for details.",
                "Paused",
                "Resumed",
            ],
            "the announcement sequence"
        );
        assert!(
            said.iter()
                .all(|announcement| !announcement.contains(SECRET)),
            "a screen reader heard the dictated text: {said:?}"
        );
    }

    #[test]
    fn resuming_from_a_start_announces_nothing() {
        let spoken = Spoken::default();
        let announcements = Announcements::new(Some(Arc::new(spoken.clone())));
        let said = spoken_turnaround(
            &announcements,
            &spoken,
            &[
                loading(),
                Status::Loading {
                    notice: "Loading the speech model.".to_owned(),
                    downloading: true,
                },
            ],
        );
        assert!(said.is_empty(), "{said:?}");
    }

    #[test]
    fn the_engine_submit_preview_is_never_spoken() {
        let spoken = Spoken::default();
        let announcements = Announcements::new(Some(Arc::new(spoken.clone())));
        let said = spoken_turnaround(
            &announcements,
            &spoken,
            &[Status::Pending {
                target: Some("Editor".to_owned()),
                route: CaptureRoute::Submit,
                text: SECRET.to_owned(),
                waiting_for_quiet: false,
                pause_ms: 2_500,
                copies: false,
                final_confirmation: Some(SubmitPreview::default()),
            }],
        );
        assert!(said.is_empty(), "{said:?}");
    }
}
