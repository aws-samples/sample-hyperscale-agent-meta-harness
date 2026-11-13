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

impl App {
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
        // Hidden, not greyed (E7): a run that ended blocked has nothing to cancel, and only a blocked run can be retried.
        let blocked = self.run_is_blocked();
        list.retain(|e| e.name != if blocked { "cancel" } else { "retry" });
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

    /// The engine the focused run is on (read from its `session.started` event). With no run open it is the engine a new run would start on:
    /// the session's choice, else `default_engine` of the file, else the default engine.
    pub(crate) fn run_engine(&self) -> &'static engines::Engine {
        match &self.cached {
            Some(view) => engines::of_view(view),
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
