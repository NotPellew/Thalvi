//! Audio from a file in place of the microphone: a WAV read into frames, and played into the engine at the pace of speech.

use std::io::{self, Read};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until};

use crate::audio::{AudioError, Framer, InputGate};
use crate::engine::FrameSource;
use crate::frame::{FRAME_SAMPLES, Frame, Queued, SAMPLE_RATE};

/// How long one frame lasts, which is the pace a file is played at.
const FRAME_TIME: Duration = Duration::from_millis(80);
const _: () = assert!(FRAME_SAMPLES * 1_000 == 80 * SAMPLE_RATE as usize);

/// The sample rates a file may have; the framer converts any of them like a microphone's.
const RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;

/// Bytes of audio read at a time.
const BLOCK: usize = 32 * 1024;

/// Why a file cannot be fed, in plain words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    #[error("There is no audio file at {0}.")]
    Missing(String),
    #[error("The audio file could not be read: {0}.")]
    Unreadable(String),
    #[error("The file is not a WAV file.")]
    NotWav,
    #[error("Only 16-bit PCM WAV files at 8 to 192 kHz can be fed.")]
    Format,
    #[error("The audio file holds no audio.")]
    Empty,
    #[error("The audio is longer than the {} seconds a capture may take.", .0.as_secs())]
    TooLong(Duration),
}

/// A WAV file's audio as the 16 kHz mono frames the microphone gives.
#[derive(Clone, PartialEq, Eq)]
pub struct Clip {
    frames: Vec<Frame>,
}

impl std::fmt::Debug for Clip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Clip({} frames)", self.frames.len())
    }
}

/// The parts of a WAV file's `fmt ` chunk that matter here.
struct Format {
    channels: u16,
    rate: u32,
}

impl Clip {
    /// Frames to play as they are.
    #[must_use]
    pub fn from_frames(frames: Vec<Frame>) -> Self {
        Self { frames }
    }

    /// Reads the PCM WAV file at `path`, converting its rate and channels as microphone input is.
    ///
    /// # Errors
    /// Why the file cannot be fed, including that it is longer than `max`.
    pub fn from_wav(path: &Path, max: Duration) -> Result<Self, FeedError> {
        let file = std::fs::File::open(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => FeedError::Missing(path.display().to_string()),
            _ => FeedError::Unreadable(error.to_string()),
        })?;
        Self::from_reader(file, max)
    }

    /// Reads a PCM WAV stream; see [`Clip::from_wav`].
    ///
    /// # Errors
    /// Why the stream cannot be fed.
    pub fn from_reader(reader: impl Read, max: Duration) -> Result<Self, FeedError> {
        let mut reader = io::BufReader::new(reader);
        let (format, declared) = read_header(&mut reader)?;
        let width = 2 * usize::from(format.channels);
        let limit = u64::try_from(max.as_micros() * u128::from(format.rate) / 1_000_000)
            .unwrap_or(u64::MAX);
        // A streamed file declares no length (0 or all ones), so its audio is counted as it is read.
        let streamed = declared == 0 || declared == u32::MAX;
        if !streamed && u64::from(declared) / u64::try_from(width).unwrap_or(1) > limit {
            return Err(FeedError::TooLong(max));
        }
        let mut data = reader.take(if streamed {
            u64::MAX
        } else {
            u64::from(declared)
        });
        let mut framer = Framer::new(format.rate);
        let mut frames = Vec::new();
        let mut emit = |frame: &Frame| frames.push(*frame);
        let mut block = vec![0_u8; BLOCK / width * width];
        let mut total = 0_u64;
        loop {
            let read = fill(&mut data, &mut block)?;
            let whole = read / width * width;
            total += u64::try_from(whole / width).unwrap_or(u64::MAX);
            if total > limit {
                return Err(FeedError::TooLong(max));
            }
            for samples in block[..whole].chunks_exact(width) {
                let mono = samples
                    .chunks_exact(2)
                    .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32_768.0)
                    .sum::<f32>()
                    / f32::from(format.channels);
                framer.push(mono, &mut emit);
            }
            if read < block.len() {
                break;
            }
        }
        framer.flush(&mut emit);
        if frames.is_empty() {
            return Err(FeedError::Empty);
        }
        Ok(Self { frames })
    }
}

/// Reads as much of `block` as the stream has.
fn fill(reader: &mut impl Read, block: &mut [u8]) -> Result<usize, FeedError> {
    let mut filled = 0;
    while filled < block.len() {
        match reader.read(&mut block[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FeedError::Unreadable(error.to_string())),
        }
    }
    Ok(filled)
}

fn not_wav(error: io::Error) -> FeedError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        FeedError::NotWav
    } else {
        FeedError::Unreadable(error.to_string())
    }
}

/// Reads up to the audio of a WAV file: its format and the declared length of its data.
fn read_header(reader: &mut impl Read) -> Result<(Format, u32), FeedError> {
    let mut riff = [0_u8; 12];
    reader.read_exact(&mut riff).map_err(not_wav)?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
        return Err(FeedError::NotWav);
    }
    let mut format = None;
    loop {
        let mut head = [0_u8; 8];
        reader.read_exact(&mut head).map_err(not_wav)?;
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        match &head[..4] {
            b"fmt " => {
                if size < 16 {
                    return Err(FeedError::NotWav);
                }
                let mut fields = [0_u8; 16];
                reader.read_exact(&mut fields).map_err(not_wav)?;
                skip(reader, u64::from(size - 16) + u64::from(size & 1))?;
                let tag = u16::from_le_bytes([fields[0], fields[1]]);
                let channels = u16::from_le_bytes([fields[2], fields[3]]);
                let rate = u32::from_le_bytes([fields[4], fields[5], fields[6], fields[7]]);
                let bits = u16::from_le_bytes([fields[14], fields[15]]);
                if tag != 1 || bits != 16 || channels == 0 || !RATES.contains(&rate) {
                    return Err(FeedError::Format);
                }
                format = Some(Format { channels, rate });
            }
            b"data" => {
                let format = format.ok_or(FeedError::NotWav)?;
                return Ok((format, size));
            }
            _ => skip(reader, u64::from(size) + u64::from(size & 1))?,
        }
    }
}

fn skip(reader: &mut impl Read, bytes: u64) -> Result<(), FeedError> {
    let skipped = io::copy(&mut reader.take(bytes), &mut io::sink()).map_err(not_wav)?;
    if skipped == bytes {
        Ok(())
    } else {
        Err(FeedError::NotWav)
    }
}

/// How a fed file ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedEnd {
    /// Every frame was given to the engine.
    Played,
    /// Stopped before the last frame.
    Cancelled,
    /// Never started, for this reason.
    Refused(&'static str),
}

/// A clip to play, and where to say how it ended.
#[derive(Debug)]
pub struct Feed {
    clip: Clip,
    end: oneshot::Sender<FeedEnd>,
}

impl Feed {
    #[must_use]
    pub fn new(clip: Clip) -> (Self, oneshot::Receiver<FeedEnd>) {
        let (end, ended) = oneshot::channel();
        (Self { clip, end }, ended)
    }

    /// Ends the feed before it starts.
    pub fn refuse(self, reason: &'static str) {
        let _ = self.end.send(FeedEnd::Refused(reason));
    }
}

/// A frame of a clip with the gate epoch and feed it was played in.
type Played = (u64, u64, Queued);

struct Shared {
    gate: InputGate,
    /// A clip is playing; the microphone's frames are dropped meanwhile.
    feeding: Arc<watch::Sender<bool>>,
    /// Bumped to stop the clip playing, and to drop its frames still queued.
    generation: watch::Sender<u64>,
    queue: Mutex<Option<mpsc::UnboundedSender<Played>>>,
}

/// Starts and stops the clip that [`Fed`] plays.
#[derive(Clone)]
pub struct Feeder(Arc<Shared>);

impl Feeder {
    /// A feeder for one microphone; `feeding` tells whether a clip is playing.
    #[must_use]
    pub fn new(gate: &InputGate, feeding: Arc<watch::Sender<bool>>) -> Self {
        Self(Arc::new(Shared {
            gate: gate.clone(),
            feeding,
            generation: watch::channel(0).0,
            queue: Mutex::new(None),
        }))
    }

    /// `microphone`'s frames, or the clip's while one plays. A source made earlier stops getting clips.
    #[must_use]
    pub fn wrap<S: FrameSource>(&self, microphone: S) -> Fed<S> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.queue.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        Fed {
            microphone,
            played: rx,
            gate: self.0.gate.clone(),
            feeding: Arc::clone(&self.0.feeding),
            generation: self.0.generation.subscribe(),
        }
    }

    /// Plays `feed` from now, one frame per frame's duration, unless one is playing already.
    pub fn start(&self, feed: Feed) {
        let Some(queue) = self
            .0
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return feed.refuse("Thalvi is not listening.");
        };
        if self.0.feeding.send_replace(true) {
            return feed.refuse("A file is already being fed.");
        }
        let generation = *self.0.generation.borrow();
        tokio::spawn(play(
            feed,
            queue,
            self.0.generation.subscribe(),
            generation,
            Arc::clone(&self.0),
        ));
    }

    /// Stops the clip playing, if one is.
    pub fn cancel(&self) {
        self.0
            .generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

/// Gives the frames of `feed` to `queue` in real time, as a microphone would, and says how it went.
async fn play(
    feed: Feed,
    queue: mpsc::UnboundedSender<Played>,
    mut stopped: watch::Receiver<u64>,
    generation: u64,
    shared: Arc<Shared>,
) {
    let start = Instant::now();
    let mut end = FeedEnd::Played;
    for (n, frame) in feed.clip.frames.into_iter().enumerate() {
        let due = start + FRAME_TIME * u32::try_from(n).unwrap_or(u32::MAX);
        tokio::select! {
            () = sleep_until(due) => {}
            _ = stopped.changed() => {
                end = FeedEnd::Cancelled;
                break;
            }
            () = queue.closed() => {
                end = FeedEnd::Cancelled;
                break;
            }
        }
        // As the capture callback does, the frame is lost while the gate is off.
        let (enabled, epoch) = shared.gate.state();
        if enabled {
            let _ = queue.send((epoch, generation, Queued::now(frame)));
        }
    }
    shared.feeding.send_replace(false);
    let _ = feed.end.send(end);
}

/// The microphone's frames, replaced by a clip's while one plays.
pub struct Fed<S> {
    microphone: S,
    played: mpsc::UnboundedReceiver<Played>,
    gate: InputGate,
    feeding: Arc<watch::Sender<bool>>,
    generation: watch::Receiver<u64>,
}

impl<S: FrameSource> FrameSource for Fed<S> {
    async fn next(&mut self) -> Result<Queued, AudioError> {
        loop {
            tokio::select! {
                biased;
                Some((epoch, generation, queued)) = self.played.recv() => {
                    if epoch == self.gate.state().1 && generation == *self.generation.borrow() {
                        return Ok(queued);
                    }
                }
                frame = self.microphone.next() => {
                    if frame.is_err() || !*self.feeding.borrow() {
                        return frame;
                    }
                }
            }
        }
    }

    fn fed(&self) -> bool {
        *self.feeding.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Duration = Duration::from_secs(60);

    /// A WAV file around `data`, with `extra` chunks before the data.
    fn wav(tag: u16, channels: u16, rate: u32, bits: u16, extra: &[u8], data: &[u8]) -> Vec<u8> {
        let mut file = b"RIFF\0\0\0\0WAVE".to_vec();
        file.extend(b"fmt ");
        file.extend(16_u32.to_le_bytes());
        for field in [tag, channels] {
            file.extend(field.to_le_bytes());
        }
        file.extend(rate.to_le_bytes());
        file.extend((rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
        file.extend((channels * bits / 8).to_le_bytes());
        file.extend(bits.to_le_bytes());
        file.extend(extra);
        file.extend(b"data");
        file.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        file.extend(data);
        file
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    fn mono(rate: u32, samples: &[i16]) -> Vec<u8> {
        wav(1, 1, rate, 16, &[], &pcm(samples))
    }

    #[test]
    fn a_wav_becomes_frames_converted_like_microphone_input() {
        let ramp: Vec<i16> = (0..3 * FRAME_SAMPLES)
            .map(|n| i16::try_from(n % 2_000).unwrap() - 1_000)
            .collect();
        let clip = |file: Vec<u8>| Clip::from_reader(file.as_slice(), MAX).unwrap().frames;

        let same = clip(mono(16_000, &ramp));
        assert_eq!(same.len(), 3);
        assert_eq!(same.concat(), ramp, "16 kHz mono passes through");

        // 48 kHz stereo: the channels are averaged, then every three samples.
        let loud: Vec<i16> = ramp.iter().flat_map(|&s| [s, s]).collect();
        let stereo = clip(wav(1, 2, 16_000, 16, &[], &pcm(&loud)));
        assert_eq!(stereo, same, "identical channels average to themselves");
        let three: Vec<i16> = ramp.iter().flat_map(|&s| [s, s, s]).collect();
        assert_eq!(
            clip(mono(48_000, &three)),
            same,
            "a constant third averages to itself"
        );

        // Chunks that no sample or frame boundary lines up with are skipped.
        let mut extra = b"LIST\x03\0\0\0abc\0".to_vec();
        extra.extend(b"junk\0\0\0\0");
        assert_eq!(clip(wav(1, 1, 16_000, 16, &extra, &pcm(&ramp))), same);
    }

    #[test]
    fn the_last_partial_frame_is_padded_with_silence() {
        let samples = vec![1_000_i16; FRAME_SAMPLES + 10];
        let frames = Clip::from_reader(mono(16_000, &samples).as_slice(), MAX)
            .unwrap()
            .frames;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1][..10], [1_000; 10]);
        assert!(frames[1][10..].iter().all(|&s| s == 0));
    }

    #[test]
    fn a_file_is_refused_with_the_reason_unless_it_is_16_bit_pcm_wav_within_the_limit() {
        let ok = pcm(&[0; 100]);
        let at_limit = pcm(&vec![5; 60 * 16_000]);
        let past_limit = pcm(&vec![5; 60 * 16_000 + 1]);
        let mut streamed = wav(1, 1, 16_000, 16, &[], &past_limit);
        let at = streamed.len() - past_limit.len() - 4;
        streamed[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut cut = wav(1, 1, 16_000, 16, &[], &ok);
        cut.truncate(30);
        let mut no_format = b"RIFF\0\0\0\0WAVEdata".to_vec();
        no_format.extend(4_u32.to_le_bytes());
        no_format.extend([0; 4]);
        let too_long = FeedError::TooLong(MAX);
        for (case, file, expected) in [
            ("at the limit", wav(1, 1, 16_000, 16, &[], &at_limit), None),
            (
                "a sample past it",
                wav(1, 1, 16_000, 16, &[], &past_limit),
                Some(too_long.clone()),
            ),
            ("past it with no length declared", streamed, Some(too_long)),
            (
                "not RIFF",
                b"OggS\0\0\0\0\0\0\0\0\0\0\0\0".to_vec(),
                Some(FeedError::NotWav),
            ),
            ("cut inside the header", cut, Some(FeedError::NotWav)),
            ("no fmt chunk", no_format, Some(FeedError::NotWav)),
            ("nothing", Vec::new(), Some(FeedError::NotWav)),
            (
                "8-bit",
                wav(1, 1, 16_000, 8, &[], &ok),
                Some(FeedError::Format),
            ),
            (
                "float",
                wav(3, 1, 16_000, 32, &[], &ok),
                Some(FeedError::Format),
            ),
            (
                "no channels",
                wav(1, 0, 16_000, 16, &[], &ok),
                Some(FeedError::Format),
            ),
            (
                "a rate too low",
                wav(1, 1, 4_000, 16, &[], &ok),
                Some(FeedError::Format),
            ),
            (
                "a rate too high",
                wav(1, 1, 384_000, 16, &[], &ok),
                Some(FeedError::Format),
            ),
            (
                "no samples",
                wav(1, 1, 16_000, 16, &[], &[]),
                Some(FeedError::Empty),
            ),
        ] {
            let result = Clip::from_reader(file.as_slice(), MAX);
            assert_eq!(result.err(), expected, "{case}");
        }
    }

    #[test]
    fn a_path_that_is_missing_or_not_a_file_says_so() {
        let dir = std::env::temp_dir().join(format!("thalvi-feed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.wav");
        assert_eq!(
            Clip::from_wav(&missing, MAX).err(),
            Some(FeedError::Missing(missing.display().to_string()))
        );
        assert!(matches!(
            Clip::from_wav(&dir, MAX),
            Err(FeedError::Unreadable(_))
        ));
        let good = dir.join("good.wav");
        std::fs::write(&good, mono(16_000, &[1; FRAME_SAMPLES])).unwrap();
        assert_eq!(Clip::from_wav(&good, MAX).unwrap().frames.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
