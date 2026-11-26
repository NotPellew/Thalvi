//! Spoken numbers into `numbers::format` and `writing::normalize`: lines are the text, a
//! language code, then the word list.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thalvi_desktop::phrases::WordList;
use thalvi_desktop::secondary::Language;
use thalvi_desktop::{numbers, writing};

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let (text, rest) = input.split_once('\n').unwrap_or((input, ""));
    let (code, words) = rest.split_once('\n').unwrap_or((rest, ""));
    let Ok(language) = Language::parse(code) else {
        return;
    };
    let Ok(words) = WordList::new(words.lines().map(str::to_owned).collect()) else {
        return;
    };
    let _ = numbers::format(text, language, words.phrases());
    let _ = writing::normalize(text);
});
