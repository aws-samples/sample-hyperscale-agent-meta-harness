//! `GET /v1/search?q=&since=&limit=&after=`: chat-content search over the caller's runs.
//!
//! Each run has one small search item under its owner's partition (`PK = OWNER#<sha256 of the caller's ARN>`, `SK = SRCH#<createdAt>#
//! <runId>`, no `ownerPrincipalArn`, so the run-list index never holds it), written when the run is finalized (`ham_core::runsearch`
//! builds its body of at most 32 KiB). The route reads that partition newest first with a short sequence of Queries (never a scan,
//! never a transcript object) and matches in the function: every word of `q` must occur in a run's body, prompt, repository or model,
//! one hit per run. It examines at most `limit` runs (default and maximum 200) newer than `since` (default: the first run), stops at its
//! deadline with `partial: true`, and gives a `next` token to carry on from when older runs remain. A hit that matched the chat carries
//! the chunk and record where the matching piece starts and an `open` cursor for the transcript route.
//!
//! The partition key is made from the caller's own ARN, so one caller's words can never match another's runs. `q` is a query value:
//! the access line logs the path only, no diagnostic carries it, and no snippet is written to a log.

use crate::cursor::{self, Position};
use crate::handler::{Blobs, Deps, Execution, Reply, Store, V1Request, error, json, unavailable};
use crate::ids::rfc3339;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ham_core::redact::redact_tokens;
use ham_core::runsearch::{
    Coverage, DEFAULT_LIMIT, MAX_LIMIT, SK_PREFIX, SearchHit, SearchItem, SearchResponse,
    find_match_redacted, owner_pk, parse_query, sk_floor,
};
use harness_protocol::common::{is_ulid, is_utc_timestamp};
use std::time::{Duration, Instant};

/// The time a search may take before it returns what it has.
pub const DEADLINE: Duration = Duration::from_secs(10);
/// The most Query calls one search makes.
pub const MAX_QUERY_CALLS: usize = 20;
/// Items asked of one Query (DynamoDB stops at 1 MB of items anyway).
pub const ITEMS_PER_CALL: usize = 100;

/// One read of the owner's partition.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchPage {
    /// The items that could be decoded, newest first.
    pub items: Vec<SearchItem>,
    /// How many items the read examined, decoded or not.
    pub examined: u32,
    /// The sort key of the last item examined, decoded or not: a search that goes on goes on after it, so an item the store could
    /// not decode never moves the continuation.
    pub last_sk: Option<String>,
    /// The sort key to continue after (older items follow it), when the partition holds more in this range.
    pub next_sk: Option<String>,
}

/// The limits of one search; tests lower them.
#[derive(Clone, Debug)]
pub struct SearchLimits {
    pub deadline: Duration,
    pub max_calls: usize,
    pub per_call: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            deadline: DEADLINE,
            max_calls: MAX_QUERY_CALLS,
            per_call: ITEMS_PER_CALL,
        }
    }
}

/// The oldest creation time a search reaches when `since` is absent: the start of the clock, so every run is in reach and the page of runs
/// (`limit`) and the continuation token bound one call.
pub fn default_since() -> String {
    rfc3339(0)
}

fn valid_since(s: &str) -> bool {
    if is_utc_timestamp(s) {
        return true;
    }
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
        && (1..=12).contains(&s[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&s[8..10].parse::<u32>().unwrap_or(0))
}

fn token_of(sk: &str) -> String {
    URL_SAFE_NO_PAD.encode(sk.as_bytes())
}

fn sk_of(token: &str) -> Option<String> {
    if token.len() > 512 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
    let sk = String::from_utf8(bytes).ok()?;
    (sk.starts_with(SK_PREFIX) && sk.len() <= 256).then_some(sk)
}

/// The route with its default limits.
pub(crate) async fn get_search<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    search_with(req, caller, deps, &SearchLimits::default()).await
}

/// The route with explicit limits.
pub async fn search_with<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
    limits: &SearchLimits,
) -> Reply {
    let started = Instant::now();
    let terms = match parse_query(req.query.get("q").map_or("", String::as_str)) {
        Ok(t) => t,
        Err(e) => return error(400, "bad-request", &format!("q: {e}")),
    };
    let since = match req.query.get("since") {
        None => default_since(),
        Some(s) if valid_since(s) => s.clone(),
        Some(_) => {
            return error(
                400,
                "bad-request",
                "since must be a date (YYYY-MM-DD) or a UTC time",
            );
        }
    };
    let limit = match req.query.get("limit").map(String::as_str) {
        None => DEFAULT_LIMIT,
        Some(n) if (1..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()) => {
            match n.parse::<u32>() {
                Ok(v) if (1..=MAX_LIMIT).contains(&v) => v,
                _ => {
                    return error(
                        400,
                        "bad-request",
                        &format!("limit must be from 1 to {MAX_LIMIT}"),
                    );
                }
            }
        }
        Some(_) => {
            return error(
                400,
                "bad-request",
                &format!("limit must be from 1 to {MAX_LIMIT}"),
            );
        }
    };
    let mut sk_after = match req.query.get("after") {
        None => None,
        Some(t) => match sk_of(t) {
            Some(sk) => Some(sk),
            None => return error(400, "bad-request", "after is not a token this API issued"),
        },
    };
    let pk = owner_pk(caller);
    let floor = sk_floor(&since);
    let mut out = SearchResponse::default();
    let mut calls = 0usize;
    // The position a search that stops early hands back: where this one began, then the last item examined.
    let mut last_sk: Option<String> = sk_after.clone();
    let mut diagnostics: Vec<String> = Vec::new();
    loop {
        let room = limit.saturating_sub(out.searched) as usize;
        if room == 0 {
            break;
        }
        if calls >= limits.max_calls || started.elapsed() >= limits.deadline {
            out.partial = started.elapsed() >= limits.deadline;
            out.more = true;
            break;
        }
        calls += 1;
        // A read that outlasts the deadline is cut off: what the search has so far is returned, and the token goes on from there.
        let remaining = limits
            .deadline
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1));
        let read = tokio::time::timeout(
            remaining,
            deps.store
                .search_items(&pk, &floor, sk_after.as_deref(), room.min(limits.per_call)),
        )
        .await;
        let page = match read {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return unavailable(&e, "retry"),
            Err(_) => {
                out.partial = true;
                out.more = true;
                break;
            }
        };
        out.searched = out.searched.saturating_add(page.examined);
        if page.last_sk.is_some() {
            last_sk.clone_from(&page.last_sk);
        }
        for item in page.items {
            if item.coverage == Some(Coverage::Partial) {
                out.partial_coverage += 1;
            }
            if let Some(hit) = hit_of(&item, &terms, caller) {
                out.hits.push(hit);
            }
        }
        match page.next_sk {
            Some(sk) if page.examined > 0 => sk_after = Some(sk),
            _ => {
                sk_after = None;
                break;
            }
        }
    }
    // Older runs remain when the loop stopped on the limit, a cap or the deadline with a place to continue from.
    if sk_after.is_some() {
        out.more = true;
    }
    if out.more {
        out.next = last_sk.as_deref().map(token_of);
    }
    if out.partial {
        diagnostics.push("search stopped at its deadline".to_string());
    }
    let (resp, _) = json(200, &out);
    (resp, diagnostics)
}

fn hit_of(item: &SearchItem, terms: &[Vec<char>], caller: &str) -> Option<SearchHit> {
    if !is_ulid(&item.run_id) {
        return None;
    }
    let m = find_match_redacted(item, terms, &redact_tokens)?;
    let open = match (m.seq, m.offset) {
        (Some(seq), Some(off)) => Some(cursor::encode(
            &item.run_id,
            caller,
            &Position::at(seq, off),
        )),
        _ => None,
    };
    Some(SearchHit {
        run_id: item.run_id.clone(),
        created_at: item.created_at.clone(),
        field: m.field,
        snippet: m.snippet,
        seq: m.seq,
        offset: m.offset,
        open,
        coverage: item.coverage,
    })
}
