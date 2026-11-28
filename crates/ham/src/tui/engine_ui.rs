//! The engine-neutral screen: what the run screen shows of the engine comes from the capability
//! record of the focused run (`ham_core::engines`), never from an assumption about which engine it is. A child module of `app`.
//!
//! - **Popup.** The built commands the session offers, then the engine's own commands named with a second slash (`//review`).
//! - **Status words.** The mode and the plain options of the session are added to the header's state (`active · mode act`).
//! - **`/engines`.** A panel: the selected engine with what the session said (or `capabilities unknown`), and every other engine of the
//!   roadmap as "not available in this release".
//! - **`//word`.** A typed double slash that the record advertises is sent to the engine as a message with one slash; any other double
//!   slash is the router's refusal that names `/send`. A pasted draft is never forwarded.

use super::*;
use ham_core::engines::{self, Capabilities};

/// The engine each run was started on from this app, by run id: what the screen knows of a run's engine before an event of the
/// run's session has arrived. A start of the New run screen, a follow-up and a retry fill it. It only chooses which known engine to
/// name: a session that names its engine wins, and nothing here is sent or stored.
#[derive(Debug, Default)]
pub(crate) struct StartedOn {
    /// The engine in the Engine box when the last start of the New run screen went out.
    asked_now: Option<&'static engines::Engine>,
    by_run: std::collections::HashMap<String, &'static engines::Engine>,
}

impl App {
    /// The New run screen sent a start that runs on `engine`.
    pub(crate) fn engine_asked(&mut self, engine: Option<&'static engines::Engine>) {
        self.started_on.asked_now = engine;
    }

    /// The start was accepted: `run_id` is the run that engine was asked for.
    pub(crate) fn engine_started(&mut self, run_id: &str) {
        if let Some(engine) = self.started_on.asked_now.take() {
            self.engine_started_as(run_id, engine);
        }
    }

    /// Run `run_id`, which this app started, is on `engine`.
    pub(crate) fn engine_started_as(&mut self, run_id: &str, engine: &'static engines::Engine) {
        self.started_on.by_run.insert(run_id.to_string(), engine);
    }

    /// The engine this app started run `id` on, when it started that run.
    pub(crate) fn engine_started_on(&self, id: &str) -> Option<&'static engines::Engine> {
        self.started_on.by_run.get(id).copied()
    }

    /// The engine the service's answer names for run `id`: the summary of the run on screen, else the run's row
    /// of the run list. The service names an engine only for a run that is not on the default engine, so `None` is a run on the default
    /// engine, a service that predates the member, a run the app holds no answer about, or an engine this build does not know. The answer
    /// is read where it stands: nothing is kept for it.
    pub(crate) fn answered_engine(&self, id: &str) -> Option<&'static engines::Engine> {
        let named = self
            .detail
            .as_ref()
            .filter(|d| d.summary.run_id == id)
            .and_then(|d| d.summary.engine)
            .or_else(|| {
                self.runs
                    .iter()
                    .find(|r| r.run_id == id)
                    .and_then(|r| r.engine)
            })?;
        engines::find(named.as_str())
    }

    /// The engine of the run on screen as far as the screen knows it: the one its session named, else the one the service's answer about
    /// the run names ([`App::answered_engine`]), else the one this app started the run on. `None` when none of them is known: a run that
    /// was started elsewhere, has no event of its session yet and whose answer names no engine (a run on the default engine, or a
    /// service that predates the member), and a session that named none.
    pub(crate) fn focused_engine(&self) -> Option<&'static engines::Engine> {
        self.cached
            .as_ref()
            .and_then(engines::named_in)
            .or_else(|| {
                let id = self.focused_run()?;
                self.answered_engine(id)
                    .or_else(|| self.engine_started_on(id))
            })
    }

    /// The capability record of the focused run again, after new events.
    pub(crate) fn caps_refresh(&mut self) {
        let mut caps = self
            .run
            .as_ref()
            .map_or_else(Capabilities::unknown, Capabilities::from_state);
        // Later updates of the stored conversation change the snapshot of the session that is running.
        self.hist_apply_capability_updates(&mut caps);
        self.caps = caps;
    }

    /// The popup for the text after the slash.
    pub(crate) fn popup_for(&self, prefix: &str) -> Vec<&'static registry::Entry> {
        let mut list = engines::popup(&self.caps, prefix);
        // Hidden, not greyed (E7): a run that has ended has nothing to cancel, and only a blocked run can be retried.
        let blocked = self.run_is_blocked();
        let ended = self.run_has_ended();
        list.retain(|e| !(e.name == "cancel" && ended) && !(e.name == "retry" && !blocked));
        list
    }

    /// The header's state with the session's words after it.
    pub(crate) fn state_with_caps(&self, state: String) -> String {
        match self.caps.status_words() {
            w if w.is_empty() => state,
            w => format!("{state} \u{b7} {w}"),
        }
    }

    /// The lines `/help` adds for the engine's own commands (empty when the session advertises none).
    pub(crate) fn engine_help(&self) -> String {
        if self.caps.commands().is_empty() {
            return String::new();
        }
        let mut out = String::from("Commands of the engine (type two slashes, as in //name):\n");
        for c in self.caps.commands() {
            match &c.description {
                Some(d) => out.push_str(&format!("  //{}  {d}\n", c.name)),
                None => out.push_str(&format!("  //{}\n", c.name)),
            }
        }
        out.push('\n');
        out
    }

    /// The engine the focused run is on: read from its `session.started` event; before that event, the engine the service's answer about
    /// the run names, else the engine this app started the run on
    /// ([`App::focused_engine`]); else the default engine, which is what a session that names no engine runs. With no run open it is the engine a new run would start on:
    /// the session's choice, else `default_engine` of the file, else the default engine.
    pub(crate) fn run_engine(&self) -> &'static engines::Engine {
        match &self.cached {
            Some(_) => self
                .focused_engine()
                .unwrap_or_else(engines::default_engine),
            None => self.new_run_engine(),
        }
    }

    /// `text` with `{engine}` replaced by the name of the engine the focused run is on.
    pub(crate) fn engine_say(&self, text: &str) -> String {
        self.run_engine().say(text)
    }

    /// `/engines`: the panel.
    pub(crate) fn engines_panel(&mut self) {
        self.clear_chat();
        let rows = engines::panel_rows(&self.caps, self.run_engine());
        self.panel = Some(Panel::info(engines::panel_title(), &rows.join("\n")));
    }

    /// A typed `//word ...` the engine advertises: sent as a message for after the turn, with the one slash. `None` when the text is not
    /// one; a pasted draft is refused with the reason.
    pub(crate) fn forward_engine_command(&mut self, text: &str) -> Option<Vec<Effect>> {
        let forwarded = self.caps.forward(text)?;
        if self.draft_is_tainted() {
            self.panel = Some(Panel::not_run(
                "Nothing was sent",
                "This text was pasted, so it is not forwarded to the engine as a command. Type the line yourself.",
            ));
            return Some(vec![]);
        }
        Some(self.send_text(&forwarded, harness_protocol::messages::MessageMode::Next))
    }
}
