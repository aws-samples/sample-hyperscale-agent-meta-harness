//! The review round on the client: the user's prompt file `review.md`, the setting `run.review_round` and the request
//! member they make. `ham` reads the file and the setting and sends the member unread; the check is the protocol's own, the one control-api and
//! agentd call.
//!
//! - `review.md` holds the prompt and, in optional frontmatter, `guard_minutes` (1 to 480). `ham rules init` writes the default, which is the
//!   protocol's default prompt; an edited file is never replaced.
//! - `run.review_round` is `auto` (the default) or `off`. `off` sends `{"mode":"off"}` and no text; `auto` and no setting send only what the file
//!   says; a user with no file and no setting sends no member at all, so the request is byte for byte the request of an older client.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, DirectArgs, parse};
use ham_core::config::{
    Config, KEYS, get_key, parse_config, read_config, render_config, review_round, set_key,
    unset_key,
};
use ham_core::config_write::plan_set;
use ham_core::notice::{NoticeKind, notice_of};
use ham_core::request::{DirectInputs, Origin, resolve_run_settings};
use ham_core::rules::{
    Problem, REVIEW_NAME, default_review_file, parse_review, review_path, validate_review,
};
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order, ROWS};
use harness_protocol::direct::{DEFAULT_REVIEW_PROMPT, ReviewMode, ReviewOptions, check_review};
use harness_protocol::envelope::Envelope;
use serde_json::{Value, json};

const PROMPT: &str = "Add a percent_change(a, b) function";

fn args() -> DirectArgs {
    let Command::StartDirect(a) = parse(&[PROMPT, "--s3-only", "--direct"]).unwrap().command else {
        panic!("not a direct start");
    };
    a
}

fn env(_: &str) -> Option<String> {
    None
}

fn with(word: &str) -> Config {
    set_key(&Config::default(), "run.review_round", word).unwrap()
}

// ---- the registry row ---------------------------------------------------------------------------------------------------------

#[test]
fn the_registry_has_one_row_for_it_after_the_lifecycle_flow_row() {
    assert_eq!(Key::ALL.len(), 35);
    assert_eq!(ROWS.len(), 35);
    assert_eq!(KEYS.len(), 35);
    assert_eq!(Key::ALL[22], Key::RunLifecycleFlow);
    assert_eq!(Key::ALL[23], Key::RunReviewRound);
    let row = Key::RunReviewRound.row();
    assert_eq!(row.name, "run.review_round");
    assert_eq!(row.file, ("run", "review_round"));
    assert_eq!(row.kind, Kind::Review);
    assert_eq!(row.default_text, "auto");
    assert_eq!(row.default, DefaultValue::Absent);
    assert_eq!(row.env, "");
    assert_eq!(row.flag, "");
    assert_eq!(row.order, Order::FileOnly);
    assert_eq!(
        row.group,
        Group::Limits,
        "an advanced setting of a run, beside the stall limits and the final message"
    );
    assert_eq!(row.since, 1);
    assert_eq!(row.applies, Applies::NextRun);
    assert_eq!(row.wire, Some("review"));
    assert!(!row.security);
    assert!(KEYS.contains(&("run.review_round", "run.review_round")));
    assert_eq!(Key::lookup("run.review_round"), Some(Key::RunReviewRound));
    for word in ["auto", "off", "review.md"] {
        assert!(
            row.help.contains(word),
            "the help names {word}: {}",
            row.help
        );
    }
    assert_eq!(Kind::Review.type_text(), "auto or off");
    assert_eq!(Kind::Review.word(), "review");
}

#[test]
fn the_reference_table_lists_the_key_with_its_default_and_its_type() {
    let table = ham_core::settings::reference_markdown();
    let line = table
        .lines()
        .find(|l| l.starts_with("| `run.review_round` |"))
        .unwrap_or_else(|| panic!("no row in {table}"));
    assert!(line.contains("auto or off"), "{line}");
    assert!(line.contains("| auto |"), "{line}");
}

// ---- the two words ------------------------------------------------------------------------------------------------------------

#[test]
fn the_two_words_are_the_values_and_nothing_else_is() {
    for word in ["auto", "off"] {
        assert_eq!(
            get_key(&with(word), "run.review_round").unwrap().as_deref(),
            Some(word)
        );
    }
    for (given, stored) in [("AUTO", "auto"), (" Off ", "off"), ("oFf", "off")] {
        assert_eq!(
            get_key(&with(given), "run.review_round")
                .unwrap()
                .as_deref(),
            Some(stored)
        );
    }
    for bad in [
        "", "on", "true", "false", "always", "never", "1", "auto off",
    ] {
        let e = set_key(&Config::default(), "run.review_round", bad)
            .expect_err(&format!("{bad:?} is not a mode"))
            .to_string();
        assert!(
            e.contains("auto or off"),
            "the refusal names the two values: {e}"
        );
    }
}

#[test]
fn the_default_is_auto_and_unset_goes_back_to_it() {
    assert_eq!(review_round(&Config::default()), ReviewMode::Auto);
    assert_eq!(review_round(&with("off")), ReviewMode::Off);
    assert_eq!(review_round(&with("auto")), ReviewMode::Auto);
    let back = unset_key(&with("off"), "run.review_round").unwrap();
    assert_eq!(get_key(&back, "run.review_round").unwrap(), None);
    assert_eq!(review_round(&back), ReviewMode::Auto);
}

#[test]
fn the_file_reads_it_from_the_run_table_and_refuses_another_value() {
    let ok = read_config("configVersion = 1\n\n[run]\nreview_round = \"off\"\n");
    assert!(!ok.findings.has_errors(), "{:?}", ok.findings);
    assert_eq!(review_round(&ok.config), ReviewMode::Off);
    for bad in ["\"on\"", "\"Auto\"", "\"always\"", "true", "1"] {
        let r = read_config(&format!(
            "configVersion = 1\n\n[run]\nreview_round = {bad}\n"
        ));
        assert!(r.findings.has_errors(), "{bad}");
        assert!(
            r.render_errors().contains("auto") && r.render_errors().contains("off"),
            "{bad}: {}",
            r.render_errors()
        );
        assert_eq!(review_round(&r.config), ReviewMode::Auto, "{bad}");
        assert!(r.run_blocked(), "a file with an error starts no run");
    }
    // The key lives in [run]; at the top level of the file it is an unknown key.
    let top = read_config("configVersion = 1\nreview_round = \"off\"\n");
    assert!(!top.findings.is_empty());
    assert_eq!(review_round(&top.config), ReviewMode::Auto);
}

#[test]
fn a_set_writes_the_word_under_run_and_a_file_with_it_reads_back_the_same() {
    let written = render_config(&with("off"));
    assert!(written.contains("configVersion = 1"), "{written}");
    assert!(written.contains("[run]"), "{written}");
    assert!(written.contains("review_round = \"off\""), "{written}");
    assert_eq!(parse_config(&written).unwrap(), with("off"));
    // The comment-keeping patcher of `ham config set` writes the same line and keeps what the file has.
    let before = "# my settings\nconfigVersion = 1\n\n[run]\ngoal = true\n";
    let plan = plan_set(before, "run.review_round", "off").unwrap();
    let after = plan.text.unwrap();
    assert!(after.starts_with("# my settings\n"), "{after}");
    assert!(after.contains("goal = true"), "{after}");
    assert!(after.contains("review_round = \"off\""), "{after}");
    assert_eq!(
        review_round(&parse_config(&after).unwrap()),
        ReviewMode::Off
    );
}

// ---- review.md -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_default_file_is_the_default_prompt_with_the_guard_left_to_the_stage() {
    let file = default_review_file();
    assert!(file.starts_with("---\n# guard_minutes: 20"), "{file}");
    let parsed = parse_review(&file).unwrap();
    assert_eq!(parsed.mode, ReviewMode::Auto);
    assert_eq!(parsed.text.as_deref(), Some(DEFAULT_REVIEW_PROMPT));
    assert_eq!(parsed.guard_s, None, "the key is commented out");
    assert_eq!(check_review(&parsed), Ok(()));
    assert_eq!(
        parsed,
        ReviewOptions::with_text(DEFAULT_REVIEW_PROMPT, None)
    );
}

#[test]
fn guard_minutes_becomes_seconds_and_stays_between_one_and_480() {
    for (minutes, seconds) in [(1, 60), (20, 1200), (45, 2700), (480, 28_800)] {
        let r = parse_review(&format!(
            "---\nguard_minutes: {minutes}\n---\nLook again.\n"
        ))
        .unwrap();
        assert_eq!(r.guard_s, Some(seconds), "{minutes}");
        assert_eq!(check_review(&r), Ok(()));
    }
    for bad in ["0", "481", "-3", "1.5", "\"ten\"", "[5]", "null", "true"] {
        let e =
            parse_review(&format!("---\nguard_minutes: {bad}\n---\nLook again.\n")).expect_err(bad);
        assert_eq!(e.name, REVIEW_NAME, "{bad}");
        let text = e.to_string();
        assert!(
            text.contains("guard_minutes") && text.contains("1") && text.contains("480"),
            "{bad}: {text}"
        );
    }
}

#[test]
fn another_frontmatter_key_is_refused_and_named() {
    let e = parse_review("---\nmax_nudges: 2\n---\nLook again.\n").unwrap_err();
    assert!(matches!(&e.problem, Problem::UnknownReviewKey(k) if k == "max_nudges"));
    let text = e.to_string();
    assert!(
        text.contains("max_nudges") && text.contains("guard_minutes"),
        "{text}"
    );
    assert!(text.starts_with("rule \"review.md\""), "{text}");
}

#[test]
fn the_body_is_checked_like_any_rule_file_and_by_the_protocol() {
    assert!(matches!(
        parse_review("").unwrap_err().problem,
        Problem::Empty
    ));
    assert!(matches!(
        parse_review("---\nguard_minutes: 5\n---\n   \n")
            .unwrap_err()
            .problem,
        Problem::Empty
    ));
    assert!(matches!(
        parse_review("a\0b").unwrap_err().problem,
        Problem::Nul
    ));
    assert!(matches!(
        parse_review("\u{feff}Look again.").unwrap_err().problem,
        Problem::Bom
    ));
    assert!(matches!(
        parse_review("---\nguard_minutes: 5\nLook again.")
            .unwrap_err()
            .problem,
        Problem::FrontmatterUnclosed
    ));
    // The old limit was 8 KiB; the review text has no size of its own.
    let at_limit = "x".repeat(8 * 1024 + 1);
    assert_eq!(
        parse_review(&at_limit).unwrap().text.as_deref(),
        Some(at_limit.as_str())
    );
    // The text is sent as typed: the tokens of the harness stay in it.
    let t = "Base {{base}}.\n{{facts}}\n";
    assert_eq!(parse_review(t).unwrap().text.as_deref(), Some(t));
}

#[test]
fn validate_review_names_the_file_for_each_problem_of_the_protocols_check() {
    let ok = ReviewOptions::with_text("go", Some(600));
    assert!(validate_review(&ok).is_ok());
    let mut hash = ok.clone();
    hash.sha256 = Some("0".repeat(64));
    assert!(matches!(
        validate_review(&hash).unwrap_err().problem,
        Problem::ShaMismatch
    ));
    let mut guard = ok.clone();
    guard.guard_s = Some(5);
    let e = validate_review(&guard).unwrap_err();
    assert_eq!(e.name, REVIEW_NAME);
    assert!(e.to_string().contains("guard_minutes"), "{e}");
    assert!(matches!(
        validate_review(&ReviewOptions::with_text(" ", None))
            .unwrap_err()
            .problem,
        Problem::Empty
    ));
}

#[test]
fn the_file_sits_beside_the_config_file() {
    let home = |k: &str| (k == "HOME").then(|| "/home/me".to_string());
    assert_eq!(
        review_path(&home).unwrap().to_string_lossy(),
        "/home/me/.config/ham/review.md"
    );
    let xdg = |k: &str| (k == "XDG_CONFIG_HOME").then(|| "/x/cfg".to_string());
    assert_eq!(
        review_path(&xdg).unwrap().to_string_lossy(),
        "/x/cfg/ham/review.md"
    );
    assert_eq!(review_path(&|_: &str| None), None);
}

// ---- the request member ---------------------------------------------------------------------------------------------------------

fn request_json(cfg: &Config, review: Option<ReviewOptions>) -> Value {
    let resolved = resolve_run_settings(cfg, &env, &args(), false).unwrap();
    let inputs = DirectInputs {
        review,
        ..DirectInputs::default()
    };
    serde_json::to_value(resolved.request(inputs).unwrap()).unwrap()
}

#[test]
fn no_file_and_no_setting_send_no_member_so_the_request_is_the_older_one() {
    let v = request_json(&Config::default(), None);
    assert!(v.get("review").is_none(), "{v}");
}

#[test]
fn a_file_with_no_setting_or_auto_sends_the_prompt_the_guard_and_its_hash() {
    let file = parse_review("---\nguard_minutes: 30\n---\nLook again at {{base}}.\n").unwrap();
    for cfg in [Config::default(), with("auto")] {
        let v = request_json(&cfg, Some(file.clone()));
        assert_eq!(v["review"]["mode"], "auto", "{v}");
        assert_eq!(v["review"]["text"], "Look again at {{base}}.\n", "{v}");
        assert_eq!(v["review"]["guardS"], 1800, "{v}");
        assert_eq!(
            v["review"]["sha256"].as_str().unwrap().len(),
            64,
            "the hash of the text: {v}"
        );
        assert_eq!(
            check_review(&serde_json::from_value(v["review"].clone()).unwrap()),
            Ok(())
        );
    }
}

#[test]
fn off_sends_the_switch_alone_and_the_file_is_not_sent() {
    let file = parse_review("Look again.\n").unwrap();
    let v = request_json(&with("off"), Some(file));
    assert_eq!(v["review"], serde_json::json!({"mode": "off"}), "{v}");
    let v = request_json(&with("off"), None);
    assert_eq!(v["review"], serde_json::json!({"mode": "off"}), "{v}");
}

#[test]
fn the_setting_is_a_file_origin_and_the_default_otherwise() {
    let none = resolve_run_settings(&Config::default(), &env, &args(), false).unwrap();
    assert_eq!(none.origin(Key::RunReviewRound), Some(Origin::Default));
    assert_eq!(none.review_round, None);
    let set = resolve_run_settings(&with("off"), &env, &args(), false).unwrap();
    assert_eq!(set.origin(Key::RunReviewRound), Some(Origin::File));
    assert_eq!(set.review_round, Some(ReviewMode::Off));
}

// ---- the lines of the run screen ------------------------------------------------------------------------------------------------

fn progress(n: u32, v: Value) -> Envelope {
    serde_json::from_value(json!({
        "schemaVersion": 1, "eventId": format!("01M5A2R7K3C9XJ4D8E6F{n:06}"), "runId": "01M4ZHFBV5SDX7J92E6YRH4QTW", "agentId": "direct", "seq": n,
        "ts": "2026-10-25T08:15:00Z", "type": "turn.progress", "authority": "agent",
        "payload": {"kind": "inline", "value": v}
    }))
    .unwrap()
}

#[test]
fn the_two_events_of_the_round_are_notices_with_the_harnesss_own_words() {
    let start = notice_of(&progress(
        1,
        json!({"turn": 1, "kind": "review-round", "phase": "start",
               "text": "The harness asks Cline to review its own delivery once, as the maintainer who must merge it."}),
    ))
    .unwrap();
    assert_eq!(start.kind, NoticeKind::Notify);
    assert!(
        start.text.starts_with("The harness asks Cline to review"),
        "{}",
        start.text
    );
    let end = notice_of(&progress(
        2,
        json!({"turn": 1, "kind": "review-round", "phase": "end", "result": "completed", "text": "The harness asked Cline once to review its own delivery as the maintainer who must merge it. It took 12 s."}),
    ))
    .unwrap();
    assert_eq!(end.kind, NoticeKind::Notify);
    assert!(end.text.contains("It took 12 s."), "{}", end.text);
    // An event with no sentence still reads.
    let bare = notice_of(&progress(3, json!({"turn": 1, "kind": "review-round"}))).unwrap();
    assert_eq!(bare.kind, NoticeKind::Notify);
    assert_eq!(bare.text, "Review round.");
}
