//! Claude Fable 5.2 in the model catalog and the `requiresProviderSharing` row member. The row is listed and marked as a model that needs provider data
//! sharing. It carries the live smoke record of both of its routes and its reasoning table (2026-11-10), so it is selectable; a row without a passing smoke
//! record stays pending and refused (root invariant 6). A model that needs sharing is a failover target only for a run that has sharing on.

#![allow(clippy::unwrap_used)]

use ham_core::models::{
    ModelPrefs, ModelSource, Models, ModelsError, SharingRefusal, builtin, render_models, resolve,
};
use serde_json::{Value, json};

const FABLE: &str = "global.anthropic.claude-fable-5-2";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";

fn doc(models: Value) -> String {
    json!({"schemaVersion": 1, "models": models}).to_string()
}

fn entry(id: &str, alias: &str, vendor: &str, smoked: bool, shares: bool) -> Value {
    let mut v = json!({
        "id": id, "name": format!("Name of {id}"), "vendor": vendor, "alias": alias, "baseModelId": "v.n", "protocol": "converse",
        "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 8192
    });
    if smoked {
        v["smokeRecordId"] = json!("s6:x+K3:y");
        v["smokedAt"] = json!("2026-10-06T21:35:00Z");
    }
    if shares {
        v["requiresProviderSharing"] = json!(true);
    }
    v
}

/// The committed list with the Fable row given a test record in place of the live one, so the rules of a selectable sharing row run on a known value.
fn builtin_with_fable_smoked() -> Models {
    let text = include_str!("../../../engine/catalog/models.json");
    let mut v: Value = serde_json::from_str(text).unwrap();
    let rows = v["models"].as_array_mut().unwrap();
    let fable = rows.iter_mut().find(|r| r["id"] == FABLE).unwrap();
    fable["smokeRecordId"] = json!("s6:test-record");
    fable["smokedAt"] = json!("2026-11-09T00:00:00Z");
    Models::parse(&v.to_string()).unwrap()
}

#[test]
fn b803_the_catalog_lists_claude_fable_5_2_after_the_five() {
    let m = builtin();
    assert_eq!(m.models().len(), 6);
    let last = m.models().last().unwrap();
    assert_eq!(last.id, FABLE);
    assert_eq!(last.name, "Claude Fable 5.2");
    assert_eq!(last.vendor, "anthropic");
    assert_eq!(last.alias.as_deref(), Some("fable"));
    assert_eq!(last.base_model_id, "anthropic.claude-fable-5-2");
    assert_eq!(last.protocol.as_str(), "converse");
    assert_eq!(
        (last.context_tokens, last.max_output_tokens),
        (1_000_000, 128_000)
    );
    assert!(last.requires_provider_sharing);
    assert!(!last.default);
    assert_eq!(m.find("fable").unwrap().id, FABLE);
    assert_eq!(m.find("FABLE").unwrap().id, FABLE);
}

#[test]
fn b803_only_the_fable_row_needs_sharing() {
    for x in builtin().models() {
        assert_eq!(x.requires_provider_sharing, x.id == FABLE, "{}", x.id);
    }
}

#[test]
fn b803_the_fable_row_carries_the_live_record_of_both_routes_and_is_selectable() {
    let f = builtin().find("fable").unwrap();
    assert_eq!(
        f.smoke_record_id.as_deref(),
        Some(
            "s6:global.anthropic.claude-fable-5-2:us-east-1:converse+audit:2026-11-10/smoke-global.anthropic.claude-fable-5-2.json"
        )
    );
    assert!(
        f.smoked_at
            .as_deref()
            .is_some_and(|t| t.starts_with("2026-11-10T"))
    );
    assert!(f.selectable());
    let r = f.reasoning.as_ref().expect("the live reasoning table");
    assert!(r.accepted.contains(&r.default));
    assert_eq!(f.alternate_routes.len(), 1);
    let alt = &f.alternate_routes[0];
    assert_eq!(alt.id, "us.anthropic.claude-fable-5-2");
    assert_eq!(alt.region, "us-east-1");
    assert!(
        alt.smoke_record_id
            .starts_with("s6:us.anthropic.claude-fable-5-2:us-east-1:converse+audit:2026-11-10/")
    );
    let r = resolve(builtin(), Some("fable"), &ModelPrefs::default()).unwrap();
    assert_eq!(r.model.id, FABLE);
    assert_eq!(builtin().check(FABLE).unwrap().id, FABLE);
}

#[test]
fn b803_the_default_is_still_sonnet_and_the_other_rows_are_unchanged() {
    let m = builtin();
    assert_eq!(m.default_model().unwrap().id, SONNET);
    assert_eq!(
        m.models().iter().filter(|x| x.selectable()).count(),
        6,
        "the five that were selectable still are, and Claude Fable 5.2 joins them"
    );
}

#[test]
fn b803_the_row_member_is_a_boolean_and_defaults_to_false() {
    let m = Models::parse(&doc(json!([entry("a.one", "one", "v", true, false)]))).unwrap();
    assert!(!m.models()[0].requires_provider_sharing);
    for bad in [json!("yes"), json!(1), json!(null)] {
        let mut e = entry("a.one", "one", "v", true, false);
        e["requiresProviderSharing"] = bad.clone();
        let r = Models::parse(&doc(json!([e])));
        assert!(matches!(r, Err(ModelsError::Parse(_))), "{bad}: {r:?}");
    }
}

#[test]
fn b803_the_list_carries_the_flag_and_leaves_it_off_the_wire_when_false() {
    let list = builtin().list();
    let fable = list.models.iter().find(|r| r.id == FABLE).unwrap();
    assert!(fable.requires_provider_sharing);
    assert!(fable.selectable);
    let wire = serde_json::to_value(&list).unwrap();
    for row in wire["models"].as_array().unwrap() {
        assert_eq!(
            row.get("requiresProviderSharing") == Some(&json!(true)),
            row["id"] == FABLE,
            "{row}"
        );
    }
}

#[test]
fn b803_the_text_listing_marks_the_model_and_explains_the_setting() {
    let text = render_models(&builtin().list(), None);
    let line = text.lines().find(|l| l.contains(FABLE)).unwrap();
    assert!(line.contains("needs provider data sharing"), "{line}");
    assert!(
        !line.contains("pending"),
        "the row has its live record, so it is not pending: {line}"
    );
    assert!(
        text.contains("run.provider_sharing") && text.contains("--provider-sharing"),
        "the listing names the setting and the flag: {text}"
    );
    let other = text.lines().find(|l| l.contains(OPUS)).unwrap();
    assert!(!other.contains("provider data sharing"), "{other}");
    // A list with no such row prints no sharing paragraph.
    let plain = render_models(
        &Models::parse(&doc(json!([entry("a.one", "one", "v", true, false)])))
            .unwrap()
            .list(),
        None,
    );
    assert!(!plain.contains("provider_sharing"), "{plain}");
}

#[test]
fn b803_a_smoked_sharing_row_is_selectable_but_refused_without_the_setting() {
    let m = builtin_with_fable_smoked();
    let fable = m.find("fable").unwrap();
    assert!(fable.selectable());
    let r = resolve(&m, Some("fable"), &ModelPrefs::default()).unwrap();
    assert_eq!(r.model.id, FABLE);
    assert_eq!(r.source, ModelSource::Flag);
    assert_eq!(fable.check_sharing(true), Ok(()));
    let e = fable.check_sharing(false).unwrap_err();
    assert_eq!(
        e,
        SharingRefusal {
            id: FABLE.into(),
            name: "Claude Fable 5.2".into()
        }
    );
    // A model that needs nothing passes either way.
    let opus = m.find("opus").unwrap();
    assert_eq!(opus.check_sharing(false), Ok(()));
    assert_eq!(opus.check_sharing(true), Ok(()));
}

#[test]
fn b803_the_refusal_names_the_model_the_reason_and_the_three_ways_to_turn_it_on() {
    let e = builtin_with_fable_smoked()
        .find("fable")
        .unwrap()
        .check_sharing(false)
        .unwrap_err();
    let text = e.to_string();
    for want in [
        "Claude Fable 5.2",
        FABLE,
        "needs provider data sharing",
        "prompts and answers",
        "model's provider",
        "30 days",
        "ham config set run.provider_sharing on",
        "Settings screen",
        "--provider-sharing on",
        "Nothing was sent",
    ] {
        assert!(text.contains(want), "{want:?} missing: {text}");
    }
    assert!(!text.contains('\n'), "one message: {text}");
    // The control plane's words share the reason and name the request member instead of the client's three ways.
    let server = e.server_message();
    for want in [
        "Claude Fable 5.2",
        FABLE,
        "needs provider data sharing",
        "prompts and answers",
        "model's provider",
        "30 days",
        "providerSharing",
    ] {
        assert!(server.contains(want), "{want:?} missing: {server}");
    }
    assert_eq!(e.code(), "provider-sharing-required");
}

#[test]
fn b803_a_run_on_a_model_that_needs_nothing_never_switches_to_a_sharing_model_whatever_the_setting_says()
 {
    let m = builtin_with_fable_smoked();
    let ids = |chosen: &str, sharing: bool| -> Vec<String> {
        m.failover_routes_for(chosen, sharing)
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    for chosen in [
        OPUS,
        SONNET,
        "us.openai.gpt-6.1-sol",
        "us.openai.gpt-6-astra",
        "us.zai.glm-5.3",
    ] {
        for sharing in [false, true] {
            assert!(
                !ids(chosen, sharing).iter().any(|i| i.contains("fable")),
                "{chosen} with sharing {sharing}: it chose its model, not that provider"
            );
        }
    }
    // The default call is the safe one.
    assert_eq!(m.failover_routes(OPUS), m.failover_routes_for(OPUS, false));
    // The routes of the other models are what they were before the Fable row existed.
    assert_eq!(ids(OPUS, true), ids(OPUS, false));
}

#[test]
fn b803_a_run_on_the_sharing_model_with_sharing_on_fails_over_to_the_others_and_never_to_itself() {
    let m = builtin_with_fable_smoked();
    let from_fable: Vec<String> = m
        .failover_routes_for(FABLE, true)
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert!(!from_fable.is_empty());
    assert!(from_fable.iter().all(|i| i != FABLE));
    assert!(
        from_fable.iter().any(|i| i == SONNET || i == OPUS),
        "{from_fable:?}"
    );
    // Without the agreement the same run is offered only models that need none.
    let without: Vec<String> = m
        .failover_routes_for(FABLE, false)
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(
        without, from_fable,
        "no other row needs sharing today, so the lists are equal"
    );
}

#[test]
fn b803_the_committed_sharing_row_is_never_a_failover_route_of_a_model_that_needs_nothing() {
    for chosen in [OPUS, SONNET] {
        for sharing in [false, true] {
            assert!(
                builtin()
                    .failover_routes_for(chosen, sharing)
                    .iter()
                    .all(|s| !s.id.contains("fable")),
                "{chosen} {sharing}"
            );
        }
    }
}

#[test]
fn b803_one_id_for_the_fable_route_lives_in_the_catalog_only() {
    // The id of the route is the row's `id`, written once for each engine that lists the model in models.json;
    // the second profile is the one `alternateRoutes` entry of the Cline row and the base id is written once for each row. Everything else
    // in the file that holds the text is a record id.
    let text = include_str!("../../../engine/catalog/models.json");
    assert_eq!(
        text.matches("\"id\": \"global.anthropic.claude-fable-5-2\"")
            .count(),
        2
    );
    assert_eq!(
        text.matches("\"id\": \"us.anthropic.claude-fable-5-2\"")
            .count(),
        1
    );
    assert_eq!(
        text.matches("\"baseModelId\": \"anthropic.claude-fable-5-2\"")
            .count(),
        2
    );
}
