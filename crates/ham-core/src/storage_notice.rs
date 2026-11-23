//! What `ham` says when a run's session storage refuses a write.
//!
//! The session's storage of a run can refuse writes with "no space left on device" while its files are far below its size. The worker
//! says so in a `turn.progress` event of kind `notify` with the reason [`REASON`] and a `storage` member
//! (`crates/agentd/src/direct/storage_watch.rs`): the session, the number of the notice in the session, the size of the files, the id of
//! the session's first notice and, when the session had saved one, the copy that was the newest before the worker first saw a refusal.
//!
//! This module reads the member ([`parse`]) and builds the client's own lines from it ([`lines`]), each within the width of a notice
//! and with no command cut, and the `Storage` rows of the card of a run that ended ([`card_rows`]) from what the fold of the run's
//! events counted ([`StorageSeen`]). The rows rest on the events alone, so they stand for every session of the run and however it ended.
//! The count is of the worker's notices, not of faults of the storage.
//!
//! Every value printed is checked first: a number is a number, an id is 26 characters of the event-id alphabet, a time parses, a
//! commit is hexadecimal. The run id comes from the client's own state.

use crate::sanitize::sanitize_line;
use harness_protocol::common::parse_utc_unix;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;

/// The reason word of the worker's notice.
pub const REASON: &str = "storage_refused";
/// How many days a copy is kept after a newer copy replaced it: the bucket's rule for replaced versions (`infra/lib/stacks/data.ts`).
pub const REPLACED_COPY_DAYS: u32 = 30;
/// The most characters a value of a card row holds: a row is its ten-character label and the value, 160 characters at the most.
const CARD_VALUE_CHARS: usize = 150;
/// The most notices the view keeps the lines of, for a surface that prints them itself (`ham watch` in line mode): the newest ones.
pub const MAX_RECENT: usize = 16;

/// The copy a notice names: the newest one of its session before the worker first saw a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyBefore {
    pub session: u32,
    pub count: u32,
    /// RFC 3339, UTC.
    pub at: String,
    pub head_sha: String,
}

/// One notice of a storage refusal, as its `storage` member says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageNotice {
    pub event_id: String,
    /// The time of the event (RFC 3339, UTC).
    pub ts: String,
    pub session: u32,
    /// The number of the notice within its session: 1 is the session's first, which carries the size of the files.
    pub stretch: u32,
    pub used_mb: Option<u64>,
    /// The size is a lower bound (the walk was cut short).
    pub partial: bool,
    pub quota_mb: u64,
    /// The id of the session's first notice, on a later one.
    pub first: Option<String>,
    pub copy: Option<CopyBefore>,
    /// The stage's test switch made the notice; the storage refused no write.
    pub test_switch: bool,
}

/// 26 characters of the event-id alphabet (Crockford base 32, upper case).
fn is_event_id(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
}

fn number(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

fn small(v: &Value, key: &str) -> Option<u32> {
    number(v, key).and_then(|n| u32::try_from(n).ok())
}

fn copy_of(v: &Value) -> Option<CopyBefore> {
    let c = v.get("copyBefore")?;
    let at = c.get("at")?.as_str()?;
    parse_utc_unix(at)?;
    let head = c.get("headSha")?.as_str()?;
    if !(7..=64).contains(&head.len()) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(CopyBefore {
        session: small(c, "session").filter(|s| *s >= 1)?,
        count: small(c, "count")?,
        at: sanitize_line(at),
        head_sha: head.to_string(),
    })
}

/// The notice `e` carries: `None` unless it is a `turn.progress` event of kind `notify` with the reason [`REASON`] whose `storage`
/// member names a session. A notice without the member is shown with the worker's own sentence (`crate::notice`).
pub fn parse(e: &Envelope) -> Option<StorageNotice> {
    if e.event_type != EventType::TurnProgress {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    parse_payload(value, &e.event_id, &e.ts)
}

/// The first line of the notice in the payload `value` of a `turn.progress` event, for the one-line notice of the run
/// (`crate::notice`); `None` when the payload carries no `storage` member that names a session.
pub fn first_line_of(value: &Value) -> Option<String> {
    parse_payload(value, "", "").map(|n| first_line(&n))
}

fn parse_payload(value: &Value, event_id: &str, ts: &str) -> Option<StorageNotice> {
    if value.get("kind").and_then(Value::as_str) != Some("notify")
        || value.get("reason").and_then(Value::as_str) != Some(REASON)
    {
        return None;
    }
    let s = value.get("storage").filter(|s| s.is_object())?;
    Some(StorageNotice {
        event_id: event_id.to_string(),
        ts: sanitize_line(ts),
        session: small(s, "session").filter(|n| *n >= 1)?,
        stretch: small(s, "stretch").unwrap_or(1).max(1),
        used_mb: number(s, "usedMb"),
        partial: s.get("partial").and_then(Value::as_bool) == Some(true),
        quota_mb: number(s, "quotaMb").unwrap_or(1024),
        first: s
            .get("first")
            .and_then(Value::as_str)
            .filter(|id| is_event_id(id))
            .map(str::to_string),
        copy: copy_of(s),
        test_switch: s.get("testSwitch").and_then(Value::as_bool) == Some(true),
    })
}

/// The sentence a notice made by the stage's test switch ends with.
pub const TEST_SWITCH_LINE: &str =
    "This notice was made by the stage's test switch; the storage refused no write.";

/// The first line of a notice: what happened, with the size of the files on the session's first one.
pub fn first_line(n: &StorageNotice) -> String {
    if n.stretch > 1 {
        return format!(
            "The session's storage refused a write again (no space left on device); ham has seen this {} times in this session.",
            n.stretch
        );
    }
    match n.used_mb {
        Some(used) => format!(
            "The session's storage refused a write while its files add up to {}{used} MiB of {} MiB (no space left on device).",
            if n.partial { "at least " } else { "" },
            n.quota_mb
        ),
        None => "The session's storage refused a write (no space left on device); the size of its files could not be measured."
            .to_string(),
    }
}

/// The command that downloads the newest copy.
fn newest(run_id: &str) -> String {
    format!("ham pull {} --snapshot", sanitize_line(run_id))
}

/// The command that downloads the copy a notice names. The id is the one of the session's first notice.
fn before(run_id: &str, n: &StorageNotice) -> String {
    format!(
        "ham pull {} --before-refusal {}",
        sanitize_line(run_id),
        n.first.as_deref().unwrap_or(&n.event_id)
    )
}

/// What is known about a refused write with room left on this storage: what it has meant in measurements. It is a statement about
/// the storage and not about the one refusal a notice reports, whose cause a session cannot read.
pub const KNOWN_LINE: &str = "On this storage a refused write with room left has meant that data the session wrote is still waiting to be backed up.";
/// How such refusals ended in measurements: each by itself, the longest after about a minute and a half, and again with a large repository.
pub const ENDED_LINE: &str = "In measurements each refusal ended by itself, the longest after about a minute and a half. With a large repository it came again.";
/// What a command can have left behind. A line of its own: joined to the sentence before, it would pass the 160 characters of a row.
pub const EMPTY_LINE: &str =
    "A file that a command wrote meanwhile can be empty; ham does not know which.";

/// The lines after the first: what is known about this storage, what ham does not know, the copy from before with its command on a
/// line of its own, and the newest copy. A line holds one command at the most and is never cut.
pub fn follow_up(n: &StorageNotice, run_id: &str) -> Vec<String> {
    let newest = format!(
        "The newest copy holds the files as they are, empty ones included: {}",
        newest(run_id)
    );
    let mut out = Vec::new();
    if n.stretch > 1 {
        if n.copy.is_some() {
            out.push(format!(
                "The last copy saved before ham first saw it: {}",
                before(run_id, n)
            ));
        }
        out.push(newest);
    } else {
        out.push(KNOWN_LINE.to_string());
        out.push(ENDED_LINE.to_string());
        out.push(EMPTY_LINE.to_string());
        match &n.copy {
            Some(c) => {
                out.push(format!(
                    "Copy {} ({}) is the last copy saved before ham first saw this. It is kept for {REPLACED_COPY_DAYS} days after a newer copy replaced it:",
                    c.count,
                    crate::saved_copy::utc_minute(&c.at)
                ));
                out.push(format!("  {}", before(run_id, n)));
            }
            None => {
                out.push("This session had saved no copy before ham first saw this.".to_string())
            }
        }
        out.push(newest);
    }
    if n.test_switch {
        out.push(TEST_SWITCH_LINE.to_string());
    }
    out
}

/// Every line of a notice: [`first_line`], then [`follow_up`].
pub fn lines(n: &StorageNotice, run_id: &str) -> Vec<String> {
    let mut out = vec![first_line(n)];
    out.extend(follow_up(n, run_id));
    out
}

/// One of the newest notices the view keeps: the notice, and its lines as line mode prints them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub notice: StorageNotice,
    pub lines: Vec<String>,
}

/// The entry of a notice in the member `storageNotices` of the JSON view of `ham watch`: `at` (the time of the notice), `id`
/// (the notice's id, which `ham pull <run> --before-refusal` takes when the notice names a copy), `session` (the session of the
/// notice), `stretch` (the number of the notice within that session, so two sessions of a run can both have a notice 1), `text`
/// ([`first_line`]) and, when the notice names a copy, `copyBefore` with the copy's `session`, `count` and `at`. The commit and
/// the bucket's version of the copy stay out of the view.
pub fn json_entry(n: &StorageNotice) -> Value {
    let mut entry = serde_json::json!({
        "at": n.ts, "id": n.event_id, "session": n.session, "stretch": n.stretch, "text": first_line(n)
    });
    if let Some(c) = &n.copy {
        entry["copyBefore"] =
            serde_json::json!({"session": c.session, "count": c.count, "at": c.at});
    }
    entry
}

/// The run's first notice, as the card names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageFirst {
    pub event_id: String,
    pub ts: String,
    pub session: u32,
    /// The notice names a copy from before.
    pub has_copy: bool,
}

/// What the fold of a run's events keeps about its storage notices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageSeen {
    /// Notices with a `storage` member, over every session of the run.
    pub count: u32,
    /// The run's first such notice.
    pub first: Option<StorageFirst>,
    /// The newest [`MAX_RECENT`] notices with their lines, for a surface that prints them itself (line mode) and for the JSON view
    /// of `ham watch` ([`json_entry`]).
    pub recent: Vec<Recent>,
    /// A notice of the run was made by the stage's test switch.
    pub test_switch: bool,
}

impl StorageSeen {
    /// Counts the notice of `e`, if it is one.
    pub fn fold(&mut self, e: &Envelope, run_id: &str) {
        let Some(n) = parse(e) else {
            return;
        };
        self.count = self.count.saturating_add(1);
        self.test_switch |= n.test_switch;
        if self.first.is_none() {
            self.first = Some(StorageFirst {
                event_id: n.event_id.clone(),
                ts: n.ts.clone(),
                session: n.session,
                has_copy: n.copy.is_some(),
            });
        }
        if self.recent.len() >= MAX_RECENT {
            self.recent.remove(0);
        }
        let lines = lines(&n, run_id);
        self.recent.push(Recent { notice: n, lines });
    }
}

/// The values of the `Storage` rows of the card of a run that ended: empty for a run without a notice. The first row says how often
/// ham saw it and when first; the others name the command for the copy from before, or say that there was none, and the command for
/// the newest copy.
pub fn card_values(seen: &StorageSeen, run_id: &str) -> Vec<String> {
    let Some(first) = &seen.first else {
        return Vec::new();
    };
    let run = sanitize_line(run_id);
    let when = crate::saved_copy::utc_minute(&first.ts);
    let often = if seen.count <= 1 {
        format!("once in this run ({when}, session {})", first.session)
    } else {
        format!(
            "{} times in this run (first {when}, session {})",
            seen.count, first.session
        )
    };
    let saw = format!("ham saw the session's storage refuse writes {often}.");
    let empty = "A file a command wrote then can be empty.";
    // One row while it fits the width of a card row with its label; with long numbers the second sentence gets a row of its own.
    let mut out = if saw.chars().count() + 1 + empty.chars().count() <= CARD_VALUE_CHARS {
        vec![format!("{saw} {empty}")]
    } else {
        vec![saw, empty.to_string()]
    };
    if first.has_copy {
        out.push(format!(
            "ham pull {run} --before-refusal {} downloads the last copy saved before ham first saw it.",
            first.event_id
        ));
        out.push(format!(
            "It is kept {REPLACED_COPY_DAYS} days after a newer copy replaced it."
        ));
    } else {
        out.push("No copy had been saved before ham first saw it.".to_string());
    }
    out.push(format!(
        "ham pull {run} --snapshot downloads the newest copy, with the files as they are."
    ));
    if seen.test_switch {
        out.push("Made by the stage's test switch: the storage refused no write.".to_string());
    }
    out
}

/// [`card_values`] as the rows of the card: the label `Storage` on the first, ten spaces before the others.
pub fn card_rows(seen: &StorageSeen, run_id: &str) -> Vec<String> {
    card_values(seen, run_id)
        .into_iter()
        .enumerate()
        .map(|(i, value)| {
            if i == 0 {
                format!("Storage   {value}")
            } else {
                format!("          {value}")
            }
        })
        .collect()
}
