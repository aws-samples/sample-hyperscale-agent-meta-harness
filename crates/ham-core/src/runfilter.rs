//! The Runs screen's filter: what the list shows when the user types words into the find-or-command box, the status gutter,
//! the scope toggle, and how the timer refreshes the list. Everything here is pure; the screen draws what it returns.
//!
//! * **Rows.** [`RunRow`] is a `RunSummary` as the screen words it: the id, the status (`usage limited` for `usage_limited`), the target
//!   (`cc:calc`, `new cc:calc`, `s3-only`), the project (the repository, whether the run started on it or created it; `s3-only` is a
//!   project too), and, once the run index exists, the prompt title and the model ([`RunRow::with_index`]; cleaned of controls and
//!   escape sequences) and the run's name with the source it came from (`ai` or `seed`). The [`Gutter`] says whether a run is
//!   running, needs the user, or failed.
//! * **Filter.** [`RunsFilter`] takes the text of the box. Every word must be
//!   found, ignoring case, in the id, the status words, the target, the title, the name, the model or one of the run's tags
//!   ([`crate::search`] does the matching, so the box and the chat search agree on case and Unicode). The list keeps its order. A scope
//!   limits it to one project; the toggle takes the project of the focused run, and turns the scope off without one. Picked tags
//!   limit it to the runs that hold all of them, each exactly as the row shows it. [`RunsFilter::highlight`] gives the character ranges to
//!   mark in a cell, [`RunsFilter::summary`] the line under the list, [`select_after`] where the selection goes when the list changes.
//!   The key line words the `p` key by what a press does ([`RunsFilter::scope_key_labels`], longest wording first) and the bottom border of
//!   the table says where the selected run stands.
//! * **Tags.** A row carries the run's effective tags: the list the run index sent, or, from a backend that sends none, the project the
//!   run's repository gives. A list from the server is final: a tag the person removed is not given back, and the client checks each tag
//!   again, so one with an escape byte or a broken grammar is never drawn.
//! * **Refresh.** The timer reads the newest page of [`FIRST_PAGE`] runs ([`Refresh::Timer`] is [`Pages::First`]) and `r` reads all pages
//!   ([`Pages::All`]); [`merge_first_page`] lays a refreshed first page over the full list (the next token, not the page's length, says
//!   whether more follow), and [`newly_needing_you`] names the runs that began to wait for the user between two lists (for the bell).

use crate::redact::redact_tokens;
use crate::router::is_command_attempt;
use crate::runtag::effective::effective;
use crate::sanitize::display_line;
use crate::search::find_ranges;
use harness_protocol::api::v1::RunSummary;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::goal::{GoalStatus, ResumeMode};
use harness_protocol::target::RunTarget;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// The runs in the newest page; the server's list page size.
pub const FIRST_PAGE: usize = 100;

/// The project of a run that has no repository.
const S3_ONLY: &str = "s3-only";

/// The longest repository name the `p` label spells out; a longer one is cut and ends with an ellipsis.
const SCOPE_LABEL_NAME: usize = 24;

/// `run 12 of 769`: where the selected run stands among the `of` runs the list shows (`at` counts from 1). The Runs screen puts it at the
/// right end of the table's bottom border.
pub fn position_text(at: usize, of: usize) -> String {
    format!("run {at} of {of}")
}

/// `name` as it appears in the `p` label: whole up to [`SCOPE_LABEL_NAME`] characters, else its start and an ellipsis (that many characters in all).
fn cut_name(name: &str) -> String {
    if name.chars().count() <= SCOPE_LABEL_NAME {
        return name.to_string();
    }
    let mut out: String = name.chars().take(SCOPE_LABEL_NAME - 1).collect();
    out.push('\u{2026}');
    out
}

/// The mark in the gutter of a run's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gutter {
    None,
    /// The run is active and nothing waits on the user.
    Running,
    /// Cline waits for an answer.
    NeedsYou,
    /// The run ended early or the harness blocked it.
    Failed,
    /// The run stopped delivered at the total time it was given; `R` resumes it.
    Paused,
    /// The run stopped delivered because it lost its progress; `R` resumes it.
    Stalled,
}

impl Gutter {
    /// The word for a state (`running`, `needs you`, `failed`, `paused`, `stalled`); empty for none.
    pub fn word(self) -> &'static str {
        match self {
            Gutter::None => "",
            Gutter::Running => "running",
            Gutter::NeedsYou => "needs you",
            Gutter::Failed => "failed",
            Gutter::Paused => "paused",
            Gutter::Stalled => "stalled",
        }
    }

    /// One character of plain ASCII (its width is the same in every terminal): `>` running, `!` needs you, `x` failed, `=` paused, `~` stalled,
    /// a space for none.
    pub fn glyph(self) -> &'static str {
        match self {
            Gutter::None => " ",
            Gutter::Running => ">",
            Gutter::NeedsYou => "!",
            Gutter::Failed => "x",
            Gutter::Paused => "=",
            Gutter::Stalled => "~",
        }
    }
}

fn enum_text<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "-".into())
}

/// One line of display text: controls and escape sequences gone, tokens hidden, blanks collapsed.
fn clean(text: &str) -> String {
    redact_tokens(&display_line(text))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A run as the Runs screen words it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRow {
    pub run_id: String,
    /// The status with underscores read as spaces (`usage limited`).
    pub status: String,
    /// `cc:calc`, `new cc:calc` or `s3-only`.
    pub target: String,
    /// The repository, or `s3-only`: what the scope toggle limits the list to.
    pub project: String,
    pub created_at: String,
    /// The prompt title of the run index (absent until the index exists).
    pub title: Option<String>,
    pub model: Option<String>,
    /// The run's name, cleaned like the other words: the model's name or the seed built from the prompt. Absent until the index
    /// exists.
    pub name: Option<String>,
    /// Where `name` came from: `ai`, `seed`, or another word of a newer writer. Only `ai` is drawn as a model name. Absent without a name.
    pub name_source: Option<String>,
    /// The run's effective tags, in the order the server sends them (the derived project first, then the person's tags, then the
    /// automatic ones). A row that has no list from the server shows the project its repository gives.
    pub tags: Vec<String>,
    /// The run this follow-up run continues from, cleaned like the other words; absent for an ordinary run.
    pub continue_from: Option<String>,
    pub gutter: Gutter,
    /// Why a `paused` or `stalled` run stopped, cleaned like the other words: `code: sentence`.
    pub status_reason: Option<String>,
    /// How a `paused` or `stalled` run can go on.
    pub resume_mode: Option<ResumeMode>,
    /// The session (slice) the run is in or ended in; absent for a run that never renewed its session or is older than the member.
    pub session: Option<u32>,
}

impl RunRow {
    pub fn from_summary(s: &RunSummary) -> RunRow {
        let needs = s.needs_input.is_some() && !s.status.is_terminal();
        let failed = s.aborted.is_some() || s.status == GoalStatus::Blocked;
        let gutter = if failed {
            Gutter::Failed
        } else if needs {
            Gutter::NeedsYou
        } else if s.status == GoalStatus::Active {
            Gutter::Running
        } else if s.status == GoalStatus::Paused {
            Gutter::Paused
        } else if s.status == GoalStatus::Stalled {
            Gutter::Stalled
        } else {
            Gutter::None
        };
        let (target, project) = match &s.target {
            RunTarget::Repo { repo } => (clean(&repo.to_string()), clean(&repo.to_string())),
            RunTarget::NewRepo { repo } => (
                format!("new {}", clean(&repo.to_string())),
                clean(&repo.to_string()),
            ),
            RunTarget::S3Only => (S3_ONLY.to_string(), S3_ONLY.to_string()),
        };
        let tags = effective(&target, &[], &[], &[]).tags;
        RunRow {
            run_id: clean(&s.run_id),
            status: enum_text(&s.status).replace('_', " "),
            target,
            project,
            created_at: s.created_at.clone(),
            title: None,
            model: None,
            name: None,
            name_source: None,
            tags,
            continue_from: s
                .continue_from
                .as_deref()
                .map(clean)
                .filter(|c| !c.is_empty()),
            gutter,
            status_reason: s
                .status_reason
                .as_deref()
                .map(clean)
                .filter(|r| !r.is_empty()),
            resume_mode: s.resume_mode,
            session: s.session,
        }
    }

    /// The same row with the title, the model, the name, the name's source and the tags that the run index holds. A source without a name is
    /// dropped. `tags` is the server's list of effective tags: `Some` replaces what the row showed, an empty list included (the person
    /// removed them, and the project of the repository is not given back); each tag is checked again as a typed tag, so one that breaks
    /// the grammar, holds an escape byte or repeats another is not kept. `None` (a backend that sends no tags) leaves the row with the
    /// project its repository gives.
    pub fn with_index(
        mut self,
        title: Option<&str>,
        model: Option<&str>,
        name: Option<&str>,
        name_source: Option<&str>,
        tags: Option<&[String]>,
    ) -> RunRow {
        self.title = title.map(clean).filter(|t| !t.is_empty());
        self.model = model.map(clean).filter(|m| !m.is_empty());
        self.name = name.map(clean).filter(|n| !n.is_empty());
        self.name_source = self
            .name
            .as_ref()
            .and_then(|_| name_source.map(clean).filter(|s| !s.is_empty()));
        self.tags = match tags {
            Some(list) => effective("", &[], list, &[]).tags,
            None => effective(&self.target, &[], &[], &[]).tags,
        };
        self
    }

    /// What names the run in a list or a box: its name, else the title of its prompt, else nothing.
    pub fn label(&self) -> Option<&str> {
        self.name.as_deref().or(self.title.as_deref())
    }

    /// The status field with the state words of the gutter, so `needs`, `you` and `failed` find the runs the gutter marks.
    fn status_words(&self) -> String {
        match self.gutter {
            Gutter::NeedsYou => format!("{} needs input needs you", self.status),
            Gutter::Failed => format!("{} failed", self.status),
            Gutter::Running => format!("{} running", self.status),
            // `resumable` finds the runs the R key resumes.
            Gutter::Paused | Gutter::Stalled if self.resume_mode != Some(ResumeMode::None) => {
                format!("{} resumable", self.status)
            }
            Gutter::Paused | Gutter::Stalled | Gutter::None => self.status.clone(),
        }
    }

    /// Whether `word` is found in the id, the status words, the target, the title, the name, the model or one of the tags.
    fn holds(&self, word: &str, status_words: &str) -> bool {
        [
            self.run_id.as_str(),
            status_words,
            self.target.as_str(),
            self.title.as_deref().unwrap_or(""),
            self.name.as_deref().unwrap_or(""),
            self.model.as_deref().unwrap_or(""),
        ]
        .into_iter()
        .chain(self.tags.iter().map(String::as_str))
        .any(|field| !find_ranges(field, word).is_empty())
    }
}

/// What the list is limited to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    All,
    /// One project (see [`RunRow::project`]).
    Project(String),
}

/// The filter of the Runs screen.
#[derive(Clone, Debug, Default)]
pub struct RunsFilter {
    text: String,
    words: Vec<String>,
    scope: Scope,
    /// The tags picked in the explorer: a run must hold every one, exactly as the row shows it.
    tags: Vec<String>,
}

impl RunsFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The text of the find-or-command box. A command attempt (`/word`, `ham word`; see [`is_command_attempt`]) filters nothing.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.words = if is_command_attempt(text) {
            Vec::new()
        } else {
            text.split_whitespace().map(str::to_string).collect()
        };
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Words are in force.
    pub fn is_filtering(&self) -> bool {
        !self.words.is_empty()
    }

    /// Words, a scope or picked tags limit the list.
    pub fn is_narrowed(&self) -> bool {
        self.is_filtering() || self.scope != Scope::All || !self.tags.is_empty()
    }

    /// The picked tags, in the order they were picked.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Picks these tags (a tag named twice counts once): the list shows the runs that hold all of them.
    pub fn set_tags(&mut self, tags: Vec<String>) {
        let mut seen = HashSet::new();
        self.tags = tags
            .into_iter()
            .filter(|t| seen.insert(t.clone()))
            .collect();
    }

    /// Esc: the words, the scope and the picked tags all go, and the list is whole again.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Turns the scope on, for the project of the focused run (nothing happens without one), or off.
    pub fn toggle_scope(&mut self, focused: Option<&RunRow>) {
        self.scope = match (&self.scope, focused) {
            (Scope::Project(_), _) => Scope::All,
            (Scope::All, Some(r)) => Scope::Project(r.project.clone()),
            (Scope::All, None) => Scope::All,
        };
    }

    pub fn matches(&self, row: &RunRow) -> bool {
        if let Scope::Project(p) = &self.scope
            && &row.project != p
        {
            return false;
        }
        if !self.tags.iter().all(|pick| row.tags.contains(pick)) {
            return false;
        }
        if self.words.is_empty() {
            return true;
        }
        let status = row.status_words();
        self.words.iter().all(|w| row.holds(w, &status))
    }

    /// The wordings of the `p` key on the Runs screen's key line, from the longest to the shortest; the screen takes the first that fits
    /// the room its line has left, so an 80-column line gets a short one and a wide line names the repository. Every wording starts with `p `
    /// (the line's chip reads its key from there). With the limit off, the label says what a press picks: `p only cc:calc` (the repository of
    /// `focused`, cut to [`SCOPE_LABEL_NAME`] characters), `p this repo`, `p repo`; a run with no repository names `s3-only`, never a
    /// repository. With the limit on, it says what a press does: `p show all runs`, `p all runs`, `p all`.
    pub fn scope_key_labels(&self, focused: Option<&RunRow>) -> Vec<String> {
        let words = |list: &[&str]| list.iter().map(|w| (*w).to_string()).collect::<Vec<_>>();
        match (&self.scope, focused) {
            (Scope::Project(_), _) => words(&["p show all runs", "p all runs", "p all"]),
            (Scope::All, Some(r)) if r.project == S3_ONLY => {
                words(&["p only s3-only", "p s3-only"])
            }
            (Scope::All, Some(r)) => vec![
                format!("p only {}", cut_name(&r.project)),
                "p this repo".to_string(),
                "p repo".to_string(),
            ],
            (Scope::All, None) => words(&["p repo"]),
        }
    }

    /// The indices of the rows that match, in the list's order.
    pub fn visible(&self, rows: &[RunRow]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| self.matches(r))
            .map(|(i, _)| i)
            .collect()
    }

    /// The character ranges of `text` to mark: where any word is found, overlapping and touching ranges joined.
    pub fn highlight(&self, text: &str) -> Vec<Range<usize>> {
        let mut all: Vec<Range<usize>> = self
            .words
            .iter()
            .flat_map(|w| find_ranges(text, w))
            .collect();
        all.sort_by_key(|r| (r.start, r.end));
        let mut out: Vec<Range<usize>> = Vec::new();
        for r in all {
            match out.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => out.push(r),
            }
        }
        out
    }

    /// The line about the list: `300 runs`, `10 of 300 runs match "calc"`, `4 of 300 runs in cc:calc`, `no run matches "x" (300 runs)`,
    /// `12 of 212 runs match +project:billing-export` (picked tags, each after a plus; after the words when both are there).
    pub fn summary(&self, shown: usize, total: usize) -> String {
        let noun = if total == 1 { "run" } else { "runs" };
        let scope = match &self.scope {
            Scope::Project(p) => Some(clean(p)),
            Scope::All => None,
        };
        let words = self
            .words
            .iter()
            .map(|w| clean(w))
            .collect::<Vec<_>>()
            .join(" ");
        let picks = self
            .tags
            .iter()
            .map(|t| format!("+{}", clean(t)))
            .collect::<Vec<_>>()
            .join(" ");
        let matching = match (words.is_empty(), picks.is_empty()) {
            (true, true) => None,
            (false, true) => Some(format!("\"{words}\"")),
            (true, false) => Some(picks),
            (false, false) => Some(format!("\"{words}\" {picks}")),
        };
        if scope.is_none() && matching.is_none() {
            return format!("{total} {noun}");
        }
        if shown == 0 {
            let mut s = String::from("no run");
            if let Some(p) = &scope {
                s.push_str(&format!(" in {p}"));
            }
            if let Some(w) = &matching {
                s.push_str(&format!(" matches {w}"));
            }
            s.push_str(&format!(" ({total} {noun})"));
            return s;
        }
        let mut s = format!("{shown} of {total} {noun}");
        if let Some(p) = &scope {
            s.push_str(&format!(" in {p}"));
        }
        if let Some(w) = &matching {
            s.push_str(&format!(" match {w}"));
        }
        s
    }
}

/// Where the selection goes when the visible list changes: the position of the run that was selected if it is still shown, else the old
/// position kept inside the new list; `None` when nothing is shown.
pub fn select_after(
    rows: &[RunRow],
    visible: &[usize],
    selected: Option<&str>,
    prev_pos: usize,
) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    if let Some(id) = selected
        && let Some(at) = visible
            .iter()
            .position(|&i| rows.get(i).is_some_and(|r| r.run_id == id))
    {
        return Some(at);
    }
    Some(prev_pos.min(visible.len() - 1))
}

/// Why the list is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// The 1 s timer.
    Timer,
    /// `r`.
    Manual,
}

/// How much of the list a read asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pages {
    /// The newest [`FIRST_PAGE`] runs.
    First,
    All,
}

impl Refresh {
    /// The timer reads the newest page only, so a long history does not load the server every second; `r` reads everything.
    pub fn pages(self) -> Pages {
        match self {
            Refresh::Timer => Pages::First,
            Refresh::Manual => Pages::All,
        }
    }
}

/// `old` (the whole list, newest first) with its newest rows replaced by `page`, a read of the first page; `has_more` says the server
/// sent a next token. Without one the page is the whole list. With one, rows of `old` that are at or older than the page's oldest row
/// and not on the page stay as they were read, and rows newer than that edge that the page lacks are gone. The page's length does not
/// decide: the server skips a run item it cannot parse, so a page can be short and still have a next page.
pub fn merge_first_page(
    old: &[RunSummary],
    page: Vec<RunSummary>,
    has_more: bool,
) -> Vec<RunSummary> {
    if !has_more {
        return page;
    }
    let Some(edge) = page.last().map(|r| r.created_at.clone()) else {
        return old.to_vec();
    };
    let on_page: HashSet<String> = page.iter().map(|r| r.run_id.clone()).collect();
    let mut out = page;
    out.extend(
        old.iter()
            .filter(|r| not_after(&r.created_at, &edge) && !on_page.contains(&r.run_id))
            .cloned(),
    );
    out
}

/// `time` is not later than `edge`, as times (seconds; a fraction is ignored, so a time in the edge's second counts as the edge). Two
/// strings that are not both RFC 3339 UTC times compare as text.
fn not_after(time: &str, edge: &str) -> bool {
    match (parse_utc_unix(time), parse_utc_unix(edge)) {
        (Some(t), Some(e)) => t <= e,
        _ => time <= edge,
    }
}

/// One read of the Runs list, kept apart from the list it will be laid over: a timer read and a manual read can be in flight together,
/// so the merge happens when a result is applied, against the list held then, never against a copy taken when the request began.
#[derive(Clone, Debug)]
pub enum RunsRead {
    /// The newest page, and whether the server sent a next token ([`merge_first_page`]).
    First {
        runs: Vec<RunSummary>,
        has_more: bool,
    },
    /// Every page.
    All(Vec<RunSummary>),
}

impl RunsRead {
    /// The list to hold now: this read laid over `held`.
    pub fn apply(self, held: &[RunSummary]) -> Vec<RunSummary> {
        match self {
            RunsRead::First { runs, has_more } => merge_first_page(held, runs, has_more),
            RunsRead::All(runs) => runs,
        }
    }
}

/// What the run index holds about one run of the list: the first line of its prompt, its model, its name with the name's source
/// and its effective tags. The list route carries them on every row of a stage that has the index; the Runs screen joins them to its rows, so the filter
/// finds a run by words of its prompt or its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub run_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
    pub name_source: Option<String>,
    /// The run's effective tags as the server sent them. `Some` is final, an empty list too; `None` is a backend that sends none.
    pub tags: Option<Vec<String>>,
}

/// A read of the list with the index entries of its rows: one entry for each row that has a title or a model.
#[derive(Clone, Debug)]
pub struct IndexedRead {
    pub read: RunsRead,
    pub index: Vec<IndexEntry>,
}

/// The ids, in `new`'s order, of the runs that wait for the user in `new` and did not in `old` (or were not in it).
pub fn newly_needing_you(old: &[RunSummary], new: &[RunSummary]) -> Vec<String> {
    let waits = |r: &RunSummary| r.needs_input.is_some() && !r.status.is_terminal();
    let waited: HashMap<&str, bool> = old.iter().map(|r| (r.run_id.as_str(), waits(r))).collect();
    new.iter()
        .filter(|r| waits(r))
        .filter(|r| !waited.get(r.run_id.as_str()).copied().unwrap_or(false))
        .map(|r| r.run_id.clone())
        .collect()
}
