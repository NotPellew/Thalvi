//! A live terminal view of the running app, drawn from the replies `state` and `metrics-summary` already give.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use thalvi_desktop::host::{Desktop, HostError};

use crate::agent::Request;

/// How often `state` is asked. Four times a second is often enough to follow a capture and rare
/// enough that the app does nothing extra for it.
pub const STATE_EVERY: Duration = Duration::from_millis(250);

/// How often `metrics-summary` is asked. Stage times move only when a dictation ends.
pub const METRICS_EVERY: Duration = Duration::from_secs(3);

/// How long one poll waits for the app. A missing app answers at once; a slow one must not stall the view.
const POLL_WAIT: Duration = Duration::from_millis(400);

const WAITING: &str = "Thalvi is not running. Waiting for it…";

/// Where `thalvi watch` writes, and whether that place is a terminal.
pub trait Out {
    /// Whether the view is redrawn in place.
    fn is_terminal(&self) -> bool;
    /// Appends `text` and shows it.
    ///
    /// # Errors
    /// When the place can no longer be written.
    fn write(&mut self, text: &str) -> io::Result<()>;
}

impl<T: Write> Out for T {
    fn is_terminal(&self) -> bool {
        false
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        self.write_all(text.as_bytes())?;
        self.flush()
    }
}

/// Standard output, redrawn in place when it is a terminal.
struct Stdout {
    terminal: bool,
}

impl Out for Stdout {
    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(text.as_bytes())?;
        out.flush()
    }
}

/// What one poll of the running app found.
#[derive(Clone, Debug, PartialEq)]
enum Poll {
    /// `state`, and `metrics` when this poll asked for it.
    Running {
        state: Value,
        metrics: Option<Value>,
    },
    /// No app answered.
    Absent,
    /// The app answered with something that is not its reply.
    Failed(String),
}

/// Asks the running app. A test supplies the replies; the command asks the socket.
trait Source {
    fn poll(&mut self, metrics: bool) -> Poll;
}

/// The socket the running app already answers on.
struct Socket;

impl Source for Socket {
    fn poll(&mut self, metrics: bool) -> Poll {
        let state = match ask(&Request::State) {
            Asked::Reply(state) => state,
            Asked::Absent => return Poll::Absent,
            Asked::Failed(error) => return Poll::Failed(error),
        };
        if !metrics {
            return Poll::Running {
                state,
                metrics: None,
            };
        }
        match ask(&Request::MetricsSummary) {
            Asked::Reply(metrics) => Poll::Running {
                state,
                metrics: Some(metrics),
            },
            Asked::Absent => Poll::Absent,
            Asked::Failed(error) => Poll::Failed(error),
        }
    }
}

enum Asked {
    Reply(Value),
    Absent,
    Failed(String),
}

fn ask(request: &Request) -> Asked {
    match Desktop::send_request(&request.line(), POLL_WAIT) {
        Ok(None) => Asked::Absent,
        Ok(Some(line)) => match serde_json::from_str(&line) {
            Ok(value) => Asked::Reply(value),
            Err(_) => Asked::Failed("Thalvi answered with something that is not a reply.".into()),
        },
        Err(
            HostError::Unsupported(error)
            | HostError::NothingTyped(error)
            | HostError::MaybePartial(error),
        ) => Asked::Failed(error),
    }
}

/// How the view is printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    /// Redrawn in place.
    Terminal,
    /// One plain line each time the view changes.
    Line,
    /// One JSON line each time the view changes.
    Json,
}

/// What is on screen, so a poll that changes nothing prints nothing.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    state: Value,
    metrics: Value,
    /// What was printed, so a reply that changes nothing on screen prints nothing.
    rendered: String,
    lines: usize,
}

/// The view so far: what was last printed, and when metrics were last asked.
struct Watch {
    style: Style,
    shown: Option<Shown>,
    since_metrics: Duration,
}

impl Watch {
    fn new(terminal: bool, json: bool) -> Self {
        let style = match (json, terminal) {
            (true, _) => Style::Json,
            (false, true) => Style::Terminal,
            (false, false) => Style::Line,
        };
        Self {
            style,
            shown: None,
            since_metrics: METRICS_EVERY,
        }
    }

    /// One poll.
    fn step<O: Out>(&mut self, poll: Poll, out: &mut O) -> io::Result<()> {
        match poll {
            Poll::Running { state, metrics } => {
                if let Some(metrics) = metrics {
                    self.since_metrics = Duration::ZERO;
                    self.show(state, metrics, out)?;
                } else if let Some(shown) = self.shown.clone() {
                    self.show(state, shown.metrics, out)?;
                } else {
                    self.show(state, json!({}), out)?;
                }
                Ok(())
            }
            Poll::Absent => self.absent(out),
            Poll::Failed(error) => self.show(json!({"ok": false, "error": error}), json!({}), out),
        }
    }

    fn absent<O: Out>(&mut self, out: &mut O) -> io::Result<()> {
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.state.is_null())
        {
            return Ok(());
        }
        let text = match self.style {
            Style::Json => format!("{}\n", json!({"running": false})),
            Style::Line => format!("{WAITING}\n"),
            Style::Terminal => frame(WAITING, self.shown.as_ref().map_or(0, |shown| shown.lines)),
        };
        out.write(&text)?;
        self.shown = Some(Shown {
            state: Value::Null,
            metrics: json!({}),
            rendered: WAITING.to_owned(),
            lines: 1,
        });
        self.since_metrics = METRICS_EVERY;
        Ok(())
    }

    fn show<O: Out>(&mut self, state: Value, metrics: Value, out: &mut O) -> io::Result<()> {
        let picture = picture(&state, &metrics);
        let lines = picture.lines().count();
        // JSON follows the reply. The other views follow what they draw, so a field they leave
        // out (such as whether a file is still being fed) does not print a copy of the last line.
        let rendered = match self.style {
            Style::Json => json_line(&state, &metrics).to_string(),
            Style::Line | Style::Terminal => picture.replace('\n', " | "),
        };
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.rendered == rendered)
        {
            return Ok(());
        }
        let text = match self.style {
            Style::Json | Style::Line => format!("{rendered}\n"),
            Style::Terminal => frame(&picture, self.shown.as_ref().map_or(0, |shown| shown.lines)),
        };
        out.write(&text)?;
        self.shown = Some(Shown {
            state,
            metrics,
            rendered,
            lines,
        });
        Ok(())
    }
}

/// Moves the cursor home, draws `picture`, and clears whatever the last frame left below it.
fn frame(picture: &str, previous: usize) -> String {
    let lines = picture.lines().count();
    let mut text = String::from("\x1b[H");
    for (index, line) in picture.lines().enumerate() {
        text.push_str("\x1b[2K");
        text.push_str(line);
        if index + 1 < lines.max(previous) {
            text.push('\n');
        }
    }
    for _ in lines..previous {
        text.push_str("\x1b[2K");
        text.push('\n');
    }
    text.push_str("\x1b[J");
    text
}

/// The handful of lines the terminal shows. Nothing here is dictated text: `state` never carries it.
fn picture(state: &Value, metrics: &Value) -> String {
    if state.get("ok") == Some(&Value::Bool(false)) {
        return format!(
            "Thalvi: {}",
            state["error"].as_str().unwrap_or("did not answer")
        );
    }
    let mut lines = vec![
        format!(
            "Thalvi: {}",
            phase_word(state["state"].as_str().unwrap_or("unknown"))
        ),
        format!("Overlay: {}", overlay_line(&state["overlay"])),
        format!("Next recognizer: {}", recognizer_line(&state["recognizer"])),
    ];
    lines.extend(capture_lines(metrics));
    lines.join("\n")
}

fn phase_word(phase: &str) -> &'static str {
    match phase {
        "paused" => "Paused",
        "loading" => "Loading",
        "listening" => "Listening",
        "keyboard" => "Listening (hold keys only)",
        "capturing" => "Capturing",
        "pending" => "Ending",
        "transcribing" => "Transcribing",
        "finishing" => "Finishing",
        "finalPass" => "Final pass",
        "merging" => "Merging",
        "delivering" => "Delivering",
        "writing" => "Writing",
        "failed" => "Failed",
        _ => "Unknown",
    }
}

fn overlay_line(overlay: &Value) -> String {
    let word = overlay["state"].as_str().unwrap_or("hidden");
    match overlay["headline"].as_str() {
        Some(headline) => format!("{word} — {headline}"),
        None => word.to_owned(),
    }
}

fn recognizer_line(recognizer: &Value) -> String {
    let name = recognizer["name"].as_str().unwrap_or("None");
    let status = match recognizer["status"].as_str() {
        Some("ready") => "Ready",
        Some("loading") => "Loading",
        Some("failed") => "Failed",
        Some("paused") => "Paused",
        _ => "Unknown",
    };
    format!("{name} — {status}")
}

/// The latest dictation, then the stage delays of the recent window.
fn capture_lines(metrics: &Value) -> Vec<String> {
    if metrics.is_null() || metrics.as_object().is_some_and(serde_json::Map::is_empty) {
        return Vec::new();
    }
    if metrics.get("ok") == Some(&Value::Bool(false)) {
        return vec![format!(
            "Metrics: {}",
            metrics["error"].as_str().unwrap_or("could not be read")
        )];
    }
    if metrics.get("enabled") == Some(&Value::Bool(false)) {
        return vec!["Metrics: off".to_owned()];
    }
    let mut lines = Vec::new();
    match metrics["recent"]
        .as_array()
        .and_then(|recent| recent.last())
    {
        Some(capture) => lines.push(capture_line(capture)),
        None => lines.push("Last capture: none".to_owned()),
    }
    let stages = stage_line(metrics);
    if !stages.is_empty() {
        lines.push(format!("  {stages}"));
    }
    lines
}

fn capture_line(capture: &Value) -> String {
    // A delivered capture names its route. One that was cancelled or failed names where the
    // audio came from, which is all the log recorded for it.
    let how = capture["route"]
        .as_str()
        .or_else(|| match capture["source"].as_str() {
            Some("file") => Some("file"),
            Some("microphone") => Some("microphone"),
            _ => None,
        })
        .unwrap_or("dictation");
    let outcome = match capture["outcome"].as_str() {
        Some("ok") => "delivered",
        Some(other) => other,
        None => "unknown",
    };
    format!("Last capture: {how} — {outcome}")
}

/// The median of each stage the window measured, in the order a dictation runs.
fn stage_line(metrics: &Value) -> String {
    [
        ("connect", "ready"),
        ("partial_lag", "ending"),
        ("final", "model"),
        ("delivery", "delivery"),
    ]
    .into_iter()
    .filter_map(|(key, name)| {
        metrics["stages"][key]["p50_ms"]
            .as_f64()
            .map(|took| format!("{name} {:.0} ms", took.max(0.0)))
    })
    .collect::<Vec<_>>()
    .join(" | ")
}

/// One JSON line: the `state` fields, plus the latest dictation when metrics name one.
fn json_line(state: &Value, metrics: &Value) -> Value {
    if state.is_null() {
        return json!({"running": false});
    }
    let mut line = state.clone();
    if let Some(latest) = metrics["recent"]
        .as_array()
        .and_then(|recent| recent.last())
    {
        line["capture"] = latest.clone();
    } else if metrics.get("enabled") == Some(&Value::Bool(false)) {
        line["metrics"] = json!({"enabled": false});
    } else if metrics.get("ok") == Some(&Value::Bool(false)) {
        line["metrics"] = json!({"ok": false, "error": metrics["error"]});
    }
    line
}

/// Waits between polls. The command sleeps; a test records the wait and returns.
trait Clock {
    fn wait(&mut self, duration: Duration);
    fn stopped(&self) -> bool;
}

/// Sleeps between polls. Ctrl+C ends the process, which is how the command stops.
struct Sleeper;

impl Clock for Sleeper {
    fn wait(&mut self, duration: Duration) {
        thread::sleep(duration);
    }

    fn stopped(&self) -> bool {
        false
    }
}

/// Follows the app until `clock` says to stop.
///
/// # Errors
/// When the view can no longer be written.
fn follow_with<O: Out>(
    watch: &mut Watch,
    source: &mut impl Source,
    out: &mut O,
    clock: &mut impl Clock,
) -> io::Result<()> {
    let restore = watch.style == Style::Terminal;
    // Hide the cursor while a terminal is redrawn, and put it back however this ends.
    if restore {
        out.write("\x1b[?25l\x1b[H")?;
    }
    let result = (|| {
        loop {
            if clock.stopped() {
                return Ok(());
            }
            let metrics = watch.since_metrics >= METRICS_EVERY;
            let poll = source.poll(metrics);
            let absent = matches!(poll, Poll::Absent);
            watch.step(poll, out)?;
            if !absent {
                watch.since_metrics = watch.since_metrics.saturating_add(STATE_EVERY);
            }
            clock.wait(STATE_EVERY);
        }
    })();
    if restore {
        let _ = out.write("\x1b[?25h");
    }
    result
}

/// Standard output, as a terminal when it is one. Ctrl+C ends the process.
///
/// # Errors
/// When the view can no longer be written.
pub fn run(json: bool) -> io::Result<()> {
    let mut out = Stdout {
        terminal: io::IsTerminal::is_terminal(&io::stdout()),
    };
    let mut watch = Watch::new(out.is_terminal(), json);
    follow_with(&mut watch, &mut Socket, &mut out, &mut Sleeper)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Memory {
        terminal: bool,
        text: String,
    }

    impl Out for Memory {
        fn is_terminal(&self) -> bool {
            self.terminal
        }

        fn write(&mut self, text: &str) -> io::Result<()> {
            self.text.push_str(text);
            Ok(())
        }
    }

    /// Replies in order. `None` is an app that is not running.
    struct Script {
        replies: Vec<Option<(Value, Option<Value>)>>,
        asked_metrics: Vec<bool>,
    }

    impl Source for Script {
        fn poll(&mut self, metrics: bool) -> Poll {
            self.asked_metrics.push(metrics);
            match self.replies.pop() {
                // A metrics reply comes back only on the poll that asked for one, as the socket does.
                Some(Some((state, summary))) => Poll::Running {
                    state,
                    metrics: summary.filter(|_| metrics),
                },
                Some(None) | None => Poll::Absent,
            }
        }
    }

    fn state(phase: &str) -> Value {
        json!({
            "ok": true,
            "state": phase,
            "paused": phase == "paused",
            "overlay": {"state": "hidden"},
            "recognizer": {"name": "On this computer", "kind": "on_device", "status": "ready"}
        })
    }

    fn with_overlay(mut value: Value, overlay: &str, headline: Option<&str>) -> Value {
        value["overlay"] = json!({"state": overlay});
        if let Some(headline) = headline {
            value["overlay"]["headline"] = json!(headline);
        }
        value
    }

    fn capture(route: &str, outcome: &str) -> Value {
        json!({"route": route, "outcome": outcome, "source": "file"})
    }

    #[test]
    fn a_capture_without_a_route_names_where_its_audio_came_from() {
        assert_eq!(
            capture_line(&json!({"outcome": "cancelled", "source": "file"})),
            "Last capture: file — cancelled"
        );
        assert_eq!(
            capture_line(&json!({"route": "paste", "outcome": "ok", "source": "microphone"})),
            "Last capture: paste — delivered"
        );
    }

    fn stages() -> Value {
        json!({
            "connect": {"p50_ms": 280.0},
            "partial_lag": {"p50_ms": 240.0},
            "final": {"p50_ms": 910.0},
            "delivery": {"p50_ms": 35.0}
        })
    }

    fn metrics(recent: &[Value]) -> Value {
        json!({"ok": true, "enabled": true, "recent": recent, "stages": stages()})
    }

    /// Drives `polls` steps and returns what was written.
    fn play(
        terminal: bool,
        json: bool,
        replies: Vec<Option<(Value, Option<Value>)>>,
    ) -> (String, Vec<bool>) {
        let mut replies = replies;
        replies.reverse();
        let mut source = Script {
            replies,
            asked_metrics: Vec::new(),
        };
        let mut out = Memory {
            terminal,
            text: String::new(),
        };
        let mut watch = Watch::new(terminal, json);
        for _ in 0..source.replies.len() {
            watch
                .step(source.poll(watch.since_metrics >= METRICS_EVERY), &mut out)
                .unwrap();
            if !watch
                .shown
                .as_ref()
                .is_some_and(|shown| shown.state.is_null())
            {
                watch.since_metrics = watch.since_metrics.saturating_add(STATE_EVERY);
            }
        }
        (out.text, source.asked_metrics)
    }

    #[test]
    fn the_terminal_names_the_phase_the_overlay_the_recognizer_and_the_last_capture() {
        let listening = state("listening");
        let failed = with_overlay(state("failed"), "error", Some("The microphone stopped"));
        let paused = state("paused");
        let cases = [
            (
                listening,
                metrics(&[capture("paste", "delivered")]),
                "\
Thalvi: Listening
Overlay: hidden
Next recognizer: On this computer — Ready
Last capture: paste — delivered
  ready 280 ms | ending 240 ms | model 910 ms | delivery 35 ms",
            ),
            (
                failed,
                json!({"ok": true, "enabled": true, "recent": [], "stages": {}}),
                "\
Thalvi: Failed
Overlay: error — The microphone stopped
Next recognizer: On this computer — Ready
Last capture: none",
            ),
            (
                paused,
                json!({"ok": true, "enabled": false}),
                "\
Thalvi: Paused
Overlay: hidden
Next recognizer: On this computer — Ready
Metrics: off",
            ),
        ];
        for (state, metrics, picture) in cases {
            let (text, _) = play(true, false, vec![Some((state, Some(metrics)))]);
            let drawn = text
                .trim_start_matches("\x1b[H")
                .trim_end_matches("\x1b[J")
                .replace("\x1b[2K", "");
            assert!(
                drawn.contains(picture),
                "view:\n{drawn:?}\nexpected:\n{picture:?}"
            );
        }
    }

    #[test]
    fn a_change_is_printed_once_and_an_unchanged_poll_prints_nothing() {
        let first = state("listening");
        let same = first.clone();
        let next = state("capturing");
        let summary = metrics(&[capture("paste", "delivered")]);
        for (json, terminal) in [(true, false), (false, false), (false, true)] {
            let (text, asked) = play(
                terminal,
                json,
                vec![
                    Some((first.clone(), Some(summary.clone()))),
                    Some((same.clone(), None)),
                    Some((next.clone(), None)),
                ],
            );
            assert_eq!(
                asked,
                vec![true, false, false],
                "metrics only on its own beat"
            );
            if json {
                let lines: Vec<&str> = text.lines().collect();
                assert_eq!(
                    lines.len(),
                    2,
                    "one JSON line per change, not per poll: {text}"
                );
                assert_eq!(lines[0], json_line(&first, &summary).to_string());
                assert!(lines[1].contains("\"state\":\"capturing\""), "{text}");
            } else if !terminal {
                let lines: Vec<&str> = text.lines().collect();
                assert_eq!(lines.len(), 2, "{text}");
                assert!(lines[0].starts_with("Thalvi: Listening"), "{text}");
                assert!(lines[1].starts_with("Thalvi: Capturing"), "{text}");
            } else {
                assert_eq!(
                    text.matches("\x1b[H").count(),
                    2,
                    "redrawn only on change: {text}"
                );
            }
        }
    }

    #[test]
    fn an_app_that_is_absent_or_quits_is_waited_for_and_picked_up_again() {
        let up = state("listening");
        let summary = metrics(&[capture("paste", "delivered")]);
        let (text, _) = play(
            false,
            false,
            vec![None, None, Some((up.clone(), Some(summary.clone()))), None],
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                WAITING,
                "Thalvi: Listening | Overlay: hidden | Next recognizer: On this computer — Ready | Last capture: paste — delivered |   ready 280 ms | ending 240 ms | model 910 ms | delivery 35 ms",
                WAITING,
            ]
        );

        let (json_text, _) = play(false, true, vec![None, Some((up, Some(summary))), None]);
        let lines: Vec<Value> = json_text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0], json!({"running": false}));
        assert_eq!(lines[1]["state"], "listening");
        assert_eq!(lines[1]["capture"]["route"], "paste");
        assert_eq!(lines[2], json!({"running": false}));
        assert!(
            !json_text.contains("hello there"),
            "a watch line never carries dictated text"
        );
    }

    #[test]
    fn metrics_are_asked_every_few_seconds_and_state_four_times_a_second() {
        assert_eq!(STATE_EVERY, Duration::from_millis(250));
        assert_eq!(METRICS_EVERY, Duration::from_secs(3));
        let up = state("listening");
        let replies = (0..14)
            .map(|_| Some((up.clone(), Some(json!({})))))
            .collect();
        let (_, asked) = play(false, true, replies);
        assert_eq!(
            asked.iter().filter(|asked| **asked).count(),
            2,
            "at 0s and at 3s: {asked:?}"
        );
    }

    /// Stops after `polls` polls, and records how long it was asked to wait.
    struct Steps {
        left: usize,
        waited: Duration,
    }

    impl Clock for Steps {
        fn wait(&mut self, duration: Duration) {
            self.waited += duration;
            self.left = self.left.saturating_sub(1);
        }

        fn stopped(&self) -> bool {
            self.left == 0
        }
    }

    #[test]
    fn ctrl_c_ends_the_watch_between_polls() {
        let mut source = Script {
            replies: vec![
                Some((state("listening"), Some(json!({})))),
                Some((state("capturing"), None)),
            ],
            asked_metrics: Vec::new(),
        };
        source.replies.reverse();
        let mut out = Memory {
            terminal: false,
            text: String::new(),
        };
        let mut watch = Watch::new(false, false);
        let mut clock = Steps {
            left: 1,
            waited: Duration::ZERO,
        };
        follow_with(&mut watch, &mut source, &mut out, &mut clock).unwrap();
        assert_eq!(clock.waited, STATE_EVERY);
        assert!(out.text.contains("Thalvi: Listening"));
        assert!(
            !out.text.contains("Capturing"),
            "the second poll never runs"
        );
    }
}
