//! `prepare` writes the run's research policy row: on or off from the request, the stage's provider and limits, the end
//! of the run's longest lifetime and the URLs of the prompt. A failure to write it never stops the run.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::research::{ResearchLimits, ResearchOptions, ResearchProvider};
use harness_protocol::target::RunTarget;
use run_session::ops::{Op, ResearchStage, handle};
use run_session::prepare::prompt_urls;
use serde_json::json;
use support::fakes::*;

fn stage() -> ResearchStage {
    ResearchStage {
        provider: ResearchProvider::Tavily,
        limits: ResearchLimits::default(),
    }
}

async fn prepare(mem: &Mem) -> Result<serde_json::Value, run_session::error::RunSessionError> {
    let op: Op = serde_json::from_value(json!({"op": "prepare", "runId": RUN})).unwrap();
    handle(&mem.deps(), &op).await
}

fn repo_target() -> RunTarget {
    serde_json::from_value(json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"})).unwrap()
}

/// Replaces the stored request's research member (and its recorded hash) the way control-api would have stored it.
fn with_research(mem: &Mem, research: Option<ResearchOptions>) {
    let mut req: StartDirectRunRequest = serde_json::from_slice(
        &mem.0.blobs.lock().unwrap()[&harness_protocol::direct::s3::request_key(RUN)],
    )
    .unwrap();
    req.research = research;
    let body = serde_json::to_vec(&req).unwrap();
    mem.0
        .blobs
        .lock()
        .unwrap()
        .insert(harness_protocol::direct::s3::request_key(RUN), body.clone());
    for e in mem.0.events.lock().unwrap().values_mut() {
        if let harness_protocol::envelope::Payload::Inline { value } = &mut e.payload {
            value["requestSha256"] = json!(hex(&body));
        }
    }
}

#[tokio::test]
async fn a_run_gets_an_enabled_row_with_the_stage_provider_limits_and_expiry() {
    let mem = Mem::default();
    *mem.0.research_cfg.lock().unwrap() = Some(stage());
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    prepare(&mem).await.unwrap();
    let rows = mem.0.research_rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.pk, format!("run#{RUN}"));
    assert!(row.enabled);
    assert_eq!(row.provider, ResearchProvider::Tavily);
    assert_eq!(row.limits, ResearchLimits::default());
    // The row outlives the longest session (28,800 s) by an hour.
    assert_eq!(row.expires_at, T0 / 1000 + 28_800 + 3600);
    row.validate().unwrap();
    assert!(row.prompt_urls.is_empty());
}

#[tokio::test]
async fn no_research_in_the_request_writes_a_disabled_row() {
    let mem = Mem::default();
    *mem.0.research_cfg.lock().unwrap() = Some(stage());
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    with_research(&mem, Some(ResearchOptions { enabled: false }));
    prepare(&mem).await.unwrap();
    let rows = mem.0.research_rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].enabled);
}

#[tokio::test]
async fn a_stage_without_research_settings_writes_no_row() {
    let mem = Mem::default();
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    prepare(&mem).await.unwrap();
    assert!(mem.0.research_rows.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_urls_of_the_prompt_go_into_the_row() {
    let mem = Mem::default();
    *mem.0.research_cfg.lock().unwrap() = Some(stage());
    mem.seed(
        repo_target(),
        "Read https://docs.rs/serde/latest/serde/ and (https://github.com/sindresorhus/is-plain-obj).",
        MODEL,
    );
    prepare(&mem).await.unwrap();
    let rows = mem.0.research_rows.lock().unwrap().clone();
    assert_eq!(
        rows[0].prompt_urls,
        vec![
            "https://docs.rs/serde/latest/serde/",
            "https://github.com/sindresorhus/is-plain-obj"
        ]
    );
}

#[tokio::test]
async fn a_row_that_cannot_be_written_does_not_stop_the_run() {
    let mem = Mem::default();
    *mem.0.research_cfg.lock().unwrap() = Some(stage());
    mem.seed(repo_target(), "Add percent_change(a, b) with tests", MODEL);
    mem.0
        .failures
        .lock()
        .unwrap()
        .entry("put_research_policy".into())
        .or_default()
        .push_back(Some(run_session::error::RunSessionError::Unavailable(
            "research table: AccessDenied".into(),
        )));
    let out = prepare(&mem).await.unwrap();
    assert_eq!(out["repo"], "hamh-e2e-python-calc", "prepare still answers");
    assert!(
        mem.0.research_rows.lock().unwrap().is_empty(),
        "the route will answer disabled"
    );
}

#[test]
fn prompt_urls_are_https_links_trimmed_unique_and_bounded() {
    let max = 2048;
    assert_eq!(
        prompt_urls(
            "see https://a.example/x, then (https://b.example/y). Also <https://c.example/z>; \"https://d.example/w\" and https://a.example/x",
            max
        ),
        [
            "https://a.example/x",
            "https://b.example/y",
            "https://c.example/z",
            "https://d.example/w"
        ]
    );
    assert_eq!(
        prompt_urls("[docs](https://docs.rs/serde/latest/serde/) end.", max),
        ["https://docs.rs/serde/latest/serde/"]
    );
    // Only https, nothing else.
    assert!(prompt_urls("http://a.example/x ftp://b.example/y just words", max).is_empty());
    // A URL over the limit is not carried.
    let long = format!("https://a.example/{}", "p".repeat(3000));
    assert!(prompt_urls(&long, 2048).is_empty());
    // At most 50.
    let many: String = (0..80).map(|i| format!("https://a.example/{i} ")).collect();
    assert_eq!(prompt_urls(&many, max).len(), 50);
    assert!(prompt_urls("", max).is_empty());
}

// ---- the function environment ---------------------------------------------------------------------------------------------------

use run_session::lambda::research_from_lookup;
use std::collections::HashMap;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

#[test]
fn research_settings_need_a_table_and_a_provider_and_otherwise_research_is_off() {
    assert_eq!(research_from_lookup(&env(&[])).unwrap(), None);
    assert_eq!(
        research_from_lookup(&env(&[("RESEARCH_TABLE_NAME", "t")])).unwrap(),
        None
    );
    assert_eq!(
        research_from_lookup(&env(&[("RESEARCH_PROVIDER", "tavily")])).unwrap(),
        None
    );
    let on = research_from_lookup(&env(&[
        ("RESEARCH_TABLE_NAME", "t"),
        ("RESEARCH_PROVIDER", "tavily-keyless"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        (on.provider, on.limits),
        (ResearchProvider::TavilyKeyless, ResearchLimits::default())
    );
    assert!(
        research_from_lookup(&env(&[
            ("RESEARCH_TABLE_NAME", "t"),
            ("RESEARCH_PROVIDER", "bing")
        ]))
        .is_err()
    );
}

/// A stage on the AgentCore Web Search provider writes the policy rows of its runs with the word `agentcore`; run-session must read it
/// from its environment like the others, or every run of that stage starts with research off.
#[test]
fn a_stage_on_the_agentcore_provider_writes_the_agentcore_word() {
    let on = research_from_lookup(&env(&[
        ("RESEARCH_TABLE_NAME", "t"),
        ("RESEARCH_PROVIDER", "agentcore"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        (on.provider, on.limits),
        (ResearchProvider::Agentcore, ResearchLimits::default())
    );
}

#[test]
fn a_limits_override_is_a_full_valid_set() {
    let mut l = ResearchLimits {
        searches: 50,
        ..ResearchLimits::default()
    };
    let json = serde_json::to_string(&l).unwrap();
    let got = research_from_lookup(&env(&[
        ("RESEARCH_TABLE_NAME", "t"),
        ("RESEARCH_PROVIDER", "fixture"),
        ("RESEARCH_LIMITS_JSON", &json),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(got.limits.searches, 50);
    let bad = r#"{"searches": 1}"#;
    assert!(
        research_from_lookup(&env(&[
            ("RESEARCH_TABLE_NAME", "t"),
            ("RESEARCH_PROVIDER", "fixture"),
            ("RESEARCH_LIMITS_JSON", bad)
        ]))
        .is_err()
    );
    l.searches = 0;
    let zero = serde_json::to_string(&l).unwrap();
    assert!(
        research_from_lookup(&env(&[
            ("RESEARCH_TABLE_NAME", "t"),
            ("RESEARCH_PROVIDER", "fixture"),
            ("RESEARCH_LIMITS_JSON", &zero)
        ]))
        .is_err()
    );
}
