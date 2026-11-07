//! The model of the tag explorer: what the overlay on the Runs screen shows, as pure functions over the rows the
//! client already holds. No terminal, network or AWS code, and nothing is kept on disk (invariant 5).
//!
//! * **Rows.** A [`Held`] is a held run row reduced to what the explorer needs: the run id, the time the run was created and its
//!   effective tags (the list of `GET /v1/runs`, or the derived project when an older backend sent none). [`TagIndex::build`] reads the
//!   rows once. A run listed twice counts once (the first listing); a tag named twice on a run counts once. A tag that is not text the
//!   screen can draw as it is (anything but printable ASCII without spaces, such as an escape sequence, a control, invisible or
//!   look-alike character; one over [`MAX_TAG_LEN`] bytes; a credential shape) is never counted, picked or drawn; [`TagIndex::dropped`]
//!   says how many were left out, and a run left with no tag is a run without tags. A pick that is not such text is ignored by the
//!   table, [`TagIndex::matching`] and the note alike, so no pick can filter the runs without a row that shows it.
//! * **View.** [`TagIndex::view`] gives the table: every tag with the number of runs that hold it, counted inside the runs that hold
//!   all the picked tags (picks are AND, so counts narrow as tags are picked), and the age of the newest of those runs ([`TagStat::last`]).
//!   Order: picked tags first, then runs descending, then the newest run first, then the tag text. A tag with no run in the result is
//!   hidden unless it is picked, so a pick that matches nothing stays on screen and can be dropped. The find line ([`find_matches`]) only
//!   cuts the table: every word must occur in the tag, ignoring case as the Runs filter does; a word that starts with `^` must begin
//!   the tag. A picked tag is never cut by the find. The find is cut at the Runs filter's query length and repeated words count once. [`View::tag_count`] is the number of tags before the find cut (so the note does not
//!   change while someone types), [`View::untagged`] counts every held run without a tag, whatever the picks.
//! * **Runs.** [`TagIndex::matching`] lists the run ids that hold all the picked tags, in row order; [`remove_plan`] lists the run ids
//!   that hold one tag and [`batches`] cuts them for the edit route; [`note_text`] words the line under the table as the Runs
//!   summary does (`12 of 212 runs match +project:billing-export · 10 tags`).
//!
//! The tag text is compared as it is held; picks match a held tag exactly.

use crate::redact::redact_tokens;
use crate::search::{MAX_QUERY_CHARS, fold};
use harness_protocol::common::parse_utc_unix;
use std::collections::{HashMap, HashSet};

/// A held run row, as the explorer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub run_id: String,
    /// RFC 3339 UTC time the run was created.
    pub created_at: String,
    /// The effective tags of the run.
    pub tags: Vec<String>,
}

/// One row of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagStat {
    pub tag: String,
    /// Runs that hold the tag, inside the runs that match the picks.
    pub runs: usize,
    /// Age of the newest of those runs (`59s`, `5m`, `3h`, `2d`, `2w`; `-` when a time cannot be read), empty when there is none.
    pub last: String,
}

/// The table and the counts under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub rows: Vec<TagStat>,
    /// Runs that hold every picked tag (all runs with no picks).
    pub matched: usize,
    /// Runs held.
    pub held: usize,
    /// Tags in the result before the find cut them.
    pub tag_count: usize,
    /// Held runs with no tag at all.
    pub untagged: usize,
}

#[derive(Clone, Debug)]
struct RowInfo {
    run_id: String,
    created: Option<i64>,
}

#[derive(Clone, Debug)]
struct TagInfo {
    text: String,
    folded: String,
    /// Indices of the rows that hold the tag, ascending.
    rows: Vec<usize>,
}

/// The held rows, read once: tags to rows and rows to tags.
#[derive(Clone, Debug, Default)]
pub struct TagIndex {
    rows: Vec<RowInfo>,
    row_tags: Vec<Vec<usize>>,
    tags: Vec<TagInfo>,
    by_text: HashMap<String, usize>,
    untagged: usize,
    dropped: usize,
}

/// The longest tag, in bytes, that is counted. The grammar allows 48; a tag over this is not a tag any writer makes.
pub const MAX_TAG_LEN: usize = 128;

/// True when the tag is text that can be counted, picked and drawn as it is: printable ASCII without a space (so no escape sequence,
/// control, invisible, wide or look-alike character), at most [`MAX_TAG_LEN`] bytes, and nothing `redact_tokens` would hide.
fn displayable(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_TAG_LEN
        && tag.chars().all(|c| c.is_ascii_graphic())
        && redact_tokens(tag) == tag
}

/// The words of a find line: `(starts the tag, folded text)`. A lone `^` holds nothing back.
struct Words(Vec<(bool, String)>);

impl Words {
    fn new(find: &str) -> Words {
        // Cut as the Runs filter's matcher cuts a query, and repeated words count once, so the cost of a line is bounded by what a
        // person can type.
        let find: String = find.chars().take(MAX_QUERY_CHARS).collect();
        let mut words: Vec<(bool, String)> = find
            .split_whitespace()
            .filter_map(|w| {
                let (prefix, rest) = match w.strip_prefix('^') {
                    Some(rest) => (true, rest),
                    None => (false, w),
                };
                (!rest.is_empty()).then(|| (prefix, fold(rest)))
            })
            .collect();
        words.sort_unstable();
        words.dedup();
        Words(words)
    }

    fn accepts(&self, folded_tag: &str) -> bool {
        self.0.iter().all(|(prefix, word)| {
            if *prefix {
                folded_tag.starts_with(word.as_str())
            } else {
                folded_tag.contains(word.as_str())
            }
        })
    }
}

/// True when `tag` passes the find line: every word is found in it, ignoring case; a word that starts with `^` must begin the tag.
/// An empty find (or one of blanks) passes every tag.
pub fn find_matches(tag: &str, find: &str) -> bool {
    Words::new(find).accepts(&fold(tag))
}

/// One table row on its way to the sort.
struct Entry {
    picked: bool,
    stat: TagStat,
    /// Creation time of the newest run in the result; `None` when there is none or its time cannot be read (the oldest).
    newest: Option<i64>,
}

/// The picks as the index reads them.
struct Resolved {
    ids: Vec<usize>,
    /// Readable picks that no held run has (shown with no runs so the person can drop them).
    absent: Vec<String>,
    /// A readable pick that is no held tag: no run can match.
    missing: bool,
}

impl TagIndex {
    /// Reads the rows. The first listing of a run id is the one that counts.
    pub fn build(rows: &[Held]) -> TagIndex {
        let mut index = TagIndex::default();
        let mut seen: HashSet<&str> = HashSet::with_capacity(rows.len());
        // The verdict on each distinct tag text: its id, or `None` when it cannot be shown.
        let mut verdict: HashMap<&str, Option<usize>> = HashMap::new();
        for held in rows {
            if !seen.insert(held.run_id.as_str()) {
                continue;
            }
            let row = index.rows.len();
            let mut ids: Vec<usize> = Vec::with_capacity(held.tags.len());
            for tag in &held.tags {
                let id = match verdict.get(tag.as_str()) {
                    Some(id) => *id,
                    None => {
                        let id = displayable(tag).then(|| {
                            let id = index.tags.len();
                            index.tags.push(TagInfo {
                                text: tag.clone(),
                                folded: fold(tag),
                                rows: Vec::new(),
                            });
                            index.by_text.insert(tag.clone(), id);
                            id
                        });
                        verdict.insert(tag.as_str(), id);
                        id
                    }
                };
                match id {
                    Some(id) => ids.push(id),
                    None => index.dropped += 1,
                }
            }
            ids.sort_unstable();
            ids.dedup();
            for &id in &ids {
                index.tags[id].rows.push(row);
            }
            if ids.is_empty() {
                index.untagged += 1;
            }
            index.rows.push(RowInfo {
                run_id: held.run_id.clone(),
                created: parse_utc_unix(&held.created_at),
            });
            index.row_tags.push(ids);
        }
        index
    }

    /// How many tags, counted as they stand in the rows read (a tag on three runs is three), were left out because they cannot be shown.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    fn resolve(&self, picks: &[String]) -> Resolved {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Resolved {
            ids: Vec::new(),
            absent: Vec::new(),
            missing: false,
        };
        for pick in picks {
            if !seen.insert(pick.as_str()) {
                continue;
            }
            match self.by_text.get(pick.as_str()) {
                Some(&id) => out.ids.push(id),
                None if displayable(pick) => {
                    out.missing = true;
                    out.absent.push(pick.clone());
                }
                // Not text the screen can draw: no row could show it, so it filters nothing.
                None => {}
            }
        }
        out
    }

    /// The rows that hold every picked tag, ascending.
    fn matched_rows(&self, picks: &Resolved) -> Vec<usize> {
        if picks.missing {
            return Vec::new();
        }
        let Some(&shortest) = picks.ids.iter().min_by_key(|&&t| self.tags[t].rows.len()) else {
            return (0..self.rows.len()).collect();
        };
        self.tags[shortest]
            .rows
            .iter()
            .copied()
            .filter(|row| {
                picks
                    .ids
                    .iter()
                    .all(|&t| t == shortest || self.tags[t].rows.binary_search(row).is_ok())
            })
            .collect()
    }

    /// The run ids that hold every picked tag, in row order; all runs with no picks.
    pub fn matching(&self, picks: &[String]) -> Vec<String> {
        self.matched_rows(&self.resolve(picks))
            .into_iter()
            .map(|row| self.rows[row].run_id.clone())
            .collect()
    }

    /// The table for the picks and the find line, with ages counted from `now` (RFC 3339 UTC).
    pub fn view(&self, picks: &[String], find: &str, now: &str) -> View {
        let resolved = self.resolve(picks);
        let matched = self.matched_rows(&resolved);
        let mut counts = vec![0usize; self.tags.len()];
        let mut newest: Vec<Option<usize>> = vec![None; self.tags.len()];
        for &row in &matched {
            let created = self.rows[row].created;
            for &tag in &self.row_tags[row] {
                counts[tag] += 1;
                let older = newest[tag].is_none_or(|n| self.rows[n].created < created);
                if older {
                    newest[tag] = Some(row);
                }
            }
        }
        let mut picked = vec![false; self.tags.len()];
        for &id in &resolved.ids {
            picked[id] = true;
        }
        let now = parse_utc_unix(now);
        let words = Words::new(find);

        let mut entries: Vec<Entry> = Vec::new();
        let mut tag_count = 0;
        for (id, info) in self.tags.iter().enumerate() {
            if counts[id] == 0 && !picked[id] {
                continue;
            }
            tag_count += 1;
            if !picked[id] && !words.accepts(&info.folded) {
                continue;
            }
            let newest_row = newest[id].map(|row| &self.rows[row]);
            entries.push(Entry {
                picked: picked[id],
                stat: TagStat {
                    tag: info.text.clone(),
                    runs: counts[id],
                    last: newest_row.map_or_else(String::new, |r| age(now, r.created)),
                },
                newest: newest_row.and_then(|r| r.created),
            });
        }
        for tag in &resolved.absent {
            tag_count += 1;
            entries.push(Entry {
                picked: true,
                stat: TagStat {
                    tag: tag.clone(),
                    runs: 0,
                    last: String::new(),
                },
                newest: None,
            });
        }
        entries.sort_by(|a, b| {
            b.picked
                .cmp(&a.picked)
                .then(b.stat.runs.cmp(&a.stat.runs))
                .then(b.newest.cmp(&a.newest))
                .then_with(|| a.stat.tag.cmp(&b.stat.tag))
        });
        View {
            rows: entries.into_iter().map(|e| e.stat).collect(),
            matched: matched.len(),
            held: self.rows.len(),
            tag_count,
            untagged: self.untagged,
        }
    }
}

/// The age of a run created at `created` seen at `now`, in the one unit that fits in five columns: `59s`, `5m`, `3h`, `2d` (up to 13
/// days), `2w` (up to `99w`), then years (`2y`, rounded, at most `9999y`). A time in the future is `0s`; a time that cannot be read is `-`.
pub fn age_text(now: &str, created: &str) -> String {
    age(parse_utc_unix(now), parse_utc_unix(created))
}

fn age(now: Option<i64>, created: Option<i64>) -> String {
    let (Some(now), Some(created)) = (now, created) else {
        return "-".to_string();
    };
    let s = now.saturating_sub(created).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3_599 => format!("{}m", s / 60),
        3_600..=86_399 => format!("{}h", s / 3_600),
        86_400..=1_209_599 => format!("{}d", s / 86_400),
        1_209_600..=60_479_999 => format!("{}w", s / 604_800),
        // The mean Gregorian year. Two timestamps can be 10,000 years apart; five columns hold `9999y`.
        _ => format!("{}y", ((s + 15_778_476) / 31_556_952).min(9_999)),
    }
}

/// The ids of the held runs that hold `tag`, in row order, each run once: the runs a "remove everywhere" edit names.
pub fn remove_plan(tag: &str, rows: &[Held]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    rows.iter()
        .filter(|r| seen.insert(r.run_id.as_str()))
        .filter(|r| r.tags.iter().any(|t| t == tag))
        .map(|r| r.run_id.clone())
        .collect()
}

/// `ids` in order, cut into batches of at most `n` (the edit route takes 40 runs at a time); a size of 0 is 1.
pub fn batches(ids: &[String], n: usize) -> Vec<Vec<String>> {
    ids.chunks(n.max(1)).map(<[String]>::to_vec).collect()
}

/// The line under the table, in the style of the Runs summary: `212 runs · 41 tags · 30 have no tags` with no picks,
/// `12 of 212 runs match +project:billing-export · 10 tags` with picks, `no run matches +tech:go (212 runs)` when none does. A pick
/// the table could not show is left out of the line, as it is left out of the table.
pub fn note_text(view: &View, picks: &[String]) -> String {
    let plural = |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
    let runs = plural(view.held, "run", "runs");
    let tags = plural(view.tag_count, "tag", "tags");
    // The picks the table read: each once, and only those it could show.
    let mut seen: HashSet<&str> = HashSet::new();
    let picks: Vec<&String> = picks
        .iter()
        .filter(|p| displayable(p) && seen.insert(p.as_str()))
        .collect();
    if picks.is_empty() {
        let mut note = format!("{} {runs} \u{b7} {} {tags}", view.held, view.tag_count);
        if view.untagged > 0 {
            note.push_str(&format!(
                " \u{b7} {} {} no tags",
                view.untagged,
                plural(view.untagged, "has", "have")
            ));
        }
        return note;
    }
    let named = picks
        .iter()
        .map(|p| format!("+{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    if view.matched == 0 {
        return format!("no run matches {named} ({} {runs})", view.held);
    }
    format!(
        "{} of {} {runs} match {named} \u{b7} {} {tags}",
        view.matched, view.held, view.tag_count
    )
}
