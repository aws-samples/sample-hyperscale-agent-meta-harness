//! The run index. `POST /v1/direct/runs` writes a small owner-keyed search item beside the run; `GET /v1/runs` joins it
//! to the page of runs GSI1 returned and answers `RunRow`s (a `RunSummary` plus title, model and last activity) that an old client
//! still parses; `q`, `target`, `status` and `since` filter on the server; `POST /v1/runs/index` builds the item for runs that were
//! created before the index existed. A second principal never sees, matches or indexes another principal's runs.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{
    ME, OTHER, SONNET, World, agent_item, at, deps, get, get_logged, goal_item, message, post,
    question, request, run_item, ulid,
};
use control_api::run_index::Budgets;
use ham_core::run_index::{
    BackfillReport, PROMPT_HEAD_CHARS, RunIndexItem, RunRowsPage, owner_pk, search_sk,
};
use harness_protocol::api::v1::ListRunsResponse;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const S3_ONLY: fn() -> Value = || json!({"mode": "s3-only"});

/// A run id: ULIDs are 26 Crockford characters, `01K5` plus 22 digits is one, and a bigger `i` is a newer run (ids and `at(i)` both sort).
fn rid(i: u64) -> String {
    format!("01K5{i:022}")
}

fn index_of(owner_run: &str, i: u64, prompt: &str) -> RunIndexItem {
    RunIndexItem {
        run_id: owner_run.into(),
        created_at: at(i),
        prompt_head: prompt.into(),
        prompt_truncated: false,
        model: Some(SONNET.into()),
        repository: "s3-only".into(),
        outcome: None,
        last_activity: at(i),
        run_name: None,
        run_name_source: None,
        run_name_model: None,
        ..Default::default()
    }
}

/// `n` runs of `owner`: run `i` is created at `at(i)`, says "calc" when `i % 7 == 0`, and is indexed when `indexed`.
fn seed(w: &World, owner: &str, n: u64, indexed: bool) {
    for i in 1..=n {
        let id = rid(i);
        w.add_run(run_item(&id, owner, "complete", &at(i), S3_ONLY()));
        let prompt = if i % 7 == 0 {
            format!("Build a calc tool number {i}\nwith tests")
        } else {
            format!("Write the parser number {i}\nplease")
        };
        w.goals
            .lock()
            .unwrap()
            .insert(id.clone(), goal_item(&id, &prompt));
        w.agents
            .lock()
            .unwrap()
            .push(agent_item(&id, SONNET, "stopped"));
        if indexed {
            w.add_index(owner, index_of(&id, i, &prompt));
        }
    }
}

async fn list(w: &World, caller: &str, q: &[(&str, &str)]) -> (u16, Value, Vec<String>) {
    get_logged(w, caller, "/v1/runs", q).await
}

fn run_ids(body: &Value) -> Vec<String> {
    body["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["runId"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_run_ids_of_the_fixture_are_real_ulids() {
    for i in [1, 7, 300] {
        assert!(harness_protocol::common::is_ulid(&rid(i)), "{}", rid(i));
    }
    assert!(rid(2) > rid(1));
}

// ---- the start writes the item ------------------------------------------------------------------------------------------

fn start_body(prompt: &str) -> String {
    json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": SONNET}).to_string()
}

async fn start(w: &World, caller: &str, key: &str, prompt: &str) -> (u16, Value, Vec<String>) {
    let mut req = request(caller, "POST", "/v1/direct/runs", &[], &start_body(prompt));
    req.headers.insert("idempotency-key".into(), key.into());
    let (out, logs) = control_api::handler::handle_logged(req, &deps(w)).await;
    (out.status, serde_json::from_str(&out.body).unwrap(), logs)
}

#[tokio::test]
async fn a_start_writes_the_search_item_under_the_callers_partition() {
    let w = World::new();
    let (status, body, _) = start(&w, ME, "k-1", "Add percent_change(a, b)\nwith tests").await;
    assert_eq!(status, 201, "{body}");
    let run = body["runId"].as_str().unwrap();
    let items = w.indexed(ME);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.run_id, run);
    assert_eq!(item.prompt_head, "Add percent_change(a, b)\nwith tests");
    assert!(!item.prompt_truncated);
    assert_eq!(item.model.as_deref(), Some(SONNET));
    assert_eq!(item.repository, "s3-only");
    assert_eq!(item.outcome, None);
    let created = w.runs.lock().unwrap().get(run).unwrap().created_at.clone();
    assert_eq!(item.created_at, created);
    assert_eq!(
        item.last_activity, created,
        "last activity starts at the creation"
    );
    // The key is the hash of the caller and the time, then the run; nobody else has one.
    assert!(w.indexed(OTHER).is_empty());
    assert!(owner_pk(ME).starts_with("OWNER#"));
    assert_eq!(search_sk(&created, run), format!("SRCH#{created}#{run}"));
}

#[tokio::test]
async fn a_long_prompt_keeps_its_first_two_thousand_characters_and_says_so() {
    let w = World::new();
    let prompt = format!("{}{}", "é".repeat(PROMPT_HEAD_CHARS), "more");
    let (status, _, _) = start(&w, ME, "k-long", &prompt).await;
    assert_eq!(status, 201);
    let item = &w.indexed(ME)[0];
    assert_eq!(item.prompt_head.chars().count(), PROMPT_HEAD_CHARS);
    assert!(item.prompt_truncated);
}

#[tokio::test]
async fn a_retry_with_the_same_key_adds_nothing_and_the_run_is_the_same() {
    let w = World::new();
    let (_, first, _) = start(&w, ME, "k-2", "Add percent_change").await;
    let (status, second, logs) = start(&w, ME, "k-2", "Add percent_change").await;
    assert_eq!(status, 201);
    assert_eq!(first["runId"], second["runId"]);
    assert_eq!(w.indexed(ME).len(), 1);
    assert!(logs.is_empty(), "{logs:?}");
}

#[tokio::test]
async fn a_failed_index_write_never_fails_the_start_and_the_retry_heals_it() {
    let w = World::new();
    w.index_write_down.store(true, Ordering::SeqCst);
    let (status, first, logs) = start(&w, ME, "k-3", "Add percent_change").await;
    assert_eq!(status, 201, "{first}");
    assert!(w.indexed(ME).is_empty());
    assert!(logs.iter().any(|l| l.contains("index")), "{logs:?}");
    // The store comes back and the client sends the same request again: the run is the same and the item now exists.
    w.index_write_down.store(false, Ordering::SeqCst);
    let (status, second, _) = start(&w, ME, "k-3", "Add percent_change").await;
    assert_eq!(status, 201);
    assert_eq!(first["runId"], second["runId"]);
    assert_eq!(w.indexed(ME).len(), 1);
}

// ---- the list ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_list_row_carries_title_model_and_last_activity() {
    let w = World::new();
    seed(&w, ME, 3, true);
    let (status, body, logs) = list(&w, ME, &[]).await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    let first = &body["runs"][0];
    assert_eq!(first["runId"], rid(3));
    assert_eq!(first["title"], "Write the parser number 3");
    assert_eq!(first["model"], SONNET);
    assert_eq!(first["lastActivity"], at(3));
    // One read of the index for the whole page, not one per row.
    assert_eq!(w.count("run_index_between"), 1);
}

#[tokio::test]
async fn a_run_without_an_index_item_is_listed_bare() {
    let w = World::new();
    seed(&w, ME, 2, false);
    let (status, body, _) = list(&w, ME, &[]).await;
    assert_eq!(status, 200);
    let r = &body["runs"][0];
    assert!(
        r.get("title").is_none() && r.get("model").is_none() && r.get("lastActivity").is_none(),
        "{r}"
    );
}

#[tokio::test]
async fn the_list_has_the_same_rows_and_page_sizes_with_and_without_index_items() {
    let (bare, rich) = (World::new(), World::new());
    seed(&bare, ME, 150, false);
    seed(&rich, ME, 150, true);
    let mut tokens = (None::<String>, None::<String>);
    let mut sizes = Vec::new();
    for _ in 0..2 {
        let q = |t: &Option<String>| {
            t.as_ref()
                .map(|t| vec![("nextToken", t.clone())])
                .unwrap_or_default()
        };
        let qa = q(&tokens.0);
        let qb = q(&tokens.1);
        let (_, a, _) = list(
            &bare,
            ME,
            &qa.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>(),
        )
        .await;
        let (_, b, _) = list(
            &rich,
            ME,
            &qb.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>(),
        )
        .await;
        assert_eq!(run_ids(&a), run_ids(&b));
        // Strip the extras: what is left is the summary the old list sent.
        let mut stripped = b.clone();
        for r in stripped["runs"].as_array_mut().unwrap() {
            for k in [
                "title",
                "model",
                "lastActivity",
                "outcome",
                "name",
                "nameSource",
                "tags",
            ] {
                r.as_object_mut().unwrap().remove(k);
            }
        }
        assert_eq!(a["runs"], stripped["runs"]);
        sizes.push(run_ids(&a).len());
        tokens = (
            a.get("nextToken").and_then(Value::as_str).map(Into::into),
            b.get("nextToken").and_then(Value::as_str).map(Into::into),
        );
        assert_eq!(tokens.0.is_some(), tokens.1.is_some());
    }
    assert_eq!(sizes, vec![100, 50]);
}

#[tokio::test]
async fn an_old_client_parses_the_new_list() {
    let w = World::new();
    seed(&w, ME, 5, true);
    let out =
        control_api::handler::handle(request(ME, "GET", "/v1/runs", &[], ""), &deps(&w)).await;
    let old: ListRunsResponse = serde_json::from_str(&out.body).unwrap();
    assert_eq!(old.runs.len(), 5);
    let new: RunRowsPage = serde_json::from_str(&out.body).unwrap();
    assert_eq!(new.runs[0].summary, old.runs[0]);
    assert_eq!(
        new.runs[0].title.as_deref(),
        Some("Write the parser number 5")
    );
}

#[tokio::test]
async fn a_failed_index_read_leaves_bare_rows_and_never_fails_the_list() {
    let w = World::new();
    seed(&w, ME, 3, true);
    w.index_read_down.store(true, Ordering::SeqCst);
    let (status, body, logs) = list(&w, ME, &[]).await;
    assert_eq!(status, 200);
    assert_eq!(run_ids(&body).len(), 3);
    assert!(body["runs"][0].get("title").is_none());
    assert!(logs.iter().any(|l| l.contains("index")), "{logs:?}");
}

#[tokio::test]
async fn a_search_that_needs_the_index_and_cannot_read_it_is_a_503_not_an_empty_answer() {
    let w = World::new();
    seed(&w, ME, 3, true);
    w.index_read_down.store(true, Ordering::SeqCst);
    // `q` reads what only the index holds: an empty answer would read as "no such run".
    let (status, body, logs) = list(&w, ME, &[("q", "parser")]).await;
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains("throttled"));
    assert!(logs.iter().any(|l| l.contains("index")), "{logs:?}");
    // The filters the run list itself can answer do not need it.
    let (status, body, _) = list(&w, ME, &[("status", "complete")]).await;
    assert_eq!(status, 200);
    assert_eq!(run_ids(&body).len(), 3);
}

#[tokio::test]
async fn a_page_counts_the_runs_that_predate_the_index() {
    let w = World::new();
    seed(&w, ME, 5, true);
    let old = rid(2);
    w.index
        .lock()
        .unwrap()
        .retain(|(_, sk), _| !sk.ends_with(&old));
    let (_, body, _) = list(&w, ME, &[]).await;
    assert_eq!(body["unindexed"], 1, "{body}");
    let (_, body, _) = list(&w, ME, &[("status", "complete")]).await;
    assert_eq!(body["unindexed"], 1);
    let (_, clean, _) = {
        let w2 = World::new();
        seed(&w2, ME, 2, true);
        list(&w2, ME, &[]).await
    };
    assert!(clean.get("unindexed").is_none());
}

#[tokio::test]
async fn the_unindexed_count_includes_the_runs_a_q_search_cannot_match() {
    // A run that predates the index has no prompt head, so `q` cannot match it: the page must still say it exists.
    let w = World::new();
    seed(&w, ME, 5, true);
    let old = rid(2);
    w.index
        .lock()
        .unwrap()
        .retain(|(_, sk), _| !sk.ends_with(&old));
    let (status, body, _) = list(&w, ME, &[("q", "parser")]).await;
    assert_eq!(status, 200);
    assert!(!run_ids(&body).contains(&old));
    assert_eq!(body["unindexed"], 1, "{body}");
}

#[tokio::test]
async fn the_work_of_enriching_active_rows_has_a_deadline_too() {
    let w = World::new();
    for i in 1..=30 {
        let id = rid(i);
        w.add_run(run_item(&id, ME, "active", &at(i), S3_ONLY()));
        w.add_index(ME, index_of(&id, i, "Work on item"));
    }
    // Every tail read takes 300 ms and the whole call may take 100 ms: the rows come back without their marks, and the log says why.
    *w.tail_sleep.lock().unwrap() = Duration::from_millis(300);
    let req = request(ME, "GET", "/v1/runs", &[], "");
    let budgets = Budgets {
        scan: Duration::from_secs(8),
        total: Duration::from_millis(100),
    };
    let t = Instant::now();
    let (out, logs) = control_api::run_index::list_rows(&req, ME, &deps(&w), budgets).await;
    assert_eq!(out.status, 200);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let body: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(run_ids(&body).len(), 30);
    assert!(logs.iter().any(|l| l.contains("time budget")), "{logs:?}");
}

#[tokio::test]
async fn thirty_active_rows_answer_in_under_two_seconds_with_a_slow_tail_read() {
    let w = World::new();
    for i in 1..=30 {
        let id = rid(i);
        w.add_run(run_item(&id, ME, "active", &at(i), S3_ONLY()));
        w.add_index(ME, index_of(&id, i, "Work on item"));
        w.add_events([message(
            &id,
            &chat_support::ulid(i),
            &at(1000 + i),
            "still going",
        )]);
        w.agents
            .lock()
            .unwrap()
            .push(agent_item(&id, SONNET, "running"));
    }
    *w.tail_sleep.lock().unwrap() = Duration::from_millis(30);
    let t = Instant::now();
    let (status, body, _) = list(&w, ME, &[]).await;
    let took = t.elapsed();
    assert_eq!(status, 200);
    assert_eq!(run_ids(&body).len(), 30);
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert_eq!(w.count("latest_events"), 30, "one tail read per active row");
}

#[tokio::test]
async fn the_last_activity_of_an_active_run_is_its_newest_event() {
    let w = World::new();
    let id = rid(1);
    w.add_run(run_item(&id, ME, "active", &at(1), S3_ONLY()));
    w.add_index(ME, index_of(&id, 1, "Work"));
    w.add_events([
        message(&id, &ulid(1), &at(500), "a"),
        message(&id, &ulid(2), &at(900), "b"),
    ]);
    let (_, body, _) = list(&w, ME, &[]).await;
    assert_eq!(body["runs"][0]["lastActivity"], at(900));
    // A finished run keeps the time its index item holds.
    let done = rid(2);
    w.add_run(run_item(&done, ME, "complete", &at(2), S3_ONLY()));
    let mut item = index_of(&done, 2, "Done");
    item.last_activity = at(77);
    item.outcome = Some("Cline reported done".into());
    w.add_index(ME, item);
    let (_, body, _) = list(&w, ME, &[]).await;
    let finished = body["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"] == done)
        .unwrap()
        .clone();
    assert_eq!(finished["lastActivity"], at(77));
    assert_eq!(finished["outcome"], "Cline reported done");
}

// ---- the filter -------------------------------------------------------------------------------------------------------

async fn follow(w: &World, caller: &str, q: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let mut query: Vec<(&str, &str)> = q.to_vec();
        if let Some(t) = &token {
            query.push(("nextToken", t));
        }
        let (status, body, _) = list(w, caller, &query).await;
        assert_eq!(status, 200, "{body}");
        out.extend(run_ids(&body));
        match body.get("nextToken").and_then(Value::as_str) {
            Some(t) => token = Some(t.to_string()),
            None => return out,
        }
    }
    panic!("the list never ended");
}

#[tokio::test]
async fn q_returns_only_matching_runs_in_a_300_run_fixture() {
    let w = World::new();
    seed(&w, ME, 300, true);
    // A second principal has the same prompts; none of them may appear.
    for i in 1..=50 {
        let id = format!("01K9{:022}", i);
        w.add_run(run_item(&id, OTHER, "complete", &at(i), S3_ONLY()));
        w.add_index(OTHER, index_of(&id, i, "Build a calc tool"));
        w.goals
            .lock()
            .unwrap()
            .insert(id.clone(), goal_item(&id, "Build a calc tool"));
    }
    let got = follow(&w, ME, &[("q", "calc")]).await;
    let want: Vec<String> = (1..=300).rev().filter(|i| i % 7 == 0).map(rid).collect();
    assert_eq!(got, want);
    assert_eq!(got.len(), 42);
    // Case and word order do not matter; every word must be there.
    assert_eq!(follow(&w, ME, &[("q", "TOOL calc")]).await, want);
    assert!(follow(&w, ME, &[("q", "calc kubernetes")]).await.is_empty());
    // The other principal finds its own runs only.
    let theirs = follow(&w, OTHER, &[("q", "calc")]).await;
    assert_eq!(theirs.len(), 50);
    assert!(theirs.iter().all(|id| id.starts_with("01K9")));
}

#[tokio::test]
async fn a_filtered_page_stops_once_it_has_a_screenful() {
    let w = World::new();
    seed(&w, ME, 300, true);
    let (_, body, _) = list(&w, ME, &[("q", "calc")]).await;
    // Two 100-run pages brought it past 25 matches; the third is left for the next call.
    assert_eq!(w.count("list_runs"), 2);
    assert!(body["nextToken"].is_string());
    assert!(run_ids(&body).len() >= 25);
}

#[tokio::test]
async fn target_status_and_since_each_narrow_the_list() {
    let w = World::new();
    seed(&w, ME, 40, true);
    w.add_run(run_item(
        &rid(41),
        ME,
        "active",
        &at(41),
        json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
    ));
    let mut item = index_of(&rid(41), 41, "Fix the calculator");
    item.repository = "cc:hamh-e2e-python-calc".into();
    w.add_index(ME, item);
    assert_eq!(
        follow(&w, ME, &[("target", "PYTHON-calc")]).await,
        vec![rid(41)]
    );
    assert_eq!(follow(&w, ME, &[("status", "active")]).await, vec![rid(41)]);
    assert_eq!(follow(&w, ME, &[("status", "complete")]).await.len(), 40);
    let since = at(36);
    let got = follow(&w, ME, &[("since", &since)]).await;
    assert_eq!(got, (36..=41).rev().map(rid).collect::<Vec<_>>());
    // A cut-off older than every run stops the scan at the first page's end.
    let long_ago = "2020-01-01T00:00:00Z";
    assert_eq!(follow(&w, ME, &[("since", long_ago)]).await.len(), 41);
}

#[tokio::test]
async fn a_bad_filter_is_a_400_before_any_read() {
    let w = World::new();
    seed(&w, ME, 3, true);
    let long = "a".repeat(201);
    for q in [
        vec![("status", "finished")],
        vec![("since", "yesterday")],
        vec![("q", long.as_str())],
        vec![("q", "a b c d e f g h i")],
        vec![("q", "bad\u{7}")],
    ] {
        let (status, body, _) = list(&w, ME, &q).await;
        assert_eq!(status, 400, "{q:?}: {body}");
        assert_eq!(body["code"], "bad-request");
    }
    assert_eq!(w.count("list_runs"), 0);
}

#[tokio::test]
async fn a_scan_that_runs_out_of_time_says_so_and_hands_back_a_token() {
    let w = World::new();
    seed(&w, ME, 300, true);
    // No match anywhere, and a budget of nothing: one page is read, then the scan stops.
    let req = request(ME, "GET", "/v1/runs", &[("q", "zzzz")], "");
    let budgets = Budgets {
        scan: Duration::ZERO,
        total: Duration::from_secs(12),
    };
    let (out, _) = control_api::run_index::list_rows(&req, ME, &deps(&w), budgets).await;
    assert_eq!(out.status, 200);
    let body: Value = serde_json::from_str(&out.body).unwrap();
    assert_eq!(body["partial"], true, "{body}");
    assert!(body["nextToken"].is_string());
    assert_eq!(body["runs"].as_array().unwrap().len(), 0);
    assert_eq!(w.count("list_runs"), 1);
}

// ---- the backfill -----------------------------------------------------------------------------------------------------

async fn backfill(w: &World, caller: &str, token: Option<&str>) -> (u16, Value, Vec<String>) {
    let q: Vec<(&str, &str)> = token.map(|t| vec![("nextToken", t)]).unwrap_or_default();
    post(w, caller, "/v1/runs/index", &q, "").await
}

#[tokio::test]
async fn a_run_created_before_the_index_is_found_by_q_after_the_backfill() {
    let w = World::new();
    seed(&w, ME, 30, false);
    // Before: the rows have nothing but the summary, so a prompt word finds nothing.
    assert!(follow(&w, ME, &[("q", "parser")]).await.is_empty());
    let (status, report, _) = backfill(&w, ME, None).await;
    assert_eq!(status, 200, "{report}");
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!(r.indexed, 30);
    assert_eq!(r.already_indexed, 0);
    assert_eq!(r.skipped, 0);
    assert!(!r.more);
    assert_eq!(w.indexed(ME).len(), 30);
    let item = w
        .indexed(ME)
        .into_iter()
        .find(|i| i.run_id == rid(7))
        .unwrap();
    assert_eq!(item.prompt_head, "Build a calc tool number 7\nwith tests");
    assert_eq!(item.model.as_deref(), Some(SONNET));
    assert_eq!(item.created_at, at(7));
    assert_eq!(item.last_activity, at(7));
    let found = follow(&w, ME, &[("q", "calc")]).await;
    assert_eq!(found, [28, 21, 14, 7].map(rid).to_vec());
}

#[tokio::test]
async fn the_backfill_writes_at_most_forty_items_a_call_and_resumes() {
    let w = World::new();
    seed(&w, ME, 250, false);
    let mut calls = 0;
    let mut indexed = 0;
    let mut token: Option<String> = None;
    loop {
        let (status, report, _) = backfill(&w, ME, token.as_deref()).await;
        assert_eq!(status, 200, "{report}");
        let r: BackfillReport = serde_json::from_value(report).unwrap();
        assert!(r.indexed <= 40, "{r:?}");
        indexed += r.indexed;
        calls += 1;
        assert!(calls < 20, "the backfill must make progress");
        if !r.more {
            break;
        }
        token = r.next_token;
    }
    assert_eq!(indexed, 250);
    assert_eq!(w.indexed(ME).len(), 250);
    assert!(calls >= 7, "{calls}");
    // Nothing left to do: a call after the last reads and writes nothing.
    let writes = w.count("put_run_index");
    let (_, report, _) = backfill(&w, ME, None).await;
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!((r.indexed, r.more), (0, false));
    assert_eq!(r.already_indexed, 250);
    assert_eq!(w.count("put_run_index"), writes);
}

#[tokio::test]
async fn the_backfill_never_overwrites_an_item_and_skips_a_run_it_cannot_describe() {
    let w = World::new();
    seed(&w, ME, 3, false);
    // Run 2 ended and its item says how.
    let mut done = index_of(&rid(2), 2, "Write the parser number 2\nplease");
    done.outcome = Some("Cline reported done".into());
    done.last_activity = at(900);
    w.add_index(ME, done.clone());
    // Run 3 lost its goal item.
    w.goals.lock().unwrap().remove(&rid(3));
    let (_, report, _) = backfill(&w, ME, None).await;
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!((r.indexed, r.already_indexed, r.skipped), (1, 1, 1));
    let kept = w
        .indexed(ME)
        .into_iter()
        .find(|i| i.run_id == rid(2))
        .unwrap();
    assert_eq!(kept, done);
    assert!(w.indexed(ME).iter().all(|i| i.run_id != rid(3)));
}

#[tokio::test]
async fn the_backfill_completes_an_item_the_end_of_a_run_wrote_first() {
    let w = World::new();
    seed(&w, ME, 2, false);
    // run-session recorded the end of run 2 before the backfill made the rest of its item.
    let partial = RunIndexItem {
        run_id: rid(2),
        created_at: at(2),
        prompt_head: String::new(),
        prompt_truncated: false,
        model: None,
        repository: String::new(),
        outcome: Some("Cline reported done".into()),
        last_activity: at(900),
        run_name: None,
        run_name_source: None,
        run_name_model: None,
        ..Default::default()
    };
    w.add_index(ME, partial);
    let (_, report, _) = backfill(&w, ME, None).await;
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!(
        (r.indexed, r.already_indexed),
        (2, 0),
        "a partial item is not an indexed run"
    );
    let item = w
        .indexed(ME)
        .into_iter()
        .find(|i| i.run_id == rid(2))
        .unwrap();
    assert_eq!(item.prompt_head, "Write the parser number 2\nplease");
    assert_eq!(item.model.as_deref(), Some(SONNET));
    // What the end of the run wrote is not touched.
    assert_eq!(item.outcome.as_deref(), Some("Cline reported done"));
    assert_eq!(item.last_activity, at(900));
}

#[tokio::test]
async fn runs_the_backfill_cannot_describe_never_hold_it_back() {
    let w = World::new();
    // 150 runs, the newest 60 of which lost their goal item: they are skipped every time, and the rest still get indexed.
    seed(&w, ME, 150, false);
    for i in 91..=150 {
        w.goals.lock().unwrap().remove(&rid(i));
    }
    let mut calls = 0;
    let mut token: Option<String> = None;
    let mut skipped_seen = 0;
    loop {
        let (status, report, _) = backfill(&w, ME, token.as_deref()).await;
        assert_eq!(status, 200, "{report}");
        let r: BackfillReport = serde_json::from_value(report).unwrap();
        skipped_seen = skipped_seen.max(r.skipped);
        calls += 1;
        assert!(calls < 10, "a run with no goal must not pin the token");
        if !r.more {
            break;
        }
        token = r.next_token;
    }
    assert_eq!(w.indexed(ME).len(), 90);
    assert!(skipped_seen >= 1);
}

#[tokio::test]
async fn one_backfill_call_reads_a_bounded_number_of_pages() {
    let w = World::new();
    // 700 runs that are all indexed: nothing to write, yet a call may not walk them all.
    seed(&w, ME, 700, true);
    let (status, report, _) = backfill(&w, ME, None).await;
    assert_eq!(status, 200);
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!(r.indexed, 0);
    assert!(r.more && r.next_token.is_some(), "{r:?}");
    assert!(w.count("list_runs") <= ham_core::run_index::BACKFILL_MAX_PAGES);
    assert_eq!(
        r.scanned as usize,
        ham_core::run_index::BACKFILL_MAX_PAGES * 100
    );
}

#[tokio::test]
async fn the_write_cap_counts_every_attempt_not_only_the_ones_that_changed_something() {
    // Another backfill completed the items first: every attempt here answers "already whole", and the cap still ends the call.
    let w = World::new();
    seed(&w, ME, 100, false);
    w.index_races.store(true, Ordering::SeqCst);
    let (status, report, _) = backfill(&w, ME, None).await;
    assert_eq!(status, 200);
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!(
        w.count("put_run_index"),
        ham_core::run_index::BACKFILL_MAX_WRITES
    );
    assert!(r.more, "{r:?}");
    assert_eq!(r.indexed, 0);
    assert_eq!(
        r.already_indexed as usize,
        ham_core::run_index::BACKFILL_MAX_WRITES
    );
}

#[tokio::test]
async fn the_backfill_stops_for_time_between_runs_after_it_has_made_progress() {
    let w = World::new();
    seed(&w, ME, 10, false);
    let req = request(ME, "POST", "/v1/runs/index", &[], "");
    let (out, _) = control_api::run_index::backfill_rows(&req, ME, &deps(&w), Duration::ZERO).await;
    assert_eq!(out.status, 200);
    let r: BackfillReport = serde_json::from_str(&out.body).unwrap();
    // One write is progress; with no time left the call stops there and says to call again from the same page.
    assert_eq!(r.indexed, 1, "{r:?}");
    assert!(r.more);
    assert_eq!(r.next_token, None, "the first page, so no token");
}

#[tokio::test]
async fn a_run_whose_goal_has_no_text_is_skipped_not_left_half_made() {
    let w = World::new();
    seed(&w, ME, 3, false);
    w.goals
        .lock()
        .unwrap()
        .insert(rid(2), goal_item(&rid(2), "   \n  "));
    let (_, report, _) = backfill(&w, ME, None).await;
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!((r.indexed, r.skipped), (2, 1), "{r:?}");
    assert!(w.indexed(ME).iter().all(|i| i.run_id != rid(2)));
}

#[tokio::test]
async fn the_backfill_covers_only_the_callers_own_runs() {
    let w = World::new();
    seed(&w, ME, 5, false);
    let other_id = format!("01K9{:022}", 1);
    w.add_run(run_item(&other_id, OTHER, "complete", &at(1), S3_ONLY()));
    w.goals
        .lock()
        .unwrap()
        .insert(other_id.clone(), goal_item(&other_id, "Secret plan"));
    let (_, report, _) = backfill(&w, ME, None).await;
    assert_eq!(report["indexed"], 5);
    assert!(w.indexed(OTHER).is_empty());
    assert!(w.indexed(ME).iter().all(|i| i.prompt_head != "Secret plan"));
    // And a token from one caller is no token for another.
    let (status, body, _) = backfill(&w, OTHER, Some(&format!("{ME}|0"))).await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn the_backfill_route_takes_post_and_no_other_verb() {
    let w = World::new();
    let (status, body) = get(&w, ME, "/v1/runs/index", &[]).await;
    assert_eq!(status, 404, "{body}");
    let out =
        control_api::handler::handle(request(ME, "DELETE", "/v1/runs/index", &[], ""), &deps(&w))
            .await;
    assert_eq!(out.status, 405);
}

#[tokio::test]
async fn an_index_that_cannot_be_written_is_a_503_not_a_silent_success() {
    let w = World::new();
    seed(&w, ME, 3, false);
    w.index_write_down.store(true, Ordering::SeqCst);
    let (status, body, _) = backfill(&w, ME, None).await;
    assert_eq!(status, 503, "{body}");
    assert!(
        !body.to_string().contains("throttled"),
        "the store's text stays in the log"
    );
}

// ---- run names --------------------------------------------------------------------------------------------------------
// `name-run` (run-session) stores the checked name on the search item; the list shows it, or the seed built from the stored prompt
// head. The Lambda that lists never calls a model and never writes a name.

const AI_NAME: &str = "Make uploads idempotent across retries now";
const HAIKU: &str = "us.anthropic.claude-haiku-5-5";

/// The search item of run `i` after `name-run` stored `AI_NAME` on it.
fn named_index_of(id: &str, i: u64, prompt: &str) -> RunIndexItem {
    RunIndexItem {
        run_name: Some(AI_NAME.into()),
        run_name_source: Some("ai".into()),
        run_name_model: Some(HAIKU.into()),
        ..index_of(id, i, prompt)
    }
}

fn row_of<'a>(body: &'a Value, run: &str) -> &'a Value {
    body["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"] == run)
        .unwrap_or_else(|| panic!("{run} is not in {body}"))
}

#[tokio::test]
async fn list_rows_carry_name_and_name_source() {
    let w = World::new();
    seed(&w, ME, 3, false);
    // Run 1 has a stored name, run 2 an item with no name, run 3 no item at all.
    w.add_index(
        ME,
        named_index_of(&rid(1), 1, "Write the parser number 1\nplease"),
    );
    w.add_index(
        ME,
        index_of(&rid(2), 2, "Write the parser number 2\nplease"),
    );
    let (status, body, logs) = list(&w, ME, &[]).await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    let named = row_of(&body, &rid(1));
    assert_eq!(named["name"], AI_NAME);
    assert_eq!(named["nameSource"], "ai");
    let seeded = row_of(&body, &rid(2));
    assert_eq!(
        seeded["name"],
        ham_core::runname::seed_name("Write the parser number 2\nplease")
    );
    assert_eq!(seeded["nameSource"], "seed");
    let bare = row_of(&body, &rid(3));
    assert!(
        bare.get("name").is_none() && bare.get("nameSource").is_none(),
        "{bare}"
    );
    // The attributes of the item and the model route stay on the server.
    let text = body.to_string();
    for k in ["runName", "runNameSource", "runNameModel", HAIKU] {
        assert!(!text.contains(k), "{k} reached the wire: {text}");
    }
    // An old client parses the new rows.
    let old: ListRunsResponse = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(old.runs.len(), 3);
    let new: RunRowsPage = serde_json::from_value(body).unwrap();
    assert_eq!(new.runs[2].name.as_deref(), Some(AI_NAME));
    // One read of the index for the whole page, as before.
    assert_eq!(w.count("run_index_between"), 1);
}

#[tokio::test]
async fn a_stored_name_that_fails_the_check_on_read_is_listed_as_the_seed() {
    let w = World::new();
    seed(&w, ME, 1, false);
    let mut it = named_index_of(&rid(1), 1, "Write the parser number 1\nplease");
    it.run_name = Some("Fix the \u{1b}[31mlogin\u{1b}[0m bug today".into());
    w.add_index(ME, it);
    let (_, body, _) = list(&w, ME, &[]).await;
    let r = row_of(&body, &rid(1));
    assert_eq!(r["nameSource"], "seed");
    assert!(!body.to_string().contains("login"), "{body}");
}

#[tokio::test]
async fn q_finds_a_word_only_a_stored_name_holds() {
    let w = World::new();
    seed(&w, ME, 5, true);
    w.add_index(
        ME,
        named_index_of(&rid(3), 3, "Write the parser number 3\nplease"),
    );
    assert_eq!(follow(&w, ME, &[("q", "idempotent")]).await, vec![rid(3)]);
    assert_eq!(
        follow(&w, ME, &[("q", "IDEMPOTENT retries")]).await,
        vec![rid(3)]
    );
    assert!(
        follow(&w, ME, &[("q", "idempotent kubernetes")])
            .await
            .is_empty()
    );
    // The prompt still finds every run, named or not.
    assert_eq!(follow(&w, ME, &[("q", "parser")]).await.len(), 5);
}

#[tokio::test]
async fn backfill_writes_no_name_and_keeps_a_stored_one() {
    let w = World::new();
    seed(&w, ME, 3, false);
    // Run 1 is whole and named. Run 2 has only what `name-run` wrote before the backfill: a name and no prompt head. Run 3 has no item.
    let whole = named_index_of(&rid(1), 1, "Write the parser number 1\nplease");
    w.add_index(ME, whole.clone());
    let race = RunIndexItem {
        prompt_head: String::new(),
        model: None,
        repository: String::new(),
        ..named_index_of(&rid(2), 2, "")
    };
    w.add_index(ME, race);
    let (status, report, _) = backfill(&w, ME, None).await;
    assert_eq!(status, 200, "{report}");
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    // The counts are those of any other mix of whole, partial and missing items: a named item without a head still counts as unfinished.
    assert_eq!(
        (r.indexed, r.already_indexed, r.skipped),
        (2, 1, 0),
        "{r:?}"
    );
    let items = w.indexed(ME);
    let by = |id: String| items.iter().find(|i| i.run_id == id).unwrap().clone();
    assert_eq!(by(rid(1)), whole, "a whole, named item is not touched");
    let completed = by(rid(2));
    assert_eq!(
        completed.prompt_head, "Write the parser number 2\nplease",
        "the head was filled in"
    );
    assert_eq!(completed.model.as_deref(), Some(SONNET));
    assert_eq!(
        (
            completed.run_name.as_deref(),
            completed.run_name_source.as_deref(),
            completed.run_name_model.as_deref()
        ),
        (Some(AI_NAME), Some("ai"), Some(HAIKU)),
        "the name written first survives the backfill"
    );
    let fresh = by(rid(3));
    assert_eq!(
        (fresh.run_name, fresh.run_name_source, fresh.run_name_model),
        (None, None, None),
        "the backfill writes no name"
    );
}

#[tokio::test]
async fn a_name_written_before_the_start_survives_the_start() {
    let w = World::new();
    let (status, first, _) = start(&w, ME, "k-name", "Add percent_change\nwith tests").await;
    assert_eq!(status, 201, "{first}");
    let run = first["runId"].as_str().unwrap().to_string();
    // The item as `name-run` leaves it when it wins the race against the start's write: the run id, the creation time and the name.
    let created = w.runs.lock().unwrap().get(&run).unwrap().created_at.clone();
    w.index.lock().unwrap().clear();
    w.add_index(
        ME,
        RunIndexItem {
            run_id: run.clone(),
            created_at: created.clone(),
            prompt_head: String::new(),
            prompt_truncated: false,
            model: None,
            repository: String::new(),
            outcome: None,
            last_activity: String::new(),
            run_name: Some(AI_NAME.into()),
            run_name_source: Some("ai".into()),
            run_name_model: Some(HAIKU.into()),
            ..Default::default()
        },
    );
    // The start writes its fields again (a retry of the same request reaches the same key).
    let (status, second, _) = start(&w, ME, "k-name", "Add percent_change\nwith tests").await;
    assert_eq!(status, 201);
    assert_eq!(second["runId"], run.as_str());
    let items = w.indexed(ME);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.prompt_head, "Add percent_change\nwith tests");
    assert_eq!(item.model.as_deref(), Some(SONNET));
    assert_eq!(item.repository, "s3-only");
    assert_eq!(
        (
            item.run_name.as_deref(),
            item.run_name_source.as_deref(),
            item.run_name_model.as_deref()
        ),
        (Some(AI_NAME), Some("ai"), Some(HAIKU)),
        "the start's write leaves the name alone"
    );
    // And the list shows the name, with the title the head gives.
    let (_, body, _) = list(&w, ME, &[]).await;
    let r = row_of(&body, &run);
    assert_eq!(r["name"], AI_NAME);
    assert_eq!(r["title"], "Add percent_change");
}

// ---- run tags ----------------------------------------------------------------------------------------------
// The tag writers (run-session's tag branch, the edit route) own eight attributes of the search item. This Lambda only reads them: the
// list row carries the effective tags, `q` finds a run by a tag, and neither the start nor the backfill writes or disturbs them.

const TAGGED_AT: &str = "2026-10-21T10:00:00Z";

/// `item` after the tag writers touched it: two automatic tags (one of them removed by the person), one typed tag, and the bookkeeping.
fn tag_attributes(item: RunIndexItem) -> RunIndexItem {
    RunIndexItem {
        run_tags: Some(vec!["topic:csv-export".into(), "tech:python".into()]),
        run_tags_user: Some(vec!["project:billing".into()]),
        run_tags_off: Some(vec!["tech:python".into()]),
        run_tags_at: Some(TAGGED_AT.into()),
        run_tags_model: Some(HAIKU.into()),
        run_tags_ver: Some("rt-1".into()),
        run_tags_inh: Some(rid(900)),
        run_tags_rev: Some(2),
        ..item
    }
}

#[tokio::test]
async fn list_rows_carry_tags_over_the_real_handler() {
    let w = World::new();
    seed(&w, ME, 3, false);
    // Run 1 has tags, run 2 an item with none, run 3 no item.
    w.add_index(
        ME,
        tag_attributes(index_of(&rid(1), 1, "Write the parser number 1\nplease")),
    );
    w.add_index(
        ME,
        index_of(&rid(2), 2, "Write the parser number 2\nplease"),
    );
    let (status, body, logs) = list(&w, ME, &[]).await;
    assert_eq!(status, 200, "{body}");
    assert!(logs.is_empty(), "{logs:?}");
    assert_eq!(
        row_of(&body, &rid(1))["tags"],
        json!(["project:billing", "topic:csv-export"]),
        "the typed project first, the automatic tag next, the removed one gone"
    );
    assert_eq!(
        row_of(&body, &rid(2))["tags"],
        json!([]),
        "an item with no tags says so"
    );
    assert!(
        row_of(&body, &rid(3)).get("tags").is_none(),
        "a run with no item has no member"
    );
    // The attributes of the writers and the model route stay on the server.
    let text = body.to_string();
    for k in [
        "runTags",
        "tech:python",
        "rt-1",
        TAGGED_AT,
        HAIKU,
        &rid(900),
    ] {
        assert!(!text.contains(k), "{k} reached the wire: {text}");
    }
    // An old client parses the new rows; a new one reads the tags back.
    let old: ListRunsResponse = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(old.runs.len(), 3);
    let new: RunRowsPage = serde_json::from_value(body).unwrap();
    let tags = |i: usize| new.runs[i].tags.clone();
    assert_eq!(
        (tags(0), tags(1), tags(2)),
        (
            None,
            Some(vec![]),
            Some(vec!["project:billing".into(), "topic:csv-export".into()])
        ),
        "newest first"
    );
    // The page costs what it cost: one read of the index, and the same calls as the same world without tag attributes.
    assert_eq!(w.count("run_index_between"), 1);
    let plain = World::new();
    seed(&plain, ME, 3, false);
    plain.add_index(
        ME,
        index_of(&rid(1), 1, "Write the parser number 1\nplease"),
    );
    plain.add_index(
        ME,
        index_of(&rid(2), 2, "Write the parser number 2\nplease"),
    );
    let _ = list(&plain, ME, &[]).await;
    assert_eq!(
        *w.calls.lock().unwrap(),
        *plain.calls.lock().unwrap(),
        "no extra store call for tags"
    );
}

#[tokio::test]
async fn q_finds_a_word_only_a_stored_tag_holds_and_not_a_removed_one() {
    let w = World::new();
    seed(&w, ME, 5, true);
    w.add_index(
        ME,
        tag_attributes(index_of(&rid(3), 3, "Write the parser number 3\nplease")),
    );
    assert_eq!(follow(&w, ME, &[("q", "csv-export")]).await, vec![rid(3)]);
    assert_eq!(follow(&w, ME, &[("q", "BILLING")]).await, vec![rid(3)]);
    assert_eq!(
        follow(&w, ME, &[("q", "csv-export parser")]).await,
        vec![rid(3)]
    );
    // `tech:python` is on the automatic list and on the person's removal list: it is not a tag of the run, so it finds nothing.
    assert!(follow(&w, ME, &[("q", "python")]).await.is_empty());
    // The prompt still finds every run, tagged or not.
    assert_eq!(follow(&w, ME, &[("q", "parser")]).await.len(), 5);
}

#[tokio::test]
async fn backfill_writes_no_tag_attributes_and_keeps_stored_ones() {
    let w = World::new();
    seed(&w, ME, 3, false);
    // Run 1 is whole and tagged. Run 2 has only what the tag branch wrote before the backfill: tags and no prompt head. Run 3 has no item.
    let whole = tag_attributes(index_of(&rid(1), 1, "Write the parser number 1\nplease"));
    w.add_index(ME, whole.clone());
    let early = tag_attributes(RunIndexItem {
        run_id: rid(2),
        created_at: at(2),
        ..RunIndexItem::default()
    });
    w.add_index(ME, early.clone());
    let (status, report, _) = backfill(&w, ME, None).await;
    assert_eq!(status, 200, "{report}");
    let r: BackfillReport = serde_json::from_value(report).unwrap();
    assert_eq!(
        (r.indexed, r.already_indexed, r.skipped),
        (2, 1, 0),
        "{r:?}"
    );
    let items = w.indexed(ME);
    let by = |id: String| items.iter().find(|i| i.run_id == id).unwrap().clone();
    assert_eq!(by(rid(1)), whole, "a whole, tagged item is not touched");
    let completed = by(rid(2));
    assert_eq!(
        completed.prompt_head, "Write the parser number 2\nplease",
        "the head was filled in"
    );
    let kept = |i: &RunIndexItem| {
        (
            i.run_tags.clone(),
            i.run_tags_user.clone(),
            i.run_tags_off.clone(),
            i.run_tags_at.clone(),
            i.run_tags_model.clone(),
            i.run_tags_ver.clone(),
            i.run_tags_inh.clone(),
            i.run_tags_rev,
        )
    };
    assert_eq!(
        kept(&completed),
        kept(&early),
        "the tags written first survive"
    );
    let fresh = by(rid(3));
    assert_eq!(
        kept(&fresh),
        (None, None, None, None, None, None, None, None),
        "the backfill writes no tag attribute"
    );
}

#[tokio::test]
async fn a_tag_written_before_the_start_survives_the_start() {
    let w = World::new();
    let (status, first, _) = start(&w, ME, "k-tags", "Add percent_change\nwith tests").await;
    assert_eq!(status, 201, "{first}");
    let run = first["runId"].as_str().unwrap().to_string();
    // The item as the tag branch leaves it when it wins the race against the start's write: the run id, the creation time and the tags.
    let created = w.runs.lock().unwrap().get(&run).unwrap().created_at.clone();
    w.index.lock().unwrap().clear();
    let early = tag_attributes(RunIndexItem {
        run_id: run.clone(),
        created_at: created,
        ..RunIndexItem::default()
    });
    w.add_index(ME, early.clone());
    // The start writes its fields again (a retry of the same request reaches the same key).
    let (status, second, _) = start(&w, ME, "k-tags", "Add percent_change\nwith tests").await;
    assert_eq!(status, 201);
    assert_eq!(second["runId"], run.as_str());
    let items = w.indexed(ME);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.prompt_head, "Add percent_change\nwith tests");
    assert_eq!(item.model.as_deref(), Some(SONNET));
    assert_eq!(item.repository, "s3-only");
    assert_eq!(
        (
            &item.run_tags,
            &item.run_tags_user,
            &item.run_tags_off,
            &item.run_tags_at,
            item.run_tags_rev
        ),
        (
            &early.run_tags,
            &early.run_tags_user,
            &early.run_tags_off,
            &early.run_tags_at,
            early.run_tags_rev
        ),
        "the start's write leaves the tags alone"
    );
    // And the list shows both: the title the head gives and the tags.
    let (_, body, _) = list(&w, ME, &[]).await;
    let r = row_of(&body, &run);
    assert_eq!(r["title"], "Add percent_change");
    assert_eq!(r["tags"], json!(["project:billing", "topic:csv-export"]));
}

// ----: the session facts of the rows ---------------------------------------------------------------------------------------

fn session_row(extra: Value) -> harness_protocol::api::v1::RunSummary {
    let mut v = json!({"runId": rid(1), "status": "active", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": at(1), "ownerPrincipalArn": ME});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

fn beating_agent(
    run: &str,
    status: &str,
    heartbeat: Option<&str>,
) -> harness_protocol::store::AgentItem {
    let mut a = agent_item(run, SONNET, status);
    a.heartbeat_at = heartbeat.map(str::to_string);
    a
}

/// The service decides what is silent, at its own clock: the last heartbeat of an active run's session is at least 150 seconds old.
#[test]
fn b846_a_session_is_silent_by_the_services_clock_and_only_while_it_could_answer() {
    use control_api::run_index::{session_silent_since, silent_after_s};
    assert_eq!(
        silent_after_s(),
        150,
        "two beats of a minute and half a minute"
    );
    // 2026-11-16T07:05:22Z
    let heard = "2026-11-16T07:05:22Z";
    let at_s = harness_protocol::common::parse_utc_unix(heard).unwrap();
    let agent = beating_agent(&rid(1), "running", Some(heard));
    let row = session_row(json!({}));
    // Before the threshold: nothing. At it and after it: the time of the last heartbeat.
    assert_eq!(session_silent_since(&row, Some(&agent), at_s + 149), None);
    assert_eq!(
        session_silent_since(&row, Some(&agent), at_s + 150).as_deref(),
        Some(heard)
    );
    assert_eq!(
        session_silent_since(&row, Some(&agent), at_s + 4_000).as_deref(),
        Some(heard)
    );
    let late = at_s + 4_000;
    // A run that is not active, a run that delivers, and a run that waits for the user's answer are not silent.
    for extra in [
        json!({"status": "complete"}),
        json!({"status": "blocked"}),
        json!({"phase": "delivering"}),
        json!({"needsInput": {"requestId": "01K5ZQ4T8M3N6P7R9S0V1W2X3Y", "reason": "question", "title": "Which file?"}}),
    ] {
        assert_eq!(
            session_silent_since(&session_row(extra.clone()), Some(&agent), late),
            None,
            "{extra}"
        );
    }
    // A session the service ended, a session that has sent no heartbeat yet, and a run with no session record.
    for status in ["stopped", "failed"] {
        let ended = beating_agent(&rid(1), status, Some(heard));
        assert_eq!(
            session_silent_since(&row, Some(&ended), late),
            None,
            "{status}"
        );
    }
    let fresh = beating_agent(&rid(1), "running", None);
    assert_eq!(session_silent_since(&row, Some(&fresh), late), None);
    assert_eq!(session_silent_since(&row, None, late), None);
    // A time that is no UTC time is no heartbeat.
    let odd = beating_agent(&rid(1), "running", Some("a while ago"));
    assert_eq!(session_silent_since(&row, Some(&odd), late), None);
}

#[tokio::test]
async fn b846_runs_rows_carry_the_session_facts() {
    let w = World::new();
    // An hour ago by the service's clock, and a moment ago.
    let now = control_api::ids::now_rfc3339();
    let long_ago = control_api::ids::rfc3339(
        u64::try_from(harness_protocol::common::parse_utc_unix(&now).unwrap()).unwrap() - 3_600,
    );
    // Run 1: active, its session silent for an hour. Run 2: active, its session beats. Run 3: active with a recorded cancel and a
    // silent session. Run 4: finished, with the record of a cancel and an old heartbeat.
    for (i, status) in [(1, "active"), (2, "active"), (3, "active"), (4, "complete")] {
        let mut run = run_item(&rid(i), ME, status, &at(i), S3_ONLY());
        if i >= 3 {
            run.cancel_requested_at = Some("2026-11-16T07:05:22Z".into());
        }
        w.add_run(run);
        let heard = if i == 2 {
            now.clone()
        } else {
            long_ago.clone()
        };
        w.agents
            .lock()
            .unwrap()
            .push(beating_agent(&rid(i), "running", Some(&heard)));
    }
    let (status, body, logs) = get_logged(&w, ME, "/v1/runs", &[]).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !logs.iter().any(|l| l.contains("session of run")),
        "{logs:?}"
    );
    let rows = body["runs"].as_array().unwrap();
    let row = |i: u64| {
        rows.iter()
            .find(|r| r["runId"] == json!(rid(i)))
            .unwrap_or_else(|| panic!("run {i} is listed: {body}"))
    };
    assert_eq!(row(1)["sessionSilentSince"], json!(long_ago), "{}", row(1));
    assert!(row(1).get("cancelRequestedAt").is_none());
    assert!(row(2).get("sessionSilentSince").is_none(), "{}", row(2));
    // The record of the cancel and the silent session are both sent; the client says the cancel.
    assert_eq!(row(3)["cancelRequestedAt"], json!("2026-11-16T07:05:22Z"));
    assert_eq!(row(3)["sessionSilentSince"], json!(long_ago));
    // A finished row carries neither.
    assert!(row(4).get("cancelRequestedAt").is_none(), "{}", row(4));
    assert!(row(4).get("sessionSilentSince").is_none(), "{}", row(4));
}

/// `GET /v1/runs/{runId}` says about the run's session what the run's row of the list says.
#[tokio::test]
async fn b846_the_run_detail_carries_the_session_facts_of_its_row() {
    let w = World::new();
    let now = control_api::ids::now_rfc3339();
    let long_ago = control_api::ids::rfc3339(
        u64::try_from(harness_protocol::common::parse_utc_unix(&now).unwrap()).unwrap() - 3_600,
    );
    // Run 1: active with a recorded cancel, its session silent for an hour. Run 2: active, its session beats. Run 3: finished, with
    // the record of a cancel and an old heartbeat.
    for (i, status) in [(1, "active"), (2, "active"), (3, "complete")] {
        let mut run = run_item(&rid(i), ME, status, &at(i), S3_ONLY());
        if i != 2 {
            run.cancel_requested_at = Some("2026-11-16T07:05:22Z".into());
        }
        w.add_run(run);
        w.goals
            .lock()
            .unwrap()
            .insert(rid(i), goal_item(&rid(i), "Build it"));
        let heard = if i == 2 {
            now.clone()
        } else {
            long_ago.clone()
        };
        w.agents
            .lock()
            .unwrap()
            .push(beating_agent(&rid(i), "running", Some(&heard)));
    }
    let (status, d1) = get(&w, ME, &format!("/v1/runs/{}", rid(1)), &[]).await;
    assert_eq!(status, 200, "{d1}");
    assert_eq!(d1["summary"]["sessionSilentSince"], json!(long_ago), "{d1}");
    assert_eq!(
        d1["summary"]["cancelRequestedAt"],
        json!("2026-11-16T07:05:22Z"),
        "{d1}"
    );
    for i in [2, 3] {
        let (status, d) = get(&w, ME, &format!("/v1/runs/{}", rid(i)), &[]).await;
        assert_eq!(status, 200, "{d}");
        assert!(d["summary"].get("sessionSilentSince").is_none(), "{d}");
        assert!(d["summary"].get("cancelRequestedAt").is_none(), "{d}");
    }
}

/// `GET /v1/runs/{runId}` does not call the session of a run that waits for the user's answer silent. The detail's summary
/// carries no question, so the route reads the run's events for it, and only for a run that it would otherwise call silent.
#[tokio::test]
async fn b846_the_run_detail_of_a_run_that_waits_for_an_answer_is_not_called_silent() {
    let w = World::new();
    let now = control_api::ids::now_rfc3339();
    let long_ago = control_api::ids::rfc3339(
        u64::try_from(harness_protocol::common::parse_utc_unix(&now).unwrap()).unwrap() - 3_600,
    );
    // Run 1 waits for the user's answer, run 2 does not. The last heartbeat of both sessions is an hour old.
    for i in [1, 2] {
        w.add_run(run_item(&rid(i), ME, "active", &at(i), S3_ONLY()));
        w.goals
            .lock()
            .unwrap()
            .insert(rid(i), goal_item(&rid(i), "Build it"));
        w.agents
            .lock()
            .unwrap()
            .push(beating_agent(&rid(i), "running", Some(&long_ago)));
    }
    w.add_events([question(&rid(1), &ulid(1), &at(500))]);
    let (status, waiting, logs) = get_logged(&w, ME, &format!("/v1/runs/{}", rid(1)), &[]).await;
    assert_eq!(status, 200, "{waiting}");
    assert!(
        waiting["summary"].get("sessionSilentSince").is_none(),
        "a session that waits for an answer is not silent: {waiting}"
    );
    assert!(
        !logs.iter().any(|l| l.contains("session of run")),
        "{logs:?}"
    );
    // TRIGGER PRESENT: the run without a question, with the same old heartbeat, is called silent; and the run list says that run 1
    // waits for an answer.
    let (status, silent, _) = get_logged(&w, ME, &format!("/v1/runs/{}", rid(2)), &[]).await;
    assert_eq!(status, 200, "{silent}");
    assert_eq!(
        silent["summary"]["sessionSilentSince"],
        json!(long_ago),
        "{silent}"
    );
    let (_, listed, _) = get_logged(&w, ME, "/v1/runs", &[]).await;
    let row = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"] == json!(rid(1)))
        .unwrap_or_else(|| panic!("run 1 is listed: {listed}"));
    assert!(
        row.get("needsInput").is_some(),
        "TRIGGER PRESENT: the run's question is open: {row}"
    );
    assert!(row.get("sessionSilentSince").is_none(), "{row}");
}

/// With the read of a stopped session's storage: a session the service has fenced is not called silent. The service has taken
/// it off the run and decides about it; the row says nothing until the next session beats.
#[test]
fn b846_a_fenced_session_is_not_called_silent() {
    use control_api::run_index::session_silent_since;
    let heard = "2026-11-16T07:05:22Z";
    let late = harness_protocol::common::parse_utc_unix(heard).unwrap() + 4_000;
    let row = session_row(json!({}));
    let mut agent = beating_agent(&rid(1), "running", Some(heard));
    agent.session_generation = Some(2);
    assert_eq!(
        session_silent_since(&row, Some(&agent), late).as_deref(),
        Some(heard),
        "TRIGGER PRESENT: the session is silent"
    );
    // The fence of an earlier generation changes nothing.
    agent.fenced_generation = Some(1);
    assert_eq!(
        session_silent_since(&row, Some(&agent), late).as_deref(),
        Some(heard)
    );
    // The generation the item is at is fenced.
    agent.fenced_generation = Some(2);
    assert_eq!(session_silent_since(&row, Some(&agent), late), None);
}
