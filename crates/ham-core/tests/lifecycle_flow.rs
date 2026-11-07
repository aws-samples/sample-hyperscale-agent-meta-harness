//! The setting `run.lifecycle_flow` and the three texts of the lifecycle contract.
//!
//! The contract is rendered from one source (`lifecycle/contract.src`, `lifecycle/contract.py`) into three texts: `full` (the eight steps), `quick` (the quick path
//! alone) and `auto` (a flow statement, then both). `ham rules init` installs one of them as the rule `10-lifecycle.md`, as `run.lifecycle_flow` says: `auto` unless
//! the file says `full` or `quick`. A rule file the user edited is never replaced; an unedited text of the pack, of this version or an earlier one, is.
//!
//! A text rule cannot make a model classify a task correctly. These tests pin the setting, the rendering and the installer; the release gate (new projects, full
//! flow, 6 of 6 runs) is the empirical check of the default.
#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, KEYS, LifecycleFlow, get_key, lifecycle_flow, parse_config, read_config, render_config,
    set_key, unset_key,
};
use ham_core::config_write::plan_set;
use ham_core::pack::{self, History, Installed};
use ham_core::rules::validate_rule;
use ham_core::settings::{Applies, DefaultValue, Group, Key, Kind, Order, ROWS};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const OLD_CONTRACT_1_2_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.2.0-AGENTS.md.txt");
const OLD_LOOP_1_2_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.2.0-lifecycle-loop-SKILL.md.txt");
const OLD_DELIVERY_1_2_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.2.0-lifecycle-delivery-SKILL.md.txt");
// The three texts of the contract in pack 1.3.0, before the Flow trailer: the pack lists `auto`, and `lock.py --earlier` listed the other two.
const OLD_AUTO_1_3_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-AGENTS.md.txt");
const OLD_FULL_1_3_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-AGENTS-full.md.txt");
const OLD_QUICK_1_3_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-AGENTS-quick.md.txt");
// The delivery skill of pack 1.3.0: its step 5 ends the body with the DOX line and says nothing of the Flow line.
const OLD_DELIVERY_1_3_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-lifecycle-delivery-SKILL.md.txt");

// The three texts of the contract in pack 1.7.0, before the outside paragraph stopped naming a service of one team's CI: the pack lists `auto`, and `lock.py --earlier` lists the other two.
const OLD_AUTO_1_7_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.7.0-AGENTS.md.txt");
const OLD_FULL_1_7_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.7.0-AGENTS-full.md.txt");
const OLD_QUICK_1_7_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.7.0-AGENTS-quick.md.txt");

// The three texts of the contract in pack 1.6.0, before the contract was reworded to name no script and no failure of one repository: the pack lists `auto`, and `lock.py --earlier` lists the other two.
const OLD_AUTO_1_6_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.6.0-AGENTS.md.txt");
const OLD_FULL_1_6_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.6.0-AGENTS-full.md.txt");
const OLD_QUICK_1_6_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.6.0-AGENTS-quick.md.txt");

// The three texts of the contract in pack 1.5.0, before the six verification bullets: the pack lists `auto`, and `lock.py --earlier` lists the other two.
const OLD_AUTO_1_5_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.5.0-AGENTS.md.txt");
const OLD_FULL_1_5_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.5.0-AGENTS-full.md.txt");
const OLD_QUICK_1_5_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.5.0-AGENTS-quick.md.txt");

// The three texts of the contract in pack 1.4.0, before the NOT RUN sentence for a missing tool: the pack lists `auto`, and `lock.py --earlier` lists the other two.
const OLD_AUTO_1_4_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-AGENTS.md.txt");
const OLD_FULL_1_4_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-AGENTS-full.md.txt");
const OLD_QUICK_1_4_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-AGENTS-quick.md.txt");
// The loop skill of pack 1.4.0: steps 5 and 7 wait for "every check" and say nothing of a check that a missing tool stops.
const OLD_LOOP_1_4_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-lifecycle-loop-SKILL.md.txt");

// The three texts of the contract in pack 1.12.0, before the order of the failing run was tightened: the pack lists `auto`, and `lock.py --earlier` listed the other two.
const OLD_AUTO_1_12_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.12.0-AGENTS.md.txt");
const OLD_FULL_1_12_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.12.0-AGENTS-full.md.txt");
const OLD_QUICK_1_12_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.12.0-AGENTS-quick.md.txt");

const PINS: [(LifecycleFlow, usize, &str); 3] = [
    (
        LifecycleFlow::Full,
        9048,
        "b5e0532968a8456684e22d5b31e7c1b4a9e981255029d00668a1772d6e0aeebb",
    ),
    (
        LifecycleFlow::Quick,
        6574,
        "2a6151223027d9faa42b15878deea40e30af47271b697109211c1d106c6e4970",
    ),
    (
        LifecycleFlow::Auto,
        11568,
        "3973da054b8cef8bdfd175f9d6292b382f9e010210babd58d09c2cff605f3d07",
    ),
];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-lifecycle-flow-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn with(word: &str) -> Config {
    set_key(&Config::default(), "run.lifecycle_flow", word).unwrap()
}

// ---- the registry row ------------------------------------------------------------------------------------------------------------

#[test]
fn the_registry_has_one_row_for_it_after_the_final_message_row() {
    assert_eq!(Key::ALL.len(), 35);
    assert_eq!(ROWS.len(), 35);
    assert_eq!(KEYS.len(), 35);
    assert_eq!(Key::ALL[21], Key::RunFinalMessage);
    assert_eq!(Key::ALL[22], Key::RunLifecycleFlow);
    let row = Key::RunLifecycleFlow.row();
    assert_eq!(row.name, "run.lifecycle_flow");
    assert_eq!(row.file, ("run", "lifecycle_flow"));
    assert_eq!(row.kind, Kind::Flow);
    assert_eq!(row.default_text, "auto");
    assert_eq!(row.default, DefaultValue::Absent);
    assert_eq!(row.env, "");
    assert_eq!(row.flag, "");
    assert_eq!(row.order, Order::FileOnly);
    assert_eq!(row.group, Group::Run);
    assert_eq!(row.since, 1);
    assert_eq!(row.applies, Applies::NextRun);
    assert_eq!(
        row.wire, None,
        "the rule text is a user file, not a request member"
    );
    assert!(!row.security);
    assert!(KEYS.contains(&("run.lifecycle_flow", "run.lifecycle_flow")));
    assert_eq!(
        Key::lookup("run.lifecycle_flow"),
        Some(Key::RunLifecycleFlow)
    );
    let help = row.help;
    for word in ["auto", "full", "quick", "ham rules init"] {
        assert!(help.contains(word), "the help names {word}: {help}");
    }
}

#[test]
fn the_reference_table_lists_the_key_with_its_default_and_its_type() {
    let table = ham_core::settings::reference_markdown();
    let line = table
        .lines()
        .find(|l| l.starts_with("| `run.lifecycle_flow` |"))
        .unwrap_or_else(|| panic!("no row in {table}"));
    assert!(line.contains("auto, full or quick"), "{line}");
    assert!(line.contains("| auto |"), "{line}");
}

// ---- the three words -------------------------------------------------------------------------------------------------------------

#[test]
fn the_three_words_are_the_values_and_nothing_else_is() {
    for word in ["auto", "full", "quick"] {
        assert_eq!(
            get_key(&with(word), "run.lifecycle_flow")
                .unwrap()
                .as_deref(),
            Some(word)
        );
    }
    for (given, stored) in [("AUTO", "auto"), (" Full ", "full"), ("QuIcK", "quick")] {
        assert_eq!(
            get_key(&with(given), "run.lifecycle_flow")
                .unwrap()
                .as_deref(),
            Some(stored)
        );
    }
    for bad in [
        "",
        "fast",
        "slow",
        "none",
        "on",
        "true",
        "1",
        "full quick",
        "eight",
    ] {
        let e = set_key(&Config::default(), "run.lifecycle_flow", bad)
            .expect_err(&format!("{bad:?} is not a flow"))
            .to_string();
        assert!(
            e.contains("auto, full or quick"),
            "the refusal names the three values: {e}"
        );
    }
}

#[test]
fn the_default_is_auto_and_unset_goes_back_to_it() {
    assert_eq!(lifecycle_flow(&Config::default()), LifecycleFlow::Auto);
    assert_eq!(LifecycleFlow::default(), LifecycleFlow::Auto);
    assert_eq!(lifecycle_flow(&with("full")), LifecycleFlow::Full);
    assert_eq!(lifecycle_flow(&with("quick")), LifecycleFlow::Quick);
    assert_eq!(lifecycle_flow(&with("auto")), LifecycleFlow::Auto);
    let back = unset_key(&with("quick"), "run.lifecycle_flow").unwrap();
    assert_eq!(get_key(&back, "run.lifecycle_flow").unwrap(), None);
    assert_eq!(lifecycle_flow(&back), LifecycleFlow::Auto);
}

#[test]
fn the_words_round_trip_and_the_list_is_the_three_in_the_order_the_text_names_them() {
    assert_eq!(
        LifecycleFlow::ALL,
        [
            LifecycleFlow::Auto,
            LifecycleFlow::Full,
            LifecycleFlow::Quick
        ]
    );
    for flow in LifecycleFlow::ALL {
        assert_eq!(LifecycleFlow::parse(flow.word()), Some(flow));
    }
    assert_eq!(LifecycleFlow::parse("  FULL\n"), Some(LifecycleFlow::Full));
    assert_eq!(LifecycleFlow::parse("fast"), None);
    assert_eq!(LifecycleFlow::parse(""), None);
}

#[test]
fn the_file_reads_it_from_the_run_table_and_refuses_another_value() {
    let ok = read_config("configVersion = 1\n\n[run]\nlifecycle_flow = \"quick\"\n");
    assert!(!ok.findings.has_errors(), "{:?}", ok.findings);
    assert_eq!(lifecycle_flow(&ok.config), LifecycleFlow::Quick);
    // A value that is not one of the three words is a finding that names them, and the key is not read.
    let bad = read_config("configVersion = 1\n\n[run]\nlifecycle_flow = \"fast\"\n");
    assert!(bad.findings.has_errors());
    assert!(
        bad.render_errors().contains("auto"),
        "{}",
        bad.render_errors()
    );
    assert!(bad.render_errors().contains("full") && bad.render_errors().contains("quick"));
    assert_eq!(lifecycle_flow(&bad.config), LifecycleFlow::Auto);
    assert!(bad.run_blocked(), "a file with an error starts no run");
    let wrong_type = read_config("configVersion = 1\n\n[run]\nlifecycle_flow = true\n");
    assert!(wrong_type.findings.has_errors());
    // The key lives in [run]; at the top level of the file it is an unknown key.
    let top = read_config("configVersion = 1\nlifecycle_flow = \"full\"\n");
    assert!(!top.findings.is_empty());
    assert_eq!(lifecycle_flow(&top.config), LifecycleFlow::Auto);
}

#[test]
fn a_set_writes_the_word_under_run_and_a_file_with_it_reads_back_the_same() {
    let written = render_config(&with("full"));
    assert!(written.contains("configVersion = 1"), "{written}");
    assert!(written.contains("[run]"), "{written}");
    assert!(written.contains("lifecycle_flow = \"full\""), "{written}");
    assert_eq!(parse_config(&written).unwrap(), with("full"));
    // The comment-keeping patcher of `ham config set` writes the same line and keeps what the file has.
    let before = "# my settings\nconfigVersion = 1\n\n[run]\ngoal = true\n";
    let plan = plan_set(before, "run.lifecycle_flow", "quick").unwrap();
    let after = plan.text.unwrap();
    assert!(after.starts_with("# my settings\n"), "{after}");
    assert!(after.contains("goal = true"), "{after}");
    assert!(after.contains("lifecycle_flow = \"quick\""), "{after}");
    assert_eq!(
        lifecycle_flow(&parse_config(&after).unwrap()),
        LifecycleFlow::Quick
    );
}

// ---- the three texts -------------------------------------------------------------------------------------------------------------

#[test]
fn the_three_texts_are_the_rendered_files_and_each_is_the_pinned_text() {
    let files = [
        (LifecycleFlow::Auto, "lifecycle/pack/AGENTS.md"),
        (
            LifecycleFlow::Auto,
            "crates/ham-core/assets/lifecycle-rules.md",
        ),
        (
            LifecycleFlow::Full,
            "crates/ham-core/assets/lifecycle-rules-full.md",
        ),
        (
            LifecycleFlow::Quick,
            "crates/ham-core/assets/lifecycle-rules-quick.md",
        ),
    ];
    for (flow, rel) in files {
        let on_disk = std::fs::read_to_string(repo().join(rel)).unwrap();
        assert_eq!(pack::contract_text(flow), on_disk, "{rel}");
    }
    assert_eq!(pack::contract_text(LifecycleFlow::Auto), pack::CONTRACT);
    for (flow, bytes, digest) in PINS {
        let text = pack::contract_text(flow);
        assert_eq!(text.len(), bytes, "{}", flow.word());
        assert_eq!(sha(text.as_bytes()), digest, "{}", flow.word());
    }
}

#[test]
fn the_texts_differ_and_stay_inside_their_caps() {
    let (auto, full, quick) = (
        pack::contract_text(LifecycleFlow::Auto),
        pack::contract_text(LifecycleFlow::Full),
        pack::contract_text(LifecycleFlow::Quick),
    );
    assert!(auto != full && auto != quick && full != quick);
    assert!(full.len() <= 9100, "{}", full.len());
    assert!(quick.len() <= 6600, "{}", quick.len());
    assert!(auto.len() <= 11600, "{}", auto.len());
    // Rendering is a pure function of the setting: the same word, the same bytes.
    for flow in LifecycleFlow::ALL {
        assert_eq!(pack::contract_text(flow), pack::contract_text(flow));
    }
}

#[test]
fn full_holds_no_quick_text_and_quick_no_eight_steps() {
    let full = pack::contract_text(LifecycleFlow::Full);
    for gone in ["Quick path", "Flow: quick", "Q1 ", "quick"] {
        assert!(!full.contains(gone), "full holds {gone:?}");
    }
    assert!(
        full.starts_with(
            "# Development lifecycle\n\nFollow these eight steps in order on every task"
        )
    );
    let quick = pack::contract_text(LifecycleFlow::Quick);
    for gone in ["1. Goal card", "8. Self-check", "Records.", "VERIFY"] {
        assert!(!quick.contains(gone), "quick holds {gone:?}");
    }
    assert!(quick.contains("Q5 "));
    let auto = pack::contract_text(LifecycleFlow::Auto);
    for must in [
        "Flow. Use the quick path only for",
        "it had commits and an AGENTS.md or README before your first change",
        "When unsure, or if a quick task grows into one of those, take the eight steps.",
        "Flow: quick",
        "Flow: full",
        "1. Goal card",
        "Q1 ",
    ] {
        assert!(auto.contains(must), "auto lacks {must:?}");
    }
}

#[test]
fn every_text_is_a_valid_rule_of_the_shared_validator() {
    for flow in LifecycleFlow::ALL {
        let rule = pack::contract_rule_for(flow);
        assert_eq!(rule.path, "10-lifecycle.md");
        assert_eq!(rule.content, pack::contract_text(flow));
        assert_eq!(rule.sha256, sha(rule.content.as_bytes()));
        validate_rule(&rule.path, &rule.content).unwrap();
    }
    assert_eq!(
        pack::contract_rule(),
        pack::contract_rule_for(LifecycleFlow::Auto)
    );
}

// ---- the installer ---------------------------------------------------------------------------------------------------------------

#[test]
fn an_unedited_text_of_another_flow_is_replaced_and_an_edited_one_is_kept() {
    let dir = scratch("rule");
    let put = |flow: LifecycleFlow| {
        pack::install_rule(&dir, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap()
    };
    assert_eq!(put(LifecycleFlow::Auto), Installed::Created);
    assert_eq!(put(LifecycleFlow::Auto), Installed::Same);
    let version = pack::version();
    assert_eq!(
        put(LifecycleFlow::Full),
        Installed::Updated {
            was: format!("{version}, flow auto")
        }
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(pack::CONTRACT_RULE)).unwrap(),
        pack::contract_text(LifecycleFlow::Full)
    );
    assert_eq!(
        put(LifecycleFlow::Quick),
        Installed::Updated {
            was: format!("{version}, flow full")
        }
    );
    assert_eq!(
        put(LifecycleFlow::Auto),
        Installed::Updated {
            was: format!("{version}, flow quick")
        }
    );
    // A file the user edited is never replaced, whichever flow is asked for.
    let mine = format!(
        "{}\nMy own rule.\n",
        pack::contract_text(LifecycleFlow::Full)
    );
    std::fs::write(dir.join(pack::CONTRACT_RULE), &mine).unwrap();
    for flow in LifecycleFlow::ALL {
        assert_eq!(put(flow), Installed::KeptEdited, "{}", flow.word());
        assert_eq!(
            std::fs::read_to_string(dir.join(pack::CONTRACT_RULE)).unwrap(),
            mine
        );
    }
}

#[test]
fn a_link_to_a_text_of_the_pack_is_the_users_and_is_kept() {
    let dir = scratch("link");
    let target = dir.join("elsewhere.md");
    std::fs::write(&target, pack::contract_text(LifecycleFlow::Quick)).unwrap();
    std::os::unix::fs::symlink(&target, dir.join(pack::CONTRACT_RULE)).unwrap();
    assert_eq!(
        pack::install_rule(
            &dir,
            pack::CONTRACT_RULE,
            pack::contract_text(LifecycleFlow::Full)
        )
        .unwrap(),
        Installed::KeptEdited
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        pack::contract_text(LifecycleFlow::Quick)
    );
}

#[test]
fn the_text_of_another_file_name_is_not_replaced() {
    // Only the contract rule has other texts of the pack; a rule of the user's own with the same bytes under another name is theirs.
    let dir = scratch("name");
    std::fs::write(
        dir.join("20-mine.md"),
        pack::contract_text(LifecycleFlow::Quick),
    )
    .unwrap();
    assert_eq!(
        pack::install_rule(&dir, "20-mine.md", pack::contract_text(LifecycleFlow::Full)).unwrap(),
        Installed::KeptEdited
    );
}

#[test]
fn the_history_names_the_1_2_0_contract_and_an_install_of_any_flow_replaces_it() {
    assert_eq!(
        History::pack().earlier_version("AGENTS.md", OLD_CONTRACT_1_2_0.as_bytes()),
        Some("1.2.0")
    );
    for flow in LifecycleFlow::ALL {
        let rule = scratch(&format!("rule-1-2-0-{}", flow.word()));
        std::fs::write(rule.join(pack::CONTRACT_RULE), OLD_CONTRACT_1_2_0).unwrap();
        assert_eq!(
            pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
            Installed::Updated {
                was: "1.2.0".to_string()
            },
            "{}",
            flow.word()
        );
        assert_eq!(
            std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
            pack::contract_text(flow)
        );
    }
}

#[test]
fn the_pack_is_version_1_14_0_and_its_contract_is_the_auto_text() {
    assert_eq!(pack::version(), "1.14.0");
    let lock: serde_json::Value = serde_json::from_str(pack::LOCK).unwrap();
    assert_eq!(
        lock["files"]["AGENTS.md"].as_str(),
        Some(sha(pack::contract_text(LifecycleFlow::Auto).as_bytes()).as_str())
    );
}

#[test]
fn the_history_names_the_1_2_0_loop_and_delivery_skills_and_an_install_replaces_them() {
    // Pack 1.3.0 changed the two skills that tell the story of BLOCKED (the outside-this-checkout rule); a machine that holds the 1.2.0 texts unedited
    // gets the new ones from `ham rules init`, and an edited copy is kept.
    for (skill, old) in [
        ("lifecycle-loop", OLD_LOOP_1_2_0),
        ("lifecycle-delivery", OLD_DELIVERY_1_2_0),
    ] {
        let path = format!(".agents/skills/{skill}/SKILL.md");
        assert_eq!(
            History::pack().earlier_version(&path, old.as_bytes()),
            Some("1.2.0"),
            "{skill}"
        );
        let current = pack::files().iter().find(|f| f.path == path).unwrap();
        let skills = scratch(&format!("skill-1-2-0-{skill}"));
        std::fs::create_dir_all(skills.join(skill)).unwrap();
        std::fs::write(skills.join(skill).join("SKILL.md"), old).unwrap();
        assert_eq!(
            pack::install_skill_file(&skills, skill, "SKILL.md", current.content).unwrap(),
            Installed::Updated {
                was: "1.2.0".to_string()
            },
            "{skill}"
        );
        assert_eq!(
            std::fs::read_to_string(skills.join(skill).join("SKILL.md")).unwrap(),
            current.content
        );
        let mine = scratch(&format!("skill-edited-{skill}"));
        std::fs::create_dir_all(mine.join(skill)).unwrap();
        std::fs::write(
            mine.join(skill).join("SKILL.md"),
            format!("{old}\nMy own line.\n"),
        )
        .unwrap();
        assert_eq!(
            pack::install_skill_file(&mine, skill, "SKILL.md", current.content).unwrap(),
            Installed::KeptEdited,
            "{skill}"
        );
    }
}

// ---- the Flow trailer ------------------------------------------------------------------------------------------

const S_FLOW: &str = "End the message of your final commit with a blank line and then, as its very last line, `Flow: quick` or `Flow: full`, naming the path you took (`Flow: full` if a quick task grew into the eight steps).";
const S_QUICK_OPEN: &str = "End the message of your final commit with a blank line and then, as its very last line, `Flow: quick`.";
const S_Q5: &str =
    "End the commit message with a blank line and then, as its very last line, `Flow: quick`.";
const S_STEP7: &str = "The final commit says what, why and the test evidence. End its message with a blank line and then, as its very last line, `Flow: full`.";

fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_text_asks_for_the_flow_trailer_where_it_names_a_commit_and_not_for_a_first_line() {
    for (flow, wants) in [
        (LifecycleFlow::Auto, vec![S_FLOW, S_Q5, S_STEP7]),
        (LifecycleFlow::Quick, vec![S_QUICK_OPEN, S_Q5]),
        (LifecycleFlow::Full, vec![S_STEP7]),
    ] {
        let text = flat(pack::contract_text(flow));
        for want in wants {
            assert!(text.contains(want), "{} lacks {want:?}", flow.word());
        }
        assert!(
            !text.contains("Your last message begins"),
            "{} still asks the last message to begin with the Flow line",
            flow.word()
        );
    }
    assert!(!flat(pack::contract_text(LifecycleFlow::Quick)).contains("Flow: full"));
    assert!(!flat(pack::contract_text(LifecycleFlow::Full)).contains("Flow: quick"));
}

#[test]
fn the_history_names_the_three_1_3_0_texts_and_an_install_of_any_flow_replaces_them() {
    // Pack 1.4.0 changed all three texts. A machine that holds an unedited 1.3.0 text of any flow gets the new one from `ham rules init`, and an edited copy is kept.
    for (label, old) in [
        ("auto", OLD_AUTO_1_3_0),
        ("full", OLD_FULL_1_3_0),
        ("quick", OLD_QUICK_1_3_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.3.0"),
            "{label}"
        );
        for flow in LifecycleFlow::ALL {
            let rule = scratch(&format!("rule-1-3-0-{label}-{}", flow.word()));
            std::fs::write(rule.join(pack::CONTRACT_RULE), old).unwrap();
            assert_eq!(
                pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::Updated {
                    was: "1.3.0".to_string()
                },
                "{label} asked for {}",
                flow.word()
            );
            assert_eq!(
                std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
                pack::contract_text(flow)
            );
            let mine = scratch(&format!("rule-edited-1-3-0-{label}-{}", flow.word()));
            let edited = format!("{old}\nMy own line.\n");
            std::fs::write(mine.join(pack::CONTRACT_RULE), &edited).unwrap();
            assert_eq!(
                pack::install_rule(&mine, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::KeptEdited,
                "{label} edited, asked for {}",
                flow.word()
            );
        }
    }
}

#[test]
fn the_history_names_the_1_3_0_delivery_skill_and_an_install_replaces_it_and_keeps_an_edited_one() {
    // Pack 1.4.0 added the order of the end of the final commit message to step 5 of the delivery skill (the DOX line, a blank line, the Flow line).
    let path = ".agents/skills/lifecycle-delivery/SKILL.md";
    assert_eq!(
        History::pack().earlier_version(path, OLD_DELIVERY_1_3_0.as_bytes()),
        Some("1.3.0")
    );
    let current = pack::files().iter().find(|f| f.path == path).unwrap();
    assert!(
        flat(current.content).contains("`Flow: full`"),
        "the current skill names the Flow line"
    );
    let skills = scratch("skill-1-3-0-delivery");
    std::fs::create_dir_all(skills.join("lifecycle-delivery")).unwrap();
    std::fs::write(
        skills.join("lifecycle-delivery").join("SKILL.md"),
        OLD_DELIVERY_1_3_0,
    )
    .unwrap();
    assert_eq!(
        pack::install_skill_file(&skills, "lifecycle-delivery", "SKILL.md", current.content)
            .unwrap(),
        Installed::Updated {
            was: "1.3.0".to_string()
        }
    );
    let mine = scratch("skill-edited-1-3-0-delivery");
    std::fs::create_dir_all(mine.join("lifecycle-delivery")).unwrap();
    std::fs::write(
        mine.join("lifecycle-delivery").join("SKILL.md"),
        format!("{OLD_DELIVERY_1_3_0}\nMy own line.\n"),
    )
    .unwrap();
    assert_eq!(
        pack::install_skill_file(&mine, "lifecycle-delivery", "SKILL.md", current.content).unwrap(),
        Installed::KeptEdited
    );
}

// ---- a missing tool is a NOT RUN line -------------------------------------------------------------------------

/// The 1.4.0 wording named "a missing tool that the checkout's own checks need" as a cause of BLOCKED without condition.
const S_TOOL_OK: &str = "A missing tool that stops only some checks, or a check this session cannot run, is no blocker and no failure: commit the change, run the checks that can run, write a NOT RUN line in your last message for each check it stops, and finish; the goal is complete for the part that can be done.";
const S_TOOL_BLOCKED: &str =
    "or a missing tool without which no part of the change can be made and proved.";
const OLD_TOOL: &str = "a missing tool that the checkout's own checks need";

#[test]
fn a_missing_tool_that_stops_only_some_checks_is_a_not_run_line_and_blocked_keeps_the_stall_and_the_password()
 {
    for (flow, stall, goal) in [
        (
            LifecycleFlow::Full,
            "the stall rule of step 6",
            "call it only when every check that can run passed in a VERIFY run made after your last edit",
        ),
        (
            LifecycleFlow::Quick,
            "three fixes on three different theories that leave one check failing",
            "call it only when every check of Q3 that can run passed after your last edit",
        ),
        (
            LifecycleFlow::Auto,
            "the stall rule of step 6 (in the quick path: three fixes on three theories that leave one check failing)",
            "call it only when every check that can run passed in such a run made after your last edit",
        ),
    ] {
        let text = flat(pack::contract_text(flow));
        let w = flow.word();
        assert!(text.contains(S_TOOL_OK), "{w} lacks the NOT RUN sentence");
        assert!(
            !text.contains(OLD_TOOL),
            "{w} still names a missing tool as a cause of BLOCKED"
        );
        let blocked = format!(
            "Use BLOCKED: only for work inside this checkout that cannot go on: {stall}, a needed persistent password, {S_TOOL_BLOCKED} Put it on the last line and name only that cause."
        );
        assert!(text.contains(&blocked), "{w} lacks the BLOCKED sentence");
        assert_eq!(text.matches("Use BLOCKED:").count(), 1, "{w}");
        assert!(
            text.find(S_TOOL_OK).unwrap() < text.find("Use BLOCKED:").unwrap(),
            "{w}: the NOT RUN sentence comes before the BLOCKED sentence"
        );
        assert!(text.contains(goal), "{w} lacks the goal-complete wording");
        assert!(
            !text.contains("every check passed") && !text.contains("every check of Q3 passed"),
            "{w}: no goal prompt waits for a check that a missing tool stops"
        );
        // The rule after BLOCKED: stays as it was.
        assert!(
            text.contains("After BLOCKED:, while the named blocker is unchanged, change no file")
        );
        assert!(text.contains("stop and end with BLOCKED:."));
    }
}

#[test]
fn the_history_names_the_three_1_4_0_texts_and_an_install_of_any_flow_replaces_them() {
    // Pack 1.5.0 changed all three texts again. A machine that holds an unedited 1.4.0 text of any flow gets the new one from `ham rules init`, and an edited copy is kept.
    for (label, old) in [
        ("auto", OLD_AUTO_1_4_0),
        ("full", OLD_FULL_1_4_0),
        ("quick", OLD_QUICK_1_4_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.4.0"),
            "{label}"
        );
        for flow in LifecycleFlow::ALL {
            let rule = scratch(&format!("rule-1-4-0-{label}-{}", flow.word()));
            std::fs::write(rule.join(pack::CONTRACT_RULE), old).unwrap();
            assert_eq!(
                pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::Updated {
                    was: "1.4.0".to_string()
                },
                "{label} asked for {}",
                flow.word()
            );
            assert_eq!(
                std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
                pack::contract_text(flow)
            );
            let mine = scratch(&format!("rule-edited-1-4-0-{label}-{}", flow.word()));
            let edited = format!("{old}\nMy own line.\n");
            std::fs::write(mine.join(pack::CONTRACT_RULE), &edited).unwrap();
            assert_eq!(
                pack::install_rule(&mine, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::KeptEdited,
                "{label} edited, asked for {}",
                flow.word()
            );
        }
    }
}

#[test]
fn the_history_names_the_three_1_12_0_texts_and_an_install_of_any_flow_replaces_them() {
    // Pack 1.13.0 changed all three texts: the failing test is run with the repository's own test command before any other edit. A machine that holds an unedited 1.12.0 text of any flow
    // gets the new one from `ham rules init`, and an edited copy is kept.
    for (label, old) in [
        ("auto", OLD_AUTO_1_12_0),
        ("full", OLD_FULL_1_12_0),
        ("quick", OLD_QUICK_1_12_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.12.0"),
            "{label}"
        );
        for flow in LifecycleFlow::ALL {
            let rule = scratch(&format!("rule-1-12-0-{label}-{}", flow.word()));
            std::fs::write(rule.join(pack::CONTRACT_RULE), old).unwrap();
            assert_eq!(
                pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::Updated {
                    was: "1.12.0".to_string()
                },
                "{label} asked for {}",
                flow.word()
            );
            assert_eq!(
                std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
                pack::contract_text(flow)
            );
            let mine = scratch(&format!("rule-edited-1-12-0-{label}-{}", flow.word()));
            let edited = format!("{old}\nMy own line.\n");
            std::fs::write(mine.join(pack::CONTRACT_RULE), &edited).unwrap();
            assert_eq!(
                pack::install_rule(&mine, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::KeptEdited,
                "{label} edited, asked for {}",
                flow.word()
            );
        }
    }
}

#[test]
fn the_history_names_the_three_1_7_0_texts_and_an_install_of_any_flow_replaces_them() {
    // Pack 1.8.0 changed all three texts again. A machine that holds an unedited 1.7.0 text of any flow gets the new one from `ham rules init`, and an edited copy is kept.
    for (label, old) in [
        ("auto", OLD_AUTO_1_7_0),
        ("full", OLD_FULL_1_7_0),
        ("quick", OLD_QUICK_1_7_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.7.0"),
            "{label}"
        );
        for flow in LifecycleFlow::ALL {
            let rule = scratch(&format!("rule-1-7-0-{label}-{}", flow.word()));
            std::fs::write(rule.join(pack::CONTRACT_RULE), old).unwrap();
            assert_eq!(
                pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::Updated {
                    was: "1.7.0".to_string()
                },
                "{label} asked for {}",
                flow.word()
            );
            assert_eq!(
                std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
                pack::contract_text(flow)
            );
            let mine = scratch(&format!("rule-edited-1-7-0-{label}-{}", flow.word()));
            let edited = format!("{old}\nMy own line.\n");
            std::fs::write(mine.join(pack::CONTRACT_RULE), &edited).unwrap();
            assert_eq!(
                pack::install_rule(&mine, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::KeptEdited,
                "{label} edited, asked for {}",
                flow.word()
            );
        }
    }
}

#[test]
fn the_history_names_the_three_1_6_0_texts_and_an_install_of_any_flow_replaces_them() {
    // Pack 1.7.0 changed all three texts again. A machine that holds an unedited 1.6.0 text of any flow gets the new one from `ham rules init`, and an edited copy is kept.
    for (label, old) in [
        ("auto", OLD_AUTO_1_6_0),
        ("full", OLD_FULL_1_6_0),
        ("quick", OLD_QUICK_1_6_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.6.0"),
            "{label}"
        );
        for flow in LifecycleFlow::ALL {
            let rule = scratch(&format!("rule-1-6-0-{label}-{}", flow.word()));
            std::fs::write(rule.join(pack::CONTRACT_RULE), old).unwrap();
            assert_eq!(
                pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::Updated {
                    was: "1.6.0".to_string()
                },
                "{label} asked for {}",
                flow.word()
            );
            assert_eq!(
                std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
                pack::contract_text(flow)
            );
            let mine = scratch(&format!("rule-edited-1-6-0-{label}-{}", flow.word()));
            let edited = format!("{old}\nMy own line.\n");
            std::fs::write(mine.join(pack::CONTRACT_RULE), &edited).unwrap();
            assert_eq!(
                pack::install_rule(&mine, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::KeptEdited,
                "{label} edited, asked for {}",
                flow.word()
            );
        }
    }
}

#[test]
fn the_history_names_the_three_1_5_0_texts_and_an_install_of_any_flow_replaces_them() {
    // Pack 1.6.0 changed all three texts again. A machine that holds an unedited 1.5.0 text of any flow gets the new one from `ham rules init`, and an edited copy is kept.
    for (label, old) in [
        ("auto", OLD_AUTO_1_5_0),
        ("full", OLD_FULL_1_5_0),
        ("quick", OLD_QUICK_1_5_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.5.0"),
            "{label}"
        );
        for flow in LifecycleFlow::ALL {
            let rule = scratch(&format!("rule-1-5-0-{label}-{}", flow.word()));
            std::fs::write(rule.join(pack::CONTRACT_RULE), old).unwrap();
            assert_eq!(
                pack::install_rule(&rule, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::Updated {
                    was: "1.5.0".to_string()
                },
                "{label} asked for {}",
                flow.word()
            );
            assert_eq!(
                std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
                pack::contract_text(flow)
            );
            let mine = scratch(&format!("rule-edited-1-5-0-{label}-{}", flow.word()));
            let edited = format!("{old}\nMy own line.\n");
            std::fs::write(mine.join(pack::CONTRACT_RULE), &edited).unwrap();
            assert_eq!(
                pack::install_rule(&mine, pack::CONTRACT_RULE, pack::contract_text(flow)).unwrap(),
                Installed::KeptEdited,
                "{label} edited, asked for {}",
                flow.word()
            );
        }
    }
}

#[test]
fn the_history_names_the_1_4_0_loop_skill_and_an_install_replaces_it_and_keeps_an_edited_one() {
    // Pack 1.5.0 named the checks that can run in steps 5 and 7 of the loop skill (the goal tool is called when every check that can run passed).
    let path = ".agents/skills/lifecycle-loop/SKILL.md";
    assert_eq!(
        History::pack().earlier_version(path, OLD_LOOP_1_4_0.as_bytes()),
        Some("1.4.0")
    );
    let current = pack::files().iter().find(|f| f.path == path).unwrap();
    let text = flat(current.content);
    assert!(
        text.contains("call it only after every check that can run passed in a VERIFY run"),
        "the current skill names the checks that can run"
    );
    assert!(!text.contains("every check passed"));
    let skills = scratch("skill-1-4-0-loop");
    std::fs::create_dir_all(skills.join("lifecycle-loop")).unwrap();
    std::fs::write(
        skills.join("lifecycle-loop").join("SKILL.md"),
        OLD_LOOP_1_4_0,
    )
    .unwrap();
    assert_eq!(
        pack::install_skill_file(&skills, "lifecycle-loop", "SKILL.md", current.content).unwrap(),
        Installed::Updated {
            was: "1.4.0".to_string()
        }
    );
    let mine = scratch("skill-edited-1-4-0-loop");
    std::fs::create_dir_all(mine.join("lifecycle-loop")).unwrap();
    std::fs::write(
        mine.join("lifecycle-loop").join("SKILL.md"),
        format!("{OLD_LOOP_1_4_0}\nMy own line.\n"),
    )
    .unwrap();
    assert_eq!(
        pack::install_skill_file(&mine, "lifecycle-loop", "SKILL.md", current.content).unwrap(),
        Installed::KeptEdited
    );
}
