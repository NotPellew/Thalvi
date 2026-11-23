//! Formats the number words of a delivered dictation as digits, without a model call.

use std::collections::BTreeSet;

use crate::secondary::Language;

/// A text with its number words rewritten, and how many numbers were rewritten. The text is
/// never recorded, only the count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formatted {
    pub text: String,
    pub rewritten: usize,
}

/// Which number rules apply to `text`: the recognizer's or entry's language, or English for
/// plain ASCII text whose language is unknown. Anything else is left as it was.
#[derive(Clone, Copy)]
enum Tongue {
    English,
    German,
    Other,
}

/// One word of the text, lower-case, with its span.
struct Word {
    lower: String,
    start: usize,
    end: usize,
}

const ENGLISH_UNITS: [(&str, u64); 10] = [
    ("zero", 0),
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
];

const ENGLISH_TEENS: [(&str, u64); 10] = [
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
    ("thirteen", 13),
    ("fourteen", 14),
    ("fifteen", 15),
    ("sixteen", 16),
    ("seventeen", 17),
    ("eighteen", 18),
    ("nineteen", 19),
];

const ENGLISH_TENS: [(&str, u64); 8] = [
    ("twenty", 20),
    ("thirty", 30),
    ("forty", 40),
    ("fifty", 50),
    ("sixty", 60),
    ("seventy", 70),
    ("eighty", 80),
    ("ninety", 90),
];

/// `text` with the number words in `language` written as digits, leaving every token of `words`
/// (the word list) as it was said. `None` formats only plain English: ASCII text.
#[must_use]
pub fn format(text: &str, language: Option<Language>, words: &[String]) -> Formatted {
    let tongue = match language.map(Language::code) {
        Some("en") => Tongue::English,
        Some("de") => Tongue::German,
        None if text.is_ascii() => Tongue::English,
        Some(_) | None => Tongue::Other,
    };
    if matches!(tongue, Tongue::Other) {
        return Formatted {
            text: text.to_owned(),
            rewritten: 0,
        };
    }
    let protected = protected_words(words);
    let tokens = tokenize(text);
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut rewritten = 0;
    let mut at = 0;
    while at < tokens.len() {
        match replacement(text, &tokens, at, tongue, &protected) {
            Some((end, digits)) => {
                out.push_str(&text[copied..tokens[at].start]);
                out.push_str(&digits);
                copied = tokens[end - 1].end;
                rewritten += 1;
                at = end;
            }
            None => at += 1,
        }
    }
    out.push_str(&text[copied..]);
    Formatted {
        text: out,
        rewritten,
    }
}

/// The word list's words, whose tokens a rewrite never changes.
fn protected_words(words: &[String]) -> BTreeSet<String> {
    words
        .iter()
        .flat_map(|entry| entry.split(|c: char| !c.is_alphanumeric()))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Every run of letters in `text`, with its span.
fn tokenize(text: &str) -> Vec<Word> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        if character.is_alphabetic() {
            start.get_or_insert(offset);
        } else if let Some(begin) = start.take() {
            tokens.push(Word {
                lower: text[begin..offset].to_lowercase(),
                start: begin,
                end: offset,
            });
        }
    }
    if let Some(begin) = start {
        tokens.push(Word {
            lower: text[begin..].to_lowercase(),
            start: begin,
            end: text.len(),
        });
    }
    tokens
}

/// The digits for the number words starting at `at`, and the token after them. A token that is
/// part of a hyphenated word or of an alphanumeric token is never rewritten.
fn replacement(
    text: &str,
    tokens: &[Word],
    at: usize,
    tongue: Tongue,
    protected: &BTreeSet<String>,
) -> Option<(usize, String)> {
    let first = tokens.get(at)?;
    if !stands_alone(text, first) || protected.contains(&first.lower) {
        return None;
    }
    let previous = at.checked_sub(1).map(|index| tokens[index].lower.as_str());
    let before = at.checked_sub(2).map(|index| tokens[index].lower.as_str());
    let next = tokens.get(at + 1).map(|word| word.lower.as_str());
    if kept_in_context(tongue, previous, before, next) {
        return None;
    }
    let mut end = at + 1;
    while let Some(next) = tokens.get(end)
        && whitespace_between(text, &tokens[end - 1], next)
        && !protected.contains(&next.lower)
    {
        end += 1;
    }
    let lower: Vec<&str> = tokens[at..end]
        .iter()
        .map(|word| word.lower.as_str())
        .collect();
    let (used, digits) = match tongue {
        Tongue::English => english_replacement(&lower)?,
        Tongue::German => german_replacement(&lower)?,
        Tongue::Other => return None,
    };
    let last = at + used.checked_sub(1)?;
    if used == 0 || !stands_alone(text, &tokens[last]) {
        return None;
    }
    if tokens[at..=last]
        .iter()
        .any(|word| protected.contains(&word.lower))
    {
        return None;
    }
    Some((last + 1, digits))
}

/// Whether a word is not glued to a number or another word by a hyphen or a letter, so
/// "twenty-two" and "Cybrex2" are left alone rather than half rewritten.
fn stands_alone(text: &str, word: &Word) -> bool {
    let glued = |character: Option<char>| {
        character.is_some_and(|character| character.is_alphanumeric() || character == '-')
    };
    !glued(text[..word.start].chars().next_back()) && !glued(text[word.end..].chars().next())
}

fn whitespace_between(text: &str, before: &Word, after: &Word) -> bool {
    text[before.end..after.start]
        .chars()
        .all(char::is_whitespace)
}

/// Whether the number word after `previous` belongs to an idiom where it stays a word:
/// "half past three", "quarter to three", "halb drei", "Viertel nach drei", "gegen fünf Uhr".
fn kept_in_context(
    tongue: Tongue,
    previous: Option<&str>,
    before: Option<&str>,
    next: Option<&str>,
) -> bool {
    match tongue {
        Tongue::English => {
            matches!(previous, Some("past" | "to")) && matches!(before, Some("half" | "quarter"))
        }
        Tongue::German => {
            matches!(previous, Some("halb"))
                || (matches!(previous, Some("nach" | "vor" | "gegen"))
                    && next.is_none_or(|next| next == "uhr"))
        }
        Tongue::Other => false,
    }
}

fn english_unit(word: &str) -> Option<u64> {
    ENGLISH_UNITS
        .iter()
        .find_map(|(known, value)| (*known == word).then_some(*value))
}

fn english_teen(word: &str) -> Option<u64> {
    ENGLISH_TEENS
        .iter()
        .find_map(|(known, value)| (*known == word).then_some(*value))
}

fn english_ten(word: &str) -> Option<u64> {
    ENGLISH_TENS
        .iter()
        .find_map(|(known, value)| (*known == word).then_some(*value))
}

fn english_scale(word: &str) -> Option<u64> {
    match word {
        "thousand" => Some(1_000),
        "million" => Some(1_000_000),
        "billion" => Some(1_000_000_000),
        _ => None,
    }
}

/// The value of the number words at the start of `words`, and how many words it used.
fn english_under_100(words: &[&str]) -> Option<(u64, usize)> {
    let first = *words.first()?;
    if let Some(value) = english_teen(first) {
        return Some((value, 1));
    }
    if let Some(ten) = english_ten(first) {
        if let Some(unit) = words.get(1).and_then(|word| english_unit(word))
            && (1..=9).contains(&unit)
        {
            return Some((ten + unit, 2));
        }
        return Some((ten, 1));
    }
    english_unit(first).map(|unit| (unit, 1))
}

fn english_under_1000(words: &[&str]) -> Option<(u64, usize)> {
    let first = *words.first()?;
    let multiplier = if first == "a" {
        Some(1)
    } else {
        english_unit(first)
    };
    if let Some(multiplier) = multiplier
        && words.get(1) == Some(&"hundred")
    {
        let mut value = multiplier * 100;
        let mut at = 2;
        if words.get(at) == Some(&"and") {
            at += 1;
        }
        if at < words.len() {
            let (rest, used) = english_under_100(&words[at..])?;
            value += rest;
            at += used;
        }
        return Some((value, at));
    }
    english_under_100(words)
}

/// The value of the whole number phrase, or `None` when it is not one.
fn english_value(words: &[&str]) -> Option<u64> {
    let mut total = 0;
    let mut at = 0;
    let mut last_scale = None;
    while at < words.len() {
        let (value, used) = english_under_1000(&words[at..])?;
        at += used;
        match words.get(at).and_then(|word| english_scale(word)) {
            Some(scale) => {
                if last_scale.is_some_and(|last| scale >= last) || value > 999 {
                    return None;
                }
                total += value.checked_mul(scale)?;
                last_scale = Some(scale);
                at += 1;
            }
            None if at == words.len() => return total.checked_add(value),
            None => return None,
        }
    }
    (at > 0).then_some(total)
}

/// The longest number phrase at the start of `words`.
fn english_prefix(words: &[&str]) -> Option<(u64, usize)> {
    let mut best = None;
    for end in 1..=words.len() {
        if let Some(value) = english_value(&words[..end]) {
            best = Some((value, end));
        }
    }
    best
}

/// The minute of a clock time: a teen or a ten, optionally with a unit, or "oh" and a unit.
fn english_minute(words: &[&str]) -> Option<(u64, usize)> {
    if words.first() == Some(&"oh") {
        let unit = english_unit(words.get(1)?)?;
        return (unit < 10).then_some((unit, 2));
    }
    let first = *words.first()?;
    if english_unit(first).is_some() {
        return None;
    }
    let (value, used) = english_under_100(words)?;
    (value <= 59).then_some((value, used))
}

/// The two-digit value of a year's second part: 10 to 99, never a lone unit.
fn english_two_digit(words: &[&str]) -> Option<(u64, usize)> {
    let first = *words.first()?;
    if english_unit(first).is_some() {
        return None;
    }
    let (value, used) = english_under_100(words)?;
    (10..=99).contains(&value).then_some((value, used))
}

fn english_decade(word: &str) -> Option<u64> {
    let value = english_under_100(&[word])?.0;
    (10..=20).contains(&value).then_some(value)
}

fn english_time(words: &[&str]) -> Option<(usize, String)> {
    let hour = english_under_100(&[words.first()?])?.0;
    if !(1..=12).contains(&hour) {
        return None;
    }
    let (minute, used) = english_minute(&words[1..])?;
    Some((1 + used, format!("{hour}:{minute:02}")))
}

fn english_replacement(words: &[&str]) -> Option<(usize, String)> {
    let first = *words.first()?;
    if first == "one" && !english_one_is_a_count(words.get(1).copied()) {
        return None;
    }
    if let Some(time) = english_time(words) {
        return Some(time);
    }
    if let Some((value, used)) = english_prefix(words) {
        if words.get(used) == Some(&"percent") {
            return Some((used + 1, format!("{}%", grouped(value, ','))));
        }
        if words.get(used) == Some(&"point") {
            let fraction = digit_run(&words[used + 1..], english_unit)?;
            return Some((
                used + 1 + fraction.1,
                format!("{}.{}", grouped(value, ','), fraction.0),
            ));
        }
        if let Some(decade) = english_decade(first)
            && let Some((second, rest)) = english_two_digit(&words[1..])
            && !english_number_word(words.get(1 + rest).copied())
        {
            return Some((1 + rest, (decade * 100 + second).to_string()));
        }
        if let Some((digits, used)) = digit_run(words, english_unit)
            && used >= 2
        {
            return Some((used, digits));
        }
        return Some((used, grouped(value, ',')));
    }
    None
}

/// Whether a lone "one" counts something ("one drive") rather than standing for a pronoun or a
/// determiner ("the other one", "no one", "one of them", "one day").
fn english_one_is_a_count(next: Option<&str>) -> bool {
    next.is_some_and(|next| !matches!(next, "of" | "day"))
}

fn english_number_word(word: Option<&str>) -> bool {
    word.is_some_and(|word| {
        english_unit(word).is_some()
            || english_teen(word).is_some()
            || english_ten(word).is_some()
            || english_scale(word).is_some()
            || matches!(word, "hundred" | "and" | "a" | "point")
    })
}

const GERMAN_WORDS: [(&str, u64); 39] = [
    ("null", 0),
    ("eins", 1),
    ("ein", 1),
    ("eine", 1),
    ("einer", 1),
    ("eines", 1),
    ("einem", 1),
    ("zwei", 2),
    ("drei", 3),
    ("vier", 4),
    ("fünf", 5),
    ("sechs", 6),
    ("sieben", 7),
    ("acht", 8),
    ("neun", 9),
    ("zehn", 10),
    ("elf", 11),
    ("zwölf", 12),
    ("dreizehn", 13),
    ("vierzehn", 14),
    ("fünfzehn", 15),
    ("sechzehn", 16),
    ("siebzehn", 17),
    ("achtzehn", 18),
    ("neunzehn", 19),
    ("zwanzig", 20),
    ("dreißig", 30),
    ("vierzig", 40),
    ("fünfzig", 50),
    ("sechzig", 60),
    ("siebzig", 70),
    ("achtzig", 80),
    ("neunzig", 90),
    ("hundert", 100),
    ("tausend", 1000),
    ("million", 1_000_000),
    ("millionen", 1_000_000),
    ("milliarde", 1_000_000_000),
    ("milliarden", 1_000_000_000),
];

fn german_word(word: &str) -> Option<u64> {
    GERMAN_WORDS
        .iter()
        .find_map(|(known, value)| (*known == word).then_some(*value))
}

fn german_below_100(word: &str) -> Option<u64> {
    if let Some(value) = german_word(word) {
        return Some(value);
    }
    let (unit, ten) = word.split_once("und")?;
    let unit = german_word(unit)?;
    let ten = german_word(ten)?;
    (unit < 10 && (20..=90).contains(&ten) && ten % 10 == 0).then_some(unit + ten)
}

fn german_below_1000(word: &str) -> Option<u64> {
    if let Some((hundreds, rest)) = word.split_once("hundert") {
        let hundreds = if hundreds.is_empty() {
            1
        } else {
            german_below_100(hundreds).filter(|value| *value < 10)?
        };
        let rest = if rest.is_empty() {
            0
        } else {
            german_below_100(rest)?
        };
        return hundreds.checked_mul(100)?.checked_add(rest);
    }
    german_below_100(word)
}

fn german_value(word: &str) -> Option<u64> {
    if let Some(value) = german_word(word).filter(|value| *value >= 1000) {
        return Some(value);
    }
    if let Some((left, right)) = word.split_once("tausend") {
        let left = if left.is_empty() {
            1
        } else {
            german_below_1000(left)?
        };
        let right = if right.is_empty() {
            0
        } else {
            german_below_1000(right)?
        };
        return left.checked_mul(1000)?.checked_add(right);
    }
    german_below_1000(word)
}

fn german_minute(word: &str) -> Option<u64> {
    let value = german_value(word)?;
    (value <= 59).then_some(value)
}

fn german_replacement(words: &[&str]) -> Option<(usize, String)> {
    let first = *words.first()?;
    if let Some((digits, used)) = digit_run(words, german_unit)
        && used >= 2
    {
        return Some((used, digits));
    }
    if german_keeps_word(first) {
        return None;
    }
    if let [hour, "uhr", minute, ..] = words
        && let Some(hour) = german_value(hour)
        && (1..=12).contains(&hour)
        && let Some(minute) = german_minute(minute)
    {
        return Some((3, format!("{hour}:{minute:02}")));
    }
    if let [number, "prozent", ..] = words
        && let Some(value) = german_value(number)
    {
        return Some((2, format!("{} %", grouped(value, '.'))));
    }
    if let [number, "komma", rest @ ..] = words
        && let Some(value) = german_value(number)
    {
        let (fraction, used) = digit_run(rest, german_unit)?;
        return Some((2 + used, format!("{},{}", grouped(value, '.'), fraction)));
    }
    german_value(first).map(|value| (1, grouped(value, '.')))
}

/// The standalone German words kept as words: articles, pronouns, ordinals and fractions,
/// whose spellings are not number compounds this rule knows.
fn german_keeps_word(word: &str) -> bool {
    matches!(word, "ein" | "eine" | "einer" | "eines" | "einem" | "eins")
}

fn german_unit(word: &str) -> Option<u64> {
    german_word(word).filter(|value| *value < 10)
}

/// A run of single digits said as words: the digits, and how many words they used.
fn digit_run(words: &[&str], value_of: impl Fn(&str) -> Option<u64>) -> Option<(String, usize)> {
    let mut digits = String::new();
    for word in words {
        let Some(value) = value_of(word).filter(|value| *value < 10) else {
            break;
        };
        digits.push(char::from_digit(u32::try_from(value).ok()?, 10)?);
    }
    let used = digits.len();
    (!digits.is_empty()).then_some((digits, used))
}

/// `value` with thousands separators once it has five digits.
fn grouped(value: u64, separator: char) -> String {
    let digits = value.to_string();
    if value < 10_000 {
        return digits;
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language(code: &str) -> Option<Language> {
        Language::parse(code).unwrap()
    }

    fn in_language(text: &str, code: &str) -> String {
        format(text, language(code), &[]).text
    }

    fn english(text: &str) -> String {
        in_language(text, "en")
    }

    fn german(text: &str) -> String {
        in_language(text, "de")
    }

    /// The English rule table: (said, delivered). Cases that do not change say what stays.
    fn english_cases() -> Vec<(&'static str, &'static str)> {
        vec![
            ("I have fifteen open issues.", "I have 15 open issues."),
            ("buy two terabyte drives", "buy 2 terabyte drives"),
            ("meet at three thirty", "meet at 3:30"),
            ("meet at three thirty five", "meet at 3:35"),
            ("meet at three oh five", "meet at 3:05"),
            ("version two point five", "version 2.5"),
            ("twenty twenty six", "2026"),
            ("nineteen ninety", "1990"),
            ("twenty twenty", "2020"),
            ("twenty six", "26"),
            ("a hundred and five", "105"),
            ("one hundred", "100"),
            ("one thousand two hundred", "1200"),
            ("twelve thousand five hundred", "12,500"),
            ("one two three four", "1234"),
            ("fifteen percent", "15%"),
            ("fifteen percent, then stop", "15%, then stop"),
            ("five pm", "5 pm"),
            ("zero", "0"),
            ("two drives", "2 drives"),
            ("one drive", "1 drive"),
            ("text (two) and [three].", "text (2) and [3]."),
            // Kept as words: pronouns, idioms and ordinals.
            ("the other one", "the other one"),
            ("one of them", "one of them"),
            ("no one", "no one"),
            ("someone", "someone"),
            ("one day", "one day"),
            ("I have one", "I have one"),
            ("the second one", "the second one"),
            ("at once", "at once"),
            ("a couple", "a couple"),
            ("first of all", "first of all"),
            ("the fifteenth", "the fifteenth"),
            ("half past three", "half past three"),
            ("quarter to three", "quarter to three"),
            // Narrow: a hyphenated number word is left alone rather than half rewritten.
            ("twenty-two", "twenty-two"),
        ]
    }

    /// The German rule table: (said, delivered).
    fn german_cases() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "Ich habe fünfzehn offene Tickets.",
                "Ich habe 15 offene Tickets.",
            ),
            ("zwei Komma fünf", "2,5"),
            ("drei Uhr dreißig", "3:30"),
            ("zweitausendsechsundzwanzig", "2026"),
            ("fünfzehn Prozent", "15 %"),
            ("zwei Laufwerke", "2 Laufwerke"),
            ("dreihundertfünfzig", "350"),
            ("einundzwanzig", "21"),
            ("eintausend", "1000"),
            ("zwölftausendfünfhundert", "12.500"),
            ("eins zwei drei", "123"),
            // Kept as words: pronouns, articles, ordinals and fractions.
            ("ein Laufwerk", "ein Laufwerk"),
            ("einer von ihnen", "einer von ihnen"),
            ("eine Frage", "eine Frage"),
            ("erste", "erste"),
            ("fünfzehnte", "fünfzehnte"),
            ("zweieinhalb", "zweieinhalb"),
            ("halb drei", "halb drei"),
            ("Viertel nach drei", "Viertel nach drei"),
            ("gegen fünf Uhr", "gegen fünf Uhr"),
            ("vor fünfzehn Minuten", "vor 15 Minuten"),
            ("nach fünf Stunden", "nach 5 Stunden"),
        ]
    }

    #[test]
    fn english_number_words_become_digits_as_the_rules_say() {
        for (said, expected) in english_cases() {
            assert_eq!(english(said), expected, "{said}");
        }
    }

    #[test]
    fn german_number_words_become_digits_as_the_rules_say() {
        for (said, expected) in german_cases() {
            assert_eq!(german(said), expected, "{said}");
        }
    }

    #[test]
    fn text_in_another_language_is_left_alone_unless_unknown_text_is_plain_english() {
        let unchanged = [("dos tres", "es"), ("dos tres", "fr"), ("fünfzehn", "es")];
        for (said, code) in unchanged {
            assert_eq!(in_language(said, code), said, "{said} in {code}");
        }
        // No hint: ASCII only is English; anything else could be another language.
        assert_eq!(format("dos tres", None, &[]).text, "dos tres");
        assert_eq!(format("I have fifteen", None, &[]).text, "I have 15");
        assert_eq!(
            format("Año dos mil veintiséis", None, &[]).text,
            "Año dos mil veintiséis"
        );
    }

    #[test]
    fn a_word_list_token_is_never_changed() {
        let words = ["Cybrex2".to_owned(), "four terabyte".to_owned()];
        assert_eq!(
            format("Cybrex2 two drives", language("en"), &words).text,
            "Cybrex2 2 drives"
        );
        assert_eq!(
            format("four terabyte two", language("en"), &words).text,
            "four terabyte 2"
        );
        assert_eq!(format("three four", language("en"), &words).text, "3 four");
    }

    #[test]
    fn formatting_a_formatted_text_changes_nothing_and_counts_only_what_it_rewrites() {
        let counted = format("fifteen percent and two drives", language("en"), &[]);
        assert_eq!(counted.text, "15% and 2 drives");
        assert_eq!(counted.rewritten, 2);
        for (_, formatted) in english_cases() {
            let again = format(formatted, language("en"), &[]);
            assert_eq!(again.text, formatted, "{formatted}");
            assert_eq!(again.rewritten, 0, "{formatted}");
        }
        for (_, formatted) in german_cases() {
            let again = format(formatted, language("de"), &[]);
            assert_eq!(again.text, formatted, "{formatted}");
            assert_eq!(again.rewritten, 0, "{formatted}");
        }
    }
}
