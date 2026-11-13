//! run-session starts the session of the engine the request names. A request without an engine is the Cline run it always was (the Agent item, the
//! dispatch, the invocation and `session.started` say `cline`). A request for another engine takes the engine's profile version, the model row of THAT engine and
//! the failover routes that engine has a smoke record for. A pair the catalog has no row for ends the start with the pair named.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::models::Catalog;
use harness_protocol::common::Engine;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::events;
use run_session::ops::{Op, handle};
use run_session::prepare::{failover_specs, failover_specs_in, model_spec, model_spec_in};
use run_session::start_session::engine_version;
use serde_json::{Value, json};
use support::fakes::*;

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");
const SOL: &str = "us.openai.gpt-6.1-sol";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn start(mem: &Mem) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
}

fn catalog_with_opencode() -> Catalog {
    let mut v: Value = serde_json::from_str(MODELS_JSON).unwrap();
    let rows = v["models"].as_array_mut().unwrap();
    // The shipped list has OpenCode rows of its own; this test makes up two with other regions and windows, so it starts from the Cline rows.
    rows.retain(|m| m.get("engine").is_none());
    for (id, region, window) in [
        (SOL, "us-west-2", 777_000u64),
        (OPUS, "us-west-2", 555_000u64),
    ] {
        let mut row = rows.iter().find(|m| m["id"] == id).unwrap().clone();
        row["engine"] = json!("opencode");
        row["smokeRecordId"] = json!(format!("b787:test:opencode:{id}"));
        row["region"] = json!(region);
        row["contextTokens"] = json!(window);
        let o = row.as_object_mut().unwrap();
        o.remove("default");
        o.remove("alternateRoutes");
        rows.push(row);
    }
    Catalog::parse(&v.to_string()).unwrap()
}

#[tokio::test]
async fn b787_a_run_without_an_engine_starts_a_cline_session_everywhere() {
    let mem = Mem::default();
    mem.seed(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
    );
    start(&mem).await.unwrap();
    let agent = mem.agent().unwrap();
    assert_eq!(agent.engine, Engine::CLINE);
    let body: Value = serde_json::from_slice(&mem.invocation_bytes(0)).unwrap();
    assert_eq!(body["engine"], "cline");
    let started = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "session.started")
        .unwrap()
        .1;
    assert_eq!(started["engine"], "cline");
}

#[tokio::test]
async fn b787_a_request_for_an_engine_with_no_row_for_the_model_ends_the_start_and_names_the_pair()
{
    // `fake-acp` has a profile in every build and no row in the catalog.
    let mem = Mem::default();
    mem.seed_with_members(
        RunTarget::S3Only,
        "Add percent_change(a, b) with tests",
        MODEL,
        json!({"engine": "fake-acp"}),
    );
    let e = start(&mem).await.unwrap_err();
    let text = e.to_string();
    assert!(text.contains("fake-acp") && text.contains(MODEL), "{text}");
    assert!(mem.agent().is_none(), "no session was made for the pair");
}

#[test]
fn b787_the_model_row_and_the_routes_are_those_of_the_engine() {
    let c = catalog_with_opencode();
    let cline = model_spec_in(&c, Engine::CLINE, SOL).unwrap();
    let open = model_spec_in(&c, Engine::OPENCODE, SOL).unwrap();
    assert_eq!(
        cline,
        model_spec(SOL).unwrap(),
        "the Cline spec is what it was"
    );
    assert_eq!(open.id, SOL);
    assert_eq!(open.region, "us-west-2");
    assert_eq!(open.context_tokens, 777_000);
    assert_ne!(cline.context_tokens, open.context_tokens);
    // A model the engine has no row for is refused with the pair in the words.
    let e = model_spec_in(&c, Engine::OPENCODE, "global.anthropic.claude-sonnet-5-5").unwrap_err();
    assert!(
        e.to_string().contains("opencode") && e.to_string().contains("claude-sonnet-5-5"),
        "{e}"
    );
    // Routes: the OpenCode list holds two models, so the only route to switch to is the other one; Cline's list is unchanged.
    let routes: Vec<String> = failover_specs_in(&c, Engine::OPENCODE, SOL, false)
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(routes, vec![OPUS.to_string()]);
    assert_eq!(
        failover_specs_in(&c, Engine::CLINE, SOL, false),
        failover_specs(SOL, false)
    );
}

#[test]
fn b787_the_version_recorded_for_a_session_is_the_profile_version_of_its_engine() {
    let cline = engine_version(Engine::CLINE).unwrap();
    let opencode = engine_version(Engine::OPENCODE).unwrap();
    assert!(!cline.is_empty() && !opencode.is_empty());
    assert_ne!(cline, opencode);
    let profile = harness_core::engine_profile::bundled_profile(Engine::OPENCODE)
        .unwrap()
        .unwrap();
    assert_eq!(opencode, profile.version);
    // An engine with no profile in the build has no version.
    assert!(engine_version(Engine::new("pi").unwrap()).is_err());
}

#[test]
fn b787_session_started_names_the_engine_it_was_given() {
    let run = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
    for (engine, word) in [(Engine::CLINE, "cline"), (Engine::OPENCODE, "opencode")] {
        let e = events::session_started(run, 1_790_000_000_000, engine, MODEL, 1_000_000);
        let value = match e.payload {
            harness_protocol::envelope::Payload::Inline { value } => value,
            _ => panic!("an inline payload"),
        };
        assert_eq!(value["engine"], word);
    }
}
