//! The vocabulary: the person's recent topic and tech tags with the number of runs that hold each, the reuse hints the model sees.
//!
//! Items are the search items of the person's newest runs, newest first (the order of the `Query`). Per item the tags counted are
//! `user ++ auto` minus the item's own off list, only the kinds `topic` and `tech` (a project is the repository or the person's, never
//! a hint), and only tags that pass the grammar as they are read, because these lines go into a prompt. Spellings of one comparison key
//! are one line, counted over the runs of all its spellings and written with the spelling most runs hold among those the model may
//! write (a key that only has spellings the model may not write, such as a generic word the person typed, is not a line and not in the
//! key map). At most 24 topic and 16 tech lines, ranked by runs, then recency, then text. A tag held by more than 30% of a window of 50
//! or more runs says nothing about any one run and is left out of the lines; it stays in the key map so a model tag still snaps to its
//! spelling. A line with an instruction-like word (matched with its endings: `ignored`, `prompts`) is left out as well.

use super::grammar::{Origin, check_stored, fold_token, key_of, kind_of};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

/// The most topic lines.
pub const MAX_TOPIC_LINES: usize = 24;
/// The most tech lines.
pub const MAX_TECH_LINES: usize = 16;
/// The shown window must have this many runs before the share cap applies.
pub const TOP_SHARE_MIN_RUNS: usize = 50;
/// A tag held by more than this percent of the window is left out of the lines.
pub const TOP_SHARE_MAX_PERCENT: usize = 30;
/// Words that make a vocabulary line look like an order to the model. A line is left out of the prompt when a word of its value, folded
/// like a comparison key token ([`fold_token`]), equals one of these folded (`ignored`, `system-prompts` and `overrides` are caught;
/// `ignorance` and `promptly` are not).
pub const IMPERATIVE_WORDS: [&str; 12] = [
    "ignore",
    "instruct",
    "instruction",
    "instructions",
    "system",
    "prompt",
    "override",
    "disregard",
    "forget",
    "reveal",
    "assistant",
    "developer",
];
/// What the block says when there is nothing to show.
pub const NONE_YET: &str = "(none yet)";

/// The tag lists of one search item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VocabItem {
    pub auto: Vec<String>,
    pub user: Vec<String>,
    pub off: Vec<String>,
}

/// The lines for the prompt and the facts the judge and the log line need.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Vocabulary {
    /// `(tag, runs)`, in the order they are shown.
    pub lines: Vec<(String, u32)>,
    /// Comparison key to the spelling the model may write, for every topic and tech tag read (not only the lines shown).
    pub keys: BTreeMap<String, String>,
    /// The runs in the window, with or without tags.
    pub window_runs: usize,
    /// The largest share of the window held by one tag, in basis points (3166 is 31.66%).
    pub top_share_bp: u32,
}

struct Entry {
    spelling: String,
    runs: u32,
    newest: usize,
}

/// What the window says about one comparison key: the runs that hold it, the newest of them, and how often each spelling occurs.
struct KeyStat {
    runs: u32,
    newest: usize,
    /// spelling -> (runs holding it, newest run)
    spellings: BTreeMap<String, (u32, usize)>,
}

fn has_imperative_word(tag: &str) -> bool {
    let value = tag.split_once(':').map_or(tag, |(_, v)| v);
    value.split('-').any(|word| {
        let folded = fold_token(word);
        IMPERATIVE_WORDS.iter().any(|w| fold_token(w) == folded)
    })
}

fn rank(e: &Entry) -> (Reverse<u32>, usize, &str) {
    (Reverse(e.runs), e.newest, e.spelling.as_str())
}

impl Vocabulary {
    /// Builds the vocabulary of a window of runs, newest first.
    pub fn from_items(items: &[VocabItem]) -> Vocabulary {
        let mut stats: HashMap<String, KeyStat> = HashMap::new();
        for (idx, item) in items.iter().enumerate() {
            let off_keys: HashSet<String> = item.off.iter().map(|t| key_of(t)).collect();
            let mut seen_keys: HashSet<String> = HashSet::new();
            let mut seen_spellings: HashSet<&str> = HashSet::new();
            let tags = item
                .user
                .iter()
                .map(|t| (t, Origin::User))
                .chain(item.auto.iter().map(|t| (t, Origin::Model)));
            for (tag, origin) in tags {
                if !matches!(kind_of(tag), Some("topic" | "tech"))
                    || check_stored(tag, origin).is_err()
                {
                    continue;
                }
                let key = key_of(tag);
                if off_keys.contains(&key) {
                    continue;
                }
                let stat = stats.entry(key.clone()).or_insert_with(|| KeyStat {
                    runs: 0,
                    newest: usize::MAX,
                    spellings: BTreeMap::new(),
                });
                if seen_keys.insert(key) {
                    stat.runs += 1;
                    stat.newest = stat.newest.min(idx);
                }
                if seen_spellings.insert(tag.as_str()) {
                    let spelling = stat.spellings.entry(tag.clone()).or_insert((0, usize::MAX));
                    spelling.0 += 1;
                    spelling.1 = spelling.1.min(idx);
                }
            }
        }

        let mut keys = BTreeMap::new();
        let mut entries: Vec<Entry> = Vec::new();
        for (key, stat) in stats {
            // The spelling most runs hold among those the model may write; a tie goes to the one in the newest run, then to the
            // smaller text. A key with none is not shown and not offered for snapping.
            let Some((spelling, _)) = stat
                .spellings
                .iter()
                .filter(|(text, _)| check_stored(text, Origin::Model).is_ok())
                .min_by_key(|(text, (n, first))| (Reverse(*n), *first, text.as_str()))
            else {
                continue;
            };
            let spelling = spelling.clone();
            keys.insert(key, spelling.clone());
            entries.push(Entry {
                spelling,
                runs: stat.runs,
                newest: stat.newest,
            });
        }

        let window = items.len();
        let top_share_bp = entries
            .iter()
            .map(|e| e.runs)
            .max()
            .map_or(0, |m| (u64::from(m) * 10_000 / window.max(1) as u64) as u32);
        let capped = |e: &Entry| {
            window >= TOP_SHARE_MIN_RUNS && e.runs as usize * 100 > TOP_SHARE_MAX_PERCENT * window
        };
        let mut shown: Vec<&Entry> = entries
            .iter()
            .filter(|e| !capped(e) && !has_imperative_word(&e.spelling))
            .collect();
        shown.sort_by(|a, b| rank(a).cmp(&rank(b)));
        let mut topics = 0;
        let mut techs = 0;
        shown.retain(|e| {
            let (count, max) = if e.spelling.starts_with("topic:") {
                (&mut topics, MAX_TOPIC_LINES)
            } else {
                (&mut techs, MAX_TECH_LINES)
            };
            *count += 1;
            *count <= max
        });
        let lines = shown
            .into_iter()
            .map(|e| (e.spelling.clone(), e.runs))
            .collect();
        Vocabulary {
            lines,
            keys,
            window_runs: window,
            top_share_bp,
        }
    }

    /// Reads a block written by [`Vocabulary::block`] (one `tag (n)` per line, or `(none yet)`) back into a vocabulary, for a tool that
    /// is handed the block as text. A line that is not `tag (n)`, or whose tag the model may not write, is ignored. The window facts are zero.
    pub fn from_block(text: &str) -> Vocabulary {
        let mut lines: Vec<(String, u32)> = Vec::new();
        let mut keys = BTreeMap::new();
        for line in text.lines().map(str::trim) {
            let Some((tag, rest)) = line.rsplit_once(" (") else {
                continue;
            };
            let Some(runs) = rest
                .strip_suffix(')')
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| *n > 0)
            else {
                continue;
            };
            if !matches!(kind_of(tag), Some("topic" | "tech"))
                || check_stored(tag, Origin::Model).is_err()
            {
                continue;
            }
            if keys.insert(key_of(tag), tag.to_string()).is_none() {
                lines.push((tag.to_string(), runs));
            }
        }
        Vocabulary {
            lines,
            keys,
            window_runs: 0,
            top_share_bp: 0,
        }
    }

    /// The text between the `<vocabulary>` tags of the prompt: one `tag (n)` per line, or `(none yet)`. A line whose tag fails the
    /// grammar is not printed, so a vocabulary built by hand cannot put an unchecked string in front of the model.
    pub fn block(&self) -> String {
        let printed: Vec<String> = self
            .lines
            .iter()
            .filter(|(tag, _)| check_stored(tag, Origin::User).is_ok())
            .map(|(tag, runs)| format!("{tag} ({runs})"))
            .collect();
        if printed.is_empty() {
            return NONE_YET.to_string();
        }
        printed.join("\n")
    }
}
