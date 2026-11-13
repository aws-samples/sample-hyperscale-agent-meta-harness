//! The model list has an engine dimension. A row of `engine/catalog/models.json` may name the engine it was smoked with (`engine`, default `cline`);
//! the same model id may be listed once for each engine, each row with its own smoke record, and a start request is accepted only for an (engine, model)
//! pair whose row has a passing smoke record. Every existing behaviour for Cline stays what it was: the same list, the same refusals, the same words.
#![allow(clippy::unwrap_used)]

use ham_core::models::{self, Catalog, ModelRefusal, Models};
use harness_protocol::common::Engine;
use serde_json::{Value, json};

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");
const SOL: &str = "us.openai.gpt-6.1-sol";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

/// The shipped list without its OpenCode rows: the tests below add the rows they need, so the shipped ones would list a model twice.
fn doc() -> Value {
    let mut d: Value = serde_json::from_str(MODELS_JSON).unwrap();
    d["models"]
        .as_array_mut()
        .unwrap()
        .retain(|m| m.get("engine").is_none());
    d
}

/// A copy of the Cline row `id` for `engine`, with its own smoke record.
fn engine_row(id: &str, engine: &str, smoked: bool) -> Value {
    let mut row = doc()["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap()
        .clone();
    row["engine"] = json!(engine);
    row.as_object_mut().unwrap().remove("alternateRoutes");
    row.as_object_mut().unwrap().remove("default");
    if smoked {
        row["smokeRecordId"] = json!(format!("b787:test:{engine}:{id}"));
        row["smokedAt"] = json!("2026-11-08T00:00:00Z");
    } else {
        row.as_object_mut().unwrap().remove("smokeRecordId");
        row.as_object_mut().unwrap().remove("smokedAt");
    }
    row
}

fn with_rows(extra: Vec<Value>) -> String {
    let mut d = doc();
    d["models"].as_array_mut().unwrap().extend(extra);
    d.to_string()
}

fn catalog(extra: Vec<Value>) -> Catalog {
    Catalog::parse(&with_rows(extra)).unwrap()
}

#[test]
fn b787_a_row_without_an_engine_is_a_cline_row_and_the_build_lists_cline_then_opencode() {
    let c = models::builtin_catalog();
    assert_eq!(c.engines(), vec![Engine::CLINE, Engine::OPENCODE]);
    assert!(
        c.default_list()
            .models()
            .iter()
            .all(|m| m.engine == Engine::CLINE)
    );
    assert_eq!(c.default_list().models().len(), 6);
    // `builtin()` is the Cline list, the same object.
    assert_eq!(models::builtin(), c.default_list());
}

#[test]
fn b787_the_same_model_id_may_be_listed_once_for_each_engine() {
    let c = catalog(vec![engine_row(SOL, "opencode", true)]);
    assert_eq!(c.engines(), vec![Engine::CLINE, Engine::OPENCODE]);
    let open = c.for_engine(Engine::OPENCODE).unwrap();
    assert_eq!(open.models().len(), 1);
    assert_eq!(open.models()[0].id, SOL);
    assert_eq!(open.models()[0].engine, Engine::OPENCODE);
    // The Cline list did not change.
    assert_eq!(c.default_list().models().len(), 6);
}

#[test]
fn b787_the_same_model_id_twice_for_one_engine_is_refused() {
    let e = Catalog::parse(&with_rows(vec![
        engine_row(SOL, "opencode", true),
        engine_row(SOL, "opencode", true),
    ]))
    .unwrap_err();
    assert!(e.to_string().contains("listed twice"), "{e}");
}

#[test]
fn b787_aliases_and_the_default_are_per_engine() {
    // The alias `sol` of the Cline row is also the alias of the OpenCode row: they are in different lists.
    let mut row = engine_row(SOL, "opencode", true);
    row["default"] = json!(true);
    let c = catalog(vec![row]);
    let open = c.for_engine(Engine::OPENCODE).unwrap();
    assert_eq!(open.find("sol").map(|m| m.id.as_str()), Some(SOL));
    assert_eq!(open.default_model().map(|m| m.id.as_str()), Some(SOL));
    // The Cline default is still Sonnet.
    assert_eq!(c.default_list().default_model().unwrap().id, SONNET);
    // Two defaults for one engine are refused.
    let mut a = engine_row(SOL, "opencode", true);
    let mut b = engine_row(OPUS, "opencode", true);
    a["default"] = json!(true);
    b["default"] = json!(true);
    let e = Catalog::parse(&with_rows(vec![a, b])).unwrap_err();
    assert!(
        e.to_string()
            .contains("more than one model is marked default"),
        "{e}"
    );
    // An alias used twice inside one engine is refused.
    let mut a = engine_row(SOL, "opencode", true);
    let mut b = engine_row(OPUS, "opencode", true);
    a["alias"] = json!("same");
    b["alias"] = json!("same");
    assert!(Catalog::parse(&with_rows(vec![a, b])).is_err());
}

#[test]
fn b787_a_pair_with_a_smoke_record_for_that_engine_is_accepted() {
    let c = catalog(vec![engine_row(SOL, "opencode", true)]);
    let m = c.check(Engine::OPENCODE, SOL).unwrap();
    assert_eq!(m.engine, Engine::OPENCODE);
    assert_eq!(
        m.smoke_record_id.as_deref(),
        Some(format!("b787:test:opencode:{SOL}").as_str())
    );
    // And the Cline pair is the Cline row, not the OpenCode one.
    assert_eq!(c.check(Engine::CLINE, SOL).unwrap().engine, Engine::CLINE);
}

#[test]
fn b787_a_pair_with_no_row_for_the_engine_is_refused_and_the_message_names_the_pair() {
    // Sonnet has a Cline row and no OpenCode row.
    let c = catalog(vec![engine_row(SOL, "opencode", true)]);
    let e = c.check(Engine::OPENCODE, SONNET).unwrap_err();
    assert!(
        matches!(e, ModelRefusal::NotSmokedForEngine { .. }),
        "{e:?}"
    );
    assert_eq!(e.code(), "model-not-smoked");
    let text = e.to_string();
    assert!(text.contains("opencode") && text.contains(SONNET), "{text}");
    assert!(text.contains("smoke record"), "{text}");
}

#[test]
fn b787_a_row_for_the_engine_without_a_smoke_record_is_refused_too() {
    let c = catalog(vec![engine_row(SOL, "opencode", false)]);
    let e = c.check(Engine::OPENCODE, SOL).unwrap_err();
    assert_eq!(e.code(), "model-not-smoked");
    assert!(e.to_string().contains("opencode"), "{e}");
    // The Cline pair is still fine.
    assert!(c.check(Engine::CLINE, SOL).is_ok());
}

#[test]
fn b787_a_model_no_engine_lists_is_unknown_for_every_engine() {
    let c = catalog(vec![engine_row(SOL, "opencode", true)]);
    let e = c.check(Engine::OPENCODE, "no.such.model").unwrap_err();
    assert_eq!(e.code(), "unknown-model");
    assert!(
        e.to_string().contains("no.such.model") && e.to_string().contains("opencode"),
        "{e}"
    );
    let e = c.check(Engine::CLINE, "no.such.model").unwrap_err();
    assert_eq!(e.code(), "unknown-model");
}

#[test]
fn b787_an_engine_with_no_rows_at_all_refuses_every_model() {
    let c = models::builtin_catalog();
    let pi = Engine::new("pi").unwrap();
    let e = c.check(pi, SOL).unwrap_err();
    assert_eq!(e.code(), "model-not-smoked");
    assert!(e.to_string().contains("pi"), "{e}");
}

#[test]
fn b787_the_cline_refusals_are_the_words_they_always_were() {
    let c = models::builtin_catalog();
    for id in ["nope", "zai.glm-5", ""] {
        let old = models::builtin().check(id).unwrap_err();
        let new = c.check(Engine::CLINE, id).unwrap_err();
        assert_eq!(old.to_string(), new.to_string(), "{id:?}");
        assert_eq!(old.code(), new.code(), "{id:?}");
    }
    // The old error variants are what a Cline refusal still is.
    assert!(matches!(
        c.check(Engine::CLINE, "nope").unwrap_err(),
        ModelRefusal::Unknown { .. }
    ));
}

#[test]
fn b787_models_parse_reads_the_cline_rows_of_a_document_that_has_other_rows_too() {
    let text = with_rows(vec![
        engine_row(SOL, "opencode", true),
        engine_row(OPUS, "opencode", true),
    ]);
    let cline_only = Models::parse(&text).unwrap();
    assert_eq!(cline_only, *models::builtin());
    assert_eq!(cline_only.models().len(), 6);
}

#[test]
fn b787_the_server_list_marks_the_rows_of_other_engines_and_leaves_cline_rows_unmarked() {
    let c = catalog(vec![engine_row(SOL, "opencode", true)]);
    let list = c.list();
    assert_eq!(list.default, SONNET);
    assert_eq!(list.models.len(), 7);
    let (cline, other): (Vec<_>, Vec<_>) = list.models.iter().partition(|r| r.engine.is_none());
    assert_eq!(cline.len(), 6);
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].engine, Some(Engine::OPENCODE));
    assert!(other[0].selectable);
    // The list of a build with only Cline rows is the list it always was.
    assert_eq!(catalog(vec![]).list(), models::builtin().list());
    // The shipped list has the same Cline rows first, then the rows of OpenCode.
    assert_eq!(
        models::builtin_catalog().list().models[..6],
        models::builtin().list().models[..]
    );
    assert!(
        models::builtin()
            .list()
            .models
            .iter()
            .all(|r| r.engine.is_none())
    );
}

#[test]
fn b787_failover_routes_stay_inside_the_engine() {
    // Two models are smoked with OpenCode; the others are not. A switch for an OpenCode run may only go to a pair with an OpenCode smoke record.
    let c = catalog(vec![
        engine_row(SOL, "opencode", true),
        engine_row(OPUS, "opencode", true),
        engine_row(SONNET, "opencode", false),
    ]);
    let routes = c.failover_routes(Engine::OPENCODE, SOL);
    let ids: Vec<&str> = routes.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&OPUS), "{ids:?}");
    assert!(!ids.contains(&SONNET), "{ids:?}");
    assert!(!ids.contains(&"us.zai.glm-5.3"), "{ids:?}");
    // The Cline list of routes is the one it was.
    assert_eq!(
        c.failover_routes(Engine::CLINE, SOL),
        models::builtin().failover_routes(SOL)
    );
}

#[test]
fn b787_a_row_whose_engine_is_not_a_profile_name_is_refused() {
    let mut row = engine_row(SOL, "opencode", true);
    row["engine"] = json!("Open Code");
    let e = Catalog::parse(&with_rows(vec![row])).unwrap_err();
    assert!(e.to_string().contains("engine"), "{e}");
}
