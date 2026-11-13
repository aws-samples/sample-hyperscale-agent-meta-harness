//! The alternate routes of the model list and the failover routes of a chosen model.
//!
//! `engine/catalog/models.json` gives each of the owner's models its one route and, for the five that have them, the other
//! inference profile of the same model (`us.` <-> `global.`) with its own smoke record. A run names one model; the control plane
//! turns the list into the routes agentd may switch to when the model fails: the other profile of the same model first, then the
//! other models (same vendor first, then catalog order), each followed by its own other profile. Only routes with a smoke record
//! are ever offered.

#![allow(clippy::unwrap_used)]

use ham_core::models::{Models, ModelsError, builtin};
use harness_protocol::common::Protocol;
use harness_protocol::direct::MAX_DIRECT_ROUTES;
use serde_json::{Value, json};

const OPUS_US: &str = "us.anthropic.claude-opus-5-5";
const OPUS_GL: &str = "global.anthropic.claude-opus-5-5";
const SONNET_GL: &str = "global.anthropic.claude-sonnet-5-5";
const SONNET_US: &str = "us.anthropic.claude-sonnet-5-5";
const SOL_US: &str = "us.openai.gpt-6.1-sol";
const SOL_GL: &str = "global.openai.gpt-6.1-sol";
const ASTRA_US: &str = "us.openai.gpt-6-astra";
const ASTRA_GL: &str = "global.openai.gpt-6-astra";
const GLM: &str = "us.zai.glm-5.3";
const FABLE_GL: &str = "global.anthropic.claude-fable-5-2";
const FABLE_US: &str = "us.anthropic.claude-fable-5-2";

fn ids(chosen: &str) -> Vec<String> {
    builtin()
        .failover_routes(chosen)
        .into_iter()
        .map(|r| r.id)
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// ---- the file ---------------------------------------------------------------------------------------------------------

#[test]
fn five_models_carry_their_other_profile_and_glm_has_none() {
    let by_id = |id: &str| builtin().find(id).unwrap();
    let alt = |id: &str| -> Vec<String> {
        by_id(id)
            .alternate_routes
            .iter()
            .map(|a| a.id.clone())
            .collect()
    };
    assert_eq!(alt(OPUS_US), strs(&[OPUS_GL]));
    assert_eq!(alt(SONNET_GL), strs(&[SONNET_US]));
    assert_eq!(alt(SOL_US), strs(&[SOL_GL]));
    assert_eq!(alt(ASTRA_US), strs(&[ASTRA_GL]));
    assert_eq!(alt(FABLE_GL), strs(&[FABLE_US]));
    assert!(alt(GLM).is_empty(), "GLM 5.3 has no smoked second profile");
}

#[test]
fn an_alternate_route_has_its_own_smoke_record_and_the_homes_region() {
    for m in builtin().models() {
        for a in &m.alternate_routes {
            assert_eq!(a.region, "us-east-1", "{}", a.id);
            assert!(
                a.smoke_record_id.starts_with(&format!("s6:{}:", a.id))
                    && (a.smoke_record_id.contains("+K3:")
                        || a.smoke_record_id.contains("+audit:")),
                "{}: {}",
                a.id,
                a.smoke_record_id
            );
            if let Some((_, file)) = a.smoke_record_id.split_once("+audit:") {
                let name = file.rsplit('/').next().unwrap();
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../engine/catalog")
                    .join(name);
                assert!(path.is_file(), "{}: no audit file {name}", a.id);
            }
            assert!(
                a.smoked_at.as_deref().is_some_and(|t| t.ends_with('Z')),
                "{}",
                a.id
            );
        }
    }
}

#[test]
fn an_alternate_id_is_not_a_model_a_request_may_name() {
    // A request names one of the five by its own id; the other profile is a route the control plane adds, not a choice.
    assert!(builtin().check(OPUS_GL).is_err());
    assert!(builtin().find(OPUS_GL).is_none());
}

fn model_with(extra: Value) -> String {
    let mut m = json!({
        "id": "us.v.m-1", "name": "M", "vendor": "v", "baseModelId": "v.m-1", "protocol": "converse",
        "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 8192,
        "smokeRecordId": "s6:x+K3:y", "smokedAt": "2026-10-06T21:35:00Z"
    });
    m["alternateRoutes"] = extra;
    json!({"schemaVersion": 1, "models": [m]}).to_string()
}

fn alt(id: &str) -> Value {
    json!({"id": id, "region": "us-east-1", "smokeRecordId": "s6:alt+K3:y", "smokedAt": "2026-10-06T21:35:00Z"})
}

#[test]
fn a_well_formed_alternate_parses() {
    let m = Models::parse(&model_with(json!([alt("global.v.m-1")]))).unwrap();
    assert_eq!(m.models()[0].alternate_routes[0].id, "global.v.m-1");
}

#[test]
fn an_alternate_without_a_smoke_record_is_refused() {
    for bad in [json!({"id": "global.v.m-1", "region": "us-east-1"}), {
        let mut a = alt("global.v.m-1");
        a["smokeRecordId"] = json!("  ");
        a
    }] {
        match Models::parse(&model_with(json!([bad]))) {
            Err(ModelsError::Invalid(m)) | Err(ModelsError::Parse(m)) => {
                assert!(m.contains("smoke") || m.contains("smokeRecordId"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn an_alternate_of_another_model_or_a_repeat_is_refused() {
    for (what, extra) in [
        ("another model", json!([alt("global.v.other-2")])),
        ("the model's own id", json!([alt("us.v.m-1")])),
        ("twice", json!([alt("global.v.m-1"), alt("global.v.m-1")])),
        ("a space", json!([alt("global.v. m-1")])),
        ("empty", json!([alt("")])),
    ] {
        assert!(
            matches!(
                Models::parse(&model_with(extra)),
                Err(ModelsError::Invalid(_))
            ),
            "{what}"
        );
    }
}

#[test]
fn an_alternate_may_not_be_another_models_id() {
    let mut a = serde_json::from_str::<Value>(&model_with(json!([alt("global.v.m-1")]))).unwrap();
    let mut b = a["models"][0].clone();
    b["id"] = json!("global.v.m-1");
    b["alias"] = json!("two");
    a["models"].as_array_mut().unwrap().push(b);
    assert!(matches!(
        Models::parse(&a.to_string()),
        Err(ModelsError::Invalid(_))
    ));
}

// ---- the failover routes ----------------------------------------------------------------------------------------------

#[test]
fn opus_falls_back_to_its_other_profile_then_sonnet_then_the_rest_in_catalog_order() {
    assert_eq!(
        ids(OPUS_US),
        strs(&[
            OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM
        ])
    );
}

#[test]
fn sonnet_the_default_falls_back_to_us_then_opus_then_the_openai_models_then_glm() {
    assert_eq!(
        ids(SONNET_GL),
        strs(&[
            SONNET_US, OPUS_US, OPUS_GL, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM
        ])
    );
}

#[test]
fn sol_prefers_astra_the_same_vendor_before_the_anthropic_models() {
    assert_eq!(
        ids(SOL_US),
        strs(&[
            SOL_GL, ASTRA_US, ASTRA_GL, OPUS_US, OPUS_GL, SONNET_GL, SONNET_US, GLM
        ])
    );
}

#[test]
fn astra_prefers_sol_then_the_anthropic_models() {
    assert_eq!(
        ids(ASTRA_US),
        strs(&[
            ASTRA_GL, SOL_US, SOL_GL, OPUS_US, OPUS_GL, SONNET_GL, SONNET_US, GLM
        ])
    );
}

#[test]
fn glm_has_no_other_profile_and_the_other_four_follow_in_catalog_order() {
    assert_eq!(
        ids(GLM),
        strs(&[
            OPUS_US, OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL
        ])
    );
}

#[test]
fn a_chosen_alternate_route_gets_the_models_own_route_first() {
    assert_eq!(
        ids(OPUS_GL),
        strs(&[
            OPUS_US, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM
        ])
    );
}

#[test]
fn an_unknown_id_has_no_failover_routes() {
    assert!(ids("us.nobody.model").is_empty());
    assert!(ids("").is_empty());
}

#[test]
fn no_list_repeats_a_route_or_the_chosen_one_and_none_is_longer_than_the_limit() {
    for m in builtin().models() {
        let routes = ids(&m.id);
        let mut seen = std::collections::BTreeSet::from([m.id.clone()]);
        for r in &routes {
            assert!(
                seen.insert(r.clone()),
                "{}: {r} twice or the chosen route",
                m.id
            );
        }
        assert!(
            routes.len() <= MAX_DIRECT_ROUTES,
            "{}: {}",
            m.id,
            routes.len()
        );
        assert!(!routes.is_empty(), "{}", m.id);
    }
}

#[test]
fn a_route_carries_the_models_own_limits_and_its_own_region() {
    let routes = builtin().failover_routes(OPUS_US);
    let sol_gl = routes.iter().find(|r| r.id == SOL_GL).unwrap();
    assert_eq!(
        (sol_gl.context_tokens, sol_gl.max_output_tokens),
        (1_000_000, 131_072),
        "the other profile of Sol has Sol's limits, not the 0 the account catalog stores for it"
    );
    assert_eq!(sol_gl.region, "us-east-1");
    assert_eq!(sol_gl.protocol, Protocol::Converse);
    let astra = routes.iter().find(|r| r.id == ASTRA_US).unwrap();
    assert_eq!(astra.context_tokens, 1_050_000);
    let glm = routes.iter().find(|r| r.id == GLM).unwrap();
    assert_eq!(glm.context_tokens, 1_048_576);
}

#[test]
fn a_model_without_a_smoke_record_is_never_offered() {
    let doc = json!({"schemaVersion": 1, "models": [
        {"id": "us.v.a-1", "name": "A", "vendor": "v", "baseModelId": "v.a-1", "protocol": "converse", "region": "us-east-1",
         "contextTokens": 100000, "maxOutputTokens": 4096, "smokeRecordId": "s6:a+K3:y", "smokedAt": "2026-10-06T21:35:00Z",
         "alternateRoutes": [alt("global.v.a-1")]},
        {"id": "us.v.b-1", "name": "B", "vendor": "v", "baseModelId": "v.b-1", "protocol": "converse", "region": "us-east-1",
         "contextTokens": 100000, "maxOutputTokens": 4096},
        {"id": "us.v.c-1", "name": "C", "vendor": "v", "baseModelId": "v.c-1", "protocol": "converse", "region": "us-east-1",
         "contextTokens": 100000, "maxOutputTokens": 4096, "smokeRecordId": "s6:c+K3:y", "smokedAt": "2026-10-06T21:35:00Z"}
    ]});
    let m = Models::parse(&doc.to_string()).unwrap();
    let got: Vec<String> = m
        .failover_routes("us.v.a-1")
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(got, strs(&["global.v.a-1", "us.v.c-1"]));
}

// ---- naming a route -----------------------------------------------------------------------------------

/// A run that switched route ends on an alternate route; the card names the model by the row that route belongs to.
#[test]
fn a_route_id_names_the_row_it_belongs_to_and_an_unknown_id_names_none() {
    let m = builtin();
    for (route, name) in [
        (SONNET_GL, "Claude Sonnet 5.5"),
        (SONNET_US, "Claude Sonnet 5.5"),
        (OPUS_US, "Claude Opus 5.5"),
        (OPUS_GL, "Claude Opus 5.5"),
        (SOL_GL, "GPT-6.1 Sol"),
        (GLM, "GLM 5.3"),
    ] {
        assert_eq!(
            m.find_route(route).map(|r| r.name.as_str()),
            Some(name),
            "{route}"
        );
    }
    assert!(m.find_route("us.example.future-model-9").is_none());
    assert!(m.find_route("sonnet").is_none(), "an alias is not a route");
    assert!(m.find_route("").is_none());
}
