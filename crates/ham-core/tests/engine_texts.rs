//! The words of the engine settings. The key table is a Rust const in
//! `harness-protocol` (names, types, ranges); the labels, help text and the template are `ham-core`'s, in `engines.rs`, the one module that
//! names an engine. A parity test fails when the two disagree, and the "Locked by the harness" texts of the client are the protocol's own table.
#![allow(clippy::unwrap_used)]

use ham_core::engines;
use ham_core::settings::LOCKED;
use harness_protocol::direct::{CLINE_KEYS, DENIED, KeyKind, keys_for};

const ENGINE: &str = "cline";

#[test]
fn every_key_of_the_table_has_a_label_and_help_and_no_other_does() {
    let table: Vec<&str> = CLINE_KEYS.iter().map(|k| k.path).collect();
    let texts = engines::setting_texts(ENGINE);
    let words: Vec<&str> = texts.iter().map(|t| t.path).collect();
    assert_eq!(words, table, "the labels follow the table, in its order");
    for t in texts {
        assert!(
            !t.label.is_empty() && t.label.len() <= 24,
            "{}: label {:?}",
            t.path,
            t.label
        );
        assert!(
            t.help.len() > 20 && t.help.len() <= 160,
            "{}: help {:?}",
            t.path,
            t.help
        );
        assert!(!t.default_shown.is_empty(), "{}", t.path);
    }
    // An engine without a table has no texts.
    for other in ["pi", "codex", "kiro", "nope"] {
        assert!(engines::setting_texts(other).is_empty(), "{other}");
        assert!(!engines::has_settings(other), "{other}");
    }
    // OpenCode has a table with one key: its settings file exists and that key has a text. The words of the
    // key are pinned in b800_opencode_limit_texts.rs.
    assert!(engines::has_settings("opencode"));
    let open: Vec<&str> = keys_for("opencode")
        .unwrap()
        .iter()
        .map(|k| k.path)
        .collect();
    let open_words: Vec<&str> = engines::setting_texts("opencode")
        .iter()
        .map(|t| t.path)
        .collect();
    assert_eq!(open, ["commands.timeout_seconds"]);
    assert_eq!(open_words, open);
    assert!(engines::has_settings(ENGINE));
    assert!(keys_for(ENGINE).is_some());
}

#[test]
fn the_reasoning_default_reads_model_default_and_not_off() {
    let t = engines::setting_texts(ENGINE)
        .iter()
        .find(|t| t.path == "reasoning.effort")
        .unwrap();
    assert_eq!(t.default_shown, "model default");
    assert!(engines::shown_value("reasoning.effort", "default").contains("model default"));
    assert_eq!(
        engines::shown_value("commands.timeout_seconds", "900"),
        "900 s"
    );
    assert_eq!(
        engines::shown_value("context.compaction_trigger", "0.9"),
        "0.90"
    );
    assert_eq!(
        engines::shown_value("tools.disable", "spawn_agent,fetch_web_content"),
        "spawn_agent, fetch_web_content"
    );
    assert_eq!(engines::shown_value("tools.disable", ""), "none");
}

#[test]
fn the_template_comments_out_every_key_with_its_range() {
    let text = engines::settings_template(ENGINE).unwrap();
    assert!(text.lines().any(|l| l == "schema = 1"));
    for row in CLINE_KEYS {
        let (table, key) = row.path.split_once('.').unwrap();
        assert!(
            text.lines()
                .any(|l| l.trim_start_matches("# ").trim() == format!("[{table}]")),
            "{table} is not in the template"
        );
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("# {key} = ")))
            .unwrap_or_else(|| panic!("{} is not commented out in:\n{text}", row.path));
        // The range or the set is on the line.
        match row.kind {
            KeyKind::Choice(set) | KeyKind::Set(set) => {
                for m in set {
                    assert!(line.contains(m), "{line}");
                }
            }
            KeyKind::Int { min, max } => {
                assert!(
                    line.contains(&format!("{min}")) && line.contains(&format!("{max}")),
                    "{line}"
                );
            }
            KeyKind::Float { min, max } => {
                assert!(
                    line.contains(&format!("{min:.2}")) && line.contains(&format!("{max:.2}")),
                    "{line}"
                );
            }
        }
    }
    // `none`, `minimal` and `max` are never offered; `off` and `xhigh` are.
    for held_back in ["\"none\"", "\"minimal\"", "\"max\""] {
        assert!(!text.contains(held_back), "{held_back} in the template");
    }
    let effort = text.lines().find(|l| l.starts_with("# effort = ")).unwrap();
    assert!(
        effort.contains("default, off, low, medium, high, xhigh"),
        "{effort}"
    );
    // Only an engine with a table has a template.
    assert!(engines::settings_template("pi").is_none());
}

#[test]
fn the_group_and_the_missing_schema_text_name_the_engine_through_the_module() {
    assert_eq!(engines::settings_group_label(ENGINE), "Cline engine");
    assert_eq!(
        engines::NO_SETTINGS_SCHEMA,
        "no settings schema in this build yet"
    );
    assert_eq!(engines::find("pi").unwrap().name, "Pi");
    assert!(engines::find("nope").is_none());
}

#[test]
fn the_locked_classes_of_the_client_are_the_denied_table_of_the_protocol() {
    assert_eq!(LOCKED.len(), DENIED.len());
    for (client, table) in LOCKED.iter().zip(DENIED) {
        assert_eq!(client.label, table.label, "{}", table.id);
        assert_eq!(client.reason, table.reason, "{}", table.id);
        assert_eq!(client.protects, table.protects, "{}", table.id);
        assert!(!client.name.is_empty());
    }
}
