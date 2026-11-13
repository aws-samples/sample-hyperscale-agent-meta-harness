//! Claude Fable 5.2 on OpenCode. The provider data sharing agreement is a rule about the model and the run, not about the engine: a model that
//! needs it needs it on every engine that lists it, `ham` refuses it before any call whichever engine the run names, and a failover never carries a run to it
//! unless the run is on it and agreed to sharing.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::Config;
use ham_core::models::{Catalog, ModelError, ModelsError, builtin_catalog};
use ham_core::request::{DirectInputs, SettingsError, resolve_run_settings};
use harness_protocol::common::Engine;
use serde_json::{Value, json};

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");
const FABLE: &str = "global.anthropic.claude-fable-5-2";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";

/// The shipped Cline rows plus made-up OpenCode rows for Opus, Sol and Fable, so these tests stand on the rule and not on which models the shipped list offers
/// for OpenCode today. `fable_flag` is the value of `requiresProviderSharing` on the made-up Fable row (`None` leaves the member out).
fn catalog_with_opencode_fable(fable_flag: Option<bool>) -> Result<Catalog, ModelsError> {
    let mut v: Value = serde_json::from_str(MODELS_JSON).unwrap();
    let rows = v["models"].as_array_mut().unwrap();
    rows.retain(|m| m.get("engine").is_none());
    let mut made = Vec::new();
    for id in [OPUS, SOL, FABLE] {
        let mut row = rows.iter().find(|m| m["id"] == id).unwrap().clone();
        let o = row.as_object_mut().unwrap();
        o.insert("engine".into(), json!("opencode"));
        o.insert("smokeRecordId".into(), json!(format!("b805:test:{id}")));
        o.remove("default");
        o.remove("alternateRoutes");
        o.remove("reasoning");
        if id == FABLE {
            match fable_flag {
                Some(f) => o.insert("requiresProviderSharing".into(), json!(f)),
                None => o.remove("requiresProviderSharing"),
            };
        }
        made.push(row);
    }
    rows.extend(made);
    Catalog::parse(&v.to_string())
}

fn ids(routes: Vec<harness_protocol::direct::DirectModelSpec>) -> Vec<String> {
    routes.into_iter().map(|r| r.id).collect()
}

fn args(extra: &[&str]) -> DirectArgs {
    let mut words = vec![
        "Add a percent_change(a, b) function",
        "--s3-only",
        "--direct",
    ];
    words.extend_from_slice(extra);
    match parse(&words).unwrap().command {
        Command::StartDirect(a) => a,
        other => panic!("{words:?} is not a direct start: {other:?}"),
    }
}

#[test]
fn b805_a_model_that_needs_sharing_on_one_engine_needs_it_on_the_other_or_the_list_is_refused() {
    // Both engines agree: the list is accepted.
    let ok = catalog_with_opencode_fable(Some(true)).unwrap();
    assert!(
        ok.for_engine(Engine::OPENCODE)
            .unwrap()
            .models()
            .iter()
            .find(|m| m.id == FABLE)
            .unwrap()
            .requires_provider_sharing
    );
    // The OpenCode row lacks the mark the Cline row of the same model has: a request for it on OpenCode would skip the agreement, so the list does not load.
    for flag in [None, Some(false)] {
        let e = catalog_with_opencode_fable(flag).unwrap_err().to_string();
        assert!(
            e.contains("requiresProviderSharing") && e.contains(FABLE) && e.contains("opencode"),
            "{flag:?}: {e}"
        );
    }
}

#[test]
fn b805_every_row_of_the_shipped_list_agrees_with_the_other_rows_of_its_model_on_sharing() {
    let catalog = builtin_catalog();
    for engine in catalog.engines() {
        for row in catalog.for_engine(engine).unwrap().models() {
            for other in catalog.engines() {
                if let Some(twin) = catalog
                    .for_engine(other)
                    .unwrap()
                    .models()
                    .iter()
                    .find(|m| m.id == row.id)
                {
                    assert_eq!(
                        twin.requires_provider_sharing, row.requires_provider_sharing,
                        "{} on {engine} and {other}",
                        row.id
                    );
                }
            }
        }
    }
}

#[test]
fn b805_an_opencode_run_switches_to_the_sharing_model_only_when_it_is_on_that_model_with_sharing_on()
 {
    let c = catalog_with_opencode_fable(Some(true)).unwrap();
    // A run on a model that needs nothing never gets the sharing model as a route, whatever its setting says.
    for sharing in [false, true] {
        let routes = ids(c.failover_routes_for(Engine::OPENCODE, OPUS, sharing));
        assert!(
            !routes.iter().any(|i| i == FABLE),
            "sharing {sharing}: {routes:?}"
        );
        assert!(
            routes.iter().any(|i| i == SOL),
            "sharing {sharing}: {routes:?}"
        );
    }
    // A run on the sharing model keeps the other models of the same engine as routes and is never its own route.
    let routes = ids(c.failover_routes_for(Engine::OPENCODE, FABLE, true));
    assert!(
        routes.iter().any(|i| i == OPUS) && routes.iter().all(|i| i != FABLE),
        "{routes:?}"
    );
    // The routes stay inside the engine: the Cline list has Sonnet, GLM and more, the made-up OpenCode list does not.
    assert!(
        routes.iter().all(|i| [OPUS, SOL].contains(&i.as_str())),
        "{routes:?}"
    );
}

#[test]
fn b805_ham_refuses_fable_on_opencode_while_the_setting_is_off_and_sends_the_agreement_and_the_engine_with_it_on()
 {
    // The client resolves the model once, from the list of the model's own rows, so the engine of the run changes nothing about the agreement.
    let e = resolve_run_settings(
        &Config::default(),
        &|_| None,
        &args(&["--model", "fable", "--engine", "opencode"]),
        false,
    )
    .unwrap_err();
    assert!(
        matches!(e, SettingsError::Model(ModelError::NeedsSharing(_))),
        "{e:?}"
    );
    assert!(e.to_string().contains("Nothing was sent"), "{e}");
    let run = resolve_run_settings(
        &Config::default(),
        &|_| None,
        &args(&[
            "--model",
            "fable",
            "--engine",
            "opencode",
            "--provider-sharing",
            "on",
        ]),
        false,
    )
    .unwrap();
    let wire = serde_json::to_value(run.request(DirectInputs::default()).unwrap()).unwrap();
    assert_eq!(wire["engine"], "opencode", "{wire}");
    assert_eq!(wire["model"], FABLE, "{wire}");
    assert_eq!(wire["providerSharing"], true, "{wire}");
}
