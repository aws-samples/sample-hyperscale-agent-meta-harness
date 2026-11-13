//! `ham_core::config_write`: what `ham config set` and `unset` do to the TEXT of `config.toml`, with no file involved. The module turns a
//! registry key and a value into edits of the comment-keeping patcher, in the spelling the file already uses, and says what is still wrong
//! with the file afterwards. A file that is not TOML, or that a newer `ham` wrote, is never changed.
#![allow(clippy::unwrap_used)]

use ham_core::config::{ConfigError, KEYS, get_key, parse_config, read_config};
use ham_core::config_write::{PlanError, door_note, plan_set, plan_unset};

const OPUS: &str = "us.anthropic.claude-opus-5-5";
const ASTRA: &str = "us.openai.gpt-6-astra";
const SOL: &str = "us.openai.gpt-6.1-sol";

/// A file with comments and no key.
const BARE: &str = "# ham settings\n# keep this note\n";

/// The owner's file: three keys, two comment lines on top and a comment after a value.
const OWNER: &str = "# ham settings\n# keep this note\napiEndpoint = \"https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\"\nregion = \"us-east-1\"\nawsProfile = \"ham-test\" # the profile\n";

fn set_text(text: &str, key: &str, value: &str) -> String {
    plan_set(text, key, value)
        .unwrap_or_else(|e| panic!("set {key} {value}: {e}"))
        .text
        .unwrap_or_else(|| panic!("set {key} {value} planned no change"))
}

fn unset_text(text: &str, key: &str) -> String {
    plan_unset(text, key)
        .unwrap_or_else(|e| panic!("unset {key}: {e}"))
        .text
        .unwrap_or_else(|| panic!("unset {key} planned no change"))
}

#[test]
fn a_set_keeps_comments_order_and_every_other_byte() {
    let plan = plan_set(OWNER, "default_model", "opus").unwrap();
    assert_eq!(plan.name, "default_model");
    assert_eq!(plan.stored.as_deref(), Some(OPUS));
    assert_eq!(
        plan.text.as_deref(),
        Some(format!("{OWNER}defaultModel = \"{OPUS}\"\n").as_str())
    );
    assert!(!plan.opens_the_door && plan.still_wrong.is_empty());
    // The same value written by the other spelling of the key is the same file.
    assert_eq!(set_text(OWNER, "defaultModel", "opus"), plan.text.unwrap());
}

#[test]
fn a_set_of_the_value_a_key_already_has_plans_no_text() {
    let plan = plan_set(OWNER, "region", "us-east-1").unwrap();
    assert_eq!(plan.text, None);
    assert_eq!(plan.stored.as_deref(), Some("us-east-1"));
    // The model by alias or by id: once stored, the id is the value.
    let with_model = set_text(OWNER, "default_model", "opus");
    assert_eq!(
        plan_set(&with_model, "default_model", OPUS).unwrap().text,
        None
    );
    assert_eq!(
        plan_set(&with_model, "default_model", "Opus").unwrap().text,
        None
    );
    // Clearing a key that is not there changes nothing either.
    assert_eq!(plan_unset(OWNER, "default_model").unwrap().text, None);
}

#[test]
fn a_fresh_machine_gets_exactly_the_one_line_it_got_before() {
    assert_eq!(
        set_text("", "default_model", "astra"),
        format!("defaultModel = \"{ASTRA}\"\n")
    );
    // The connection keys land in struct order whatever order they are set in.
    let mut text = String::new();
    for (key, value) in [
        ("aws_profile", "p"),
        ("api_endpoint", "https://a.example/v1"),
        ("region", "us-east-1"),
    ] {
        text = set_text(&text, key, value);
    }
    assert_eq!(
        text,
        "apiEndpoint = \"https://a.example/v1\"\nregion = \"us-east-1\"\nawsProfile = \"p\"\n"
    );
}

#[test]
fn the_first_new_key_opens_the_door_once_and_unset_returns_the_original_bytes() {
    let plan = plan_set(OWNER, "run.goal", "off").unwrap();
    assert!(plan.opens_the_door, "the first table is the one-way door");
    assert_eq!(plan.name, "run.goal");
    assert_eq!(plan.stored.as_deref(), Some("false"));
    let with_run = plan.text.unwrap();
    assert_eq!(
        with_run,
        format!("{OWNER}configVersion = 1\n\n[run]\ngoal = false\n")
    );
    assert!(parse_config(&with_run).is_ok());
    // A second new key, in a table or in a new one, crosses no door.
    let second = plan_set(&with_run, "research.enabled", "on").unwrap();
    assert!(!second.opens_the_door);
    let third = plan_set(&with_run, "run.rules", "off").unwrap();
    assert!(!third.opens_the_door);
    assert!(
        third
            .text
            .unwrap()
            .contains("[run]\ngoal = false\nrules = false\n")
    );
    // A legacy key never opens it.
    assert!(!plan_set(OWNER, "mouse", "auto").unwrap().opens_the_door);
    // Unset removes the key, the empty table, the blank line before it and `configVersion`: the file is the one it was.
    let back = plan_unset(&with_run, "run.goal").unwrap();
    assert_eq!(back.text.as_deref(), Some(OWNER));
    assert_eq!(back.stored, None);
    assert!(!back.opens_the_door);
}

#[test]
fn every_key_sets_and_unsets_back_to_the_bytes_before_it() {
    let values: [(&str, &str); 46] = [
        (
            "api_endpoint",
            "https://x.execute-api.us-east-1.amazonaws.com/direct",
        ),
        ("region", "us-west-2"),
        ("aws_profile", "other"),
        ("default_repo", "cc:my-service"),
        ("download_dir", "/downloads"),
        ("default_model", "sol"),
        ("input_wait_minutes", "7"),
        ("mouse", "auto"),
        ("report_tests", "off"),
        ("run.goal", "off"),
        ("run.rules", "no"),
        ("run.skills", "0"),
        ("research.enabled", "on"),
        ("ui.notify", "yes"),
        ("ui.bell", "false"),
        ("ui.mascot", "ascii"),
        ("run.report_command_minutes", "6"),
        ("run.stall_calls", "45"),
        ("run.stall_window_minutes", "30"),
        ("run.stall_min_calls", "12"),
        ("run.command_grace_minutes", "90"),
        ("run.final_message", "no"),
        ("run.lifecycle_flow", "quick"),
        ("run.review_round", "off"),
        ("run.compute", "instances"),
        ("run.stall_new_calls", "250"),
        ("run.cargo_no_debug_info", "off"),
        ("run.cargo_no_incremental", "no"),
        ("run.exclude_target_dir", "0"),
        ("run.outside_checkout_note", "off"),
        ("ui.deprecation_notices", "off"),
        ("ui.mascot_motion", "off"),
        ("run.max_run_hours", "48"),
        ("run.provider_sharing", "on"),
        ("default_backend", "default"),
        ("customer_content", "on"),
        ("ui.copy_limit_kib", "256"),
        ("run.max_interrupts", "20"),
        ("run.max_questions", "5"),
        ("run.max_fresh_sessions", "6"),
        ("research.max_file_mib", "4"),
        ("research.max_unpacked_mib", "500"),
        ("research.max_run_mib", "900"),
        ("run.container_signals", "none"),
        ("default_engine", "opencode"),
        ("update_source", "https://example.com/me/fork.git"),
    ];
    assert_eq!(values.len(), KEYS.len());
    // Each key alone, on a commented file.
    for (key, value) in values {
        let set = set_text(BARE, key, value);
        assert_eq!(unset_text(&set, key), BARE, "{key}");
    }
    // All of them at once, then back in the opposite order.
    let mut text = BARE.to_string();
    for (key, value) in values {
        text = set_text(&text, key, value);
    }
    let cfg = parse_config(&text).unwrap();
    for (key, value) in values {
        let want = if key == "default_model" {
            SOL.to_string()
        } else if key == "run.review_round" {
            // A word setting (`auto` or `off`) reads back as the word, not as a boolean.
            value.to_string()
        } else if value == "off" || value == "no" || value == "0" || value == "false" {
            "false".to_string()
        } else if value == "on" || value == "yes" {
            "true".to_string()
        } else {
            value.to_string()
        };
        assert_eq!(get_key(&cfg, key).unwrap(), Some(want), "{key}");
    }
    for (key, _) in values.iter().rev() {
        text = unset_text(&text, key);
    }
    assert_eq!(
        text, BARE,
        "fifteen sets and fifteen unsets leave the file as it was"
    );
}

#[test]
fn a_hand_written_snake_spelling_is_updated_in_place_and_no_second_spelling_appears() {
    let file = format!("# model\ndefault_model = \"{SOL}\"\nregion = \"us-east-1\"\n");
    let next = set_text(&file, "default_model", "opus");
    assert_eq!(
        next,
        format!("# model\ndefault_model = \"{OPUS}\"\nregion = \"us-east-1\"\n")
    );
    assert!(!next.contains("defaultModel"));
    assert_eq!(
        unset_text(&next, "defaultModel"),
        "# model\nregion = \"us-east-1\"\n"
    );
    // The two other keys that always had a snake spelling.
    let file = "input_wait_minutes = 5\nreport_tests = false\n";
    assert_eq!(
        set_text(file, "inputWaitMinutes", "9"),
        "input_wait_minutes = 9\nreport_tests = false\n"
    );
    assert_eq!(
        set_text(file, "report_tests", "on"),
        "input_wait_minutes = 5\nreport_tests = true\n"
    );
}

#[test]
fn a_crlf_file_keeps_its_line_endings() {
    // A new key goes right after the last assignment, so the note below it stays below.
    let file = "region = \"us-east-1\"\r\n# note\r\n";
    let next = set_text(file, "default_model", "opus");
    assert_eq!(
        next,
        format!("region = \"us-east-1\"\r\ndefaultModel = \"{OPUS}\"\r\n# note\r\n")
    );
    // The first table, in the file's own line endings, and back.
    let with_table = set_text(file, "run.goal", "off");
    assert_eq!(
        with_table,
        "region = \"us-east-1\"\r\nconfigVersion = 1\r\n# note\r\n\r\n[run]\r\ngoal = false\r\n"
    );
    assert_eq!(unset_text(&with_table, "run.goal"), file);
}

#[test]
fn a_set_patches_around_a_bad_key_and_says_what_is_still_wrong() {
    let plan = plan_set("surprise = 1\n", "default_model", "opus").unwrap();
    assert_eq!(
        plan.text.as_deref(),
        Some(format!("surprise = 1\ndefaultModel = \"{OPUS}\"\n").as_str())
    );
    assert_eq!(plan.still_wrong.len(), 1, "{:?}", plan.still_wrong);
    assert!(
        plan.still_wrong[0].contains("line 1") && plan.still_wrong[0].contains("surprise"),
        "{:?}",
        plan.still_wrong
    );
    // A typo inside a table the user has: the line it is on is named, the other lines are untouched.
    let file = "configVersion = 1\n\n[run]\ngoall = true\n";
    let plan = plan_set(file, "mouse", "off").unwrap();
    let next = plan.text.unwrap();
    assert!(next.contains("[run]\ngoall = true\n"), "{next}");
    // `mouse` goes before `configVersion`, so the typo moved down one line, and the finding names the new line.
    assert!(
        next.starts_with("mouse = false\nconfigVersion = 1\n"),
        "{next}"
    );
    assert!(plan.still_wrong[0].contains("line 5") && plan.still_wrong[0].contains("goall"));
}

#[test]
fn a_wrong_value_is_replaced_by_set_and_removed_by_unset_and_nothing_else_moves() {
    let file = "# keep\nregion = \"us-east-1\"\nmouse = \"maybe\" # oops\n";
    let read = read_config(file);
    assert!(read.has_errors());
    let set = plan_set(file, "mouse", "off").unwrap();
    assert_eq!(
        set.text.as_deref(),
        Some("# keep\nregion = \"us-east-1\"\nmouse = false # oops\n")
    );
    assert!(set.still_wrong.is_empty(), "{:?}", set.still_wrong);
    // The value the reader could not store is not "unset": unset still removes exactly that line.
    let unset = plan_unset(file, "mouse").unwrap();
    assert_eq!(
        unset.text.as_deref(),
        Some("# keep\nregion = \"us-east-1\"\n")
    );
    assert!(unset.still_wrong.is_empty());
}

#[test]
fn a_file_that_is_not_toml_is_refused_with_its_line_and_never_changed() {
    for text in [
        "region = \n",
        "this is not toml {\n",
        "region = \"us-east-1\"\nmouse = maybe\n",
        "\u{feff}region = \"us-east-1\"\n",
        "region = \"a\0b\"\n",
    ] {
        for plan in [
            plan_set(text, "default_model", "opus"),
            plan_unset(text, "region"),
        ] {
            match plan {
                Err(PlanError::Unreadable(why)) => {
                    assert!(!why.is_empty() && !why.contains("maybe"), "{why}");
                }
                other => panic!("{text:?}: expected a refusal, got {other:?}"),
            }
        }
    }
    let Err(PlanError::Unreadable(why)) = plan_set("a = 1\nb = \n", "region", "x") else {
        panic!("a refusal");
    };
    assert!(why.contains("line 2"), "{why}");
}

#[test]
fn a_file_from_a_newer_ham_is_never_changed() {
    let file = "configVersion = 2\nregion = \"us-east-1\"\n[ui]\nfuture = 1\n";
    for plan in [
        plan_set(file, "region", "us-west-2"),
        plan_unset(file, "region"),
    ] {
        match plan {
            Err(PlanError::Config(ConfigError::Newer { version: 2 })) => {}
            other => panic!("expected the newer-version refusal, got {other:?}"),
        }
    }
}

#[test]
fn a_bad_key_a_bad_value_and_a_hostile_value_change_nothing() {
    let Err(PlanError::Config(ConfigError::UnknownKey { key, known })) =
        plan_set(OWNER, "nokey", "v")
    else {
        panic!("an unknown key");
    };
    assert_eq!(key, "nokey");
    assert!(known.contains(&"run.goal".to_string()) && known.len() == 46);
    assert!(matches!(
        plan_unset(OWNER, "nokey"),
        Err(PlanError::Config(ConfigError::UnknownKey { .. }))
    ));
    for (key, value, needle) in [
        ("mouse", "maybe", "true, false or auto"),
        ("input_wait_minutes", "43201", "0 (do not wait) to 43200"),
        ("run.goal", "maybe", "takes on or off"),
        ("default_model", "gpt-9", "default_model"),
        ("default_repo", "cc:x\n[evil]", "no control characters"),
    ] {
        match plan_set(OWNER, key, value) {
            Err(PlanError::Config(e)) => assert!(e.to_string().contains(needle), "{key}: {e}"),
            other => panic!("{key} {value:?}: {other:?}"),
        }
    }
}

#[test]
fn the_door_note_names_the_version_the_key_the_table_and_the_way_back() {
    let note = door_note("4.07", "run.goal");
    assert!(
        note.starts_with("ham versions before v4.07 refuse this file."),
        "{note}"
    );
    assert!(note.contains("`ham config unset run.goal`"), "{note}");
    assert!(note.contains("`[run]` and `configVersion`"), "{note}");
    assert!(door_note("4.07", "ui.bell").contains("`[ui]`"));
    assert!(door_note("4.07", "research.enabled").contains("`[research]`"));
}
