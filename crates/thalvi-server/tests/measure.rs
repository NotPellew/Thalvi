//! Behavior tests for the measurement run, the judge, and the summary table.
#![expect(clippy::unwrap_used, reason = "test setup failures panic immediately")]
#![expect(
    clippy::unused_async_trait_impl,
    reason = "fake trait implementations need no await"
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use bytes::Bytes;
use serde_json::{Value, json};
use thalvi_server::judge::{Grade, Judge, judge_all, parse_grade};
use thalvi_server::measure::{
    Case, Measured, Sample, Setup, load_setup, measure_reply, parse_cases, run_all,
};
use thalvi_server::model::{ChatMessage, ChatModel, DeltaStream, ModelError};
use thalvi_server::report::{Stat, render_table, summarize};
use thalvi_server::tts::{AudioStream, Tts, TtsError};
use tokio::time::{Duration, sleep};

// ---------- fakes ----------

#[derive(Clone)]
enum Step {
    Delta(u64, &'static str),
    Fail(u64),
    Hang,
}

type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

type Log = Arc<Mutex<Vec<(&'static str, Vec<ChatMessage>)>>>;

#[derive(Clone)]
struct FakeModel {
    name: &'static str,
    script: Vec<Step>,
    cannot_start: bool,
    log: Log,
}

impl FakeModel {
    fn new(name: &'static str, script: Vec<Step>) -> Self {
        Self {
            name,
            script,
            cannot_start: false,
            log: Log::default(),
        }
    }
}

impl ChatModel for FakeModel {
    async fn stream_reply(&self, history: &[ChatMessage]) -> Result<DeltaStream, ModelError> {
        self.log.lock().unwrap().push((self.name, history.to_vec()));
        if self.cannot_start {
            return Err(ModelError("offline".to_string()));
        }
        let steps = self.script.clone().into_iter();
        Ok(Box::pin(futures_util::stream::unfold(
            steps,
            |mut steps| async move {
                match steps.next()? {
                    Step::Delta(ms, text) => {
                        sleep(Duration::from_millis(ms)).await;
                        Some((Ok(text.to_string()), steps))
                    }
                    Step::Fail(ms) => {
                        sleep(Duration::from_millis(ms)).await;
                        Some((Err(ModelError("stream cut".to_string())), steps))
                    }
                    Step::Hang => std::future::pending().await,
                }
            },
        )))
    }
}

/// Speaks every sentence as chunks of (delay in ms before it, bytes).
#[derive(Clone)]
struct FakeTts {
    chunks: Vec<(u64, usize)>,
    cannot_start: bool,
}

impl FakeTts {
    fn new(chunks: Vec<(u64, usize)>) -> Self {
        Self {
            chunks,
            cannot_start: false,
        }
    }
}

impl Tts for FakeTts {
    async fn synthesize(&self, _text: &str) -> Result<AudioStream, TtsError> {
        if self.cannot_start {
            return Err(TtsError("no voice".to_string()));
        }
        Ok(Box::pin(futures_util::stream::unfold(
            self.chunks.clone().into_iter(),
            |mut chunks| async move {
                let (ms, bytes) = chunks.next()?;
                sleep(Duration::from_millis(ms)).await;
                Some((Ok(Bytes::from(vec![0; bytes])), chunks))
            },
        )))
    }
}

fn one_second_tts() -> FakeTts {
    FakeTts::new(vec![(40, 24_000), (60, 48_000)])
}

fn case(id: &str, turns: &[&str]) -> Case {
    Case {
        id: id.to_string(),
        turns: turns.iter().map(|turn| (*turn).to_string()).collect(),
    }
}

fn user(text: &str) -> ChatMessage {
    ChatMessage::user(text.to_string())
}

// ---------- timing ----------

fn near(actual: Option<f64>, expected: Option<f64>, what: &str) {
    match (actual, expected) {
        (Some(a), Some(e)) => assert!((a - e).abs() < 1e-9, "{what}: {a} != {e}"),
        _ => assert_eq!(actual, expected, "{what}"),
    }
}

#[tokio::test(start_paused = true)]
async fn each_time_is_taken_at_its_own_event() {
    let model = FakeModel::new(
        "m",
        vec![
            Step::Delta(100, "Hello"),
            Step::Delta(200, " there. How"),
            Step::Delta(100, " are you?"),
        ],
    );
    let measured = measure_reply(&model, &one_second_tts(), &[user("hi")]).await;
    // Token at 0.1, "Hello there." complete at 0.3, the model ends at 0.4 with
    // "How are you?"; speech of sentence one starts at 0.3 (first bytes 0.34,
    // done 0.4), sentence two starts at 0.4 and its last bytes land at 0.5.
    near(measured.first_token_s, Some(0.1), "first token");
    near(measured.first_sentence_s, Some(0.3), "first sentence");
    near(measured.first_audio_s, Some(0.34), "first audio");
    near(measured.end_to_end_s, Some(0.5), "end to end");
    near(Some(measured.audio_s), Some(3.0), "audio seconds");
    assert_eq!(measured.sentences, 2);
    assert_eq!(measured.reply, "Hello there. How are you?");
    assert_eq!(measured.error, None);
}

#[tokio::test(start_paused = true)]
async fn failures_are_recorded_with_the_timings_reached_before_them() {
    struct Failure {
        name: &'static str,
        model: FakeModel,
        tts: FakeTts,
        error_has: &'static str,
        first_audio: Option<f64>,
        sentences: usize,
    }
    let mut cannot_start = FakeModel::new("m", vec![]);
    cannot_start.cannot_start = true;
    let mut mute = one_second_tts();
    mute.cannot_start = true;
    let cases = [
        Failure {
            name: "model unreachable",
            model: cannot_start,
            tts: one_second_tts(),
            error_has: "offline",
            first_audio: None,
            sentences: 0,
        },
        Failure {
            name: "stream cut after the first sentence",
            model: FakeModel::new("m", vec![Step::Delta(100, "One. Two"), Step::Fail(50)]),
            tts: one_second_tts(),
            error_has: "stream cut",
            first_audio: Some(0.14),
            sentences: 1,
        },
        Failure {
            name: "speech unavailable",
            model: FakeModel::new("m", vec![Step::Delta(100, "One. Two.")]),
            tts: mute,
            error_has: "no voice",
            first_audio: None,
            sentences: 2,
        },
        Failure {
            name: "speech returns no audio",
            model: FakeModel::new("m", vec![Step::Delta(100, "One. Two.")]),
            tts: FakeTts::new(vec![(10, 0)]),
            error_has: "no audio",
            first_audio: None,
            sentences: 2,
        },
        Failure {
            name: "empty reply",
            model: FakeModel::new("m", vec![]),
            tts: one_second_tts(),
            error_has: "no text",
            first_audio: None,
            sentences: 0,
        },
    ];
    for failure in cases {
        let measured = measure_reply(&failure.model, &failure.tts, &[user("hi")]).await;
        let error = measured.error.as_deref().unwrap_or_default();
        assert!(
            error.contains(failure.error_has),
            "{}: {error}",
            failure.name
        );
        assert_eq!(measured.end_to_end_s, None, "{}", failure.name);
        near(measured.first_audio_s, failure.first_audio, failure.name);
        assert_eq!(measured.sentences, failure.sentences, "{}", failure.name);
    }
}

#[tokio::test(start_paused = true)]
async fn the_first_failure_ends_the_reply_and_keeps_its_own_error() {
    let mut mute = one_second_tts();
    mute.cannot_start = true;
    let cases = [
        (
            "speech fails while the model hangs",
            FakeModel::new("m", vec![Step::Delta(10, "One. Two"), Step::Hang]),
            mute,
            "no voice",
        ),
        (
            "the model fails while speech stalls",
            FakeModel::new("m", vec![Step::Delta(10, "One. Two"), Step::Fail(40)]),
            FakeTts::new(vec![(3_600_000, 48_000)]),
            "stream cut",
        ),
    ];
    for (name, model, tts, error_has) in cases {
        let start = tokio::time::Instant::now();
        let measured = tokio::time::timeout(
            Duration::from_secs(10),
            measure_reply(&model, &tts, &[user("hi")]),
        )
        .await
        .unwrap_or_else(|_| panic!("{name}: still waiting on the other side"));
        let error = measured.error.as_deref().unwrap_or_default();
        assert!(error.contains(error_has), "{name}: {error}");
        assert!(start.elapsed() < Duration::from_secs(1), "{name}");
        near(measured.first_sentence_s, Some(0.01), name);
    }
}

#[tokio::test(start_paused = true)]
async fn a_reply_that_never_finishes_times_out_and_the_run_goes_on() {
    let setups = [Setup {
        name: "a".to_string(),
        label: String::new(),
        model: FakeModel::new("a", vec![Step::Delta(10, "Hi."), Step::Hang]),
        tts: one_second_tts(),
    }];
    let samples = run_all(
        &setups,
        &[case("c1", &["q"]), case("c2", &["q"])],
        1,
        |_| {},
    )
    .await;
    assert_eq!(samples.len(), 2);
    for sample in samples {
        let error = sample.measured.error.unwrap();
        assert!(error.contains("120 seconds"), "{error}");
    }
}

// ---------- interleaving ----------

#[tokio::test(start_paused = true)]
async fn setups_alternate_within_each_case_and_history_is_sent_whole() {
    let log = Log::default();
    let setup = |name: &'static str| {
        let mut model = FakeModel::new(name, vec![Step::Delta(10, "Fine.")]);
        model.log = Arc::clone(&log);
        Setup {
            name: name.to_string(),
            label: String::new(),
            model,
            tts: one_second_tts(),
        }
    };
    let cases = [
        case("first", &["one"]),
        case("second", &["two", "answer", "three"]),
    ];
    let mut seen = Vec::new();
    let samples = run_all(&[setup("A"), setup("B")], &cases, 2, |sample| {
        seen.push((sample.setup.clone(), sample.case.clone(), sample.run));
    })
    .await;

    let expected: Vec<(&str, &str, usize)> = vec![
        ("A", "first", 0),
        ("B", "first", 0),
        ("A", "first", 1),
        ("B", "first", 1),
        ("A", "second", 0),
        ("B", "second", 0),
        ("A", "second", 1),
        ("B", "second", 1),
    ];
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(s, c, r)| (s.to_string(), c.to_string(), r))
        .collect();
    assert_eq!(seen, expected);
    let results: Vec<_> = samples
        .iter()
        .map(|s| (s.setup.clone(), s.case.clone(), s.run))
        .collect();
    assert_eq!(results, expected);
    let calls: Vec<_> = log.lock().unwrap().iter().map(|(name, _)| *name).collect();
    assert_eq!(calls, ["A", "B", "A", "B", "A", "B", "A", "B"]);
    assert_eq!(
        log.lock().unwrap()[4].1,
        vec![
            user("two"),
            ChatMessage::assistant("answer".to_string()),
            user("three")
        ]
    );
}

// ---------- cases and setups ----------

#[test]
fn the_committed_prompt_set_is_valid() {
    let cases = parse_cases(include_str!("../measure/cases.json")).unwrap();
    assert!((16..=20).contains(&cases.len()), "{} cases", cases.len());
    assert!(
        cases.iter().any(|case| case.turns.len() == 3),
        "a follow-up"
    );
}

#[test]
fn malformed_prompt_sets_are_refused_with_a_reason() {
    for (json, reason) in [
        (r#"[{"id":"a","turns":["q"]}]"#, ""),
        ("[]", "no cases"),
        ("nope", "invalid cases"),
        (
            r#"[{"id":"a","turns":["q"]},{"id":"a","turns":["q"]}]"#,
            "twice",
        ),
        (r#"[{"id":"a","turns":["q","a"]}]"#, "ending on a user turn"),
        (r#"[{"id":"a","turns":[" "]}]"#, "non-empty"),
    ] {
        let result = parse_cases(json);
        if reason.is_empty() {
            assert_eq!(result.unwrap().len(), 1);
        } else {
            let error = result.unwrap_err();
            assert!(error.contains(reason), "{json}: {error}");
        }
    }
}

#[test]
fn override_lines_win_over_the_normal_environment() {
    let base: HashMap<&str, &str> = [
        ("THALVI_ADDR", "127.0.0.1:8080"),
        ("THALVI_WEB_DIR", "web/dist"),
        ("LLM_BASE_URL", "http://127.0.0.1:1234/v1"),
        ("LLM_MODEL", "base-model"),
        ("LLM_API_KEY", ""),
        ("LLM_REASONING_EFFORT", "minimal"),
        ("STT_URL", "wss://example.invalid/asr"),
        ("STT_MODEL", "s"),
        ("STT_API_KEY", ""),
        ("TTS_API", "openai"),
        ("TTS_BASE_URL", "http://127.0.0.1:8880/v1"),
        ("TTS_MODEL", "kokoro"),
        ("TTS_VOICE", "af_heart"),
        ("TTS_API_KEY", ""),
    ]
    .into_iter()
    .collect();
    let lookup = |name: &str| base.get(name).map(|value| (*value).to_string());
    for (overrides, label) in [
        ("", Ok("base-model (effort minimal), kokoro")),
        (
            "# a comment\nLLM_MODEL=spark-1.2\nLLM_REASONING_EFFORT=low\n",
            Ok("spark-1.2 (effort low), kokoro"),
        ),
        (
            "LLM_REASONING_EFFORT=\nTTS_API=qwen3\n",
            Ok("base-model (effort default), qwen3-tts"),
        ),
        ("LLM_BASE_URL=ftp://x\n", Err("LLM_BASE_URL")),
        ("not a line\n", Err("override file")),
    ] {
        let setup = load_setup("s", overrides, lookup);
        match label {
            Ok(label) => assert_eq!(setup.unwrap().label, label, "{overrides:?}"),
            Err(mention) => {
                let error = setup.err().unwrap();
                assert!(error.contains(mention), "{overrides:?}: {error}");
            }
        }
    }
}

// ---------- judge ----------

#[test]
fn grades_are_read_from_the_judge_reply() {
    let scored = |score, note: &str| {
        Some(Grade::Scored {
            score,
            note: note.to_string(),
        })
    };
    for (reply, expected) in [
        (
            "SCORE: 4\nNOTE: Clear and short.",
            scored(4, "Clear and short."),
        ),
        ("**SCORE:** 5\n**NOTE:** Good.", scored(5, "Good.")),
        ("score = 3 \nnote: Too long", scored(3, "Too long")),
        ("SCORE: 4/5\nNOTE: ok", scored(4, "ok")),
        ("SCORE: 2", scored(2, "")),
        (
            "Let me weigh it.\nSCORE: 2\nNOTE: first thought\nSCORE: 5\nNOTE: final",
            scored(5, "final"),
        ),
        ("SCORE: 7\nNOTE: out of range", None),
        ("SCORE: 0", None),
        ("SCORE: 12", None),
        ("SCORE: high", None),
        ("I would give it a four.", None),
        ("", None),
    ] {
        assert_eq!(parse_grade(reply), expected, "{reply:?}");
    }
}

#[test]
fn judge_settings_come_from_the_environment() {
    for (vars, expected) in [
        (vec![], Ok(None)),
        (vec![("JUDGE_BASE_URL", " ")], Ok(None)),
        (
            vec![("JUDGE_BASE_URL", "http://j/v1/"), ("JUDGE_MODEL", "jm")],
            Ok(Some("jm")),
        ),
        (vec![("JUDGE_BASE_URL", "http://j/v1")], Err(())),
        (vec![("JUDGE_API_KEY", "k")], Err(())),
    ] {
        let vars: HashMap<_, _> = vars.into_iter().collect();
        let judge = Judge::from_lookup(|name| vars.get(name).map(|v| (*v).to_string()));
        match expected {
            Ok(model) => assert_eq!(judge.unwrap().as_ref().map(Judge::model), model),
            Err(()) => assert!(judge.unwrap_err().contains("JUDGE_MODEL"), "{vars:?}"),
        }
    }
}

fn sample(setup: &str, case: &str, run: usize, measured: Measured) -> Sample {
    Sample {
        setup: setup.to_string(),
        case: case.to_string(),
        run,
        measured,
        grade: None,
        judge_s: None,
    }
}

fn ok(
    first_token: f64,
    first_sentence: f64,
    first_audio: f64,
    end: f64,
    sentences: usize,
) -> Measured {
    Measured {
        first_token_s: Some(first_token),
        first_sentence_s: Some(first_sentence),
        first_audio_s: Some(first_audio),
        end_to_end_s: Some(end),
        sentences,
        audio_s: 3.0,
        reply: "An answer.".to_string(),
        error: None,
    }
}

#[tokio::test]
async fn the_judge_grades_finished_replies_and_records_what_went_wrong() {
    let seen: Seen = Arc::default();
    let seen2 = Arc::clone(&seen);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, body: String| {
            let seen = Arc::clone(&seen2);
            async move {
                let body: Value = serde_json::from_str(&body).unwrap();
                let auth = headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_string());
                let prompt = body["messages"][1]["content"].as_str().unwrap().to_string();
                seen.lock().unwrap().push((auth, body));
                if prompt.contains("fine answer") {
                    (
                        StatusCode::OK,
                        json!({"choices":[{"message":{"content":"SCORE: 4\nNOTE: Short and right."}}]})
                            .to_string(),
                    )
                } else if prompt.contains("garbled answer") {
                    (
                        StatusCode::OK,
                        json!({"choices":[{"message":{"content":"Pretty good overall!"}}]})
                            .to_string(),
                    )
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, "boom".to_string())
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cases = [case("c", &["What is it?"])];
    let reply = |text: &str| Measured {
        reply: text.to_string(),
        ..ok(0.1, 0.2, 0.3, 0.4, 1)
    };
    let mut samples = vec![
        sample("a", "c", 0, reply("a fine answer")),
        sample("a", "c", 1, reply("a garbled answer")),
        sample("a", "c", 2, reply("an answer the judge chokes on")),
        sample("a", "c", 3, Measured::failed("model offline".to_string())),
    ];
    let judge = Judge::new(
        format!("http://{addr}/v1"),
        "jm".to_string(),
        "key".to_string(),
    );
    let mut graded = 0;
    judge_all(&judge, &cases, &mut samples, |_| graded += 1).await;

    assert_eq!(graded, 3);
    assert_eq!(
        samples[0].grade,
        Some(Grade::Scored {
            score: 4,
            note: "Short and right.".to_string()
        })
    );
    assert_eq!(
        samples[1].grade,
        Some(Grade::Unparsed {
            reply: "Pretty good overall!".to_string()
        })
    );
    match &samples[2].grade {
        Some(Grade::Failed { error }) => assert!(error.contains("500"), "{error}"),
        other => panic!("expected a failed grade, got {other:?}"),
    }
    assert_eq!(samples[3].grade, None, "a failed reply is not judged");
    assert!(samples[..3].iter().all(|s| s.judge_s.is_some()));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for (auth, body) in seen.iter() {
        assert_eq!(auth.as_deref(), Some("Bearer key"));
        assert_eq!(body["model"], "jm");
        assert!(body["max_tokens"].as_u64().unwrap() >= 4096);
        let prompt = body["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("User: What is it?"), "{prompt}");
    }
}

// ---------- summary ----------

#[test]
fn medians_and_ranges_are_taken_over_the_replies_that_have_them() {
    for (values, expected) in [
        (vec![], None),
        (vec![0.5], Some((0.5, 0.5, 0.5))),
        (vec![0.75, 0.25, 0.5], Some((0.5, 0.25, 0.75))),
        (vec![1.0, 0.25, 0.75, 0.5], Some((0.625, 0.25, 1.0))),
    ] {
        let stat = Stat::of(values.clone()).map(|s| (s.median, s.min, s.max));
        assert_eq!(stat, expected, "{values:?}");
    }
}

#[test]
fn the_table_puts_each_setups_numbers_side_by_side() {
    let graded = |mut sample: Sample, grade| {
        sample.grade = Some(grade);
        sample
    };
    let scored = |score| Grade::Scored {
        score,
        note: String::new(),
    };
    let samples = vec![
        graded(sample("fast", "c", 0, ok(0.1, 0.2, 0.3, 1.0, 2)), scored(5)),
        graded(sample("fast", "c", 1, ok(0.2, 0.3, 0.5, 2.0, 4)), scored(4)),
        graded(
            sample("fast", "c", 2, ok(0.3, 0.4, 0.4, 3.0, 3)),
            Grade::Unparsed {
                reply: "hm".to_string(),
            },
        ),
        // A failed reply's partial numbers must stay out of the statistics.
        sample(
            "fast",
            "c",
            3,
            Measured {
                first_token_s: Some(9.0),
                first_sentence_s: Some(9.0),
                first_audio_s: Some(9.0),
                sentences: 7,
                audio_s: 99.0,
                ..Measured::failed("speech stopped".to_string())
            },
        ),
        sample("slow", "c", 0, ok(1.0, 1.5, 2.0, 5.0, 6)),
    ];
    let summaries = [
        summarize("fast", "model f", &samples),
        summarize("slow", "model s", &samples),
    ];
    let fast = &summaries[0];
    assert_eq!((fast.replies, fast.succeeded, fast.errors), (4, 3, 1));
    assert_eq!(
        fast.quality.map(|q| (q.mean, q.scored, q.ungraded)),
        Some((Some(4.5), 2, 1))
    );
    assert_eq!(summaries[1].quality, None);

    let table = render_table(&summaries);
    let row = |label: &str| -> Vec<String> {
        let line = table.lines().find(|line| line.starts_with(label)).unwrap();
        line[label.len()..]
            .split("  ")
            .filter(|cell| !cell.trim().is_empty())
            .map(|cell| cell.trim().to_string())
            .collect()
    };
    assert_eq!(row("setup"), ["fast", "slow"]);
    assert_eq!(row("replies ok"), ["3 of 4", "1 of 1"]);
    assert_eq!(row("errors"), ["1", "0"]);
    assert_eq!(
        row("first token (s)"),
        ["0.20 (0.10-0.30)", "1.00 (1.00-1.00)"]
    );
    assert_eq!(
        row("first audio (s)"),
        ["0.40 (0.30-0.50)", "2.00 (2.00-2.00)"]
    );
    assert_eq!(row("sentences"), ["3.0 (2.0-4.0)", "6.0 (6.0-6.0)"]);
    assert_eq!(row("audio (s)"), ["3.0 (3.0-3.0)", "3.0 (3.0-3.0)"]);
    assert_eq!(
        row("end to end (s)"),
        ["2.00 (1.00-3.00)", "5.00 (5.00-5.00)"]
    );
    assert_eq!(
        row("judge score (1-5)"),
        ["4.50 (2 scored, 1 ungraded)", "not judged"]
    );
}

#[test]
fn raw_results_carry_every_field_as_json() {
    let mut graded = sample("a", "c", 1, ok(0.1, 0.2, 0.3, 0.4, 2));
    graded.grade = Some(Grade::Scored {
        score: 5,
        note: "Good.".to_string(),
    });
    graded.judge_s = Some(1.5);
    let value = serde_json::to_value(&graded).unwrap();
    assert_eq!(value["setup"], "a");
    assert_eq!(value["run"], 1);
    assert_eq!(value["first_audio_s"], 0.3);
    assert_eq!(value["reply"], "An answer.");
    assert_eq!(value["error"], Value::Null);
    assert_eq!(
        value["grade"],
        json!({"outcome": "scored", "score": 5, "note": "Good."})
    );
    assert_eq!(value["judge_s"], 1.5);
}
