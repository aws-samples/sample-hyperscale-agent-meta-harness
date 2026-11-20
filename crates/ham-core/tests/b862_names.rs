//! The short names of the models: what a name may look like, and that a name can never be read as something else.
//!
//! A name may now carry a version (`sol-6.1`), so the grammar takes a dot. `Models::find` reads a typed word as an exact id first and as a name
//! second, so a name that equals a model id could never be typed, and a name that equals an alternate route id would look like a route a run may
//! not name: the parser refuses both. A longer name must not break the table of `ham models`, so its name column follows the longest name.

#![allow(clippy::unwrap_used)]

use ham_core::models::{
    ALIAS_RULE, ModelList, ModelSource, Models, ModelsError, builtin, render_models,
};
use serde_json::{Value, json};

fn row(id: &str, alias: Option<&str>) -> Value {
    let base = id.split_once('.').map_or(id, |(_, rest)| rest);
    let mut v = json!({
        "id": id, "name": format!("Name of {id}"), "vendor": "v", "baseModelId": base, "protocol": "converse",
        "region": "us-east-1", "contextTokens": 200_000, "maxOutputTokens": 8192, "smokeRecordId": "s6:test-record"
    });
    if let Some(a) = alias {
        v["alias"] = json!(a);
    }
    v
}

fn parse(rows: Vec<Value>) -> Result<Models, ModelsError> {
    Models::parse(&json!({"schemaVersion": 1, "models": rows}).to_string())
}

fn refusal(rows: Vec<Value>) -> String {
    match parse(rows) {
        Err(ModelsError::Invalid(m)) => m,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn b862_an_alias_may_hold_a_dot_but_not_start_or_end_with_one() {
    let longest = "a".repeat(24);
    for alias in [
        "sol-6.1",
        "model-1.2",
        "astra-6",
        "a.b",
        "6",
        "x-1.2.3",
        longest.as_str(),
    ] {
        let list = parse(vec![row("us.v.m-1", Some(alias))])
            .unwrap_or_else(|e| panic!("{alias:?} is a name: {e}"));
        // A typed name is read in any letter case and with white space around it, a dot included.
        for typed in [
            alias.to_string(),
            alias.to_ascii_uppercase(),
            format!(" {alias}\n"),
        ] {
            assert_eq!(list.find(&typed).unwrap().id, "us.v.m-1", "{typed:?}");
        }
    }
    let too_long = "a".repeat(25);
    for alias in [
        ".x",
        "x.",
        "a..b",
        "-x",
        "x-",
        "x_y",
        "UPPER",
        "Sol-6.1",
        "has space",
        "",
        too_long.as_str(),
    ] {
        let words = refusal(vec![row("us.v.m-1", Some(alias))]);
        assert!(
            words.contains("alias") && words.contains(ALIAS_RULE),
            "{alias:?}: {words}"
        );
        assert!(words.contains("us.v.m-1"), "the row is named: {words}");
    }
}

#[test]
fn b862_a_name_is_used_once_and_is_no_id() {
    // The same name on two rows.
    let words = refusal(vec![
        row("us.v.m-1", Some("m-1.0")),
        row("us.v.n-1", Some("m-1.0")),
    ]);
    assert!(words.contains("m-1.0") && words.contains("two"), "{words}");
    // A name that is a model id of the list: its own row's or another's.
    for (first, second) in [("us.v.n-1", "n"), ("m", "us.v.n-1")] {
        let words = refusal(vec![
            row("us.v.m-1", Some(first)),
            row("us.v.n-1", Some(second)),
        ]);
        assert!(
            words.contains("us.v.n-1") && words.contains("model id"),
            "{words}"
        );
    }
    // A name that is the id of an alternate route, of its own row or of another.
    for (first, second) in [("global.v.m-1", "n"), ("m", "global.v.m-1")] {
        let mut with_route = row("us.v.m-1", Some(first));
        with_route["alternateRoutes"] = json!([{"id": "global.v.m-1", "region": "us-east-1", "smokeRecordId": "s6:test-record"}]);
        let words = refusal(vec![with_route, row("us.v.n-1", Some(second))]);
        assert!(
            words.contains("global.v.m-1") && words.contains("alternate route id"),
            "{words}"
        );
    }
}

#[test]
fn b862_the_refusal_of_a_name_states_the_rule() {
    // The words a person reads when a hand-edited list has a bad name: the row, the name and what a name looks like.
    let words = refusal(vec![row("us.v.m-1", Some("Sol_6.1"))]);
    assert!(
        words.contains("us.v.m-1") && words.contains("\"Sol_6.1\"") && words.contains(ALIAS_RULE),
        "{words}"
    );
    for part in [
        "1 to 24",
        "lower-case letters",
        "digits",
        "'-'",
        "'.'",
        "letter or a digit",
        "two dots",
    ] {
        assert!(ALIAS_RULE.contains(part), "{part}: {ALIAS_RULE}");
    }
}

#[test]
fn b862_the_listing_keeps_its_columns_with_a_longer_name() {
    // A list of short names prints as it always did: this is the header line of every release so far.
    let today = render_models(&builtin().list(), None);
    assert_eq!(
        today.lines().next().unwrap(),
        "  alias   model id                            name                context  notes"
    );
    // A name of nine characters widens the column for the header, for every row and for the notes.
    let mut noted = row("us.v.n-1", Some("n"));
    noted["note"] = json!("A note about n.");
    let list = parse(vec![row("us.v.m-1", Some("model-1.2")), noted])
        .unwrap()
        .list();
    let text = render_models(&list, Some(("us.v.n-1", ModelSource::Flag)));
    let lines: Vec<&str> = text.lines().collect();
    let column = |line: &str, word: &str| {
        line.find(word)
            .unwrap_or_else(|| panic!("{word:?} is not in {line:?}"))
    };
    let at = column(lines[0], "model id");
    assert_eq!(at, 2 + 9 + 1, "{text}");
    assert_eq!(
        lines[1],
        format!(
            "  {:<9} {:<35} {:<19} {:>8}  ",
            "model-1.2", "us.v.m-1", "Name of us.v.m-1", 200_000
        )
    );
    assert!(
        lines[2].starts_with(&format!("* {:<9} us.v.n-1", "n")),
        "{text}"
    );
    assert_eq!(column(lines[1], "us.v.m-1"), at, "{text}");
    assert_eq!(column(lines[2], "us.v.n-1"), at, "{text}");
    assert_eq!(
        column(lines[0], "name"),
        column(lines[2], "Name of"),
        "{text}"
    );
    let note = lines
        .iter()
        .find(|l| l.contains("A note about n."))
        .unwrap_or_else(|| panic!("the note is printed: {text}"));
    assert_eq!(column(note, "A note about n."), at, "{text}");
    assert!(note.starts_with("  n "), "{note}");
}

/// Mixed versions: a stage may list a model under a name that this client's own grammar refuses (a stage of a later release read by an earlier
/// client). The list a client reads from a stage is the wire type, which has no grammar of its own: the client reads it and prints it with the
/// table in line, and it still holds its own list to the rule.
#[test]
fn b862_a_list_from_a_stage_is_read_and_printed_whatever_its_names() {
    let wire = json!({
        "default": "us.v.n-1",
        "models": [
            {"id": "us.v.m-1", "name": "M", "vendor": "v", "alias": "Sol_6.1", "protocol": "converse", "region": "us-east-1",
             "contextTokens": 200_000, "maxOutputTokens": 8192, "selectable": true, "default": false},
            {"id": "us.v.n-1", "name": "N", "vendor": "v", "alias": "n-10.25", "protocol": "converse", "region": "us-east-1",
             "contextTokens": 200_000, "maxOutputTokens": 8192, "selectable": true, "default": true}
        ]
    });
    let list: ModelList = serde_json::from_value(wire).unwrap();
    assert_eq!(list.models[0].alias.as_deref(), Some("Sol_6.1"));
    let text = render_models(&list, None);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[1].starts_with("  Sol_6.1 us.v.m-1"), "{text}");
    assert!(lines[2].starts_with("  n-10.25 us.v.n-1"), "{text}");
    assert!(refusal(vec![row("us.v.m-1", Some("Sol_6.1"))]).contains(ALIAS_RULE));
}
