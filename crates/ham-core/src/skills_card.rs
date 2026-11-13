//! The skills of a run on the card.
//!
//! agentd's skills module writes one `turn.progress` event of kind `skills` after it placed the run's skills (and, where the fork can list
//! them, after the engine's own listing showed each one): each skill with its `name`, `digest`, `files` and `bytes`, and
//! `preflight {ran, ms}` (`crates/agentd/src/direct/skills/mod.rs`, `skills_event`). It holds no instruction text. [`from_events`] reads
//! that event and nothing else and builds the [`SkillsCard`] that `RunView::skills` carries; the run screen draws it as a folded card,
//! `ham watch` prints it once, `--json` lists the skills, and the transcript gets one plain notice ([`notice_line`]).
//!
//! The card says what was placed and whether the engine's own list showed it. It says nothing about the work: no line calls
//! anything verified, tested, gated or evaluated. Every string from the event passes the sanitizer before it reaches a screen. Its sentences name
//! the engine the run is on, read from the run's own `session.started` event as the goal card does.

use crate::notice::{NOTICE_MAX_CHARS, cut};
use crate::redact::redact_tokens;
use crate::sanitize::{display_line, sanitize_block};
use crate::tables::group_digits;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;

/// The most skills a card keeps: every skill a run may carry ([`harness_protocol::direct::MAX_SKILLS`]); a longer list from an event is cut and
/// counted in [`SkillsCard::total`].
pub const MAX_ROWS: usize = harness_protocol::direct::MAX_SKILLS;

/// The digits of the digest the card shows, as `ham rules show` prints them.
const DIGEST_SHOWN: usize = 12;

/// One placed skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillRow {
    pub name: String,
    /// The skill's SHA-256 as 64 lowercase hex digits; empty when the event gave anything else.
    pub digest: String,
    pub files: u64,
    pub bytes: u64,
}

impl SkillRow {
    /// The first 12 digits of the digest, or `?` when there is none.
    pub fn short_digest(&self) -> &str {
        self.digest.get(..DIGEST_SHOWN).unwrap_or("?")
    }

    fn size_words(&self) -> String {
        format!(
            "{} file{}, {} bytes",
            self.files,
            if self.files == 1 { "" } else { "s" },
            group_digits(self.bytes)
        )
    }
}

/// What the card of a run's skills holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillsCard {
    /// The event id of the `skills` event: `ham watch` prints the card once per event.
    pub request_id: String,
    /// How many skills the event named (the rows can be fewer, see [`MAX_ROWS`]).
    pub total: usize,
    /// The skills in the order the event listed them.
    pub rows: Vec<SkillRow>,
    /// How long the engine's own listing of the skills took, when it ran: it showed each placed skill before the first prompt. `None` when
    /// this fork cannot list skills, so they were placed and not listed.
    pub preflight_ms: Option<u64>,
    /// The id of the engine the run is on (an id of `crate::engines`), for the words of the card; the default engine when no event named one.
    pub engine: &'static str,
}

fn clean(text: &str) -> String {
    redact_tokens(&display_line(text)).trim().to_string()
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The skills an event's payload names, how many it named, and the preflight; `None` for a payload that has no usable list.
fn parse(v: &Value) -> Option<(usize, Vec<SkillRow>, Option<u64>)> {
    let list = v.get("skills")?.as_array()?;
    let mut total = 0;
    let mut rows = Vec::new();
    for item in list {
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .map(clean)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        total += 1;
        if rows.len() < MAX_ROWS {
            let digest = item
                .get("digest")
                .and_then(Value::as_str)
                .filter(|d| is_digest(d))
                .unwrap_or_default();
            rows.push(SkillRow {
                name,
                digest: digest.to_string(),
                files: item.get("files").and_then(Value::as_u64).unwrap_or(0),
                bytes: item.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            });
        }
    }
    if total == 0 {
        return None;
    }
    // agentd lists the skills that fit an inline event and names the total beside them (a run may carry 32).
    if let Some(named) = v.get("total").and_then(Value::as_u64) {
        total = total.max(usize::try_from(named).unwrap_or(total));
    }
    let pre = v.get("preflight");
    let ran = pre
        .and_then(|p| p.get("ran"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ms = ran.then(|| {
        pre.and_then(|p| p.get("ms"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    });
    Some((total, rows, ms))
}

fn skills_event(e: &Envelope) -> Option<&Value> {
    if e.event_type != EventType::TurnProgress {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    (value.get("kind")?.as_str()? == "skills").then_some(value)
}

/// The card of the last usable `skills` event in `events` (in event-id order), or `None` for a run that placed no skills.
pub fn from_events<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> Option<SkillsCard> {
    let mut card: Option<SkillsCard> = None;
    let mut engine = crate::engines::default_engine();
    let mut engine_seen = false;
    for e in events {
        if !engine_seen && let Some(named) = crate::engines::engine_in(e) {
            engine = named;
            engine_seen = true;
        }
        if let Some(v) = skills_event(e)
            && let Some((total, rows, preflight_ms)) = parse(v)
        {
            card = Some(SkillsCard {
                request_id: e.event_id.clone(),
                total,
                rows,
                preflight_ms,
                engine: engine.id,
            });
        }
    }
    // The session event can come after the skills event in the order of the event ids: the card names the engine the run is on either way.
    card.map(|c| SkillsCard {
        engine: engine.id,
        ..c
    })
}

/// The transcript's one line for a `skills` event: `Skills: 3 placed (a, b, c).`, with as many names as fit and a count of the rest.
pub(crate) fn notice_line(v: &Value) -> String {
    let Some((total, rows, _)) = parse(v) else {
        return "Skills placed.".to_string();
    };
    let head = format!("Skills: {total} placed");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    for k in (1..=names.len()).rev() {
        let line = if k == total {
            format!("{head} ({}).", names[..k].join(", "))
        } else {
            format!(
                "{head} ({}, and {} more).",
                names[..k].join(", "),
                total - k
            )
        };
        if line.chars().count() <= NOTICE_MAX_CHARS {
            return line;
        }
    }
    format!("{head}.")
}

impl SkillsCard {
    /// The border title of the card.
    pub fn title(&self) -> String {
        format!("skills · {} placed", self.total)
    }

    /// The sentence about the engine's own listing.
    fn preflight_sentence(&self) -> String {
        let say = |t: &str| crate::engines::say_for(Some(self.engine), t);
        match self.preflight_ms {
            Some(ms) => say(&format!(
                "{{engine}} listed all {} before it started ({} ms).",
                self.total,
                group_digits(ms)
            )),
            None => say(
                "{engine}'s own skills list was not available, so these skills were placed but not listed.",
            ),
        }
    }

    /// The lines of the card for a screen `width` columns wide: one row per skill (name, the first 12 digits of the digest, files and
    /// bytes), a count of the skills left out, where the skills sit and what the engine does with them, and whether its own list showed them.
    pub fn lines(&self, width: usize) -> Vec<String> {
        let w = width.max(1);
        let pad = self
            .rows
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = self
            .rows
            .iter()
            .map(|r| {
                fit_row(
                    &r.name,
                    pad,
                    &format!("sha256 {}  {}", r.short_digest(), r.size_words()),
                    w,
                )
            })
            .collect();
        if self.total > self.rows.len() {
            out.push(cut(
                &format!("and {} more", self.total - self.rows.len()),
                w,
            ));
        }
        let wrap = |text: &str| sanitize_block(text, 8, w);
        out.extend(wrap(&crate::engines::say_for(
            Some(self.engine),
            "Placed read-only beside your rules before the first prompt; {engine} loads a skill when a task matches its description.",
        )));
        out.extend(wrap(&self.preflight_sentence()));
        out
    }
}

/// `name  tail` in at most `w` characters: the name padded to the column when the line fits, else the name cut to what is left, else the
/// whole line cut.
fn fit_row(name: &str, pad: usize, tail: &str, w: usize) -> String {
    let len = |s: &str| s.chars().count();
    let padded = format!("{name:<pad$}  {tail}");
    if len(&padded) <= w {
        return padded;
    }
    let plain = format!("{name}  {tail}");
    if len(&plain) <= w {
        return plain;
    }
    let room = w.saturating_sub(len(tail) + 2);
    if room >= 8 {
        return format!("{}  {tail}", cut(name, room));
    }
    cut(&plain, w)
}
