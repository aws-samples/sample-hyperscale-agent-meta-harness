//! The events route read from the end: `GET /v1/runs/{runId}/events?shard=&tail=&before=`.
//!
//! `tail=N` returns the newest N events of one shard (1 to [`MAX_TAIL`]), oldest first; `before=<eventId>` returns the newest N events
//! strictly older than that id, so a client opens the end of a long run in one call and pages back from there. The answer is
//! [`EventsView`], a superset of `harness_protocol::api::v1::EventsPage`: the two old members keep their meaning, so an old client reads it
//! unchanged. The new members live here, outside the worker image key.
//!
//! `control-api` and `ham` share [`EventsQuery::from_params`] and [`EventsQuery::to_path_query`], so a client never sends what the server
//! refuses, and the server refuses what the contract refuses.

use harness_protocol::common::is_ulid;
use harness_protocol::direct_input::InstructAction;
use harness_protocol::envelope::Envelope;
use harness_protocol::store::EVENT_SHARDS;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The page `before=` alone asks for.
pub const DEFAULT_TAIL: usize = 50;
/// The most events one read returns, forward or backward: the old forward page size.
pub const MAX_TAIL: usize = 500;
/// The most controls one read of the user's answers looks at. A page whose list was cut says so ([`EventsView::answers_truncated`]).
pub const MAX_ANSWERS: usize = 200;
/// The most bytes of answers one page carries, counted as they sit inside the Lambda proxy answer (the page is a JSON string in that
/// answer, so its quotation marks and backslashes are escaped a second time); past it the list is cut and says so.
pub const ANSWERS_BYTE_BUDGET: usize = 1_000_000;
/// The most bytes of events one page carries, counted the same way, so the whole answer stays under the 6 MB a Lambda may return (the
/// answers and the cursors included).
pub const EVENTS_BYTE_BUDGET: usize = 4_500_000;

/// What the query asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventsRead {
    /// The events after the cursor (the start of the log when none), oldest first.
    Forward { after: Option<String> },
    /// The newest `count` events, or the newest `count` older than `before`, oldest first.
    Tail {
        count: usize,
        before: Option<String>,
    },
}

/// A parsed events query: one shard and one kind of read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventsQuery {
    pub shard: u8,
    pub read: EventsRead,
}

/// Why a query is refused; the text names the parameter.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct QueryError(pub String);

fn digits(s: &str, max_len: usize) -> Option<u32> {
    if s.is_empty() || s.len() > max_len || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl EventsQuery {
    pub fn forward(shard: u8, after: Option<&str>) -> Self {
        Self {
            shard,
            read: EventsRead::Forward {
                after: after.map(str::to_string),
            },
        }
    }

    /// The newest `count` events of `shard`.
    pub fn tail(shard: u8, count: usize) -> Self {
        Self {
            shard,
            read: EventsRead::Tail {
                count,
                before: None,
            },
        }
    }

    /// The newest `count` events of `shard` older than the event `before`.
    pub fn older(shard: u8, before: &str, count: usize) -> Self {
        Self {
            shard,
            read: EventsRead::Tail {
                count,
                before: Some(before.to_string()),
            },
        }
    }

    /// Reads the query parameters of the route. `shard` is required (0 to 31). `after` and `tail`/`before` are two different reads and
    /// cannot be mixed; `tail` is 1 to [`MAX_TAIL`] and `before` alone means [`DEFAULT_TAIL`].
    pub fn from_params(params: &BTreeMap<String, String>) -> Result<Self, QueryError> {
        let shard = params
            .get("shard")
            .and_then(|s| digits(s, 2))
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| *n < EVENT_SHARDS)
            .ok_or_else(|| {
                QueryError(format!(
                    "shard must be an integer from 0 to {}",
                    EVENT_SHARDS - 1
                ))
            })?;
        let ulid = |name: &str| -> Result<Option<String>, QueryError> {
            match params.get(name) {
                None => Ok(None),
                Some(v) if is_ulid(v) => Ok(Some(v.clone())),
                Some(_) => Err(QueryError(format!("{name} must be an eventId (ULID)"))),
            }
        };
        let after = ulid("after")?;
        let before = ulid("before")?;
        let tail = match params.get("tail") {
            None => None,
            Some(t) => Some(
                digits(t, 3)
                    .map(|n| n as usize)
                    .filter(|n| (1..=MAX_TAIL).contains(n))
                    .ok_or_else(|| {
                        QueryError(format!("tail must be an integer from 1 to {MAX_TAIL}"))
                    })?,
            ),
        };
        if after.is_some() && (tail.is_some() || before.is_some()) {
            return Err(QueryError(
                "after reads forward; it cannot be combined with tail or before".into(),
            ));
        }
        let read = match (after, tail, before) {
            (after, None, None) => EventsRead::Forward { after },
            (_, count, before) => EventsRead::Tail {
                count: count.unwrap_or(DEFAULT_TAIL),
                before,
            },
        };
        Ok(Self { shard, read })
    }

    /// The query string of the route (no leading `?`); every value is a number or a ULID, so none needs escaping.
    pub fn to_path_query(&self) -> String {
        let mut q = format!("shard={}", self.shard);
        match &self.read {
            EventsRead::Forward { after } => {
                if let Some(a) = after {
                    q.push_str("&after=");
                    q.push_str(a);
                }
            }
            EventsRead::Tail { count, before } => {
                q.push_str(&format!("&tail={count}"));
                if let Some(b) = before {
                    q.push_str("&before=");
                    q.push_str(b);
                }
            }
        }
        q
    }
}

/// The user's answer to a question of a direct run: what `POST /v1/runs/{runId}/input` stored, returned with the question it answers
/// (`request_id` is the question's event id). Only an answer a person gave through the API is returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAnswer {
    pub request_id: String,
    pub action: InstructAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub created_at: String,
    /// When agentd took the answer; absent while it waits in the queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One page of events. A forward read without a question on the page serializes exactly as `EventsPage` does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsView {
    /// Oldest first.
    pub events: Vec<Envelope>,
    /// The last `eventId` returned: the cursor of the next forward read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
    /// Tail and `before` reads only, when older events exist: the first `eventId` returned, the `before` of the next older page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before: Option<String>,
    /// Tail and `before` reads only: older events exist than the ones returned. On a forward read it is left out.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_more: bool,
    /// The answers to the questions on this page, when the page has any question that was answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<UserAnswer>,
    /// The read of the run's answers stopped at [`MAX_ANSWERS`]: an answer missing from `answers` may exist.
    #[serde(default, skip_serializing_if = "is_false")]
    pub answers_truncated: bool,
}
