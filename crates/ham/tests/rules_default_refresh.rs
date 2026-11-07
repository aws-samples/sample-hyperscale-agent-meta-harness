//! `ham rules init` and the default rules file (ripgrep by default, first slice): a new default reaches a machine that already has the file.
//! `init` never overwrote `00-my-rules.md`, so a change to the default text reached new installs only. Now an installed `00-my-rules.md` that
//! holds exactly an earlier default (`crates/ham-core/assets/default-rules.history.json`; the texts are the files of
//! `fixtures/rules/default-earlier/`, taken from git) was never edited, and `init` replaces it and says `updated ... (was an earlier default;
//! not edited by you)`; an edited file is kept and the output says so. The real `ham` binary in a temp HOME; no network, no AWS credentials.
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::rules::DEFAULT_RULES;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(label: &str) -> Home {
        Home {
            root: support::scratch(label),
        }
    }
    fn rules_dir(&self) -> PathBuf {
        self.root.join(".config/ham/rules")
    }
    fn default_file(&self) -> PathBuf {
        self.rules_dir().join("00-my-rules.md")
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ham"));
        cmd.env_clear();
        for (k, v) in std::env::vars() {
            if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
                cmd.env(k, v);
            }
        }
        cmd.args(args)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("TMPDIR", &self.root);
        cmd.stdin(std::process::Stdio::null()).output().unwrap()
    }
    /// A machine that ran `ham rules init` when `text` was the default: the file is there, nothing else.
    fn with_default(&self, text: &[u8]) {
        std::fs::create_dir_all(self.rules_dir()).unwrap();
        std::fs::write(self.default_file(), text).unwrap();
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// (file name, bytes) of every earlier default, sorted by name: the defaults of 2026-10-13 to 2026-10-16.
fn earlier() -> Vec<(String, Vec<u8>)> {
    let dir = repo().join("fixtures/rules/default-earlier");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let bytes = std::fs::read(dir.join(&n)).unwrap();
            (n, bytes)
        })
        .collect()
}

/// The default of v3 (4,394 bytes): the file on the maintainers' machine, and the baseline rules of the lifecycle validation.
fn rc3_default() -> Vec<u8> {
    std::fs::read(repo().join("fixtures/lifecycle/baseline-rules/00-my-rules.md")).unwrap()
}

fn updated_line(h: &Home) -> String {
    format!(
        "updated {} (was an earlier default; not edited by you)",
        h.default_file().display()
    )
}

#[test]
fn a_bare_init_replaces_the_unedited_rc3_default_and_says_so() {
    let h = Home::new("refresh-v3");
    h.with_default(&rc3_default());
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES,
        "the file is the current default"
    );
    let text = out(&o);
    assert!(
        text.lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{text}"
    );
    assert!(
        !text
            .lines()
            .any(|l| l.starts_with("kept") && l.contains("00-my-rules.md")),
        "the default was not kept:\n{text}"
    );
    // The rest of the install goes ahead beside it.
    assert!(h.rules_dir().join("10-lifecycle.md").exists());
}

#[test]
fn every_earlier_default_is_replaced() {
    let files = earlier();
    assert!(files.len() >= 5, "{} earlier defaults", files.len());
    for (name, bytes) in files {
        let h = Home::new("refresh-each");
        h.with_default(&bytes);
        let o = h.run(&["rules", "init", "--no-skills"]);
        assert_eq!(code(&o), 0, "{name}: {}{}", out(&o), err(&o));
        assert_eq!(
            std::fs::read_to_string(h.default_file()).unwrap(),
            DEFAULT_RULES,
            "{name}"
        );
        assert!(out(&o).lines().any(|l| l == updated_line(&h)), "{name}");
    }
}

#[test]
fn the_current_default_is_kept_as_already_there_and_a_second_init_changes_nothing() {
    let h = Home::new("refresh-current");
    h.with_default(&rc3_default());
    assert_eq!(code(&h.run(&["rules", "init"])), 0);
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES,
        "the first init brought the earlier default up to date"
    );
    let before = std::fs::read(h.default_file()).unwrap();
    let mtime = std::fs::metadata(h.default_file())
        .unwrap()
        .modified()
        .unwrap();
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let text = out(&o);
    assert!(
        text.lines().any(|l| l
            == format!(
                "kept {} (already there; not changed)",
                h.default_file().display()
            )),
        "{text}"
    );
    assert!(!text.contains("updated"), "{text}");
    assert_eq!(std::fs::read(h.default_file()).unwrap(), before);
    assert_eq!(
        std::fs::metadata(h.default_file())
            .unwrap()
            .modified()
            .unwrap(),
        mtime,
        "the file was not written"
    );
}

#[test]
fn an_edited_default_is_kept_and_the_output_says_you_edited_it() {
    let h = Home::new("refresh-edited");
    let mut edited = rc3_default();
    edited.extend_from_slice(b"\n## Mine\n\n- Always answer in French.\n");
    h.with_default(&edited);
    let o = h.run(&["rules", "init"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read(h.default_file()).unwrap(), edited);
    let line = out(&o)
        .lines()
        .find(|l| l.contains("00-my-rules.md"))
        .unwrap_or_else(|| panic!("a line about the file:\n{}", out(&o)))
        .to_string();
    assert!(
        line.starts_with(&format!("kept {}", h.default_file().display()))
            && line.contains("edited by you; not changed")
            && line.contains(&sha(DEFAULT_RULES.as_bytes())),
        "{line}"
    );
    assert!(!out(&o).contains("updated"), "{}", out(&o));
}

#[test]
fn a_file_with_other_line_ends_or_no_final_newline_counts_as_edited() {
    for (what, bytes) in [
        (
            "crlf",
            String::from_utf8(rc3_default())
                .unwrap()
                .replace('\n', "\r\n")
                .into_bytes(),
        ),
        (
            "no final newline",
            String::from_utf8(rc3_default())
                .unwrap()
                .trim_end()
                .as_bytes()
                .to_vec(),
        ),
    ] {
        let h = Home::new("refresh-bytes");
        h.with_default(&bytes);
        let o = h.run(&["rules", "init", "--no-skills"]);
        assert_eq!(code(&o), 0, "{what}: {}{}", out(&o), err(&o));
        assert_eq!(std::fs::read(h.default_file()).unwrap(), bytes, "{what}");
        assert!(out(&o).contains("edited by you; not changed"), "{what}");
    }
}

#[test]
fn a_link_to_an_earlier_default_is_kept_and_its_target_is_not_written() {
    let h = Home::new("refresh-link");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    let target = h.root.join("dotfiles-00-my-rules.md");
    std::fs::write(&target, rc3_default()).unwrap();
    symlink(&target, h.default_file()).unwrap();
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        std::fs::symlink_metadata(h.default_file())
            .unwrap()
            .file_type()
            .is_symlink(),
        "still a link"
    );
    assert_eq!(std::fs::read(&target).unwrap(), rc3_default());
    assert!(!out(&o).contains("updated"), "{}", out(&o));
}

#[test]
fn the_other_install_forms_refresh_the_default_too() {
    for args in [
        vec!["rules", "init", "--pack", "none"],
        vec!["rules", "init", "--no-skills"],
        vec!["rules", "init", "--skills-as-rules"],
    ] {
        let h = Home::new("refresh-forms");
        h.with_default(&rc3_default());
        let o = h.run(&args);
        assert_eq!(code(&o), 0, "{args:?}: {}{}", out(&o), err(&o));
        assert_eq!(
            std::fs::read_to_string(h.default_file()).unwrap(),
            DEFAULT_RULES,
            "{args:?}"
        );
        assert!(
            out(&o).lines().any(|l| l == updated_line(&h)),
            "{args:?}: {}",
            out(&o)
        );
    }
}

#[test]
fn an_earlier_default_beside_files_of_your_own_is_still_replaced_and_yours_are_not_touched() {
    let h = Home::new("refresh-beside");
    h.with_default(&rc3_default());
    std::fs::write(h.rules_dir().join("20-mine.md"), "# Mine\n\n- Be brief.\n").unwrap();
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    assert_eq!(
        std::fs::read_to_string(h.rules_dir().join("20-mine.md")).unwrap(),
        "# Mine\n\n- Be brief.\n"
    );
}

#[test]
fn a_folder_of_your_own_files_without_the_default_still_gets_no_default() {
    // The rule that was already there stays: the default is not added to a folder that holds other files.
    let h = Home::new("refresh-absent");
    std::fs::create_dir_all(h.rules_dir()).unwrap();
    std::fs::write(h.rules_dir().join("20-mine.md"), "# Mine\n\n- Be brief.\n").unwrap();
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(!h.default_file().exists());
    assert!(out(&o).contains("the default is not added"), "{}", out(&o));
}

#[test]
fn the_replacement_keeps_the_mode_and_leaves_no_temporary_file() {
    let h = Home::new("refresh-mode");
    h.with_default(&rc3_default());
    std::fs::set_permissions(h.default_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES,
        "the file was replaced"
    );
    assert_eq!(
        std::fs::metadata(h.default_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut names: Vec<String> = std::fs::read_dir(h.rules_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["00-my-rules.md", "10-lifecycle.md"]);
}

#[test]
fn rules_show_after_the_update_sends_the_ripgrep_bullet() {
    let h = Home::new("refresh-show");
    h.with_default(&rc3_default());
    assert_eq!(code(&h.run(&["rules", "init", "--no-skills"])), 0);
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("rg --files -g '*.java'") && out(&o).contains("--hidden --no-ignore"),
        "the next run would send the rg bullet:\n{}",
        out(&o)
    );
}

/// The default that held the large-disk session's user is the text of a machine that ran `ham rules init` just before the
/// bullets on a stopped check script and on the form of a suppression comment. `init` replaces it unedited and the next run sends both rules.
#[test]
fn init_replaces_the_default_of_3465a36cf_and_the_next_run_sends_the_retry_and_suppression_rules() {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-3465a36cf-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-retry");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "read its header and docs for a setting that redirects it",
        "A scanner or linter suppression comment",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// `init` replaces it unedited and the next run sends the sentence.
#[test]
fn init_replaces_the_default_of_bc79033f5_and_the_next_run_sends_the_check_after_every_change() {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-bc79033f5-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-docs");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "Run it after every change, also when only documents or comments changed",
        "its checks read documents too",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// `init` replaces it unedited and the next run sends the sentence.
#[test]
fn init_replaces_the_default_of_885c1c975_and_the_next_run_sends_the_look_at_the_excused_patterns()
{
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-885c1c975-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-excused");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "the repository already excuses",
        "check your diff for the patterns they",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// `init` replaces it unedited and the next run sends the new sentence.
#[test]
fn init_replaces_the_default_of_f69120dbb_and_the_next_run_sends_the_reasons_of_the_excused_patterns()
 {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-f69120dbb-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-reasons");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "list what the repository already excuses with the reasons, one line per rule id",
        "read them, check your diff for the patterns they describe",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// `init` replaces it unedited and the next run sends the new sentence.
#[test]
fn init_replaces_the_default_of_352351312_and_the_next_run_sends_the_comparison_of_the_first_error_line()
 {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-352351312-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-first-error");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "compare the first error line of each failing test, not only its name",
        "read every error that differs.",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// `init` replaces it unedited and the next run sends the bullet.
#[test]
fn init_replaces_the_default_of_c050f2c50_and_the_next_run_sends_the_scanner_kit_bullet() {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-c050f2c50-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-scanner-kit");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "Run the scanner kit before the last commit",
        "Put a KIT-NOT-RUN line in your last message as NOT RUN, with its reason.",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// `init` replaces it unedited and the next run sends the command.
#[test]
fn init_replaces_the_default_of_76e3a433c_and_the_next_run_sends_the_exact_kit_command() {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-28-76e3a433c-00-my-rules.md.txt"),
    )
    .unwrap();
    let h = Home::new("refresh-kit-command");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    for sent in [
        "python3 \"${CLINE_GLOBAL_SETTINGS_PATH%/locked/*}/skills/lifecycle-check/scripts/scanner_kit.py\"",
        "run it in the background, output to a file",
    ] {
        assert!(
            out(&o).contains(sent),
            "the next run would send {sent:?}:\n{}",
            out(&o)
        );
    }
}

/// A fresh `ham rules init` writes the section that asks the agent to date-time stamp the rules it writes, and `ham rules show` prints it
/// as part of the text a run would send.
#[test]
fn b769_a_fresh_init_writes_the_dated_rules_section() {
    let h = Home::new("b769-fresh");
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let written = std::fs::read_to_string(h.default_file()).unwrap();
    assert_eq!(written, DEFAULT_RULES, "the file is the current default");
    for sent in [
        "\n## Dated rules\n",
        "End each rule you write or change in AGENTS.md, a rules file or a steering file with a stamp, `YYYY-MM-DD HH:MM TZ`",
        "Change, re-stamp or remove a rule only after the user confirms.",
        "A rule without a stamp is undated, not stale",
    ] {
        assert!(written.contains(sent), "the file lacks {sent:?}");
    }
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("## Dated rules"),
        "the next run would send the section:\n{}",
        out(&o)
    );
}

/// The section is the user's text. A user who deleted it keeps a file without it: `init` does not put it back and says the file is theirs.
#[test]
fn b769_a_user_who_removed_the_section_keeps_a_file_without_it() {
    let (before, rest) = DEFAULT_RULES.split_once("## Dated rules\n").unwrap();
    let (_, after) = rest.split_once("\n## ").unwrap();
    let mut edited = format!("{before}## {after}").into_bytes();
    edited.extend_from_slice(b"\n- My own rule.\n");
    assert!(!String::from_utf8_lossy(&edited).contains("Dated rules"));
    let h = Home::new("b769-removed");
    h.with_default(&edited);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(std::fs::read(h.default_file()).unwrap(), edited);
    assert!(
        out(&o).contains("edited by you; not changed"),
        "the file is kept:\n{}",
        out(&o)
    );
    assert!(!out(&o).contains("updated"), "{}", out(&o));
}

/// A machine that holds the unedited default from before the section gets the new default at the next `ham rules init`.
#[test]
fn b769_init_replaces_the_default_of_a7fa791de_and_the_next_run_sends_the_dated_rules() {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-10-31-a7fa791de-00-my-rules.md.txt"),
    )
    .unwrap();
    assert!(
        !String::from_utf8_lossy(&old).contains("Dated rules"),
        "the earlier default has no such section"
    );
    let h = Home::new("b769-refresh");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("Take the time from `date`."),
        "the next run would send the stamp request:\n{}",
        out(&o)
    );
}

/// follow-up: a fresh `ham rules init` writes opening lines that state no size, and `ham rules show` prints them as part of the text a run would send.
#[test]
fn b769_a_fresh_init_writes_default_rules_that_state_no_size_limit() {
    let h = Home::new("b769-nosize-fresh");
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let written = std::fs::read_to_string(h.default_file()).unwrap();
    assert_eq!(written, DEFAULT_RULES, "the file is the current default");
    assert!(
        written.contains("Put no secrets in it."),
        "the opening lines keep the secrets sentence"
    );
    for gone in ["Keep it under", " KB"] {
        assert!(!written.contains(gone), "the file still says {gone:?}");
    }
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("Put no secrets in it.") && !out(&o).contains("Keep it under"),
        "the next run would send the opening lines without a size:\n{}",
        out(&o)
    );
}

/// follow-up: a machine that holds the unedited default of the 5.06 release gets the current default at the
/// next `ham rules init`, and the next run sends the opening lines without the size.
#[test]
fn b769_init_replaces_the_default_of_9d91bee8a_and_the_next_run_sends_no_size_limit() {
    let old = std::fs::read(
        repo().join("fixtures/rules/default-earlier/2026-11-05-9d91bee8a-00-my-rules.md.txt"),
    )
    .unwrap();
    assert!(
        String::from_utf8_lossy(&old).contains("Keep it under 9 KB"),
        "the earlier default has the size note"
    );
    let h = Home::new("b769-nosize-refresh");
    h.with_default(&old);
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).lines().any(|l| l == updated_line(&h)),
        "the updated line:\n{}",
        out(&o)
    );
    assert_eq!(
        std::fs::read_to_string(h.default_file()).unwrap(),
        DEFAULT_RULES
    );
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("Put no secrets in it.") && !out(&o).contains("Keep it under"),
        "the next run would send the opening lines without a size:\n{}",
        out(&o)
    );
}

/// follow-up: a copy of the 5.06 default that the user edited is theirs. The user who changed the size sentence to a size of their own, and the user
/// who added a rule, both keep their file and the output says so.
#[test]
fn b769_an_edited_copy_of_the_9d91bee8a_default_is_kept() {
    let old = String::from_utf8(
        std::fs::read(
            repo().join("fixtures/rules/default-earlier/2026-11-05-9d91bee8a-00-my-rules.md.txt"),
        )
        .unwrap(),
    )
    .unwrap();
    for (what, edited) in [
        (
            "a size of the user's own",
            old.replace("Keep it under 9 KB", "Keep it under 20 KB"),
        ),
        ("an added rule", format!("{old}\n- My own rule.\n")),
    ] {
        let h = Home::new("b769-nosize-edited");
        h.with_default(edited.as_bytes());
        let o = h.run(&["rules", "init", "--no-skills"]);
        assert_eq!(code(&o), 0, "{what}: {}{}", out(&o), err(&o));
        assert_eq!(
            std::fs::read_to_string(h.default_file()).unwrap(),
            edited,
            "{what}: the file is kept"
        );
        assert!(
            out(&o).contains("edited by you; not changed"),
            "{what}:\n{}",
            out(&o)
        );
        assert!(!out(&o).contains("updated"), "{what}:\n{}", out(&o));
    }
}
