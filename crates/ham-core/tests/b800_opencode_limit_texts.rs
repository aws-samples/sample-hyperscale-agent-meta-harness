//! The shell command time limit is a setting of the second engine's file. The key table is the protocol's (one key); the
//! words of the key, the template and the reading of the file are the client's. The other keys of the first engine stay out of the second
//! engine's file.
#![allow(clippy::unwrap_used)]

use ham_core::engine_file;
use ham_core::engines;
use harness_protocol::direct::{CLINE_KEYS, OPENCODE_KEYS, keys_for};

const ENGINE: &str = "opencode";
const KEY: &str = "commands.timeout_seconds";

fn read(text: &str) -> engine_file::Reading {
    engine_file::read_text(text, ENGINE)
}

#[test]
fn b800_the_one_key_of_the_table_has_its_words_and_no_other_does() {
    let table: Vec<&str> = OPENCODE_KEYS.iter().map(|k| k.path).collect();
    let texts = engines::setting_texts(ENGINE);
    let words: Vec<&str> = texts.iter().map(|t| t.path).collect();
    assert_eq!(words, table);
    assert_eq!(words, [KEY]);
    let t = &texts[0];
    assert_eq!(t.label, "Command time limit");
    assert!(t.label.len() <= 24, "{}", t.label);
    assert!(t.help.len() > 20 && t.help.len() <= 160, "{}", t.help);
    assert_eq!(t.default_shown, "600 s");
}

#[test]
fn b800_the_help_says_how_long_a_command_may_run_and_names_the_engine_that_stops_it() {
    let help = engines::setting_texts(ENGINE)[0].help;
    assert!(
        help.contains("How long one shell command may run before OpenCode stops it"),
        "{help}"
    );
    assert!(help.contains("1 to 1800 seconds"), "{help}");
    assert!(help.contains("600"), "{help}");
    // Each engine's words name its own engine.
    assert!(!help.contains("Cline"), "{help}");
    let first = engines::setting_texts("cline")
        .iter()
        .find(|t| t.path == KEY)
        .unwrap();
    assert!(!first.help.contains("OpenCode"), "{}", first.help);
    assert_eq!(first.label, engines::setting_texts(ENGINE)[0].label);
}

#[test]
fn b800_the_other_keys_of_the_first_engine_have_no_words_for_the_second() {
    for row in CLINE_KEYS.iter().filter(|r| r.path != KEY) {
        assert!(
            !engines::setting_texts(ENGINE)
                .iter()
                .any(|t| t.path == row.path),
            "{}",
            row.path
        );
    }
    assert_eq!(keys_for(ENGINE).unwrap().len(), 1);
}

#[test]
fn b800_the_group_of_the_rows_is_named_for_the_engine() {
    assert_eq!(engines::settings_group_label(ENGINE), "OpenCode engine");
    assert!(engines::has_settings(ENGINE));
}

#[test]
fn b800_the_template_comments_out_the_limit_with_its_range() {
    let text = engines::settings_template(ENGINE).unwrap();
    assert!(text.lines().any(|l| l == "schema = 1"), "{text}");
    assert!(text.lines().any(|l| l == "# [commands]"), "{text}");
    let line = text
        .lines()
        .find(|l| l.starts_with("# timeout_seconds = "))
        .unwrap_or_else(|| panic!("the limit is not commented out in:\n{text}"));
    assert!(
        line.starts_with("# timeout_seconds = 600") && line.contains("from 1 to 1800"),
        "{line}"
    );
    assert!(text.contains("OpenCode"), "{text}");
    // Nothing is switched on: the file sets nothing until the person removes a #.
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        assert!(l.starts_with('#') || l == "schema = 1", "{l:?}");
    }
    // The Settings screen and `show` word a stored value with its unit.
    assert_eq!(engines::shown_value(KEY, "900"), "900 s");
}

#[test]
fn b800_a_file_with_the_limit_is_a_member_of_the_second_engine() {
    let r =
        read("# keep this\nschema = 1\n\n[commands]\ntimeout_seconds = 900   # fifteen minutes\n");
    assert!(r.ok(), "{:?}", r.problems);
    let c = r.config.unwrap();
    assert_eq!(c.engine, "opencode");
    assert_eq!(c.get(KEY), Some(&serde_json::json!(900)));
    assert_eq!(
        serde_json::to_value(&c).unwrap(),
        serde_json::json!({"engine": "opencode", "schema": 1, "settings": {"commands": {"timeout_seconds": 900}}})
    );
    // Comments never leave the computer.
    assert!(!serde_json::to_string(&c).unwrap().contains("keep this"));
    assert_eq!(
        read("schema = 1\n[commands]\ntimeout_seconds = 900\n").line_of(KEY),
        Some(3)
    );
}

#[test]
fn b800_a_file_with_only_the_schema_line_still_sends_nothing() {
    for text in ["", "schema = 1\n", "schema = 1\n[commands]\n"] {
        let r = read(text);
        assert!(r.ok() && r.config.is_none(), "{text:?}: {:?}", r.problems);
    }
}

#[test]
fn b800_the_range_is_named_with_the_line_and_the_value_is_not_echoed() {
    for bad in ["0", "1801", "99999"] {
        let r = read(&format!(
            "schema = 1\n[commands]\ntimeout_seconds = {bad}\n"
        ));
        assert!(!r.ok(), "{bad}");
        let p = &r.problems[0];
        assert_eq!(
            (p.code.as_str(), p.line),
            ("out_of_range", Some(3)),
            "{bad}"
        );
        assert!(
            p.message.contains("a whole number from 1 to 1800"),
            "{}",
            p.message
        );
        // The message holds the range (1 and 1800) but never the value that was typed.
        assert!(
            !p.message.contains("1801") && !p.message.contains("99999"),
            "{}",
            p.message
        );
    }
    let r = read("schema = 1\n[commands]\ntimeout_seconds = \"900\"\n");
    assert_eq!(r.problems[0].code, "bad_type");
}

#[test]
fn b800_a_key_of_the_first_engine_is_not_an_engine_setting_of_the_second() {
    for text in [
        "schema = 1\n[reasoning]\neffort = \"low\"\n",
        "schema = 1\n[context]\ncompaction = \"off\"\n",
        "schema = 1\n[tools]\ndisable = [\"spawn_agent\"]\n",
    ] {
        let r = read(text);
        assert!(!r.ok(), "{text:?}");
        assert_eq!(r.problems[0].code, "unknown_key", "{text:?}");
    }
}
