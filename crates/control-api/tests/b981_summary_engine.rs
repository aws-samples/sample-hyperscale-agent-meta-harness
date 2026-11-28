//! The service's answer about a run names the engine the run is on, in the run list and in the answer about
//! the one run, and only for a run that is not on the default engine. The answer about a run on the default engine, and about a run
//! that was stored before the Run item named an engine, is byte for byte the answer of release 5.34: the two bodies below are the ones
//! release 5.34's service gave for these stored runs, and the tests that compare them passed against it.

#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, World, at, deps, request, run_item, ulid};
use harness_protocol::store::RunItem;
use serde_json::{Value, json};

/// A finished run as the store holds it. `engine` is what the start wrote: `None` for a request that named no engine or the default
/// engine, and for every item written before the member existed.
fn stored(n: u64, engine: Option<&str>) -> RunItem {
    let plain = run_item(&ulid(n), ME, "complete", &at(n), json!({"mode": "s3-only"}));
    let mut v = serde_json::to_value(plain).unwrap();
    if let Some(e) = engine {
        v["engine"] = json!(e);
    }
    serde_json::from_value(v).unwrap()
}

/// The body of a 200 answer, as the bytes the service sent.
async fn body(w: &World, path: &str) -> String {
    let out = control_api::handler::handle(request(ME, "GET", path, &[], ""), &deps(w)).await;
    assert_eq!(out.status, 200, "{path}: {}", out.body);
    out.body
}

/// The run list's answer of release 5.34 for the one stored run `n`.
fn list_of_5_34(n: u64) -> String {
    format!(
        r#"{{"runs":[{{"runId":"{}","status":"complete","phase":"green","target":{{"mode":"s3-only"}},"createdAt":"{}","ownerPrincipalArn":"{ME}"}}],"unindexed":1}}"#,
        ulid(n),
        at(n)
    )
}

/// The answer of release 5.34 about the one stored run `n`.
fn run_of_5_34(n: u64) -> String {
    format!(
        r#"{{"summary":{{"runId":"{}","status":"complete","phase":"green","target":{{"mode":"s3-only"}},"createdAt":"{}","ownerPrincipalArn":"{ME}"}},"tokensUsed":0,"costUsd":0.0,"links":{{}}}}"#,
        ulid(n),
        at(n)
    )
}

#[tokio::test]
async fn b981_the_list_and_the_run_name_the_engine_of_a_run_on_opencode() {
    let w = World::new();
    w.add_run(stored(1, Some("opencode")));
    let list: Value = serde_json::from_str(&body(&w, "/v1/runs").await).unwrap();
    assert_eq!(list["runs"][0]["engine"], json!("opencode"), "{list}");
    let one: Value =
        serde_json::from_str(&body(&w, &format!("/v1/runs/{}", ulid(1))).await).unwrap();
    assert_eq!(one["summary"]["engine"], json!("opencode"), "{one}");
    // The member is the only difference from the answer about the same run without it.
    let mut row = list["runs"][0].clone();
    row.as_object_mut().unwrap().remove("engine");
    let plain: Value = serde_json::from_str(&list_of_5_34(1)).unwrap();
    assert_eq!(row, plain["runs"][0]);
    let mut summary = one["summary"].clone();
    summary.as_object_mut().unwrap().remove("engine");
    let plain: Value = serde_json::from_str(&run_of_5_34(1)).unwrap();
    assert_eq!(summary, plain["summary"]);
}

#[tokio::test]
async fn b981_a_run_stored_before_the_item_named_an_engine_answers_byte_for_byte_as_in_release_5_34()
 {
    let w = World::new();
    w.add_run(stored(1, None));
    assert_eq!(body(&w, "/v1/runs").await, list_of_5_34(1));
    assert_eq!(
        body(&w, &format!("/v1/runs/{}", ulid(1))).await,
        run_of_5_34(1)
    );
}

#[tokio::test]
async fn b981_a_run_on_the_default_engine_answers_byte_for_byte_as_in_release_5_34() {
    // The service stores no engine for a start that names the default engine; an item that names it all the same answers as one
    // that names none.
    let w = World::new();
    w.add_run(stored(1, Some("cline")));
    assert_eq!(body(&w, "/v1/runs").await, list_of_5_34(1));
    assert_eq!(
        body(&w, &format!("/v1/runs/{}", ulid(1))).await,
        run_of_5_34(1)
    );
}

#[tokio::test]
async fn b981_a_list_of_runs_on_both_engines_names_only_the_one_that_is_not_the_default() {
    let w = World::new();
    w.add_run(stored(1, None));
    w.add_run(stored(2, Some("opencode")));
    w.add_run(stored(3, Some("cline")));
    let list: Value = serde_json::from_str(&body(&w, "/v1/runs").await).unwrap();
    let rows = list["runs"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{list}");
    for row in rows {
        let named = row.get("engine").cloned();
        if row["runId"] == json!(ulid(2)) {
            assert_eq!(named, Some(json!("opencode")), "{row}");
        } else {
            assert_eq!(named, None, "{row}");
        }
    }
}
