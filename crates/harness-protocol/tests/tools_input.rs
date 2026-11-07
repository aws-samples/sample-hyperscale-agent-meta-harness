//! Protocol 1.40.0, wire B: the input of `code_search`. A model sends the arguments, Cline does not validate them,
//! so the server does, and the answer is a plain usage message. The message names the member and the rule and never repeats a value: a
//! pattern may be a secret, and the text of a refusal must be safe to show anywhere.
#![allow(clippy::unwrap_used)]

use harness_protocol::tools::{
    CodeSearchInput, DEFAULT_MAX_PER_FILE, DEFAULT_MAX_RESULTS, ToolInputError,
};
use serde_json::{Value, json};

/// A value that stands for a secret in every test below; no message may hold it.
const CANARY: &str = "CANARY-value-7d41";

fn parse(v: Value) -> Result<CodeSearchInput, ToolInputError> {
    CodeSearchInput::from_arguments(&v)
}

fn refused(v: Value) -> String {
    let rendered = v.to_string();
    let err = parse(v).expect_err(&format!("should be refused: {rendered}"));
    let text = err.to_string();
    assert!(
        !text.contains(CANARY),
        "the message repeats a value: {text}"
    );
    assert!(!text.is_empty());
    text
}

#[test]
fn a_minimal_call_parses_and_every_default_applies() {
    let i = parse(json!({"pattern": "fn main"})).unwrap();
    assert_eq!(i.pattern, "fn main");
    assert_eq!((DEFAULT_MAX_RESULTS, DEFAULT_MAX_PER_FILE), (50, 10));
    assert_eq!(i.max_results(), 50);
    assert_eq!(i.max_per_file(), 10);
    assert_eq!(i.context(), 0);
    assert!(
        !i.fixed() && !i.files_only() && !i.count() && !i.include_hidden() && !i.include_ignored()
    );
    assert!(i.paths.is_empty() && i.glob.is_empty());
}

#[test]
fn every_member_parses_under_its_camel_case_name_and_round_trips() {
    let args = json!({
        "pattern": "a.b", "fixed": true, "paths": ["src", "/ctx"], "glob": ["*.rs", "!target/**"],
        "maxResults": 7, "maxPerFile": 2, "filesOnly": true, "context": 3,
        "includeHidden": true, "includeIgnored": true
    });
    let i = parse(args.clone()).unwrap();
    assert_eq!(i.max_results(), 7);
    assert_eq!(i.max_per_file(), 2);
    assert_eq!(i.context(), 3);
    assert!(i.fixed() && i.files_only() && i.include_hidden() && i.include_ignored() && !i.count());
    assert_eq!(i.paths, ["src", "/ctx"]);
    assert_eq!(i.glob, ["*.rs", "!target/**"]);
    // What is left out stays left out; what was sent is sent back unchanged.
    assert_eq!(serde_json::to_value(&i).unwrap(), args);
    let counting = parse(json!({"pattern": "x", "count": true})).unwrap();
    assert!(counting.count() && !counting.files_only());
}

#[test]
fn an_absolute_path_is_the_servers_business_not_the_wires() {
    // The context directory is absolute; whether a path lies in the work tree or the context directory is decided after link resolution by the
    // server. The wire only bounds the entry.
    assert!(parse(json!({"pattern": "x", "paths": ["/some/absolute/dir", "../up", "."]})).is_ok());
}

#[test]
fn arguments_that_are_not_an_object_are_refused() {
    for v in [
        json!(null),
        json!([]),
        json!("pattern"),
        json!(5),
        json!(true),
    ] {
        let text = refused(v);
        assert!(text.contains("object"), "{text}");
    }
}

#[test]
fn pattern_is_required_and_bounded_in_characters() {
    assert!(refused(json!({})).contains("pattern"));
    assert!(refused(json!({"pattern": ""})).contains("pattern"));
    assert!(
        parse(json!({"pattern": "é".repeat(1000)})).is_ok(),
        "1,000 characters of two bytes each"
    );
    assert!(parse(json!({"pattern": "a".repeat(1000)})).is_ok());
    let text = refused(json!({"pattern": "a".repeat(1001)}));
    assert!(text.contains("pattern") && text.contains("1000"), "{text}");
}

#[test]
fn an_unknown_member_is_refused_and_not_repeated() {
    let mut m = serde_json::Map::new();
    m.insert("pattern".into(), json!("a"));
    m.insert(CANARY.into(), json!(1));
    let text = refused(Value::Object(m));
    assert!(text.contains("unknown member"), "{text}");
    // The allowed names are listed so the model can correct itself.
    for name in ["pattern", "maxResults", "includeIgnored"] {
        assert!(text.contains(name), "{text}");
    }
    for member in ["case", "multiline"] {
        let text = refused(json!({"pattern": "a", member: "x"}));
        assert!(text.contains("unknown member"), "{member}: {text}");
    }
}

#[test]
fn a_wrong_type_names_the_member_and_never_repeats_the_value() {
    let cases = [
        ("pattern", json!(CANARY.len())),
        ("fixed", json!(CANARY)),
        ("filesOnly", json!(CANARY)),
        ("count", json!(1)),
        ("includeHidden", json!(CANARY)),
        ("includeIgnored", json!([CANARY])),
        ("paths", json!(CANARY)),
        ("paths", json!([CANARY.len()])),
        ("glob", json!({"a": CANARY})),
        ("maxResults", json!(CANARY)),
        ("maxResults", json!(1.5)),
        ("maxResults", json!(-1)),
        ("maxPerFile", json!(CANARY)),
        ("context", json!(CANARY)),
        ("context", json!(0.5)),
    ];
    for (member, value) in cases {
        let mut m = serde_json::Map::new();
        m.insert("pattern".into(), json!("a"));
        m.insert(member.into(), value.clone());
        let text = refused(Value::Object(m));
        assert!(text.contains(member), "{member} = {value}: {text}");
    }
}

#[test]
fn null_means_absent() {
    // The model-facing schema says "integer", never "null"; an engine that sends null for an optional member is read as leaving it out.
    let i =
        parse(json!({"pattern": "a", "maxResults": null, "paths": null, "fixed": null})).unwrap();
    assert_eq!((i.max_results(), i.paths.len(), i.fixed()), (50, 0, false));
}

#[test]
fn numbers_hold_their_limits_at_both_edges() {
    for (member, min, max) in [
        ("maxResults", 1, 200),
        ("maxPerFile", 1, 50),
        ("context", 0, 5),
    ] {
        for ok in [min, max] {
            let mut m = serde_json::Map::new();
            m.insert("pattern".into(), json!("a"));
            m.insert(member.into(), json!(ok));
            assert!(parse(Value::Object(m)).is_ok(), "{member} = {ok}");
        }
        for bad in [min - 1, max + 1] {
            let mut m = serde_json::Map::new();
            m.insert("pattern".into(), json!("a"));
            m.insert(member.into(), json!(bad));
            let text = refused(Value::Object(m));
            assert!(
                text.contains(member)
                    && text.contains(&min.to_string())
                    && text.contains(&max.to_string()),
                "{member} = {bad}: {text}"
            );
        }
    }
}

#[test]
fn lists_hold_ten_entries_of_one_to_1024_bytes_without_a_nul() {
    for member in ["paths", "glob"] {
        let list = |entries: Vec<String>| {
            let mut m = serde_json::Map::new();
            m.insert("pattern".into(), json!("a"));
            m.insert(member.into(), json!(entries));
            Value::Object(m)
        };
        let ten: Vec<String> = (0..10).map(|i| format!("e{i}")).collect();
        assert!(parse(list(ten.clone())).is_ok(), "{member}: ten entries");
        let mut eleven = ten;
        eleven.push("e10".into());
        assert!(refused(list(eleven)).contains(member));
        assert!(
            parse(list(vec!["x".repeat(1024)])).is_ok(),
            "{member}: 1,024 bytes"
        );
        assert!(refused(list(vec!["x".repeat(1025)])).contains(member));
        assert!(
            parse(list(vec!["é".repeat(512)])).is_ok(),
            "{member}: the limit counts bytes"
        );
        assert!(refused(list(vec!["é".repeat(513)])).contains(member));
        assert!(
            refused(list(vec![String::new()])).contains(member),
            "{member}: an empty entry"
        );
        assert!(
            refused(list(vec!["a\u{0}b".into()])).contains(member),
            "{member}: a NUL cannot be passed to a child process"
        );
    }
    assert!(refused(json!({"pattern": "a\u{0}b"})).contains("pattern"));
}

#[test]
fn files_only_and_count_exclude_each_other() {
    assert!(parse(json!({"pattern": "a", "filesOnly": true, "count": false})).is_ok());
    assert!(parse(json!({"pattern": "a", "filesOnly": false, "count": true})).is_ok());
    let text = refused(json!({"pattern": "a", "filesOnly": true, "count": true}));
    assert!(
        text.contains("filesOnly") && text.contains("count"),
        "{text}"
    );
    // Context with a file list or a count has nothing to show; it is accepted and ignored, not an error.
    assert!(parse(json!({"pattern": "a", "filesOnly": true, "context": 2})).is_ok());
}

#[test]
fn a_usage_message_is_one_short_plain_line() {
    let text = refused(json!({"pattern": "a", "maxResults": 9999}));
    assert!(
        !text.contains('\n') && text.chars().count() <= 160,
        "{text}"
    );
}
