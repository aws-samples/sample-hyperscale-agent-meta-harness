//! Failover and provider data sharing: the routes agentd may switch to are built by run-session from the request. A model that needs provider
//! data sharing is a route only for a run whose request carries `providerSharing: true`; any other run never has it in its failover list, so its
//! session credentials never cover that model and its prompts never go to that provider by a switch.
#![allow(clippy::unwrap_used)]

use ham_core::models::Catalog;
use harness_protocol::common::Engine;
use run_session::prepare::{failover_specs, failover_specs_in};
use serde_json::{Value, json};

const FABLE: &str = "global.anthropic.claude-fable-5-2";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

fn smoked() -> Catalog {
    let text = include_str!("../../../engine/catalog/models.json");
    let mut v: Value = serde_json::from_str(text).unwrap();
    let row = v["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["id"] == FABLE)
        .unwrap();
    row["smokeRecordId"] = json!("s6:test-record");
    Catalog::parse(&v.to_string()).unwrap()
}

fn ids(models: &Catalog, chosen: &str, sharing: bool) -> Vec<String> {
    failover_specs_in(models, Engine::CLINE, chosen, sharing)
        .into_iter()
        .map(|s| s.id)
        .collect()
}

#[test]
fn b803_a_run_on_a_model_that_needs_nothing_never_lists_the_sharing_model_as_a_route() {
    let m = smoked();
    for sharing in [false, true] {
        assert!(
            !ids(&m, OPUS, sharing).iter().any(|i| i == FABLE),
            "sharing {sharing}"
        );
    }
}

#[test]
fn b803_a_run_on_the_sharing_model_keeps_the_other_models_as_routes() {
    let m = smoked();
    let routes = ids(&m, FABLE, true);
    assert!(
        !routes.is_empty() && routes.iter().all(|i| i != FABLE),
        "{routes:?}"
    );
}

#[test]
fn b803_the_committed_list_gives_the_same_routes_with_or_without_sharing_until_the_row_has_a_record()
 {
    assert_eq!(failover_specs(OPUS, false), failover_specs(OPUS, true));
    assert!(failover_specs(OPUS, true).iter().all(|s| s.id != FABLE));
}

#[test]
fn b803_the_session_passes_the_request_member_to_the_route_builder() {
    let src = include_str!("../src/start_session.rs");
    let squeezed: String = src.split_whitespace().collect();
    assert!(
        squeezed.contains(
            "failover_specs_for(engine,&request.model,request.provider_sharing==Some(true),)"
        ),
        "start_session.rs must build the failover list from the request's providerSharing"
    );
}
