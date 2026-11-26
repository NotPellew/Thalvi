//! Owns where one stretch of speech ends and the next begins: the endpoint, the flush before a final and the partials between.

use std::sync::Arc;

use crate::frame::Frame;
use crate::local::SpeechStream;
use crate::recognizer::{RecognizerError, RecognizerEvent};
use crate::vad::VadDetector;

const PCM_SCALE: f32 = 32768.0;

/// Blank frames to wait for tokens that the look-ahead found before ending the stretch anyway.
const HOLD_FRAMES: usize = 4;

/// Quiet audio kept once a whole-stretch decode finds no words: room noise must not grow the
/// stretch, while a quiet word the VAD missed still has its start.
const WORDLESS_TAIL: usize = 16 * 1280;

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

    /// Whether queued frames may be pushed together so a sliding-window encoder catches up in one
    /// pass instead of one per step.
    fn batches_frames(&self) -> bool {
        false
    }
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
    /// Returns tokens by 80 ms encoder frame, replacing the preceding snapshot. `speech` tells
    /// whether the voice detector heard speech since the previous call.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn snapshot(&mut self, audio: &[f32], speech: bool)
    -> Result<Vec<Vec<usize>>, RecognizerError>;

    /// Commits every frame received, including the newest, and returns the whole stretch's tokens.
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
    zeros: usize,
    shown: String,
    /// The voice detector heard speech since the decoder last looked.
    spoke: bool,
    vad: VadDetector,
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
            zeros: 0,
            shown: String::new(),
            spoke: false,
            vad: VadDetector::new(),
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
            self.spoke = true;
        } else {
            self.quiet += samples.len();
        }
        let silent = samples.iter().all(|sample| *sample == 0.0);
        if self.audio.is_empty() && silent {
            return Ok(Vec::new());
        }
        self.zeros = if silent {
            self.zeros + samples.len()
        } else {
            0
        };
        // A microphone never sends digital silence; the engine does, to end the input.
        let flush = self.zeros >= ENDPOINT;
        self.audio.extend_from_slice(samples);
        self.waiting += samples.len();
        let finish = flush || self.quiet >= ENDPOINT;
        let just_quiet = flush || finish && self.quiet - samples.len() < ENDPOINT;
        if self.waiting < UPDATE && !just_quiet {
            return Ok(Vec::new());
        }
        self.waiting = 0;
        let spoke = std::mem::take(&mut self.spoke);
        let frames = if finish {
            self.decoder.finish(&self.audio)?
        } else {
            self.decoder.snapshot(&self.audio, spoke)?
        };
        let trailing = frames
            .iter()
            .rev()
            .take_while(|frame| frame.is_empty())
            .count();
        let tokens: Vec<usize> = frames.into_iter().flatten().collect();
        let current = text(&tokens, &self.pieces);
        if finish && (flush || trailing >= 8) && !current.is_empty() {
            self.audio.clear();
            self.zeros = 0;
            self.shown.clear();
            self.decoder.next_segment();
            return Ok(vec![RecognizerEvent::Final(current)]);
        }
        if flush {
            self.audio.clear();
            self.zeros = 0;
            self.decoder.next_segment();
        } else if finish && current.is_empty() && self.audio.len() > WORDLESS_TAIL {
            self.audio.drain(..self.audio.len() - WORDLESS_TAIL);
            self.decoder.next_segment();
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
        let speech = self.vad.speech(frame, false);
        let samples: Vec<f32> = frame.iter().map(|s| f32::from(*s) / PCM_SCALE).collect();
        self.push(&samples, speech)
    }

    fn end_input(&mut self) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        if self.audio.is_empty() {
            return Ok(Vec::new());
        }
        let frames = self.decoder.finish(&self.audio)?;
        let tokens: Vec<usize> = frames.into_iter().flatten().collect();
        let current = text(&tokens, &self.pieces);
        self.audio.clear();
        self.zeros = 0;
        self.shown.clear();
        self.decoder.next_segment();
        Ok(if current.is_empty() {
            Vec::new()
        } else {
            vec![RecognizerEvent::Final(current)]
        })
    }
}

impl<D: FrameDecoder> SpeechStream for Segments<D> {
    fn accept(&mut self, frame: &Frame) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        self.accept_batch(std::slice::from_ref(frame))
    }

    fn accept_batch(&mut self, frames: &[Frame]) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        let samples: Vec<f32> = frames
            .iter()
            .flatten()
            .map(|s| f32::from(*s) / PCM_SCALE)
            .collect();
        let encoded_frames = self.decoder.push(&samples)?;
        let mut events = Vec::new();
        for encoded in encoded_frames {
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

    fn batches_frames(&self) -> bool {
        self.decoder.batches_frames()
    }

    fn end_input(&mut self) -> Result<Vec<RecognizerEvent>, RecognizerError> {
        let pending = self.decoder.lookahead(&vec![0.0; self.lookahead_samples])?;
        self.take(pending);
        if self.spoken.is_empty() {
            return Ok(Vec::new());
        }
        let text = self.text();
        self.spoken.clear();
        self.trailing_blanks = 0;
        self.looked_ahead = false;
        self.shown.clear();
        self.decoder.next_segment();
        Ok(vec![RecognizerEvent::Final(text)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FRAME_SAMPLES;
    use crate::vad::tests::{hiss, voiced};

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
        fn snapshot(
            &mut self,
            audio: &[f32],
            _speech: bool,
        ) -> Result<Vec<Vec<usize>>, RecognizerError> {
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
                "Sqq",
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
                "Sqq",
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

    #[test]
    fn the_end_of_the_input_brings_out_the_words_the_stretch_still_holds() {
        let (events, mut stream) =
            run("S", vec![(false, 8, vec![(0, 1)]), (true, 8, vec![(0, 1)])]);
        assert_eq!(events, [RecognizerEvent::Partial("one".into())]);
        assert_eq!(
            stream.end_input().unwrap(),
            [RecognizerEvent::Final("one".into())]
        );
        assert!(stream.decoder.script.is_empty());
        assert_eq!(stream.decoder.resets, 1);
        assert_eq!(
            stream.end_input().unwrap(),
            [],
            "an input that already ended brings out nothing more"
        );
    }

    #[test]
    fn room_noise_without_words_keeps_only_a_bounded_tail() {
        let mut snapshots = vec![(true, 8, vec![]), (true, 16, vec![])];
        snapshots.extend(std::iter::repeat_n((true, 24, vec![]), 4));
        snapshots.extend([(false, 24, vec![(23, 1)]), (true, 32, vec![(23, 1)])]);
        let (events, stream) = run("qqqqqqS.", snapshots);
        assert_eq!(
            events,
            [
                RecognizerEvent::Partial("one".into()),
                RecognizerEvent::Final("one".into()),
            ]
        );
        assert!(stream.decoder.script.is_empty());
        assert_eq!(stream.decoder.resets, 5, "each trim restarts the decoding");
    }

    #[test]
    fn digital_silence_ends_the_stretch_at_once() {
        for (name, snapshots, script, expected) in [
            (
                "with its pending word",
                vec![(false, 8, vec![(0, 1)]), (true, 16, vec![(0, 1), (15, 2)])],
                "S.",
                "one two",
            ),
            (
                "and a wordless stretch is not carried into the next",
                vec![
                    (true, 8, vec![]),
                    (true, 16, vec![]),
                    (false, 8, vec![(0, 1)]),
                    (true, 16, vec![(0, 1)]),
                ],
                "q.S.",
                "one",
            ),
        ] {
            let (events, stream) = run(script, snapshots);
            assert_eq!(
                events.last(),
                Some(&RecognizerEvent::Final(expected.into())),
                "{name}"
            );
            assert!(stream.decoder.script.is_empty(), "{name}");
        }
    }

    struct Encoded {
        frames: std::collections::VecDeque<Vec<Vec<usize>>>,
        resets: usize,
    }

    impl FrameDecoder for Encoded {
        fn push(&mut self, _samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
            Ok(self.frames.pop_front().unwrap_or_default())
        }

        fn lookahead(&self, _silence: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
            Ok(Vec::new())
        }

        fn next_segment(&mut self) {
            self.resets += 1;
        }
    }

    #[test]
    fn a_stream_that_never_reaches_its_endpoint_answers_when_the_input_ends() {
        let pieces = Arc::new(["▁one"].map(str::to_owned).to_vec());
        let mut stream = Segments::new(
            Encoded {
                frames: std::collections::VecDeque::from([vec![vec![0]], vec![]]),
                resets: 0,
            },
            pieces,
            8,
            1_280,
        );
        let frame = [0i16; FRAME_SAMPLES];
        let mut events = stream.accept(&frame).unwrap();
        events.extend(stream.accept(&frame).unwrap());
        assert_eq!(events, [RecognizerEvent::Partial("one".into())]);
        assert_eq!(
            stream.end_input().unwrap(),
            [RecognizerEvent::Final("one".into())]
        );
        assert_eq!(
            stream.end_input().unwrap(),
            [],
            "a stretch that already ended brings out nothing more"
        );
    }

    fn synthetic_capture(script: &str) -> Vec<Frame> {
        let mut state = 1;
        let mut offset = 0;
        script
            .chars()
            .flat_map(|mark| std::iter::repeat_n(mark, 8))
            .map(|mark| match mark {
                'S' => {
                    let frame = voiced(offset);
                    offset += FRAME_SAMPLES;
                    frame
                }
                'q' => hiss(50, &mut state),
                '.' => [0; FRAME_SAMPLES],
                _ => panic!("unknown fixture mark {mark}"),
            })
            .collect()
    }

    /// The fixture's voiced frames peak above 0.08 and its room noise below 0.002.
    struct SampleDecoder;

    impl SnapshotDecoder for SampleDecoder {
        fn snapshot(
            &mut self,
            audio: &[f32],
            _speech: bool,
        ) -> Result<Vec<Vec<usize>>, RecognizerError> {
            self.finish(audio)
        }

        fn finish(&mut self, audio: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
            Ok(audio
                .chunks(FRAME_SAMPLES)
                .map(|frame| {
                    if frame.iter().any(|sample| sample.abs() > 0.01) {
                        vec![0]
                    } else {
                        Vec::new()
                    }
                })
                .collect())
        }

        fn next_segment(&mut self) {}
    }

    fn finals(events: Vec<RecognizerEvent>) -> impl Iterator<Item = String> {
        events.into_iter().filter_map(|event| match event {
            RecognizerEvent::Final(text) => Some(text),
            _ => None,
        })
    }

    #[test]
    fn persistent_vad_preserves_the_segments_of_the_ten_frame_replay() {
        for (script, stretches) in [
            ("SS.", 1),
            ("S..............S.", 2),
            ("Sqq", 1),
            ("qS.", 1),
            ("S.", 1),
            ("q.S.", 1),
            (
                "..........SSSSqqqqSSSSqq......................................",
                2,
            ),
        ] {
            let capture = synthetic_capture(script);
            let pieces = Arc::new(vec!["▁one".to_owned()]);
            let mut current = Revising::new(SampleDecoder, Arc::clone(&pieces));
            let mut replay = Revising::new(SampleDecoder, pieces);
            let mut current_finals = Vec::new();
            let mut replay_finals = Vec::new();
            let mut current_flags = String::new();
            let mut replay_flags = String::new();
            let mut differences = Vec::new();

            for (index, frame) in capture.iter().enumerate() {
                current_finals.extend(finals(current.accept(frame).unwrap()));
                // Every frame has samples, so only a speech decision leaves quiet at zero.
                let current_speech = current.quiet == 0;

                let mut vad = VadDetector::new();
                let mut replay_speech = false;
                for prior in &capture[index.saturating_sub(9)..=index] {
                    replay_speech = vad.speech(prior, false);
                }
                let samples: Vec<_> = frame
                    .iter()
                    .map(|sample| f32::from(*sample) / PCM_SCALE)
                    .collect();
                replay_finals.extend(finals(replay.push(&samples, replay_speech).unwrap()));

                current_flags.push(if current_speech { 'S' } else { '.' });
                replay_flags.push(if replay_speech { 'S' } else { '.' });
                if current_speech != replay_speech {
                    differences.push(index);
                }
            }

            println!(
                "{script}\nreplay:  {replay_flags}\ncurrent: {current_flags}\ndiffering frames: {differences:?}"
            );
            if script.starts_with("..........SSSS") {
                assert!(
                    !differences.is_empty(),
                    "room noise must exercise adaptation"
                );
            }
            current_finals.extend(finals(current.end_input().unwrap()));
            replay_finals.extend(finals(replay.end_input().unwrap()));
            assert_eq!(current_finals, replay_finals, "{script}");
            assert_eq!(current_finals.len(), stretches, "{script}");
        }
    }
}
