//! The model list and the default model.
//! `engine/catalog/models.json` is the one list; `ham_core::models` reads it for
//! `ham`, control-api and the TUI. Precedence of the choice, strongest first: the `--model` flag, `HAMH_MODEL`, the config file's
//! `default_model`, the built-in default (Claude Sonnet 5.5). A value that is set and wrong is an error, never a fall-through.

#![allow(clippy::unwrap_used)]

use ham_core::models::{
    DEFAULT_MODEL_ID, ModelError, ModelPrefs, ModelSource, Models, ModelsError, builtin,
    render_models, resolve,
};
use serde_json::{Value, json};

const SONNET: &str = "global.anthropic.claude-sonnet-5-5";
const OPUS: &str = "us.anthropic.claude-opus-5-5";
const SOL: &str = "us.openai.gpt-6.1-sol";
const ASTRA: &str = "us.openai.gpt-6-astra";
const GLM: &str = "us.zai.glm-5.3";

fn doc(models: Value) -> String {
    json!({"schemaVersion": 1, "models": models}).to_string()
}

fn entry(id: &str, alias: Option<&str>, default: bool, smoked: bool) -> Value {
    let mut v = json!({
        "id": id, "name": format!("Name of {id}"), "vendor": "v", "baseModelId": "v.n", "protocol": "converse",
        "region": "us-east-1", "contextTokens": 200000, "maxOutputTokens": 8192
    });
    if smoked {
        v["smokeRecordId"] = json!("s6:x+K3:y");
        v["smokedAt"] = json!("2026-10-06T21:35:00Z");
    }
    if let Some(a) = alias {
        v["alias"] = json!(a);
    }
    if default {
        v["default"] = json!(true);
    }
    v
}

fn custom() -> Models {
    Models::parse(&doc(json!([
        entry("a.one", Some("one"), false, true),
        entry("a.two", Some("two"), true, true),
        entry("a.three", Some("three"), false, false),
    ])))
    .unwrap()
}

fn prefs(env: Option<&str>, config: Option<&str>) -> ModelPrefs {
    ModelPrefs {
        env: env.map(str::to_string),
        config: config.map(str::to_string),
    }
}

#[test]
fn the_built_in_default_is_claude_sonnet_5_5_in_the_constant_and_in_the_file() {
    assert_eq!(DEFAULT_MODEL_ID, SONNET);
    let m = builtin();
    let d = m.default_model().unwrap();
    assert_eq!(d.id, SONNET);
    assert_eq!(d.name, "Claude Sonnet 5.5");
    assert_eq!(d.alias.as_deref(), Some("sonnet"));
    assert!(d.selectable());
    assert_eq!(
        m.models().iter().filter(|x| x.default).count(),
        1,
        "exactly one row is the default"
    );
}

#[test]
fn the_five_aliases_name_the_owners_five_models() {
    let m = builtin();
    for (alias, id) in [
        ("opus", OPUS),
        ("sonnet", SONNET),
        ("sol", SOL),
        ("astra", ASTRA),
        ("glm", GLM),
    ] {
        assert_eq!(m.find(alias).unwrap().id, id, "{alias}");
        assert_eq!(m.find(id).unwrap().id, id, "the full id of {alias}");
    }
    // Claude Fable 5.2 is the sixth row; it is selectable with its live record (tests/b803_catalog_sharing.rs).
    assert_eq!(m.models().len(), 6);
}

#[test]
fn glm_is_5_3_on_its_converse_route_and_glm_5_is_not_listed() {
    let m = builtin();
    let g = m.find("glm").unwrap();
    assert_eq!(g.id, GLM);
    assert_eq!(g.base_model_id, "zai.glm-5.3");
    assert_eq!(g.name, "GLM 5.3");
    assert_eq!(g.protocol.as_str(), "converse");
    for old in ["zai.glm-5", "glm-5", "glm5", "us.zai.glm-5", "glm-5.3"] {
        assert!(m.find(old).is_none(), "{old} must not resolve");
        assert!(resolve(m, Some(old), &ModelPrefs::default()).is_err());
    }
    assert!(m.models().iter().all(|x| x.id != "zai.glm-5"));
}

#[test]
fn nothing_set_gives_the_default_with_its_source() {
    let r = resolve(builtin(), None, &ModelPrefs::default()).unwrap();
    assert_eq!(
        (r.model.id.as_str(), r.source),
        (SONNET, ModelSource::BuiltIn)
    );
    assert_eq!(r.source.label(), "built-in default");
}

#[test]
fn precedence_is_flag_then_env_then_config_then_built_in() {
    let m = builtin();
    // (flag, HAMH_MODEL, config default_model, the id chosen, the layer it came from)
    type Case<'a> = (
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        &'a str,
        ModelSource,
    );
    let cases: [Case; 8] = [
        (
            Some("opus"),
            Some("sol"),
            Some("astra"),
            OPUS,
            ModelSource::Flag,
        ),
        (None, Some("sol"), Some("astra"), SOL, ModelSource::Env),
        (None, None, Some("astra"), ASTRA, ModelSource::Config),
        (Some(SOL), None, Some("opus"), SOL, ModelSource::Flag),
        (
            Some("sonnet"),
            Some("opus"),
            None,
            SONNET,
            ModelSource::Flag,
        ),
        (None, Some(OPUS), None, OPUS, ModelSource::Env),
        // An empty or blank value is unset, so the next layer answers.
        (None, Some(""), Some("opus"), OPUS, ModelSource::Config),
        (None, Some("  "), None, SONNET, ModelSource::BuiltIn),
    ];
    for (flag, env, config, want, source) in cases {
        let r = resolve(m, flag, &prefs(env, config)).unwrap();
        assert_eq!(
            (r.model.id.as_str(), r.source),
            (want, source),
            "flag {flag:?} env {env:?} config {config:?}"
        );
    }
    assert_eq!(ModelSource::Flag.label(), "--model");
    assert_eq!(ModelSource::Env.label(), "HAMH_MODEL");
    assert_eq!(ModelSource::Config.label(), "config default_model");
}

#[test]
fn a_value_that_is_set_and_wrong_is_an_error_even_when_a_weaker_layer_is_fine() {
    let m = builtin();
    let e = resolve(m, Some("nope"), &prefs(Some("opus"), Some("sol"))).unwrap_err();
    assert!(
        matches!(&e, ModelError::Unknown { from, .. } if *from == ModelSource::Flag),
        "{e:?}"
    );
    let e = resolve(m, None, &prefs(Some("nope"), Some("sol"))).unwrap_err();
    assert!(
        matches!(&e, ModelError::Unknown { from, .. } if *from == ModelSource::Env),
        "{e:?}"
    );
    let e = resolve(m, None, &prefs(None, Some("nope"))).unwrap_err();
    assert!(
        matches!(&e, ModelError::Unknown { from, .. } if *from == ModelSource::Config),
        "{e:?}"
    );
    // An explicit empty flag is not "unset".
    for flag in ["", "   "] {
        let e = resolve(m, Some(flag), &prefs(Some("opus"), None)).unwrap_err();
        assert!(matches!(e, ModelError::Empty { .. }), "{flag:?}");
        assert!(e.to_string().contains("--model"), "{e}");
    }
}

#[test]
fn an_unknown_model_message_names_the_source_and_lists_every_selectable_model_with_its_alias() {
    let e = resolve(builtin(), None, &prefs(Some("gpt-9"), None)).unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains("gpt-9") && text.contains("HAMH_MODEL"),
        "{text}"
    );
    for want in [OPUS, SONNET, SOL, ASTRA, "opus", "sonnet", "sol", "astra"] {
        assert!(text.contains(want), "{want} missing: {text}");
    }
    assert!(
        !text.contains("zai.glm-5\n") && !text.contains("zai.glm-5,"),
        "{text}"
    );
}

#[test]
fn aliases_ignore_case_and_surrounding_space_but_ids_are_exact() {
    let m = builtin();
    for alias in ["Sonnet", "SONNET", " sonnet ", "sonnet\n"] {
        assert_eq!(
            resolve(m, Some(alias), &ModelPrefs::default())
                .unwrap()
                .model
                .id,
            SONNET,
            "{alias:?}"
        );
    }
    for id in [
        "GLOBAL.ANTHROPIC.CLAUDE-SONNET-5-5",
        "anthropic.claude-sonnet-5-5",
        "us.anthropic.claude-sonnet-5-5",
        "global.anthropic.claude-opus-5-5",
        "sonnet5",
    ] {
        assert!(
            resolve(m, Some(id), &ModelPrefs::default()).is_err(),
            "{id}"
        );
    }
}

#[test]
fn a_listed_model_without_a_smoke_record_is_not_selectable_and_says_so() {
    let m = custom();
    assert_eq!(m.default_model().unwrap().id, "a.two");
    let e = resolve(&m, Some("three"), &ModelPrefs::default()).unwrap_err();
    assert!(matches!(e, ModelError::NotSmoked { .. }), "{e:?}");
    assert!(e.to_string().contains("smoke"), "{e}");
    // The unsmoked model is absent from the "choose one of" list.
    let e = resolve(&m, Some("zzz"), &ModelPrefs::default()).unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains("a.one") && text.contains("a.two") && !text.contains("a.three"),
        "{text}"
    );
    let r = resolve(&m, None, &ModelPrefs::default()).unwrap();
    assert_eq!(
        r.model.id, "a.two",
        "the file's default row is the built-in default of that list"
    );
}

#[test]
fn the_parser_refuses_a_default_or_alias_that_would_misroute() {
    let bad = |rows: Vec<Value>| Models::parse(&doc(Value::Array(rows))).unwrap_err();
    // Two defaults.
    let e = bad(vec![
        entry("a.one", Some("one"), true, true),
        entry("a.two", Some("two"), true, true),
    ]);
    assert!(
        matches!(&e, ModelsError::Invalid(m) if m.contains("default")),
        "{e:?}"
    );
    // A default with no smoke record cannot be selected.
    let e = bad(vec![entry("a.one", Some("one"), true, false)]);
    assert!(
        matches!(&e, ModelsError::Invalid(m) if m.contains("default") && m.contains("smoke")),
        "{e:?}"
    );
    // A default needs an alias too (the TUI and `ham models` show it).
    let e = bad(vec![entry("a.one", None, true, true)]);
    assert!(
        matches!(&e, ModelsError::Invalid(m) if m.contains("alias")),
        "{e:?}"
    );
    // Duplicate alias, alias equal to another row's id, and malformed aliases.
    let e = bad(vec![
        entry("a.one", Some("x"), false, true),
        entry("a.two", Some("x"), false, true),
    ]);
    assert!(
        matches!(&e, ModelsError::Invalid(m) if m.contains('x')),
        "{e:?}"
    );
    let e = bad(vec![
        entry("a.one", Some("a.two"), false, true),
        entry("a.two", Some("two"), false, true),
    ]);
    assert!(
        matches!(&e, ModelsError::Invalid(m) if m.contains("a.two")),
        "{e:?}"
    );
    for alias in [
        "",
        "Has Space",
        "UPPER",
        "a.b",
        "-x",
        "x_y",
        "way-too-long-alias-way-too-long",
    ] {
        let e = bad(vec![entry("a.one", Some(alias), false, true)]);
        assert!(
            matches!(&e, ModelsError::Invalid(m) if m.contains("alias")),
            "{alias:?}: {e:?}"
        );
    }
    // A list with no default row parses (control-api tests build such lists); `default_model` says none.
    let m = Models::parse(&doc(json!([entry("a.one", Some("one"), false, true)]))).unwrap();
    assert!(m.default_model().is_none());
    assert!(
        resolve(&m, None, &ModelPrefs::default()).is_err(),
        "no default row: nothing to fall back to"
    );
}

#[test]
fn the_list_view_marks_the_default_and_which_rows_can_be_chosen() {
    let list = custom().list();
    assert_eq!(list.default, "a.two");
    let rows: Vec<(&str, bool, bool)> = list
        .models
        .iter()
        .map(|r| (r.id.as_str(), r.default, r.selectable))
        .collect();
    assert_eq!(
        rows,
        [
            ("a.one", false, true),
            ("a.two", true, true),
            ("a.three", false, false)
        ]
    );
    let v = serde_json::to_value(&list).unwrap();
    assert_eq!(v["default"], "a.two");
    assert_eq!(v["models"][1]["default"], true);
    assert_eq!(v["models"][0]["contextTokens"], 200_000);
    assert_eq!(v["models"][2]["selectable"], false);
    assert_eq!(v["models"][0]["alias"], "one");
    let back: ham_core::models::ModelList = serde_json::from_value(v).unwrap();
    assert_eq!(back, list);
}

#[test]
fn the_text_listing_marks_the_default_and_the_model_the_next_run_uses() {
    let list = builtin().list();
    let text = render_models(&list, Some((OPUS, ModelSource::Env)));
    let line = |id: &str| {
        text.lines()
            .find(|l| l.contains(id))
            .unwrap_or_default()
            .to_string()
    };
    assert!(line(SONNET).contains("default"), "{text}");
    assert!(!line(OPUS).contains("default"), "{text}");
    assert!(
        line(OPUS).contains("next run") && line(OPUS).contains("HAMH_MODEL"),
        "{text}"
    );
    assert!(line(OPUS).trim_start().starts_with('*'), "{text}");
    for alias in ["opus", "sonnet", "sol", "astra", "glm"] {
        assert!(text.contains(alias), "{alias}: {text}");
    }
    assert!(
        text.contains("Claude Sonnet 5.5") && text.contains("1000000"),
        "{text}"
    );
    assert!(
        text.contains("--model")
            && text.contains("HAMH_MODEL")
            && text.contains("ham config set default_model"),
        "the listing says how to change the choice: {text}"
    );
    // Nothing selected explicitly: the default is the model the next run uses.
    let plain = render_models(&list, Some((SONNET, ModelSource::BuiltIn)));
    let sonnet_line = plain.lines().find(|l| l.contains(SONNET)).unwrap();
    assert!(
        sonnet_line.contains("next run") && sonnet_line.contains("built-in default"),
        "{plain}"
    );
    // An unselectable row is shown as pending, never offered.
    let pending = render_models(&custom().list(), None);
    let three = pending.lines().find(|l| l.contains("a.three")).unwrap();
    assert!(
        three.contains("pending") && three.contains("smoke"),
        "{pending}"
    );
}

/// GLM 5.3 is slow at high effort and can end without an answer at xhigh, so the list says so: selectable, not recommended for a first run.
#[test]
fn only_glm_5_3_carries_a_note_and_the_listing_prints_it_under_the_table() {
    let list = builtin().list();
    for row in &list.models {
        assert_eq!(
            row.note.is_some(),
            row.alias.as_deref() == Some("glm"),
            "{}: only the GLM row has a note",
            row.id
        );
    }
    let glm = list.models.iter().find(|m| m.id == GLM).unwrap();
    let note = glm.note.as_deref().unwrap();
    assert!(
        note.contains("high effort by default")
            && note.contains("xhigh")
            && note.contains("without an answer")
            && note.contains("reasoning.effort to low")
            && note.contains("not recommended for a first run"),
        "{note}"
    );
    assert!(!note.contains('\n'), "one line: {note}");
    assert!(
        glm.selectable,
        "the note does not change what can be picked"
    );

    let text = render_models(&list, None);
    let note_line = text
        .lines()
        .find(|l| l.contains(note))
        .unwrap_or_else(|| panic!("the note is printed: {text}"));
    assert!(
        note_line.trim_start().starts_with("glm"),
        "the note line starts with the alias: {note_line}"
    );
    let table_end = text.find("\nPick one run's model").unwrap();
    assert!(
        text.find(note).unwrap() < table_end,
        "the note comes before the how-to-change lines: {text}"
    );
    // A list without a note prints none, and the JSON of a row without one has no member.
    let plain = render_models(&custom().list(), None);
    assert!(!plain.contains("Notes"), "{plain}");
    let v = serde_json::to_value(custom().list()).unwrap();
    assert!(v["models"][0].get("note").is_none(), "{v}");
    let v = serde_json::to_value(&list).unwrap();
    let glm_json = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == GLM)
        .unwrap();
    assert_eq!(glm_json["note"], note);
}

#[test]
fn a_wrong_value_says_how_to_fix_it_for_its_layer() {
    let m = builtin();
    let env = resolve(m, None, &prefs(Some("nope"), None))
        .unwrap_err()
        .to_string();
    assert!(env.contains("unset HAMH_MODEL"), "{env}");
    let cfg = resolve(m, None, &prefs(None, Some("nope")))
        .unwrap_err()
        .to_string();
    assert!(
        cfg.contains("ham config set default_model")
            && cfg.contains("ham config unset default_model"),
        "{cfg}"
    );
    let flag = resolve(m, Some("nope"), &ModelPrefs::default())
        .unwrap_err()
        .to_string();
    assert!(
        !flag.contains("unset") && flag.contains("from --model"),
        "{flag}"
    );
    let given = ham_core::models::resolve_given(m, "nope")
        .unwrap_err()
        .to_string();
    assert!(
        !given.contains("(from") && given.contains("choose one of"),
        "{given}"
    );
    assert_eq!(
        ham_core::models::resolve_given(m, " Opus ")
            .unwrap()
            .model
            .id,
        "us.anthropic.claude-opus-5-5"
    );
}
