//! Owns the automatic quality judge: its client, rubric, and grade parsing.

use std::fmt::Write;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::measure::{Case, Sample};

/// Judge requests in flight at once, after all timing is done.
const CONCURRENCY: usize = 4;

/// Generous because the judge model reasons before it answers.
const MAX_TOKENS: u32 = 8192;

const RUBRIC: &str = "You grade a voice assistant's answer. The answer is read aloud by speech synthesis. Score it from 1 to 5 on three points: it is correct and answers the question; its length fits the question (a greeting or simple fact takes a sentence or two, an explanation gets the detail it needs, and a requested length is followed exactly); it is speakable (no markdown, lists, headings or emoji, and numbers, units and abbreviations written the way they are said). 5 is excellent on all three, 3 has a clear flaw in one, and 1 fails the question. Reply with exactly two lines and nothing else:\nSCORE: <a whole number from 1 to 5>\nNOTE: <one line naming the main flaw, or what was good>";

/// What the judge made of one answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Grade {
    /// A score from 1 to 5 with the judge's one-line note.
    Scored {
        /// The score.
        score: u8,
        /// The judge's note; empty if it gave none.
        note: String,
    },
    /// The judge replied, but not in the requested format.
    Unparsed {
        /// What the judge said.
        reply: String,
    },
    /// The judge could not be reached or returned an error.
    Failed {
        /// Why, in plain words.
        error: String,
    },
}

/// An OpenAI-compatible chat model that grades answers.
#[derive(Debug, Clone)]
pub struct Judge {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl Judge {
    /// Creates a judge client.
    #[must_use]
    pub fn new(base_url: String, model: String, api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base_url,
            model,
            api_key,
        }
    }

    /// Reads `JUDGE_BASE_URL`, `JUDGE_MODEL` and `JUDGE_API_KEY`; `None` when
    /// none of them is set.
    ///
    /// # Errors
    ///
    /// Returns a plain message when only some of the variables are set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let get = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        match (
            get("JUDGE_BASE_URL"),
            get("JUDGE_MODEL"),
            get("JUDGE_API_KEY"),
        ) {
            (None, None, None) => Ok(None),
            (Some(base_url), Some(model), api_key) => Ok(Some(Self::new(
                base_url.trim_end_matches('/').to_string(),
                model,
                api_key.unwrap_or_default(),
            ))),
            _ => Err("JUDGE_BASE_URL and JUDGE_MODEL must both be set to judge".to_string()),
        }
    }

    /// The judge model's name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Builds the request body asking for a grade of `answer` to `case`.
    #[must_use]
    pub fn request_body(&self, case: &Case, answer: &str) -> Value {
        let mut conversation = String::new();
        for (index, turn) in case.turns.iter().enumerate() {
            let speaker = if index % 2 == 0 { "User" } else { "Assistant" };
            let _ = writeln!(conversation, "{speaker}: {turn}");
        }
        json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "messages": [
                {"role": "system", "content": RUBRIC},
                {
                    "role": "user",
                    "content": format!(
                        "Conversation:\n{conversation}\nThe assistant's answer to the last user message, to grade:\n{answer}"
                    ),
                },
            ],
        })
    }

    /// Grades one answer and says how long the call took.
    pub async fn grade(&self, case: &Case, answer: &str) -> (Grade, f64) {
        let start = Instant::now();
        let grade = match self.ask(case, answer).await {
            Ok(reply) => parse_grade(&reply).unwrap_or(Grade::Unparsed { reply }),
            Err(error) => Grade::Failed { error },
        };
        (grade, start.elapsed().as_secs_f64())
    }

    async fn ask(&self, case: &Case, answer: &str) -> Result<String, String> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self
            .client
            .post(&url)
            .json(&self.request_body(case, answer));
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("the judge could not be reached: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the judge returned an error ({})",
                response.status()
            ));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| format!("the judge sent an unreadable reply: {error}"))?;
        Ok(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }
}

/// Reads `SCORE: n` (1 to 5) and `NOTE: …` lines from a judge reply; the last
/// of each wins, so a reply that thinks aloud first still parses.
#[must_use]
pub fn parse_grade(reply: &str) -> Option<Grade> {
    let mut score = None;
    let mut note = String::new();
    for line in reply.lines() {
        let line = line.trim_start_matches(|c: char| !c.is_alphanumeric());
        if let Some(rest) = strip_label(line, "score") {
            let mut digits = rest.chars();
            if let (Some(first), next) = (digits.next(), digits.next())
                && ('1'..='5').contains(&first)
                && !next.is_some_and(|c| c.is_ascii_digit())
            {
                score = first.to_digit(10).and_then(|d| u8::try_from(d).ok());
            }
        } else if let Some(rest) = strip_label(line, "note") {
            note = rest.trim_end_matches('*').trim().to_string();
        }
    }
    score.map(|score| Grade::Scored { score, note })
}

fn strip_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let head = line.get(..label.len())?;
    if !head.eq_ignore_ascii_case(label) {
        return None;
    }
    let rest = line[label.len()..].trim_start_matches(['*', ' ']);
    let rest = rest.strip_prefix([':', '=']).unwrap_or(rest);
    Some(rest.trim_start_matches(['*', ' ']))
}

/// Grades every reply that finished, `CONCURRENCY` at a time, storing the
/// grade and call time on its sample. Run it after all timing is done.
/// `on_graded` sees each sample as it is graded.
pub async fn judge_all(
    judge: &Judge,
    cases: &[Case],
    samples: &mut [Sample],
    mut on_graded: impl FnMut(&Sample),
) {
    let jobs: Vec<_> = samples
        .iter()
        .enumerate()
        .filter(|(_, sample)| sample.measured.error.is_none())
        .filter_map(|(index, sample)| {
            let case = cases.iter().find(|case| case.id == sample.case)?;
            Some((index, case, sample.measured.reply.clone()))
        })
        .collect();
    let mut graded = futures_util::stream::iter(jobs)
        .map(|(index, case, reply)| async move { (index, judge.grade(case, &reply).await) })
        .buffer_unordered(CONCURRENCY);
    while let Some((index, (grade, seconds))) = graded.next().await {
        let sample = &mut samples[index];
        sample.grade = Some(grade);
        sample.judge_s = Some(seconds);
        on_graded(sample);
    }
}
