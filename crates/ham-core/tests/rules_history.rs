//! The earlier texts of the default rules file (ripgrep by default, first slice): `assets/default-rules.history.json` lists, for
//! `00-my-rules.md`, the SHA-256 of every text the default had before the current one and the date it became the default. `ham rules init`
//! reads it (`pack::refresh_default_rules`): an installed `00-my-rules.md` equal to one of those texts was never edited, so the installer
//! replaces it with the current default; any other file is the user's and is kept. The earlier texts used here are the files of
//! `fixtures/rules/default-earlier/`, taken from git (`<date>-<commit>-00-my-rules.md.txt`); their hashes are the ones the history lists.
//! The history has the same canonical form as the pack's (`lifecycle/pack.history.json`: `{"files": {"<path>": {"<sha256>": "<label>"}}}`)
//! and the same parser; the label is a date here and a pack version there.
#![allow(clippy::unwrap_used)]

use ham_core::pack::{self, History, Installed};
use ham_core::rules::{DEFAULT_RULES, DEFAULT_RULES_NAME};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// SHA-256 of the current `assets/default-rules.md`. A change to that file fails the test below until the text it replaces is in
/// `fixtures/rules/default-earlier/` and its hash is in the history; then this value moves to the new text.
const CURRENT_SHA256: &str = "e8c9c138b709f3a28b861c60d82a03f8459347d46444b97ccf872af8d92c1438";

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
        "ham-rules-history-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// (date, commit, bytes) of every file in `fixtures/rules/default-earlier/`, sorted by name.
fn fixtures() -> Vec<(String, String, Vec<u8>)> {
    let dir = repo().join("fixtures/rules/default-earlier");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let (date, rest) = name.split_at(10);
            let commit = rest
                .strip_prefix('-')
                .and_then(|r| r.strip_suffix("-00-my-rules.md.txt"))
                .unwrap_or_else(|| panic!("{name}: <date>-<commit>-00-my-rules.md.txt"));
            assert!(
                commit.len() >= 7 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
                "{name}: the commit"
            );
            (
                date.to_string(),
                commit.to_string(),
                std::fs::read(dir.join(&name)).unwrap(),
            )
        })
        .collect()
}

fn history_naming(old: &str, label: &str) -> String {
    format!(
        "{{\n  \"files\": {{\n    \"{DEFAULT_RULES_NAME}\": {{\n      \"{}\": \"{label}\"\n    }}\n  }}\n}}\n",
        sha(old.as_bytes())
    )
}

// ---- the history file ---------------------------------------------------------------------------------------------------------

#[test]
fn every_earlier_text_has_a_fixture_and_every_fixture_is_listed_with_its_date() {
    let history = History::default_rules();
    let files = fixtures();
    assert!(
        files.len() >= 7,
        "the seven earlier defaults of git, 2026-10-13 to 2026-10-23: {}",
        files.len()
    );
    for (date, commit, bytes) in &files {
        assert_eq!(
            history.earlier_version(DEFAULT_RULES_NAME, bytes),
            Some(date.as_str()),
            "{date}-{commit}: listed with the date it became the default"
        );
    }
    let listed = history.hashes(DEFAULT_RULES_NAME);
    assert_eq!(
        listed.len(),
        files.len(),
        "a listed hash with no fixture, or a fixture listed twice: {listed:?}"
    );
    assert_eq!(
        history.paths().collect::<Vec<_>>(),
        vec![DEFAULT_RULES_NAME],
        "only the default rules file has earlier texts"
    );
}

#[test]
fn the_rc3_default_of_the_lifecycle_baseline_is_an_earlier_text() {
    // fixtures/lifecycle/baseline-rules/00-my-rules.md is the default of v3 (4,394 bytes), the one a machine that ran `ham rules init`
    // then has; the history must reach it.
    let v3 =
        std::fs::read(repo().join("fixtures/lifecycle/baseline-rules/00-my-rules.md")).unwrap();
    assert_eq!(v3.len(), 4394);
    assert_eq!(
        History::default_rules().earlier_version(DEFAULT_RULES_NAME, &v3),
        Some("2026-10-15")
    );
}

/// The default of 2026-10-23: the text that was current when the Rust,
/// `ham-research` and repository-check bullets were added. A machine that ran `ham rules init` since then holds exactly this text.
const MAVEN_DEFAULT_SHA256: &str =
    "f3f53ca776dcc3bf2d408ee4db83ab8bfc0ecb18a7ea53fe5267b79ae3d12b66";
const MAVEN_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-23-de2b3f63c-00-my-rules.md.txt";

#[test]
fn the_default_of_de2b3f63c_is_an_earlier_text_dated_2026_10_23() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(MAVEN_DEFAULT_SHA256, "2026-10-23")),
        "the history lacks the 4,163-byte default of de2b3f63c ({MAVEN_DEFAULT_SHA256}) dated 2026-10-23: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(MAVEN_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 4163, "the fixture is the text of de2b3f63c");
    assert_eq!(sha(&bytes), MAVEN_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-23")
    );
}

#[test]
fn an_unedited_copy_of_the_de2b3f63c_default_is_replaced_and_an_edited_copy_is_kept() {
    let old = std::fs::read(repo().join(MAVEN_DEFAULT_FIXTURE)).unwrap();
    let dir = scratch("maven-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-23".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), DEFAULT_RULES);
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Same,
        "the second call finds the current text"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-25: the text that was current
/// when the build-output bullet was added. A machine that ran `ham rules init` since the slice holds exactly this text.
const CHECK_DEFAULT_SHA256: &str =
    "e7fcd42604d85b86090791c020e6a5800f11876e4def5ddbd455db533773cf53";
const CHECK_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-25-f0dc9793c-00-my-rules.md.txt";

#[test]
fn the_default_of_f0dc9793c_is_an_earlier_text_dated_2026_10_25() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(CHECK_DEFAULT_SHA256, "2026-10-25")),
        "the history lacks the 5,410-byte default of f0dc9793c ({CHECK_DEFAULT_SHA256}) dated 2026-10-25: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(CHECK_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 5410, "the fixture is the text of f0dc9793c");
    assert_eq!(sha(&bytes), CHECK_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-25")
    );
}

#[test]
fn an_unedited_copy_of_the_f0dc9793c_default_is_replaced_and_an_edited_copy_is_kept() {
    let old = std::fs::read(repo().join(CHECK_DEFAULT_FIXTURE)).unwrap();
    let dir = scratch("check-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-25".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), DEFAULT_RULES);
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-26: the text that was current
/// when the build-output bullet stopped telling a run to poll a log.
const BUDGET_DEFAULT_SHA256: &str =
    "feec750000fe200eb037f5d61a6a293f089b7fc89ab392340efb991f7f0ed109";
const BUDGET_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-26-2aaafe9ea-00-my-rules.md.txt";

#[test]
fn the_default_of_2aaafe9ea_is_an_earlier_text_dated_2026_10_26() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(BUDGET_DEFAULT_SHA256, "2026-10-26")),
        "the history lacks the 5,966-byte default of 2aaafe9ea ({BUDGET_DEFAULT_SHA256}) dated 2026-10-26: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(BUDGET_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 5966, "the fixture is the text of 2aaafe9ea");
    assert_eq!(sha(&bytes), BUDGET_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-26")
    );
}

#[test]
fn an_unedited_copy_of_the_2aaafe9ea_default_is_replaced_and_an_edited_copy_is_kept() {
    let old = std::fs::read(repo().join(BUDGET_DEFAULT_FIXTURE)).unwrap();
    let dir = scratch("budget-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-26".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), DEFAULT_RULES);
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-26: the text that was current
/// when the build-output bullet, the check-command bullet and the Rust bullet were reworded without the numbers and the example script of one
/// repository.
const VERIFY_DEFAULT_SHA256: &str =
    "a0ddd32919d8d6031650cd899bd7dc440ae67ae11f461eb1959d8ae249812975";
const VERIFY_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-26-b1ebf91dd-00-my-rules.md.txt";

#[test]
fn the_default_of_b1ebf91dd_is_an_earlier_text_dated_2026_10_26() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(VERIFY_DEFAULT_SHA256, "2026-10-26")),
        "the history lacks the 6,053-byte default of b1ebf91dd ({VERIFY_DEFAULT_SHA256}) dated 2026-10-26: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(VERIFY_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 6053, "the fixture is the text of b1ebf91dd");
    assert_eq!(sha(&bytes), VERIFY_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-26")
    );
}

#[test]
fn an_unedited_copy_of_the_b1ebf91dd_default_is_replaced_and_an_edited_copy_is_kept() {
    let old = std::fs::read(repo().join(VERIFY_DEFAULT_FIXTURE)).unwrap();
    let dir = scratch("verify-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-26".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), DEFAULT_RULES);
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28: the text that was current when
/// the check-command bullet gained the sentence that sends a stopped check script back to its own settings, and the bullet on the form of a
/// suppression comment was added. A machine that ran `ham rules init` since the large-disk session was described holds exactly this text.
const RETRY_DEFAULT_SHA256: &str =
    "f380401f00513eb103a5c0bdf358c5923ea958cc5da2901d32b8f4a41b984455";
const RETRY_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-3465a36cf-00-my-rules.md.txt";

#[test]
fn the_default_of_3465a36cf_is_an_earlier_text_dated_2026_10_28() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(RETRY_DEFAULT_SHA256, "2026-10-28")),
        "the history lacks the 6,823-byte default of 3465a36cf ({RETRY_DEFAULT_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(RETRY_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 6823, "the fixture is the text of 3465a36cf");
    assert_eq!(sha(&bytes), RETRY_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_3465a36cf_default_is_replaced_by_the_text_with_the_retry_and_suppression_rules()
 {
    let old = std::fs::read(repo().join(RETRY_DEFAULT_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        !old_text.contains("redirects it") && !old_text.contains("suppression comment"),
        "the earlier text has neither new rule"
    );
    let dir = scratch("retry-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("redirects it") && now.contains("suppression comment"),
        "the installed file carries both new rules"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28: the text that was
/// current when the check-command bullet gained the sentence that asks for the check after every change, also a change of documents or comments
/// only. A machine that ran `ham rules init` since that commit holds exactly this text.
const DOCS_DEFAULT_SHA256: &str =
    "d01b5121025dd826c9528d8837bb3030c4b99a3870788fd30120ee40695ba44a";
const DOCS_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-bc79033f5-00-my-rules.md.txt";

#[test]
fn the_default_of_bc79033f5_is_an_earlier_text_dated_2026_10_28() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(DOCS_DEFAULT_SHA256, "2026-10-28")),
        "the history lacks the 7,404-byte default of bc79033f5 ({DOCS_DEFAULT_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(DOCS_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 7404, "the fixture is the text of bc79033f5");
    assert_eq!(sha(&bytes), DOCS_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_bc79033f5_default_is_replaced_by_the_text_that_asks_for_the_check_after_every_change()
 {
    let old = std::fs::read(repo().join(DOCS_DEFAULT_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        !old_text.contains("read documents too"),
        "the earlier text lacks the new sentence"
    );
    let dir = scratch("docs-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("its checks read documents too"),
        "the installed file carries the new sentence"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28: the text that was
/// current when the suppression bullet gained the sentence that asks for a look at the patterns the repository already excuses. A machine that ran
/// `ham rules init` since that commit holds exactly this text.
const EXCUSED_DEFAULT_SHA256: &str =
    "2a93fd979449819215031ab0d908fb1e4e09341d0432e81dfb2c78be3a57b0ff";
const EXCUSED_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-885c1c975-00-my-rules.md.txt";

#[test]
fn the_default_of_885c1c975_is_an_earlier_text_dated_2026_10_28_with_7558_bytes() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(EXCUSED_DEFAULT_SHA256, "2026-10-28")),
        "the history lacks the 7,558-byte default of 885c1c975 ({EXCUSED_DEFAULT_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(EXCUSED_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 7558, "the fixture is the text of 885c1c975");
    assert_eq!(sha(&bytes), EXCUSED_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_885c1c975_default_is_replaced_by_the_text_that_asks_for_a_look_at_the_excused_patterns()
 {
    let old = std::fs::read(repo().join(EXCUSED_DEFAULT_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        !old_text.contains("already excuses"),
        "the earlier text lacks the new sentence"
    );
    let dir = scratch("excused-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("the repository already excuses"),
        "the installed file carries the sentence on the suppressions the repository already excuses"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28:
/// the text that was current when that sentence began to ask for the reasons of the excused suppressions, one line per rule id, and not for
/// the bare rule ids. A machine that ran `ham rules init` since that commit holds exactly this text.
const REASONS_DEFAULT_SHA256: &str =
    "cf62276550f332d8e778b70b583c4d55b8a2aaedded095355a4fea649f9da924";
const REASONS_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-f69120dbb-00-my-rules.md.txt";

#[test]
fn the_default_of_f69120dbb_is_an_earlier_text_dated_2026_10_28_with_7865_bytes() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(REASONS_DEFAULT_SHA256, "2026-10-28")),
        "the history lacks the 7,865-byte default of f69120dbb ({REASONS_DEFAULT_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(REASONS_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 7865, "the fixture is the text of f69120dbb");
    assert_eq!(sha(&bytes), REASONS_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_f69120dbb_default_is_replaced_by_the_text_that_asks_for_the_reasons_of_the_excused_patterns()
 {
    let old = std::fs::read(repo().join(REASONS_DEFAULT_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        old_text.contains("list the rule ids the repository already excuses")
            && !old_text.contains("one line per rule id"),
        "the earlier text asks for the bare rule ids"
    );
    let dir = scratch("reasons-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("one line per rule id"),
        "the installed file carries the new sentence"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28: the text that was current when the
/// Checks bullet gained the sentence that asks to compare the first error line of each failing test with the base, not only its name. A machine that
/// ran `ham rules init` since that commit holds exactly this text.
const FIRST_ERROR_DEFAULT_SHA256: &str =
    "7a07a298ecf7ea690f076d210f03961d7f4302e1cfecadeeb0bdcb6fe3e16960";
const FIRST_ERROR_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-352351312-00-my-rules.md.txt";

#[test]
fn the_default_of_352351312_is_an_earlier_text_dated_2026_10_28_with_7993_bytes() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(FIRST_ERROR_DEFAULT_SHA256, "2026-10-28")),
        "the history lacks the 7,993-byte default of 352351312 ({FIRST_ERROR_DEFAULT_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(FIRST_ERROR_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 7993, "the fixture is the text of 352351312");
    assert_eq!(sha(&bytes), FIRST_ERROR_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_352351312_default_is_replaced_by_the_text_that_compares_the_first_error_line_of_a_failing_test()
 {
    let old = std::fs::read(repo().join(FIRST_ERROR_DEFAULT_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        !old_text.contains("first error line"),
        "the earlier text lacks the new sentence"
    );
    let dir = scratch("first-error-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("compare the first error line of each failing test"),
        "the installed file carries the new sentence"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28: the text
/// that was current when the bullet about the scanner kit joined the rules. A machine that ran `ham rules init` since that commit holds exactly this text.
const KIT_DEFAULT_SHA256: &str = "23384818e2ed8cb74a0967e28093818a8dd8751c5268261a2d85a76b04ed3ee0";
const KIT_DEFAULT_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-c050f2c50-00-my-rules.md.txt";

#[test]
fn the_default_of_c050f2c50_is_an_earlier_text_dated_2026_10_28_with_8132_bytes() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(KIT_DEFAULT_SHA256, "2026-10-28")),
        "the history lacks the 8,132-byte default of c050f2c50 ({KIT_DEFAULT_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(KIT_DEFAULT_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 8132, "the fixture is the text of c050f2c50");
    assert_eq!(sha(&bytes), KIT_DEFAULT_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_c050f2c50_default_is_replaced_by_the_text_that_adds_the_scanner_kit_bullet()
 {
    let old = std::fs::read(repo().join(KIT_DEFAULT_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        !old_text.contains("scanner kit"),
        "the earlier text has no bullet about the scanner kit"
    );
    let dir = scratch("kit-default");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("Run the scanner kit before the last commit"),
        "the installed file carries the new bullet"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of 2026-10-28: the text with the bullet that said "if your skills include a scanner kit". A live probe showed that a
/// run never opened the skill to read it, so the bullet now carries the exact command. A machine that ran `ham rules init` since 76e3a433c holds exactly this text.
const KIT_BULLET_SHA256: &str = "ef4c4df5455c479cbd98185ec01adfc21f20f357acff354202c9d277eca4c6ec";
const KIT_BULLET_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-10-28-76e3a433c-00-my-rules.md.txt";

#[test]
fn the_default_of_76e3a433c_is_an_earlier_text_dated_2026_10_28_with_8186_bytes() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(KIT_BULLET_SHA256, "2026-10-28")),
        "the history lacks the 8,186-byte default of 76e3a433c ({KIT_BULLET_SHA256}) dated 2026-10-28: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(KIT_BULLET_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 8186, "the fixture is the text of 76e3a433c");
    assert_eq!(sha(&bytes), KIT_BULLET_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-10-28")
    );
}

#[test]
fn an_unedited_copy_of_the_76e3a433c_default_is_replaced_by_the_text_with_the_exact_kit_command() {
    let old = std::fs::read(repo().join(KIT_BULLET_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        old_text.contains("If your skills include a scanner kit")
            && !old_text.contains("CLINE_GLOBAL_SETTINGS_PATH"),
        "the earlier text has the bullet without the command"
    );
    let dir = scratch("kit-bullet");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-10-28".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("${HAMH_ENGINE_CONFIG_ROOT}/skills/lifecycle-check/scripts/scanner_kit.py"),
        "the installed file carries the exact command"
    );
    let mut edited = old.clone();
    edited.extend_from_slice(b"- Mine.\n");
    std::fs::write(&file, &edited).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::KeptEdited,
        "an edited copy is the user's"
    );
    assert_eq!(std::fs::read(&file).unwrap(), edited);
}

/// The default of the 5.06 release: the text whose first lines said "Keep it under 9 KB". The size note is
/// gone from the current default, so a machine that ran `ham rules init` with 5.06 holds exactly this text and gets the current one at the next `init`.
const SIZE_NOTE_SHA256: &str = "4333e207eee37549e7bdfb4e5c951e03e386a4b823caed32cccc77c7152ff41c";
const SIZE_NOTE_FIXTURE: &str =
    "fixtures/rules/default-earlier/2026-11-05-9d91bee8a-00-my-rules.md.txt";

#[test]
fn b769_the_default_of_9d91bee8a_is_an_earlier_text_dated_2026_11_05_with_8743_bytes() {
    let history = History::default_rules();
    assert!(
        history
            .hashes(DEFAULT_RULES_NAME)
            .contains(&(SIZE_NOTE_SHA256, "2026-11-05")),
        "the history lacks the 8,743-byte default of 9d91bee8a ({SIZE_NOTE_SHA256}) dated 2026-11-05: {:?}",
        history.hashes(DEFAULT_RULES_NAME)
    );
    let bytes = std::fs::read(repo().join(SIZE_NOTE_FIXTURE)).unwrap();
    assert_eq!(bytes.len(), 8743, "the fixture is the text of 9d91bee8a");
    assert_eq!(sha(&bytes), SIZE_NOTE_SHA256);
    assert_eq!(
        history.earlier_version(DEFAULT_RULES_NAME, &bytes),
        Some("2026-11-05")
    );
}

#[test]
fn b769_an_unedited_copy_of_the_9d91bee8a_default_is_replaced_by_the_text_without_a_size_note_and_an_edited_one_is_kept()
 {
    let old = std::fs::read(repo().join(SIZE_NOTE_FIXTURE)).unwrap();
    let old_text = String::from_utf8(old.clone()).unwrap();
    assert!(
        old_text.contains("Keep it under 9 KB and put no secrets in it."),
        "the earlier text has the size note"
    );
    let dir = scratch("size-note");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, &old).unwrap();
    assert_eq!(
        pack::refresh_default_rules(&file).unwrap(),
        Installed::Updated {
            was: "2026-11-05".into()
        },
        "an unedited copy of the earlier default is replaced"
    );
    let now = std::fs::read_to_string(&file).unwrap();
    assert_eq!(now, DEFAULT_RULES);
    assert!(
        now.contains("Put no secrets in it.") && !now.contains("Keep it under"),
        "the installed file states no size"
    );
    // A user who changed the sentence to a size of their own keeps their file, and so does one who added a rule.
    for edited in [
        old_text.replace("Keep it under 9 KB", "Keep it under 20 KB"),
        format!("{old_text}\n- My own rule.\n"),
    ] {
        std::fs::write(&file, &edited).unwrap();
        assert_eq!(
            pack::refresh_default_rules(&file).unwrap(),
            Installed::KeptEdited,
            "an edited copy is the user's"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);
    }
}

#[test]
fn the_current_default_is_not_an_earlier_text() {
    assert_eq!(
        History::default_rules().earlier_version(DEFAULT_RULES_NAME, DEFAULT_RULES.as_bytes()),
        None,
        "the current text is not listed as an earlier one"
    );
    for (date, commit, bytes) in fixtures() {
        assert_ne!(
            bytes,
            DEFAULT_RULES.as_bytes(),
            "{date}-{commit} is the current text, not an earlier one"
        );
    }
}

#[test]
fn a_change_to_the_default_rules_must_bring_the_text_it_replaces_into_the_history() {
    assert_eq!(
        sha(DEFAULT_RULES.as_bytes()),
        CURRENT_SHA256,
        "assets/default-rules.md changed. Save the text it had before in fixtures/rules/default-earlier/<date>-<commit>-00-my-rules.md.txt \
         (git show <commit>:crates/ham-core/assets/default-rules.md), add its SHA-256 and the date it became the default to \
         assets/default-rules.history.json, and set CURRENT_SHA256 here to the new text. Without that, `ham rules init` leaves the old text \
         on every machine that has it."
    );
}

#[test]
fn the_embedded_history_is_the_file_on_disk_in_canonical_form() {
    let on_disk =
        std::fs::read_to_string(repo().join("crates/ham-core/assets/default-rules.history.json"))
            .unwrap();
    assert_eq!(pack::DEFAULT_RULES_HISTORY, on_disk);
    let parsed = History::parse_dated(pack::DEFAULT_RULES_HISTORY).unwrap();
    assert_eq!(
        &parsed,
        History::default_rules(),
        "the default rules' history is the parsed file"
    );
    // Canonical: {"files": {"<path>": {"<sha256>": "<date>"}}}, keys sorted, two-space indent, one final newline.
    let mut files = serde_json::Map::new();
    for path in parsed.paths() {
        let mut hashes = serde_json::Map::new();
        for (digest, label) in parsed.hashes(path) {
            hashes.insert(digest.to_string(), label.into());
        }
        files.insert(path.to_string(), hashes.into());
    }
    let canonical =
        serde_json::to_string_pretty(&serde_json::json!({ "files": files })).unwrap() + "\n";
    assert_eq!(on_disk, canonical, "not in the canonical form");
}

#[test]
fn every_label_in_the_history_is_a_date() {
    for (digest, date) in History::default_rules().hashes(DEFAULT_RULES_NAME) {
        let parts: Vec<u32> = date.split('-').map(|n| n.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3, "{digest}: {date}");
        assert!(
            (2026..=2099).contains(&parts[0])
                && (1..=12).contains(&parts[1])
                && (1..=31).contains(&parts[2]),
            "{digest}: {date} is not a date"
        );
    }
}

#[test]
fn the_parser_refuses_a_bad_hash_a_bad_label_an_extra_key_and_text_that_is_not_json() {
    let ok = "a".repeat(64);
    let doc = |digest: &str, label: &str| {
        format!("{{\"files\": {{\"{DEFAULT_RULES_NAME}\": {{\"{digest}\": \"{label}\"}}}}}}")
    };
    assert!(History::parse_dated(&doc(&ok, "2026-10-16")).is_ok());
    for (what, text) in [
        ("short hash", doc("abc", "2026-10-16")),
        ("upper-case hash", doc(&"A".repeat(64), "2026-10-16")),
        ("a pack version as the label", doc(&ok, "1.0.7")),
        ("a month without its zero", doc(&ok, "2026-1-16")),
        ("an empty label", doc(&ok, "")),
        ("an extra key", "{\"files\": {}, \"extra\": 1}".to_string()),
        ("not json", "files".to_string()),
    ] {
        assert!(History::parse_dated(&text).is_err(), "{what} is refused");
    }
    // The pack's own parser still wants a pack version: a date is no pack version.
    let pack_doc = format!("{{\"files\": {{\"AGENTS.md\": {{\"{ok}\": \"2026-10-16\"}}}}}}");
    assert!(History::parse(&pack_doc).is_err());
}

// ---- refreshing one installed file --------------------------------------------------------------------------------------------

const OLD: &str = "# My rules (an earlier default)\n\n- Old text.\n";
const NEW: &str = "# My rules (the current default)\n\n- New text.\n";

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    out.sort();
    out
}

#[test]
fn a_file_equal_to_the_current_text_is_the_same_and_is_not_written() {
    let dir = scratch("same");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, NEW).unwrap();
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    let history = history_naming(OLD, "2026-10-15");
    let h = History::parse_dated(&history).unwrap();
    assert_eq!(
        pack::refresh_default_rules_with(&h, &file, NEW).unwrap(),
        Installed::Same
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), NEW);
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn a_file_equal_to_an_earlier_text_is_replaced_with_its_mode_and_no_temporary_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("updated");
    let file = dir.join(DEFAULT_RULES_NAME);
    std::fs::write(&file, OLD).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let history = history_naming(OLD, "2026-10-15");
    let h = History::parse_dated(&history).unwrap();
    assert_eq!(
        pack::refresh_default_rules_with(&h, &file, NEW).unwrap(),
        Installed::Updated {
            was: "2026-10-15".into()
        }
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), NEW);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600,
        "the file keeps its mode"
    );
    assert_eq!(
        names(&dir),
        vec![DEFAULT_RULES_NAME.to_string()],
        "no temporary file is left"
    );
    // The second call finds the current text.
    assert_eq!(
        pack::refresh_default_rules_with(&h, &file, NEW).unwrap(),
        Installed::Same
    );
}

#[test]
fn an_edited_file_a_crlf_copy_and_a_text_the_history_never_listed_are_kept() {
    let history = history_naming(OLD, "2026-10-15");
    let h = History::parse_dated(&history).unwrap();
    let dir = scratch("kept");
    let file = dir.join(DEFAULT_RULES_NAME);
    for (what, text) in [
        ("edited", format!("{OLD}- Mine.\n")),
        ("crlf", OLD.replace('\n', "\r\n")),
        ("trailing newline removed", OLD.trim_end().to_string()),
        ("unlisted", "# Something else\n".to_string()),
        ("empty", String::new()),
    ] {
        std::fs::write(&file, &text).unwrap();
        assert_eq!(
            pack::refresh_default_rules_with(&h, &file, NEW).unwrap(),
            Installed::KeptEdited,
            "{what}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{what}");
    }
}

#[test]
fn a_link_is_never_replaced_whatever_it_holds_and_a_folder_is_kept() {
    use std::os::unix::fs::symlink;
    let history = history_naming(OLD, "2026-10-15");
    let h = History::parse_dated(&history).unwrap();
    let dir = scratch("link");
    let target = dir.join("dotfiles-copy.md");
    std::fs::write(&target, OLD).unwrap();
    let link = dir.join(DEFAULT_RULES_NAME);
    symlink(&target, &link).unwrap();
    assert_eq!(
        pack::refresh_default_rules_with(&h, &link, NEW).unwrap(),
        Installed::KeptEdited,
        "a link of a dotfile manager is the user's"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), OLD);
    let folder = scratch("folder").join(DEFAULT_RULES_NAME);
    std::fs::create_dir(&folder).unwrap();
    assert_eq!(
        pack::refresh_default_rules_with(&h, &folder, NEW).unwrap(),
        Installed::KeptEdited,
        "a folder where the file belongs is kept, as before"
    );
}

#[test]
fn a_file_that_is_not_there_is_an_error_not_a_write() {
    let history = history_naming(OLD, "2026-10-15");
    let h = History::parse_dated(&history).unwrap();
    let dir = scratch("missing");
    let file = dir.join(DEFAULT_RULES_NAME);
    assert!(pack::refresh_default_rules_with(&h, &file, NEW).is_err());
    assert!(!file.exists(), "refreshing creates nothing");
}
