//! Owns where one stretch of speech ends and the next begins: the endpoint, the flush before a final and the partials between.

use std::sync::Arc;

use crate::frame::Frame;
use crate::local::SpeechStream;
use crate::recognizer::{RecognizerError, RecognizerEvent};
use crate::vad::VadDetector;

const PCM_SCALE: f32 = 32768.0;

/// Blank frames to wait for tokens that the look-ahead found before ending the stretch anyway.
const HOLD_FRAMES: usize = 4;

fn text(tokens: &[usize], pieces: &[String]) -> String {
    tokens
        .iter()
        .filter_map(|id| pieces.get(*id))
        .flat_map(|piece| piece.chars())
        .map(|c| if c == '\u{2581}' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A streaming model that turns samples into the tokens of each encoder frame.
pub trait FrameDecoder: Send + 'static {
    /// Takes 16 kHz samples scaled to -1..1 and returns, for every encoder frame they completed,
    /// the tokens it emitted; an empty list is a blank.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError>;

    /// The tokens that `silence` after everything received so far would still bring out, found
    /// without changing what the decoder holds.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn lookahead(&self, silence: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError>;

    /// A new stretch of text begins; what the model heard stays its acoustic context.
    fn next_segment(&mut self);
}

/// Cuts a decoder's tokens into partials and finals.
pub struct Segments<D> {
    decoder: D,
    pieces: Arc<Vec<String>>,
    /// Blank encoder frames after the last token that end a stretch of speech.
    endpoint_blanks: usize,
    /// Samples of silence that the look-ahead appends.
    lookahead_samples: usize,
    spoken: Vec<usize>,
    trailing_blanks: usize,
    /// The look-ahead was done in the current silence.
    looked_ahead: bool,
    shown: String,
}

impl<D: FrameDecoder> Segments<D> {
    /// `lookahead_samples` is the silence to look ahead by, enough for the model to bring out
    /// the words it has already heard.
    #[must_use]
    pub fn new(
        decoder: D,
        pieces: Arc<Vec<String>>,
        endpoint_blanks: usize,
        lookahead_samples: usize,
    ) -> Self {
        Self {
            decoder,
            pieces,
            endpoint_blanks,
            lookahead_samples,
            spoken: Vec::new(),
            trailing_blanks: 0,
            looked_ahead: false,
            shown: String::new(),
        }
    }

    fn text(&self) -> String {
        text(&self.spoken, &self.pieces)
    }

    /// Whether the stretch of speech is over. The model hears a word some frames after it is
    /// spoken, so before ending, it looks ahead for words already received; if there are some,
    /// the stretch goes on until they arrive.
    fn ends_here(&mut self) -> Result<bool, RecognizerError> {
        if self.spoken.is_empty() || self.trailing_blanks < self.endpoint_blanks {
            return Ok(false);
        }
        if self.looked_ahead {
            return Ok(self.trailing_blanks >= self.endpoint_blanks + HOLD_FRAMES);
        }
        self.looked_ahead = true;
        let pending = self.decoder.lookahead(&vec![0.0; self.lookahead_samples])?;
        Ok(pending.iter().all(Vec::is_empty))
    }

    fn take(&mut self, frames: Vec<Vec<usize>>) {
        for tokens in frames {
            if tokens.is_empty() {
                self.trailing_blanks += 1;
            } else {
                self.spoken.extend(tokens);
                // A frame that emitted ends in a blank, which sherpa-onnx's endpoint counts too.
                self.trailing_blanks = 1;
                self.looked_ahead = false;
            }
        }
    }
}

/// Re-decodes tentative audio; no token is irrevocable until its speech stretch ends.
pub trait SnapshotDecoder: Send + 'static {
    /// Returns tokens by 80 ms encoder frame, replacing the preceding snapshot.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn snapshot(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError>;

    /// Reconsiders the complete stretch before publishing immutable text.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn finish(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError>;

    /// The finalized stretch is no longer part of the tentative decoding state.
    fn next_segment(&mut self);
}

/// Live text from an offline encoder, with each silence-delimited stretch decoded as a whole.
pub struct Revising<D> {
    decoder: D,
    pieces: Arc<Vec<String>>,
    audio: Vec<f32>,
    waiting: usize,
    quiet: usize,
    shown: String,
    recent: std::collections::VecDeque<Frame>,
}

impl<D: SnapshotDecoder> Revising<D> {
    #[must_use]
    pub fn new(decoder: D, pieces: Arc<Vec<String>>) -> Self {
        Self {
            decoder,
            pieces,
            audio: Vec::new(),
            waiting: 0,
            quiet: 0,
            shown: String::new(),
            recent: std::collections::VecDeque::new(),
        }
    }

    fn push(
        &mut self,
        samples: &[f32],
        speech: bool,
    ) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        const UPDATE: usize = 10_240;
        const ENDPOINT: usize = 10_240;
        if speech {
            self.quiet = 0;
        } else {
            self.quiet += samples.len();
        }
        if self.audio.is_empty() && samples.iter().all(|sample| *sample == 0.0) {
            return Ok(Vec::new());
        }
        self.audio.extend_from_slice(samples);
        self.waiting += samples.len();
        let finish = self.quiet >= ENDPOINT;
        let just_quiet = finish && self.quiet - samples.len() < ENDPOINT;
        if self.waiting < UPDATE && !just_quiet {
            return Ok(Vec::new());
        }
        self.waiting = 0;
        let frames = if finish {
            self.decoder.finish(&self.audio)?
        } else {
            self.decoder.snapshot(&self.audio)?
        };
        let trailing = frames
            .iter()
            .rev()
            .take_while(|frame| frame.is_empty())
            .count();
        let tokens: Vec<usize> = frames.into_iter().flatten().collect();
        let current = text(&tokens, &self.pieces);
        if finish && trailing >= 8 && !current.is_empty() {
            self.audio.clear();
            self.shown.clear();
            self.decoder.next_segment();
            return Ok(vec![RecognizerEvent::Final(current)]);
        }
        if current == self.shown {
            return Ok(Vec::new());
        }
        self.shown.clone_from(&current);
        Ok(vec![RecognizerEvent::Partial(current)])
    }
}

impl<D: SnapshotDecoder> SpeechStream for Revising<D> {
    fn accept(&mut self, frame: &Frame) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        self.recent.push_back(*frame);
        if self.recent.len() > 10 {
            self.recent.pop_front();
        }
        // The VAD handle cannot cross threads; replay a bounded history on this worker instead.
        let mut vad = VadDetector::new();
        let mut speech = false;
        for frame in &self.recent {
            speech = vad.speech(frame, false);
        }
        let samples: Vec<f32> = frame.iter().map(|s| f32::from(*s) / PCM_SCALE).collect();
        self.push(&samples, speech)
    }
}

impl<D: FrameDecoder> SpeechStream for Segments<D> {
    fn accept(&mut self, frame: &Frame) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        let samples: Vec<f32> = frame.iter().map(|s| f32::from(*s) / PCM_SCALE).collect();
        let frames = self.decoder.push(&samples)?;
        let mut events = Vec::new();
        for encoded in frames {
            self.take(vec![encoded]);
            if !self.ends_here()? {
                continue;
            }
            events.push(RecognizerEvent::Final(self.text()));
            self.spoken.clear();
            self.trailing_blanks = 0;
            self.looked_ahead = false;
            self.shown.clear();
            self.decoder.next_segment();
        }
        let text = self.text();
        if text != self.shown {
            events.push(RecognizerEvent::Partial(text.clone()));
            self.shown = text;
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FRAME_SAMPLES;

    type ScriptedSnapshot = (bool, usize, Vec<(usize, usize)>);

    struct Snapshots {
        script: std::collections::VecDeque<ScriptedSnapshot>,
        resets: usize,
    }

    impl Snapshots {
        fn next(&mut self, audio: &[f32], finishing: bool) -> Vec<Vec<usize>> {
            let (fresh, count, tokens) = self.script.pop_front().expect("snapshot scripted");
            assert_eq!(finishing, fresh, "a final uses a fresh full-stretch decode");
            assert_eq!(
                audio.len() / FRAME_SAMPLES,
                count,
                "audio is retained until its final"
            );
            let mut frames = vec![Vec::new(); count];
            for (at, token) in tokens {
                frames[at].push(token);
            }
            frames
        }
    }

    impl SnapshotDecoder for Snapshots {
        fn snapshot(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
            Ok(self.next(audio, false))
        }

        fn finish(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
            Ok(self.next(audio, true))
        }

        fn next_segment(&mut self) {
            self.resets += 1;
        }
    }

    fn run(
        script: &str,
        snapshots: Vec<ScriptedSnapshot>,
    ) -> (Vec<RecognizerEvent>, Revising<Snapshots>) {
        let pieces = Arc::new(["▁wrong", "▁one", "▁two"].map(str::to_owned).to_vec());
        let mut stream = Revising::new(
            Snapshots {
                script: snapshots.into(),
                resets: 0,
            },
            pieces,
        );
        let mut events = Vec::new();
        for mark in script.chars() {
            for _ in 0..8 {
                let samples = [if mark == '.' { 0.0 } else { 0.01 }; FRAME_SAMPLES];
                events.extend(stream.push(&samples, mark == 'S').unwrap());
            }
        }
        (events, stream)
    }

    #[test]
    fn tentative_text_can_be_replaced_but_finals_are_disjoint_and_survive_a_long_pause() {
        let cases = [
            (
                "revised word",
                vec![
                    (false, 8, vec![(0, 0)]),
                    (false, 16, vec![(0, 1), (1, 2)]),
                    (true, 24, vec![(0, 1), (1, 2)]),
                ],
                "SS.",
                vec![
                    RecognizerEvent::Partial("wrong".into()),
                    RecognizerEvent::Partial("one two".into()),
                    RecognizerEvent::Final("one two".into()),
                ],
            ),
            (
                "separate windows and long pause",
                vec![
                    (false, 8, vec![(0, 1)]),
                    (true, 16, vec![(0, 1)]),
                    (false, 8, vec![(0, 2)]),
                    (true, 16, vec![(0, 2)]),
                ],
                "S..............S.",
                vec![
                    RecognizerEvent::Partial("one".into()),
                    RecognizerEvent::Final("one".into()),
                    RecognizerEvent::Partial("two".into()),
                    RecognizerEvent::Final("two".into()),
                ],
            ),
            (
                "silence alone cannot cut a pending word",
                vec![
                    (false, 8, vec![(0, 1)]),
                    (true, 16, vec![(0, 1), (15, 2)]),
                    (true, 24, vec![(0, 1), (15, 2)]),
                ],
                "S..",
                vec![
                    RecognizerEvent::Partial("one".into()),
                    RecognizerEvent::Partial("one two".into()),
                    RecognizerEvent::Final("one two".into()),
                ],
            ),
            (
                "quiet opening remains available",
                vec![
                    (true, 8, vec![]),
                    (false, 16, vec![(0, 1)]),
                    (true, 24, vec![(0, 1)]),
                ],
                "qS.",
                vec![
                    RecognizerEvent::Partial("one".into()),
                    RecognizerEvent::Final("one".into()),
                ],
            ),
            (
                "the final can correct tentative words",
                vec![(false, 8, vec![(0, 0)]), (true, 16, vec![(0, 1)])],
                "S.",
                vec![
                    RecognizerEvent::Partial("wrong".into()),
                    RecognizerEvent::Final("one".into()),
                ],
            ),
            (
                "an empty recheck is not a final",
                vec![
                    (false, 8, vec![(0, 0)]),
                    (true, 16, vec![]),
                    (true, 24, vec![(0, 1)]),
                ],
                "S..",
                vec![
                    RecognizerEvent::Partial("wrong".into()),
                    RecognizerEvent::Partial(String::new()),
                    RecognizerEvent::Final("one".into()),
                ],
            ),
        ];
        for (name, snapshots, script, expected) in cases {
            let (events, stream) = run(script, snapshots);
            assert_eq!(events, expected, "{name}");
            assert!(stream.decoder.script.is_empty(), "{name}");
            assert_eq!(
                stream.decoder.resets,
                expected
                    .iter()
                    .filter(|event| matches!(event, RecognizerEvent::Final(_)))
                    .count(),
                "a new stretch starts only after a final: {name}"
            );
        }
    }

}
