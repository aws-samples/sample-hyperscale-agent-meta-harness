//! The retry of a blocked run as the client words it.
//!
//! A run that ended `blocked` delivered nothing and its session is over: a message cannot reach the engine, `/cancel` has nothing to stop, and a
//! follow-up has no delivered head to start from. The service holds the request the run was started with (`runs/<id>/input/request.json`), so the
//! client asks it to start that request again, `POST /v1/runs/{runId}/retry`, and sends none of the request itself (invariant 5: `ham` keeps no
//! copy). The new run names the blocked run in `continueFrom`, which the Runs list shows.
//!
//! The Idempotency-Key is `retry-<blocked run id>`: a second press, or a send that got no answer, returns the run the first one started, and a
//! blocked run has one retry (if that retry blocks too, the person retries the new run). This module holds that key, the words of the box, the
//! offer and the refusals, and the note that marks a run that continues another. Every word that comes from the service passes the line
//! sanitizer, and no word names the engine (`engines::say`).

use crate::engines;
use crate::sanitize::sanitize_line;

/// The start of the Idempotency-Key of a retry.
pub const KEY_PREFIX: &str = "retry-";

/// The mark in front of the name of a run that continues another run (the Runs list's NAME column).
pub const LINK_MARK: &str = "\u{21b3} ";

/// The Idempotency-Key of the retry of `blocked_run`: the same for every press, so the service returns one run.
pub fn start_key(blocked_run: &str) -> String {
    format!("{KEY_PREFIX}{blocked_run}")
}

/// The hint on a blocked run while the box holds text: what happened and what Enter does.
pub const HINT: &str = "This run is blocked: it ended without a result and takes no messages. Enter offers to start it again as a new run with the same request. Your text stays in the box and is not sent.";

/// [`HINT`] and where the work of the blocked run is ([`crate::saved_copy::blocked_work`]): that it stays saved when the run's events
/// show the copy of the end, the time of an earlier copy, else what the snapshot command does.
pub fn hint(run_id: &str, saved: Option<&crate::saved_copy::SavedCopy>) -> String {
    format!("{HINT} {}", crate::saved_copy::blocked_work(run_id, saved))
}

/// What `/retry` says on a run that is not blocked.
pub const NOT_BLOCKED: &str =
    "Nothing to retry: only a blocked run can be retried, and this run is not blocked.";

/// What `c` says on a blocked run: it has ended, so there is nothing to cancel, and the key that does something is named.
pub const ENDED: &str =
    "This run already ended (blocked): there is nothing to cancel. r retries it.";

/// The title of the confirmation panel.
pub const CONFIRM_TITLE: &str = "Retry blocked run?";

/// The words after the box's name in its title on a blocked run: `Retry blocked run 01K6A000: new billable run`.
pub fn header(run_id: &str) -> String {
    let short: String = sanitize_line(run_id).chars().take(8).collect();
    format!("Retry blocked run {short}: new billable run")
}

/// The confirmation of the second Enter: what starts (a new billable run with the same request, linked to this one), the target as the blocked
/// run's summary spells it when it is known, and that the text in the box, if any, is kept and not sent. The words must match the call the
/// second Enter makes (`ham::tui::send`).
pub fn confirmation(run_id: &str, target: Option<&str>, has_text: bool) -> String {
    confirmation_with(run_id, target, has_text, None)
}

/// [`confirmation`] with where the work of the blocked run is: a retry starts from the request alone, so the panel says that this
/// run's work stays saved (when the run's events show the copy of the end; for an earlier copy, its time) and names the command that
/// downloads it.
pub fn confirmation_with(
    run_id: &str,
    target: Option<&str>,
    has_text: bool,
    saved: Option<&crate::saved_copy::SavedCopy>,
) -> String {
    let run = sanitize_line(run_id);
    let work = crate::saved_copy::blocked_work(run_id, saved);
    let target = match target.map(sanitize_line).filter(|t| !t.trim().is_empty()) {
        Some(t) => format!("\nTarget: {t}, as before."),
        None => String::new(),
    };
    let kept = if has_text {
        "\nYour text stays in the box and is not sent."
    } else {
        ""
    };
    engines::say(&format!(
        "This starts a new billable run with the same request as run {run}: the same prompt, target, model, rules and settings. The new run is linked to run {run}.{target}\nThe new {{engine}} receives the request only, not this run's history.\n{work}{kept}\nEnter starts it \u{b7} Esc cancels"
    ))
}

/// The notice when the service started the retry: both runs by id, and where the text of the box went.
pub fn started(blocked_run: &str, new_run: &str, kept_text: bool) -> String {
    let tail = if kept_text {
        "; your text is in the box"
    } else {
        ""
    };
    format!(
        "started run {}, a retry of run {}{tail}",
        sanitize_line(new_run),
        sanitize_line(blocked_run)
    )
}

/// The notice when the service started the retry and the new run opened on screen: the run it retries and where the text of the box
/// went, in at most 80 columns, because the run screen's notice row is one row.
pub fn started_here(blocked_run: &str, kept_text: bool) -> String {
    let tail = if kept_text {
        "; your text is in the box"
    } else {
        ""
    };
    format!(
        "the retry of run {} started{tail}",
        sanitize_line(blocked_run)
    )
}

/// The words for a refusal of the retry route, in one line: the service's own message made safe, with what the person can do next. `status` is
/// 0 when there was no answer.
pub fn explain(status: u16, code: &str, message: &str) -> String {
    let m = sanitize_line(message);
    let code_word = sanitize_line(code);
    match (status, code) {
        // A stage deployed before the route existed answers a path it does not know.
        (404, "not-found") if m.contains("no such route") => {
            "this stage does not offer retry yet: it needs a newer backend. Start a new run with the same prompt (n) instead".to_string()
        }
        (404, _) | (409, "run-active" | "run-not-blocked") | (422, "stored-request-unreadable") => m,
        (409, "no-delivered-head" | "head-mismatch") => {
            format!("{m}; start a new run instead")
        }
        // A stage that is being updated starts no new runs and says so in one sentence, which needs no word of ours.
        (409, crate::update::STAGE_UPDATING) => m,
        (422, "unknown-model") => format!("{m}; start a new run and pick another model"),
        (422, "idempotency-key-reused") => {
            "the key of this retry was already used for another request; start a new run instead".to_string()
        }
        // A stage whose function is newer than its role, or whose stored request is gone, answers like an outage for as long as it lasts.
        (500..=599, _) => "the service did not answer: press the key again in a moment (a retry never starts a second run); if it keeps failing, start a new run with the same prompt".to_string(),
        (0, _) => format!(
            "{m}; the retry may not have reached the service: press the key again (a retry never starts a second run)"
        ),
        _ => format!("{status} {code_word}: {m}"),
    }
}

/// The note for a run whose summary has `continueFrom`: it continues `source`, a follow-up of a delivered run or the retry of a blocked one.
pub fn link_note(source: &str) -> String {
    format!("continues run {}", sanitize_line(source))
}
