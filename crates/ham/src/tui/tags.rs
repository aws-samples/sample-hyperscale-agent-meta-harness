//! The tag explorer and the `/tag` word. A child module of `app`, so it reads the app's private state.
//!
//! **The explorer.** `t` on the Runs list opens an overlay that takes the whole frame and every key. It is built from the rows the screen already
//! holds (`ham_core::tagexplore`): the table of tags with the number of held runs that hold each, the picks (AND), the find line, and the counts
//! of the picks. `Space` picks the tag under the cursor, `/` opens the find line, `Enter` applies the picks to the Runs list (with no pick, the
//! tag under the cursor), `d` removes the tag under the cursor from every held run after a `y/n` question, `r` reads every page of the list, `Esc` and
//! `t` clear the find and then close the overlay without applying. Nothing is kept on disk and no screen, scenario or key row was added.
//!
//! * **Counts cover the held rows.** The box title says whether the list was read in full (`all 212 runs loaded at 14:05`) or only its newest page
//!   (`newest 100 runs only - r loads all`). A timer read that leaves a gap (a newest page with a next token that shares no run with the held list)
//!   takes the claim back, and `d` is refused until a full read has been applied.
//! * **Rows change under the overlay.** The 5 s timer lays new tags into the held rows; the index is built again only when a row changed (a
//!   10,000-row build costs up to 200 ms), the cursor stays on its tag text, or goes to the nearest row when that tag is gone. The picks stay.
//! * **Remove everywhere** sends the held runs that hold the tag in batches of 40 (`Effect::EditTags`, one effect for all batches, run one after
//!   the other; a failed request stops the rest). A run counts as changed when its answer is `ok` or `unchanged` and its tags no longer hold the
//!   tag; the held rows take the tags the answers carry. An answer that lands while a timer read is out would be overwritten by that older read, so
//!   the tags of the answers are laid over the first read that arrives after it.
//! * **`/tag`** (`tag_word`) parses the words of the line with `ham_core::runtag::edit::parse_tag_words`, refuses what the server would refuse (a tag
//!   added and removed), shows a preview (`registry::find("tag")` says it writes the service, so it asks for a second Enter) and sends one edit for the
//!   run on screen on the second Enter, 300 ms or more after the preview. The run screen draws the run's tags after its name (`chat.rs`) from the held
//!   row, read at every draw.

use super::*;
use crate::api::{TAGS_UNSUPPORTED, utc_rfc3339};
use crate::tui::mouse::{PaneId, Target, stable_id};
use ham_core::router::CONFIRM_DELAY_MS;
use ham_core::runfilter::FIRST_PAGE;
use ham_core::runtag::edit::{TagState, TagWords, apply_edit, parse_tag_words};
use ham_core::runtag::wire::{
    EditStatus, MAX_EDITS, TagEdit, TagEditRequest, TagEditResponse, TagEditResult,
};
use ham_core::tagexplore::{self, Held, TagIndex, View};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, Borders, Clear, Paragraph};
use std::hash::{Hash, Hasher};

/// The scrolling pane of the table (the Runs list has `PaneId(0)`).
const TAGS_PANE: PaneId = PaneId(1);
/// The longest find text a person can type or paste (the matcher cuts a query at its own limit too).
const FIND_MAX: usize = 120;
/// The widest the tag column goes, and the narrowest it starts at.
const TAG_MAX: usize = 48;
const TAG_MIN: usize = 20;
/// The prefix of a table row (cursor, pick, a space), the gap between columns, and the least width of the RUNS and LAST columns.
const PREFIX: usize = 3;
const GAP: usize = 1;
const COUNT_MIN: usize = 5;
/// From this screen width the table has the LAST column.
const LAST_FROM: u16 = 100;

/// A question waiting for `y`: remove `tag` from `ids`.
#[derive(Debug)]
struct Ask {
    tag: String,
    ids: Vec<String>,
}

/// A removal that has been sent and has not answered: how many runs it asked about.
#[derive(Debug)]
struct Sent {
    total: usize,
}

/// A `/tag` line that is previewed and waits for its second Enter.
#[derive(Clone, Debug)]
struct Confirm {
    run_id: String,
    words: TagWords,
    shown_ms: u64,
}

/// The state of the explorer and of the `/tag` preview.
#[derive(Debug, Default)]
pub(crate) struct TagExplorer {
    /// The overlay is open (the app reads it to route keys, the wheel and a paste).
    pub(super) open: bool,
    find: String,
    editing: bool,
    cursor: usize,
    cursor_tag: Option<String>,
    picks: Vec<String>,
    index: TagIndex,
    /// A hash of the rows the index was built from.
    fingerprint: u64,
    builds: u64,
    ask: Option<Ask>,
    sent: Option<Sent>,
    /// A one-line answer for the note row, until the next key.
    note: Option<String>,
    /// The time (`14:05`) a read of every page was applied; `None`: only the newest page is known.
    loaded: Option<String>,
    /// The tags of the runs an answer changed while a timer read was out, laid over that read when it arrives.
    pinned: HashMap<String, Vec<String>>,
    confirm: Option<Confirm>,
}

/// An index entry that holds nothing but the tags the answer of the edit route gave.
fn blank_entry(run_id: &str) -> IndexEntry {
    IndexEntry {
        run_id: run_id.to_string(),
        title: None,
        model: None,
        name: None,
        name_source: None,
        tags: None,
    }
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// `text` cut to `width` characters; every tag the explorer draws is printable ASCII (`tagexplore` leaves out anything else).
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// `text` in `width` columns: cut with a `…` as the last character when it is longer, padded with blanks when it is shorter.
fn cell(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n <= width {
        format!("{text}{}", " ".repeat(width - n))
    } else if width == 0 {
        String::new()
    } else {
        format!("{}\u{2026}", clip(text, width - 1))
    }
}

impl App {
    // ---- the explorer's state ---------------------------------------------------------------------------------------------------

    /// The held rows as the explorer reads them.
    fn tags_held(&self) -> Vec<Held> {
        self.runs_rows
            .iter()
            .map(|r| Held {
                run_id: r.run_id.clone(),
                created_at: r.created_at.clone(),
                tags: r.tags.clone(),
            })
            .collect()
    }

    /// A hash of the held rows' ids, times and tags: the index is built again when it changes.
    fn tags_fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for r in &self.runs_rows {
            r.run_id.hash(&mut h);
            r.created_at.hash(&mut h);
            r.tags.hash(&mut h);
        }
        h.finish()
    }

    /// Builds the index of the held rows when they differ from the ones it was built from (or `force`).
    fn tags_rebuild(&mut self, force: bool) -> bool {
        let fingerprint = self.tags_fingerprint();
        if !force && fingerprint == self.tags.fingerprint {
            return false;
        }
        self.tags.index = TagIndex::build(&self.tags_held());
        self.tags.fingerprint = fingerprint;
        self.tags.builds += 1;
        true
    }

    /// How many times the index of the explorer was built (tests: it is built again only when the held rows changed).
    #[doc(hidden)]
    pub fn tag_index_builds(&self) -> u64 {
        self.tags.builds
    }

    /// The moment as the table's ages read it (RFC 3339 UTC); empty before the loop has told the app the time (every age is then `-`).
    fn tags_now(&self) -> String {
        self.now.map(utc_rfc3339).unwrap_or_default()
    }

    fn tags_view(&self) -> View {
        self.tags
            .index
            .view(&self.tags.picks, &self.tags.find, &self.tags_now())
    }

    /// Puts the cursor back on its tag after the rows changed; when the tag is gone it goes to the nearest row (or the first, `reset`).
    fn tags_resync(&mut self, reset: bool) {
        let view = self.tags_view();
        if view.rows.is_empty() {
            self.tags.cursor = 0;
            self.tags.cursor_tag = None;
            return;
        }
        let at = self
            .tags
            .cursor_tag
            .as_ref()
            .and_then(|t| view.rows.iter().position(|r| &r.tag == t));
        let at = at.unwrap_or(if reset {
            0
        } else {
            self.tags.cursor.min(view.rows.len() - 1)
        });
        self.tags.cursor = at;
        self.tags.cursor_tag = Some(view.rows[at].tag.clone());
    }

    /// The held rows changed (a read of the list, an edit's answer): while the overlay is open the index follows.
    pub(crate) fn tags_rows_changed(&mut self) {
        if self.tags.open && self.tags_rebuild(false) {
            self.tags_resync(false);
        }
    }

    /// The clock of a read of every page: `14:05`, the first word of the reader's clock text.
    fn tags_clock(&self) -> String {
        self.now
            .map(|n| self.clock_text(n))
            .and_then(|t| t.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| "--:--".to_string())
    }

    /// A read of the list is about to be applied: every page is the whole list; the newest page with no next token is too; a newest page
    /// with a next token keeps the claim only when it shares a run with the held list (so nothing can be missing between them).
    pub(crate) fn tags_note_read(&mut self, read: &RunsRead) {
        match read {
            RunsRead::All(_) => self.tags.loaded = Some(self.tags_clock()),
            RunsRead::First { runs, has_more } => {
                if !has_more {
                    self.tags.loaded = Some(self.tags_clock());
                } else if self.tags.loaded.is_some() && !self.runs.is_empty() {
                    let held: HashSet<&str> = self.runs.iter().map(|r| r.run_id.as_str()).collect();
                    if !runs.iter().any(|r| held.contains(r.run_id.as_str())) {
                        self.tags.loaded = None;
                    }
                }
            }
        }
    }

    /// Every page of the list was read (`Msg::Runs`).
    pub(crate) fn tags_note_full_read(&mut self) {
        self.tags.loaded = Some(self.tags_clock());
    }

    /// Whether a timer read is out when an edit's answer lands: that read is older than the answer.
    fn tags_read_out(&self) -> bool {
        self.runs_inflight
    }

    /// Lays the tags an answer carried over the older read that arrives next: `entries` are the index entries of that read.
    pub(crate) fn tags_unpin(&mut self, entries: &mut Vec<IndexEntry>) {
        if self.tags.pinned.is_empty() {
            return;
        }
        let pinned = std::mem::take(&mut self.tags.pinned);
        for (run_id, tags) in pinned {
            match entries.iter_mut().find(|e| e.run_id == run_id) {
                Some(e) => e.tags = Some(tags),
                None => {
                    let mut e = blank_entry(&run_id);
                    e.tags = Some(tags);
                    entries.push(e);
                }
            }
        }
    }

    // ---- opening, closing, applying -----------------------------------------------------------------------------------------------

    /// `t` on the Runs list.
    pub(crate) fn tags_open(&mut self) -> Vec<Effect> {
        self.tags.open = true;
        self.tags.find.clear();
        self.tags.editing = false;
        self.tags.ask = None;
        self.tags.note = None;
        self.tags.picks = self.runs_filter.tags().to_vec();
        self.tags_rebuild(true);
        self.tags.cursor = 0;
        self.tags.cursor_tag = None;
        self.tags_resync(true);
        vec![]
    }

    pub(crate) fn tags_close(&mut self) {
        self.tags.open = false;
        self.tags.editing = false;
        self.tags.find.clear();
        self.tags.ask = None;
        self.tags.note = None;
    }

    /// `Enter`: the Runs list narrows to the picks (with no pick, to the tag under the cursor) and the overlay closes.
    pub(crate) fn tags_apply(&mut self) {
        let picks = if self.tags.picks.is_empty() {
            self.tags.cursor_tag.clone().into_iter().collect()
        } else {
            self.tags.picks.clone()
        };
        self.tags_close();
        if !picks.is_empty() {
            self.change_runs_filter(|f| f.set_tags(picks));
        }
    }

    fn tags_move(&mut self, by: isize) {
        let view = self.tags_view();
        if view.rows.is_empty() {
            return;
        }
        let at = self
            .tags
            .cursor
            .min(view.rows.len() - 1)
            .saturating_add_signed(by)
            .min(view.rows.len() - 1);
        self.tags.cursor = at;
        self.tags.cursor_tag = Some(view.rows[at].tag.clone());
    }

    /// The wheel and the mouse: the cursor goes to row `index` of the table.
    pub(crate) fn tags_select(&mut self, index: usize) {
        let view = self.tags_view();
        if let Some(row) = view.rows.get(index) {
            self.tags.cursor = index;
            self.tags.cursor_tag = Some(row.tag.clone());
        }
    }

    /// The wheel moves the cursor one row a notch.
    pub(crate) fn tags_wheel(&mut self, lines: i32) {
        self.tags_move(isize::try_from(lines.signum()).unwrap_or(0));
    }

    fn tags_toggle_pick(&mut self) {
        let Some(tag) = self.tags.cursor_tag.clone() else {
            return;
        };
        if let Some(at) = self.tags.picks.iter().position(|p| p == &tag) {
            self.tags.picks.remove(at);
        } else {
            self.tags.picks.push(tag);
        }
        self.tags_resync(false);
    }

    // ---- keys -----------------------------------------------------------------------------------------------------------------------

    /// A key while the overlay is open. It takes every key; Ctrl-C and Ctrl-L were handled before it.
    pub(crate) fn tags_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        self.tags.note = None;
        if self.tags.ask.is_some() {
            return self.tags_ask_key(key);
        }
        if self.tags.editing {
            self.tags_find_key(key);
            return vec![];
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc | KeyCode::Char('t') if plain => {
                if self.tags.find.is_empty() {
                    self.tags_close();
                } else {
                    self.tags.find.clear();
                    self.tags_resync(false);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if plain => self.tags_move(-1),
            KeyCode::Down | KeyCode::Char('j') if plain => self.tags_move(1),
            KeyCode::Char(' ') if plain => self.tags_toggle_pick(),
            KeyCode::Char('/') if plain => self.tags.editing = true,
            KeyCode::Enter => self.tags_apply(),
            KeyCode::Char('d') if plain => self.tags_remove_start(),
            KeyCode::Char('r') if plain => {
                if self.tags.sent.is_some() {
                    self.tags.note = Some("wait for the removal that is running".to_string());
                } else {
                    return vec![Effect::LoadRuns];
                }
            }
            _ => {}
        }
        vec![]
    }

    /// A key while the find line is open: every character is its text.
    fn tags_find_key(&mut self, key: &KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.tags.find.clear();
                self.tags.editing = false;
            }
            KeyCode::Enter => self.tags.editing = false,
            KeyCode::Backspace => {
                self.tags.find.pop();
            }
            KeyCode::Char('u') if ctrl => self.tags.find.clear(),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if self.tags.find.chars().count() < FIND_MAX {
                    self.tags.find.push(c);
                    self.tags.find = sanitize_line(&self.tags.find);
                }
            }
            _ => return,
        }
        self.tags_resync(true);
    }

    /// A paste while the overlay is open: the find line takes it as one line of text, nothing else does.
    pub(crate) fn tags_paste(&mut self, text: &str) {
        if !self.tags.editing {
            return;
        }
        let one_line = sanitize_line(&text.replace(['\n', '\r'], " "));
        let room = FIND_MAX.saturating_sub(self.tags.find.chars().count());
        self.tags.find.push_str(&clip(&one_line, room));
        self.tags_resync(true);
    }

    // ---- remove everywhere ---------------------------------------------------------------------------------------------------------

    fn tags_remove_start(&mut self) {
        if self.tags.sent.is_some() {
            self.tags.note = Some("wait for the removal that is running".to_string());
            return;
        }
        if self.tags.loaded.is_none() {
            self.tags.note =
                Some("remove needs every run loaded: press r to load them all, then d".to_string());
            return;
        }
        let Some(tag) = self.tags.cursor_tag.clone() else {
            return;
        };
        let ids = tagexplore::remove_plan(&tag, &self.tags_held());
        if ids.is_empty() {
            self.tags.note = Some(format!("no held run has {}", sanitize_line(&tag)));
            return;
        }
        self.tags.ask = Some(Ask { tag, ids });
    }

    /// A key while the question waits: `y` sends; any other key withdraws it and is consumed.
    fn tags_ask_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let Some(ask) = self.tags.ask.take() else {
            return vec![];
        };
        let yes = matches!(key.code, KeyCode::Char('y' | 'Y'))
            && !key.modifiers.contains(KeyModifiers::CONTROL);
        if !yes {
            return vec![];
        }
        let batches = tagexplore::batches(&ask.ids, MAX_EDITS)
            .into_iter()
            .map(|ids| TagEditRequest {
                edits: ids
                    .into_iter()
                    .map(|run_id| TagEdit {
                        run_id,
                        add: Vec::new(),
                        remove: vec![ask.tag.clone()],
                    })
                    .collect(),
            })
            .collect();
        self.tags.note = Some(format!(
            "removing {} from {} {}...",
            sanitize_line(&ask.tag),
            ask.ids.len(),
            plural(ask.ids.len(), "run", "runs")
        ));
        self.tags.sent = Some(Sent {
            total: ask.ids.len(),
        });
        vec![Effect::EditTags {
            origin: TagsOrigin::Remove { tag: ask.tag },
            batches,
        }]
    }

    // ---- the answers of the edit route ---------------------------------------------------------------------------------------------

    /// The answer to [`Effect::EditTags`].
    pub(crate) fn tags_edited(
        &mut self,
        origin: TagsOrigin,
        results: Vec<Result<TagEditResponse, String>>,
    ) -> Vec<Effect> {
        match origin {
            TagsOrigin::Remove { tag } => self.tags_removed(&tag, results),
            TagsOrigin::Word { run_id } => self.tags_worded(&run_id, results),
        }
        vec![]
    }

    /// Lays the tags of answered edits into the held rows (and over the older read that is still out).
    fn tags_take(&mut self, updates: Vec<(String, Vec<String>)>) {
        if updates.is_empty() {
            return;
        }
        let read_out = self.tags_read_out();
        for (run_id, tags) in updates {
            if read_out {
                self.tags.pinned.insert(run_id.clone(), tags.clone());
            }
            self.run_index
                .entry(run_id.clone())
                .or_insert_with(|| blank_entry(&run_id))
                .tags = Some(tags);
        }
        self.rebuild_rows();
        self.tags_rows_changed();
    }

    fn tags_removed(&mut self, tag: &str, results: Vec<Result<TagEditResponse, String>>) {
        let total = self
            .tags
            .sent
            .take()
            .map_or_else(|| results.len(), |s| s.total);
        let mut cleared = 0usize;
        let mut others: Vec<(EditStatus, usize)> = Vec::new();
        let mut error: Option<String> = None;
        let mut updates: Vec<(String, Vec<String>)> = Vec::new();
        for result in results {
            match result {
                Err(e) => {
                    error.get_or_insert(e);
                }
                Ok(resp) => {
                    for r in resp.results {
                        match r.status {
                            EditStatus::Ok | EditStatus::Unchanged => {
                                if !r.tags.iter().any(|t| t == tag) {
                                    cleared += 1;
                                }
                                updates.push((r.run_id, r.tags));
                            }
                            other => match others.iter_mut().find(|(s, _)| *s == other) {
                                Some((_, n)) => *n += 1,
                                None => others.push((other, 1)),
                            },
                        }
                    }
                }
            }
        }
        self.tags_take(updates);
        // A picked tag that no held run holds any more would leave the Runs list empty with nothing to show why.
        if !self
            .tags_held()
            .iter()
            .any(|h| h.tags.iter().any(|t| t == tag))
        {
            self.tags.picks.retain(|p| p != tag);
            self.tags_resync(false);
        }
        let shown = sanitize_line(tag);
        let note = if error.as_deref() == Some(TAGS_UNSUPPORTED) && cleared == 0 {
            TAGS_UNSUPPORTED.to_string()
        } else if cleared == total && error.is_none() && others.is_empty() {
            format!(
                "removed {shown} from {total} {}",
                plural(total, "run", "runs")
            )
        } else {
            let mut note = format!("{cleared} of {total} runs changed");
            if !others.is_empty() {
                let words: Vec<String> = others
                    .iter()
                    .map(|(s, n)| format!("{n} {}", status_word(*s)))
                    .collect();
                note.push_str(&format!(" ({})", words.join(", ")));
            }
            if let Some(e) = error {
                note.push_str(&format!(" \u{b7} {}", sanitize_line(&e)));
            }
            note
        };
        self.tags.note = Some(note);
    }

    fn tags_worded(&mut self, run_id: &str, results: Vec<Result<TagEditResponse, String>>) {
        let result: Option<TagEditResult> = match results.into_iter().next() {
            Some(Ok(resp)) => resp.results.into_iter().find(|r| r.run_id == run_id),
            Some(Err(e)) => {
                self.say(&sanitize_line(&e));
                return;
            }
            None => None,
        };
        let Some(result) = result else {
            self.say("the service answered without a result for this run");
            return;
        };
        match result.status {
            EditStatus::Ok | EditStatus::Unchanged => {
                let now = if result.tags.is_empty() {
                    "none".to_string()
                } else {
                    result
                        .tags
                        .iter()
                        .map(|t| sanitize_line(t))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let said = if result.status == EditStatus::Unchanged {
                    format!("no change \u{b7} tags now: {now}")
                } else {
                    format!("tags now: {now}")
                };
                self.tags_take(vec![(run_id.to_string(), result.tags)]);
                self.say(&said);
            }
            EditStatus::Invalid => {
                let why = result.message.unwrap_or_default();
                self.panel = Some(Panel::not_run(
                    "Nothing was changed",
                    if why.trim().is_empty() {
                        "the service refused the tags"
                    } else {
                        &why
                    },
                ));
            }
            EditStatus::NotFound => {
                self.say("run not found: the service has no run with this id for you");
            }
            EditStatus::Conflict => {
                self.say("the tags changed while the edit was sent: try again");
            }
        }
    }

    // ---- /tag ---------------------------------------------------------------------------------------------------------------------------

    /// The effective tags of the held row of `run_id`, for the name row of the run screen (empty without a held row).
    pub(crate) fn run_tags_shown(&self, run_id: &str) -> Vec<String> {
        self.runs_rows
            .iter()
            .find(|r| r.run_id == run_id)
            .map(|r| r.tags.clone())
            .unwrap_or_default()
    }

    /// `/tag +kind:value -kind:value word`: parsed, checked against the server's rules, previewed; the second Enter sends it.
    pub(crate) fn tag_word(&mut self, args: &[String]) -> Vec<Effect> {
        self.tags.confirm = None;
        let usage = |app: &mut App, why: Option<&str>| {
            let mut text = String::new();
            if let Some(why) = why {
                text.push_str(why);
                text.push('\n');
            }
            text.push_str(
                "usage: /tag <+kind:value | -kind:value | word>...\n+ or a bare word adds a tag, - removes one; a word without a kind is a topic.\nKinds: project, topic, tech. Example: /tag +project:billing -tech:s3 csv-export",
            );
            app.panel = Some(Panel::not_run("Nothing was sent", &text));
        };
        let Some(run_id) = self.focused_run().map(str::to_string) else {
            usage(
                self,
                Some("open a run first: /tag changes the tags of the run on screen"),
            );
            return vec![];
        };
        let line = args.join(" ");
        if line.trim().is_empty() {
            usage(self, None);
            return vec![];
        }
        let words = match parse_tag_words(&line) {
            Ok(w) => w,
            Err(reason) => {
                self.panel = Some(Panel::not_run("Nothing was sent", reason.message()));
                return vec![];
            }
        };
        // The server's rules, tried on an empty run: a tag added and removed, more tags than an edit takes, a tag the grammar refuses.
        if let Err(e) = apply_edit(&TagState::default(), "", &words.add, &words.remove) {
            self.panel = Some(Panel::not_run("Nothing was sent", &e.message()));
            return vec![];
        }
        let confirm = Confirm {
            run_id: run_id.clone(),
            words,
            shown_ms: self.key_ms,
        };
        // A word that writes the service asks for a second Enter (the class the registry gives it).
        if !registry::find("tag").is_none_or(|e| e.class().needs_confirm()) {
            return self.tags_send_word(&confirm);
        }
        let mut text = format!("Change the tags of run {}?", sanitize_line(&run_id));
        if !confirm.words.add.is_empty() {
            text.push_str(&format!("\nAdd: {}", confirm.words.add.join(" ")));
        }
        if !confirm.words.remove.is_empty() {
            text.push_str(&format!("\nRemove: {}", confirm.words.remove.join(" ")));
        }
        text.push_str("\nEnter applies it \u{b7} Esc cancels");
        self.panel = Some(Panel::info("Confirm", &text));
        self.tags.confirm = Some(confirm);
        vec![]
    }

    /// The keys while a `/tag` preview waits (typing mode on the run screen): Enter after the delay sends it, Esc withdraws it, any other key
    /// withdraws it and is the box's. A preview does not outlive the run it was made for.
    pub(crate) fn tags_confirm_key(&mut self, key: &KeyEvent) -> Option<Vec<Effect>> {
        let confirm = self.tags.confirm.clone()?;
        if self.screen != Screen::Dashboard || self.focused_run() != Some(confirm.run_id.as_str()) {
            self.tags.confirm = None;
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                if !self.calm || self.key_ms.saturating_sub(confirm.shown_ms) < CONFIRM_DELAY_MS {
                    return Some(vec![]);
                }
                self.tags.confirm = None;
                self.panel = None;
                self.clear_chat();
                Some(self.tags_send_word(&confirm))
            }
            KeyCode::Esc => {
                self.tags.confirm = None;
                self.panel = None;
                Some(vec![])
            }
            _ => {
                self.tags.confirm = None;
                self.panel = None;
                None
            }
        }
    }

    fn tags_send_word(&mut self, confirm: &Confirm) -> Vec<Effect> {
        self.say("changing tags...");
        vec![Effect::EditTags {
            origin: TagsOrigin::Word {
                run_id: confirm.run_id.clone(),
            },
            batches: vec![TagEditRequest {
                edits: vec![TagEdit {
                    run_id: confirm.run_id.clone(),
                    add: confirm.words.add.clone(),
                    remove: confirm.words.remove.clone(),
                }],
            }],
        }]
    }

    // ---- drawing ------------------------------------------------------------------------------------------------------------------------

    /// The explorer over the whole frame (design 9.2 and 9.3): the title row, a box with the table (a header row and one row for each tag), the
    /// note row and the key line. The version tag is drawn over row 0 afterwards, as on every screen.
    pub(crate) fn draw_tags(&self, f: &mut Frame, split: &crate::tui::head::Split) {
        if !self.tags.open {
            return;
        }
        // The overlay shields the whole frame; it lays itself out below the title band when there is one (`head.rs`).
        let frame = f.area();
        f.render_widget(Clear, frame);
        let area = split.area;
        if area.width == 0 || area.height < 4 {
            return;
        }
        let [title, body, note, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(Line::from("ham \u{b7} Tags"))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            split.title_over(title),
        );
        let view = self.tags_view();
        let snapshot = match &self.tags.loaded {
            Some(at) => format!(
                "all {} {} loaded at {at}",
                view.held,
                plural(view.held, "run", "runs")
            ),
            None => format!("newest {FIRST_PAGE} runs only - r loads all"),
        };
        let block = UiBlock::default()
            .borders(Borders::ALL)
            .title(format!(" tags \u{b7} {snapshot} "));
        let inner = block.inner(body);
        f.render_widget(block, body);
        {
            // The overlay shields what lies under it; the table is a scrolling pane and each tag row a region of the list.
            let mut map = self.hits.borrow_mut();
            map.push(frame, Target::Block, "Esc");
            map.push(body, Target::Scroll(TAGS_PANE), "\u{2191}\u{2193}");
        }
        if inner.width > 0 && inner.height > 0 {
            self.draw_tags_table(f, inner, &view);
        }
        let note_text = self.tags_note(&view);
        f.render_widget(
            Paragraph::new(clip(&note_text, usize::from(note.width))),
            note,
        );
        if self.tags.editing {
            let text = format!("/{}", sanitize_line(&self.tags.find));
            f.render_widget(Paragraph::new(clip(&text, usize::from(help.width))), help);
            let col = u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX);
            f.set_cursor_position((help.x + col.min(help.width.saturating_sub(1)), help.y));
        } else {
            let line = self.tags_help(&view);
            f.render_widget(Paragraph::new(clip(&line, usize::from(help.width))), help);
            self.push_chips(&line, help);
        }
    }

    /// The line under the table: the question, a one-key answer, or the counts of the picks.
    fn tags_note(&self, view: &View) -> String {
        if let Some(ask) = &self.tags.ask {
            return format!(
                "Remove {} from {} {}? y/n",
                sanitize_line(&ask.tag),
                ask.ids.len(),
                plural(ask.ids.len(), "run", "runs")
            );
        }
        if let Some(note) = &self.tags.note {
            return sanitize_line(note);
        }
        let mut text = tagexplore::note_text(view, &self.tags.picks);
        if !self.tags.find.trim().is_empty() {
            text.push_str(&format!(
                " \u{b7} find \"{}\"",
                sanitize_line(&self.tags.find)
            ));
        }
        text
    }

    /// The key line under the note.
    fn tags_help(&self, view: &View) -> String {
        if self.tags.ask.is_some() {
            return "y remove \u{b7} n cancel".to_string();
        }
        if view.rows.is_empty() && self.tags.find.trim().is_empty() && self.tags.picks.is_empty() {
            return "Esc back".to_string();
        }
        let last = if self.tags.find.trim().is_empty() {
            "Esc back"
        } else {
            "Esc clears the find"
        };
        format!(
            "Space pick \u{b7} Enter show runs \u{b7} / find \u{b7} d remove \u{b7} r reload \u{b7} {last}"
        )
    }

    /// The header and the rows of the table in `inner` (the box without its border), the cursor row marked and the pick marks.
    fn draw_tags_table(&self, f: &mut Frame, inner: Rect, view: &View) {
        let width = usize::from(inner.width);
        let show_last = f.area().width >= LAST_FROM;
        let runs_w = view
            .rows
            .iter()
            .map(|r| r.runs.to_string().len())
            .max()
            .unwrap_or(0)
            .max(COUNT_MIN);
        let last_w = view
            .rows
            .iter()
            .map(|r| r.last.len())
            .max()
            .unwrap_or(0)
            .max(COUNT_MIN);
        let fixed = PREFIX + GAP + runs_w + if show_last { GAP + last_w } else { 0 };
        let room = width.saturating_sub(fixed);
        let longest = view
            .rows
            .iter()
            .map(|r| r.tag.chars().count())
            .max()
            .unwrap_or(0);
        let tag_w = longest.clamp(TAG_MIN, TAG_MAX).min(room);
        let line_of = |prefix: &str, tag: &str, runs: &str, last: &str| -> String {
            let mut row = format!("{prefix}{} {runs:>runs_w$}", cell(tag, tag_w));
            if show_last {
                row.push_str(&format!(" {last:>last_w$}"));
            }
            clip(&row, width)
        };
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut lines: Vec<Line> = vec![Line::styled(line_of("   ", "TAG", "RUNS", "LAST"), bold)];
        let capacity = usize::from(inner.height).saturating_sub(1);
        if view.rows.is_empty() {
            lines.extend(self.tags_empty_lines(width));
        }
        let skip = (self.tags.cursor + 1).saturating_sub(capacity);
        let mut map = self.hits.borrow_mut();
        for (i, row) in view.rows.iter().enumerate().skip(skip).take(capacity) {
            let at_cursor = i == self.tags.cursor;
            let picked = self.tags.picks.iter().any(|p| p == &row.tag);
            let prefix = format!(
                "{}{} ",
                if at_cursor { "\u{25b6}" } else { " " },
                if picked { "+" } else { " " }
            );
            let text = line_of(&prefix, &row.tag, &row.runs.to_string(), &row.last);
            let style = if at_cursor { bold } else { Style::default() };
            lines.push(Line::styled(text, style));
            let y = inner
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(i - skip).unwrap_or(u16::MAX));
            map.push(
                Rect::new(inner.x, y, inner.width, 1),
                Target::Row {
                    list: TAGS_LIST,
                    index: i,
                    id: stable_id(&row.tag),
                },
                "Enter",
            );
        }
        drop(map);
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// What the table says when it has no row: no tag anywhere (design 9.3), or no tag that the find line lets through.
    fn tags_empty_lines(&self, width: usize) -> Vec<Line<'static>> {
        let say = |text: &str| Line::from(clip(&format!("{}{text}", " ".repeat(PREFIX)), width));
        if self.tags.picks.is_empty() && self.tags.find.trim().is_empty() {
            vec![
                Line::from(""),
                say("No tags yet."),
                say("New runs get topic and tech tags a few seconds after they start."),
                say("A run on a repository shows project:<repository> at once."),
                say("To add a tag by hand, open a run and type /tag +topic:name."),
            ]
        } else {
            vec![
                Line::from(""),
                say(&format!(
                    "No tag matches \"{}\".",
                    sanitize_line(&self.tags.find)
                )),
            ]
        }
    }
}

/// The word a person reads for an edit status that did not change the run.
fn status_word(status: EditStatus) -> &'static str {
    match status {
        EditStatus::Ok => "ok",
        EditStatus::Unchanged => "unchanged",
        EditStatus::Invalid => "invalid",
        EditStatus::NotFound => "not found",
        EditStatus::Conflict => "conflict",
    }
}
