//! Stretches of speech end when the speaker is silent, and never before the words already spoken are in.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::sync::Arc;

use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::SpeechStream;
use thalvi_desktop::recognizer::{RecognizerError, RecognizerEvent};
use thalvi_desktop::segments::{FrameDecoder, Segments};

/// Blank encoder frames after speech that end a stretch.
const ENDPOINT: usize = 4;
/// Frames between a word being spoken and the model emitting it.
const LAG: usize = 3;
const GHOST: usize = 99;
/// Silence the look-ahead is asked to find words in.
const LOOKAHEAD: usize = 3_000;

/// Emits each scripted word `LAG` frames after it was spoken; the look-ahead finds the words
/// spoken but not yet emitted, or a word that never comes when `ghost` is set.
struct Fake {
    spoken: Vec<(usize, usize)>,
    ghost: bool,
    frame: usize,
}

impl FrameDecoder for Fake {
    fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        assert_eq!(
            samples.len(),
            FRAME_SAMPLES,
            "only the live frames are pushed"
        );
        let now = self.frame;
        self.frame += 1;
        Ok(vec![
            self.spoken
                .iter()
                .filter(|(at, _)| at + LAG == now)
                .map(|(_, token)| *token)
                .collect(),
        ])
    }

    fn lookahead(&self, silence: &[f32]) -> Result<Vec<Vec<usize>>, RecognizerError> {
        assert_eq!(
            silence.len(),
            LOOKAHEAD,
            "the look-ahead is as long as the model needs"
        );
        let now = self.frame;
        let mut found: Vec<usize> = self
            .spoken
            .iter()
            .filter(|(at, _)| *at < now && at + LAG >= now)
            .map(|(_, token)| *token)
            .collect();
        if self.ghost {
            found.push(GHOST);
        }
        Ok(vec![found])
    }

    fn next_segment(&mut self) {}
}

/// Runs `frames` live frames of audio and returns the events with the frame each came at.
fn run(spoken: &[(usize, usize)], ghost: bool, frames: usize) -> Vec<(usize, RecognizerEvent)> {
    let pieces: Arc<Vec<String>> = Arc::new(
        ["\u{2581}one", "\u{2581}two", "\u{2581}three"]
            .iter()
            .map(|piece| (*piece).to_owned())
            .chain(std::iter::repeat_n("\u{2581}ghost".to_owned(), 97))
            .collect(),
    );
    let fake = Fake {
        spoken: spoken.to_vec(),
        ghost,
        frame: 0,
    };
    let mut segments = Segments::new(fake, pieces, ENDPOINT, LOOKAHEAD);
    let frame: Frame = [0; FRAME_SAMPLES];
    (0..frames)
        .flat_map(|at| {
            segments
                .accept(&frame)
                .expect("frame is accepted")
                .into_iter()
                .map(move |event| (at, event))
        })
        .collect()
}

fn finals(events: &[(usize, RecognizerEvent)]) -> Vec<(usize, &str)> {
    events
        .iter()
        .filter_map(|(at, event)| match event {
            RecognizerEvent::Final(text) => Some((*at, text.as_str())),
            RecognizerEvent::Partial(_) | RecognizerEvent::Ended => None,
        })
        .collect()
}

#[test]
fn a_stretch_ends_in_a_final_with_the_words_the_look_ahead_still_held() {
    for (case, spoken, ghost, frames, expected) in [
        (
            "silence ends a stretch as soon as the model has nothing more for it: \"one\" is emitted at frame 3, and its own frame and three blanks make four at frame 6",
            &[(0, 0)][..],
            false,
            12,
            &[(6, "one")][..],
        ),
        (
            "a word spoken as the endpoint falls is in the final: \"two\" is spoken at frame 6, still in the look-ahead when the endpoint is reached, and arrives at frame 9",
            &[(0, 0), (6, 1)],
            false,
            20,
            &[(12, "one two")],
        ),
        (
            "words after a long pause start the next stretch",
            &[(0, 0), (14, 1)],
            false,
            30,
            &[(6, "one"), (20, "two")],
        ),
        (
            "words the look-ahead promised but never came delay the final only briefly",
            &[(0, 0)],
            true,
            20,
            &[(10, "one")],
        ),
    ] {
        let events = run(spoken, ghost, frames);
        assert_eq!(finals(&events), expected, "{case}");
    }
}
