//! `check_engine_config`, the key table and the denied classes.
//!
//! One pure function is the check that `ham`, control-api and agentd all call. These tests pin its order, its codes, its limits, what it
//! never echoes, and the table it reads. The list of `reasoning.effort` levels is ONE constant of the crate (`EFFORT_LEVELS` in
//! `src/direct_engine_config.rs`); the probe package decides it, and when it changes, `the_key_table_hash_is_pinned` is the one test
//! whose golden is edited with it.

#![allow(clippy::unwrap_used)]

use harness_protocol::direct::{
    CLINE_KEYS, DENIED, EngineConfig, EngineConfigProblem, KeyKind, KeyRow,
    MAX_ENGINE_CONFIG_BYTES, MAX_ENGINE_CONFIG_LEAVES, check_engine_config, denied_table_sha256,
    key_table_sha256, keys_for, table_problems,
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

fn cfg(settings: Value) -> EngineConfig {
    EngineConfig {
        engine: "cline".into(),
        schema: 1,
        settings: settings.as_object().unwrap().clone(),
    }
}

fn check(settings: Value) -> Result<(), EngineConfigProblem> {
    check_engine_config(&cfg(settings))
}

fn code(settings: Value) -> &'static str {
    match check(settings.clone()) {
        Err(p) => p.code(),
        Ok(()) => panic!("accepted: {settings}"),
    }
}

fn nested(path: &str, leaf: Value) -> Value {
    let mut out = leaf;
    for part in path.rsplit('.') {
        let mut m = Map::new();
        m.insert(part.to_string(), out);
        out = Value::Object(m);
    }
    out
}

fn row(path: &str) -> &'static KeyRow {
    CLINE_KEYS.iter().find(|r| r.path == path).unwrap()
}

// ---- the table -------------------------------------------------------------------------------------------------------------

#[test]
fn the_cline_table_has_the_five_keys_of_the_design_and_other_engines_have_none() {
    let paths: Vec<&str> = CLINE_KEYS.iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "reasoning.effort",
            "commands.timeout_seconds",
            "context.compaction",
            "context.compaction_trigger",
            "tools.disable",
        ]
    );
    assert_eq!(
        row("commands.timeout_seconds").kind,
        KeyKind::Int { min: 1, max: 1800 }
    );
    assert_eq!(row("commands.timeout_seconds").default, "600");
    assert_eq!(
        row("context.compaction_trigger").kind,
        KeyKind::Float {
            min: 0.10,
            max: 0.99
        }
    );
    assert_eq!(row("context.compaction_trigger").default, "0.9");
    assert_eq!(
        row("context.compaction").kind,
        KeyKind::Choice(&["agentic", "basic", "off"])
    );
    assert_eq!(row("context.compaction").default, "agentic");
    assert_eq!(
        row("tools.disable").kind,
        KeyKind::Set(&["spawn_agent", "fetch_web_content"])
    );
    // The effort levels are the probe package's to decide; the table always offers `default` and never `none` or `max`.
    let KeyKind::Choice(levels) = row("reasoning.effort").kind else {
        panic!("reasoning.effort is a choice");
    };
    assert!(levels.contains(&"default"));
    assert!(!levels.contains(&"none") && !levels.contains(&"max"));
    assert_eq!(row("reasoning.effort").default, "default");

    assert_eq!(keys_for("cline").unwrap().len(), 5);
    for other in ["pi", "kiro", "codex", "", "Cline", "cline "] {
        assert!(keys_for(other).is_none(), "{other:?}");
    }
}

#[test]
fn the_shipped_table_has_no_problems_and_a_mutated_table_has() {
    assert_eq!(table_problems(CLINE_KEYS), Vec::<String>::new());

    let base = row("commands.timeout_seconds");
    // a path twice
    let twice = [*base, *base];
    assert!(!table_problems(&twice).is_empty());
    // a path that is also the start of another path (a key cannot be a value and a table)
    let leaf = KeyRow {
        path: "reasoning",
        ..*row("tools.disable")
    };
    assert!(!table_problems(&[leaf, *row("reasoning.effort")]).is_empty());
    // a default the key itself would refuse
    let bad_default = KeyRow {
        default: "5000",
        ..*base
    };
    assert!(!table_problems(&[bad_default]).is_empty());
    // a row whose name the denied classes would catch: the allow-list is checked first, so this would hide a denial
    let hidden = KeyRow {
        path: "aws.profile",
        ..*row("tools.disable")
    };
    assert!(!table_problems(&[hidden]).is_empty());
}

#[test]
fn tools_in_the_table_have_an_empty_required_by_and_a_mutation_fails_it() {
    let tools = row("tools.disable");
    assert!(matches!(tools.kind, KeyKind::Set(_)));
    assert!(tools.required_by.is_empty(), "{:?}", tools.required_by);
    for r in CLINE_KEYS {
        assert!(r.required_by.is_empty(), "{}", r.path);
    }
    // Mutation proof: a tool that something in the harness depends on may not be user-disabled, and the table check says so.
    let depended_on = KeyRow {
        required_by: &["the goal plugin"],
        ..*tools
    };
    let problems = table_problems(&[depended_on]);
    assert!(
        problems.iter().any(|p| p.contains("tools.disable")),
        "{problems:?}"
    );
}

#[test]
fn the_key_table_hash_is_pinned() {
    // A change to any row (a level, a bound, a default) or to a denied class (its words) changes the worker image key and the
    // frozen contract. These goldens force that edit to be a conscious one.
    assert_eq!(
        key_table_sha256(CLINE_KEYS),
        "f9fdc476751b16521c34ba9150ec22420a235b4d370f02ba515af642050a2a9d"
    );
    assert_eq!(
        denied_table_sha256(),
        "87a3a9ee0e252e19ff62970d431541a8a6e80729b063b29d1e4520585cccbae0"
    );
}

#[test]
fn the_denied_table_has_the_seven_classes_with_one_short_reason_each() {
    assert_eq!(DENIED.len(), 7);
    let ids: Vec<&str> = DENIED.iter().map(|d| d.id).collect();
    assert_eq!(
        ids,
        [
            "credentials",
            "provider",
            "telemetry",
            "processes",
            "permissions",
            "network",
            "owned-elsewhere"
        ]
    );
    for d in DENIED {
        assert!(!d.label.is_empty() && !d.reason.is_empty() && !d.protects.is_empty());
        assert!(d.reason.is_ascii() && !d.reason.contains('"'), "{}", d.id);
        assert!(!d.reason.ends_with('.'), "one short phrase: {}", d.id);
    }
    assert_eq!(
        DENIED[0].reason,
        "the harness supplies credentials itself; this cannot be set"
    );
}

// ---- every key at its bounds -----------------------------------------------------------------------------------------------

#[test]
fn every_key_passes_at_min_and_max_and_fails_one_past_each() {
    // Generic over the table, so an edit of a range or a level list needs no edit here.
    for r in CLINE_KEYS {
        match r.kind {
            KeyKind::Int { min, max } => {
                for ok in [min, max, (min + max) / 2] {
                    check(nested(r.path, json!(ok))).unwrap();
                }
                for bad in [min - 1, max + 1] {
                    assert_eq!(
                        code(nested(r.path, json!(bad))),
                        "out_of_range",
                        "{}",
                        r.path
                    );
                }
            }
            KeyKind::Float { min, max } => {
                for ok in [min, max, (min + max) / 2.0] {
                    check(nested(r.path, json!(ok))).unwrap();
                }
                for bad in [min - 0.01, max + 0.01, min / 2.0, max * 2.0] {
                    assert_eq!(
                        code(nested(r.path, json!(bad))),
                        "out_of_range",
                        "{}",
                        r.path
                    );
                }
            }
            KeyKind::Choice(levels) => {
                for ok in levels {
                    check(nested(r.path, json!(ok))).unwrap();
                }
                for bad in ["none", "max", "xhigh", "", "HIGH", "High", " high", "high "] {
                    if !levels.contains(&bad) {
                        assert_eq!(
                            code(nested(r.path, json!(bad))),
                            "out_of_range",
                            "{} {bad:?}",
                            r.path
                        );
                    }
                }
            }
            KeyKind::Set(members) => {
                check(nested(r.path, json!([]))).unwrap();
                check(nested(r.path, json!(members))).unwrap();
                for m in members {
                    check(nested(r.path, json!([m]))).unwrap();
                }
                assert_eq!(code(nested(r.path, json!(["bash"]))), "out_of_range");
                assert_eq!(
                    code(nested(r.path, json!([members[0], members[0]]))),
                    "duplicate_value"
                );
            }
        }
    }
}

#[test]
fn each_key_refuses_the_wrong_json_type_with_bad_type() {
    for (path, wrong) in [
        (
            "reasoning.effort",
            vec![json!(1), json!(true), json!(null), json!([]), json!({})],
        ),
        (
            "commands.timeout_seconds",
            vec![
                json!("600"),
                json!(600.0),
                json!(600.5),
                json!(true),
                json!(null),
                json!([600]),
                json!({}),
                json!(u64::MAX),
                json!(i64::MAX as u64 + 1),
            ],
        ),
        (
            "context.compaction",
            vec![json!(0), json!(false), json!(null), json!(["off"])],
        ),
        (
            "context.compaction_trigger",
            vec![
                json!("0.9"),
                json!(true),
                json!(null),
                json!([0.9]),
                json!({}),
            ],
        ),
        (
            "tools.disable",
            vec![
                json!("spawn_agent"),
                json!(1),
                json!(true),
                json!(null),
                json!({}),
                json!([1]),
                json!([null]),
                json!([true]),
            ],
        ),
    ] {
        for v in wrong {
            assert_eq!(code(nested(path, v.clone())), "bad_type", "{path} {v}");
        }
    }
}

#[test]
fn the_integer_edges_are_exact() {
    let p = "commands.timeout_seconds";
    check(nested(p, json!(1))).unwrap();
    check(nested(p, json!(1800))).unwrap();
    for bad in [0, -1, 1801, i64::MIN, i64::MAX] {
        assert_eq!(code(nested(p, json!(bad))), "out_of_range", "{bad}");
    }
    // A JSON integer above i64::MAX is not a value of this key at all.
    assert_eq!(code(nested(p, json!(i64::MAX as u64 + 1))), "bad_type");
    assert_eq!(code(nested(p, json!(u64::MAX))), "bad_type");
}

#[test]
fn the_trigger_edges_are_exact_and_nan_and_infinity_are_not_in_range() {
    let p = "context.compaction_trigger";
    check(nested(p, json!(0.1))).unwrap();
    check(nested(p, json!(0.99))).unwrap();
    for bad in [
        json!(0.09),
        json!(0.0999999),
        json!(0.9900001),
        json!(1),
        json!(0),
        json!(-0.5),
        json!(600.0),
        json!(1e308),
    ] {
        assert_eq!(code(nested(p, bad.clone())), "out_of_range", "{bad}");
    }
    // A JSON value cannot hold NaN or infinity, and the range test is written so that they fail it all the same.
    let KeyKind::Float { .. } = row(p).kind else {
        panic!("a float key");
    };
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0] {
        assert!(!row(p).kind.admits_f64(x), "{x}");
    }
    assert!(row(p).kind.admits_f64(0.5));
    assert!(serde_json::from_str::<Value>("NaN").is_err());
}

#[test]
fn a_document_with_all_five_keys_passes_and_so_does_an_empty_settings_object() {
    check(json!({
        "reasoning": {"effort": "low"},
        "commands": {"timeout_seconds": 900},
        "context": {"compaction": "basic", "compaction_trigger": 0.8},
        "tools": {"disable": ["fetch_web_content", "spawn_agent"]}
    }))
    .unwrap();
    check(json!({})).unwrap();
    check(json!({"reasoning": {}})).unwrap();
}

// ---- the order of the checks -----------------------------------------------------------------------------------------------

#[test]
fn the_checks_run_in_the_documented_order() {
    let mut c = cfg(json!({"a": {"b": {"c": {"d": 1}}}}));
    // wrong engine first, whatever else is wrong
    c.engine = "pi".into();
    c.schema = 7;
    assert_eq!(check_engine_config(&c).unwrap_err().code(), "wrong_engine");
    // then the schema
    c.engine = "cline".into();
    assert_eq!(check_engine_config(&c).unwrap_err().code(), "bad_schema");
    for schema in [0, 2, u64::MAX] {
        c.schema = schema;
        assert_eq!(check_engine_config(&c).unwrap_err().code(), "bad_schema");
    }
    // then the size, the depth and the number of values, then the keys, one at a time
    c.schema = 1;
    assert_eq!(check_engine_config(&c).unwrap_err().code(), "too_deep");
    let mut big = Map::new();
    big.insert("zz".into(), json!("x".repeat(MAX_ENGINE_CONFIG_BYTES)));
    big.insert("a".into(), json!({"b": {"c": {"d": 1}}}));
    c.settings = big;
    assert_eq!(check_engine_config(&c).unwrap_err().code(), "too_large");
    // an unknown key is reported before a later key's bad value, in key order
    assert_eq!(
        code(json!({"commands": {"timeout_seconds": 0}, "aaa": 1})),
        "unknown_key"
    );
    assert_eq!(
        code(json!({"commands": {"timeout_seconds": 0}, "tools": {"disable": ["x"]}})),
        "out_of_range"
    );
}

#[test]
fn the_codes_are_the_eleven_stable_words() {
    let mut words: Vec<&str> = vec![];
    let mut c = cfg(json!({}));
    c.engine = "x".into();
    words.push(check_engine_config(&c).unwrap_err().code());
    c.engine = "cline".into();
    c.schema = 2;
    words.push(check_engine_config(&c).unwrap_err().code());
    words.push(code(json!({"zz": "x".repeat(MAX_ENGINE_CONFIG_BYTES)})));
    words.push(code(json!({"a": {"b": {"c": {"d": 1}}}})));
    words.push(code(
        json!({"tools": {"disable": (0..=MAX_ENGINE_CONFIG_LEAVES).map(|i| format!("a{i}")).collect::<Vec<_>>()}}),
    ));
    words.push(code(json!({"nope": 1})));
    words.push(code(json!({"proxy": 1})));
    words.push(code(json!({"reasoning": {"effort": 1}})));
    words.push(code(json!({"reasoning": {"effort": "max"}})));
    words.push(code(
        json!({"tools": {"disable": ["spawn_agent", "spawn_agent"]}}),
    ));
    // Every closed set of the v1 table refuses a long text as out of range before its length is looked at, so this word is the one of a key
    // that would take text; `src/direct_engine_config.rs` reaches it with a table of its own (unit test in that file).
    words.push(EngineConfigProblem::StringTooLong { key: None }.code());
    assert_eq!(
        words,
        [
            "wrong_engine",
            "bad_schema",
            "too_large",
            "too_deep",
            "too_many_keys",
            "unknown_key",
            "denied_key",
            "bad_type",
            "out_of_range",
            "duplicate_value",
            "string_too_long"
        ]
    );
}

// ---- the limits ------------------------------------------------------------------------------------------------------------

#[test]
fn the_size_limit_is_4096_bytes_of_the_serialized_member() {
    // `zz` is an unknown key, so a document that passes the size check ends with unknown_key and one that does not ends with too_large.
    let sized = |n: usize| {
        let c = cfg(json!({"zz": "x".repeat(n)}));
        (serde_json::to_vec(&c).unwrap().len(), c)
    };
    let (len, _) = sized(0);
    let (at, on) = sized(MAX_ENGINE_CONFIG_BYTES - len);
    assert_eq!(at, MAX_ENGINE_CONFIG_BYTES);
    assert_eq!(check_engine_config(&on).unwrap_err().code(), "unknown_key");
    let (over, off) = sized(MAX_ENGINE_CONFIG_BYTES - len + 1);
    assert_eq!(over, MAX_ENGINE_CONFIG_BYTES + 1);
    assert_eq!(check_engine_config(&off).unwrap_err().code(), "too_large");
}

#[test]
fn the_value_limit_is_64_leaves_and_the_depth_limit_is_3() {
    let list = |n: usize| json!({"tools": {"disable": (0..n).map(|i| format!("a{i}")).collect::<Vec<_>>()}});
    assert_eq!(code(list(MAX_ENGINE_CONFIG_LEAVES)), "out_of_range");
    assert_eq!(code(list(MAX_ENGINE_CONFIG_LEAVES + 1)), "too_many_keys");
    // the deepest legal document is settings, table, list
    check(json!({"tools": {"disable": ["spawn_agent"]}})).unwrap();
    assert_eq!(
        code(json!({"tools": {"disable": [["spawn_agent"]]}})),
        "too_deep"
    );
    assert_eq!(code(json!({"a": {"b": {"c": {"d": 1}}}})), "too_deep");
    // an empty container counts as one value
    assert_eq!(code(json!({"a": {"b": {"c": {}}}})), "too_deep");
}

// ---- denied classes --------------------------------------------------------------------------------------------------------

#[test]
fn each_denied_class_is_refused_with_its_reason_and_never_echoes_the_value() {
    const SECRET: &str = "VALUE-SENTINEL-4f3a9c";
    let cases: Vec<(Value, &str, Option<&str>)> = vec![
        (
            json!({"aws": {"profile": SECRET}}),
            "credentials",
            Some("aws.profile"),
        ),
        (json!({"provider": SECRET}), "provider", Some("provider")),
        (json!({"telemetryOptOut": SECRET}), "telemetry", None),
        (json!({"env": SECRET}), "processes", Some("env")),
        (json!({"hooks": [SECRET]}), "processes", Some("hooks")),
        (
            json!({"mcpServers": {"x": {"command": SECRET}}}),
            "processes",
            None,
        ),
        (json!({"autoApprove": SECRET}), "permissions", None),
        (json!({"proxy": SECRET}), "network", Some("proxy")),
        (json!({"goal": SECRET}), "owned-elsewhere", Some("goal")),
        (json!({"api_key": SECRET}), "credentials", Some("api_key")),
        (json!({"model": SECRET}), "provider", Some("model")),
        (json!({"maxTokens": SECRET}), "provider", None),
        (json!({"sandbox": SECRET}), "permissions", Some("sandbox")),
        (
            json!({"research": SECRET}),
            "owned-elsewhere",
            Some("research"),
        ),
    ];
    let mut classes = std::collections::BTreeSet::new();
    for (settings, class, key) in cases {
        let p = check(settings.clone()).unwrap_err();
        assert_eq!(p.code(), "denied_key", "{settings}");
        assert_eq!(p.key(), key, "{settings}");
        let d = DENIED.iter().find(|d| d.id == class).unwrap();
        let text = p.to_string();
        assert!(text.contains(d.reason), "{text}");
        assert_eq!(p.rule(), d.reason, "{settings}");
        assert!(
            !text.contains(SECRET) && !p.rule().contains(SECRET),
            "{text}"
        );
        classes.insert(class);
        match key {
            Some(k) => assert!(text.contains(&format!("key \"{k}\"")), "{text}"),
            None => assert!(text.contains("a key with unusual characters"), "{text}"),
        }
    }
    assert_eq!(
        classes.len(),
        DENIED.len(),
        "every class is exercised: {classes:?}"
    );
}

#[test]
fn a_key_of_the_table_is_never_classified_as_denied_because_the_allow_list_comes_first() {
    // `commands` is near the denied word `command`, `timeout_seconds` near `timeout`, `context` near `contextWindow`.
    for r in CLINE_KEYS {
        check(match r.kind {
            KeyKind::Int { min, .. } => nested(r.path, json!(min)),
            KeyKind::Float { min, .. } => nested(r.path, json!(min)),
            KeyKind::Choice(c) => nested(r.path, json!(c[0])),
            KeyKind::Set(_) => nested(r.path, json!([])),
        })
        .unwrap();
    }
    // Their cousins outside the table are not.
    assert_eq!(code(json!({"command": "x"})), "denied_key");
    assert_eq!(code(json!({"timeout": 5})), "denied_key");
    assert_eq!(code(json!({"commands": {"run": "x"}})), "unknown_key");
    assert_eq!(code(json!({"context": {"window": 5}})), "unknown_key");
    assert_eq!(code(json!({"tools": {"enable": ["x"]}})), "unknown_key");
    assert_eq!(
        code(json!({"reasoning": {"effort": "low", "budget": 5}})),
        "unknown_key"
    );
}

// ---- hostile JSON ----------------------------------------------------------------------------------------------------------

#[test]
fn hostile_json_is_refused_with_a_stable_code() {
    let sixty_five: Vec<String> = (0..65).map(|i| format!("a{i}")).collect();
    let ten_thousand: Vec<&str> = vec!["spawn_agent"; 10_000];
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "a table where a scalar belongs",
            json!({"reasoning": {"effort": {"x": 1}}}),
            "bad_type",
        ),
        (
            "a scalar where a table belongs",
            json!({"reasoning": "high"}),
            "bad_type",
        ),
        (
            "a list where a table belongs",
            json!({"tools": ["spawn_agent"]}),
            "bad_type",
        ),
        ("null", json!({"reasoning": {"effort": null}}), "bad_type"),
        ("null as a table", json!({"reasoning": null}), "bad_type"),
        ("depth 4", json!({"a": {"b": {"c": {"d": 1}}}}), "too_deep"),
        (
            "65 leaves",
            json!({"tools": {"disable": sixty_five}}),
            "too_many_keys",
        ),
        (
            "a 10,000-entry list",
            json!({"tools": {"disable": ten_thousand}}),
            "too_large",
        ),
        (
            "a repeated list entry",
            json!({"tools": {"disable": ["spawn_agent", "spawn_agent"]}}),
            "duplicate_value",
        ),
        (
            "a 65-byte string (a closed set refuses a long text as out of range first)",
            json!({"reasoning": {"effort": "y".repeat(65)}}),
            "out_of_range",
        ),
        (
            "a 64-byte string",
            json!({"reasoning": {"effort": "y".repeat(64)}}),
            "out_of_range",
        ),
        (
            "a 65-byte list entry",
            json!({"tools": {"disable": ["y".repeat(65)]}}),
            "out_of_range",
        ),
        (
            "__proto__",
            json!({"__proto__": {"polluted": true}}),
            "unknown_key",
        ),
        (
            "constructor",
            json!({"constructor": {"prototype": {}}}),
            "unknown_key",
        ),
        (
            "a Cyrillic look-alike of effort",
            json!({"reasoning": {"\u{0435}ffort": "high"}}),
            "unknown_key",
        ),
        (
            "a Cyrillic look-alike of the table",
            json!({"r\u{0435}asoning": {"effort": "high"}}),
            "unknown_key",
        ),
        (
            "a quoted key with a dot",
            json!({"reasoning.effort": "high"}),
            "unknown_key",
        ),
        ("an empty key", json!({"": 1}), "unknown_key"),
        (
            "an escape sequence as a key",
            json!({"\u{1b}[31mred": 1}),
            "unknown_key",
        ),
        ("a newline in a key", json!({"a\nb": 1}), "unknown_key"),
        ("a NUL in a key", json!({"a\u{0}b": 1}), "unknown_key"),
        (
            "a bidi override in a key",
            json!({"a\u{202e}b": 1}),
            "unknown_key",
        ),
        ("a 41-byte key", json!({"k".repeat(41): 1}), "unknown_key"),
    ];
    for (name, settings, want) in cases {
        let p = check(settings).unwrap_err();
        assert_eq!(p.code(), want, "{name}: {p}");
        let text = p.to_string();
        assert!(!text.chars().any(char::is_control), "{name}: {text:?}");
        assert!(text.is_ascii(), "{name}: {text:?}");
        assert!(text.len() < 400, "{name}: {} bytes", text.len());
    }
}

#[test]
fn a_key_is_named_only_when_it_is_short_plain_text() {
    let named = |k: &str| {
        check(json!({ k: 1 }))
            .unwrap_err()
            .key()
            .map(str::to_string)
    };
    assert_eq!(named("nope"), Some("nope".into()));
    assert_eq!(named("under_score-dash"), Some("under_score-dash".into()));
    assert_eq!(named(&"k".repeat(40)), Some("k".repeat(40)));
    for hostile in [
        "k".repeat(41),
        "Upper".to_string(),
        "has space".to_string(),
        "\u{0435}".to_string(),
        "a\nb".to_string(),
        "\u{1b}[0m".to_string(),
        "a.b".to_string(),
        String::new(),
    ] {
        assert_eq!(named(&hostile), None, "{hostile:?}");
    }
    // a path is named whole or not at all
    let p = check(json!({"aws": {"profile": 1}})).unwrap_err();
    assert_eq!(p.key(), Some("aws.profile"));
    let p = check(json!({"aws": {"Profile": 1}})).unwrap_err();
    assert_eq!(p.key(), None);
}

#[test]
fn the_key_is_named_in_a_range_error_and_the_value_is_not() {
    const SECRET: &str = "VALUE-SENTINEL-71ce";
    let p = check(json!({"reasoning": {"effort": SECRET}})).unwrap_err();
    assert_eq!(p.code(), "out_of_range");
    assert_eq!(p.key(), Some("reasoning.effort"));
    assert!(!p.to_string().contains(SECRET));
    assert!(p.to_string().contains("reasoning.effort"));
    assert!(
        p.rule().starts_with("must be one of: default"),
        "{}",
        p.rule()
    );
    let p = check(json!({"commands": {"timeout_seconds": 1801}})).unwrap_err();
    assert_eq!(p.rule(), "must be a whole number from 1 to 1800");
    let p = check(json!({"context": {"compaction_trigger": 2.5}})).unwrap_err();
    assert_eq!(p.rule(), "must be a number from 0.10 to 0.99");
    let p = check(json!({"tools": {"disable": ["bash"]}})).unwrap_err();
    assert!(
        p.rule().contains("spawn_agent") && p.rule().contains("fetch_web_content"),
        "{}",
        p.rule()
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn any_document_is_accepted_or_refused_with_a_clean_message(settings in json_object()) {
        let c = EngineConfig { engine: "cline".into(), schema: 1, settings };
        match check_engine_config(&c) {
            Ok(()) => {
                prop_assert!(serde_json::to_vec(&c).unwrap().len() <= MAX_ENGINE_CONFIG_BYTES);
                for top in c.settings.keys() {
                    prop_assert!(["reasoning", "commands", "context", "tools"].contains(&top.as_str()), "{top}");
                }
                prop_assert!(c.key_count() as usize <= CLINE_KEYS.len());
            }
            Err(p) => {
                let text = p.to_string();
                prop_assert!(!text.chars().any(char::is_control), "{text:?}");
                prop_assert!(!text.contains("~v~"), "a value was echoed: {text:?}");
                prop_assert!(!p.rule().contains("~v~"));
                if let Some(k) = p.key() {
                    prop_assert!(!k.is_empty() && k.len() <= 40, "{k:?}");
                    prop_assert!(k.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.')), "{k:?}");
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn every_document_built_from_in_range_keys_passes_and_hashes_the_same_in_any_order(
        effort in prop::option::of(0usize..4),
        timeout in prop::option::of(1i64..=1800),
        compaction in prop::option::of(0usize..3),
        trigger in prop::option::of(0.10f64..=0.99),
        disable in prop::option::of(prop::sample::subsequence(vec!["spawn_agent", "fetch_web_content"], 0..=2)),
    ) {
        let KeyKind::Choice(levels) = row("reasoning.effort").kind else { panic!("choice") };
        let mut parts: Vec<(&str, Value)> = vec![];
        if let Some(i) = effort {
            parts.push(("reasoning", json!({"effort": levels[i % levels.len()]})));
        }
        if let Some(t) = timeout {
            parts.push(("commands", json!({"timeout_seconds": t})));
        }
        let mut context = Map::new();
        if let Some(i) = compaction {
            context.insert("compaction".into(), json!(["agentic", "basic", "off"][i]));
        }
        if let Some(t) = trigger {
            context.insert("compaction_trigger".into(), json!(t));
        }
        if !context.is_empty() {
            parts.push(("context", Value::Object(context)));
        }
        if let Some(d) = &disable {
            parts.push(("tools", json!({"disable": d})));
        }
        let forward: Map<String, Value> = parts.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let backward: Map<String, Value> = parts.iter().rev().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let a = EngineConfig { engine: "cline".into(), schema: 1, settings: forward };
        let b = EngineConfig { engine: "cline".into(), schema: 1, settings: backward };
        prop_assert_eq!(check_engine_config(&a), Ok(()));
        prop_assert_eq!(a.canonical_sha256(), b.canonical_sha256());
        let expected = usize::from(effort.is_some()) + usize::from(timeout.is_some())
            + usize::from(compaction.is_some()) + usize::from(trigger.is_some()) + usize::from(disable.is_some());
        prop_assert_eq!(a.key_count() as usize, expected);
    }
}

fn json_object() -> impl Strategy<Value = Map<String, Value>> {
    let key = prop_oneof![
        Just("reasoning".to_string()),
        Just("effort".to_string()),
        Just("commands".to_string()),
        Just("timeout_seconds".to_string()),
        Just("context".to_string()),
        Just("compaction".to_string()),
        Just("compaction_trigger".to_string()),
        Just("tools".to_string()),
        Just("disable".to_string()),
        Just("aws".to_string()),
        ".{0,12}",
    ];
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        (0u64..2000).prop_map(|n| json!(n)),
        any::<u64>().prop_map(|n| json!(n)),
        any::<f64>().prop_filter_map("finite", |f| serde_json::Number::from_f64(f)
            .map(Value::Number)),
        (0.0f64..1.0).prop_map(|f| json!(f)),
        "~v~[a-z0-9]{0,70}".prop_map(Value::String),
        Just(json!("low")),
        Just(json!("spawn_agent")),
    ];
    let value = leaf.prop_recursive(4, 48, 6, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(key.clone(), inner, 0..5)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    });
    prop::collection::btree_map(
        prop_oneof![
            Just("reasoning".to_string()),
            Just("commands".to_string()),
            Just("context".to_string()),
            Just("tools".to_string()),
            ".{0,12}",
        ],
        value,
        0..5,
    )
    .prop_map(|m| m.into_iter().collect())
}

// ---- the hash --------------------------------------------------------------------------------------------------------------

#[test]
fn canonical_sha256_is_the_same_for_reordered_settings_and_is_the_hash_of_the_sorted_text() {
    let a: EngineConfig = serde_json::from_str(
        r#"{"engine":"cline","schema":1,"settings":{"tools":{"disable":["spawn_agent"]},"reasoning":{"effort":"high"},"commands":{"timeout_seconds":900}}}"#,
    )
    .unwrap();
    let b: EngineConfig = serde_json::from_str(
        r#"{"settings":{"commands":{"timeout_seconds":900},"reasoning":{"effort":"high"},"tools":{"disable":["spawn_agent"]}},"schema":1,"engine":"cline"}"#,
    )
    .unwrap();
    let sorted = r#"{"engine":"cline","schema":1,"settings":{"commands":{"timeout_seconds":900},"reasoning":{"effort":"high"},"tools":{"disable":["spawn_agent"]}}}"#;
    let want: String = Sha256::digest(sorted.as_bytes())
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    assert_eq!(a.canonical_sha256(), want);
    assert_eq!(b.canonical_sha256(), want);
    assert_eq!(want.len(), 64);
    // notation does not matter: 0.90 and 0.9 are one number
    let x: EngineConfig = serde_json::from_str(
        r#"{"engine":"cline","schema":1,"settings":{"context":{"compaction_trigger":0.90}}}"#,
    )
    .unwrap();
    let y: EngineConfig = serde_json::from_str(
        r#"{"engine":"cline","schema":1,"settings":{"context":{"compaction_trigger":0.9}}}"#,
    )
    .unwrap();
    assert_eq!(x.canonical_sha256(), y.canonical_sha256());
    // a different value, a different engine or a different schema is a different hash
    let mut z = y.clone();
    z.settings = json!({"context": {"compaction_trigger": 0.8}})
        .as_object()
        .unwrap()
        .clone();
    assert_ne!(y.canonical_sha256(), z.canonical_sha256());
    let mut w = y.clone();
    w.schema = 2;
    assert_ne!(y.canonical_sha256(), w.canonical_sha256());
}

#[test]
fn key_count_counts_table_keys_set_not_list_entries() {
    let c = cfg(json!({
        "reasoning": {"effort": "low"},
        "tools": {"disable": ["spawn_agent", "fetch_web_content"]}
    }));
    assert_eq!(c.key_count(), 2);
    assert_eq!(cfg(json!({})).key_count(), 0);
    assert_eq!(c.get("reasoning.effort"), Some(&json!("low")));
    assert_eq!(c.get("commands.timeout_seconds"), None);
    assert_eq!(c.get("reasoning"), None, "a table is not a key");
    let r = c.to_ref();
    assert_eq!((r.engine.as_str(), r.schema, r.keys), ("cline", 1, 2));
    assert_eq!(r.sha256, c.canonical_sha256());
}

// ---- ranks inside one document ---------------------------------------------------------------------------------------------

#[test]
fn the_ranks_inside_one_value_and_between_keys_follow_the_documented_order() {
    // one list: the JSON type of every entry first, then the set, then a repeat
    assert_eq!(
        code(json!({"tools": {"disable": ["spawn_agent", "spawn_agent", null]}})),
        "bad_type"
    );
    assert_eq!(
        code(json!({"tools": {"disable": ["spawn_agent", "spawn_agent", "bogus"]}})),
        "out_of_range"
    );
    assert_eq!(
        code(json!({"tools": {"disable": ["bogus", 7]}})),
        "bad_type"
    );
    assert_eq!(
        code(json!({"tools": {"disable": ["spawn_agent", "spawn_agent"]}})),
        "duplicate_value"
    );
    // a long text that is not a member is out of range
    assert_eq!(
        code(json!({"reasoning": {"effort": "y".repeat(65)}})),
        "out_of_range"
    );
    // between keys: depth first, each table's keys in sorted order
    assert_eq!(
        code(json!({"context": {"compaction": "zzz"}, "commands": {"timeout_seconds": 5000}})),
        "out_of_range"
    );
    assert_eq!(
        code(json!({"commands": {"timeout_seconds": "x"}, "context": {"compaction": "zzz"}})),
        "bad_type"
    );
    assert_eq!(
        code(json!({"zzz": 1, "commands": {"timeout_seconds": "x"}})),
        "bad_type",
        "commands sorts before zzz"
    );
    assert_eq!(
        code(json!({"aaa": 1, "commands": {"timeout_seconds": "x"}})),
        "unknown_key"
    );
    // inside a table the keys are sorted too: compaction before compaction_trigger
    assert_eq!(
        code(json!({"context": {"compaction_trigger": 7, "compaction": 7}})),
        "bad_type"
    );
    assert_eq!(
        check(json!({"context": {"compaction_trigger": 7, "compaction": 7}}))
            .unwrap_err()
            .key(),
        Some("context.compaction")
    );
    // too large wins over too deep, whatever the depth
    let mut c = cfg(json!({}));
    c.settings
        .insert("zz".into(), json!({"a": {"b": {"c": "x".repeat(5000)}}}));
    assert_eq!(check_engine_config(&c).unwrap_err().code(), "too_large");
}

/// A second, independent reading of the table: does the document hold only keys of the table, each with a value its kind admits? Written
/// without the code under test, for small documents (inside the size, depth and value limits).
fn oracle_accepts(settings: &Map<String, Value>) -> bool {
    fn admits(kind: &KeyKind, v: &Value) -> bool {
        match kind {
            KeyKind::Choice(set) => v.as_str().is_some_and(|s| set.contains(&s)),
            KeyKind::Int { min, max } => v.as_i64().is_some_and(|x| x >= *min && x <= *max),
            KeyKind::Float { min, max } => v.as_f64().is_some_and(|x| x >= *min && x <= *max),
            KeyKind::Set(members) => v.as_array().is_some_and(|items| {
                let mut seen = std::collections::HashSet::new();
                items.iter().all(|e| {
                    e.as_str()
                        .is_some_and(|s| members.contains(&s) && seen.insert(s))
                })
            }),
        }
    }
    settings.iter().all(|(table, inner)| {
        let Some(inner) = inner.as_object() else {
            return false;
        };
        inner.iter().all(|(key, v)| {
            let path = format!("{table}.{key}");
            CLINE_KEYS
                .iter()
                .find(|r| r.path == path)
                .is_some_and(|r| admits(&r.kind, v))
        })
    })
}

fn junk() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<u64>().prop_map(|n| json!(n)),
        (-2.0f64..2.0).prop_map(|f| json!(f)),
        "[a-z_]{0,12}".prop_map(Value::String),
        Just(json!("spawn_agent")),
        Just(json!("low")),
        Just(json!(["spawn_agent", "fetch_web_content"])),
        Just(json!(["spawn_agent", "spawn_agent"])),
        prop::collection::vec(
            prop_oneof![
                Just(json!("spawn_agent")),
                Just(json!("fetch_web_content")),
                Just(json!("bogus")),
                Just(json!(1)),
            ],
            0..4
        )
        .prop_map(Value::Array),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, failure_persistence: None, ..ProptestConfig::default() })]

    /// For small documents built from the five keys, an in-range value, a junk value or nothing, plus an unknown key now and then, the check
    /// accepts exactly what an independent reading of the table accepts.
    #[test]
    fn the_check_agrees_with_an_independent_reading_of_the_table(
        effort in prop::option::of(prop_oneof![(0usize..4).prop_map(|i| json!(["default", "low", "medium", "high"][i])), junk()]),
        timeout in prop::option::of(prop_oneof![(1i64..=1800).prop_map(|n| json!(n)), junk()]),
        compaction in prop::option::of(prop_oneof![(0usize..3).prop_map(|i| json!(["agentic", "basic", "off"][i])), junk()]),
        trigger in prop::option::of(prop_oneof![(0.10f64..=0.99).prop_map(|f| json!(f)), junk()]),
        disable in prop::option::of(prop_oneof![
            prop::sample::subsequence(vec!["spawn_agent", "fetch_web_content"], 0..=2).prop_map(|v| json!(v)),
            junk()
        ]),
        extra in prop::option::of(("[a-z]{1,8}", junk(), any::<bool>())),
    ) {
        let mut tables: Map<String, Value> = Map::new();
        let mut put = |table: &str, key: &str, v: Option<Value>| {
            if let Some(v) = v {
                let t = tables.entry(table.to_string()).or_insert_with(|| json!({}));
                t.as_object_mut().unwrap().insert(key.to_string(), v);
            }
        };
        put("reasoning", "effort", effort);
        put("commands", "timeout_seconds", timeout);
        put("context", "compaction", compaction);
        put("context", "compaction_trigger", trigger);
        put("tools", "disable", disable);
        if let Some((name, v, nested)) = extra {
            if nested {
                put("tools", &name, Some(v));
            } else {
                tables.insert(name, v);
            }
        }
        let c = EngineConfig { engine: "cline".into(), schema: 1, settings: tables };
        prop_assert_eq!(
            check_engine_config(&c).is_ok(),
            oracle_accepts(&c.settings),
            "{}",
            serde_json::to_string(&c).unwrap()
        );
    }
}

// ---- the API around the check ----------------------------------------------------------------------------------------------

#[test]
fn a_float_range_never_admits_an_infinity_even_when_a_table_says_so() {
    let open_ended = KeyKind::Float {
        min: 0.0,
        max: f64::INFINITY,
    };
    assert!(open_ended.admits_f64(1e300));
    assert!(!open_ended.admits_f64(f64::INFINITY));
    assert!(!open_ended.admits_f64(f64::NAN));
}

#[test]
fn the_table_check_refuses_ranges_and_sets_that_could_let_text_or_a_non_number_through() {
    let tools = *row("tools.disable");
    let timeout = *row("commands.timeout_seconds");
    let trigger = *row("context.compaction_trigger");
    let bad: Vec<(&str, KeyRow)> = vec![
        (
            "an inverted integer range",
            KeyRow {
                kind: KeyKind::Int { min: 5, max: 1 },
                default: "3",
                ..timeout
            },
        ),
        (
            "a float range that is not finite",
            KeyRow {
                kind: KeyKind::Float {
                    min: 0.1,
                    max: f64::INFINITY,
                },
                default: "0.5",
                ..trigger
            },
        ),
        (
            "a float range that is NaN",
            KeyRow {
                kind: KeyKind::Float {
                    min: f64::NAN,
                    max: 1.0,
                },
                default: "0.5",
                ..trigger
            },
        ),
        (
            "an empty set",
            KeyRow {
                kind: KeyKind::Set(&[]),
                ..tools
            },
        ),
        (
            "a repeated member",
            KeyRow {
                kind: KeyKind::Set(&["a", "a"]),
                ..tools
            },
        ),
        (
            "a member with a space",
            KeyRow {
                kind: KeyKind::Set(&["has space"]),
                ..tools
            },
        ),
        (
            "a member with a quote",
            KeyRow {
                kind: KeyKind::Set(&["say\"x"]),
                ..tools
            },
        ),
        (
            "an upper-case member",
            KeyRow {
                kind: KeyKind::Set(&["Spawn"]),
                ..tools
            },
        ),
        (
            "an empty member",
            KeyRow {
                kind: KeyKind::Set(&[""]),
                ..tools
            },
        ),
    ];
    for (what, r) in bad {
        assert!(!table_problems(&[r]).is_empty(), "{what}");
    }
}

#[test]
fn debug_of_the_member_shows_no_value() {
    const SECRET: &str = "VALUE-SENTINEL-55aa";
    let c = cfg(json!({"reasoning": {"effort": SECRET}, "aws": {"profile": SECRET}}));
    let text = format!("{c:?}");
    assert!(!text.contains(SECRET) && !text.contains("effort"), "{text}");
    assert!(
        text.contains("cline") && text.contains("schema") && text.contains("2 top-level keys"),
        "{text}"
    );
    // inside a request, too
    let request: harness_protocol::api::v1::StartDirectRunRequest = serde_json::from_value(json!({
        "prompt": "p", "target": {"mode": "s3-only"}, "model": "m", "rules": [],
        "engineConfig": c
    }))
    .unwrap();
    let text = format!("{request:?}");
    assert!(!text.contains(SECRET), "{text}");
    // the problem never carries it either
    let p = check_engine_config(&c).unwrap_err();
    assert!(!format!("{p:?}").contains(SECRET));
}
