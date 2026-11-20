//! The alternate routes of the model list and the failover routes of a chosen model.
//!
//! `engine/catalog/models.json` gives each of the owner's models its one route and, for those that have one, the other
//! inference profile of the same model (`us.` <-> `global.`) with its own smoke record. A run names one model; the control plane
//! turns the list into the routes agentd may switch to when the model fails: the other profile of the same model first, then the
//! other models (same vendor first, at most four of its routes, then catalog order), each followed by its own other profile. Only
//! routes with a smoke record are ever offered.

#![allow(clippy::unwrap_used)]

use ham_core::models::{Models, ModelsError, builtin, builtin_catalog};
use harness_protocol::common::{Engine, Protocol};
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
const NEW_SOL_US: &str = "us.openai.gpt-6.2-sol";
const NEW_SOL_GL: &str = "global.openai.gpt-6.2-sol";
const NEW_ASTRA_US: &str = "us.openai.gpt-6.2-astra";
const NEW_ASTRA_GL: &str = "global.openai.gpt-6.2-astra";

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
fn seven_models_carry_their_other_profile_and_glm_has_none() {
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
    assert_eq!(alt(NEW_SOL_US), strs(&[NEW_SOL_GL]));
    assert_eq!(alt(NEW_ASTRA_US), strs(&[NEW_ASTRA_GL]));
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
    // A request names a model by its own id; the other profile is a route the control plane adds, not a choice.
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
    // Its first target stays GPT-6 Astra; GPT-6.2 Sol, appended to the list, follows; three routes of another vendor end the list.
    assert_eq!(
        ids(SOL_US),
        strs(&[
            SOL_GL, ASTRA_US, ASTRA_GL, NEW_SOL_US, NEW_SOL_GL, OPUS_US, OPUS_GL, SONNET_GL
        ])
    );
}

#[test]
fn astra_prefers_sol_then_the_anthropic_models() {
    assert_eq!(
        ids(ASTRA_US),
        strs(&[
            ASTRA_GL, SOL_US, SOL_GL, NEW_SOL_US, NEW_SOL_GL, OPUS_US, OPUS_GL, SONNET_GL
        ])
    );
}

/// The lists the release ships, id by id: the rule of `Models::failover_routes_for` on the shipped rows. A conversation was moved
/// along every hop with a new model that these lists hold, through the pinned Cline, before the rows were added: each such pair passed.
/// A pair that could not be measured is in no list: GLM 5.3 and Claude Fable 5.2 are no target of a new model, and a new model is no
/// target of theirs. The rule leaves those pairs out itself (`NOT_MEASURED` and the tests after this one).
#[test]
fn b862_the_real_lists_are_those_of_the_design() {
    let with_sharing = |chosen: &str, sharing: bool| -> Vec<String> {
        builtin()
            .failover_routes_for(chosen, sharing)
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    let lists: [(&str, [&str; 8]); 8] = [
        (
            OPUS_US,
            [
                OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM,
            ],
        ),
        (
            SONNET_GL,
            [
                SONNET_US, OPUS_US, OPUS_GL, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM,
            ],
        ),
        (
            SOL_US,
            [
                SOL_GL, ASTRA_US, ASTRA_GL, NEW_SOL_US, NEW_SOL_GL, OPUS_US, OPUS_GL, SONNET_GL,
            ],
        ),
        (
            ASTRA_US,
            [
                ASTRA_GL, SOL_US, SOL_GL, NEW_SOL_US, NEW_SOL_GL, OPUS_US, OPUS_GL, SONNET_GL,
            ],
        ),
        (
            GLM,
            [
                OPUS_US, OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL,
            ],
        ),
        (
            FABLE_GL,
            [
                FABLE_US, OPUS_US, OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US,
            ],
        ),
        (
            NEW_SOL_US,
            [
                NEW_SOL_GL, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, OPUS_US, OPUS_GL, SONNET_GL,
            ],
        ),
        (
            NEW_ASTRA_US,
            [
                NEW_ASTRA_GL,
                SOL_US,
                SOL_GL,
                ASTRA_US,
                ASTRA_GL,
                OPUS_US,
                OPUS_GL,
                SONNET_GL,
            ],
        ),
    ];
    // Every row of the list is a home here, in file order.
    let homes: Vec<&str> = builtin().models().iter().map(|m| m.id.as_str()).collect();
    assert_eq!(homes, lists.map(|(home, _)| home));
    for (home, want) in lists {
        assert_eq!(ids(home), strs(&want), "{home}");
        // Provider data sharing changes no list: the one model that needs it keeps the routes it had.
        assert_eq!(with_sharing(home, true), strs(&want), "{home} with sharing");
    }
    let is_new = |id: &str| id.contains("gpt-6.2");
    for (home, routes) in lists {
        let unmeasured = home == GLM || home == FABLE_GL;
        for r in routes {
            if is_new(home) {
                assert!(
                    r != GLM && !r.contains("fable"),
                    "{home}: {r} is a pair that was not measured"
                );
            }
            if unmeasured {
                assert!(!is_new(r), "{home}: {r} is a pair that was not measured");
            }
        }
        // Three routes of another vendor cover a failure of the whole vendor of an OpenAI home.
        if home.contains("openai") {
            assert_eq!(
                routes.iter().filter(|r| !r.contains("openai")).count(),
                3,
                "{home}"
            );
        }
    }
}

/// The pairs of models the switch record of the release lists as not measured, the home first. When the record was made GLM 5.3
/// answered no call (every call was throttled) and Claude Fable 5.2 answered none (the accounts of the record lacked the data retention mode
/// it needs), so no conversation reached either model, to or from GPT-6.2 Sol and GPT-6.2 Astra. A pair names the two models by the ids of
/// their rows and covers every route of both.
const NOT_MEASURED: [(&str, &str); 8] = [
    (GLM, NEW_SOL_US),
    (GLM, NEW_ASTRA_US),
    (FABLE_GL, NEW_SOL_US),
    (FABLE_GL, NEW_ASTRA_US),
    (NEW_SOL_US, GLM),
    (NEW_SOL_US, FABLE_GL),
    (NEW_ASTRA_US, GLM),
    (NEW_ASTRA_US, FABLE_GL),
];

/// The lists the release ships for the other engine, id by id, beside the lists of the default engine above. Its rows have no
/// second profile and it has no row of GLM 5.3. Every pair with a new model that these lists hold passed in the switch record, which moved
/// the conversations through the pinned Cline; no switch was measured through OpenCode itself.
#[test]
fn b862_the_lists_of_the_other_engine_are_those_of_the_design() {
    let catalog = builtin_catalog();
    let list = catalog.for_engine(Engine::OPENCODE).unwrap();
    let lists: [(&str, &[&str]); 7] = [
        (
            OPUS_US,
            &[SONNET_GL, SOL_US, ASTRA_US, NEW_SOL_US, NEW_ASTRA_US],
        ),
        (
            SONNET_GL,
            &[OPUS_US, SOL_US, ASTRA_US, NEW_SOL_US, NEW_ASTRA_US],
        ),
        (
            SOL_US,
            &[ASTRA_US, NEW_SOL_US, NEW_ASTRA_US, OPUS_US, SONNET_GL],
        ),
        (
            ASTRA_US,
            &[SOL_US, NEW_SOL_US, NEW_ASTRA_US, OPUS_US, SONNET_GL],
        ),
        // The model that needs provider data sharing keeps the list it had before the two models: its pairs with them were not measured.
        (FABLE_GL, &[OPUS_US, SONNET_GL, SOL_US, ASTRA_US]),
        (
            NEW_SOL_US,
            &[SOL_US, ASTRA_US, NEW_ASTRA_US, OPUS_US, SONNET_GL],
        ),
        (
            NEW_ASTRA_US,
            &[SOL_US, ASTRA_US, NEW_SOL_US, OPUS_US, SONNET_GL],
        ),
    ];
    // Every row of that engine's list is a home here, in file order.
    let homes: Vec<&str> = list.models().iter().map(|m| m.id.as_str()).collect();
    assert_eq!(homes, lists.map(|(home, _)| home));
    for (home, want) in lists {
        for sharing in [false, true] {
            let got: Vec<String> = catalog
                .failover_routes_for(Engine::OPENCODE, home, sharing)
                .into_iter()
                .map(|r| r.id)
                .collect();
            assert_eq!(got, strs(want), "{home} with sharing {sharing}");
        }
    }
}

/// No list of either engine holds a pair the switch record did not measure: for every home of such a pair in every engine's list,
/// on its own route and on its other profile, with and without provider data sharing.
#[test]
fn b862_no_list_of_either_engine_holds_a_pair_that_was_not_measured() {
    let catalog = builtin_catalog();
    let mut engines_read = Vec::new();
    for engine in catalog.engines() {
        let list = catalog.for_engine(engine).unwrap();
        for (home, target) in NOT_MEASURED {
            let Some(row) = list.models().iter().find(|m| m.id == home) else {
                continue;
            };
            let chosen = std::iter::once(row.id.clone())
                .chain(row.alternate_routes.iter().map(|a| a.id.clone()));
            for route in chosen {
                for sharing in [false, true] {
                    for r in list.failover_routes_for(&route, sharing) {
                        let of = list.find_route(&r.id).unwrap();
                        assert_ne!(
                            of.id, target,
                            "{engine}: a run on {route} with sharing {sharing} may switch to {}, a pair that was not measured",
                            r.id
                        );
                    }
                }
            }
            if !engines_read.contains(&engine) {
                engines_read.push(engine);
            }
        }
    }
    assert_eq!(engines_read, vec![Engine::CLINE, Engine::OPENCODE]);
}

/// The rule leaves such a pair out; the cut at eight routes is not what keeps it away. On a list of four of the shipped rows (GLM 5.3,
/// Claude Fable 5.2 and the two new models) no list reaches the cut.
#[test]
fn b862_a_pair_that_was_not_measured_is_left_out_by_the_rule_and_not_by_the_cut() {
    let mut doc: Value =
        serde_json::from_str(include_str!("../../../engine/catalog/models.json")).unwrap();
    doc["models"].as_array_mut().unwrap().retain(|r| {
        r.get("engine").is_none()
            && [GLM, FABLE_GL, NEW_SOL_US, NEW_ASTRA_US].contains(&r["id"].as_str().unwrap())
    });
    let list = Models::parse(&doc.to_string()).unwrap();
    assert_eq!(list.models().len(), 4);
    let ids = |chosen: &str, sharing: bool| -> Vec<String> {
        list.failover_routes_for(chosen, sharing)
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    // GLM 5.3 has no other profile, Claude Fable 5.2 is no route of a run that is not on it, and the pairs with the new models were not measured.
    assert_eq!(ids(GLM, false), strs(&[]));
    // Claude Fable 5.2: its other profile and GLM 5.3, never a new model, with the agreement and without it.
    assert_eq!(ids(FABLE_GL, true), strs(&[FABLE_US, GLM]));
    assert_eq!(ids(FABLE_GL, false), strs(&[FABLE_US, GLM]));
    // A new model: its other profile and the other new model, never GLM 5.3 and never Claude Fable 5.2.
    assert_eq!(
        ids(NEW_SOL_US, false),
        strs(&[NEW_SOL_GL, NEW_ASTRA_US, NEW_ASTRA_GL])
    );
    assert_eq!(
        ids(NEW_ASTRA_GL, true),
        strs(&[NEW_ASTRA_US, NEW_SOL_US, NEW_SOL_GL])
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
        (920_000, 131_072),
        "the other profile of Sol has Sol's limits, not the 0 the account catalog stores for it"
    );
    assert_eq!(sol_gl.region, "us-east-1");
    assert_eq!(sol_gl.protocol, Protocol::Converse);
    let astra = routes.iter().find(|r| r.id == ASTRA_US).unwrap();
    assert_eq!(astra.context_tokens, 920_000);
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
