//! Owns the phrase tree built from the word list and the greedy transducer search that scores with it.

use std::collections::{HashMap, VecDeque};

use crate::phrases::WordList;
use crate::recognizer::RecognizerError;

/// The most tokens one encoder frame may emit.
const MAX_TOKENS_PER_FRAME: usize = 10;
/// Bonus of a phrase's first token; later tokens add [`DEPTH_SCALING`] times this.
const CONTEXT_SCORE: f32 = 1.0;
const DEPTH_SCALING: f32 = 2.0;
/// Weight of the tree's scores against the model's log-probabilities; at `NeMo`'s 1.0 no "Cybrex2" came out.
const ALPHA: f32 = 2.0;
/// Marks the start of a word in the model's pieces.
const WORD_START: char = '\u{2581}';

const ROOT: usize = 0;

struct Node {
    /// Children by token.
    next: Vec<(usize, usize)>,
    /// The bonus of the arc into this node.
    arc: f32,
    /// The bonus accumulated along the path to this node.
    score: f32,
    /// The node of the longest proper suffix that is also a path in the tree.
    fail: usize,
    /// Added when the match is abandoned: takes back what the path earned beyond its suffix.
    backoff: f32,
}

/// Where the text spoken so far stands in the tree: the longest listed prefix it ends with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeState(usize);

/// The listed phrases as paths of model tokens, each token scored by how deep into its phrase it is.
pub struct PhraseTree {
    nodes: Vec<Node>,
}

/// Splits `text` into the longest pieces that match, `None` when some character has no piece.
fn tokenize(text: &str, pieces: &HashMap<&str, usize>, longest: usize) -> Option<Vec<usize>> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0;
    let mut tokens = Vec::new();
    while at < chars.len() {
        let end = chars.len().min(at + longest);
        let (token, len) = (1..=end - at).rev().find_map(|len| {
            let piece: String = chars[at..at + len].iter().collect();
            pieces.get(piece.as_str()).map(|token| (*token, len))
        })?;
        tokens.push(token);
        at += len;
    }
    Some(tokens)
}

impl PhraseTree {
    /// The tree of `phrases` in terms of `pieces`, whose index is the token. A phrase with a
    /// character that no piece covers cannot be heard as listed and is left out.
    #[must_use]
    pub fn new(phrases: &WordList, pieces: &[String]) -> Self {
        let mut ids = HashMap::new();
        for (token, piece) in pieces.iter().enumerate() {
            if !piece.is_empty() {
                ids.entry(piece.as_str()).or_insert(token);
            }
        }
        let longest = pieces.iter().map(|p| p.chars().count()).max().unwrap_or(0);
        let mut nodes = vec![Node {
            next: Vec::new(),
            arc: 0.0,
            score: 0.0,
            fail: ROOT,
            backoff: 0.0,
        }];
        let mut ends = vec![false];
        for phrase in phrases.phrases() {
            let mut text = String::new();
            for word in phrase.split_whitespace() {
                text.push(WORD_START);
                text.push_str(word);
            }
            let Some(tokens) = tokenize(&text, &ids, longest) else {
                continue;
            };
            let mut at = ROOT;
            for (token, depth) in tokens.into_iter().zip(0_u8..) {
                at = if let Some((_, child)) = nodes[at].next.iter().find(|(t, _)| *t == token) {
                    *child
                } else {
                    let arc = ALPHA
                        * if depth == 0 {
                            CONTEXT_SCORE
                        } else {
                            CONTEXT_SCORE * DEPTH_SCALING + f32::from(depth + 1).ln()
                        };
                    nodes.push(Node {
                        next: Vec::new(),
                        arc,
                        score: nodes[at].score + arc,
                        fail: ROOT,
                        backoff: 0.0,
                    });
                    ends.push(false);
                    let child = nodes.len() - 1;
                    nodes[at].next.push((token, child));
                    child
                };
            }
            ends[at] = true;
        }
        let mut tree = Self { nodes };
        tree.link(&ends);
        tree
    }

    /// Fills the fail links breadth first, and the backoffs they imply; a finished phrase takes
    /// nothing back.
    fn link(&mut self, ends: &[bool]) {
        let mut queue = VecDeque::from([ROOT]);
        while let Some(at) = queue.pop_front() {
            for (token, child) in self.nodes[at].next.clone() {
                let mut fail = ROOT;
                if at != ROOT {
                    let mut up = self.nodes[at].fail;
                    fail = loop {
                        if let Some((_, found)) =
                            self.nodes[up].next.iter().find(|(t, _)| *t == token)
                        {
                            break *found;
                        }
                        if up == ROOT {
                            break ROOT;
                        }
                        up = self.nodes[up].fail;
                    };
                }
                self.nodes[child].fail = fail;
                if !ends[child] {
                    self.nodes[child].backoff = self.nodes[fail].score - self.nodes[child].score;
                }
                queue.push_back(child);
            }
        }
    }

    /// The tree's score for every token at `state`: what any token without an arc gets, and the
    /// tokens that have one with the state each leads to. Leaving a partial match costs what it
    /// had earned, and the shorter match left over may still go on.
    fn scores(&self, state: TreeState) -> (f32, Vec<(usize, f32, TreeState)>) {
        let mut listed: Vec<(usize, f32, TreeState)> = Vec::new();
        let (mut at, mut back) = (state.0, 0.0);
        loop {
            for (token, child) in &self.nodes[at].next {
                if listed.iter().all(|(t, ..)| t != token) {
                    listed.push((*token, back + self.nodes[*child].arc, TreeState(*child)));
                }
            }
            if at == ROOT {
                return (back, listed);
            }
            back += self.nodes[at].backoff;
            at = self.nodes[at].fail;
        }
    }

    /// The non-blank token with the best model score plus the tree's, and the state it leads to.
    /// The joiner's log-softmax normalizer is the same for every token, so the raw logits rank them alike.
    fn pick(&self, state: TreeState, logits: &[f32], blank: usize) -> (usize, TreeState) {
        let (rest, listed) = self.scores(state);
        let (mut best, mut score) = (0, f32::NEG_INFINITY);
        for (token, logit) in logits[..blank].iter().enumerate() {
            if logit + rest > score {
                (best, score) = (token, logit + rest);
            }
        }
        let mut next = TreeState::default();
        for (token, bonus, to) in listed {
            if logits[token] + bonus > score {
                (best, score, next) = (token, logits[token] + bonus, to);
            }
        }
        (best, next)
    }
}

/// What the greedy search asks of the model.
pub trait Joiner {
    /// The joiner's logits for encoder frame `frame` given the tokens emitted so far; the blank is last.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn logits(&mut self, frame: usize) -> Result<Vec<f32>, RecognizerError>;

    /// The predictor takes `token` as the one emitted last.
    ///
    /// # Errors
    /// Fails when the model cannot continue.
    fn advance(&mut self, token: usize) -> Result<(), RecognizerError>;
}

/// Greedy search over `frames` encoder frames: the model alone decides between blank and a token,
/// and the tree only reorders the tokens. `state` is where the match stands, and carries on to the
/// next call.
///
/// # Errors
/// Fails when the model cannot continue.
pub fn greedy(
    joiner: &mut impl Joiner,
    tree: &PhraseTree,
    state: &mut TreeState,
    frames: usize,
) -> Result<Vec<Vec<usize>>, RecognizerError> {
    let mut emitted = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut tokens = Vec::new();
        for _ in 0..MAX_TOKENS_PER_FRAME {
            let logits = joiner.logits(frame)?;
            let blank = logits.len() - 1;
            let model_best = logits
                .iter()
                .enumerate()
                .fold((0, f32::NEG_INFINITY), |best, (id, value)| {
                    if *value > best.1 { (id, *value) } else { best }
                })
                .0;
            if model_best == blank {
                break;
            }
            let (token, next) = tree.pick(*state, &logits, blank);
            *state = next;
            tokens.push(token);
            joiner.advance(token)?;
        }
        emitted.push(tokens);
    }
    Ok(emitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIECES: [&str; 16] = [
        "\u{2581}C",
        "y",
        "b",
        "rex",
        "re",
        "x",
        "2",
        "\u{2581}Th",
        "al",
        "vi",
        "\u{2581}hey",
        "\u{2581}a",
        "\u{2581}the",
        "z",
        "d",
        "\u{2581}",
    ];
    const BLANK: usize = PIECES.len();
    const C: usize = 0;
    const Y: usize = 1;
    const B: usize = 2;
    const REX: usize = 3;
    const X: usize = 5;
    const TWO: usize = 6;
    const A: usize = 11;
    const Z: usize = 13;
    const D: usize = 14;

    fn pieces() -> Vec<String> {
        PIECES.iter().map(|p| (*p).to_owned()).collect()
    }

    fn tree(phrases: &[&str]) -> PhraseTree {
        let list = WordList::new(phrases.iter().map(ToString::to_string).collect()).unwrap();
        PhraseTree::new(&list, &pieces())
    }

    /// Logits that are far down everywhere except at the given tokens.
    fn row(at: &[(usize, f32)]) -> Vec<f32> {
        let mut row = vec![-30.0; BLANK + 1];
        for (token, logit) in at {
            row[*token] = *logit;
        }
        row
    }

    /// Plays one list of logit rows per encoder frame, one row per call.
    struct Script {
        frames: Vec<std::vec::IntoIter<Vec<f32>>>,
        advanced: Vec<usize>,
    }

    impl Script {
        fn new(frames: Vec<Vec<Vec<f32>>>) -> Self {
            Self {
                frames: frames.into_iter().map(Vec::into_iter).collect(),
                advanced: Vec::new(),
            }
        }
    }

    impl Joiner for Script {
        fn logits(&mut self, frame: usize) -> Result<Vec<f32>, RecognizerError> {
            self.frames[frame]
                .next()
                .ok_or_else(|| RecognizerError::said("the script ran out"))
        }

        fn advance(&mut self, token: usize) -> Result<(), RecognizerError> {
            self.advanced.push(token);
            Ok(())
        }
    }

    fn run(
        tree: &PhraseTree,
        state: &mut TreeState,
        frames: Vec<Vec<Vec<f32>>>,
    ) -> Vec<Vec<usize>> {
        let count = frames.len();
        let mut script = Script::new(frames);
        let got = greedy(&mut script, tree, state, count).unwrap();
        assert_eq!(
            script.advanced,
            got.concat(),
            "the predictor follows the emitted tokens"
        );
        got
    }

    fn blank_row() -> Vec<f32> {
        row(&[(BLANK, -0.1)])
    }

    #[test]
    fn a_phrase_is_cut_into_the_longest_pieces_with_its_digits() {
        let ids: HashMap<&str, usize> = PIECES.iter().copied().zip(0..).collect();
        let longest = PIECES.iter().map(|p| p.chars().count()).max().unwrap();
        let cases: [(&str, Option<Vec<usize>>); 4] = [
            ("\u{2581}Cybrex2", Some(vec![C, Y, B, REX, TWO])),
            ("\u{2581}hey\u{2581}Thalvi", Some(vec![10, 7, 8, 9])),
            ("\u{2581}Cybrex2\u{2581}\u{3a9}", None),
            ("", Some(vec![])),
        ];
        for (text, expected) in cases {
            assert_eq!(tokenize(text, &ids, longest), expected, "{text}");
        }
    }

    #[test]
    fn a_listed_phrase_comes_out_when_it_is_close_behind_the_best_path() {
        let tree = tree(&["Cybrex2"]);
        let cases = [("close behind", 0.8, C), ("far behind", 3.0, A)];
        for (name, gap, first) in cases {
            let frames = vec![vec![row(&[(A, -0.5), (C, -0.5 - gap)]), blank_row()]];
            let got = run(&tree, &mut TreeState::default(), frames);
            assert_eq!(got, [[first]], "{name}");
        }
    }

    #[test]
    fn a_match_that_is_given_up_pays_back_what_it_earned() {
        let tree = tree(&["Cybrex2", "a"]);
        let cases = [
            ("keeps going", 10.0, vec![B], 1),
            ("gives up", 17.0, vec![A], 0),
        ];
        for (name, gap, expected, after_depth) in cases {
            let mut state = TreeState::default();
            let begun = vec![
                vec![row(&[(C, -0.5)]), blank_row()],
                vec![row(&[(Y, -0.5)]), blank_row()],
            ];
            assert_eq!(run(&tree, &mut state, begun), [[C], [Y]], "{name}");
            let next = vec![vec![row(&[(B, -0.5 - gap), (A, -0.5)]), blank_row()]];
            assert_eq!(run(&tree, &mut state, next), [expected], "{name}");
            let moved = run(
                &tree,
                &mut state,
                vec![vec![row(&[(X, -0.5), (REX, -1.5)]), blank_row()]],
            );
            let expected_after = if after_depth == 1 { vec![REX] } else { vec![X] };
            assert_eq!(moved, [expected_after], "{name}");
        }
    }

    #[test]
    fn blank_wins_wherever_the_model_says_so() {
        let tree = tree(&["Cybrex2"]);
        let mut state = TreeState::default();
        let silence = |token: usize| vec![vec![row(&[(BLANK, -0.5), (token, -1.2)])]; 3];
        assert_eq!(
            run(&tree, &mut state, silence(C)),
            vec![Vec::<usize>::new(); 3]
        );
        assert_eq!(state, TreeState::default());
        run(
            &tree,
            &mut state,
            vec![vec![row(&[(C, -0.5)]), blank_row()]],
        );
        assert_ne!(state, TreeState::default());
        assert_eq!(
            run(&tree, &mut state, silence(Y)),
            vec![Vec::<usize>::new(); 3]
        );
    }

    #[test]
    fn an_empty_list_searches_as_the_plain_greedy_search_does() {
        let tree = tree(&[]);
        let mut state = TreeState::default();
        let endless: Vec<Vec<f32>> = vec![row(&[(Z, -0.1), (BLANK, -9.0)]); MAX_TOKENS_PER_FRAME];
        let frames = vec![
            vec![
                row(&[(A, -1.0), (D, -1.0), (BLANK, -3.0)]),
                row(&[(X, -0.2)]),
                blank_row(),
            ],
            vec![blank_row()],
            endless,
        ];
        let got = run(&tree, &mut state, frames);
        assert_eq!(got, [vec![A, X], vec![], vec![Z; MAX_TOKENS_PER_FRAME]]);
        assert_eq!(state, TreeState::default());
    }

    #[test]
    fn the_match_carries_on_into_the_next_call() {
        let tree = tree(&["Cybrex2"]);
        let second = || vec![vec![row(&[(A, -0.5), (Y, -3.0)]), blank_row()]];
        let mut state = TreeState::default();
        run(
            &tree,
            &mut state,
            vec![vec![row(&[(C, -0.5)]), blank_row()]],
        );
        assert_eq!(run(&tree, &mut state, second()), [[Y]]);
        assert_eq!(run(&tree, &mut TreeState::default(), second()), [[A]]);
    }

    #[test]
    fn a_failed_match_goes_on_in_a_listed_phrase_it_overlaps() {
        let tree = tree(&["Cd Cy", "Cb"]);
        let mut state = TreeState::default();
        let begun = [C, D, C].map(|token| vec![row(&[(token, -0.5)]), blank_row()]);
        run(&tree, &mut state, begun.into());
        let got = run(
            &tree,
            &mut state,
            vec![vec![row(&[(X, -0.5), (B, -2.0)]), blank_row()]],
        );
        assert_eq!(got, [[B]]);
    }
}
