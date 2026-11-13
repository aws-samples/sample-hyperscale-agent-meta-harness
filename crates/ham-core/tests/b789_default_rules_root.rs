//! The shipped default rules reach the lifecycle pack's scanner kit through the engine-neutral variable: every engine's launch
//! carries `HAMH_ENGINE_CONFIG_ROOT`, the directory whose `skills/` holds the run's skills, so the rule works for any engine and not through a
//! path that only one engine's settings file defines.
#![allow(clippy::unwrap_used)]

use ham_core::pack::{self, Installed};
use ham_core::rules::{DEFAULT_RULES, DEFAULT_RULES_NAME};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const KIT: &str =
    "python3 \"${HAMH_ENGINE_CONFIG_ROOT}/skills/lifecycle-check/scripts/scanner_kit.py\"";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-b789-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn b789_the_default_rules_run_the_scanner_kit_through_the_neutral_config_root_variable() {
    assert!(
        DEFAULT_RULES.contains(KIT),
        "the exact command is in the rules"
    );
    assert!(
        !DEFAULT_RULES.contains("CLINE_GLOBAL_SETTINGS_PATH"),
        "no engine's own settings variable is in the shipped rules"
    );
    // The variable is the one agentd exports for every engine (pinned in agentd's own test as well).
    assert!(DEFAULT_RULES.contains("HAMH_ENGINE_CONFIG_ROOT"));
}

#[test]
fn b789_an_unedited_copy_of_the_previous_default_is_replaced_by_the_text_with_the_neutral_variable()
{
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-11-05-dbd235ca5-00-my-rules.md.txt"),
    )
    .unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(old_text.contains(
        "${CLINE_GLOBAL_SETTINGS_PATH%/locked/*}/skills/lifecycle-check/scripts/scanner_kit.py"
    ));
    let dir = scratch("neutral-root");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-11-05".into()
        }
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), DEFAULT_RULES);
    // The user's edit of the previous default is theirs and stays.
    let mut edited = old;
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}
