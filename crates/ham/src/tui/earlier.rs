//! The prompts of earlier runs in the history of the message box. A child module of `app` (it reads and writes the app's private
//! state); the pure parts are `ham_core::history`.
//!
//! * **Feed.** The Runs list rows carry the run index's `title` (the first line of the prompt, cut at 80 characters). After every read of the
//!   list, and when another run is opened, the history is given one `Earlier` for each run that has a title, newest first, without the run on
//!   screen (its own messages are the history's other tier). A chat opened without the Runs screen (`ham watch`) reads the list once
//!   ([`App::load_earlier_runs`]).
//! * **Recall.** Up and Down reach these entries after the run's own messages, and Ctrl-R searches them. A title is a short form, so a recall
//!   of one asks for the run's detail (`Job::FullPrompt`, once for each run while the answer is out) and starts a wait.
//! * **The wait.** While the full prompt is on its way Enter sends nothing and says why. The full prompt replaces the title only if the box
//!   still holds it exactly (an edit ends the walk, so a late answer changes nothing). The wait ends when the prompt arrives, when the fetch
//!   fails, when the stage returns a text that is not that run's prompt, and after [`FULL_WAIT`] at the latest; each of the last three says so
//!   on the notice row, so the box cannot hang.
//! * **No file.** Everything is in memory (invariant 5); this file names no file function, and a test reads it to prove that.

use super::*;
use ham_core::history::Earlier;

/// The longest the box waits for the full prompt of a recalled earlier run.
const FULL_WAIT: Duration = Duration::from_secs(10);

/// The words of the notice while Enter waits.
const WAITING: &str =
    "Fetching the full prompt of that run: Enter works when it arrives (Esc puts your draft back).";

/// The words of the notice when Enter is pressed on a short form after the wait ended: the first Enter asks, the second sends.
const CONFIRM: &str = "Only the first line of that prompt is in the box. Enter again sends it as it is; edit it or press Esc to change it.";

#[derive(Debug, Default)]
pub(crate) struct EarlierRuns {
    /// The runs whose full prompt has been asked for and not yet answered.
    fetching: HashSet<String>,
    /// When the wait for the recalled run's full prompt ends.
    wait_ends: Option<Duration>,
    /// The box holds this short form of a prompt, because the wait for the full one ended without it: the first Enter asks, the second
    /// sends it as it is. Cleared by any change of the box.
    short_form: Option<String>,
    /// The first Enter on that short form has been refused with the question.
    short_form_asked: bool,
}

impl App {
    /// A chat opened without the Runs screen asks for the list once, so that the history has the prompts of earlier runs. Nothing when a list
    /// is held or a read is out.
    pub fn load_earlier_runs(&mut self) -> Vec<Effect> {
        if !self.runs.is_empty() || self.runs_inflight {
            return vec![];
        }
        self.runs_inflight = true;
        vec![Effect::RefreshRuns(Refresh::Timer)]
    }

    /// Another run is on screen: the wait for a full prompt belonged to the run that was (its box is not this one), and the earlier runs are
    /// listed again without the new run.
    pub(crate) fn earlier_open(&mut self) {
        self.history.forget_pending();
        self.earlier.wait_ends = None;
        self.earlier.short_form = None;
        self.earlier_feed();
    }

    /// The history's earlier runs from the list read so far: every run with a title except the one on screen, newest first.
    pub(crate) fn earlier_feed(&mut self) {
        let focused = self.focused_run().map(str::to_string);
        let mut rows: Vec<(&RunSummary, &str)> = self
            .runs
            .iter()
            .filter(|r| Some(r.run_id.as_str()) != focused.as_deref())
            .filter_map(|r| {
                let title = self.run_index.get(&r.run_id)?.title.as_deref()?;
                Some((r, title))
            })
            .collect();
        // The list is newest first already; the stable sort keeps its order for runs of the same second and puts a time that cannot be read last.
        rows.sort_by_key(|(r, _)| std::cmp::Reverse(parse_utc_unix(&r.created_at)));
        let earlier: Vec<Earlier> = rows
            .into_iter()
            .map(|(r, title)| Earlier::from_title(&r.run_id, title))
            .filter(|e| !e.text.is_empty())
            .collect();
        self.history.set_earlier_runs(earlier);
    }

    /// A recall that holds only the short form of an earlier run's prompt: asks for the full prompt (once for each run while the answer is
    /// out) and starts the wait. Any other recall ends the wait.
    pub(crate) fn earlier_wait(&mut self, recall: &Recall) -> Vec<Effect> {
        self.earlier.short_form = None;
        let Some(run_id) = recall.fetch_full.as_ref() else {
            self.earlier.wait_ends = None;
            return vec![];
        };
        self.earlier.wait_ends = Some(self.clock + FULL_WAIT);
        if self.earlier.fetching.insert(run_id.clone()) {
            vec![Effect::Ext(Job::FullPrompt {
                run_id: run_id.clone(),
            })]
        } else {
            vec![]
        }
    }

    /// Enter while the box holds a short form: nothing is sent, and the notice says why. `true` when it was refused. While the full prompt
    /// is on its way Enter waits for it. After the wait ended without it, the first Enter asks and the second sends the short form as it is,
    /// so that a stray Enter cannot send the first line of a prompt as if it were the whole of it; a change of the box ends the question.
    pub(crate) fn earlier_blocks_send(&mut self) -> bool {
        if self.history.pending_full().is_some() {
            self.say(WAITING);
            return true;
        }
        let Some(short) = self.earlier.short_form.clone() else {
            return false;
        };
        if self.chat_text().as_deref() != Some(short.as_str()) {
            self.earlier.short_form = None;
            return false;
        }
        if self.earlier.short_form_asked {
            self.earlier.short_form = None;
            return false;
        }
        self.earlier.short_form_asked = true;
        self.say(CONFIRM);
        true
    }

    /// The wait is over without a full prompt: Enter works again, and the notice says that the box holds only the first line (the most
    /// important words come first: the notice row cuts a long line at the right).
    fn earlier_give_up(&mut self, why: &str) {
        // A request that is still out is not waited for again: the next recall of that run asks anew.
        if let Some(run) = self.history.pending_full().map(str::to_string) {
            self.earlier.fetching.remove(&run);
        }
        self.history.forget_pending();
        self.earlier.wait_ends = None;
        self.earlier.short_form = self.chat_text();
        self.earlier.short_form_asked = false;
        self.say(&format!(
            "Only the first line of that prompt is in the box: {why}. Edit it, or press Esc, before you send."
        ));
    }

    /// The 1 s timer: the wait for a full prompt ends after [`FULL_WAIT`].
    pub(crate) fn earlier_tick(&mut self) {
        let Some(end) = self.earlier.wait_ends else {
            return;
        };
        if self.history.pending_full().is_none() {
            self.earlier.wait_ends = None;
        } else if self.clock >= end {
            self.earlier_give_up(&format!(
                "its full prompt did not arrive in {} s",
                FULL_WAIT.as_secs()
            ));
        }
    }

    /// The answer to `Job::FullPrompt`.
    pub(crate) fn earlier_full_done(
        &mut self,
        run_id: &str,
        result: Result<String, String>,
    ) -> Vec<Effect> {
        self.earlier.fetching.remove(run_id);
        let waiting = self.history.pending_full() == Some(run_id);
        match result {
            Ok(full) => match self.history.supply_full(run_id, &full) {
                Some(recall) => {
                    // The box is replaced only while it still holds the short form that was recalled.
                    if self.expanded_chat_text().as_deref() == recall.replaces.as_deref() {
                        self.put_recall(&recall);
                    }
                    if self.history.pending_full().is_none() {
                        self.earlier.wait_ends = None;
                    }
                }
                None if waiting => self
                    .earlier_give_up("the stage returned another full prompt than the list shows"),
                None => {}
            },
            Err(why) if waiting => self.earlier_give_up(&format!(
                "its full prompt could not be fetched ({})",
                sanitize_line(&why)
            )),
            Err(_) => {}
        }
        vec![]
    }
}
