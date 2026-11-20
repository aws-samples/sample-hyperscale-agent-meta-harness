//! The failover routes of a model whose vendor has many models: a bound for the routes of the home model's own vendor.
//!
//! A request lists at most `MAX_DIRECT_ROUTES` (8) routes after the chosen model, because the session policy over nine routes must fit the limit
//! of STS. Before this rule the list took every model of the home's vendor first. With four models of one vendor that left one route of
//! another vendor, which is thin cover when the whole vendor fails. The rule (`Models::failover_routes_for`):
//!
//! 1. the home model's other profile;
//! 2. routes of the other models of the home's vendor in file order, at most four routes;
//! 3. the other vendors in file order;
//! 4. the routes of the home's vendor that step 2 left out, if places are left.
//!
//! The fixture is the list as it was before the two models of the update, plus those two models, appended, with their real ids and a test
//! record. The list ships their rows now, so the fixture takes them out first: the rule is tested on rows this file controls. The lists of the
//! shipped rows are pinned in `tests/model_routes.rs`.

#![allow(clippy::unwrap_used)]

use ham_core::models::{Catalog, Model, Models};
use harness_protocol::common::Engine;
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
const NEW_SOL_US: &str = "us.openai.gpt-6.2-sol";
const NEW_SOL_GL: &str = "global.openai.gpt-6.2-sol";
const NEW_ASTRA_US: &str = "us.openai.gpt-6.2-astra";
const NEW_ASTRA_GL: &str = "global.openai.gpt-6.2-astra";

const SHIPPED: &str = include_str!("../../../engine/catalog/models.json");

/// A made-up row of a new model: the real ids, a test record. A Cline row carries the second profile and a reasoning object; a row of another
/// engine carries neither, as the shipped rows of that engine do.
fn new_row(base: &str, name: &str, engine: Option<&str>) -> Value {
    let mut row = json!({
        "id": format!("us.{base}"), "name": name, "vendor": "openai", "baseModelId": base,
        "protocol": "converse", "region": "us-east-1", "contextTokens": 1_050_000, "maxOutputTokens": 128_000,
        "smokeRecordId": "s6:test-record"
    });
    match engine {
        Some(e) => row["engine"] = json!(e),
        None => {
            row["reasoning"] = json!({
                "default": "high", "accepted": ["low", "medium", "high", "xhigh"], "smokeRecordId": "b649:test-record"
            });
            row["alternateRoutes"] = json!([{"id": format!("global.{base}"), "region": "us-east-1", "smokeRecordId": "s6:test-record"}]);
        }
    }
    row
}

/// The shipped list as it was before the two models of the update: their rows taken out again.
fn earlier_text() -> String {
    let mut doc: Value = serde_json::from_str(SHIPPED).unwrap();
    doc["models"]
        .as_array_mut()
        .unwrap()
        .retain(|r| !r["id"].as_str().unwrap().contains("gpt-6.2"));
    doc.to_string()
}

/// The earlier list with the two new models appended to each engine's rows, GPT-6.2 Sol first.
fn fixture_text() -> String {
    let mut doc: Value = serde_json::from_str(&earlier_text()).unwrap();
    let rows = doc["models"].as_array_mut().unwrap();
    for engine in [None, Some("opencode")] {
        rows.push(new_row("openai.gpt-6.2-sol", "GPT-6.2 Sol", engine));
        rows.push(new_row("openai.gpt-6.2-astra", "GPT-6.2 Astra", engine));
    }
    doc.to_string()
}

fn fixture() -> Models {
    Models::parse(&fixture_text()).unwrap()
}

/// The list a home had before the update: the shipped rows without the two new models.
fn shipped() -> Models {
    Models::parse(&earlier_text()).unwrap()
}

fn ids(list: &Models, chosen: &str, sharing: bool) -> Vec<String> {
    list.failover_routes_for(chosen, sharing)
        .into_iter()
        .map(|r| r.id)
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn vendor_of<'a>(list: &'a Models, route: &str) -> &'a str {
    list.find_route(route).unwrap().vendor.as_str()
}

/// The list as it was built before this rule: the home's other profile, every other model of the home's vendor in file order, then the rest
/// in file order, cut at the limit. Written out here so that "the same as before" is a comparison and not a memory.
fn before(list: &Models, chosen: &str, sharing: bool) -> Vec<String> {
    let home = list.find_route(chosen).unwrap();
    let allow = sharing && home.requires_provider_sharing;
    let routes = |m: &Model| -> Vec<String> {
        if !m.selectable() {
            return Vec::new();
        }
        std::iter::once(m.id.clone())
            .chain(m.alternate_routes.iter().map(|a| a.id.clone()))
            .collect()
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |id: String| {
        if id != chosen && !out.contains(&id) {
            out.push(id);
        }
    };
    routes(home).into_iter().for_each(&mut push);
    for same_vendor in [true, false] {
        for m in list.models().iter().filter(|m| {
            m.id != home.id
                && (allow || !m.requires_provider_sharing)
                && (m.vendor == home.vendor) == same_vendor
        }) {
            routes(m).into_iter().for_each(&mut push);
        }
    }
    out.truncate(MAX_DIRECT_ROUTES);
    out
}

#[test]
fn b862_same_vendor_targets_stop_at_four_routes_and_other_vendors_follow() {
    let list = fixture();
    for home in [NEW_SOL_US, NEW_ASTRA_US, SOL_US, ASTRA_US] {
        let routes = ids(&list, home, false);
        assert_eq!(routes.len(), MAX_DIRECT_ROUTES, "{home}: {routes:?}");
        let own_vendor: Vec<&String> = routes
            .iter()
            .filter(|r| vendor_of(&list, r) == "openai")
            .collect();
        let other: Vec<&String> = routes
            .iter()
            .filter(|r| vendor_of(&list, r) != "openai")
            .collect();
        // The home's other profile and four more routes of its vendor, then three routes of another vendor.
        assert_eq!(own_vendor.len(), 5, "{home}: {routes:?}");
        assert_eq!(other.len(), 3, "{home}: {routes:?}");
        assert!(
            routes[..5].iter().all(|r| vendor_of(&list, r) == "openai"),
            "{home}: the vendor's own routes come first: {routes:?}"
        );
        assert_eq!(
            list.find_route(&routes[0]).unwrap().id,
            home,
            "{home}: its own other profile is the first route"
        );
    }
}

#[test]
fn b862_the_vendors_own_routes_come_in_file_order() {
    let list = fixture();
    let rest = [OPUS_US, OPUS_GL, SONNET_GL];
    for (home, own_vendor) in [
        // A new model falls back to the two earlier models of its vendor.
        (NEW_SOL_US, [NEW_SOL_GL, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL]),
        (
            NEW_ASTRA_US,
            [NEW_ASTRA_GL, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL],
        ),
        // An earlier model keeps the first target it had, the other earlier model, and the appended model follows it.
        (SOL_US, [SOL_GL, ASTRA_US, ASTRA_GL, NEW_SOL_US, NEW_SOL_GL]),
        (ASTRA_US, [ASTRA_GL, SOL_US, SOL_GL, NEW_SOL_US, NEW_SOL_GL]),
    ] {
        let want: Vec<String> = own_vendor
            .iter()
            .chain(rest.iter())
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ids(&list, home, false), want, "{home}");
    }
    // A run that is on the model's second profile gets the model's own route first and the same list after it.
    assert_eq!(
        ids(&list, NEW_SOL_GL, false),
        strs(&[
            NEW_SOL_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, OPUS_US, OPUS_GL, SONNET_GL
        ])
    );
    // The first target of an earlier model after its own second profile is the one it has on the shipped list.
    for home in [SOL_US, ASTRA_US] {
        assert_eq!(
            ids(&list, home, false)[..3],
            ids(&shipped(), home, false)[..3],
            "{home}"
        );
    }
}

#[test]
fn b862_homes_of_other_vendors_keep_their_lists() {
    for (name, list) in [("the shipped list", shipped()), ("the fixture", fixture())] {
        // The lists `tests/model_routes.rs` pins, id by id: two new rows at the end change none of them.
        assert_eq!(
            ids(&list, OPUS_US, false),
            strs(&[
                OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM
            ]),
            "{name}"
        );
        assert_eq!(
            ids(&list, SONNET_GL, false),
            strs(&[
                SONNET_US, OPUS_US, OPUS_GL, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL, GLM
            ]),
            "{name}"
        );
        assert_eq!(
            ids(&list, GLM, false),
            strs(&[
                OPUS_US, OPUS_GL, SONNET_GL, SONNET_US, SOL_US, SOL_GL, ASTRA_US, ASTRA_GL
            ]),
            "{name}"
        );
        // Every home of another vendor, on either profile, with and without provider data sharing (the model that needs it has four routes
        // of its own vendor after its other profile, which is exactly the bound).
        for m in list.models().iter().filter(|m| m.vendor != "openai") {
            let chosen = std::iter::once(m.id.clone())
                .chain(m.alternate_routes.iter().map(|a| a.id.clone()));
            for route in chosen {
                for sharing in [false, true] {
                    assert_eq!(
                        ids(&list, &route, sharing),
                        before(&list, &route, sharing),
                        "{name}: {route} sharing {sharing}"
                    );
                }
            }
        }
    }
    assert_eq!(
        ids(&fixture(), FABLE_GL, true)[1..5],
        strs(&[OPUS_US, OPUS_GL, SONNET_GL, SONNET_US])[..]
    );
}

/// The shipped rows today: two models of one vendor never reach the bound, so every list is the one it was.
#[test]
fn b862_the_shipped_list_gives_every_home_the_list_it_had() {
    let list = shipped();
    for m in list.models() {
        let chosen =
            std::iter::once(m.id.clone()).chain(m.alternate_routes.iter().map(|a| a.id.clone()));
        for route in chosen {
            for sharing in [false, true] {
                assert_eq!(
                    ids(&list, &route, sharing),
                    before(&list, &route, sharing),
                    "{route} sharing {sharing}"
                );
            }
        }
    }
}

/// Step 4: the bound orders the list, it drops nothing while places are left.
#[test]
fn b862_routes_the_bound_left_out_follow_the_other_vendors_when_places_are_left() {
    let row = |id: &str, vendor: &str, second: bool| {
        let base = id.split_once('.').unwrap().1;
        let mut r = json!({
            "id": id, "name": id, "vendor": vendor, "baseModelId": base, "protocol": "converse", "region": "us-east-1",
            "contextTokens": 100_000, "maxOutputTokens": 4096, "smokeRecordId": "s6:test-record"
        });
        if second {
            r["alternateRoutes"] = json!([{"id": format!("global.{base}"), "region": "us-east-1", "smokeRecordId": "s6:test-record"}]);
        }
        r
    };
    let doc = json!({"schemaVersion": 1, "models": [
        row("us.v.a-1", "v", true),
        row("us.v.b-1", "v", true),
        row("us.w.x-1", "w", false),
        row("us.v.c-1", "v", true),
        row("us.v.d-1", "v", false),
    ]});
    let list = Models::parse(&doc.to_string()).unwrap();
    // Home b-1: its other profile; a-1 and c-1 in file order, four routes; the other vendor; then d-1, which the bound left out.
    assert_eq!(
        ids(&list, "us.v.b-1", false),
        strs(&[
            "global.v.b-1",
            "us.v.a-1",
            "global.v.a-1",
            "us.v.c-1",
            "global.v.c-1",
            "us.w.x-1",
            "us.v.d-1"
        ])
    );
    assert_eq!(
        ids(&list, "us.v.a-1", false),
        strs(&[
            "global.v.a-1",
            "us.v.b-1",
            "global.v.b-1",
            "us.v.c-1",
            "global.v.c-1",
            "us.w.x-1",
            "us.v.d-1"
        ])
    );
    // A home of the small vendor is not bounded by the size of the other vendor: file order, cut at the limit.
    assert_eq!(
        ids(&list, "us.w.x-1", false),
        strs(&[
            "us.v.a-1",
            "global.v.a-1",
            "us.v.b-1",
            "global.v.b-1",
            "us.v.c-1",
            "global.v.c-1",
            "us.v.d-1"
        ])
    );
}

/// Each engine's list follows the same rule on its own rows: a switch never leaves the engine.
#[test]
fn b862_the_rule_holds_inside_another_engines_list() {
    let catalog = Catalog::parse(&fixture_text()).unwrap();
    let ids = |chosen: &str| -> Vec<String> {
        catalog
            .failover_routes(Engine::OPENCODE, chosen)
            .into_iter()
            .map(|r| r.id)
            .collect()
    };
    // The rows of that engine have no second profile: the home's vendor in file order, then the other vendors.
    assert_eq!(
        ids(NEW_SOL_US),
        strs(&[SOL_US, ASTRA_US, NEW_ASTRA_US, OPUS_US, SONNET_GL])
    );
    assert_eq!(
        ids(ASTRA_US),
        strs(&[SOL_US, NEW_SOL_US, NEW_ASTRA_US, OPUS_US, SONNET_GL])
    );
    // Two appended rows give every home of that engine two more routes, after the ones it had.
    assert_eq!(
        ids(OPUS_US),
        strs(&[SONNET_GL, SOL_US, ASTRA_US, NEW_SOL_US, NEW_ASTRA_US])
    );
    let shipped = Catalog::parse(&earlier_text()).unwrap();
    let before: Vec<String> = shipped
        .failover_routes(Engine::OPENCODE, OPUS_US)
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(before, strs(&[SONNET_GL, SOL_US, ASTRA_US]));
}
