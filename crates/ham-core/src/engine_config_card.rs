//! The engine settings of a run on the card.
//!
//! agentd's engine-config module writes one `turn.progress` event of kind `engine-config` after it placed the user's settings where the engine reads
//! them and read them back (`crates/agentd/src/direct/engine_config/mod.rs`, `event`): `engine`, the settings hash `sha256`, `keys`, the `settings`
//! it placed (each a key of the engine's table with a short value) and `readBack: "ok"`. It holds nothing of the user's file but those. [`from_events`]
//! reads that event and nothing else and builds the [`EngineConfigCard`] that `RunView::engine_config` carries; the run screen draws it as a folded
//! card, `ham watch` prints it once, `--json` lists the keys, and the transcript gets one plain notice ([`notice_line`]).
//!
//! The card says **placed**: agentd wrote the value where the engine reads it and read it back. That is not a statement that the engine used it, and
//! no line calls anything verified, tested, gated or evaluated. Every string from the event passes the sanitizer and the redactor before
//! it reaches a screen; an event whose read-back is not `ok`, or that places no key, makes no card. The module names no engine: the engine is the
//! event's own word.

use crate::notice::{NOTICE_MAX_CHARS, cut};
use crate::redact::redact_tokens;
use crate::sanitize::display_line;
use harness_protocol::envelope::{Envelope, EventType, Payload};
use serde_json::Value;

/// The most settings a card keeps (the event lists at most 24; a longer list is cut and counted in [`EngineConfigCard::keys`]).
pub const MAX_ROWS: usize = 24;

/// The longest value a row shows, in characters.
const VALUE_MAX: usize = 64;

/// The digits of the settings hash the card shows, as `ham rules show` shows a digest.
const HASH_SHOWN: usize = 12;

/// One placed setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    /// The dotted name of the key in the engine's table.
    pub key: String,
    /// The value as the event worded it: a text as it is, a list joined by commas, a number as written.
    pub value: String,
}

/// What the card of a run's engine settings holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfigCard {
    /// The event id of the `engine-config` event: `ham watch` prints the card once per event.
    pub request_id: String,
    /// The engine, as the event names it (cleaned).
    pub engine: String,
    /// The settings hash as 64 lowercase hex digits; empty when the event gave anything else.
    pub sha256: String,
    /// How many keys the event says were placed (the rows can be fewer, see [`MAX_ROWS`]).
    pub keys: usize,
    /// The settings in the order the event listed them.
    pub rows: Vec<SettingRow>,
}

fn clean(text: &str) -> String {
    redact_tokens(&display_line(text)).trim().to_string()
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The card an event's payload makes; `None` for a payload that places nothing or whose read-back is not `ok`.
fn parse(request_id: &str, v: &Value) -> Option<EngineConfigCard> {
    if v.get("readBack")?.as_str()? != "ok" {
        return None;
    }
    let keys = usize::try_from(v.get("keys")?.as_u64()?).ok()?;
    if keys == 0 {
        return None;
    }
    let engine = v
        .get("engine")
        .and_then(Value::as_str)
        .map(clean)
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "?".to_string());
    let sha256 = v
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|h| is_digest(h))
        .unwrap_or_default()
        .to_string();
    let rows = v
        .get("settings")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    let key = clean(item.get("key")?.as_str()?);
                    let value: String = clean(item.get("value")?.as_str()?)
                        .chars()
                        .take(VALUE_MAX)
                        .collect();
                    (!key.is_empty()).then_some(SettingRow { key, value })
                })
                .take(MAX_ROWS)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(EngineConfigCard {
        request_id: request_id.to_string(),
        engine,
        sha256,
        keys,
        rows,
    })
}

fn config_event(e: &Envelope) -> Option<&Value> {
    if e.event_type != EventType::TurnProgress {
        return None;
    }
    let Payload::Inline { value } = &e.payload else {
        return None;
    };
    (value.get("kind")?.as_str()? == "engine-config").then_some(value)
}

/// The card of the last usable `engine-config` event in `events` (in event-id order), or `None` for a run that placed no engine settings.
pub fn from_events<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> Option<EngineConfigCard> {
    let mut card = None;
    for e in events {
        if let Some(v) = config_event(e)
            && let Some(c) = parse(&e.event_id, v)
        {
            card = Some(c);
        }
    }
    card
}

/// The transcript's one line for an `engine-config` event: `Engine settings: 3 placed (settings hash a1b2c3d4e5f6).`
pub(crate) fn notice_line(v: &Value) -> String {
    let Some(card) = parse("", v) else {
        return "Engine settings: the event could not be read.".to_string();
    };
    let line = format!(
        "Engine settings: {} placed (settings hash {}).",
        card.keys,
        card.short_hash()
    );
    cut(&line, NOTICE_MAX_CHARS)
}

impl EngineConfigCard {
    /// The first 12 digits of the settings hash, or `?` when the event gave none.
    pub fn short_hash(&self) -> &str {
        self.sha256.get(..HASH_SHOWN).unwrap_or("?")
    }

    /// The border title of the card: `engine settings placed · <engine> · 3 set · settings hash a1b2c3d4e5f6`.
    pub fn title(&self) -> String {
        format!(
            "engine settings placed · {} · {} set · settings hash {}",
            self.engine,
            self.keys,
            self.short_hash()
        )
    }

    /// The lines of the card for a screen `width` columns wide: one `key = value` per placed setting, and a count of those left out.
    pub fn lines(&self, width: usize) -> Vec<String> {
        let w = width.max(1);
        let mut out: Vec<String> = self
            .rows
            .iter()
            .map(|r| cut(&format!("{} = {}", r.key, r.value), w))
            .collect();
        if self.keys > self.rows.len() {
            out.push(cut(&format!("and {} more", self.keys - self.rows.len()), w));
        }
        out
    }
}
