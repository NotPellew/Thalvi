//! Owns splitting a streamed reply into sentences as deltas arrive.

/// Incremental sentence splitter for streamed model output.
#[derive(Debug, Default)]
pub struct SentenceSplitter {
    buffer: String,
}

impl SentenceSplitter {
    /// Creates an empty splitter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one text delta and returns every sentence completed by it.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        let mut sentences = Vec::new();
        while let Some(end) = sentence_end(&self.buffer) {
            let sentence: String = self.buffer.drain(..end).collect();
            let sentence = sentence.trim().to_string();
            self.buffer = self.buffer.trim_start().to_string();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
        }
        sentences
    }

    /// Returns the remaining buffered text as a final sentence, if any.
    pub fn finish(&mut self) -> Option<String> {
        let tail = self.buffer.trim().to_string();
        self.buffer.clear();
        if tail.is_empty() { None } else { Some(tail) }
    }
}

fn sentence_end(buffer: &str) -> Option<usize> {
    let bytes = buffer.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(byte, b'.' | b'!' | b'?') {
            match bytes.get(index + 1) {
                Some(next) if next.is_ascii_whitespace() => return Some(index + 1),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::SentenceSplitter;

    #[test]
    fn splits_only_on_terminator_followed_by_whitespace() {
        let cases: Vec<(Vec<&str>, Vec<&str>, Option<&str>)> = vec![
            (
                vec!["Hello there. How are you? Great! Bye"],
                vec!["Hello there.", "How are you?", "Great!"],
                Some("Bye"),
            ),
            (
                vec!["Pi is 3.5 roughly. Done"],
                vec!["Pi is 3.5 roughly."],
                Some("Done"),
            ),
            (
                vec!["Split acr", "oss deltas. Tail"],
                vec!["Split across deltas."],
                Some("Tail"),
            ),
            (vec!["No terminator"], vec![], Some("No terminator")),
            (vec!["End."], vec![], Some("End.")),
            (vec!["Wow! ", "Next"], vec!["Wow!"], Some("Next")),
        ];
        for (deltas, expected_sentences, expected_tail) in cases {
            let mut splitter = SentenceSplitter::new();
            let mut sentences = Vec::new();
            for delta in deltas {
                sentences.extend(splitter.push(delta));
            }
            assert_eq!(sentences, expected_sentences);
            assert_eq!(splitter.finish().as_deref(), expected_tail);
        }
    }
}
