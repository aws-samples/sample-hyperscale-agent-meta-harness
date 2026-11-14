//! The pack's history of earlier texts: `lifecycle/pack.history.json` lists, for each pack path, the SHA-256 of every
//! earlier text and the last pack version that held it. The installers that write into the user's own folders (`install_rule`,
//! `install_skill_file`) replace an installed file that equals one of those texts, because nobody edited it, and keep every other file. The
//! installer that writes into a repository (`install_repo`) never replaces anything. The earlier texts used here are the files of
//! `fixtures/lifecycle/pack-earlier/`, taken from git; their hashes are the ones the history lists.
#![allow(clippy::unwrap_used)]

use ham_core::pack::{self, History, Installed};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DELIVERY: &str = ".agents/skills/lifecycle-delivery/SKILL.md";
const CHECK_SCRIPT: &str = ".agents/skills/lifecycle-check/scripts/lifecycle_check.py";
const CHECK_SKILL: &str = ".agents/skills/lifecycle-check/SKILL.md";
const KIT_SCRIPT: &str = ".agents/skills/lifecycle-check/scripts/scanner_kit.py";
const LOOP: &str = ".agents/skills/lifecycle-loop/SKILL.md";

const OLD_CONTRACT_1_0_6: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.6-AGENTS.md.txt");
const OLD_DELIVERY_1_0_7: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.7-lifecycle-delivery-SKILL.md.txt");
const OLD_SCRIPT_1_0_7: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.7-lifecycle_check.py.txt");
const OLD_CONTRACT_1_0_8: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.8-AGENTS.md.txt");
const OLD_DELIVERY_1_0_8: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.8-lifecycle-delivery-SKILL.md.txt");
const OLD_SCRIPT_1_0_8: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.8-lifecycle_check.py.txt");
const OLD_CONTRACT_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-AGENTS.md.txt");
const OLD_DELIVERY_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-lifecycle-delivery-SKILL.md.txt");
const OLD_LOOP_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-lifecycle-loop-SKILL.md.txt");
const OLD_SCRIPT_1_0_9: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.0.9-lifecycle_check.py.txt");
const OLD_CONTRACT_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-AGENTS.md.txt");
const OLD_DELIVERY_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-lifecycle-delivery-SKILL.md.txt");
const OLD_LOOP_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-lifecycle-loop-SKILL.md.txt");
const OLD_SCRIPT_1_1_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.1.0-lifecycle_check.py.txt");
const OLD_SCRIPT_1_8_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.8.0-lifecycle_check.py.txt");
const OLD_CHECK_SKILL_1_9_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.9.0-lifecycle-check-SKILL.md.txt");
const OLD_CHECK_SKILL_1_10_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.10.0-lifecycle-check-SKILL.md.txt");
const OLD_KIT_1_10_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.10.0-scanner_kit.py.txt");

const OLD_DELIVERY_1_13_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.13.0-lifecycle-delivery-SKILL.md.txt");
const OLD_STARTER_1_13_0: &str = include_str!(
    "../../../fixtures/lifecycle/pack-earlier/1.13.0-lifecycle-delivery-dox-root.md.txt"
);
const OLD_BUILD_TEST_1_13_0: &str = include_str!(
    "../../../fixtures/lifecycle/pack-earlier/1.13.0-lifecycle-build-test-SKILL.md.txt"
);

const OLD_TDD_1_12_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.12.0-lifecycle-tdd-SKILL.md.txt");

const OLD_CONTRACT_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-AGENTS.md.txt");
const OLD_CONTRACT_FULL_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-AGENTS-full.md.txt");
const OLD_CONTRACT_QUICK_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-AGENTS-quick.md.txt");
const OLD_BUILD_TEST_1_11_0: &str = include_str!(
    "../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-build-test-SKILL.md.txt"
);
const OLD_CHECK_SKILL_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-check-SKILL.md.txt");
const OLD_CONTEXT_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-context-SKILL.md.txt");
const OLD_DELIVERY_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-delivery-SKILL.md.txt");
const OLD_LOOP_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-loop-SKILL.md.txt");
const OLD_RESEARCH_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-research-SKILL.md.txt");
const OLD_TDD_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle-tdd-SKILL.md.txt");
const OLD_CHECK_SCRIPT_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-lifecycle_check.py.txt");
const OLD_KIT_1_11_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.11.0-scanner_kit.py.txt");

const OLD_CHECK_1_14_0: &str =
    include_str!("../../../fixtures/lifecycle/pack-earlier/1.14.0-lifecycle_check.py.txt");

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A scratch folder under the temp dir (set by the project's env.sh); like the other `ham` tests it is not removed by the test.
fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-pack-history-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pack_text(path: &str) -> &'static str {
    pack::files()
        .iter()
        .find(|f| f.path == path)
        .unwrap()
        .content
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The names in a folder, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    out.sort();
    out
}

fn lock() -> serde_json::Value {
    serde_json::from_str(pack::LOCK).unwrap()
}

// ---- the history file ---------------------------------------------------------------------------------------------------------

#[test]
fn the_history_lists_earlier_hashes_of_the_contract_and_never_the_current_one() {
    let history = History::pack();
    let contract = history.hashes("AGENTS.md");
    assert!(
        contract.len() >= 4,
        "the contract text changed several times before 1.0.8: {contract:?}"
    );
    for (digest, version) in &contract {
        assert_eq!(digest.len(), 64, "{digest}");
        assert!(version.split('.').count() == 3, "{version}");
    }
    // No listed hash is the current text of any path (the history lists earlier texts only).
    let lock = lock();
    let current: Vec<&str> = lock["files"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_str().unwrap())
        .collect();
    for path in history.paths() {
        for (digest, version) in history.hashes(path) {
            assert!(
                !current.contains(&digest),
                "{path}: {digest} (pack {version}) is a current text"
            );
        }
    }
    for f in pack::files() {
        assert_eq!(
            history.earlier_version(f.path, f.content.as_bytes()),
            None,
            "{}: the current text is not an earlier text",
            f.path
        );
    }
}

#[test]
fn the_embedded_history_is_the_history_on_disk_and_parses_whole() {
    let on_disk = std::fs::read_to_string(repo().join("lifecycle/pack.history.json")).unwrap();
    assert_eq!(pack::HISTORY, on_disk);
    let parsed = History::parse(pack::HISTORY).unwrap();
    assert_eq!(
        &parsed,
        History::pack(),
        "the pack's history is the parsed file"
    );
    assert!(
        parsed.paths().count() >= 9,
        "every file with a past is listed: {:?}",
        parsed.paths().collect::<Vec<_>>()
    );
}

#[test]
fn every_version_in_the_history_is_older_than_the_pack() {
    let now: Vec<u32> = pack::version()
        .split('.')
        .map(|n| n.parse().unwrap())
        .collect();
    let history = History::pack();
    for path in history.paths() {
        for (digest, version) in history.hashes(path) {
            let v: Vec<u32> = version.split('.').map(|n| n.parse().unwrap()).collect();
            assert!(
                v < now,
                "{path} {digest}: pack {version} is not older than {now:?}"
            );
        }
    }
}

#[test]
fn the_history_names_the_1_0_7_delivery_skill_and_script_and_the_1_0_6_contract() {
    let history = History::pack();
    assert_eq!(
        history.earlier_version(DELIVERY, OLD_DELIVERY_1_0_7.as_bytes()),
        Some("1.0.7")
    );
    assert_eq!(
        history.earlier_version(CHECK_SCRIPT, OLD_SCRIPT_1_0_7.as_bytes()),
        Some("1.0.7")
    );
    assert_eq!(
        history.earlier_version("AGENTS.md", OLD_CONTRACT_1_0_6.as_bytes()),
        Some("1.0.6")
    );
    // The history is kept per path: a text listed for one path says nothing about another.
    assert_eq!(
        history.earlier_version(DELIVERY, OLD_CONTRACT_1_0_6.as_bytes()),
        None
    );
    assert_eq!(
        history.earlier_version("AGENTS.md", OLD_DELIVERY_1_0_7.as_bytes()),
        None
    );
    assert_eq!(
        history.earlier_version("not/a/pack/file.md", OLD_DELIVERY_1_0_7.as_bytes()),
        None
    );
    assert_eq!(history.earlier_version(DELIVERY, b""), None);
}

// Pack 1.0.9 changed three files; a machine that holds their unedited 1.0.8 texts gets the new ones from `ham rules init`.

#[test]
fn the_history_names_the_1_0_8_self_check_script_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(CHECK_SCRIPT, OLD_SCRIPT_1_0_8.as_bytes()),
        Some("1.0.8")
    );
    let skills = scratch("script-1-0-8");
    write(
        &skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_0_8,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "scripts/lifecycle_check.py",
            pack_text(CHECK_SCRIPT)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.0.8".to_string()
        }
    );
}

#[test]
fn the_history_names_the_1_0_8_contract_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version("AGENTS.md", OLD_CONTRACT_1_0_8.as_bytes()),
        Some("1.0.8")
    );
    let rule = scratch("rule-1-0-8");
    write(&rule.join(pack::CONTRACT_RULE), OLD_CONTRACT_1_0_8);
    assert_eq!(
        pack::install_rule(&rule, pack::CONTRACT_RULE, pack::CONTRACT).unwrap(),
        Installed::Updated {
            was: "1.0.8".to_string()
        }
    );
    assert_eq!(
        std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
        pack::CONTRACT
    );
}

#[test]
fn the_history_names_the_1_0_8_delivery_skill_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(DELIVERY, OLD_DELIVERY_1_0_8.as_bytes()),
        Some("1.0.8")
    );
    let skills = scratch("delivery-1-0-8");
    write(
        &skills.join("lifecycle-delivery/SKILL.md"),
        OLD_DELIVERY_1_0_8,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-delivery",
            "SKILL.md",
            pack_text(DELIVERY)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.0.8".to_string()
        }
    );
}

// Pack 1.1.0 changed nine files; the four whose 1.0.9 texts are kept in `fixtures/lifecycle/pack-earlier/` show that a machine that holds them
// unedited gets the new ones from `ham rules init`.

#[test]
fn the_history_names_the_1_0_9_self_check_script_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(CHECK_SCRIPT, OLD_SCRIPT_1_0_9.as_bytes()),
        Some("1.0.9")
    );
    let skills = scratch("script-1-0-9");
    write(
        &skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_0_9,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "scripts/lifecycle_check.py",
            pack_text(CHECK_SCRIPT)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.0.9".to_string()
        }
    );
}

#[test]
fn the_history_names_the_1_0_9_contract_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version("AGENTS.md", OLD_CONTRACT_1_0_9.as_bytes()),
        Some("1.0.9")
    );
    let rule = scratch("rule-1-0-9");
    write(&rule.join(pack::CONTRACT_RULE), OLD_CONTRACT_1_0_9);
    assert_eq!(
        pack::install_rule(&rule, pack::CONTRACT_RULE, pack::CONTRACT).unwrap(),
        Installed::Updated {
            was: "1.0.9".to_string()
        }
    );
    assert_eq!(
        std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
        pack::CONTRACT
    );
}

#[test]
fn the_history_names_the_1_0_9_delivery_skill_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(DELIVERY, OLD_DELIVERY_1_0_9.as_bytes()),
        Some("1.0.9")
    );
    let skills = scratch("delivery-1-0-9");
    write(
        &skills.join("lifecycle-delivery/SKILL.md"),
        OLD_DELIVERY_1_0_9,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-delivery",
            "SKILL.md",
            pack_text(DELIVERY)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.0.9".to_string()
        }
    );
}

#[test]
fn the_history_names_the_1_0_9_loop_skill_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(LOOP, OLD_LOOP_1_0_9.as_bytes()),
        Some("1.0.9")
    );
    let skills = scratch("loop-1-0-9");
    write(&skills.join("lifecycle-loop/SKILL.md"), OLD_LOOP_1_0_9);
    assert_eq!(
        pack::install_skill_file(&skills, "lifecycle-loop", "SKILL.md", pack_text(LOOP)).unwrap(),
        Installed::Updated {
            was: "1.0.9".to_string()
        }
    );
}

// Pack 1.2.0 put the records in the hidden folder `.ham` and changed nine files; the four whose 1.1.0 texts are kept in
// `fixtures/lifecycle/pack-earlier/` show that a machine that holds them unedited gets the new ones from `ham rules init`.

#[test]
fn the_history_names_the_1_1_0_self_check_script_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(CHECK_SCRIPT, OLD_SCRIPT_1_1_0.as_bytes()),
        Some("1.1.0")
    );
    let skills = scratch("script-1-1-0");
    write(
        &skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_1_0,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "scripts/lifecycle_check.py",
            pack_text(CHECK_SCRIPT)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.1.0".to_string()
        }
    );
}

#[test]
fn the_history_names_the_1_1_0_contract_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version("AGENTS.md", OLD_CONTRACT_1_1_0.as_bytes()),
        Some("1.1.0")
    );
    let rule = scratch("rule-1-1-0");
    write(&rule.join(pack::CONTRACT_RULE), OLD_CONTRACT_1_1_0);
    assert_eq!(
        pack::install_rule(&rule, pack::CONTRACT_RULE, pack::CONTRACT).unwrap(),
        Installed::Updated {
            was: "1.1.0".to_string()
        }
    );
    assert_eq!(
        std::fs::read_to_string(rule.join(pack::CONTRACT_RULE)).unwrap(),
        pack::CONTRACT
    );
}

#[test]
fn the_history_names_the_1_1_0_delivery_skill_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(DELIVERY, OLD_DELIVERY_1_1_0.as_bytes()),
        Some("1.1.0")
    );
    let skills = scratch("delivery-1-1-0");
    write(
        &skills.join("lifecycle-delivery/SKILL.md"),
        OLD_DELIVERY_1_1_0,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-delivery",
            "SKILL.md",
            pack_text(DELIVERY)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.1.0".to_string()
        }
    );
}

#[test]
fn the_history_names_the_1_1_0_loop_skill_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(LOOP, OLD_LOOP_1_1_0.as_bytes()),
        Some("1.1.0")
    );
    let skills = scratch("loop-1-1-0");
    write(&skills.join("lifecycle-loop/SKILL.md"), OLD_LOOP_1_1_0);
    assert_eq!(
        pack::install_skill_file(&skills, "lifecycle-loop", "SKILL.md", pack_text(LOOP)).unwrap(),
        Installed::Updated {
            was: "1.1.0".to_string()
        }
    );
}

// Pack 1.9.0 changed the comments of the self-check script and nothing else. A machine that holds the unedited 1.8.0
// script gets the new one from `ham rules init`, and an edited copy is kept.
#[test]
fn the_history_names_the_1_8_0_self_check_script_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(CHECK_SCRIPT, OLD_SCRIPT_1_8_0.as_bytes()),
        Some("1.8.0")
    );
    let skills = scratch("script-1-8-0");
    write(
        &skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        OLD_SCRIPT_1_8_0,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "scripts/lifecycle_check.py",
            pack_text(CHECK_SCRIPT)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.8.0".to_string()
        }
    );
    write(
        &skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        &format!("{OLD_SCRIPT_1_8_0}# edited by the user\n"),
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "scripts/lifecycle_check.py",
            pack_text(CHECK_SCRIPT)
        )
        .unwrap(),
        Installed::KeptEdited
    );
}

// Pack 1.10.0 added scanner_kit.py to the lifecycle-check skill and rewrote the skill's text. A machine that holds the
// unedited 1.9.0 skill gets the new one from `ham rules init`, an edited copy is kept, and the new script is created beside the old one.
#[test]
fn the_history_names_the_1_9_0_check_skill_and_an_install_replaces_it() {
    assert_eq!(
        History::pack().earlier_version(CHECK_SKILL, OLD_CHECK_SKILL_1_9_0.as_bytes()),
        Some("1.9.0")
    );
    let skills = scratch("check-skill-1-9-0");
    write(
        &skills.join("lifecycle-check/SKILL.md"),
        OLD_CHECK_SKILL_1_9_0,
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "SKILL.md",
            pack_text(CHECK_SKILL)
        )
        .unwrap(),
        Installed::Updated {
            was: "1.9.0".to_string()
        }
    );
    write(
        &skills.join("lifecycle-check/SKILL.md"),
        &format!("{OLD_CHECK_SKILL_1_9_0}\nmy own line\n"),
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "SKILL.md",
            pack_text(CHECK_SKILL)
        )
        .unwrap(),
        Installed::KeptEdited
    );
}

// Pack 1.11.0 added bandit and a secret-shape check to the kit: the skill text and the script changed. A machine that holds the
// unedited 1.10.0 files gets the new ones from `ham rules init`, and an edited copy of either is kept.
#[test]
fn the_history_names_the_1_10_0_check_skill_and_kit_script_and_an_install_replaces_them() {
    for (path, rel, old, label) in [
        (
            CHECK_SKILL,
            "SKILL.md",
            OLD_CHECK_SKILL_1_10_0,
            "check-skill-1-10-0",
        ),
        (
            KIT_SCRIPT,
            "scripts/scanner_kit.py",
            OLD_KIT_1_10_0,
            "kit-script-1-10-0",
        ),
    ] {
        assert_eq!(
            History::pack().earlier_version(path, old.as_bytes()),
            Some("1.10.0"),
            "{path}"
        );
        let skills = scratch(label);
        write(&skills.join("lifecycle-check").join(rel), old);
        assert_eq!(
            pack::install_skill_file(&skills, "lifecycle-check", rel, pack_text(path)).unwrap(),
            Installed::Updated {
                was: "1.10.0".to_string()
            },
            "{path}"
        );
        assert_eq!(
            std::fs::read_to_string(skills.join("lifecycle-check").join(rel)).unwrap(),
            pack_text(path)
        );
        write(
            &skills.join("lifecycle-check").join(rel),
            &format!("{old}\n# my own line\n"),
        );
        assert_eq!(
            pack::install_skill_file(&skills, "lifecycle-check", rel, pack_text(path)).unwrap(),
            Installed::KeptEdited,
            "{path}"
        );
    }
}

// Pack 1.12.0 (the rename to ham) changed ten of the twelve files: the contract, the seven skills and the two scripts. A machine that holds the
// unedited 1.11.0 text of any of them gets the new one from `ham rules init`, and an edited copy is kept. The three contract texts of 1.11.0 (the
// `auto` text the pack lists, and the `full` and `quick` texts that the install of a flow looks up under the same path) are in the history too.
#[test]
fn the_history_names_the_1_11_0_texts_and_an_install_replaces_them() {
    for (label, old) in [
        ("auto", OLD_CONTRACT_1_11_0),
        ("full", OLD_CONTRACT_FULL_1_11_0),
        ("quick", OLD_CONTRACT_QUICK_1_11_0),
    ] {
        assert_eq!(
            History::pack().earlier_version("AGENTS.md", old.as_bytes()),
            Some("1.11.0"),
            "the {label} contract"
        );
        assert_ne!(
            old,
            pack::CONTRACT,
            "the {label} contract is an earlier text"
        );
    }
    for (skill, rel, old, label) in [
        (
            "lifecycle-build-test",
            "SKILL.md",
            OLD_BUILD_TEST_1_11_0,
            "build-test",
        ),
        (
            "lifecycle-check",
            "SKILL.md",
            OLD_CHECK_SKILL_1_11_0,
            "check-skill",
        ),
        (
            "lifecycle-context",
            "SKILL.md",
            OLD_CONTEXT_1_11_0,
            "context",
        ),
        (
            "lifecycle-delivery",
            "SKILL.md",
            OLD_DELIVERY_1_11_0,
            "delivery",
        ),
        ("lifecycle-loop", "SKILL.md", OLD_LOOP_1_11_0, "loop"),
        (
            "lifecycle-research",
            "SKILL.md",
            OLD_RESEARCH_1_11_0,
            "research",
        ),
        ("lifecycle-tdd", "SKILL.md", OLD_TDD_1_11_0, "tdd"),
        (
            "lifecycle-check",
            "scripts/lifecycle_check.py",
            OLD_CHECK_SCRIPT_1_11_0,
            "check-script",
        ),
        (
            "lifecycle-check",
            "scripts/scanner_kit.py",
            OLD_KIT_1_11_0,
            "kit-script",
        ),
    ] {
        let path = format!(".agents/skills/{skill}/{rel}");
        assert_eq!(
            History::pack().earlier_version(&path, old.as_bytes()),
            Some("1.11.0"),
            "{path}"
        );
        let skills = scratch(&format!("{label}-1-11-0"));
        write(&skills.join(skill).join(rel), old);
        assert_eq!(
            pack::install_skill_file(&skills, skill, rel, pack_text(&path)).unwrap(),
            Installed::Updated {
                was: "1.11.0".to_string()
            },
            "{path}"
        );
        assert_eq!(
            std::fs::read_to_string(skills.join(skill).join(rel)).unwrap(),
            pack_text(&path)
        );
        write(
            &skills.join(skill).join(rel),
            &format!("{old}\n# my own line\n"),
        );
        assert_eq!(
            pack::install_skill_file(&skills, skill, rel, pack_text(&path)).unwrap(),
            Installed::KeptEdited,
            "{path}"
        );
    }
}

#[test]
fn an_install_adds_the_kit_script_beside_the_self_check_of_an_earlier_pack() {
    let skills = scratch("kit-script");
    write(
        &skills.join("lifecycle-check/scripts/lifecycle_check.py"),
        pack_text(CHECK_SCRIPT),
    );
    assert_eq!(
        pack::install_skill_file(
            &skills,
            "lifecycle-check",
            "scripts/scanner_kit.py",
            pack_text(KIT_SCRIPT)
        )
        .unwrap(),
        Installed::Created
    );
    assert_eq!(
        names(&skills.join("lifecycle-check/scripts")),
        ["lifecycle_check.py", "scanner_kit.py"]
    );
    assert!(
        History::pack()
            .earlier_version(KIT_SCRIPT, pack_text(KIT_SCRIPT).as_bytes())
            .is_none()
    );
}

// Pack 1.13.0 changed the contract and the test-first skill. A machine that holds the unedited 1.12.0 text of the skill gets the new one from `ham rules init`, and an edited copy is kept.
#[test]
fn the_history_names_the_1_12_0_tdd_skill_and_an_install_replaces_it_and_keeps_an_edited_one() {
    let path = ".agents/skills/lifecycle-tdd/SKILL.md";
    assert_eq!(
        History::pack().earlier_version(path, OLD_TDD_1_12_0.as_bytes()),
        Some("1.12.0")
    );
    assert_ne!(
        OLD_TDD_1_12_0,
        pack_text(path),
        "the 1.12.0 skill is an earlier text"
    );
    let skills = scratch("tdd-1-12-0");
    write(&skills.join("lifecycle-tdd/SKILL.md"), OLD_TDD_1_12_0);
    assert_eq!(
        pack::install_skill_file(&skills, "lifecycle-tdd", "SKILL.md", pack_text(path)).unwrap(),
        Installed::Updated {
            was: "1.12.0".to_string()
        }
    );
    assert_eq!(
        std::fs::read_to_string(skills.join("lifecycle-tdd/SKILL.md")).unwrap(),
        pack_text(path)
    );
    write(
        &skills.join("lifecycle-tdd/SKILL.md"),
        &format!("{OLD_TDD_1_12_0}\n# my own line\n"),
    );
    assert_eq!(
        pack::install_skill_file(&skills, "lifecycle-tdd", "SKILL.md", pack_text(path)).unwrap(),
        Installed::KeptEdited
    );
}

// Pack 1.14.0 changed the delivery skill, the build-and-test skill and the DOX starter (the harness now writes the starter before the first prompt). The starter had no
// earlier text in the history before, because it had not changed since pack 1.0.8. A machine that holds the unedited 1.13.0 text of any of the three gets the new one from
// `ham rules init`, and an edited copy is kept.
#[test]
fn b766_the_history_names_the_1_13_0_delivery_skill_build_test_skill_and_starter_and_an_install_replaces_them()
 {
    for (skill, path, old) in [
        ("lifecycle-delivery", "SKILL.md", OLD_DELIVERY_1_13_0),
        (
            "lifecycle-delivery",
            "references/dox-root.md",
            OLD_STARTER_1_13_0,
        ),
        ("lifecycle-build-test", "SKILL.md", OLD_BUILD_TEST_1_13_0),
    ] {
        let pack_path = format!(".agents/skills/{skill}/{path}");
        assert_eq!(
            History::pack().earlier_version(&pack_path, old.as_bytes()),
            Some("1.13.0"),
            "{pack_path}"
        );
        assert_ne!(old, pack_text(&pack_path), "{pack_path}: an earlier text");
        let skills = scratch(&format!("b766-{skill}-{}", path.replace('/', "-")));
        write(&skills.join(skill).join(path), old);
        assert_eq!(
            pack::install_skill_file(&skills, skill, path, pack_text(&pack_path)).unwrap(),
            Installed::Updated {
                was: "1.13.0".to_string()
            },
            "{pack_path}"
        );
        assert_eq!(
            std::fs::read_to_string(skills.join(skill).join(path)).unwrap(),
            pack_text(&pack_path)
        );
        write(
            &skills.join(skill).join(path),
            &format!("{old}\n# my own line\n"),
        );
        assert_eq!(
            pack::install_skill_file(&skills, skill, path, pack_text(&pack_path)).unwrap(),
            Installed::KeptEdited,
            "{pack_path}"
        );
    }
}

#[test]
fn b766_the_starter_tells_when_to_fill_the_project_part_and_still_holds_no_build_or_test_word() {
    let starter = pack_text(".agents/skills/lifecycle-delivery/references/dox-root.md");
    assert!(starter.contains("as soon as you know how to make and check the project"));
    assert!(starter.contains("current as you work"));
    let lower = starter.to_ascii_lowercase();
    for word in ["build", "test"] {
        assert!(
            !lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == word || w == format!("{word}s")),
            "{word}"
        );
    }
    assert!(!starter.contains('`'));
}

#[test]
fn the_pack_is_version_1_16_0_and_its_contract_puts_the_records_in_the_hidden_folder() {
    assert_eq!(pack::version(), "1.16.0");
    assert!(pack::CONTRACT.contains(".ham/lifecycle/GOAL.md"));
    assert!(!pack::CONTRACT.contains("docs/lifecycle"));
}

#[test]
fn parse_refuses_a_bad_hash_a_bad_version_an_extra_key_and_text_that_is_not_json() {
    let good = format!(r#"{{"files": {{"a.md": {{"{}": "1.0.7"}}}}}}"#, sha(b"x"));
    let parsed = History::parse(&good).unwrap();
    assert_eq!(parsed.earlier_version("a.md", b"x"), Some("1.0.7"));
    assert_eq!(parsed.earlier_version("a.md", b"y"), None);
    assert_eq!(
        History::parse(r#"{"files": {}}"#).unwrap().paths().count(),
        0
    );
    for (label, text) in [
        (
            "short hash",
            r#"{"files": {"a.md": {"abc": "1.0.7"}}}"#.to_string(),
        ),
        (
            "upper case hash",
            format!(
                r#"{{"files": {{"a.md": {{"{}": "1.0.7"}}}}}}"#,
                sha(b"x").to_uppercase()
            ),
        ),
        (
            "version word",
            format!(r#"{{"files": {{"a.md": {{"{}": "latest"}}}}}}"#, sha(b"x")),
        ),
        (
            "two part version",
            format!(r#"{{"files": {{"a.md": {{"{}": "1.0"}}}}}}"#, sha(b"x")),
        ),
        (
            "extra key",
            r#"{"files": {}, "version": "1.0.7"}"#.to_string(),
        ),
        ("files is a list", r#"{"files": []}"#.to_string()),
        ("not json", "{nope".to_string()),
        ("empty", String::new()),
    ] {
        assert!(History::parse(&text).is_err(), "{label} is refused");
    }
}

// ---- the installers -----------------------------------------------------------------------------------------------------------

#[test]
fn place_file_updates_a_file_that_equals_an_earlier_pack_version() {
    let dir = scratch("update");
    write(&dir.join("lifecycle-delivery/SKILL.md"), OLD_DELIVERY_1_0_7);
    let current = pack_text(DELIVERY);
    let how = pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", current).unwrap();
    assert_eq!(
        how,
        Installed::Updated {
            was: "1.0.7".to_string()
        }
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("lifecycle-delivery/SKILL.md")).unwrap(),
        current
    );
    assert_eq!(
        names(&dir.join("lifecycle-delivery")),
        ["SKILL.md"],
        "no new file is left beside it"
    );
    // The next install finds the current text.
    assert_eq!(
        pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", current).unwrap(),
        Installed::Same
    );
}

#[test]
fn place_file_keeps_an_edited_file() {
    let dir = scratch("edited");
    let edited = format!("{OLD_DELIVERY_1_0_7}\nMy own line.\n");
    write(&dir.join("lifecycle-delivery/SKILL.md"), &edited);
    let how = pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", pack_text(DELIVERY))
        .unwrap();
    assert_eq!(how, Installed::KeptEdited);
    assert_eq!(
        std::fs::read_to_string(dir.join("lifecycle-delivery/SKILL.md")).unwrap(),
        edited
    );
    assert_eq!(names(&dir.join("lifecycle-delivery")), ["SKILL.md"]);
}

#[test]
fn place_file_keeps_a_file_equal_to_an_earlier_hash_of_another_path() {
    // The 1.0.6 contract is an earlier text of AGENTS.md. Under the delivery skill's name it is somebody's own file.
    let dir = scratch("other-path");
    write(&dir.join("lifecycle-delivery/SKILL.md"), OLD_CONTRACT_1_0_6);
    let how = pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", pack_text(DELIVERY))
        .unwrap();
    assert_eq!(how, Installed::KeptEdited);
    assert_eq!(
        std::fs::read_to_string(dir.join("lifecycle-delivery/SKILL.md")).unwrap(),
        OLD_CONTRACT_1_0_6
    );
    // And the 1.0.7 delivery skill under the name of another skill's file.
    let other = scratch("other-skill");
    write(&other.join("lifecycle-tdd/SKILL.md"), OLD_DELIVERY_1_0_7);
    let how = pack::install_skill_file(
        &other,
        "lifecycle-tdd",
        "SKILL.md",
        pack_text(".agents/skills/lifecycle-tdd/SKILL.md"),
    )
    .unwrap();
    assert_eq!(how, Installed::KeptEdited);
}

#[test]
fn an_update_is_atomic_and_keeps_the_executable_bit_of_a_script() {
    use std::os::unix::fs::PermissionsExt;
    // A history that lists the old script text, so the test needs no copy of the real script.
    let old = "#!/usr/bin/env python3\nprint('old')\n";
    let new = "#!/usr/bin/env python3\nprint('new')\n";
    let text = format!(
        r#"{{"files": {{"{CHECK_SCRIPT}": {{"{}": "1.0.7"}}}}}}"#,
        sha(old.as_bytes())
    );
    let history = History::parse(&text).unwrap();
    let dir = scratch("atomic");
    let script = dir.join("lifecycle-check/scripts/lifecycle_check.py");
    write(&script, old);
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();
    // A second name for the old inode: a write into the file would change what this name reads, a rename over it does not.
    let hold = dir.join("hold-old-inode");
    std::fs::hard_link(&script, &hold).unwrap();
    let how = pack::install_skill_file_with(
        &history,
        &dir,
        "lifecycle-check",
        "scripts/lifecycle_check.py",
        new,
    )
    .unwrap();
    assert_eq!(
        how,
        Installed::Updated {
            was: "1.0.7".to_string()
        }
    );
    assert_eq!(std::fs::read_to_string(&script).unwrap(), new);
    assert_eq!(
        std::fs::read_to_string(&hold).unwrap(),
        old,
        "the old file was replaced, not written into"
    );
    assert_eq!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o750,
        "the mode of the old file is kept"
    );
    assert_eq!(
        names(&dir.join("lifecycle-check/scripts")),
        ["lifecycle_check.py"],
        "no temporary file is left"
    );
}

#[test]
fn an_update_that_cannot_be_written_leaves_the_old_file_and_no_new_one() {
    use std::os::unix::fs::PermissionsExt;
    let text = format!(
        r#"{{"files": {{"{DELIVERY}": {{"{}": "1.0.7"}}}}}}"#,
        sha(OLD_DELIVERY_1_0_7.as_bytes())
    );
    let history = History::parse(&text).unwrap();
    let dir = scratch("readonly");
    let skill = dir.join("lifecycle-delivery");
    write(&skill.join("SKILL.md"), OLD_DELIVERY_1_0_7);
    std::fs::set_permissions(&skill, std::fs::Permissions::from_mode(0o555)).unwrap();
    // A user who may write into a read-only folder (root) cannot show the failure; the check is skipped for them.
    let probe = skill.join(".probe");
    let can_write = std::fs::write(&probe, b"x").is_ok();
    if can_write {
        std::fs::remove_file(&probe).unwrap();
    } else {
        let result = pack::install_skill_file_with(
            &history,
            &dir,
            "lifecycle-delivery",
            "SKILL.md",
            pack_text(DELIVERY),
        );
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(skill.join("SKILL.md")).unwrap(),
            OLD_DELIVERY_1_0_7,
            "the old file is as it was"
        );
        assert_eq!(names(&skill), ["SKILL.md"]);
    }
    std::fs::set_permissions(&skill, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_leftover_new_file_from_a_killed_run_is_skipped_not_reused() {
    let dir = scratch("leftover");
    let skill = dir.join("lifecycle-delivery");
    write(&skill.join("SKILL.md"), OLD_DELIVERY_1_0_7);
    // The name the first attempt would use, left by an earlier run that was killed.
    let stale_name = format!(".SKILL.md.ham-new-{}-0", std::process::id());
    write(&skill.join(&stale_name), "stale\n");
    let how = pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", pack_text(DELIVERY))
        .unwrap();
    assert!(matches!(how, Installed::Updated { .. }), "{how:?}");
    assert_eq!(
        std::fs::read_to_string(skill.join("SKILL.md")).unwrap(),
        pack_text(DELIVERY)
    );
    assert_eq!(
        std::fs::read_to_string(skill.join(&stale_name)).unwrap(),
        "stale\n",
        "the stale file is left as it was"
    );
}

#[test]
fn an_update_with_no_free_new_file_name_fails_and_leaves_the_old_file() {
    let dir = scratch("no-free-name");
    let skill = dir.join("lifecycle-delivery");
    write(&skill.join("SKILL.md"), OLD_DELIVERY_1_0_7);
    for attempt in 0..16 {
        write(
            &skill.join(format!(
                ".SKILL.md.ham-new-{}-{attempt}",
                std::process::id()
            )),
            "stale\n",
        );
    }
    let err = pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", pack_text(DELIVERY))
        .unwrap_err();
    assert!(err.contains("no free name"), "{err}");
    assert_eq!(
        std::fs::read_to_string(skill.join("SKILL.md")).unwrap(),
        OLD_DELIVERY_1_0_7,
        "the old file is as it was"
    );
}

#[test]
fn a_link_where_the_file_belongs_is_still_refused() {
    let dir = scratch("link");
    let elsewhere = scratch("link-target");
    write(&elsewhere.join("real.md"), OLD_DELIVERY_1_0_7);
    std::fs::create_dir_all(dir.join("lifecycle-delivery")).unwrap();
    std::os::unix::fs::symlink(
        elsewhere.join("real.md"),
        dir.join("lifecycle-delivery/SKILL.md"),
    )
    .unwrap();
    let err = pack::install_skill_file(&dir, "lifecycle-delivery", "SKILL.md", pack_text(DELIVERY))
        .unwrap_err();
    assert!(err.contains("symbolic link"), "{err}");
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("real.md")).unwrap(),
        OLD_DELIVERY_1_0_7,
        "nothing was written through the link"
    );
}

#[test]
fn install_repo_never_updates() {
    let repo = scratch("repo");
    write(&repo.join(DELIVERY), OLD_DELIVERY_1_0_7);
    let done = pack::install_repo(&repo, false).unwrap();
    let how = &done.files.iter().find(|(p, _)| p == DELIVERY).unwrap().1;
    assert_eq!(*how, Installed::KeptEdited);
    assert_eq!(
        std::fs::read_to_string(repo.join(DELIVERY)).unwrap(),
        OLD_DELIVERY_1_0_7,
        "a file of a repository is the repository's"
    );
    assert!(
        done.files
            .iter()
            .all(|(_, how)| !matches!(how, Installed::Updated { .. })),
        "{:?}",
        done.files
    );
}

#[test]
fn install_rule_updates_an_unedited_earlier_contract_and_keeps_an_edited_one() {
    let dir = scratch("rule");
    write(&dir.join(pack::CONTRACT_RULE), OLD_CONTRACT_1_0_6);
    let how = pack::install_rule(&dir, pack::CONTRACT_RULE, pack::CONTRACT).unwrap();
    assert_eq!(
        how,
        Installed::Updated {
            was: "1.0.6".to_string()
        }
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(pack::CONTRACT_RULE)).unwrap(),
        pack::CONTRACT
    );
    assert_eq!(names(&dir), ["10-lifecycle.md"], "no new file is left");
    assert_eq!(
        pack::install_rule(&dir, pack::CONTRACT_RULE, pack::CONTRACT).unwrap(),
        Installed::Same
    );

    let edited = scratch("rule-edited");
    let mine = format!("{OLD_CONTRACT_1_0_6}\nMy own rule.\n");
    write(&edited.join(pack::CONTRACT_RULE), &mine);
    assert_eq!(
        pack::install_rule(&edited, pack::CONTRACT_RULE, pack::CONTRACT).unwrap(),
        Installed::KeptEdited
    );
    assert_eq!(
        std::fs::read_to_string(edited.join(pack::CONTRACT_RULE)).unwrap(),
        mine
    );
}

#[test]
fn only_the_contract_rule_has_a_history() {
    // A rule file that is not the contract is the user's, whatever it holds. The text of the old contract in a file of another name stays.
    let dir = scratch("rule-other");
    write(&dir.join("11-mine.md"), OLD_CONTRACT_1_0_6);
    assert_eq!(
        pack::install_rule(&dir, "11-mine.md", pack::CONTRACT).unwrap(),
        Installed::KeptEdited
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("11-mine.md")).unwrap(),
        OLD_CONTRACT_1_0_6
    );
    // The rendered skill rules (`--skills-as-rules`) have no history either.
    let rendered = pack::skills_as_rules();
    let first = rendered.first().unwrap();
    write(&dir.join(&first.path), OLD_DELIVERY_1_0_7);
    assert_eq!(
        pack::install_rule(&dir, &first.path, &first.content).unwrap(),
        Installed::KeptEdited
    );
}

#[test]
fn install_rule_never_replaces_a_link() {
    let dir = scratch("rule-link");
    let elsewhere = scratch("rule-link-target");
    write(&elsewhere.join("dotfile.md"), OLD_CONTRACT_1_0_6);
    std::os::unix::fs::symlink(elsewhere.join("dotfile.md"), dir.join(pack::CONTRACT_RULE))
        .unwrap();
    // A rule that is a link to a file under the user's dotfile manager is the user's: nothing changes, link and target alike.
    assert_eq!(
        pack::install_rule(&dir, pack::CONTRACT_RULE, pack::CONTRACT).unwrap(),
        Installed::KeptEdited
    );
    assert!(
        std::fs::symlink_metadata(dir.join(pack::CONTRACT_RULE))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("dotfile.md")).unwrap(),
        OLD_CONTRACT_1_0_6
    );
}

// Pack 1.15.0 changed the self-check script alone: it names the flow it checks and passes a correct quick-path delivery. A machine that holds the
// unedited 1.14.0 script gets the new one from `ham rules init`, and an edited copy is kept.
#[test]
fn b696_the_history_names_the_1_14_0_self_check_script_and_an_install_replaces_it() {
    let (skill, path) = ("lifecycle-check", "scripts/lifecycle_check.py");
    let pack_path = format!(".agents/skills/{skill}/{path}");
    assert_eq!(
        History::pack().earlier_version(&pack_path, OLD_CHECK_1_14_0.as_bytes()),
        Some("1.14.0")
    );
    assert_ne!(OLD_CHECK_1_14_0, pack_text(&pack_path));
    assert!(pack_text(&pack_path).contains("FLOW %s (%s)"));
    let skills = scratch("b696-check");
    write(&skills.join(skill).join(path), OLD_CHECK_1_14_0);
    assert_eq!(
        pack::install_skill_file(&skills, skill, path, pack_text(&pack_path)).unwrap(),
        Installed::Updated {
            was: "1.14.0".to_string()
        }
    );
    write(
        &skills.join(skill).join(path),
        &format!("{OLD_CHECK_1_14_0}\n# my own line\n"),
    );
    assert_eq!(
        pack::install_skill_file(&skills, skill, path, pack_text(&pack_path)).unwrap(),
        Installed::KeptEdited
    );
}
