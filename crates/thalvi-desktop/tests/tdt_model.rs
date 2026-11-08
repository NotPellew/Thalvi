//! Opt-in local-model replays through the public speech stream, without downloads.

#![expect(clippy::expect_used, reason = "a test fails by panicking")]

use std::path::{Path, PathBuf};

use thalvi_desktop::audio::Framer;
use thalvi_desktop::frame::{FRAME_SAMPLES, Frame};
use thalvi_desktop::local::{SpeechModel, SpeechStream};
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::recognizer::RecognizerEvent;
use thalvi_desktop::transducer::{Transducer, TransducerFiles};

const ENGLISH: &str =
    "ask not what your country can do for you ask what you can do for your country";

fn words(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn wav_frames(path: &Path) -> Vec<Frame> {
    let wav = std::fs::read(path).expect("local WAV");
    let rate = u32::from_le_bytes(wav[24..28].try_into().expect("sample rate"));
    let mut offset = 12;
    while &wav[offset..offset + 4] != b"data" {
        let size = u32::from_le_bytes(
            wav[offset + 4..offset + 8]
                .try_into()
                .expect("chunk length"),
        );
        offset += 8
            + usize::try_from(size).expect("chunk fits")
            + usize::try_from(size % 2).expect("padding");
    }
    let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().expect("data length"));
    let mut down = Framer::new(rate);
    let mut frames = Vec::new();
    for pair in wav[offset + 8..offset + 8 + usize::try_from(size).expect("data fits")]
        .as_chunks::<2>()
        .0
    {
        down.push(
            f32::from(i16::from_le_bytes(*pair)) / 32768.0,
            &mut |frame| frames.push(*frame),
        );
    }
    frames
}

fn model() -> Transducer {
    let folder = PathBuf::from(std::env::var("THALVI_SPEECH_MODEL").expect("THALVI_SPEECH_MODEL"));
    Transducer::load(&TransducerFiles::in_folder(&folder), &WordList::default())
        .expect("model loads")
}

fn recognize(model: &Transducer, frames: &[Frame]) -> (Vec<String>, usize) {
    let mut stream = model.stream();
    let mut finals = Vec::new();
    let mut partials = 0;
    for frame in frames
        .iter()
        .chain(std::iter::repeat_n(&[0; FRAME_SAMPLES], 40))
    {
        for event in stream.accept(frame).expect("frame recognized") {
            match event {
                RecognizerEvent::Final(text) if !text.is_empty() => finals.push(words(&text)),
                RecognizerEvent::Partial(text) if !text.is_empty() => partials += 1,
                _ => {}
            }
        }
    }
    (finals, partials)
}

#[test]
#[ignore = "needs THALVI_SPEECH_MODEL and THALVI_TEST_WAVS local folders"]
fn english_is_live_and_finalizes_in_full() {
    let wavs = PathBuf::from(std::env::var("THALVI_TEST_WAVS").expect("THALVI_TEST_WAVS"));
    let (finals, partials) = recognize(&model(), &wav_frames(&wavs.join("en.wav")));
    assert_eq!(finals.join(" "), ENGLISH);
    assert!(partials > 0, "text appears before the final");
    println!("en.wav: 0/17 word errors; {partials} nonempty partials");
}

#[test]
#[ignore = "needs THALVI_SPEECH_MODEL and THALVI_TEST_WAVS local folders"]
fn the_multilingual_samples_are_recognized_in_full() {
    let wavs = PathBuf::from(std::env::var("THALVI_TEST_WAVS").expect("THALVI_TEST_WAVS"));
    let model = model();
    for (language, reference) in [
        ("en", ENGLISH),
        ("de", "alles hat ein ende nur die wurst hat zwei"),
        (
            "es",
            "no preguntes qué puede hacer tu país por ti pregunta qué puedes hacer tú por tu país",
        ),
    ] {
        let (finals, partials) =
            recognize(&model, &wav_frames(&wavs.join(format!("{language}.wav"))));
        assert_eq!(finals.join(" "), reference, "{language}");
        assert!(partials > 0, "{language} has live text");
        println!(
            "{language}.wav: 0/{} word errors; {partials} nonempty partials",
            reference.split_whitespace().count()
        );
    }
}

#[test]
#[ignore = "needs THALVI_SPEECH_MODEL and THALVI_TEST_WAVS local folders"]
fn quiet_english_keeps_all_its_words() {
    let wavs = PathBuf::from(std::env::var("THALVI_TEST_WAVS").expect("THALVI_TEST_WAVS"));
    let model = model();
    for divisor in [8, 16, 64] {
        let frames: Vec<Frame> = wav_frames(&wavs.join("en.wav"))
            .into_iter()
            .map(|frame| frame.map(|sample| sample / divisor))
            .collect();
        let (finals, _) = recognize(&model, &frames);
        assert_eq!(finals.join(" "), ENGLISH, "input divided by {divisor}");
    }
}

#[test]
#[ignore = "needs THALVI_SPEECH_MODEL and THALVI_TEST_WAVS local folders"]
fn english_survives_seven_seconds_of_digital_silence() {
    let wavs = PathBuf::from(std::env::var("THALVI_TEST_WAVS").expect("THALVI_TEST_WAVS"));
    let clip = wav_frames(&wavs.join("en.wav"));
    let mut frames = clip.clone();
    frames.extend(std::iter::repeat_n(
        [0; FRAME_SAMPLES],
        7 * 16_000 / FRAME_SAMPLES,
    ));
    frames.extend(clip);
    let (finals, _) = recognize(&model(), &frames);
    assert_eq!(finals.join(" "), format!("{ENGLISH} {ENGLISH}"));
    println!("en.wav + 7 s digital silence + en.wav: both copies complete");
}
