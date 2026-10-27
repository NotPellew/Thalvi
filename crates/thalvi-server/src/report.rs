//! Owns summarizing measured samples and printing them side by side.

use std::fmt::Write;

use crate::judge::Grade;
use crate::measure::Sample;

/// Median and range of a set of values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stat {
    /// The middle value; the mean of the two middle values for an even count.
    pub median: f64,
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
}

impl Stat {
    /// Summarizes `values`; `None` when there are none.
    #[must_use]
    pub fn of(mut values: Vec<f64>) -> Option<Self> {
        values.sort_by(f64::total_cmp);
        let (&min, &max) = (values.first()?, values.last()?);
        let middle = values.len() / 2;
        let median = if values.len() % 2 == 1 {
            values[middle]
        } else {
            f64::midpoint(values[middle - 1], values[middle])
        };
        Some(Self { median, min, max })
    }
}

/// How the judge did across one setup's replies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quality {
    /// Mean score of the replies that were scored.
    pub mean: Option<f64>,
    /// Replies the judge scored.
    pub scored: usize,
    /// Replies the judge answered in an unusable format or could not grade.
    pub ungraded: usize,
}

/// Everything the table shows for one setup.
#[derive(Debug, Clone, PartialEq)]
pub struct SetupSummary {
    /// The setup's name.
    pub name: String,
    /// What the setup is, from its configuration.
    pub label: String,
    /// Replies measured.
    pub replies: usize,
    /// Replies that finished without an error; the numbers below cover these.
    pub succeeded: usize,
    /// Replies that failed.
    pub errors: usize,
    /// Time to first content token.
    pub first_token: Option<Stat>,
    /// Time to first sentence.
    pub first_sentence: Option<Stat>,
    /// Time to first audio byte.
    pub first_audio: Option<Stat>,
    /// Time to last audio byte.
    pub end_to_end: Option<Stat>,
    /// Sentences per reply.
    pub sentences: Option<Stat>,
    /// Audio seconds per reply.
    pub audio: Option<Stat>,
    /// The judge's results; `None` when nothing was judged.
    pub quality: Option<Quality>,
}

/// Summarizes `samples` for the setup `name`.
#[must_use]
pub fn summarize(name: &str, label: &str, samples: &[Sample]) -> SetupSummary {
    let mine: Vec<&Sample> = samples.iter().filter(|s| s.setup == name).collect();
    let finished: Vec<&Sample> = mine
        .iter()
        .copied()
        .filter(|s| s.measured.error.is_none())
        .collect();
    let stat = |value: fn(&Sample) -> Option<f64>| {
        Stat::of(finished.iter().filter_map(|s| value(s)).collect())
    };
    let scores: Vec<f64> = mine
        .iter()
        .filter_map(|s| match s.grade {
            Some(Grade::Scored { score, .. }) => Some(f64::from(score)),
            _ => None,
        })
        .collect();
    let ungraded = mine
        .iter()
        .filter(|s| matches!(s.grade, Some(Grade::Unparsed { .. } | Grade::Failed { .. })))
        .count();
    let quality = if mine.iter().any(|s| s.grade.is_some()) {
        #[expect(clippy::cast_precision_loss, reason = "score counts are tiny")]
        let mean = (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64);
        Some(Quality {
            mean,
            scored: scores.len(),
            ungraded,
        })
    } else {
        None
    };
    SetupSummary {
        name: name.to_string(),
        label: label.to_string(),
        replies: mine.len(),
        succeeded: finished.len(),
        errors: mine.iter().filter(|s| s.measured.error.is_some()).count(),
        first_token: stat(|s| s.measured.first_token_s),
        first_sentence: stat(|s| s.measured.first_sentence_s),
        first_audio: stat(|s| s.measured.first_audio_s),
        end_to_end: stat(|s| s.measured.end_to_end_s),
        #[expect(clippy::cast_precision_loss, reason = "sentence counts are tiny")]
        sentences: stat(|s| Some(s.measured.sentences as f64)),
        audio: stat(|s| Some(s.measured.audio_s)),
        quality,
    }
}

/// Explains what the numbers are; printed under the table.
pub const NOTE: &str = "Times, sentences and audio cover the replies that finished without an error. Times are model and speech latencies in seconds from the request start, shown as median (min-max). The wait the user perceives adds endpointing (about 0.5-0.75 s) on top.";

/// Renders the summaries as a table with one column per setup.
#[must_use]
pub fn render_table(summaries: &[SetupSummary]) -> String {
    let stat = |value: Option<Stat>, decimals: usize| {
        value.map_or_else(
            || "-".to_string(),
            |s| {
                format!(
                    "{:.decimals$} ({:.decimals$}-{:.decimals$})",
                    s.median, s.min, s.max
                )
            },
        )
    };
    let mut rows: Vec<(&str, Vec<String>)> = vec![
        ("setup", summaries.iter().map(|s| s.name.clone()).collect()),
        (
            "config",
            summaries.iter().map(|s| s.label.clone()).collect(),
        ),
        (
            "replies ok",
            summaries
                .iter()
                .map(|s| format!("{} of {}", s.succeeded, s.replies))
                .collect(),
        ),
        (
            "errors",
            summaries.iter().map(|s| s.errors.to_string()).collect(),
        ),
        (
            "first token (s)",
            summaries.iter().map(|s| stat(s.first_token, 2)).collect(),
        ),
        (
            "first sentence (s)",
            summaries
                .iter()
                .map(|s| stat(s.first_sentence, 2))
                .collect(),
        ),
        (
            "first audio (s)",
            summaries.iter().map(|s| stat(s.first_audio, 2)).collect(),
        ),
        (
            "end to end (s)",
            summaries.iter().map(|s| stat(s.end_to_end, 2)).collect(),
        ),
        (
            "sentences",
            summaries.iter().map(|s| stat(s.sentences, 1)).collect(),
        ),
        (
            "audio (s)",
            summaries.iter().map(|s| stat(s.audio, 1)).collect(),
        ),
    ];
    rows.push((
        "judge score (1-5)",
        summaries
            .iter()
            .map(|s| match s.quality {
                None => "not judged".to_string(),
                Some(q) => {
                    let mean = q
                        .mean
                        .map_or_else(|| "-".to_string(), |m| format!("{m:.2}"));
                    format!("{mean} ({} scored, {} ungraded)", q.scored, q.ungraded)
                }
            })
            .collect(),
    ));
    let label_width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..summaries.len())
        .map(|column| {
            rows.iter()
                .map(|(_, cells)| cells[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut table = String::new();
    for (label, cells) in &rows {
        let _ = write!(table, "{label:<label_width$}");
        for (cell, width) in cells.iter().zip(&widths) {
            let _ = write!(table, "  {cell:<width$}");
        }
        table.truncate(table.trim_end().len());
        table.push('\n');
    }
    table
}
