//! `GET /v1/runs/{runId}/events`: one shard of a run's log read forward (`after`), or from the end (`tail`, `before`), with
//! the user's answer text beside the question it answers.
//!
//! The query is `ham_core::events_view::EventsQuery`, the one parser the client builds its text with. The answer is
//! `ham_core::events_view::EventsView`, a superset of the old page: a forward read with no answered question on the page serializes as
//! before. Everything is owner-only (another caller's run is a 404, as on every run route) and bounded: at most 500 events, at most
//! [`EVENTS_BYTE_BUDGET`] serialized bytes of events, at most [`MAX_ANSWERS`] controls and [`ANSWERS_BYTE_BUDGET`] bytes of answers.

use crate::handler::{
    Blobs, Deps, EVENTS_PAGE, Execution, Reply, Store, V1Request, error, json, not_found, owned,
    unavailable,
};
use crate::input;
use ham_core::events_view::{
    ANSWERS_BYTE_BUDGET, EVENTS_BYTE_BUDGET, EventsQuery, EventsRead, EventsView, MAX_ANSWERS,
    UserAnswer,
};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::direct_input::InstructOrigin;
use harness_protocol::envelope::Envelope;
use std::collections::BTreeSet;

/// The bytes `value` takes inside the Lambda proxy answer: the page is a JSON string in that answer, so what the page's JSON escapes
/// (quotation marks, backslashes, control characters) is escaped a second time. Measuring the inner JSON alone let a page dense in quotation
/// marks cross the 6 MB limit.
fn proxied_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value)
        .ok()
        .and_then(|inner| serde_json::to_string(&inner).ok())
        .map_or(0, |outer| outer.len())
}

/// Bytes of one event as the page carries it, a separator included.
fn size_of(e: &Envelope) -> usize {
    proxied_len(e) + 1
}

/// The events that fit [`EVENTS_BYTE_BUDGET`], keeping the newest (`keep_newest`, a read from the end) or the oldest (a forward read);
/// at least one is always kept. `events` is oldest first. Returns whether any were left out.
fn fit(events: &mut Vec<Envelope>, keep_newest: bool) -> bool {
    let mut total = 0usize;
    let mut kept = 0usize;
    let order: Box<dyn Iterator<Item = &Envelope>> = if keep_newest {
        Box::new(events.iter().rev())
    } else {
        Box::new(events.iter())
    };
    for e in order {
        total += size_of(e);
        if kept > 0 && total > EVENTS_BYTE_BUDGET {
            break;
        }
        kept += 1;
    }
    let cut = events.len() - kept;
    if cut == 0 {
        return false;
    }
    if keep_newest {
        events.drain(..cut);
    } else {
        events.truncate(kept);
    }
    true
}

/// The user's answers to the questions among `events`: one read of the run's controls of the direct agent, kept when a question on the
/// page has the control's id and a person wrote the control. A failed read is a diagnostic, never a failed page.
async fn answers_for<S: Store>(
    store: &S,
    run_id: &str,
    events: &[Envelope],
    logs: &mut Vec<String>,
) -> (Vec<UserAnswer>, bool) {
    let questions: BTreeSet<&str> = events
        .iter()
        .filter(|e| input::admit(run_id, Some(e)).is_ok())
        .map(|e| e.event_id.as_str())
        .collect();
    // The window of the questions on this page: a control's id is its question's id, so the answers of the newest questions of a long run
    // are found as well as the oldest.
    let (Some(oldest), Some(newest)) = (questions.first(), questions.last()) else {
        return (Vec::new(), false);
    };
    let (controls, more) = match store
        .list_controls(run_id, DIRECT_AGENT_ID, oldest, newest, MAX_ANSWERS)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            logs.push(format!("answers of run {run_id}: {e}"));
            return (Vec::new(), false);
        }
    };
    let mut answers = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = more;
    for c in controls {
        if !questions.contains(c.control_id.as_str()) {
            continue;
        }
        let PendingControl::Instruct {
            action,
            text,
            model,
            origin: InstructOrigin::User,
            ..
        } = c.control
        else {
            continue;
        };
        let answer = UserAnswer {
            request_id: c.control_id,
            action,
            text,
            model,
            created_at: c.created_at,
            delivered_at: c.delivered_at,
        };
        bytes += proxied_len(&answer) + 1;
        if bytes > ANSWERS_BYTE_BUDGET {
            truncated = true;
            break;
        }
        answers.push(answer);
    }
    (answers, truncated)
}

pub(crate) async fn read<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    let query = match EventsQuery::from_params(&req.query) {
        Ok(q) => q,
        Err(e) => return error(400, "bad-request", &e.0),
    };
    match owned(&deps.store, run_id, caller).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(&format!("no run {run_id} for this caller")),
        Err(e) => return unavailable(&e, "retry"),
    }
    let (mut events, mut has_more, after) = match &query.read {
        EventsRead::Forward { after } => {
            match deps
                .store
                .events(run_id, query.shard, after.as_deref(), EVENTS_PAGE)
                .await
            {
                Ok(ev) => (ev, false, after.clone()),
                Err(e) => return unavailable(&e, "retry"),
            }
        }
        EventsRead::Tail { count, before } => {
            // One more than asked, to know whether older events exist; the oldest of them is not part of the page.
            match deps
                .store
                .events_before(run_id, query.shard, before.as_deref(), count + 1)
                .await
            {
                Ok(mut ev) => {
                    let more = ev.len() > *count;
                    if more {
                        ev.remove(0);
                    }
                    (ev, more, None)
                }
                Err(e) => return unavailable(&e, "retry"),
            }
        }
    };
    let backward = matches!(query.read, EventsRead::Tail { .. });
    if fit(&mut events, backward) && backward {
        has_more = true;
    }
    let mut logs = Vec::new();
    let (answers, answers_truncated) = answers_for(&deps.store, run_id, &events, &mut logs).await;
    let next_after = events.last().map(|e| e.event_id.clone()).or(after);
    let next_before = if has_more {
        events.first().map(|e| e.event_id.clone())
    } else {
        None
    };
    let (resp, _) = json(
        200,
        &EventsView {
            events,
            next_after,
            next_before,
            has_more,
            answers,
            answers_truncated,
        },
    );
    (resp, logs)
}
