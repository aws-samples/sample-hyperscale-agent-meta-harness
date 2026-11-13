//! The name of the run on screen. A child module of `app` (it reads the app's private state).
//!
//! The run screen shows the name of its run on a row under the header. The name comes from the first of these that has one: a model's name
//! (a read of the run's row, or the held Runs list), else the name the held list carries (a seed when no model has written one yet), else the
//! seed this app computed from the prompt it sent when the run was started from the composer. A seed is built from the prompt head with no
//! model (`ham_core::runname::seed_name`, the same function the server uses to name a run with no stored name), so the screen has a name the
//! moment a run starts.
//!
//! A model writes its name a few seconds after the run exists. While the shown name is not a model's, the screen reads the run's row: once
//! when the run is opened, and then every [`POLL_EVERY`] ticks while the run is under [`WATCH_SECS`] seconds old. The read stops at the first
//! model name. A reply for a run that is no longer on screen changes nothing, a failed read keeps what is shown, and nothing here writes: the
//! name is display text only, never an input of anything the app sends.

use super::*;
use ham_core::run_index::NAME_SOURCE_AI;

/// The ticks (seconds) between two reads of the run's row.
pub(crate) const POLL_EVERY: u8 = 5;
/// A run is followed on the timer while it is younger than this many seconds.
pub(crate) const WATCH_SECS: i64 = 60;

/// A name and where it came from (`ai`, `seed`, or another word of a newer writer).
type Named = (String, Option<String>);

/// What the app knows about the names of runs.
#[derive(Debug, Default)]
pub(crate) struct RunNames {
    /// The seed of the prompt the composer just sent, until the start is answered.
    asked_now: Option<String>,
    /// The seed of the prompt of each run this app started, by run id.
    started: HashMap<String, String>,
    /// What the last read of each run's row said, by run id. A model's name is never replaced by another source.
    read: HashMap<String, Named>,
    /// Ticks since the focused run was opened or its row was last asked for.
    ticks: u8,
    /// The app's clock (a tick counter) when the focused run was opened: the age of a run the list does not hold.
    opened_at: Duration,
    /// The run a read is out for.
    inflight: Option<String>,
}

fn is_ai(named: &Named) -> bool {
    named.1.as_deref() == Some(NAME_SOURCE_AI)
}

impl App {
    /// The composer sent a start with `prompt`: its seed is the name of the run once the start is answered.
    pub(crate) fn name_asked(&mut self, prompt: &str) {
        let (head, _) = ham_core::run_index::prompt_head(prompt);
        self.names.asked_now =
            Some(ham_core::runname::seed_name(&head)).filter(|s| !s.trim().is_empty());
    }

    /// The start was accepted: `run_id` is the run that prompt started.
    pub(crate) fn name_started(&mut self, run_id: &str) {
        if let Some(seed) = self.names.asked_now.take() {
            self.names.started.insert(run_id.to_string(), seed);
        }
    }

    /// The name the screen shows for `run_id` and whether a model wrote it: a model's name from any source wins; otherwise the read of the
    /// run's row, then the held list, then the seed of the prompt this app sent.
    pub(crate) fn run_name_shown(&self, run_id: &str) -> Option<(String, bool)> {
        let read = self.names.read.get(run_id).cloned();
        let held = self
            .runs_rows
            .iter()
            .find(|r| r.run_id == run_id)
            .and_then(|r| Some((r.name.clone()?, r.name_source.clone())));
        let seed = self.names.started.get(run_id).map(|s| {
            (
                s.clone(),
                Some(ham_core::run_index::NAME_SOURCE_SEED.to_string()),
            )
        });
        let candidates = [read, held, seed];
        let best = candidates
            .iter()
            .flatten()
            .find(|c| is_ai(c))
            .or_else(|| candidates.iter().flatten().next())?;
        Some((best.0.clone(), is_ai(best)))
    }

    /// The run was opened on the dashboard: the clock for its age starts, and one read asks for its row unless a model's name is known.
    pub(crate) fn name_open(&mut self, run_id: &str) -> Vec<Effect> {
        self.names.ticks = 0;
        self.names.opened_at = self.clock;
        self.names.inflight = None;
        if self.run_name_shown(run_id).is_some_and(|(_, ai)| ai) {
            return vec![];
        }
        self.names.inflight = Some(run_id.to_string());
        vec![Effect::ReadRunName {
            run_id: run_id.to_string(),
        }]
    }

    /// The 1 s timer on the dashboard: the fifth tick since the last read asks again while the name is not a model's, no read is out and the
    /// run is under a minute old.
    pub(crate) fn run_name_tick(&mut self) -> Vec<Effect> {
        if self.screen != Screen::Dashboard || self.names.inflight.is_some() {
            return vec![];
        }
        let Some(run_id) = self.focused_run().map(str::to_string) else {
            return vec![];
        };
        if self.run_name_shown(&run_id).is_some_and(|(_, ai)| ai) || !self.run_is_young(&run_id) {
            return vec![];
        }
        self.names.ticks = self.names.ticks.saturating_add(1);
        if self.names.ticks < POLL_EVERY {
            return vec![];
        }
        self.names.ticks = 0;
        self.names.inflight = Some(run_id.clone());
        vec![Effect::ReadRunName { run_id }]
    }

    /// Whether the run is under [`WATCH_SECS`] seconds old: by its `createdAt` when the list or the detail holds it and the wall clock is
    /// known, else by the ticks since the screen opened.
    fn run_is_young(&self, run_id: &str) -> bool {
        let created = self
            .runs
            .iter()
            .find(|r| r.run_id == run_id)
            .map(|r| r.created_at.as_str())
            .or_else(|| {
                self.detail
                    .as_ref()
                    .filter(|d| d.summary.run_id == run_id)
                    .map(|d| d.summary.created_at.as_str())
            })
            .and_then(parse_utc_unix);
        match (created, self.now) {
            (Some(created), Some(now)) => now.saturating_sub(created) < WATCH_SECS,
            _ => {
                self.clock.saturating_sub(self.names.opened_at)
                    < Duration::from_secs(WATCH_SECS.unsigned_abs())
            }
        }
    }

    /// The answer to a read of the row of `run_id`: the name and its source, or nothing (a failed read, no row, no name). It changes
    /// nothing when that run is not the one on screen. A model's name is never taken from the screen by a seed.
    pub(crate) fn name_read(&mut self, run_id: &str, name: Option<Named>) {
        if self.names.inflight.as_deref() == Some(run_id) {
            self.names.inflight = None;
        }
        if self.focused_run() != Some(run_id) {
            return;
        }
        let Some((text, source)) = name.filter(|(t, _)| !t.trim().is_empty()) else {
            return;
        };
        let new = (text, source);
        if self.names.read.get(run_id).is_some_and(is_ai) && !is_ai(&new) {
            return;
        }
        self.names.read.insert(run_id.to_string(), new);
    }
}
