//! The utility catalog this build ships: the one row every helper call outside the engine uses by
//! default, and the walls that keep it apart from the five models a person can pick. The parser and `resolve` are tested on fixture
//! catalogs in `utility.rs`; this file reads the committed file.

use ham_core::config::{Config, set_key};
use ham_core::models::{self, ModelPrefs};
use ham_core::utility::{UtilityChoice, UtilityModels};
use harness_protocol::common::Protocol;
use serde_json::json;

#[test]
fn built_in_default_is_haiku_us_profile() {
    let catalog = UtilityModels::builtin();
    assert_eq!(catalog.models().len(), 1, "one row today");
    let row = catalog.default_model();
    assert_eq!(row.alias, "haiku");
    assert_eq!(row.id, "us.anthropic.claude-haiku-5-5");
    assert_eq!(row.name, "Claude Haiku 5.5");
    assert_eq!(row.vendor, "anthropic");
    assert_eq!(row.base_model_id, "anthropic.claude-haiku-5-5");
    assert_eq!(row.protocol, Protocol::Converse);
    assert_eq!(row.region, "us-east-1");
    assert!(
        row.selectable(),
        "the default row carries its passing smoke record"
    );
    assert_eq!(
        row.additional_model_request_fields,
        Some(json!({"thinking": {"type": "disabled"}})),
        "with the default thinking, a 60-token cap came back empty in 19 of 24 odd-input calls"
    );
    assert!(
        row.alternate_routes.is_empty(),
        "the global profile is a data row added on a measured trigger, not now"
    );
    let record = row.smoke_record_id.as_deref().unwrap();
    assert!(
        record.starts_with("s6:us.anthropic.claude-haiku-5-5:us-east-1:converse+u1:"),
        "{record}"
    );
    assert!(
        record.ends_with("/smoke-utility-us.anthropic.claude-haiku-5-5.json"),
        "{record}"
    );
    assert!(row.smoked_at.as_deref().is_some_and(|t| t.ends_with('Z')));
    assert_eq!(catalog.choices(), vec![format!("haiku = {}", row.id)]);
}

#[test]
fn utility_ids_are_disjoint_from_the_five_models() {
    let utility = UtilityModels::builtin();
    let coding = models::builtin();
    for u in utility.models() {
        let mut names: Vec<&str> = u.route_ids();
        names.push(&u.alias);
        for n in names {
            assert!(
                coding.find(n).is_none() && coding.find_route(n).is_none(),
                "{n} of the utility file is a coding model"
            );
        }
    }
    for m in coding.models() {
        let mut names: Vec<&str> = vec![m.id.as_str()];
        names.extend(m.alias.as_deref());
        names.extend(m.alternate_routes.iter().map(|a| a.id.as_str()));
        for n in names {
            assert!(
                utility.find(n).is_none(),
                "{n} of models.json is in the utility file"
            );
        }
    }
    assert_eq!(coding.models().len(), 6, "models.json keeps its six rows");
}

#[test]
fn model_flag_haiku_is_still_unknown_model() {
    let coding = models::builtin();
    let flag = models::resolve(coding, Some("haiku"), &ModelPrefs::default())
        .expect_err("--model haiku is not one of the five");
    let text = flag.to_string();
    assert!(text.contains("haiku"), "{text}");
    assert!(
        text.contains("sonnet") && text.contains("glm"),
        "the error lists the five: {text}"
    );
    for id in [
        "us.anthropic.claude-haiku-5-5",
        "global.anthropic.claude-haiku-5-5",
    ] {
        assert!(
            models::resolve(coding, Some(id), &ModelPrefs::default()).is_err(),
            "--model {id}"
        );
    }
    // HAMH_MODEL and `default_model` of the config file refuse it as well, with their own source named.
    let env = ModelPrefs {
        env: Some("haiku".into()),
        config: None,
    };
    assert!(models::resolve(coding, None, &env).is_err());
    assert!(models::resolve_given(coding, "haiku").is_err());
    let set = set_key(&Config::default(), "default_model", "haiku");
    assert!(
        set.is_err(),
        "ham config set default_model haiku must be refused"
    );
}

#[test]
fn failover_routes_never_yield_a_utility_id() {
    let coding = models::builtin();
    let utility = UtilityModels::builtin();
    let utility_ids: Vec<&str> = utility
        .models()
        .iter()
        .flat_map(|u| u.route_ids())
        .collect();
    assert!(!utility_ids.is_empty());
    for m in coding.models() {
        let mut starts: Vec<&str> = vec![m.id.as_str()];
        starts.extend(m.alternate_routes.iter().map(|a| a.id.as_str()));
        for start in starts {
            for spec in coding.failover_routes(start) {
                assert!(
                    !utility_ids.contains(&spec.id.as_str()),
                    "the failover list of {start} holds the utility route {}",
                    spec.id
                );
            }
        }
    }
}

#[test]
fn resolve_off_and_unknown_with_the_built_in_file() {
    let catalog = UtilityModels::builtin();
    assert_eq!(catalog.resolve(Some("off")), Ok(UtilityChoice::Disabled));
    assert_eq!(catalog.resolve(Some("OFF")), Ok(UtilityChoice::Disabled));
    for unset in [None, Some(""), Some("   ")] {
        assert_eq!(
            catalog.resolve(unset),
            Ok(UtilityChoice::Model(catalog.default_model())),
            "{unset:?}"
        );
    }
    for named in ["HAIKU", "haiku", " haiku ", "us.anthropic.claude-haiku-5-5"] {
        assert_eq!(
            catalog.resolve(Some(named)),
            Ok(UtilityChoice::Model(catalog.default_model())),
            "{named:?}"
        );
    }
    for wrong in ["sonnet", "nope", "global.anthropic.claude-haiku-5-5"] {
        let e = catalog
            .resolve(Some(wrong))
            .expect_err("a setting that names no row never falls through");
        let text = e.to_string();
        assert!(text.contains("HAMH_UTILITY_MODEL"), "{text}");
        assert!(text.contains(wrong), "{text}");
        assert!(text.contains("haiku"), "the choices are listed: {text}");
    }
    let lambda = catalog
        .resolve_named(Some("nope"), "UTILITY_MODEL")
        .unwrap_err();
    assert!(lambda.to_string().contains("UTILITY_MODEL"), "{lambda}");
}
