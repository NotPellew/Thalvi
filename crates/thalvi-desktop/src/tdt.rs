//! Duration-aware frame traversal over the shared transducer greedy search.

use crate::boosting::{Joiner, PhraseTree, TreeState, greedy};
use crate::recognizer::RecognizerError;

/// The TDT joiner's last five logits represent skips of zero through four frames.
pub(crate) const DURATIONS: usize = 5;

struct DurationJoiner<J> {
    inner: J,
    blank: usize,
    next: usize,
}

pub(crate) fn tdt_greedy<J: Joiner>(
    joiner: J,
    tree: &PhraseTree,
    state: &mut TreeState,
    blank: usize,
    frames: usize,
    skip: &mut usize,
) -> Result<(J, Vec<Vec<usize>>), RecognizerError> {
    let mut duration = DurationJoiner {
        inner: joiner,
        blank,
        next: *skip,
    };
    let emitted = greedy(&mut duration, tree, state, frames)?;
    *skip = duration.next.saturating_sub(frames);
    Ok((duration.inner, emitted))
}

fn best(logits: &[f32]) -> usize {
    logits
        .iter()
        .enumerate()
        .fold((0, f32::NEG_INFINITY), |best, (id, value)| {
            if *value > best.1 { (id, *value) } else { best }
        })
        .0
}

impl<J: Joiner> Joiner for DurationJoiner<J> {
    fn logits(&mut self, frame: usize) -> Result<Vec<f32>, RecognizerError> {
        if frame < self.next {
            let mut logits = vec![f32::NEG_INFINITY; self.blank + 1];
            logits[self.blank] = 0.0;
            return Ok(logits);
        }
        let mut logits = self.inner.logits(frame)?;
        if logits.len() != self.blank + 1 + DURATIONS {
            return Err(RecognizerError::said(
                "The speech model returned invalid TDT scores.",
            ));
        }
        let duration = best(&logits[self.blank + 1..]);
        logits.truncate(self.blank + 1);
        // Greedy's blank and symbol-cap exits advance a frame even when the duration is zero.
        self.next = frame + duration;
        Ok(logits)
    }

    fn advance(&mut self, token: usize) -> Result<(), RecognizerError> {
        self.inner.advance(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phrases::WordList;

    struct Script {
        rows: std::collections::VecDeque<(usize, usize, usize)>,
        calls: Vec<usize>,
        tokens: Vec<usize>,
    }

    impl Joiner for Script {
        fn logits(&mut self, frame: usize) -> Result<Vec<f32>, RecognizerError> {
            self.calls.push(frame);
            let (at, token, duration) = self.rows.pop_front().expect("script row");
            assert_eq!(frame, at, "only the duration's destination is decoded");
            let mut row = vec![-10.0; 3 + DURATIONS];
            row[token] = 10.0;
            row[3 + duration] = 10.0;
            Ok(row)
        }

        fn advance(&mut self, token: usize) -> Result<(), RecognizerError> {
            self.tokens.push(token);
            Ok(())
        }
    }

    fn check(rows: Vec<(usize, usize, usize)>, expected: &[Vec<usize>]) {
        let tree = PhraseTree::new(&WordList::default(), &["▁one".into(), "▁two".into()]);
        let mut joiner = DurationJoiner {
            inner: Script {
                rows: rows.into(),
                calls: Vec::new(),
                tokens: Vec::new(),
            },
            blank: 2,
            next: 0,
        };
        let emitted = greedy(
            &mut joiner,
            &tree,
            &mut TreeState::default(),
            expected.len(),
        )
        .unwrap();
        assert_eq!(
            emitted, expected,
            "tokens stay on their duration-selected frames"
        );
        assert_eq!(
            joiner.inner.tokens,
            expected.concat(),
            "only emitted tokens update the predictor"
        );
        assert!(joiner.inner.rows.is_empty());
    }

    #[test]
    fn tokens_and_durations_advance_encoder_frames() {
        for (rows, expected) in [
            (
                vec![(0, 0, 2), (2, 2, 3), (5, 1, 1)],
                vec![vec![0], vec![], vec![], vec![], vec![], vec![1]],
            ),
            (vec![(0, 0, 0), (0, 1, 0), (0, 2, 1)], vec![vec![0, 1]]),
        ] {
            check(rows, &expected);
        }
    }

    #[test]
    fn a_zero_duration_blank_still_advances_one_frame() {
        for duration in [0, 1] {
            check(vec![(0, 2, duration), (1, 0, 1)], &[vec![], vec![0]]);
        }
    }

    #[test]
    fn the_symbol_cap_forces_progress_on_zero_duration_tokens() {
        for bursts in [1, 2] {
            let rows = (0..bursts)
                .flat_map(|frame| std::iter::repeat_n((frame, 0, 0), 10))
                .chain([(bursts, 1, 1)])
                .collect();
            let mut expected = vec![vec![0; 10]; bursts];
            expected.push(vec![1]);
            check(rows, &expected);
        }
    }

    #[test]
    fn duration_skips_survive_replay_boundaries() {
        let tree = PhraseTree::new(&WordList::default(), &["▁one".into(), "▁two".into()]);
        for duration in [3, 4] {
            let mut joiner = Script {
                rows: vec![(0, 0, duration), (duration - 2, 1, 1)].into(),
                calls: Vec::new(),
                tokens: Vec::new(),
            };
            let mut skip = 0;
            let mut emitted = Vec::new();
            for frames in [1, 1, duration - 1] {
                let (next, tokens) = tdt_greedy(
                    joiner,
                    &tree,
                    &mut TreeState::default(),
                    2,
                    frames,
                    &mut skip,
                )
                .unwrap();
                joiner = next;
                emitted.extend(tokens);
            }
            let mut expected = vec![Vec::new(); duration + 1];
            expected[0].push(0);
            expected[duration].push(1);
            assert_eq!(
                emitted, expected,
                "a duration that crosses windows is consumed once"
            );
            assert_eq!(joiner.tokens, [0, 1]);
            assert_eq!(skip, 0);
        }
    }

    struct Scores(Vec<f32>);

    impl Joiner for Scores {
        fn logits(&mut self, _frame: usize) -> Result<Vec<f32>, RecognizerError> {
            Ok(self.0.clone())
        }

        fn advance(&mut self, _token: usize) -> Result<(), RecognizerError> {
            Ok(())
        }
    }

    #[test]
    fn tdt_scores_must_have_both_the_vocabulary_and_five_durations() {
        for count in [7, 8, 9, 0] {
            let mut joiner = DurationJoiner {
                inner: Scores(vec![0.0; count]),
                blank: 2,
                next: 0,
            };
            assert_eq!(joiner.logits(0).is_ok(), count == 8, "{count} scores");
        }
    }
}
