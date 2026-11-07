//! The run index: the search item beside each run, the run list that joins it, the server-side filter, and the backfill for
//! runs created before the index existed. The shared pieces (the key, the item, the title, the filter) are `ham_core::run_index`.
//!
//! Where it sits: `PK = OWNER#<sha256 of the caller>`, `SK = SRCH#<createdAt>#<runId>`, with no `ownerPrincipalArn`, so GSI1 (the run
//! list's index, sparse on that attribute) never lists an item. The owner partition is derived from the authenticated caller on the
//! server and from nothing a request says.
//!
//! The list: GSI1 gives a page of up to 100 `RunItem`s, newest first, and one Query of the owner partition over the page's creation times
//! gives their items. The status, the target and the creation time of a row always come from the `RunItem`; the item adds the prompt
//! head, the model, the outcome word and the last activity. A filtered list scans GSI1 pages in order, applies the filter to the joined
//! rows in Rust (DynamoDB has no case-insensitive substring match, and a filter expression would read the same bytes), and stops after the
//! first page that brings it to [`FILTER_FILL`] matches. It never cuts a page, so the token is a plain GSI1 token.
//!
//! Two writers in `control-api`: the start writes the fields of a new item, and the backfill completes the items of runs that predate
//! the index. Both go through `Store::put_run_index`, which sets a field only where it is not set, so neither can overwrite what the end
//! of a run (`run-session`) wrote.

use crate::handler::{
    Blobs, Deps, Execution, Reply, Store, StoreError, V1Request, error, json, run_summary,
    unavailable,
};
use crate::input;
use ham_core::run_index::{
    BACKFILL_MAX_PAGES, BACKFILL_MAX_WRITES, BackfillReport, FILTER_FILL, FILTER_MAX_PAGES,
    RunFilter, RunIndexItem, RunRow, RunRowsPage,
};
use harness_protocol::common::parse_utc_unix;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::RunItem;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, Instant};

/// The time a filtered scan or a backfill call may spend reading pages; the Lambda has 15 s and the answer needs the rest.
pub const SCAN_BUDGET: Duration = Duration::from_secs(8);
/// The time the whole list call may take, the marks of active runs included, so the answer is built and sent before the Lambda's 15 s.
pub const TOTAL_BUDGET: Duration = Duration::from_secs(12);

/// The time limits of one list call: reading pages (a scan that reaches it answers `partial`) and the call as a whole (the tail reads
/// that mark active runs stop at it).
#[derive(Clone, Copy, Debug)]
pub struct Budgets {
    pub scan: Duration,
    pub total: Duration,
}

impl Budgets {
    pub const DEFAULT: Budgets = Budgets {
        scan: SCAN_BUDGET,
        total: TOTAL_BUDGET,
    };
}
/// Tail reads of active runs in flight at once.
const TAIL_WIDTH: usize = 8;

/// Runs `futures` `width` at a time, in order, and returns their outputs in order. Hand-rolled because a new dependency would move the
/// worker image key; the futures borrow the store, so tasks (which need `'static`) do not fit.
pub(crate) async fn join_bounded<F: Future>(futures: Vec<F>, width: usize) -> Vec<F::Output> {
    let mut out = Vec::with_capacity(futures.len());
    let mut rest = futures.into_iter();
    loop {
        let mut chunk: Vec<Pin<Box<F>>> = rest.by_ref().take(width.max(1)).map(Box::pin).collect();
        if chunk.is_empty() {
            return out;
        }
        let mut done: Vec<Option<F::Output>> = chunk.iter().map(|_| None).collect();
        std::future::poll_fn(|cx| {
            let mut pending = false;
            for (f, slot) in chunk.iter_mut().zip(done.iter_mut()) {
                if slot.is_none() {
                    match f.as_mut().poll(cx) {
                        Poll::Ready(v) => *slot = Some(v),
                        Poll::Pending => pending = true,
                    }
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        out.extend(done.into_iter().flatten());
    }
}

/// The index items of a page of runs, by run id: one Query of the owner partition from the page's oldest to its newest creation time.
/// Items of other runs in that window are read and dropped.
async fn join_items<S: Store>(
    store: &S,
    owner: &str,
    runs: &[RunItem],
) -> Result<HashMap<String, RunIndexItem>, StoreError> {
    let (Some(oldest), Some(newest)) = (
        runs.iter().map(|r| r.created_at.as_str()).min(),
        runs.iter().map(|r| r.created_at.as_str()).max(),
    ) else {
        return Ok(HashMap::new());
    };
    let wanted: std::collections::HashSet<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
    Ok(store
        .run_index_between(owner, oldest, newest)
        .await?
        .into_iter()
        .filter(|i| wanted.contains(i.run_id.as_str()))
        .map(|i| (i.run_id.clone(), i))
        .collect())
}

/// The rows of a page of runs: each `RunSummary` with the extras of its item. `Err` is the index read that failed; the rows are
/// then the bare summaries.
async fn rows_of<S: Store>(
    store: &S,
    owner: &str,
    runs: Vec<RunItem>,
) -> (Vec<RunRow>, Result<(), StoreError>) {
    let (items, status) = match join_items(store, owner, &runs).await {
        Ok(m) => (m, Ok(())),
        Err(e) => (HashMap::new(), Err(e)),
    };
    let rows = runs
        .into_iter()
        .map(|r| {
            let item = items.get(&r.run_id);
            RunRow::new(run_summary(r), item)
        })
        .collect();
    (rows, status)
}

/// For each active, non-probe row: the open question (`needsInput`) and the time of the newest event of the direct agent, read from a
/// tail of its shard, [`TAIL_WIDTH`] at a time, until `deadline`. A failed read, or a deadline reached, leaves the rows as they are and
/// adds a diagnostic.
async fn enrich_active<S: Store>(
    store: &S,
    rows: &mut [RunRow],
    logs: &mut Vec<String>,
    deadline: Instant,
) {
    let active: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.summary.status == GoalStatus::Active && !r.summary.is_probe())
        .map(|(i, _)| i)
        .collect();
    for chunk in active.chunks(TAIL_WIDTH) {
        let left = deadline.saturating_duration_since(Instant::now());
        let ids: Vec<String> = chunk
            .iter()
            .map(|i| rows[*i].summary.run_id.clone())
            .collect();
        let reads = ids.iter().map(|id| input::active_tail(store, id)).collect();
        let results = if left.is_zero() {
            None
        } else {
            tokio::time::timeout(left, join_bounded(reads, TAIL_WIDTH))
                .await
                .ok()
        };
        let Some(results) = results else {
            logs.push(
                "needsInput of the remaining active runs skipped: time budget reached".into(),
            );
            return;
        };
        for (i, result) in chunk.iter().copied().zip(results) {
            match result {
                Ok((brief, newest)) => {
                    rows[i].summary.needs_input = brief.map(Box::new);
                    if let Some(ts) = newest
                        && rows[i]
                            .last_activity
                            .as_deref()
                            .is_none_or(|old| ts.as_str() > old)
                    {
                        rows[i].last_activity = Some(ts);
                    }
                }
                Err(e) => logs.push(format!("needsInput of run {}: {e}", rows[i].summary.run_id)),
            }
        }
    }
}

fn unindexed(rows: &[RunRow]) -> u32 {
    u32::try_from(
        rows.iter()
            .filter(|r| !r.summary.is_probe() && !r.is_indexed())
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// `GET /v1/runs`.
pub(crate) async fn list<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    list_rows(req, caller, deps, Budgets::DEFAULT).await
}

/// `GET /v1/runs` within the given time limits.
pub async fn list_rows<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
    budgets: Budgets,
) -> Reply {
    let filter = match RunFilter::from_params(&req.query) {
        Ok(f) => f,
        Err(e) => return error(400, "bad-request", &e.0),
    };
    let store = &deps.store;
    let mut token = req.query.get("nextToken").cloned();
    let started = Instant::now();
    let mut logs: Vec<String> = Vec::new();
    let mut rows: Vec<RunRow> = Vec::new();
    let mut partial = false;
    let mut index_ok = true;
    // Rows read that predate the index, counted before the filter: a `q` search cannot match them, and it is then that the client needs
    // to be told they exist.
    let mut unindexed_rows = 0u32;
    let since = filter.since.as_deref().and_then(parse_utc_unix);
    let mut pages = 0usize;
    loop {
        let (runs, next) = match store.list_runs(caller, token.as_deref()).await {
            Ok(page) => page,
            Err(StoreError::BadCursor) => {
                return error(400, "bad-request", "nextToken is not valid");
            }
            Err(e) => return unavailable(&e, "retry"),
        };
        pages += 1;
        // Rows are newest first: once one is older than `since`, no later row can match.
        let past_since = since.is_some_and(|s| {
            runs.iter()
                .any(|r| parse_utc_unix(&r.created_at).is_some_and(|c| c < s))
        });
        let (page_rows, joined) = rows_of(store, caller, runs).await;
        if joined.is_ok() {
            unindexed_rows = unindexed_rows.saturating_add(unindexed(&page_rows));
        }
        if let Err(e) = joined {
            // A search on what only the index holds would answer "no such run" for a store that is merely slow.
            if filter.q.is_some() {
                return unavailable(&e, "retry");
            }
            index_ok = false;
            logs.push(format!("run index: {e}"));
        }
        if filter.is_empty() {
            rows = page_rows;
            token = next;
            break;
        }
        rows.extend(page_rows.into_iter().filter(|r| filter.matches(r)));
        token = next;
        if past_since {
            token = None;
        }
        if token.is_none() || rows.len() >= FILTER_FILL {
            break;
        }
        if pages >= FILTER_MAX_PAGES || started.elapsed() >= budgets.scan {
            partial = true;
            break;
        }
    }
    enrich_active(store, &mut rows, &mut logs, started + budgets.total).await;
    let unindexed = if index_ok { unindexed_rows } else { 0 };
    let (resp, _) = json(
        200,
        &RunRowsPage {
            runs: rows,
            next_token: token,
            partial,
            unindexed,
        },
    );
    (resp, logs)
}

/// The index item `POST /v1/direct/runs` writes for a run it started or found: the run's own `createdAt`, so a retry addresses the same
/// key. A failure is a diagnostic, never a failed start; the backfill makes the item later.
pub(crate) async fn write_at_start<S: Store>(
    store: &S,
    owner: &str,
    run: &RunItem,
    prompt: &str,
    model: &str,
) -> Option<String> {
    let item = RunIndexItem::at_start(
        &run.run_id,
        &run.created_at,
        prompt,
        Some(model),
        &run.target,
    );
    match store.put_run_index(owner, &item).await {
        Ok(_) => None,
        Err(e) => Some(format!("run index of {}: {e}", run.run_id)),
    }
}

/// `POST /v1/runs/index`: makes the index items of the caller's runs that lack one (or lack the prompt head). Makes at most
/// [`BACKFILL_MAX_WRITES`] write attempts (every attempt counts, whatever it found) and reads at most [`BACKFILL_MAX_PAGES`] pages of 100
/// runs per call, and stops between two runs when [`SCAN_BUDGET`] is spent (after at least one write, so every call makes progress);
/// `more` says to call again, with `nextToken` when there is one. A run with no goal item cannot be described and is skipped without
/// holding the scan back.
pub(crate) async fn backfill<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    backfill_rows(req, caller, deps, SCAN_BUDGET).await
}

/// [`backfill`] with the time it may spend.
pub async fn backfill_rows<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    caller: &str,
    deps: &Deps<S, B, E>,
    budget: Duration,
) -> Reply {
    let store = &deps.store;
    let started = Instant::now();
    let mut report = BackfillReport::default();
    let mut writes = 0usize;
    let mut pages = 0usize;
    let mut page_token = req.query.get("nextToken").cloned();
    loop {
        let (runs, next) = match store.list_runs(caller, page_token.as_deref()).await {
            Ok(page) => page,
            Err(StoreError::BadCursor) => {
                return error(400, "bad-request", "nextToken is not valid");
            }
            Err(e) => return unavailable(&e, "retry"),
        };
        pages += 1;
        let items = match join_items(store, caller, &runs).await {
            Ok(m) => m,
            Err(e) => return unavailable(&e, "retry"),
        };
        for run in &runs {
            report.scanned += 1;
            if items.get(&run.run_id).is_some_and(|i| !i.is_partial()) {
                report.already_indexed += 1;
                continue;
            }
            // Time is checked between runs, not only between pages: one page is up to 100 runs of three requests each.
            if writes >= 1 && started.elapsed() >= budget {
                report.more = true;
                report.next_token = page_token;
                return json(200, &report);
            }
            let goal = match store.get_root_goal(&run.run_id).await {
                // A goal with no text cannot be searched, and an item with an empty prompt head would stay unfinished for ever.
                Ok(Some(g)) if !g.objective.trim().is_empty() => g,
                Ok(_) => {
                    report.skipped += 1;
                    continue;
                }
                Err(e) => return unavailable(&e, "retry"),
            };
            let model = match store.list_agents(&run.run_id).await {
                Ok(agents) => agents
                    .into_iter()
                    .find(|a| a.agent_id == DIRECT_AGENT_ID)
                    .map(|a| a.model_id),
                Err(e) => return unavailable(&e, "retry"),
            };
            let item = RunIndexItem::at_start(
                &run.run_id,
                &run.created_at,
                &goal.objective,
                model.as_deref(),
                &run.target,
            );
            writes += 1;
            match store.put_run_index(caller, &item).await {
                Ok(true) => report.indexed += 1,
                Ok(false) => report.already_indexed += 1,
                Err(e) => return unavailable(&e, "retry"),
            }
            if writes >= BACKFILL_MAX_WRITES {
                // Resume at this page: its finished rows are skipped by the join next time.
                report.more = true;
                report.next_token = page_token;
                return json(200, &report);
            }
        }
        match next {
            None => return json(200, &report),
            Some(t) => {
                if pages >= BACKFILL_MAX_PAGES || started.elapsed() >= budget {
                    report.more = true;
                    report.next_token = Some(t);
                    return json(200, &report);
                }
                page_token = Some(t);
            }
        }
    }
}
