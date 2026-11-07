//! The model in the title and the status card never depends on one event (live proof A1, textstats leg: the card lacked the model name in 1 of 10
//! legs because `session.started` was missed). A child module of `app` (it reads the app's private state). A session that names its model
//! wins; until one does, the model the person asked for in the composer is shown, else the model the run index holds for the run (the list
//! route's rows carry it), and a run that shows events and no model reads the list once to get it, again after a long pause if it is still
//! missing. Nothing here writes a model: it only chooses which known id to show.

use super::*;

/// The pause between two reads of the list made to find a model.
const PROBE_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(crate) struct ModelTitle {
    /// The model on the composer's screen when the last start went out.
    pub(crate) asked_now: Option<String>,
    /// The model each run was started with from this app, by run id.
    pub(crate) asked: std::collections::HashMap<String, String>,
    /// The earliest the list may be read again to find a model.
    next_probe: Duration,
}

impl App {
    /// The id to show for the run when its session has not named a model: the one asked for, else the run index's.
    pub(crate) fn model_fallback(&self, run_id: &str) -> Option<&str> {
        self.titled
            .asked
            .get(run_id)
            .map(String::as_str)
            .or_else(|| self.run_index.get(run_id).and_then(|e| e.model.as_deref()))
            .filter(|m| !m.trim().is_empty())
    }

    /// The composer sent a start with `model`.
    pub(crate) fn model_asked(&mut self, model: Option<String>) {
        self.titled.asked_now = model;
    }

    /// The start was accepted: `run_id` is the run that model was asked for.
    pub(crate) fn model_started(&mut self, run_id: &str) {
        if let Some(m) = self.titled.asked_now.take() {
            self.titled.asked.insert(run_id.to_string(), m);
        }
    }

    /// The 1 s timer: a run that shows events and no model, and has no fallback, reads the list once (its rows carry the model).
    pub(crate) fn model_probe(&mut self) -> Vec<Effect> {
        if self.screen != Screen::Dashboard
            || self.runs_inflight
            || self.clock < self.titled.next_probe
        {
            return vec![];
        }
        let Some(v) = self.cached.as_ref() else {
            return vec![];
        };
        if v.events == 0
            || v.direct_model_id().is_some()
            || self.model_fallback(&v.run_id).is_some()
        {
            return vec![];
        }
        self.titled.next_probe = self.clock + PROBE_EVERY;
        self.runs_inflight = true;
        vec![Effect::RefreshRuns(Refresh::Timer)]
    }
}
