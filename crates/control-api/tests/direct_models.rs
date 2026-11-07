//! `engine/catalog/models.json`: the five supported models, each with
//! its one route, context window and smoke-record id. control-api refuses every other id, and a listed entry without a
//! passing smoke record (invariant 6). The new line does not use harness-catalog.

#![allow(clippy::unwrap_used)]

use control_api::direct_models::{
    DirectModels, ModelRefusal, ModelsError, builtin, models_response, models_response_of,
};
use serde_json::{Value, json};

const FIVE: [(&str, &str, u32); 5] = [
    ("us.anthropic.claude-opus-5-5", "converse", 1_000_000),
    ("global.anthropic.claude-sonnet-5-5", "converse", 1_000_000),
    ("us.openai.gpt-6.1-sol", "converse", 1_000_000),
    ("us.openai.gpt-6-astra", "converse", 1_050_000),
    ("us.zai.glm-5.3", "converse", 1_048_576),
];

const AUDITED: [&str; 1] = ["us.zai.glm-5.3"];

fn doc(models: Value) -> String {
    json!({"schemaVersion": 1, "models": models}).to_string()
}

fn entry(id: &str) -> Value {
    json!({
        "id": id, "name": "N", "vendor": "v", "baseModelId": "v.n", "protocol": "converse",
        "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 8192,
        "smokeRecordId": "s6:x+K3:y", "smokedAt": "2026-10-06T21:35:00Z"
    })
}

#[test]
fn the_built_in_file_lists_exactly_the_owners_five_models_in_order() {
    let m = builtin();
    let got: Vec<(&str, &str, u32)> = m
        .models()
        .iter()
        .map(|x| (x.id.as_str(), x.protocol.as_str(), x.context_tokens))
        .collect();
    assert_eq!(got, FIVE);
}

#[test]
fn every_built_in_model_has_a_smoke_record_a_window_and_a_real_route() {
    for x in builtin().models() {
        let id = x.smoke_record_id.as_deref().unwrap_or_default();
        if AUDITED.contains(&x.id.as_str()) {
            assert!(
                id.starts_with(&format!("s6:{}:", x.id)) && id.contains("+audit:2026-10-13/"),
                "{}: smoke id {id:?}",
                x.id
            );
            // The audit's record is committed beside the list, and it says what the row says.
            let file = id.rsplit('/').next().unwrap();
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../engine/catalog")
                .join(file);
            let rec: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(rec["profileId"], x.id.as_str());
            assert_eq!(rec["bedrockBaseModelId"], x.base_model_id.as_str());
            assert_eq!(rec["protocol"], x.protocol.as_str());
            assert_eq!(rec["limits"]["contextTokensMeasured"], x.context_tokens);
            assert_eq!(rec["limits"]["maxOutputTokens"], x.max_output_tokens);
            assert_eq!(rec["regions"][x.region.as_str()], "pass");
            assert_eq!(rec["mantle"], false, "GLM 5.3 is Converse only");
        } else {
            assert!(
                id.starts_with("s6:") && id.contains("+K3:"),
                "{}: smoke id {:?}",
                x.id,
                x.smoke_record_id
            );
        }
        assert!(x.context_tokens >= 100_000, "{}", x.id);
        assert!(x.max_output_tokens > 0, "{}", x.id);
        assert!(!x.region.is_empty() && !x.base_model_id.is_empty() && !x.name.is_empty());
    }
}

#[test]
fn the_alternate_routes_equal_the_committed_models_json() {
    // The built-in list is the compiled-in copy of engine/catalog/models.json: every alternate route it carries is the one in the file,
    // with the same Region, smoke record and smoke time, and a model's alternates are profiles of the same base model.
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../engine/catalog/models.json");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let rows = file["models"].as_array().unwrap();
    let mut checked = 0;
    for x in builtin().models() {
        let row = rows
            .iter()
            .find(|r| r["id"] == x.id.as_str())
            .unwrap_or_else(|| panic!("{} is not in models.json", x.id));
        let alternates = row["alternateRoutes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(alternates.len(), x.alternate_routes.len(), "{}", x.id);
        for a in &x.alternate_routes {
            let file_route = alternates
                .iter()
                .find(|r| r["id"] == a.id.as_str())
                .unwrap_or_else(|| panic!("{} has no alternate {} in models.json", x.id, a.id));
            assert_eq!(file_route["region"], a.region.as_str(), "{}", a.id);
            assert_eq!(
                file_route["smokeRecordId"],
                a.smoke_record_id.as_str(),
                "{}",
                a.id
            );
            assert_eq!(
                file_route["smokedAt"].as_str(),
                a.smoked_at.as_deref(),
                "{}",
                a.id
            );
            assert!(
                a.id.ends_with(x.base_model_id.as_str()),
                "{}: not a profile of {}",
                a.id,
                x.base_model_id
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked, 4,
        "Opus, Sonnet, Sol and Astra each carry one other profile"
    );
}

#[test]
fn the_list_a_client_sees_does_not_show_the_alternate_routes() {
    // `GET /v1/direct/models` lists the five a run may name; an alternate is a route the control plane adds, never a choice.
    let ids: Vec<String> = builtin().list().models.into_iter().map(|m| m.id).collect();
    assert_eq!(ids.len(), 5);
    assert!(
        ids.iter()
            .all(|i| !i.starts_with("global.anthropic.claude-opus"))
    );
    let rows = models_response().rows;
    assert_eq!(rows.len(), 5);
}

#[test]
fn the_five_are_accepted() {
    for (id, _, window) in FIVE {
        let m = builtin().check(id).unwrap();
        assert_eq!((m.id.as_str(), m.context_tokens), (id, window));
    }
}

#[test]
fn an_unknown_model_is_refused_and_the_message_lists_the_five() {
    for id in [
        "",
        " ",
        "claude",
        "OPUS",
        "zai.glm-5",
        "us.zai.glm-5",
        "zai.glm-5.3",
        "global.zai.glm-5.3",
        "opus",
        "sonnet",
        "us.anthropic.claude-opus-5-5 ",
        "us.anthropic.claude-opus-5-5\n",
        "anthropic.claude-opus-5-5",
        "global.anthropic.claude-opus-5-5",
        "aws-mantle/zai.glm-5",
        "us.anthropic.claude-fable-5-1",
        "us.anthropic.claude-sonnet-5-5",
    ] {
        let e = builtin().check(id).unwrap_err();
        assert!(matches!(e, ModelRefusal::Unknown { .. }), "{id:?}: {e:?}");
        let text = e.to_string();
        for (known, _, _) in FIVE {
            assert!(text.contains(known), "{id:?}: {text}");
        }
    }
}

#[test]
fn a_listed_model_without_a_smoke_record_is_refused_as_not_smoked() {
    for smoke in [json!(null), json!(""), json!("   ")] {
        let mut e = entry("zai.glm-5.3");
        e["smokeRecordId"] = smoke;
        let m = DirectModels::parse(&doc(json!([e]))).unwrap();
        let r = m.check("zai.glm-5.3").unwrap_err();
        assert_eq!(
            r,
            ModelRefusal::NotSmoked {
                id: "zai.glm-5.3".into()
            }
        );
        assert!(r.to_string().contains("smoke"), "{r}");
    }
    // A missing member is the same as null.
    let mut e = entry("zai.glm-5.3");
    e.as_object_mut().unwrap().remove("smokeRecordId");
    let m = DirectModels::parse(&doc(json!([e]))).unwrap();
    assert!(matches!(
        m.check("zai.glm-5.3"),
        Err(ModelRefusal::NotSmoked { .. })
    ));
}

#[test]
fn the_parser_refuses_a_file_that_would_dispatch_with_no_window_or_twice() {
    let bad = |models: Value| DirectModels::parse(&doc(models)).unwrap_err();
    let mut zero = entry("a.b");
    zero["contextTokens"] = json!(0);
    assert!(matches!(bad(json!([zero])), ModelsError::Invalid(m) if m.contains("contextTokens")));
    assert!(
        matches!(bad(json!([entry("a.b"), entry("a.b")])), ModelsError::Invalid(m) if m.contains("a.b"))
    );
    assert!(matches!(bad(json!([])), ModelsError::Invalid(m) if m.contains("no models")));
    let mut blank = entry("a.b");
    blank["id"] = json!(" ");
    assert!(matches!(bad(json!([blank])), ModelsError::Invalid(_)));
    let mut proto = entry("a.b");
    proto["protocol"] = json!("telepathy");
    assert!(matches!(bad(json!([proto])), ModelsError::Parse(_)));
    let mut extra = entry("a.b");
    extra["notes"] = json!("x");
    assert!(
        matches!(bad(json!([extra])), ModelsError::Parse(_)),
        "unknown members fail loud"
    );
    assert!(matches!(
        DirectModels::parse(&json!({"schemaVersion": 2, "models": [entry("a.b")]}).to_string()),
        Err(ModelsError::Invalid(m)) if m.contains("schemaVersion")
    ));
    assert!(matches!(
        DirectModels::parse("not json"),
        Err(ModelsError::Parse(_))
    ));
}

#[test]
fn the_models_endpoint_lists_only_the_models_that_can_be_selected() {
    // review S5 (models_unsmoked_shown): every built-in row is smoked, so the filter needs a list with one that is not
    let mut unsmoked = entry("us.example.unsmoked");
    unsmoked["smokeRecordId"] = json!("  ");
    let mut absent = entry("us.example.absent");
    absent.as_object_mut().unwrap().remove("smokeRecordId");
    let list =
        DirectModels::parse(&doc(json!([entry("us.example.smoked"), unsmoked, absent]))).unwrap();
    let rows = models_response_of(&list).rows;
    assert_eq!(
        rows.iter()
            .map(|r| r.profile_id.as_str())
            .collect::<Vec<_>>(),
        ["us.example.smoked"]
    );
    assert_eq!(rows[0].smoke_record_id, "s6:x+K3:y");
    // the endpoint over the built-in list shows its five
    assert_eq!(models_response().rows.len(), 5);
}
