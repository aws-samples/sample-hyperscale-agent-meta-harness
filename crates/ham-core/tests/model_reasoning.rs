//! The reasoning level of each model: `engine/catalog/models.json` gives every row a `reasoning` object, the level the model
//! reasons at when the user keeps `default`, the levels its route accepted in a live table, and the record of that table. The live table itself is
//! `engine/catalog/reasoning-levels.json`: one cell for each route and level, from real calls (Converse on the route with the request fields the
//! pinned engine sends, and a turn through the pinned engine launched with `--thinking <level>`). A model is never offered at a level the table
//! does not show as accepted.

#![allow(clippy::unwrap_used)]

use ham_core::models::{Models, ModelsError, builtin};
use harness_protocol::direct::REASONING_LEVELS;
use serde_json::{Value, json};

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");
const LEVELS_JSON: &str = include_str!("../../../engine/catalog/reasoning-levels.json");

const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";
const ASTRA: &str = "us.openai.gpt-6-astra";
const GLM: &str = "us.zai.glm-5.3";

fn table() -> Value {
    serde_json::from_str(LEVELS_JSON).unwrap()
}

fn cells(kind: &str) -> Vec<Value> {
    table()["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == kind)
        .cloned()
        .collect()
}

fn cell(kind: &str, route: &str, level: &str) -> Option<Value> {
    cells(kind)
        .into_iter()
        .find(|c| c["route"] == route && c["level"] == level)
}

fn doc() -> Value {
    serde_json::from_str(MODELS_JSON).unwrap()
}

fn parse(v: &Value) -> Result<Models, ModelsError> {
    Models::parse(&v.to_string())
}

fn row_mut<'a>(v: &'a mut Value, id: &str) -> &'a mut Value {
    v["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["id"] == id)
        .unwrap()
}

// ---- the catalog ---------------------------------------------------------------------------------------------------------------

#[test]
fn every_listed_model_has_a_reasoning_row_whose_default_it_accepts() {
    assert_eq!(builtin().models().len(), 5);
    for m in builtin().models() {
        let r = m
            .reasoning
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no reasoning row", m.id));
        assert!(r.accepted.contains(&r.default), "{}: {r:?}", m.id);
        assert!(!r.smoke_record_id.trim().is_empty(), "{}", m.id);
        assert!(
            r.accepted
                .iter()
                .all(|l| REASONING_LEVELS.contains(&l.as_str())),
            "{}: {:?}",
            m.id,
            r.accepted
        );
        assert_eq!(m.reasoning_default(), Some(r.default.as_str()), "{}", m.id);
    }
}

#[test]
fn the_defaults_are_the_levels_the_live_table_supports() {
    // xhigh where the model finished its hard runs at xhigh (GPT-6.1 Sol and GPT-6 Astra: 8 of 8). high for the two Claude models, whose xhigh is the
    // wire word `max` and ended in the engine's idle limit in 8 of 8 hard runs, and for GLM 5.3, whose xhigh gave no answer in 2 of 4 runs (the time
    // cap, and a provider error after 16 minutes) while high finished 3 of 3 (the report of the reasoning-level sweep of 2026-10-27). A
    // change of a default is a decision, so it is pinned.
    let want = [
        (OPUS, "high"),
        (SONNET, "high"),
        (SOL, "xhigh"),
        (ASTRA, "xhigh"),
        (GLM, "high"),
    ];
    for (id, level) in want {
        assert_eq!(
            builtin().find(id).unwrap().reasoning_default(),
            Some(level),
            "{id}"
        );
    }
}

#[test]
fn the_accepted_levels_of_a_row_are_the_levels_the_engine_launch_cells_show_accepted() {
    for m in builtin().models() {
        let r = m.reasoning.as_ref().unwrap();
        let shown: Vec<&str> = REASONING_LEVELS
            .iter()
            .copied()
            .filter(|l| {
                cell("launch", &m.id, l)
                    .unwrap_or_else(|| panic!("no launch cell for {} at {l}", m.id))["accepted"]
                    == true
            })
            .collect();
        let listed: Vec<&str> = r.accepted.iter().map(String::as_str).collect();
        assert_eq!(listed, shown, "{}", m.id);
    }
}

#[test]
fn the_table_has_a_route_cell_for_every_route_and_level_it_names() {
    let words = ["low", "medium", "high", "xhigh", "max", "minimal", "none"];
    for m in builtin().models() {
        let mut routes = vec![m.id.clone()];
        routes.extend(m.alternate_routes.iter().map(|a| a.id.clone()));
        for route in routes {
            for level in words {
                let c = cell("route", &route, level)
                    .unwrap_or_else(|| panic!("no route cell for {route} at {level}"));
                assert!(c["accepted"].is_boolean(), "{route} {level}");
                if c["accepted"] == false {
                    assert!(
                        c["detail"].as_str().is_some_and(|d| !d.is_empty()),
                        "{route} {level}: a refused level carries the error text"
                    );
                }
            }
            for level in REASONING_LEVELS {
                assert_eq!(
                    cell("route", &route, level).unwrap()["accepted"],
                    true,
                    "the route {route} accepted {level}"
                );
            }
        }
    }
}

#[test]
fn the_table_names_no_account_profile_or_host() {
    let text = LEVELS_JSON.to_ascii_lowercase();
    let words = [
        ["alias", "-one"].concat(),
        ["alias", "-two"].concat(),
        ["alias", "-three"].concat(),
        ["amazon", ".com"].concat(),
        ["arn:", "aws"].concat(),
    ];
    for word in &words {
        assert!(!text.contains(word.as_str()), "{word}");
    }
    assert!(
        !regex_like_account_number(&text),
        "no twelve-digit account number"
    );
}

fn regex_like_account_number(text: &str) -> bool {
    let mut run = 0;
    for b in text.bytes() {
        if b.is_ascii_digit() {
            run += 1;
            if run >= 12 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

// ---- the parse -----------------------------------------------------------------------------------------------------------------

#[test]
fn a_row_without_a_reasoning_object_still_parses_and_has_no_default() {
    let mut v = doc();
    row_mut(&mut v, GLM)
        .as_object_mut()
        .unwrap()
        .remove("reasoning");
    let m = parse(&v).unwrap();
    let glm = m.find(GLM).unwrap();
    assert!(glm.reasoning.is_none());
    assert_eq!(glm.reasoning_default(), None);
    assert!(
        m.failover_routes(SOL)
            .iter()
            .find(|r| r.id == GLM)
            .unwrap()
            .default_reasoning_effort
            .is_none()
    );
}

#[test]
fn a_default_the_row_does_not_accept_is_refused() {
    let mut v = doc();
    row_mut(&mut v, OPUS)["reasoning"]["accepted"] = json!(["low", "medium"]);
    row_mut(&mut v, OPUS)["reasoning"]["default"] = json!("high");
    let e = parse(&v).unwrap_err().to_string();
    assert!(e.contains(OPUS) && e.contains("default"), "{e}");
}

#[test]
fn a_word_that_is_not_a_launch_level_is_refused_in_either_list() {
    for (key, value) in [
        ("accepted", json!(["low", "max"])),
        ("accepted", json!(["low", "none"])),
        ("accepted", json!(["Low"])),
        ("accepted", json!([])),
        ("accepted", json!(["low", "low"])),
        ("default", json!("max")),
        ("default", json!("off")),
        ("default", json!("default")),
    ] {
        let mut v = doc();
        row_mut(&mut v, SOL)["reasoning"][key] = value.clone();
        if key == "default" {
            row_mut(&mut v, SOL)["reasoning"]["accepted"] =
                json!(["low", "medium", "high", "xhigh"]);
        }
        let e = parse(&v).unwrap_err().to_string();
        assert!(e.contains(SOL), "{key} {value}: {e}");
    }
}

#[test]
fn a_reasoning_row_needs_its_own_smoke_record() {
    let mut v = doc();
    row_mut(&mut v, ASTRA)["reasoning"]["smokeRecordId"] = json!("  ");
    assert!(parse(&v).unwrap_err().to_string().contains(ASTRA));
    let mut v = doc();
    row_mut(&mut v, ASTRA)["reasoning"]
        .as_object_mut()
        .unwrap()
        .remove("smokeRecordId");
    assert!(parse(&v).is_err());
}

#[test]
fn an_unknown_member_of_the_reasoning_row_is_refused() {
    let mut v = doc();
    row_mut(&mut v, SOL)["reasoning"]["maxEffort"] = json!("max");
    assert!(parse(&v).is_err());
}

// ---- the specs and the check ---------------------------------------------------------------------------------------------------

#[test]
fn the_failover_specs_carry_the_level_of_their_own_model() {
    let want = |id: &str| {
        builtin()
            .find_route(id)
            .unwrap()
            .reasoning_default()
            .map(str::to_string)
    };
    for chosen in [OPUS, SONNET, SOL, ASTRA, GLM] {
        let routes = builtin().failover_routes(chosen);
        assert!(!routes.is_empty());
        for r in routes {
            assert_eq!(
                r.default_reasoning_effort,
                want(&r.id),
                "{chosen} -> {}",
                r.id
            );
            assert!(r.default_reasoning_effort.is_some(), "{}", r.id);
        }
    }
}

#[test]
fn check_reasoning_accepts_each_accepted_level_and_refuses_the_rest_with_a_code_and_the_list() {
    for m in builtin().models() {
        for level in REASONING_LEVELS {
            builtin().check_reasoning(&m.id, level).unwrap();
        }
    }
    let mut v = doc();
    row_mut(&mut v, SONNET)["reasoning"]["accepted"] = json!(["low", "medium", "high"]);
    let models = parse(&v).unwrap();
    let e = models.check_reasoning(SONNET, "xhigh").unwrap_err();
    assert_eq!(e.code(), "reasoning-level-not-accepted");
    let text = e.to_string();
    assert!(
        text.contains("xhigh")
            && text.contains("low, medium, high")
            && text.contains("reasoning.effort"),
        "{text}"
    );
    // A model without a reasoning row accepts no explicit level; an unknown model is the model check's business.
    let mut v = doc();
    row_mut(&mut v, GLM)
        .as_object_mut()
        .unwrap()
        .remove("reasoning");
    let e = parse(&v).unwrap().check_reasoning(GLM, "low").unwrap_err();
    assert_eq!(e.code(), "reasoning-level-not-accepted");
}
