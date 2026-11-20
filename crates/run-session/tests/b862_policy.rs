//! The session policy of a lease with the ids of the next model update.
//!
//! `start-session` mints the session's credentials with a policy over the run's own route and every failover route, and STS refuses an inline
//! policy of more than 2,048 characters. Two more models of one vendor change the failover lists (`Models::failover_routes_for`: at most four
//! routes of the home's vendor in file order, then the other vendors), so the proof that the longest lease still fits is made here,
//! on the list the Lambda builds: the shipped list, which holds the two new models since their rows were added.
#![allow(clippy::unwrap_used)]

use dispatcher::policy::{MAX_SESSION_POLICY_CHARS, session_policy_for_routes};
use ham_core::models::Catalog;
use harness_protocol::common::Engine;
use run_session::prepare::{failover_specs_in, model_spec_in};
use serde_json::Value;

const SHIPPED: &str = include_str!("../../../engine/catalog/models.json");
const NEW_IDS: [&str; 4] = [
    "us.openai.gpt-6.2-sol",
    "global.openai.gpt-6.2-sol",
    "us.openai.gpt-6.2-astra",
    "global.openai.gpt-6.2-astra",
];
/// The room the longest policy keeps under the limit, so that a later id a few characters longer still fits.
const ROOM: usize = 100;

/// The shipped list: the two new models are its last rows for each engine, GPT-6.2 Sol first.
fn fixture() -> Catalog {
    Catalog::parse(SHIPPED).unwrap()
}

/// The shipped list as it was before the two models: their rows taken out again.
fn earlier() -> Catalog {
    let mut doc: Value = serde_json::from_str(SHIPPED).unwrap();
    doc["models"]
        .as_array_mut()
        .unwrap()
        .retain(|r| !r["id"].as_str().unwrap().contains("gpt-6.2"));
    Catalog::parse(&doc.to_string()).unwrap()
}

/// The routes of a run's lease as `start-session` builds them: the run's own route, then its failover routes.
fn lease(catalog: &Catalog, engine: Engine, id: &str, sharing: bool) -> Vec<(String, String)> {
    let model = model_spec_in(catalog, engine, id).unwrap();
    std::iter::once((model.id, model.region))
        .chain(
            failover_specs_in(catalog, engine, id, sharing)
                .into_iter()
                .map(|r| (r.id, r.region)),
        )
        .collect()
}

fn policy(routes: &[(String, String)]) -> String {
    let refs: Vec<(&str, &str)> = routes
        .iter()
        .map(|(id, region)| (id.as_str(), region.as_str()))
        .collect();
    session_policy_for_routes(&refs).unwrap()
}

#[test]
fn b862_the_longest_lease_with_the_new_ids_fits_sts() {
    let catalog = fixture();
    let mut longest = (0, String::new());
    let mut homes = 0;
    for engine in catalog.engines() {
        let list = catalog.for_engine(engine).unwrap();
        for m in list.models().iter().filter(|m| m.selectable()) {
            for sharing in [false, true] {
                let routes = lease(&catalog, engine, &m.id, sharing);
                let chars = policy(&routes).chars().count();
                assert!(
                    chars <= MAX_SESSION_POLICY_CHARS,
                    "{engine} {} sharing {sharing}: {chars} characters",
                    m.id
                );
                if chars > longest.0 {
                    longest = (chars, format!("{engine} {} sharing {sharing}", m.id));
                }
            }
            homes += 1;
        }
    }
    // Both engines' rows were walked, the new ones among them.
    assert!(homes >= 8 + 7, "{homes} homes");
    assert!(
        longest.0 <= MAX_SESSION_POLICY_CHARS - ROOM,
        "the longest policy is {} characters ({}); it must keep {ROOM} under the limit of {MAX_SESSION_POLICY_CHARS}",
        longest.0,
        longest.1
    );
    // The number is written to the test's output: `cargo nextest run --no-capture` shows it.
    eprintln!(
        "b862: the longest session policy is {} characters ({})",
        longest.0, longest.1
    );
}

#[test]
fn b862_the_lease_of_each_new_model_names_both_of_its_profiles_and_three_routes_of_another_vendor()
{
    let catalog = fixture();
    let vendor = |route: &str| -> String {
        catalog
            .default_list()
            .find_route(route)
            .unwrap()
            .vendor
            .clone()
    };
    for home in [NEW_IDS[0], NEW_IDS[2]] {
        let routes = lease(&catalog, Engine::CLINE, home, false);
        assert_eq!(routes.len(), 9, "{home}: the run's route and eight more");
        let text = policy(&routes);
        let second = home.replacen("us.", "global.", 1);
        for id in [home, second.as_str()] {
            assert!(
                text.contains(&format!("inference-profile/{id}\"")),
                "{home}: the policy does not name {id}"
            );
        }
        let other = routes
            .iter()
            .filter(|(id, _)| vendor(id) != "openai")
            .count();
        assert_eq!(
            other, 3,
            "{home}: a lease covers three routes of another vendor: {routes:?}"
        );
    }
    // A run on an earlier model of the vendor may switch to the first new model, after the earlier model it could always switch to: its
    // lease covers both profiles of that new model.
    for home in ["us.openai.gpt-6.1-sol", "us.openai.gpt-6-astra"] {
        let routes = lease(&catalog, Engine::CLINE, home, false);
        let text = policy(&routes);
        for id in [NEW_IDS[0], NEW_IDS[1]] {
            assert!(
                text.contains(&format!("inference-profile/{id}\"")),
                "{home}: the policy does not name {id}"
            );
        }
        let other = routes
            .iter()
            .filter(|(id, _)| vendor(id) != "openai")
            .count();
        assert_eq!(other, 3, "{home}: {routes:?}");
    }
}

#[test]
fn b862_the_leases_of_the_shipped_list_are_the_ones_they_were() {
    // Two new rows at the end change no lease of a model of another vendor: same routes, same policy text.
    let shipped = earlier();
    let with_new = fixture();
    for m in shipped
        .default_list()
        .models()
        .iter()
        .filter(|m| m.vendor != "openai")
    {
        for sharing in [false, true] {
            assert_eq!(
                policy(&lease(&with_new, Engine::CLINE, &m.id, sharing)),
                policy(&lease(&shipped, Engine::CLINE, &m.id, sharing)),
                "{} sharing {sharing}",
                m.id
            );
        }
    }
}
