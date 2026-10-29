//! Compares the model's echo of an audio turn with the recognizer's caption.

/// Largest word-level edit distance, as a share of the caption's words, still counted as the same speech.
pub const MAX_HEARD_MISMATCH: f64 = 0.5;

const ECHO_PREFIX: &str = "the user said:";

fn words(text: &str) -> Vec<String> {
    let lowered = text.trim().to_lowercase();
    let body = lowered.strip_prefix(ECHO_PREFIX).unwrap_or(&lowered);
    let plain: String = body
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    plain.split_whitespace().map(str::to_string).collect()
}

fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, word_a) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, word_b) in b.iter().enumerate() {
            let cost = usize::from(word_a != word_b);
            let next = (row[j] + 1).min(row[j + 1] + 1).min(diagonal + cost);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Whether the model's echo disagrees with the caption, or the caption is too short to trust.
#[must_use]
pub fn disagrees(heard: &str, caption: &str) -> bool {
    let caption = words(caption);
    if caption.len() < 2 {
        return true;
    }
    let distance = edit_distance(&words(heard), &caption);
    #[expect(
        clippy::cast_precision_loss,
        reason = "word counts are far below f64's exact integer range"
    )]
    let ratio = distance as f64 / caption.len() as f64;
    ratio > MAX_HEARD_MISMATCH
}

#[cfg(test)]
mod tests {
    use super::disagrees;

    #[test]
    fn compares_normalized_words_against_the_caption() {
        let cases = [
            (
                "The user said: \"turn on the lights\"",
                "turn on the lights",
                false,
            ),
            (
                "the USER said: Turn on, the lights!",
                "Turn on the lights.",
                false,
            ),
            ("turn on the light", "turn on the lights", false),
            (
                "The user said: \"what's the weather\"",
                "whats the weather",
                false,
            ),
            (
                "The user said: \"play some jazz\"",
                "turn on the lights",
                true,
            ),
            ("the user said: lights", "turn on the lights", true),
            ("", "turn on the lights", true),
            ("The user said: \"atmosphere\"", " atmosphere,", true),
            ("anything at all", "", true),
        ];
        for (heard, caption, expected) in cases {
            assert_eq!(
                disagrees(heard, caption),
                expected,
                "{heard:?} vs {caption:?}"
            );
        }
    }
}
