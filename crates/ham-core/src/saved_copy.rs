//! The saved copy of a run's work as the client words it.
//!
//! While a run works, the service keeps a copy of its latest work in the bucket; when the engine has stopped it saves the end state once
//! more. agentd tells the run of each stored copy with a progress event that has no `kind` and carries a `savedCopy` member
//! (`{at, count, headSha, session, final?}`); a client older than the member ignores the event. The newest one is what the run screen and
//! the card show ([`screen_line`], [`card_line`]). The record in the bucket stays the authority: the event is what the client shows, and
//! `ham pull <runId> --snapshot` asks the service.
//!
//! This module also holds the sentences that say where the work of a run without a zip is ([`no_zip`], [`no_delivery`]): they name the
//! snapshot command and say what it does, never that a copy exists.

use crate::engines::Engine;
use crate::sanitize::sanitize_line;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;

/// The newest copy of its work a run saved, as its events say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedCopy {
    /// When the copy was stored: an RFC 3339 UTC time.
    pub at: String,
    /// [`SavedCopy::at`] in Unix seconds.
    pub at_unix: i64,
    /// The number of the copy within its session.
    pub count: u32,
    /// The commit the copy holds (hexadecimal digits only).
    pub head_sha: String,
    /// The session (slice) of the run that saved it; 1 for a run that never renewed its session.
    pub session: u32,
    /// The copy was saved when the engine had stopped: it holds the work as the engine left it.
    pub final_copy: bool,
}

impl SavedCopy {
    /// Whether `self` is a newer copy than `other`: a later session, or a later copy of the same session. An event that arrives late
    /// never puts an older copy back.
    pub fn newer_than(&self, other: &SavedCopy) -> bool {
        (self.session, self.count) > (other.session, other.count)
    }
}

/// The `savedCopy` member of a progress event of an agent, when it is one: a UTC time, a count and a commit id. Anything else is no copy.
pub fn parse(e: &Envelope) -> Option<SavedCopy> {
    if e.event_type != EventType::TurnProgress {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    let c = value.get("savedCopy")?;
    let at = c.get("at")?.as_str()?;
    let at_unix = parse_utc_unix(at)?;
    let head = c.get("headSha")?.as_str()?;
    if !(7..=64).contains(&head.len()) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let number = |k: &str| {
        c.get(k)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    Some(SavedCopy {
        at: sanitize_line(at),
        at_unix,
        count: number("count")?,
        head_sha: head.to_string(),
        session: number("session").unwrap_or(1).max(1),
        final_copy: c.get("final").and_then(serde_json::Value::as_bool) == Some(true),
    })
}

/// `12 s ago`, `2 min ago`, `3 h ago`: how long ago `at_unix` was at `now`; `just now` for less than five seconds and for a clock that
/// is behind the service's.
pub fn ago(at_unix: i64, now: i64) -> String {
    match now.saturating_sub(at_unix) {
        i64::MIN..=4 => "just now".to_string(),
        s @ 5..=59 => format!("{s} s ago"),
        s @ 60..=3_599 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3_600),
    }
}

/// What the copy is, after its time: `when <engine> stopped` for the copy of the end, `while the run worked` for an earlier one of a run that
/// is over, and how long ago for a run that works (`now` is the client's clock; without one, nothing).
fn what(c: &SavedCopy, engine: &Engine, over: bool, now: Option<i64>) -> Option<String> {
    if c.final_copy {
        Some(engine.say("when {engine} stopped"))
    } else if over {
        Some("while the run worked".to_string())
    } else {
        now.map(|n| ago(c.at_unix, n))
    }
}

/// The `Saved` line of the run screen's status block: `Saved  17:40 MST, 2 min ago` while the run works, `Saved  17:47 MST, when <engine>
/// stopped` for the copy of the end, `Saved  17:40 MST, while the run worked` for an earlier copy of a run that is over. `clock` writes a
/// time of day for Unix seconds (the screen's local clock).
pub fn screen_line(
    c: &SavedCopy,
    engine: &Engine,
    over: bool,
    now: Option<i64>,
    clock: &dyn Fn(i64) -> String,
) -> String {
    let time = sanitize_line(&clock(c.at_unix));
    match what(c, engine, over, now) {
        Some(w) => format!("Saved  {time}, {w}"),
        None => format!("Saved  {time}"),
    }
}

/// `2026-11-12 00:47 UTC` for the RFC 3339 UTC time of a copy: seconds add nothing to a line a person reads.
fn utc_minute(at: &str) -> String {
    let time = at.replace('T', " ");
    time.strip_suffix('Z').map_or(time.clone(), |t| {
        format!("{} UTC", t.get(..16).unwrap_or(t))
    })
}

/// The `Saved` line of the delivery card: the UTC time the copy was stored, what it is, and the command that downloads it.
pub fn card_line(c: &SavedCopy, engine: &Engine, over: bool, run_id: &str) -> String {
    let time = utc_minute(&c.at);
    let w = what(c, engine, over, None).map_or_else(String::new, |w| format!(", {w}"));
    format!(
        "Saved     {time}{w}; ham pull {} --snapshot downloads it",
        sanitize_line(run_id)
    )
}

fn status_word(s: GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
        .replace('_', " ")
}

/// What `ham pull`, `ham clone`, `ham link` and `ham push` say for a run that has no zip. The sentence follows the run's status, and it
/// says what the snapshot command does: whether the run saved a copy is the command's answer.
pub fn no_zip(run_id: &str, status: Option<GoalStatus>) -> String {
    let run = sanitize_line(run_id);
    let copy = format!("ham pull {run} --snapshot downloads the latest saved copy of its work.");
    match status {
        Some(GoalStatus::Active) => format!(
            "run {run} is still working and has no zip yet. Follow it with ham watch {run}; {copy}"
        ),
        Some(s) => format!(
            "run {run} ended {} and delivered no zip. {copy}",
            status_word(s)
        ),
        None => format!("run {run} has no zip. {copy}"),
    }
}

/// [`no_zip`] for a key of the run screen (download, link, clone, the artifact list) on a run that ended without a zip: one short line,
/// with the command as it is typed there. `None` for a run that works or whose status is not known: the key keeps its own words.
pub fn no_delivery(status: Option<GoalStatus>) -> Option<String> {
    match status {
        Some(GoalStatus::Active) | None => None,
        Some(s) => Some(format!(
            "this run ended {} and delivered no zip; /pull --snapshot downloads the latest saved copy of its work",
            status_word(s)
        )),
    }
}

/// What the retry panel and the hint of a blocked run say about its work: that it stays saved when the run's events show the copy of the
/// end; the time of the copy and that later work is not in it for a copy made while the run worked (the copy of the end was not saved, or
/// the service is older than it); else what the snapshot command does.
pub fn blocked_work(run_id: &str, saved: Option<&SavedCopy>) -> String {
    let run = sanitize_line(run_id);
    match saved {
        Some(c) if c.final_copy => {
            format!("This run's work stays saved: ham pull {run} --snapshot downloads it.")
        }
        Some(c) => format!(
            "ham pull {run} --snapshot downloads the copy of this run's work saved at {}; work done after that is not in it.",
            utc_minute(&c.at)
        ),
        None => format!(
            "ham pull {run} --snapshot downloads this run's last saved copy, if it saved one."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_age_is_said_in_the_largest_whole_unit() {
        assert_eq!(ago(1_000, 1_000), "just now");
        assert_eq!(ago(1_000, 990), "just now");
        assert_eq!(ago(1_000, 1_012), "12 s ago");
        assert_eq!(ago(1_000, 1_000 + 125), "2 min ago");
        assert_eq!(ago(1_000, 1_000 + 3 * 3_600 + 5), "3 h ago");
    }
}
