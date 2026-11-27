//! How Thalvi keeps a cleanup of one pasted dictation and suggests corrected names.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use thalvi_desktop::learn::{
    Field, FieldEnd, FieldLimit, FieldProbe, Learned, Spans, Suggestion, suggestions, watch,
};
use thalvi_desktop::metrics::Recorder;
use tokio::sync::{oneshot, watch as tokio_watch};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Read {
    Text(String),
    End(FieldEnd),
}

#[derive(Default)]
struct Shared {
    reads: Vec<Read>,
    taken: usize,
}

struct FakeField {
    shared: Arc<Mutex<Shared>>,
}

impl Field for FakeField {
    fn text(&mut self) -> Result<String, FieldEnd> {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let read = shared.reads.get(shared.taken).cloned();
        shared.taken += 1;
        match read {
            Some(Read::Text(text)) => Ok(text),
            Some(Read::End(end)) => Err(end),
            None => Ok(String::new()),
        }
    }
}

struct FakeProbe {
    shared: Arc<Mutex<Shared>>,
    result: Option<FieldLimit>,
}

impl FieldProbe for FakeProbe {
    fn field(self: Box<Self>, _pasted: &str) -> Result<Box<dyn Field>, FieldLimit> {
        match self.result {
            Some(limit) => Err(limit),
            None => Ok(Box::new(FakeField {
                shared: self.shared,
            })),
        }
    }
}

fn resolved(
    shared: Arc<Mutex<Shared>>,
    limit: Option<FieldLimit>,
) -> Result<Box<dyn Field>, FieldLimit> {
    Box::new(FakeProbe {
        shared,
        result: limit,
    })
    .field("cyber x2")
}

fn reads(shared: &Arc<Mutex<Shared>>) -> usize {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .taken
}

#[test]
fn the_span_keeps_only_the_users_edit_between_the_anchors() {
    // (case, first read, later reads, expected cleaned text)
    let cases: [(&str, &str, &[&str], Option<&str>); 9] = [
        (
            "an edit inside the paste",
            "before cyber x2 after",
            &["before Cybrex2 after"],
            Some("Cybrex2"),
        ),
        (
            "text typed outside the paste is excluded",
            "before cyber x2 after",
            &["typed before Cybrex2 after typed later"],
            Some("Cybrex2"),
        ),
        (
            "a missing anchor keeps the last good version",
            "before cyber x2 after",
            &["before Cybrex2 after", "everything changed"],
            Some("Cybrex2"),
        ),
        (
            "a twice-found anchor keeps the last good version",
            "before cyber x2 after",
            &[
                "before Cybrex2 after",
                "before Cybrex2 after before something",
            ],
            Some("Cybrex2"),
        ),
        (
            "an unchanged paste keeps nothing",
            "before cyber x2 after",
            &["before cyber x2 after"],
            None,
        ),
        (
            "a field emptied after edits keeps the version before it emptied",
            "before cyber x2 after",
            &["before Cybrex2 after", ""],
            Some("Cybrex2"),
        ),
        (
            "empty anchors at the field's start and end",
            "cyber x2",
            &["Cybrex2"],
            Some("Cybrex2"),
        ),
        (
            "a whole-field paste edited then sent keeps the edit",
            "cyber x2",
            &["Cybrex2", ""],
            Some("Cybrex2"),
        ),
        (
            "a whole-field paste emptied without edits keeps nothing",
            "cyber x2",
            &[""],
            None,
        ),
    ];
    for (label, first, later, expected) in cases {
        let mut spans = Spans::new("cyber x2");
        spans.observe(first);
        for text in later {
            spans.observe(text);
        }
        assert_eq!(spans.kept(), expected, "{label}");
    }
}

#[test]
fn only_name_like_single_edits_become_suggestions() {
    type Case<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], &'a [Suggestion]);
    let cases: [Case; 5] = [
        (
            "a corrected name",
            "send the Cyber X2 build log",
            "Send the Cybrex2 build log.",
            &[],
            &[Suggestion {
                word: "Cybrex2".to_owned(),
                heard: "Cyber X2".to_owned(),
            }],
        ),
        ("a lowercase edit", "hello wrld", "hello world", &[], &[]),
        (
            "a rewrite",
            "one two three four",
            "completely different words here with many more tokens",
            &[],
            &[],
        ),
        (
            "a word already in the list",
            "the cybrex2 build",
            "the Cybrex2 build",
            &["Cybrex2"],
            &[],
        ),
        (
            "at most five per paste",
            "meet ada lovelace and grace hopper and alan turing and cybrex2 and qwen and thalvi",
            "Meet Ada Lovelace and Grace Hopper and Alan Turing and Cybrex2 and Qwen and Thalvi",
            &[],
            &[
                Suggestion {
                    word: "Ada".to_owned(),
                    heard: "ada".to_owned(),
                },
                Suggestion {
                    word: "Lovelace".to_owned(),
                    heard: "lovelace".to_owned(),
                },
                Suggestion {
                    word: "Grace".to_owned(),
                    heard: "grace".to_owned(),
                },
                Suggestion {
                    word: "Hopper".to_owned(),
                    heard: "hopper".to_owned(),
                },
                Suggestion {
                    word: "Alan".to_owned(),
                    heard: "alan".to_owned(),
                },
            ],
        ),
    ];
    for (label, pasted, cleaned, known, expected) in cases {
        let known: Vec<String> = known.iter().map(ToString::to_string).collect();
        assert_eq!(suggestions(pasted, cleaned, &known), expected, "{label}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_watch_stops_on_each_field_end_and_keeps_the_latest_version() {
    for end in [
        FieldEnd::FocusLeft,
        FieldEnd::Closed,
        FieldEnd::Protected,
        FieldEnd::Unreadable,
    ] {
        let shared = Arc::new(Mutex::new(Shared {
            reads: vec![
                Read::Text("before cyber x2".to_owned()),
                Read::Text("before Cybrex2".to_owned()),
                Read::End(end),
                Read::Text("never read again".to_owned()),
            ],
            taken: 0,
        }));
        let (_enabled, enabled_rx) = tokio_watch::channel(true);
        let (_cancel, cancel_rx) = oneshot::channel();
        let learned = watch(
            resolved(Arc::clone(&shared), None),
            "cyber x2",
            &[],
            enabled_rx,
            cancel_rx,
            &Recorder::disabled().metrics(),
        )
        .await;
        assert_eq!(reads(&shared), 3, "{end:?}");
        assert_eq!(
            learned,
            Learned {
                corrected: Some("Cybrex2".to_owned()),
                suggestions: vec![Suggestion {
                    word: "Cybrex2".to_owned(),
                    heard: "cyber x2".to_owned(),
                }],
            },
            "{end:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_newer_delivery_or_the_switch_going_off_stops_the_watch() {
    for by_switch in [false, true] {
        let shared = Arc::new(Mutex::new(Shared {
            reads: vec![Read::Text("cyber x2".to_owned())],
            taken: 0,
        }));
        let (enabled, enabled_rx) = tokio_watch::channel(true);
        let (cancel, cancel_rx) = oneshot::channel();
        let task_shared = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            watch(
                resolved(task_shared, None),
                "cyber x2",
                &[],
                enabled_rx,
                cancel_rx,
                &Recorder::disabled().metrics(),
            )
            .await
        });
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        if by_switch {
            assert!(enabled.send(false).is_ok(), "the watch is still running");
        } else {
            assert!(cancel.send(()).is_ok(), "the watch is still running");
        }
        let Ok(learned) = task.await else {
            panic!("the watch task failed");
        };
        assert!(learned.corrected.is_none());
        let after = reads(&shared);
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(reads(&shared), after, "no reads after the stop");
    }
}

#[tokio::test(start_paused = true)]
async fn a_watch_ends_at_five_minutes() {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (_enabled, enabled_rx) = tokio_watch::channel(true);
    let (_cancel, cancel_rx) = oneshot::channel();
    let learned = watch(
        resolved(Arc::clone(&shared), None),
        "cyber x2",
        &[],
        enabled_rx,
        cancel_rx,
        &Recorder::disabled().metrics(),
    )
    .await;
    assert!(learned.corrected.is_none());
    assert!(
        reads(&shared) >= 1200,
        "a read every 250 ms for five minutes"
    );
}

#[tokio::test(start_paused = true)]
async fn a_field_limit_ends_the_watch_without_reading() {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (_enabled, enabled_rx) = tokio_watch::channel(true);
    let (_cancel, cancel_rx) = oneshot::channel();
    let learned = watch(
        resolved(Arc::clone(&shared), Some(FieldLimit::NoAccessibility)),
        "cyber x2",
        &[],
        enabled_rx,
        cancel_rx,
        &Recorder::disabled().metrics(),
    )
    .await;
    assert_eq!(learned, Learned::default());
    assert_eq!(reads(&shared), 0);
    assert_eq!(
        FieldLimit::NotShared {
            app: "T3 Code".to_owned()
        }
        .reason(),
        "T3 Code shares no text with Thalvi. For Chromium and Electron apps such as T3 Code, start them with ACCESSIBILITY_ENABLED=1 and --force-renderer-accessibility."
    );
}
