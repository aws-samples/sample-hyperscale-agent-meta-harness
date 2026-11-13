//! The files of the goal loop: the engine's spec, the settings, the state file the plugin reads and
//! writes, the rounds log the observer writes, and the placing of the two plugin folders under the engine's configuration directory.
//! Real files in a scratch directory; no engine and no run.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::goal::files::place_plugins;
use agentd::direct::goal::rounds::{RoundLine, RoundsReader, blocked_line, last_line, parse_line};
use agentd::direct::goal::settings::{DEFAULT_GOAL_TEXT, GoalSettings};
use agentd::direct::goal::state::{
    GoalState, cleared_state, parse_state, pending_state, read_state, write_state,
};
use agentd::engine::goal::goal_engine;
use common::scratch;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---- the engine's spec -------------------------------------------------------------------------------------------------------------

#[test]
fn the_engine_spec_names_the_plugin_folders_the_mode_and_the_files_and_only_the_cline_profile_has_a_plugin()
 {
    let g = goal_engine("cline").unwrap();
    assert_eq!(g.plugins, ["goal", "harness-rounds"]);
    assert_eq!(g.env, [("CLINE_PLUGIN_MODE", "in_process")]);
    assert_eq!(g.state_file, "plugins/goal/goals.json");
    assert_eq!(g.rounds_file, "plugins/harness-rounds/rounds.jsonl");
    assert_eq!(g.completion_tool, "mark_goal_complete");
    assert!(goal_engine("fake-acp").is_none());
    // OpenCode has a goal loop the harness drives: no plugin folder, no file, no mode
    let o = goal_engine("opencode").unwrap();
    assert!(o.plugins.is_empty() && o.env.is_empty() && o.state_file.is_empty());
    assert!(goal_engine("").is_none());
}

// ---- the settings ------------------------------------------------------------------------------------------------------------------

fn settings(pairs: &[(&str, &str)]) -> GoalSettings {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    GoalSettings::from_lookup(&|k| map.get(k).cloned())
}

#[test]
fn the_goal_loop_is_on_by_default_with_the_decision_records_numbers() {
    let s = settings(&[]);
    assert!(s.enabled);
    assert_eq!(s.plugin_dir, Path::new("/opt/harness/engine/plugins"));
    assert_eq!(s.goal_text, DEFAULT_GOAL_TEXT);
    let l = s.limits();
    assert_eq!(
        (l.blocked_rounds, l.stall_rounds, l.idle_rounds, l.kicks_max),
        (2, 3, 5, 2)
    );
    assert_eq!(l.kick_after, Duration::from_secs(15));
    assert_eq!(s.drain, Duration::from_secs(120));
    assert_eq!(s.claim_wait, Duration::from_secs(3));
    assert_eq!(s.poll, Duration::from_secs(1));
}

#[test]
fn off_switches_the_goal_loop_off_in_the_ways_a_person_writes_it() {
    for v in ["off", "OFF", "0", "false", "no", "disabled"] {
        assert!(!settings(&[("AGENTD_GOAL", v)]).enabled, "{v}");
    }
    for v in ["on", "1", "true", "", "anything else"] {
        assert!(settings(&[("AGENTD_GOAL", v)]).enabled, "{v}");
    }
}

#[test]
fn a_setting_that_does_not_parse_is_the_default_and_one_out_of_range_is_cut_to_its_bound() {
    let s = settings(&[
        ("AGENTD_GOAL_STALL_ROUNDS", "many"),
        ("AGENTD_GOAL_IDLE_ROUNDS", "100000"),
        ("AGENTD_GOAL_BLOCKED_ROUNDS", "0"),
        ("AGENTD_GOAL_POLL_MS", "1"),
        ("AGENTD_GOAL_DRAIN_S", "99999"),
        ("AGENTD_GOAL_KICKS_MAX", "0"),
    ]);
    let l = s.limits();
    assert_eq!(l.stall_rounds, 3);
    assert_eq!(l.idle_rounds, 50);
    assert_eq!(l.blocked_rounds, 1);
    assert_eq!(l.kicks_max, 0);
    assert_eq!(s.poll, Duration::from_millis(10));
    assert_eq!(s.drain, Duration::from_secs(600));
}

#[test]
fn a_text_of_the_goal_and_a_plugin_directory_can_be_named() {
    let s = settings(&[
        ("AGENTD_GOAL_TEXT", "Make the tests pass."),
        ("AGENTD_GOAL_PLUGIN_DIR", "/somewhere/plugins"),
    ]);
    assert_eq!(s.goal_text, "Make the tests pass.");
    assert_eq!(s.plugin_dir, Path::new("/somewhere/plugins"));
    // blank text is the default; text over the bound is cut at a character boundary
    assert_eq!(
        settings(&[("AGENTD_GOAL_TEXT", "  ")]).goal_text,
        DEFAULT_GOAL_TEXT
    );
    let long = "é".repeat(5000);
    let cut = settings(&[("AGENTD_GOAL_TEXT", &long)]).goal_text;
    assert!(
        cut.len() <= 2000 && cut.chars().all(|c| c == 'é'),
        "{}",
        cut.len()
    );
}

#[test]
fn the_default_goal_text_names_the_goal_card_the_blocked_line_and_no_claim_word() {
    let t = DEFAULT_GOAL_TEXT;
    assert!(t.len() < 700, "{}", t.len());
    assert!(t.contains(".ham/lifecycle/GOAL.md"));
    // The card is not in docs/ unless the repository says so
    assert!(!t.contains("docs/lifecycle"), "{t}");
    // the card can sit elsewhere: the text names the file that says where
    assert!(t.contains(".ham/records"), "{t}");
    assert!(t.contains("BLOCKED:"));
    for banned in ["verified", "tested", "gated", "evaluated"] {
        assert!(!t.to_lowercase().contains(banned), "{banned}");
    }
}

const OUTSIDE_WORK: &str =
    "Work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest.";

#[test]
fn the_default_goal_text_says_that_work_outside_the_checkout_is_reported_and_not_a_blocker() {
    let t = DEFAULT_GOAL_TEXT;
    assert!(t.contains(OUTSIDE_WORK), "{t}");
    // the BLOCKED line stays, for work inside the checkout that cannot go on
    assert!(t.contains("BLOCKED:"), "{t}");
    // and the text stays short: the plugin quotes it in every follow-up prompt
    assert!(t.len() < 700, "{}", t.len());
}

#[test]
fn the_continue_prompt_says_the_same_and_stays_short() {
    let k = agentd::direct::goal::KICK_PROMPT;
    assert!(k.contains(OUTSIDE_WORK), "{k}");
    assert!(k.contains("BLOCKED:"), "{k}");
    assert!(k.len() < 400, "{}", k.len());
}

#[test]
fn the_default_goal_text_names_the_moved_card_by_its_whole_path() {
    // With `.ham/records` naming a folder the card is `<folder>/lifecycle/GOAL.md`, because the folder takes the place of `.ham`. The text said "GOAL.md in the
    // folder that .ham/records names", which points at `<folder>/GOAL.md`, a file the pack never writes.
    let t = DEFAULT_GOAL_TEXT;
    assert!(
        t.contains("or the same path with the folder that .ham/records names in place of .ham"),
        "{t}"
    );
    assert!(!t.contains("GOAL.md in the folder"), "{t}");
}

// ---- the state file -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_pending_state_is_what_the_plugin_reads_and_the_goal_text_survives_the_json() {
    let goal = "Fix \"it\"\nthen \\ stop";
    let doc: Value =
        serde_json::from_str(&pending_state(goal, "2026-10-16T10:00:00.000Z")).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["pendingGoal"]["goal"], goal);
    assert_eq!(doc["pendingGoal"]["createdAt"], "2026-10-16T10:00:00.000Z");
    assert_eq!(doc["activeGoals"], serde_json::json!({}));
    assert_eq!(
        parse_state(pending_state(goal, "x").as_bytes()),
        GoalState::Seen {
            pending: true,
            active: 0
        }
    );
}

#[test]
fn the_cleared_state_has_no_pending_and_no_active_goal() {
    let doc: Value = serde_json::from_str(&cleared_state()).unwrap();
    assert_eq!(doc, serde_json::json!({"version": 1, "activeGoals": {}}));
    assert_eq!(
        parse_state(cleared_state().as_bytes()),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
}

#[test]
fn a_claimed_goal_is_active_and_a_completed_one_is_not() {
    let claimed = br#"{"version":1,"activeGoals":{"sess-1":{"goal":"g","createdAt":"t","sessionId":"sess-1","awaitingVerification":true}}}"#;
    assert_eq!(
        parse_state(claimed),
        GoalState::Seen {
            pending: false,
            active: 1
        }
    );
    let done = br#"{"version":1,"activeGoals":{"sess-1":{"goal":"g","createdAt":"t","completedAt":"t2"}}}"#;
    assert_eq!(
        parse_state(done),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
    let both = br#"{"version":1,"pendingGoal":{"goal":"g","createdAt":"t"},"activeGoals":{"a":{"goal":"g"},"b":{"goal":"h"}}}"#;
    assert_eq!(
        parse_state(both),
        GoalState::Seen {
            pending: true,
            active: 2
        }
    );
}

#[test]
fn a_state_that_does_not_parse_is_damaged_never_empty() {
    for bad in [
        &b""[..],
        b"not json",
        b"[1,2]",
        b"{\"version\":1,",
        b"\"x\"",
    ] {
        assert_eq!(
            parse_state(bad),
            GoalState::Damaged,
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
}

#[test]
fn reading_a_missing_file_a_link_or_a_directory_is_not_a_state() {
    let d = scratch("goal-state-read");
    assert_eq!(read_state(&d.join("none.json")), GoalState::Missing);
    std::fs::write(d.join("real.json"), cleared_state()).unwrap();
    std::os::unix::fs::symlink(d.join("real.json"), d.join("link.json")).unwrap();
    assert_eq!(
        read_state(&d.join("link.json")),
        GoalState::Missing,
        "a link is never followed"
    );
    std::fs::create_dir(d.join("dir.json")).unwrap();
    assert_eq!(read_state(&d.join("dir.json")), GoalState::Missing);
    assert_eq!(
        read_state(&d.join("real.json")),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
}

#[test]
fn writing_the_state_replaces_the_file_whole_and_leaves_no_temporary_file() {
    let d = scratch("goal-state-write");
    let path = d.join("plugins").join("goal").join("goals.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_state(&path, &pending_state("g", "t"), None).unwrap();
    assert_eq!(
        parse_state(&std::fs::read(&path).unwrap()),
        GoalState::Seen {
            pending: true,
            active: 0
        }
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    write_state(&path, &cleared_state(), None).unwrap();
    assert_eq!(
        parse_state(&std::fs::read(&path).unwrap()),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["goals.json"]);
}

#[test]
fn writing_over_a_link_replaces_the_link_and_not_what_it_points_at() {
    let d = scratch("goal-state-link");
    let other_file = d.join("other_file.txt");
    std::fs::write(&other_file, "keep me").unwrap();
    let path = d.join("goals.json");
    std::os::unix::fs::symlink(&other_file, &path).unwrap();
    write_state(&path, &cleared_state(), None).unwrap();
    assert_eq!(std::fs::read_to_string(&other_file).unwrap(), "keep me");
    assert_eq!(
        parse_state(&std::fs::read(&path).unwrap()),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    );
}

// ---- the rounds log -----------------------------------------------------------------------------------------------------------------

#[test]
fn a_start_line_and_an_end_line_parse_with_every_field_the_supervisor_reads() {
    match parse_line(r#"{"ev":"start","n":3,"t":1760000000000}"#).unwrap() {
        RoundLine::Start { n } => assert_eq!(n, 3),
        other => panic!("{other:?}"),
    }
    let line = r#"{"ev":"end","n":3,"t":1,"runId":"r","status":"completed","iterations":4,"inputTokens":9,"outputTokens":2,"text":"all\ndone"}"#;
    match parse_line(line).unwrap() {
        RoundLine::End(e) => {
            assert_eq!(
                (e.n, e.status.as_str(), e.iterations, e.text.as_str()),
                (3, "completed", 4, "all\ndone")
            );
            assert_eq!(e.error, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_end_line_with_missing_fields_still_reads_and_an_error_is_one_short_line() {
    match parse_line(r#"{"ev":"end","n":1,"error":"boom\nsecond line"}"#).unwrap() {
        RoundLine::End(e) => {
            assert_eq!(
                (e.status.as_str(), e.iterations, e.text.as_str()),
                ("unknown", 0, "")
            );
            assert_eq!(e.error.as_deref(), Some("boom second line"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn text_beyond_the_bound_is_cut_and_a_line_that_is_not_a_round_line_is_nothing() {
    let big = format!(
        r#"{{"ev":"end","n":1,"status":"completed","text":"{}"}}"#,
        "x".repeat(10_000)
    );
    match parse_line(&big).unwrap() {
        RoundLine::End(e) => assert_eq!(e.text.chars().count(), 2000),
        other => panic!("{other:?}"),
    }
    for bad in [
        "",
        "garbage",
        "[]",
        r#"{"ev":"middle","n":1}"#,
        r#"{"ev":"start"}"#,
        r#"{"ev":"start","n":"one"}"#,
        r#"{"n":1}"#,
    ] {
        assert!(parse_line(bad).is_none(), "{bad}");
    }
}

#[test]
fn the_last_line_of_a_text_is_its_last_non_empty_line_trimmed() {
    assert_eq!(last_line("a\nb\n\n  \n"), Some("b"));
    assert_eq!(last_line("  only  "), Some("only"));
    assert_eq!(last_line(""), None);
    assert_eq!(last_line(" \n \n"), None);
}

#[test]
fn a_blocked_line_starts_the_last_line_and_compares_without_case_or_edge_spaces() {
    let b = blocked_line("Tried.\n  BLOCKED: Need the API key  \n").unwrap();
    assert_eq!(b.shown, "BLOCKED: Need the API key");
    assert_eq!(b.key, "blocked: need the api key");
    assert!(blocked_line("BLOCKED: need the key\nmore").is_none());
    assert!(blocked_line("not BLOCKED: here").is_none());
    assert!(blocked_line("blocked: lower case start").is_none());
    assert!(blocked_line("").is_none());
    // a very long reason is kept whole for the key and cut for the words shown
    let long = format!("BLOCKED: {}", "w".repeat(1000));
    let b = blocked_line(&long).unwrap();
    assert!(
        b.shown.chars().count() <= 120,
        "{}",
        b.shown.chars().count()
    );
    assert_eq!(b.key.len(), long.len());
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

#[test]
fn the_reader_returns_only_new_lines_and_keeps_a_half_written_line_for_the_next_read() {
    let d = scratch("goal-rounds-read");
    let path = d.join("rounds.jsonl");
    let mut r = RoundsReader::new(path.clone());
    let p = r.poll();
    assert!(!p.present && p.lines.is_empty() && !p.damaged);
    append(
        &path,
        "{\"ev\":\"start\",\"n\":1,\"t\":1}\n{\"ev\":\"end\",\"n\":1,\"status\":\"comple",
    );
    let p = r.poll();
    assert!(p.present);
    assert_eq!(p.lines.len(), 1, "the half line waits");
    append(
        &path,
        "ted\",\"iterations\":2,\"text\":\"ok\"}\n{\"ev\":\"start\",\"n\":2}\n",
    );
    let p = r.poll();
    assert_eq!(p.lines.len(), 2);
    assert!(
        matches!(&p.lines[0], RoundLine::End(e) if e.status == "completed" && e.iterations == 2)
    );
    assert!(matches!(&p.lines[1], RoundLine::Start { n: 2 }));
    assert!(r.poll().lines.is_empty());
    assert_eq!(p.bad_lines, 0);
}

#[test]
fn lines_that_are_not_round_lines_are_counted_and_skipped() {
    let d = scratch("goal-rounds-bad");
    let path = d.join("rounds.jsonl");
    append(
        &path,
        "garbage\n{\"ev\":\"start\",\"n\":1}\n{\"ev\":\"zzz\"}\n",
    );
    let mut r = RoundsReader::new(path);
    let p = r.poll();
    assert_eq!(p.lines.len(), 1);
    assert_eq!(p.bad_lines, 2);
    assert!(!p.damaged);
}

#[test]
fn a_file_that_shrank_or_was_replaced_by_a_link_is_damaged() {
    let d = scratch("goal-rounds-damaged");
    let path = d.join("rounds.jsonl");
    append(
        &path,
        "{\"ev\":\"start\",\"n\":1}\n{\"ev\":\"start\",\"n\":2}\n",
    );
    let mut r = RoundsReader::new(path.clone());
    assert_eq!(r.poll().lines.len(), 2);
    std::fs::write(&path, "{\"ev\":\"start\",\"n\":9}\n").unwrap();
    let p = r.poll();
    assert!(p.damaged, "the file is shorter than what was read");
    assert!(p.lines.is_empty());
    // a link where the file belongs is never followed
    let other = d.join("other.jsonl");
    std::fs::write(&other, "{\"ev\":\"start\",\"n\":1}\n").unwrap();
    let link = d.join("link.jsonl");
    std::os::unix::fs::symlink(&other, &link).unwrap();
    let mut r2 = RoundsReader::new(link);
    let p = r2.poll();
    assert!(!p.present && p.lines.is_empty(), "{p:?}");
}

#[test]
fn a_single_line_over_the_bound_is_dropped_and_the_reader_goes_on_with_the_next() {
    let d = scratch("goal-rounds-long");
    let path = d.join("rounds.jsonl");
    append(
        &path,
        &format!("{}\n{{\"ev\":\"start\",\"n\":7}}\n", "y".repeat(200_000)),
    );
    let mut r = RoundsReader::new(path);
    let mut lines = Vec::new();
    let mut bad = 0;
    for _ in 0..5 {
        let p = r.poll();
        bad += p.bad_lines;
        lines.extend(p.lines);
    }
    assert_eq!(lines.len(), 1);
    assert!(matches!(lines[0], RoundLine::Start { n: 7 }));
    assert_eq!(bad, 1);
}

#[test]
fn a_log_of_many_rounds_is_read_in_pieces_without_losing_a_line() {
    let d = scratch("goal-rounds-many");
    let path = d.join("rounds.jsonl");
    let mut text = String::new();
    for n in 1..=3000u64 {
        text.push_str(&format!("{{\"ev\":\"end\",\"n\":{n},\"status\":\"completed\",\"iterations\":1,\"text\":\"{}\"}}\n", "z".repeat(200)));
    }
    append(&path, &text);
    let mut r = RoundsReader::new(path);
    let mut seen = Vec::new();
    for _ in 0..40 {
        let p = r.poll();
        if p.lines.is_empty() {
            break;
        }
        seen.extend(p.lines);
    }
    assert_eq!(seen.len(), 3000);
    assert!(
        seen.iter()
            .enumerate()
            .all(|(i, l)| matches!(l, RoundLine::End(e) if e.n == i as u64 + 1))
    );
}

// ---- placing the plugin folders -----------------------------------------------------------------------------------------------------

fn source(d: &Path, change: Option<&str>) -> std::path::PathBuf {
    let src = d.join("plugins-src");
    let files: [(&str, &[u8]); 5] = [
        ("goal/index.js", b"export default {};\n"),
        ("goal/package.json", b"{\"name\":\"goal\"}\n"),
        ("goal/LICENSE", b"Apache License\n"),
        ("harness-rounds/index.js", b"export default {};\n"),
        (
            "harness-rounds/package.json",
            b"{\"name\":\"harness-rounds\"}\n",
        ),
    ];
    let mut lock = serde_json::Map::new();
    for (rel, bytes) in files {
        std::fs::create_dir_all(src.join(rel).parent().unwrap()).unwrap();
        std::fs::write(src.join(rel), bytes).unwrap();
        lock.insert(rel.to_string(), Value::String(sha(bytes)));
    }
    if let Some(rel) = change {
        std::fs::write(src.join(rel), b"changed\n").unwrap();
    }
    std::fs::write(
        src.join("lock.json"),
        serde_json::to_string_pretty(&serde_json::json!({"version": 1, "files": lock})).unwrap(),
    )
    .unwrap();
    src
}

#[test]
fn the_plugin_folders_are_copied_read_only_into_the_configuration_directory() {
    let d = scratch("goal-place");
    let src = source(&d, None);
    let config = d.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let placed = place_plugins(&src, &config, &["goal", "harness-rounds"]).unwrap();
    assert_eq!(placed.files, 5);
    for rel in [
        "goal/index.js",
        "goal/package.json",
        "goal/LICENSE",
        "harness-rounds/index.js",
        "harness-rounds/package.json",
    ] {
        let p = config.join("plugins").join(rel);
        assert_eq!(
            std::fs::read(&p).unwrap(),
            std::fs::read(src.join(rel)).unwrap(),
            "{rel}"
        );
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o444,
            "{rel}"
        );
    }
    for dir in ["plugins", "plugins/goal", "plugins/harness-rounds"] {
        assert_eq!(
            std::fs::metadata(config.join(dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555,
            "{dir}"
        );
    }
    // a second call with the same files changes nothing and fails nothing
    let again = place_plugins(&src, &config, &["goal", "harness-rounds"]).unwrap();
    assert_eq!(again.files, 5);
    // leave the scratch directory removable
    for dir in ["plugins/goal", "plugins/harness-rounds", "plugins"] {
        std::fs::set_permissions(config.join(dir), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn a_file_that_does_not_match_the_lock_stops_the_placing_and_names_the_file() {
    let d = scratch("goal-place-change");
    let src = source(&d, Some("goal/index.js"));
    let config = d.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let err = place_plugins(&src, &config, &["goal", "harness-rounds"]).unwrap_err();
    assert!(err.contains("goal/index.js"), "{err}");
    assert!(err.contains("lock"), "{err}");
    assert!(
        !config.join("plugins").exists(),
        "nothing is placed when a file does not match"
    );
}

#[test]
fn a_missing_lock_a_missing_file_a_missing_plugin_and_a_path_that_leaves_the_folder_are_refused() {
    let d = scratch("goal-place-refuse");
    let config = d.join("config");
    std::fs::create_dir_all(&config).unwrap();
    // no lock
    let empty = d.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        place_plugins(&empty, &config, &["goal"])
            .unwrap_err()
            .contains("lock.json")
    );
    // a listed file that is not there
    let src = source(&d, None);
    std::fs::remove_file(src.join("goal/LICENSE")).unwrap();
    assert!(
        place_plugins(&src, &config, &["goal"])
            .unwrap_err()
            .contains("goal/LICENSE")
    );
    // a plugin the lock has no file for
    let src2 = d.join("src2");
    let src2 = {
        std::fs::create_dir_all(&src2).unwrap();
        std::fs::write(src2.join("lock.json"), "{\"version\":1,\"files\":{}}").unwrap();
        src2
    };
    assert!(
        place_plugins(&src2, &config, &["goal"])
            .unwrap_err()
            .contains("goal")
    );
    // a path outside the plugin folder
    let odd = d.join("odd");
    std::fs::create_dir_all(&odd).unwrap();
    std::fs::write(odd.join("x"), b"x").unwrap();
    std::fs::write(
        odd.join("lock.json"),
        serde_json::json!({"version": 1, "files": {"goal/../../x": sha(b"x")}}).to_string(),
    )
    .unwrap();
    assert!(place_plugins(&odd, &config, &["goal"]).is_err());
    // a lock of another version
    std::fs::write(odd.join("lock.json"), "{\"version\":2,\"files\":{}}").unwrap();
    assert!(
        place_plugins(&odd, &config, &["goal"])
            .unwrap_err()
            .contains("version")
    );
}

#[test]
fn a_file_left_by_an_earlier_run_with_other_content_is_replaced_and_made_read_only() {
    let d = scratch("goal-place-stale");
    let src = source(&d, None);
    let config = d.join("config");
    let stale = config.join("plugins/goal/index.js");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, "left over\n").unwrap();
    place_plugins(&src, &config, &["goal", "harness-rounds"]).unwrap();
    assert_eq!(
        std::fs::read(&stale).unwrap(),
        std::fs::read(src.join("goal/index.js")).unwrap()
    );
    assert_eq!(
        std::fs::metadata(&stale).unwrap().permissions().mode() & 0o777,
        0o444
    );
    for dir in ["plugins/goal", "plugins/harness-rounds", "plugins"] {
        std::fs::set_permissions(config.join(dir), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// ---- the reader at its edges --------------------------------------------------------------------------------------------------------

#[test]
fn the_reader_knows_its_path_and_whether_a_regular_file_is_there() {
    let d = scratch("goal-rounds-exists");
    let path = d.join("rounds.jsonl");
    let r = RoundsReader::new(path.clone());
    assert_eq!(r.path(), path.as_path());
    assert!(!r.exists(), "no file yet");
    append(&path, "{\"ev\":\"start\",\"n\":1}\n");
    assert!(r.exists());
    // a link is not a regular file reached without a link
    let link = d.join("link.jsonl");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(!RoundsReader::new(link).exists());
    // nor is a directory
    let dir = d.join("dir.jsonl");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!RoundsReader::new(dir).exists());
}

#[test]
fn a_missing_file_or_a_directory_gives_nothing_and_is_not_present() {
    let d = scratch("goal-rounds-absent");
    let mut r = RoundsReader::new(d.join("none.jsonl"));
    let p = r.poll();
    assert!(!p.present && !p.damaged && p.lines.is_empty() && p.bad_lines == 0);
    let dir = d.join("dir.jsonl");
    std::fs::create_dir_all(&dir).unwrap();
    let p = RoundsReader::new(dir).poll();
    assert!(!p.present && p.lines.is_empty());
    // the file appears later: the same reader picks it up from the start
    append(&d.join("none.jsonl"), "{\"ev\":\"start\",\"n\":1}\n");
    let p = r.poll();
    assert!(p.present && p.lines.len() == 1);
}

#[test]
fn a_file_replaced_by_another_file_of_the_same_size_or_more_is_damaged() {
    let d = scratch("goal-rounds-replaced");
    let path = d.join("rounds.jsonl");
    append(&path, "{\"ev\":\"start\",\"n\":1}\n");
    let mut r = RoundsReader::new(path.clone());
    assert_eq!(r.poll().lines.len(), 1);
    let other = d.join("other.jsonl");
    append(
        &other,
        "{\"ev\":\"start\",\"n\":1}\n{\"ev\":\"start\",\"n\":2}\n",
    );
    std::fs::rename(&other, &path).unwrap();
    let p = r.poll();
    assert!(p.damaged && p.present && p.lines.is_empty(), "{p:?}");
    // once damaged it stays damaged and reads nothing more
    append(&path, "{\"ev\":\"start\",\"n\":3}\n");
    let p = r.poll();
    assert!(p.damaged && p.lines.is_empty());
}

#[test]
fn blank_lines_are_ignored_and_a_line_longer_than_one_piece_is_skipped_to_its_end() {
    let d = scratch("goal-rounds-blank");
    let path = d.join("rounds.jsonl");
    append(&path, "\n   \n{\"ev\":\"start\",\"n\":1}\n");
    let mut r = RoundsReader::new(path);
    let p = r.poll();
    assert_eq!((p.lines.len(), p.bad_lines), (1, 0));

    // a line of 600 KB reaches over several pieces of one read each: it is dropped once and the next line is read
    let long = d.join("long.jsonl");
    append(
        &long,
        &format!("{}\n{{\"ev\":\"start\",\"n\":2}}\n", "q".repeat(600_000)),
    );
    let mut r = RoundsReader::new(long);
    let (mut lines, mut bad) = (Vec::new(), 0);
    for _ in 0..8 {
        let p = r.poll();
        bad += p.bad_lines;
        lines.extend(p.lines);
    }
    assert_eq!((lines.len(), bad), (1, 1));
    assert!(matches!(lines[0], RoundLine::Start { n: 2 }));
}

#[test]
fn writing_the_state_can_hand_the_file_to_a_user_and_the_settings_come_from_the_environment() {
    use std::os::unix::fs::MetadataExt;
    let d = scratch("goal-state-owner");
    let path = d.join("goals.json");
    let me = std::fs::metadata(&d).unwrap();
    write_state(&path, &cleared_state(), Some((me.uid(), me.gid()))).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().uid(), me.uid());
    assert!(matches!(
        read_state(&path),
        GoalState::Seen {
            pending: false,
            active: 0
        }
    ));
    // the process environment holds no AGENTD_GOAL_* setting in a test run, so these are the defaults
    let s = GoalSettings::from_env();
    assert_eq!(s.limits().who, "the engine");
}

#[test]
fn the_brake_inside_a_round_has_its_settings_and_their_bounds() {
    let s = settings(&[]);
    assert_eq!(s.round_calls, 30);
    assert_eq!(s.round_window, Duration::from_secs(1200));
    assert_eq!(s.round_min_calls, 10);
    assert_eq!(s.look_every, Duration::from_secs(5));
    let s = settings(&[
        ("AGENTD_GOAL_ROUND_CALLS", "12"),
        ("AGENTD_GOAL_ROUND_WINDOW_S", "90"),
        ("AGENTD_GOAL_ROUND_MIN_CALLS", "3"),
        ("AGENTD_GOAL_LOOK_EVERY_MS", "50"),
    ]);
    assert_eq!(
        (
            s.round_calls,
            s.round_window,
            s.round_min_calls,
            s.look_every
        ),
        (12, Duration::from_secs(90), 3, Duration::from_millis(50))
    );
    // out of range is cut, a value that does not parse is the default
    let s = settings(&[
        ("AGENTD_GOAL_ROUND_CALLS", "1"),
        ("AGENTD_GOAL_ROUND_WINDOW_S", "1"),
        ("AGENTD_GOAL_ROUND_MIN_CALLS", "x"),
        ("AGENTD_GOAL_LOOK_EVERY_MS", "99999999"),
    ]);
    assert_eq!(s.round_calls, 5);
    assert_eq!(s.round_window, Duration::from_secs(10));
    assert_eq!(s.round_min_calls, 10);
    assert_eq!(s.look_every, Duration::from_secs(60));
}
