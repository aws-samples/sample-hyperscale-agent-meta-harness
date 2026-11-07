//! What the clean-checkout test report is doing, on the screens of the client.
//!
//! After the engine ends, agentd clones the delivered head and runs the repository's own test commands (`crates/agentd/src/direct/report/`). That
//! takes minutes, and the stored phase of the run stays `green` until delivery starts. agentd therefore writes `turn.progress` events of kind
//! `report` (`harness_protocol::direct::ReportProgress`): one when the report begins, one at the start of each command and one when the commands
//! are over. [`parse`] reads one event; the run view keeps the newest ([`ReportNow`]) and names the phase from it while the report goes on
//! (`RunView::phase_now`). The events carry agent authority, so they change no status and no stored phase: they are a display of what agentd said
//! it was doing, and a run that is over or in delivery shows none of it. The event's `text` is also the agent's newest words (the live row)
//! through the ordinary progress path.
//!
//! Nothing here says anything about the tests: the words name the step and the time limit, never a result.

use crate::notice::{NOTICE_MAX_CHARS, cut};
use crate::redact::redact_tokens;
use crate::sanitize::display_line;
use harness_protocol::direct::{ReportProgress, ReportProgressState};
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;

/// The phase word the header shows while the report runs, in place of the stored phase.
pub const PHASE_WORDS: &str = "running the repository's tests";

/// The most characters of the command a screen keeps.
const COMMAND_MAX_CHARS: usize = 120;

/// The newest `report` event of a run, cleaned for the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportNow {
    /// The event id: the newest report event wins.
    pub event_id: String,
    pub state: ReportProgressState,
    /// The command that runs now (`running` only), one clean line.
    pub command: Option<String>,
    /// `running` only: the 1-based number of the command and how many are planned.
    pub step: Option<u32>,
    pub of: Option<u32>,
    /// `running` only: the time limit this command was given, in seconds.
    pub limit_s: Option<u32>,
    /// One clean line for the live row and the transcript.
    pub text: String,
}

impl ReportNow {
    /// The report is going on: it began or a command runs, and the commands are not over.
    pub fn is_running(&self) -> bool {
        self.state != ReportProgressState::Ended
    }
}

fn clean(text: &str) -> String {
    redact_tokens(&display_line(text)).trim().to_string()
}

/// The payload value of a `turn.progress` event of kind `report`, or `None` for any other event.
fn payload(e: &Envelope) -> Option<&Value> {
    if e.event_type != EventType::TurnProgress {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    (value.get("kind")?.as_str()? == harness_protocol::direct::PROGRESS_REPORT).then_some(value)
}

/// What a `report` event says, or `None` for another event or for a state this client does not know.
pub fn parse(e: &Envelope) -> Option<ReportNow> {
    let p = ReportProgress::from_value(payload(e)?)?;
    Some(ReportNow {
        event_id: e.event_id.clone(),
        state: p.state,
        command: p
            .command
            .as_deref()
            .map(|c| cut(&clean(c), COMMAND_MAX_CHARS))
            .filter(|c| !c.is_empty()),
        step: p.step,
        of: p.of,
        limit_s: p.limit_s,
        text: cut(&clean(&p.text), NOTICE_MAX_CHARS),
    })
}

/// The transcript's one line for a `report` event: its own text, else words by state.
pub(crate) fn notice_line(v: &Value) -> String {
    let own = v
        .get("text")
        .and_then(Value::as_str)
        .map(clean)
        .filter(|t| !t.is_empty());
    if let Some(t) = own {
        return cut(&t, NOTICE_MAX_CHARS);
    }
    match v.get("state").and_then(Value::as_str) {
        Some("checkout") => "Making a clean checkout to run the repository's tests.",
        Some("running") => "Running the repository's tests.",
        Some("ended") => "The repository's tests ended.",
        _ => "Test report.",
    }
    .to_string()
}
