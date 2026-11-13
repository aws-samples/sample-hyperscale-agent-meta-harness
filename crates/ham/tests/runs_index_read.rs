//! The two reads of the Runs screen with the run index joined: the route that lists the runs also carries, per row, the title (the
//! first line of the prompt) and the model of the run index. A read of the newest page is one request; a read of every page follows the tokens.
//! The rows of a stage without the index are the bare summaries and give no entry.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham_core::runfilter::{IndexEntry, Refresh, RunsRead};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ARN: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";

#[derive(Default)]
struct Scripted {
    calls: Mutex<Vec<String>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl ControlPlane for Scripted {
    async fn call(
        &self,
        _method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push(path.to_string());
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((500, json!({"code": "unscripted", "message": path})));
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        })
    }
    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("these calls fetch nothing")
    }
}

fn row(id: &str, title: Option<&str>, model: Option<&str>) -> Value {
    let mut r = json!({"runId": id, "status": "complete", "phase": "planning", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-16T08:00:00Z", "ownerPrincipalArn": ARN});
    if let Some(t) = title {
        r["title"] = json!(t);
    }
    if let Some(m) = model {
        r["model"] = json!(m);
    }
    r
}

fn with_tags(mut row: Value, tags: Value) -> Value {
    row["tags"] = tags;
    row
}

fn api(replies: Vec<Value>) -> (Api<Scripted>, Arc<Scripted>) {
    let plane = Arc::new(Scripted::default());
    *plane.replies.lock().unwrap() = replies.into_iter().map(|b| (200, b)).collect();
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    (api, plane)
}

#[tokio::test]
async fn the_newest_page_gives_the_runs_and_the_entries_of_the_rows_that_have_an_index() {
    let (api, plane) = api(vec![json!({"runs": [
        row("R1", Some("Add a percent_change function"), Some("Claude Sonnet 5.5")),
        row("R2", None, None),
        row("R3", Some("Build a todo API"), None)
    ], "nextToken": "t2"})]);
    let got = api.read_runs_indexed(Refresh::Timer).await.unwrap();
    match got.read {
        RunsRead::First { runs, has_more } => {
            assert_eq!(runs.len(), 3);
            assert!(has_more);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        got.index,
        vec![
            IndexEntry {
                run_id: "R1".into(),
                title: Some("Add a percent_change function".into()),
                model: Some("Claude Sonnet 5.5".into()),
                name: None,
                name_source: None,
                tags: None,
            },
            IndexEntry {
                run_id: "R3".into(),
                title: Some("Build a todo API".into()),
                model: None,
                name: None,
                name_source: None,
                tags: None,
            }
        ]
    );
    assert_eq!(plane.calls.lock().unwrap().clone(), vec!["/v1/runs"]);
}

#[tokio::test]
async fn every_page_follows_the_tokens() {
    let (api, plane) = api(vec![
        json!({"runs": [row("R1", Some("one"), None)], "nextToken": "t2"}),
        json!({"runs": [row("R2", Some("two"), None)]}),
    ]);
    let got = api.read_runs_indexed(Refresh::Manual).await.unwrap();
    match got.read {
        RunsRead::All(runs) => assert_eq!(runs.len(), 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(got.index.len(), 2);
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].contains("nextToken=t2"), "{calls:?}");
}

#[tokio::test]
async fn a_stage_without_the_index_gives_bare_rows_and_no_entry() {
    let (api, _plane) = api(vec![json!({"runs": [row("R1", None, None)]})]);
    let got = api.read_runs_indexed(Refresh::Timer).await.unwrap();
    assert!(got.index.is_empty());
}

// ---- run names ---------------------------------------------------------------------------------------------------------

fn named_row(id: &str, title: Option<&str>, name: &str, source: &str) -> Value {
    let mut r = row(id, title, None);
    r["name"] = json!(name);
    r["nameSource"] = json!(source);
    r
}

#[tokio::test]
async fn the_entry_of_a_row_carries_its_name_and_the_source_of_the_name() {
    let (api, _plane) = api(vec![json!({"runs": [
        named_row("R1", Some("Add a percent_change function"), "Percent change function for the calculator", "ai"),
        named_row("R2", Some("Fix the login redirect loop"), "Fix the login redirect loop", "seed"),
        row("R3", Some("Build a todo API"), None)
    ]})]);
    let got = api.read_runs_indexed(Refresh::Timer).await.unwrap();
    let by_id = |id: &str| got.index.iter().find(|e| e.run_id == id).unwrap().clone();
    assert_eq!(
        by_id("R1").name.as_deref(),
        Some("Percent change function for the calculator")
    );
    assert_eq!(by_id("R1").name_source.as_deref(), Some("ai"));
    assert_eq!(by_id("R2").name_source.as_deref(), Some("seed"));
    assert_eq!(by_id("R3").name, None);
    assert_eq!(by_id("R3").name_source, None);
}

#[tokio::test]
async fn a_row_with_only_a_name_still_gives_an_entry() {
    // A search item the end of the run wrote first has no prompt head, so the row has a name and no title.
    let mut r = row("R1", None, None);
    r["name"] = json!("Order list pagination with cursors");
    r["nameSource"] = json!("ai");
    let (api, _plane) = api(vec![json!({ "runs": [r] })]);
    let got = api.read_runs_indexed(Refresh::Manual).await.unwrap();
    assert_eq!(got.index.len(), 1, "{:?}", got.index);
    assert_eq!(got.index[0].title, None);
    assert_eq!(
        got.index[0].name.as_deref(),
        Some("Order list pagination with cursors")
    );
}

#[tokio::test]
async fn the_rows_of_every_page_are_listed_with_their_names_for_ham_runs() {
    let (api, plane) = api(vec![
        json!({"runs": [named_row("R1", Some("one"), "First run named here today", "ai")], "nextToken": "t2"}),
        json!({"runs": [row("R2", None, None)]}),
    ]);
    let rows = api.list_run_rows().await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name.as_deref(), Some("First run named here today"));
    assert_eq!(rows[0].name_source.as_deref(), Some("ai"));
    assert_eq!(rows[1].name, None);
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "/v1/runs");
    assert!(calls[1].contains("nextToken=t2"), "{calls:?}");
}

#[tokio::test]
async fn the_name_of_a_run_is_one_page_asked_with_its_id_and_a_start_and_the_row_of_that_run_answers()
 {
    let (api, plane) = api(vec![json!({"runs": [
        named_row("R9", Some("another"), "Another run entirely named here", "ai"),
        named_row("R1", Some("this one"), "Order list pagination with cursors", "ai")
    ], "nextToken": "t2"})]);
    let got = api.run_name_of("R1").await.unwrap();
    assert_eq!(
        got,
        Some((
            "Order list pagination with cursors".to_string(),
            Some("ai".to_string())
        ))
    );
    let calls = plane.calls.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        1,
        "one page, whatever the token says: {calls:?}"
    );
    assert!(calls[0].starts_with("/v1/runs?q=R1&since="), "{}", calls[0]);
    assert!(!calls[0].contains("nextToken"), "{}", calls[0]);
}

#[tokio::test]
async fn a_run_with_no_row_or_no_name_has_no_name_to_read() {
    let (api, _plane) = api(vec![
        json!({"runs": [named_row("R9", Some("another"), "Another run entirely named here", "ai")]}),
        json!({"runs": [row("R1", Some("title only"), None)]}),
        json!({"runs": []}),
    ]);
    assert_eq!(
        api.run_name_of("R1").await.unwrap(),
        None,
        "another run's row"
    );
    assert_eq!(
        api.run_name_of("R1").await.unwrap(),
        None,
        "a row without a name"
    );
    assert_eq!(api.run_name_of("R1").await.unwrap(), None, "no row");
}

#[tokio::test]
async fn a_failed_read_of_the_name_is_an_error_the_caller_can_ignore() {
    let plane = Arc::new(Scripted::default());
    plane
        .replies
        .lock()
        .unwrap()
        .push_back((503, json!({"code": "unavailable", "message": "retry"})));
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    assert!(api.run_name_of("R1").await.is_err());
}

#[tokio::test]
async fn the_entries_carry_the_tags_of_their_rows_and_an_item_with_only_tags_gives_one() {
    // R1: a title and a list of tags. R2: an item with no prompt head (so no title, model or name) and an empty list: the person emptied it.
    // R3: a bare summary. R4: a title and no `tags` member, a backend that sends none.
    let (api, _plane) = api(vec![json!({"runs": [
        with_tags(row("R1", Some("Add x"), None), json!(["project:calc", "topic:csv-export"])),
        with_tags(row("R2", None, None), json!([])),
        row("R3", None, None),
        row("R4", Some("Old backend"), None)
    ]})]);
    let got = api.read_runs_indexed(Refresh::Timer).await.unwrap();
    let entry = |id: &str| got.index.iter().find(|e| e.run_id == id).cloned();
    assert_eq!(
        entry("R1").unwrap().tags,
        Some(vec![
            "project:calc".to_string(),
            "topic:csv-export".to_string()
        ])
    );
    assert_eq!(
        entry("R2").map(|e| e.tags),
        Some(Some(vec![])),
        "tags alone make an entry, and [] stays [] (it is not 'no tags member')"
    );
    assert!(entry("R3").is_none(), "no item: no entry");
    assert_eq!(entry("R4").unwrap().tags, None);
    assert_eq!(got.index.len(), 3);
}
