//! Owns the limits of merging the texts of several recognizers into one, and the check that the merged text invents no word.

use std::{collections::HashSet, time::Duration};

use crate::writing::normalize;

/// How long, from the end of a capture, the extras and the merge call may take in all.
pub const LIMIT: Duration = Duration::from_secs(8);

/// How long of [`LIMIT`] the extras are waited for; the rest is for the merge call.
pub const EXTRAS_WAIT: Duration = Duration::from_secs(5);

/// Whether every word of `merged`, ignoring case and punctuation, appears in one of `inputs`.
#[must_use]
pub fn faithful(inputs: &[String], merged: &str) -> bool {
    let heard: HashSet<String> = inputs
        .iter()
        .flat_map(|input| {
            normalize(input)
                .split(' ')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    normalize(merged)
        .split(' ')
        .all(|word| word.is_empty() || heard.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merged_word_no_input_has_makes_the_merge_unfaithful() {
        let inputs = [
            "Call Cybrex two, now.".to_owned(),
            "call cybrex2 now".to_owned(),
        ];
        for (merged, expected) in [
            ("Call Cybrex2 now!", true),
            ("call cybrex two", true),
            ("now call", true),
            ("Call Cyber now", false),
            ("Please call Cybrex2 now", false),
            ("", true),
        ] {
            assert_eq!(faithful(&inputs, merged), expected, "{merged:?}");
        }
    }
}
