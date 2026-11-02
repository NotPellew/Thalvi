//! Owns where one stretch of speech ends and the next begins: the endpoint, the flush before a final and the partials between.

use std::sync::Arc;

use crate::frame::Frame;
use crate::local::SpeechStream;
use crate::recognizer::{RecognizerError, RecognizerEvent};

const PCM_SCALE: f32 = 32768.0;

/// Silence appended to look ahead of the audio received: a 25-frame window plus the last frame's tail.
const LOOKAHEAD_SAMPLES: usize = 4_400;

/// Blank frames to wait for tokens that the look-ahead found before ending the stretch anyway.
const HOLD_FRAMES: usize = 4;

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
    spoken: Vec<usize>,
    trailing_blanks: usize,
    /// The look-ahead was done in the current silence.
    looked_ahead: bool,
    shown: String,
}

impl<D: FrameDecoder> Segments<D> {
    #[must_use]
    pub fn new(decoder: D, pieces: Arc<Vec<String>>, endpoint_blanks: usize) -> Self {
        Self {
            decoder,
            pieces,
            endpoint_blanks,
            spoken: Vec::new(),
            trailing_blanks: 0,
            looked_ahead: false,
            shown: String::new(),
        }
    }

    fn text(&self) -> String {
        self.spoken
            .iter()
            .filter_map(|id| self.pieces.get(*id))
            .flat_map(|piece| piece.chars())
            .map(|c| if c == '\u{2581}' { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_owned()
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
        let pending = self.decoder.lookahead(&vec![0.0; LOOKAHEAD_SAMPLES])?;
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
