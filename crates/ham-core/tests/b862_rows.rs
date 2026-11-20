//! The rows of GPT-6.2 Sol and GPT-6.2 Astra: `engine/catalog/models.json` lists both models for Cline and for OpenCode, each row on a
//! committed live record of its engine. `sol` and `astra` are the names of the two new models; GPT-6.1 Sol and GPT-6 Astra keep their rows, their
//! records and their full ids and answer to `sol-6.1` and `astra-6`. A row has one name. The default model stays Claude Sonnet 5.5.
#![allow(clippy::unwrap_used)]

use ham_core::models::{
    self, DEFAULT_MODEL_ID, Model, ModelPrefs, ModelSource, Models, render_models, resolve,
    resolve_given,
};
use harness_protocol::common::Engine;
use serde_json::{Value, json};
use std::path::PathBuf;

const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const SOL_61: &str = "us.openai.gpt-6.1-sol";
const ASTRA_6: &str = "us.openai.gpt-6-astra";
const GLM: &str = "us.zai.glm-5.3";
const FABLE: &str = "global.anthropic.claude-fable-5-2";
const SOL_62: &str = "us.openai.gpt-6.2-sol";
const ASTRA_62: &str = "us.openai.gpt-6.2-astra";

/// (the short name, the model id) of the four OpenAI rows, the same in the list of each engine.
const NAMES: [(&str, &str); 4] = [
    ("sol", SOL_62),
    ("astra", ASTRA_62),
    ("sol-6.1", SOL_61),
    ("astra-6", ASTRA_6),
];

const SOL_61_NOTE: &str =
    "An earlier Sol model. The name sol now selects GPT-6.2 Sol; sol-6.1 selects this model.";
const ASTRA_6_NOTE: &str =
    "An earlier Astra model. The name astra now selects GPT-6.2 Astra; astra-6 selects this model.";

fn catalog_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/catalog")
}

fn read(file: &str) -> Value {
    let text =
        std::fs::read_to_string(catalog_dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"))
}

fn cline() -> &'static Models {
    models::builtin()
}

fn opencode() -> &'static Models {
    models::builtin_catalog()
        .for_engine(Engine::OPENCODE)
        .expect("the catalog lists models for OpenCode")
}

fn row<'a>(list: &'a Models, id: &str) -> &'a Model {
    list.models()
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("{id} has no row in the list of {:?}", list.engine()))
}

fn prefs(env: Option<&str>, config: Option<&str>) -> ModelPrefs {
    ModelPrefs {
        env: env.map(str::to_string),
        config: config.map(str::to_string),
    }
}

#[test]
fn b862_sol_and_astra_name_the_6_2_models_and_the_pinned_names_the_old_ones() {
    for list in [cline(), opencode()] {
        for (name, id) in NAMES {
            let m = list
                .find(name)
                .unwrap_or_else(|| panic!("{name} names no model of {:?}", list.engine()));
            assert_eq!(m.id, id, "{name} in the list of {:?}", list.engine());
            assert_eq!(m.alias.as_deref(), Some(name), "a row has one name: {id}");
            assert_eq!(list.find(id).unwrap().id, id, "the full id of {name}");
            assert!(m.selectable(), "{id}");
        }
        // A row has one name: no second name was built for the new rows, and no near miss resolves.
        for no_name in ["sol-6.2", "astra-6.2", "sol-6", "astra-6.1", "gpt-6.2-sol"] {
            assert!(
                list.find(no_name).is_none(),
                "{no_name} must name no model of {:?}",
                list.engine()
            );
        }
    }
    // The default model does not move.
    assert_eq!(DEFAULT_MODEL_ID, SONNET);
    for list in [cline(), opencode()] {
        let d = list.default_model().unwrap();
        assert_eq!(
            (d.id.as_str(), d.name.as_str()),
            (SONNET, "Claude Sonnet 5.5")
        );
        assert_eq!(list.models().iter().filter(|m| m.default).count(), 1);
    }
    let r = resolve(cline(), None, &ModelPrefs::default()).unwrap();
    assert_eq!(
        (r.model.id.as_str(), r.source),
        (SONNET, ModelSource::BuiltIn)
    );
}

#[test]
fn b862_a_saved_default_with_a_full_id_keeps_its_model_and_a_typed_name_follows_the_list() {
    // `ham config set default_model` stores the full id, so a default saved before the names moved still names the model it named.
    for id in [SOL_61, ASTRA_6] {
        let r = resolve(cline(), None, &prefs(None, Some(id))).unwrap();
        assert_eq!((r.model.id.as_str(), r.source), (id, ModelSource::Config));
        assert_eq!(resolve_given(cline(), id).unwrap().model.id, id);
    }
    // A short name is read from the list of the client that reads it, in every layer.
    // (flag, HAMH_MODEL, config default_model, the id chosen, the layer it came from)
    type Case<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        &'a str,
        ModelSource,
    );
    let cases: [Case; 6] = [
        (Some("sol"), None, None, SOL_62, ModelSource::Flag),
        (
            Some("sol-6.1"),
            Some("astra"),
            None,
            SOL_61,
            ModelSource::Flag,
        ),
        (None, Some("astra"), Some("sol"), ASTRA_62, ModelSource::Env),
        (None, Some("ASTRA-6"), None, ASTRA_6, ModelSource::Env),
        (None, None, Some("sol"), SOL_62, ModelSource::Config),
        (None, None, Some("astra-6"), ASTRA_6, ModelSource::Config),
    ];
    for (flag, env, config, id, source) in cases {
        let r = resolve(cline(), flag, &prefs(env, config)).unwrap();
        assert_eq!(
            (r.model.id.as_str(), r.source),
            (id, source),
            "{flag:?} {env:?} {config:?}"
        );
    }
    // What `ham config set default_model <name>` stores for each name is the full id of the row.
    for (name, id) in NAMES {
        assert_eq!(resolve_given(cline(), name).unwrap().model.id, id, "{name}");
    }
}

#[test]
fn b862_the_new_rows_stand_on_their_record_files() {
    let table = read("reasoning-levels.json");
    for (id, name, base, alias) in [
        (SOL_62, "GPT-6.2 Sol", "openai.gpt-6.2-sol", "sol"),
        (ASTRA_62, "GPT-6.2 Astra", "openai.gpt-6.2-astra", "astra"),
    ] {
        let m = row(cline(), id);
        let file = format!("smoke-{id}.json");
        let rec = read(&file);
        let global = format!("global.{base}");
        let date = rec["date"].as_str().unwrap();
        assert_eq!(
            (m.name.as_str(), m.vendor.as_str(), m.base_model_id.as_str()),
            (name, "openai", base)
        );
        assert_eq!(m.engine, Engine::DEFAULT);
        assert_eq!(rec["model"], name);
        assert_eq!(rec["bedrockBaseModelId"], base);
        assert_eq!(rec["profileId"], id);
        assert_eq!(rec["alsoVerified"], json!([global]));
        assert_eq!(rec["protocol"], m.protocol.as_str());
        assert_eq!(rec["regions"][m.region.as_str()], "pass");
        // Both routes passed, and the row and its alternate route carry the record's ids and times.
        let routes = &rec["results"]["routes"];
        assert_eq!(routes[id]["result"], "pass");
        assert_eq!(routes[global.as_str()]["result"], "pass");
        assert_eq!(
            m.smoke_record_id.as_deref(),
            Some(format!("s6:{id}:us-east-1:converse+audit:{date}/{file}").as_str())
        );
        assert_eq!(m.smoked_at.as_deref(), routes[id]["smokedAt"].as_str());
        assert_eq!(m.alternate_routes.len(), 1, "{id}: one other profile");
        let alt = &m.alternate_routes[0];
        assert_eq!(alt.id, global);
        assert_eq!(alt.region, m.region);
        assert_eq!(
            alt.smoke_record_id,
            format!("s6:{global}:us-east-1:converse+audit:{date}/{file}")
        );
        assert_eq!(
            alt.smoked_at.as_deref(),
            routes[global.as_str()]["smokedAt"].as_str()
        );
        // The output limit is the measured one. The window is at or below the largest input Bedrock accepted, so the engine, which shortens a
        // conversation at nine tenths of the window, does so before Bedrock refuses the input.
        assert_eq!(rec["limits"]["maxOutputTokens"], m.max_output_tokens);
        assert_eq!(m.max_output_tokens, 128_000);
        let measured = rec["limits"]["contextTokensMeasured"].as_u64().unwrap();
        let refused = rec["limits"]["inputRefusedFromTokens"].as_u64().unwrap();
        assert_eq!(m.context_tokens, 920_000);
        assert!(u64::from(m.context_tokens) <= measured && measured < refused);
        assert!(!m.requires_provider_sharing, "{id} needs no sharing");
        assert!(!m.default && m.note.is_none(), "{id}");
        // The reasoning object names the table, and the table's cells of this route carry the row's name.
        let r = m.reasoning.as_ref().unwrap();
        assert_eq!(
            r.smoke_record_id,
            format!("b649:reasoning-levels:{date}:{id}:converse+launch")
        );
        assert_eq!(r.default, "xhigh");
        assert_eq!(r.accepted, ["low", "medium", "high", "xhigh"]);
        assert_eq!(
            table["recordedAtByModel"][alias].as_str(),
            r.smoked_at.as_deref(),
            "the table's time of model {alias}"
        );
        // The OpenCode row stands on the engine smoke record of the same id.
        let oc = row(opencode(), id);
        let orec = read(&format!("smoke-opencode-{id}.json"));
        assert_eq!(orec["result"], "pass");
        assert_eq!(orec["engine"], "opencode");
        assert_eq!(orec["model"]["id"], id);
        assert_eq!(oc.engine, Engine::OPENCODE);
        assert_eq!(
            oc.smoke_record_id.as_deref(),
            orec["smokeRecordId"].as_str()
        );
        assert_eq!(oc.smoked_at.as_deref(), orec["smokedAt"].as_str());
        assert_eq!(Some(oc.region.as_str()), orec["route"]["region"].as_str());
        assert_eq!(
            (
                oc.name.as_str(),
                oc.vendor.as_str(),
                oc.base_model_id.as_str()
            ),
            (name, "openai", base)
        );
        assert_eq!(oc.alias.as_deref(), Some(alias));
        // OpenCode takes the window and the output limit apart; 131,072 as the output limit makes every turn fail.
        assert_eq!(
            (oc.context_tokens, oc.max_output_tokens),
            (1_050_000, 128_000)
        );
        assert!(oc.reasoning.is_none() && oc.alternate_routes.is_empty());
        assert!(oc.note.is_none() && !oc.default && !oc.requires_provider_sharing);
    }
}

#[test]
fn b862_the_cells_of_the_reasoning_table_carry_the_name_of_their_row() {
    // The table labels a cell with the short name of its model. The names moved, so the labels moved with them: a later run of the
    // probe, which reads the name from the list, writes cells that sit beside the committed ones.
    let table = read("reasoning-levels.json");
    let label_of = |route: &str| -> &'static str {
        match route {
            r if r.ends_with("openai.gpt-6.2-sol") => "sol",
            r if r.ends_with("openai.gpt-6.2-astra") => "astra",
            r if r.ends_with("openai.gpt-6.1-sol") => "sol-6.1",
            r if r.ends_with("openai.gpt-6-astra") => "astra-6",
            _ => "",
        }
    };
    let mut seen = 0;
    for part in ["cells", "hardRuns"] {
        for c in table[part].as_array().unwrap() {
            let route = c["route"].as_str().unwrap();
            let want = label_of(route);
            if want.is_empty() {
                continue;
            }
            assert_eq!(c["model"], want, "{part}: the label of {route}");
            seen += 1;
        }
    }
    assert!(
        seen >= 4 * 18,
        "the four OpenAI models have their cells: {seen}"
    );
    for r in table["earlierRuns"]["rows"].as_array().unwrap() {
        let label = r["model"].as_str().unwrap();
        assert!(
            cline().find(label).is_some(),
            "{label} of the earlier runs names a row"
        );
        assert!(
            label != "sol" && label != "astra",
            "the earlier runs were of the earlier models"
        );
    }
    for key in ["recordedAtByModel", "engineByModel", "idleLimitSByModel"] {
        for label in table[key].as_object().unwrap().keys() {
            assert!(cline().find(label).is_some(), "{key}: {label} names no row");
        }
        assert!(
            table[key]["sol"].is_string() || table[key]["sol"].is_number(),
            "{key}"
        );
        assert!(
            table[key]["astra"].is_string() || table[key]["astra"].is_number(),
            "{key}"
        );
    }
    let about = table["about"].as_str().unwrap();
    assert!(
        !about.contains("sol-6.2") && !about.contains("astra-6.2"),
        "{about}"
    );
}

#[test]
fn b862_the_new_rows_are_appended_and_the_old_rows_carry_the_note() {
    // File order is failover order, so a new model is appended: the lists of the models before it stay what they were.
    let ids = |l: &Models| -> Vec<String> { l.models().iter().map(|m| m.id.clone()).collect() };
    assert_eq!(
        ids(cline()),
        [OPUS, SONNET, SOL_61, ASTRA_6, GLM, FABLE, SOL_62, ASTRA_62]
    );
    assert_eq!(
        ids(opencode()),
        [OPUS, SONNET, SOL_61, ASTRA_6, FABLE, SOL_62, ASTRA_62]
    );
    // The move of the two names is said where `ham models` already says what to know about a model: the note of the two earlier rows.
    assert_eq!(row(cline(), SOL_61).note.as_deref(), Some(SOL_61_NOTE));
    assert_eq!(row(cline(), ASTRA_6).note.as_deref(), Some(ASTRA_6_NOTE));
    for id in [OPUS, SONNET, FABLE, SOL_62, ASTRA_62] {
        assert!(row(cline(), id).note.is_none(), "{id} carries no note");
    }
    assert!(row(cline(), GLM).note.is_some());
    assert!(opencode().models().iter().all(|m| m.note.is_none()));
    // The listing prints both notes under the table, each after the name of its row, and keeps the old ids in the table.
    let text = render_models(&cline().list(), None);
    assert!(
        text.contains(&format!("  sol-6.1 {SOL_61_NOTE}\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!("  astra-6 {ASTRA_6_NOTE}\n")),
        "{text}"
    );
    for (name, id) in NAMES {
        assert!(
            text.lines()
                .any(|l| l.split_whitespace().take(2).eq([name, id])),
            "no line `{name} {id}` in\n{text}"
        );
    }
    // No date and no release number in a note: the text is shipped and stays true.
    for note in [SOL_61_NOTE, ASTRA_6_NOTE] {
        assert!(!note.contains("2026") && !note.contains("5.24"), "{note}");
    }
}

#[test]
fn b862_the_gate_model_stays_selectable_by_its_full_id() {
    // The release gate runs on GPT-6.1 Sol by its full id; the row, its records and both engines' pairs stay.
    let catalog = models::builtin_catalog();
    for id in [SOL_61, ASTRA_6] {
        assert_eq!(cline().check(id).unwrap().id, id);
        assert_eq!(catalog.check(Engine::DEFAULT, id).unwrap().id, id);
        assert_eq!(catalog.check(Engine::OPENCODE, id).unwrap().id, id);
    }
    let sol = row(cline(), SOL_61);
    assert_eq!(sol.name, "GPT-6.1 Sol");
    assert!(sol.smoke_record_id.as_deref().unwrap().contains("+K3:"));
    assert_eq!(sol.alternate_routes[0].id, "global.openai.gpt-6.1-sol");
    assert_eq!(sol.reasoning_default(), Some("xhigh"));
    // The new models are selectable on both engines too.
    for id in [SOL_62, ASTRA_62] {
        assert_eq!(catalog.check(Engine::DEFAULT, id).unwrap().id, id);
        assert_eq!(catalog.check(Engine::OPENCODE, id).unwrap().id, id);
    }
}

#[test]
fn b862_the_list_a_stage_sends_carries_one_name_a_row_and_no_new_member() {
    // No new protocol line: the wire row has the members it had, and its one name is the row's name.
    let list = models::builtin_catalog().list();
    let v = serde_json::to_value(&list).unwrap();
    let rows = v["models"].as_array().unwrap();
    assert_eq!(rows.len(), 15, "eight Cline rows and seven OpenCode rows");
    for r in rows {
        for gone in ["otherAliases", "family", "aliases"] {
            assert!(r.get(gone).is_none(), "{gone} on a wire row");
        }
    }
    let name_of = |id: &str, engine: Option<&str>| -> String {
        rows.iter()
            .find(|r| r["id"] == id && r.get("engine").and_then(Value::as_str) == engine)
            .unwrap_or_else(|| panic!("{id} {engine:?}"))["alias"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for (name, id) in NAMES {
        assert_eq!(name_of(id, None), name);
        assert_eq!(name_of(id, Some("opencode")), name);
    }
    assert_eq!(v["default"], SONNET);
}
