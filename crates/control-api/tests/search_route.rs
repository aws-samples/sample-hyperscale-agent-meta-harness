//! `GET /v1/search`: one Query sequence on the caller's own partition, matching in the function, one hit
//! per run, the read cost counted by a fake that mimics DynamoDB's 1 MB pages. The route never touches the transcript bucket.
#![allow(clippy::unwrap_used)]

mod support;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use control_api::cursor::{self, CursorError, Position};
use control_api::handler::{Deps, V1Request, handle_logged};
use control_api::ids::rfc3339;
use control_api::search::{SearchLimits, default_since, search_with};
use ham_core::runsearch::{
    BodyBuilder, Coverage, HitField, Piece, SearchItem, SearchResponse, Source, owner_pk,
    parse_response, search_sk, segs_to_string, sk_floor,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::{Bucket, NoExecution, Tables};

const ME: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const YOU: &str = "arn:aws:sts::800000000022:assumed-role/Admin/someone";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn run_id(n: u32) -> String {
    format!("01K83D{n:020}")
}

/// A search item for run number `n` created `minutes_ago` minutes ago, with a body made by the real builder.
fn item(n: u32, minutes_ago: u64, lines: &[&str]) -> SearchItem {
    let created = rfc3339(now() - minutes_ago * 60);
    let mut b = BodyBuilder::new();
    let pieces: Vec<Piece> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| Piece {
            source: Source::Engine {
                seq: u64::from(n % 7),
                rec: i as u32,
            },
            text: (*l).to_string(),
            message: None,
        })
        .collect();
    b.head_chunk(u64::from(n % 7), pieces);
    let body = b.finish(true);
    SearchItem {
        sk: search_sk(&created, &run_id(n)),
        run_id: run_id(n),
        created_at: created,
        body: Some(body.body),
        segs: Some(segs_to_string(&body.segs)),
        coverage: Some(body.coverage),
        body_through: body.through,
        ..SearchItem::default()
    }
}

fn put(t: &Tables, caller: &str, items: Vec<SearchItem>) {
    t.search
        .lock()
        .unwrap()
        .entry(owner_pk(caller))
        .or_default()
        .extend(items);
}

fn deps<'a>(t: &'a Tables, b: &'a Bucket) -> Deps<&'a Tables, &'a Bucket, NoExecution> {
    Deps {
        store: t,
        blobs: b,
        execution: NoExecution,
        account: "800000000022".into(),
        operators: vec![],
        repo_names: Default::default(),
    }
}

fn request(caller: &str, query: &[(&str, &str)]) -> V1Request {
    V1Request {
        method: "GET".into(),
        path: "/v1/search".into(),
        caller_arn: Some(caller.into()),
        source_ip: None,
        headers: BTreeMap::new(),
        query: query
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        body: String::new(),
    }
}

async fn call(t: &Tables, caller: &str, query: &[(&str, &str)]) -> (u16, String, Vec<String>) {
    let b = Bucket::default();
    let (resp, diag) = handle_logged(request(caller, query), &deps(t, &b)).await;
    assert_eq!(b.calls(), 0, "a search never reads the bucket");
    (resp.status, resp.body, diag)
}

async fn search(t: &Tables, caller: &str, query: &[(&str, &str)]) -> SearchResponse {
    let (status, body, _) = call(t, caller, query).await;
    assert_eq!(status, 200, "{body}");
    parse_response(body.as_bytes()).unwrap()
}

// ---- finding ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_phrase_from_the_middle_of_a_chat_finds_that_run_with_a_snippet_and_the_place_to_open() {
    let t = Tables::default();
    put(
        &t,
        ME,
        vec![
            item(
                1,
                300,
                &[
                    "Add a percent_change function",
                    "Edit src/finance.py",
                    "The tests pass now. Compound interest is next.",
                ],
            ),
            item(2, 200, &["Fix the login page", "Edit web/login.tsx"]),
            item(3, 100, &["Write the README"]),
        ],
    );
    let r = search(&t, ME, &[("q", "Compound INTEREST")]).await;
    assert_eq!(
        (r.searched, r.partial, r.more, r.hits.len()),
        (3, false, false, 1)
    );
    let h = &r.hits[0];
    assert_eq!(
        (h.run_id.as_str(), h.field),
        (run_id(1).as_str(), HitField::Chat)
    );
    assert!(
        h.snippet.contains("Compound interest is next."),
        "{}",
        h.snippet
    );
    assert_eq!(
        (h.seq, h.offset),
        (Some(1), Some(2)),
        "chunk and record where the matching piece starts"
    );
    // The open cursor is the transcript route's: bound to this run and this caller, positioned at the match.
    let open = h.open.as_deref().unwrap();
    assert_eq!(
        cursor::decode(&run_id(1), ME, open).unwrap(),
        Position::at(1, 2)
    );
    assert_eq!(
        cursor::decode(&run_id(2), ME, open),
        Err(CursorError::WrongBinding)
    );
    assert_eq!(
        cursor::decode(&run_id(1), YOU, open),
        Err(CursorError::WrongBinding)
    );
    assert_eq!(h.coverage, Some(Coverage::Full));
    // A file name finds the tool line, a word the run it is in.
    assert_eq!(
        search(&t, ME, &[("q", "web/login.tsx")]).await.hits[0].run_id,
        run_id(2)
    );
    assert!(search(&t, ME, &[("q", "zebra")]).await.hits.is_empty());
}

#[tokio::test]
async fn another_principals_phrase_never_matches_and_its_partition_is_never_read() {
    let t = Tables::default();
    put(
        &t,
        ME,
        vec![item(1, 50, &["the secret project codename is Bluebird"])],
    );
    put(
        &t,
        YOU,
        vec![
            item(2, 40, &["lunch plans only"]),
            item(3, 30, &["the other codename is Greenfinch"]),
        ],
    );
    assert_eq!(search(&t, ME, &[("q", "bluebird")]).await.hits.len(), 1);
    t.search_calls.lock().unwrap().clear();
    let r = search(&t, YOU, &[("q", "bluebird")]).await;
    assert_eq!((r.hits.len(), r.searched), (0, 2));
    assert!(search(&t, ME, &[("q", "greenfinch")]).await.hits.is_empty());
    assert_eq!(
        search(&t, YOU, &[("q", "greenfinch")]).await.hits[0].run_id,
        run_id(3)
    );
    // Every read named the caller's own partition and no other.
    {
        let calls = t.search_calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .all(|(pk, ..)| *pk == owner_pk(YOU) || *pk == owner_pk(ME))
        );
        assert_eq!(calls[0].0, owner_pk(YOU));
    }
    // A caller with no items has an empty partition.
    let r = search(
        &t,
        "arn:aws:sts::800000000022:assumed-role/Admin/nobody",
        &[("q", "bluebird")],
    )
    .await;
    assert_eq!((r.hits.len(), r.searched), (0, 0));
}

#[tokio::test]
async fn one_hit_per_run_newest_first_whatever_the_number_of_matches() {
    let t = Tables::default();
    put(
        &t,
        ME,
        vec![
            item(1, 300, &["parser parser parser", "parser again"]),
            item(2, 200, &["no match here"]),
            item(3, 100, &["parser", "parser", "parser"]),
        ],
    );
    let r = search(&t, ME, &[("q", "parser")]).await;
    assert_eq!(
        r.hits.iter().map(|h| h.run_id.clone()).collect::<Vec<_>>(),
        vec![run_id(3), run_id(1)]
    );
}

#[tokio::test]
async fn runs_without_a_body_are_searched_on_prompt_repository_and_model() {
    let t = Tables::default();
    let bare = |n: u32, ago: u64, prompt: &str, repo: &str, model: &str| {
        let created = rfc3339(now() - ago * 60);
        SearchItem {
            sk: search_sk(&created, &run_id(n)),
            run_id: run_id(n),
            created_at: created,
            prompt_head: Some(prompt.into()),
            repository: Some(repo.into()),
            model: Some(model.into()),
            ..SearchItem::default()
        }
    };
    put(
        &t,
        ME,
        vec![
            bare(
                1,
                30,
                "Fix the Rust parser",
                "cc:hamh-e2e-calc",
                "Claude Sonnet 5.5",
            ),
            bare(2, 20, "Add docs", "cc:docs", "GPT-6.1 Sol"),
        ],
    );
    let r = search(&t, ME, &[("q", "RUST")]).await;
    assert_eq!(
        (r.hits[0].field, r.hits[0].seq, r.hits[0].open.clone()),
        (HitField::Prompt, None, None)
    );
    assert_eq!(
        search(&t, ME, &[("q", "e2e-calc")]).await.hits[0].field,
        HitField::Repository
    );
    let sol = search(&t, ME, &[("q", "sol")]).await;
    assert_eq!(
        (sol.hits[0].run_id.clone(), sol.hits[0].field),
        (run_id(2), HitField::Model)
    );
    // Every word must occur, across the fields.
    assert_eq!(search(&t, ME, &[("q", "rust sonnet")]).await.hits.len(), 1);
    assert!(search(&t, ME, &[("q", "rust sol")]).await.hits.is_empty());
}

#[tokio::test]
async fn a_snippet_and_the_response_hold_no_token_the_body_kept() {
    let t = Tables::default();
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    put(
        &t,
        ME,
        vec![item(1, 10, &[&format!("deploy with {token} now")])],
    );
    let (status, body, diag) = call(&t, ME, &[("q", "deploy")]).await;
    assert_eq!(status, 200);
    assert!(!body.contains(&token), "{body}");
    assert!(diag.iter().all(|d| !d.contains(&token)));
    assert!(
        parse_response(body.as_bytes()).unwrap().hits[0]
            .snippet
            .starts_with("deploy with")
    );
}

#[tokio::test]
async fn a_malformed_item_never_makes_a_hit() {
    let t = Tables::default();
    let mut bad = item(1, 10, &["matches the query"]);
    bad.run_id = "not-a-ulid".into();
    put(&t, ME, vec![bad, item(2, 5, &["matches the query too"])]);
    let r = search(&t, ME, &[("q", "matches")]).await;
    assert_eq!(
        (r.searched, r.hits.len(), r.hits[0].run_id.clone()),
        (2, 1, run_id(2))
    );
}

// ---- the query -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_bad_query_is_a_400_that_never_repeats_the_query_and_never_reaches_the_store() {
    let t = Tables::default();
    put(&t, ME, vec![item(1, 10, &["x"])]);
    let long = "a".repeat(201);
    let nine = "alpha beta gamma delta epsilon zeta eta theta iota";
    let not_a_key = URL_SAFE_NO_PAD.encode("nope");
    for (query, want) in [
        (vec![], "q"),
        (vec![("q", "")], "q"),
        (vec![("q", "   ")], "q"),
        (vec![("q", long.as_str())], "q"),
        (vec![("q", nine)], "q"),
        (vec![("q", "bad\u{0}word")], "q"),
        (vec![("q", "x"), ("since", "yesterday")], "since"),
        (vec![("q", "x"), ("since", "2026-13-45")], "since"),
        (vec![("q", "x"), ("since", "2026-10-1")], "since"),
        (vec![("q", "x"), ("limit", "0")], "limit"),
        (vec![("q", "x"), ("limit", "201")], "limit"),
        (vec![("q", "x"), ("limit", "abc")], "limit"),
        (vec![("q", "x"), ("limit", "")], "limit"),
        (vec![("q", "x"), ("after", "garbage")], "after"),
        (vec![("q", "x"), ("after", not_a_key.as_str())], "after"),
    ] {
        let (status, body, diag) = call(&t, ME, &query).await;
        assert_eq!(status, 400, "{query:?}: {body}");
        assert!(body.contains(want), "{body}");
        for (_, v) in &query {
            assert!(v.len() < 8 || !body.contains(*v), "{body}");
        }
        assert!(diag.is_empty());
    }
    assert!(
        t.search_calls.lock().unwrap().is_empty(),
        "no bad query reached the store"
    );
    // What is accepted.
    let position = URL_SAFE_NO_PAD.encode("SRCH#2026-10-01T00:00:00Z#01K83D00000000000000000001");
    for ok in [
        ("since", "2026-09-01"),
        ("since", "2026-09-01T10:00:00Z"),
        ("limit", "1"),
        ("limit", "200"),
        ("after", position.as_str()),
    ] {
        assert_eq!(call(&t, ME, &[("q", "x"), ok]).await.0, 200, "{ok:?}");
    }
    let mut req = request(ME, &[("q", "x")]);
    req.method = "POST".into();
    let b = Bucket::default();
    assert_eq!(handle_logged(req, &deps(&t, &b)).await.0.status, 405);
}

#[tokio::test]
async fn b776_a_search_reaches_back_to_the_first_run_and_since_moves_the_start() {
    let t = Tables::default();
    put(
        &t,
        ME,
        vec![
            item(1, 60 * 24 * 100, &["old chat about parsers"]),
            item(2, 60 * 24 * 10, &["recent chat about parsers"]),
        ],
    );
    let r = search(&t, ME, &[("q", "parsers")]).await;
    assert_eq!(
        (r.searched, r.hits.len()),
        (2, 2),
        "the 100-day-old run is inside the default reach"
    );
    let floor = t.search_calls.lock().unwrap()[0].1.clone();
    assert!(floor == sk_floor(&default_since()), "{floor}");
    assert_eq!(default_since(), rfc3339(0));
    let since = rfc3339(now() - 30 * 86_400);
    let r = search(&t, ME, &[("q", "parsers"), ("since", &since)]).await;
    assert_eq!(r.hits.len(), 1, "a since still moves the start");
    let r = search(&t, ME, &[("q", "parsers"), ("since", "2999-01-01")]).await;
    assert_eq!((r.searched, r.hits.len()), (0, 0));
}

// ---- the read cost ---------------------------------------------------------------------------------------------------

fn big_item(n: u32, ago_minutes: u64, needle: &str) -> SearchItem {
    let line = format!("{needle} {}", "filler words for the body ".repeat(40));
    let lines: Vec<String> = (0..32).map(|i| format!("{line} {i}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    item(n, ago_minutes, &refs)
}

#[tokio::test]
async fn a_thousand_runs_cost_at_most_a_few_queries_of_the_owners_own_partition() {
    // Each item holds a body of about 32 KB; a Query page stops at 1 MB, so about thirty items.
    let t = Tables::with_page_bytes(1_000_000);
    put(
        &t,
        ME,
        (0..1000u32)
            .map(|n| {
                big_item(
                    n,
                    10 + u64::from(n),
                    if n == 700 {
                        "NEEDLE-SEVEN-HUNDRED"
                    } else {
                        "common"
                    },
                )
            })
            .collect(),
    );
    // Runs n and n+1 are one minute apart, so run 0 is newest. The needle is in run 700: 700 runs deep, past the default limit.
    let r = search(&t, ME, &[("q", "needle-seven-hundred")]).await;
    assert_eq!(
        (r.searched, r.more, r.partial, r.hits.len()),
        (200, true, false, 0)
    );
    let calls = t.search_calls.lock().unwrap().clone();
    assert!(calls.len() <= 8, "{} queries for 200 runs", calls.len());
    assert_eq!(
        *t.search_items_read.lock().unwrap(),
        200,
        "exactly the limit is read, and nothing else"
    );
    assert!(
        *t.search_bytes_read.lock().unwrap() < 8_000_000,
        "{} bytes",
        t.search_bytes_read.lock().unwrap()
    );
    assert!(
        calls.iter().all(|(pk, ..)| *pk == owner_pk(ME)),
        "never another partition, never a scan"
    );
    // The first call asks for the newest, the next ones continue after the last item read: no run is read twice.
    assert_eq!(calls[0].2, None);
    assert!(calls[1..].iter().all(|c| c.2.is_some()));
    // The token continues where the limit stopped, until the needle's run turns up.
    let mut token = r.next.clone().unwrap();
    let mut found = None;
    let mut examined = r.searched;
    for _ in 0..6 {
        let r = search(&t, ME, &[("q", "needle-seven-hundred"), ("after", &token)]).await;
        examined += r.searched;
        if let Some(h) = r.hits.first() {
            found = Some(h.run_id.clone());
            break;
        }
        token = r.next.clone().unwrap();
    }
    assert_eq!(found, Some(run_id(700)));
    assert_eq!(examined, 800, "runs 0 to 799 were examined once each");
}

#[tokio::test]
async fn limit_sets_how_many_runs_are_examined_and_asks_the_store_for_no_more() {
    let t = Tables::with_page_bytes(1_000_000);
    put(
        &t,
        ME,
        (0..120u32)
            .map(|n| big_item(n, 10 + u64::from(n), "common"))
            .collect(),
    );
    let r = search(&t, ME, &[("q", "common"), ("limit", "50")]).await;
    assert_eq!((r.searched, r.hits.len(), r.more), (50, 50, true));
    assert!(t.search_calls.lock().unwrap().iter().all(|c| c.3 <= 50));
    assert_eq!(*t.search_items_read.lock().unwrap(), 50);
    let r = search(&t, ME, &[("q", "common"), ("limit", "200")]).await;
    assert_eq!((r.searched, r.more, r.next), (120, false, None));
}

#[tokio::test]
async fn the_deadline_ends_the_search_partial_and_the_token_continues_it() {
    let t = Tables {
        latency: Duration::from_millis(30),
        ..Tables::default()
    };
    put(
        &t,
        ME,
        (0..60u32)
            .map(|n| item(n, 10 + u64::from(n), &["a chat about caching"]))
            .collect(),
    );
    let b = Bucket::default();
    let limits = SearchLimits {
        deadline: Duration::from_millis(70),
        per_call: 5,
        max_calls: 20,
    };
    let mut seen = Vec::new();
    let mut token: Option<String> = None;
    let mut rounds = 0;
    let mut partial_seen = false;
    loop {
        let mut q = vec![("q", "caching")];
        if let Some(tk) = &token {
            q.push(("after", tk.as_str()));
        }
        let (resp, _) = search_with(&request(ME, &q), ME, &deps(&t, &b), &limits).await;
        assert_eq!(resp.status, 200);
        let r = parse_response(resp.body.as_bytes()).unwrap();
        partial_seen |= r.partial;
        seen.extend(r.hits.iter().map(|h| h.run_id.clone()));
        rounds += 1;
        match (r.more, r.next) {
            (true, Some(n)) => token = Some(n),
            _ => break,
        }
        assert!(rounds < 40, "no end");
    }
    assert!(partial_seen, "the deadline should have ended a round");
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        60,
        "every run found once across the rounds: {} of 60",
        sorted.len()
    );
    assert_eq!(seen.len(), 60);
    // A cap on queries ends a search too, without calling it partial.
    let t = Tables::default();
    put(
        &t,
        ME,
        (0..30u32)
            .map(|n| item(n, 10 + u64::from(n), &["a chat about caching"]))
            .collect(),
    );
    let limits = SearchLimits {
        deadline: Duration::from_secs(5),
        per_call: 5,
        max_calls: 2,
    };
    let (resp, _) = search_with(
        &request(ME, &[("q", "caching")]),
        ME,
        &deps(&t, &b),
        &limits,
    )
    .await;
    let r = parse_response(resp.body.as_bytes()).unwrap();
    assert_eq!(
        (r.searched, r.more, r.partial, r.next.is_some()),
        (10, true, false, true)
    );
}

#[tokio::test]
async fn the_p95_of_a_search_over_a_thousand_runs_stays_under_two_seconds_in_the_function() {
    let t = Tables::with_page_bytes(1_000_000);
    put(
        &t,
        ME,
        (0..1000u32)
            .map(|n| big_item(n, 10 + u64::from(n), "common"))
            .collect(),
    );
    let mut times = Vec::new();
    for i in 0..20 {
        let q = if i % 2 == 0 {
            "common"
        } else {
            "no-such-phrase-anywhere"
        };
        let started = std::time::Instant::now();
        let r = search(&t, ME, &[("q", q)]).await;
        times.push(started.elapsed());
        assert_eq!(r.searched, 200);
    }
    times.sort();
    let p95 = times[18];
    assert!(p95 < Duration::from_secs(2), "p95 {p95:?} over {times:?}");
}

// ---- what is logged --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_access_line_and_every_diagnostic_hold_neither_the_query_nor_a_snippet() {
    let t = Tables {
        latency: Duration::from_millis(5),
        ..Tables::default()
    };
    put(
        &t,
        ME,
        vec![item(1, 10, &["the zanzibar protocol is described here"])],
    );
    let b = Bucket::default();
    let req = request(ME, &[("q", "zanzibar protocol")]);
    let limits = SearchLimits {
        deadline: Duration::from_millis(1),
        per_call: 1,
        max_calls: 20,
    };
    let (resp, diag) = search_with(&req, ME, &deps(&t, &b), &limits).await;
    let lines = control_api::lambda::log_lines(&req, resp.status, &diag);
    for line in &lines {
        assert!(!line.contains("zanzibar") && !line.contains("q="), "{line}");
    }
    assert!(lines[0].contains("/v1/search"));
    // A failing store is a 503 whose body names nothing, and its reason is the log's only.
    let down = Tables {
        down: true,
        ..Tables::default()
    };
    let (status, body, diag) = call(&down, ME, &[("q", "zanzibar")]).await;
    assert_eq!(status, 503);
    assert!(
        !body.contains("SECRET-STORE-DETAIL") && !body.contains("zanzibar"),
        "{body}"
    );
    assert!(diag.iter().all(|d| !d.contains("zanzibar")), "{diag:?}");
    // And the 400 for a bad query does not echo it.
    let (status, body, _) = call(&t, ME, &[("q", "zanzibar\u{0}")]).await;
    assert_eq!(status, 400);
    assert!(!body.contains("zanzibar"), "{body}");
}

#[tokio::test]
async fn partly_searchable_runs_are_counted_whether_or_not_they_match() {
    let t = Tables::default();
    let mut partial = item(1, 30, &["a chat about parsers"]);
    partial.coverage = Some(Coverage::Partial);
    let mut other = item(2, 20, &["something else entirely"]);
    other.coverage = Some(Coverage::Partial);
    put(
        &t,
        ME,
        vec![partial, other, item(3, 10, &["another chat about parsers"])],
    );
    let r = search(&t, ME, &[("q", "parsers")]).await;
    assert_eq!((r.hits.len(), r.partial_coverage), (2, 2));
    assert_eq!(
        r.hits.iter().map(|h| h.coverage).collect::<Vec<_>>(),
        vec![Some(Coverage::Full), Some(Coverage::Partial)]
    );
    let v: Value = serde_json::from_str(&call(&t, ME, &[("q", "parsers")]).await.1).unwrap();
    assert_eq!(v["partialCoverage"], 2);
}

// ---- findings of the implementation review (2026-10-16) --------------------------------------------------------------

#[tokio::test]
async fn a_page_of_items_the_store_could_not_decode_does_not_end_the_search() {
    // The first two reads return only items that cannot be decoded; the next reads hold the run.
    let t = Tables {
        page_bytes: 1_000_000,
        bad_pages: 2,
        ..Tables::default()
    };
    put(
        &t,
        ME,
        (0..120u32)
            .map(|n| item(n, 10 + u64::from(n), &["an old chat about caching"]))
            .collect(),
    );
    let r = search(&t, ME, &[("q", "caching"), ("limit", "40")]).await;
    // 120 items fit in one fake page unless the page is cut by the limit: with limit 40 each read examines 40.
    assert_eq!(r.searched, 40);
    assert!(r.more && r.next.is_some());
    let mut token = r.next.clone().unwrap();
    let mut hits = r.hits.len();
    let mut examined = r.searched;
    for _ in 0..5 {
        let r = search(
            &t,
            ME,
            &[("q", "caching"), ("limit", "40"), ("after", &token)],
        )
        .await;
        hits += r.hits.len();
        examined += r.searched;
        if !r.more {
            break;
        }
        token = r.next.clone().unwrap();
    }
    assert_eq!(examined, 120, "undecodable items count as examined");
    assert_eq!(
        hits,
        120 - 40 - 40,
        "the first two reads were all undecodable; the rest are found"
    );
}

#[tokio::test]
async fn half_undecodable_items_do_not_shift_the_continuation() {
    let t = Tables {
        page_bytes: 1_000_000,
        bad_every: 2,
        ..Tables::default()
    };
    put(
        &t,
        ME,
        (0..30u32)
            .map(|n| item(n, 10 + u64::from(n), &["a chat about parsers"]))
            .collect(),
    );
    let mut found = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let mut q = vec![("q", "parsers"), ("limit", "7")];
        if let Some(tk) = &token {
            q.push(("after", tk.as_str()));
        }
        let r = search(&t, ME, &q).await;
        found.extend(r.hits.iter().map(|h| h.run_id.clone()));
        match (r.more, r.next) {
            (true, Some(n)) => token = Some(n),
            _ => break,
        }
    }
    let mut sorted = found.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), found.len(), "no run twice");
    assert_eq!(
        found.len(),
        15,
        "every second item cannot be decoded; the other 15 are each found once"
    );
}

#[tokio::test]
async fn an_early_stop_hands_back_the_position_it_was_given() {
    let t = Tables::default();
    put(
        &t,
        ME,
        (0..10u32)
            .map(|n| item(n, 10 + u64::from(n), &["a chat about caching"]))
            .collect(),
    );
    let first = search(&t, ME, &[("q", "caching"), ("limit", "4")]).await;
    let token = first.next.clone().unwrap();
    let b = Bucket::default();
    let limits = SearchLimits {
        deadline: Duration::ZERO,
        per_call: 5,
        max_calls: 20,
    };
    let (resp, _) = search_with(
        &request(ME, &[("q", "caching"), ("after", &token)]),
        ME,
        &deps(&t, &b),
        &limits,
    )
    .await;
    let r = parse_response(resp.body.as_bytes()).unwrap();
    assert_eq!(
        (r.searched, r.partial, r.more, r.next.as_deref()),
        (0, true, true, Some(token.as_str())),
        "retry from the same place"
    );
    // Without a position there is nothing to hand back: the client asks again.
    let (resp, _) = search_with(
        &request(ME, &[("q", "caching")]),
        ME,
        &deps(&t, &b),
        &limits,
    )
    .await;
    let r = parse_response(resp.body.as_bytes()).unwrap();
    assert_eq!(
        (r.searched, r.partial, r.more, r.next),
        (0, true, true, None)
    );
}

#[tokio::test]
async fn a_query_that_outlasts_the_deadline_is_cut_off_and_reported_partial() {
    let t = Tables {
        latency: Duration::from_millis(400),
        ..Tables::default()
    };
    put(&t, ME, vec![item(1, 10, &["a chat about caching"])]);
    let b = Bucket::default();
    let limits = SearchLimits {
        deadline: Duration::from_millis(60),
        per_call: 5,
        max_calls: 20,
    };
    let started = std::time::Instant::now();
    let (resp, _) = search_with(
        &request(ME, &[("q", "caching")]),
        ME,
        &deps(&t, &b),
        &limits,
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    let r = parse_response(resp.body.as_bytes()).unwrap();
    assert_eq!((r.partial, r.more, r.hits.len()), (true, true, 0));
}

#[tokio::test]
async fn a_snippet_is_made_from_redacted_text_so_clipping_cannot_cut_a_secret_in_half() {
    let t = Tables::default();
    let token = format!("ghp_{}", "a1B2c3D4".repeat(5));
    let filler = "words ".repeat(18);
    // The secret sits where the snippet window ends: clipped first, only its front half would survive.
    let line = format!("needle {filler}{token} {filler}");
    put(&t, ME, vec![item(1, 10, &[&line])]);
    let r = search(&t, ME, &[("q", "needle")]).await;
    let snippet = &r.hits[0].snippet;
    assert!(
        !snippet.contains("ghp_") && !snippet.contains("a1B2c3D4"),
        "{snippet}"
    );
    assert!(snippet.contains("needle"));
}
