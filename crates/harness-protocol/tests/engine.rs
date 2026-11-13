//! The engine as a profile name: any `engine/profiles/<name>.toml`
//! stem is an engine, so a new engine needs no protocol change, and every record written with the
//! closed 1.x set (`"cline"`, `"opencode"`) reads and writes back the same.
#![allow(clippy::unwrap_used)]

use harness_protocol::common::{ENGINE_NAME_MAX, ENGINE_NAME_PATTERN, Engine};
use serde_json::json;

#[test]
fn every_profile_name_is_an_engine_and_is_its_name_on_the_wire() {
    let longest = "a".repeat(ENGINE_NAME_MAX);
    for name in [
        "cline",
        "opencode",
        "fake-acp",
        "pi",
        "a",
        "x9-y",
        longest.as_str(),
    ] {
        let e = Engine::new(name).unwrap();
        assert_eq!(e.as_str(), name);
        assert_eq!(e.to_string(), name);
        assert_eq!(format!("{e:?}"), format!("Engine({name:?})"));
        assert_eq!(serde_json::to_value(e).unwrap(), json!(name));
        assert_eq!(serde_json::from_value::<Engine>(json!(name)).unwrap(), e);
        assert_eq!(name.parse::<Engine>().unwrap(), e);
    }
    assert_eq!(Engine::new("cline").unwrap(), Engine::CLINE);
    assert_eq!(Engine::new("opencode").unwrap(), Engine::OPENCODE);
    assert_eq!(Engine::named("fake-acp"), Engine::new("fake-acp").unwrap());
}

#[test]
fn a_name_that_cannot_be_a_profile_file_stem_is_refused() {
    let too_long = "a".repeat(ENGINE_NAME_MAX + 1);
    for bad in [
        "",
        "Cline",
        "1engine",
        "-x",
        "a_b",
        "a.b",
        "../x",
        "a/b",
        "a b",
        "\u{e9}",
        too_long.as_str(),
    ] {
        assert!(Engine::new(bad).is_err(), "{bad:?}");
        assert!(
            serde_json::from_value::<Engine>(json!(bad)).is_err(),
            "{bad:?}"
        );
    }
    let e = Engine::new("Bad").unwrap_err();
    assert!(e.to_string().contains("\"Bad\""), "{e}");
    assert!(e.to_string().contains("[a-z][a-z0-9-]"), "{e}");
    assert!(serde_json::from_value::<Engine>(json!(5)).is_err());
}

#[test]
#[should_panic(expected = "not an engine profile name")]
fn a_constant_name_is_checked_too() {
    let _ = Engine::named("Not-A-Name");
}

#[test]
fn engines_order_by_name_are_copy_and_the_constants_are_match_patterns() {
    let fake = Engine::new("fake-acp").unwrap();
    let mut v = [
        Engine::OPENCODE,
        fake,
        Engine::CLINE,
        Engine::new("cline-b").unwrap(),
        Engine::new("c").unwrap(),
    ];
    v.sort();
    let names: Vec<&str> = v.iter().map(|e| e.as_str()).collect();
    assert_eq!(names, ["c", "cline", "cline-b", "fake-acp", "opencode"]);
    let kind = |e: Engine| match e {
        Engine::CLINE => "fork",
        Engine::OPENCODE => "alternate",
        _ => "profile",
    };
    let copied = fake;
    assert_eq!(
        [
            kind(Engine::CLINE),
            kind(Engine::OPENCODE),
            kind(fake),
            kind(copied)
        ],
        ["fork", "alternate", "profile", "profile"]
    );
}

#[test]
fn the_schema_is_a_string_of_the_profile_name_pattern() {
    let schema = serde_json::to_value(schemars::schema_for!(Engine)).unwrap();
    assert_eq!(schema["type"], "string");
    assert_eq!(schema["pattern"], ENGINE_NAME_PATTERN);
    assert!(schema.get("enum").is_none(), "{schema}");
    assert_eq!(ENGINE_NAME_PATTERN, "^[a-z][a-z0-9-]{0,31}$");
}
