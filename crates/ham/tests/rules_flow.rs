//! `ham rules init` and the setting `run.lifecycle_flow`: the contract is installed as the rule `10-lifecycle.md` in one of three
//! texts, `auto` (the model picks the quick path or the eight steps; the default), `full` (the eight steps) or `quick` (the quick path). The setting is a key of
//! `config.toml` under `[run]`, set with `ham config set run.lifecycle_flow <word>`. An unknown word is refused with the three words, an unedited text of the pack is
//! replaced when the flow changes, and a file the user edited is never touched. The real `ham` binary in a temp HOME; no network and no AWS credentials.
//!
//! A text rule cannot make a model classify a task correctly: the tests pin which text reaches the rules folder, and the release gate (new projects, full flow,
//! 6 of 6 runs) is the empirical check of the default.
#![allow(clippy::unwrap_used)]

use ham_core::config::LifecycleFlow;
use ham_core::pack;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

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
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ham-rules-flow-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Home { root }
    }
    fn contract(&self) -> PathBuf {
        self.root.join(".config/ham/rules/10-lifecycle.md")
    }
    fn config(&self) -> PathBuf {
        self.root.join(".config/ham/config.toml")
    }
    fn write_config(&self, text: &str) {
        std::fs::create_dir_all(self.config().parent().unwrap()).unwrap();
        std::fs::write(self.config(), text).unwrap();
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
    fn installed(&self) -> String {
        std::fs::read_to_string(self.contract()).unwrap()
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

#[test]
fn with_no_setting_the_auto_text_is_installed_and_the_output_names_the_flow_and_its_switch() {
    let h = Home::new("default");
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(h.installed(), pack::contract_text(LifecycleFlow::Auto));
    assert_eq!(h.installed(), pack::CONTRACT);
    let text = out(&o);
    assert!(text.contains("flow auto"), "{text}");
    assert!(text.contains("run.lifecycle_flow"), "{text}");
    assert!(
        text.contains("ham config set run.lifecycle_flow"),
        "the output says how to change it: {text}"
    );
}

#[test]
fn the_setting_in_the_file_picks_the_text_that_is_installed() {
    for flow in LifecycleFlow::ALL {
        let h = Home::new(flow.word());
        h.write_config(&format!(
            "configVersion = 1\n\n[run]\nlifecycle_flow = \"{}\"\n",
            flow.word()
        ));
        let o = h.run(&["rules", "init", "--no-skills"]);
        assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
        assert_eq!(h.installed(), pack::contract_text(flow), "{}", flow.word());
        assert!(
            out(&o).contains(&format!("flow {}", flow.word())),
            "{}",
            out(&o)
        );
        let size = pack::contract_text(flow).len();
        assert!(
            out(&o).contains(&format!("({size} bytes)")),
            "the created line gives the size of the text: {}",
            out(&o)
        );
    }
}

#[test]
fn ham_config_set_then_rules_init_installs_the_flow_and_an_unknown_word_is_refused() {
    let h = Home::new("set");
    let o = h.run(&["config", "set", "run.lifecycle_flow", "quick"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let o = h.run(&["config", "get", "run.lifecycle_flow"]);
    assert_eq!(out(&o).trim(), "quick", "{}{}", out(&o), err(&o));
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(h.installed(), pack::contract_text(LifecycleFlow::Quick));
    // An unknown word is refused with the three words, and the file keeps its value.
    let before = std::fs::read_to_string(h.config()).unwrap();
    let o = h.run(&["config", "set", "run.lifecycle_flow", "fast"]);
    assert_ne!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(err(&o).contains("auto, full or quick"), "{}", err(&o));
    assert_eq!(std::fs::read_to_string(h.config()).unwrap(), before);
}

#[test]
fn a_bad_value_in_the_file_stops_the_install_before_anything_is_written() {
    let h = Home::new("bad");
    h.write_config("configVersion = 1\n\n[run]\nlifecycle_flow = \"fast\"\n");
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_ne!(code(&o), 0, "{}{}", out(&o), err(&o));
    let e = err(&o);
    assert!(
        e.contains("auto") && e.contains("full") && e.contains("quick"),
        "the refusal names the three values: {e}"
    );
    assert!(
        e.contains("lifecycle_flow"),
        "the refusal names the key: {e}"
    );
    assert!(
        !h.root.join(".config/ham/rules").exists(),
        "no rules folder is made"
    );
}

#[test]
fn a_change_of_flow_replaces_the_unedited_text_and_says_so() {
    let h = Home::new("switch");
    assert_eq!(code(&h.run(&["rules", "init", "--no-skills"])), 0);
    assert_eq!(h.installed(), pack::CONTRACT);
    assert_eq!(
        code(&h.run(&["config", "set", "run.lifecycle_flow", "full"])),
        0
    );
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(h.installed(), pack::contract_text(LifecycleFlow::Full));
    let line = out(&o)
        .lines()
        .find(|l| l.starts_with("updated "))
        .unwrap_or_else(|| panic!("no updated line: {}", out(&o)))
        .to_string();
    assert!(
        line.ends_with(&format!(
            "10-lifecycle.md (was pack {}, flow auto; not edited by you)",
            pack::version()
        )),
        "{line}"
    );
    // The same setting again changes nothing.
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert!(
        out(&o).lines().any(|l| l.starts_with("kept ")
            && l.ends_with("10-lifecycle.md (already there; not changed)")),
        "{}",
        out(&o)
    );
    assert_eq!(h.installed(), pack::contract_text(LifecycleFlow::Full));
}

#[test]
fn a_file_the_user_edited_is_never_replaced_whatever_the_setting() {
    let h = Home::new("edited");
    assert_eq!(code(&h.run(&["rules", "init", "--no-skills"])), 0);
    let mine = format!("{}\nMy own rule.\n", h.installed());
    std::fs::write(h.contract(), &mine).unwrap();
    for word in ["full", "quick", "auto"] {
        assert_eq!(
            code(&h.run(&["config", "set", "run.lifecycle_flow", word])),
            0
        );
        let o = h.run(&["rules", "init", "--no-skills"]);
        assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
        assert_eq!(h.installed(), mine, "{word}");
        let flow = LifecycleFlow::parse(word).unwrap();
        assert!(
            out(&o).contains(&format!(
                "(edited by you; not changed; the pack's text has sha256 {})",
                sha(pack::contract_text(flow).as_bytes())
            )),
            "{}",
            out(&o)
        );
    }
}

#[test]
fn the_contract_of_pack_1_2_0_is_replaced_by_the_flow_that_is_set() {
    let h = Home::new("old");
    let old = include_str!("../../../fixtures/lifecycle/pack-earlier/1.2.0-AGENTS.md.txt");
    std::fs::create_dir_all(h.contract().parent().unwrap()).unwrap();
    std::fs::write(h.contract(), old).unwrap();
    assert_eq!(
        code(&h.run(&["config", "set", "run.lifecycle_flow", "quick"])),
        0
    );
    let o = h.run(&["rules", "init", "--no-skills"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(h.installed(), pack::contract_text(LifecycleFlow::Quick));
    assert!(
        out(&o)
            .lines()
            .any(|l| l.ends_with("10-lifecycle.md (was pack 1.2.0; not edited by you)")),
        "{}",
        out(&o)
    );
}

#[test]
fn pack_none_installs_no_contract_and_the_setting_changes_nothing_about_that() {
    let h = Home::new("none");
    assert_eq!(
        code(&h.run(&["config", "set", "run.lifecycle_flow", "full"])),
        0
    );
    let o = h.run(&["rules", "init", "--pack", "none"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(!h.contract().exists());
}

#[test]
fn rules_show_lists_the_installed_text_with_its_size() {
    let h = Home::new("show");
    assert_eq!(
        code(&h.run(&["config", "set", "run.lifecycle_flow", "full"])),
        0
    );
    assert_eq!(code(&h.run(&["rules", "init", "--no-skills"])), 0);
    let o = h.run(&["rules", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let size = pack::contract_text(LifecycleFlow::Full).len();
    assert!(
        out(&o).contains(&format!("10-lifecycle.md  {size} bytes")),
        "{}",
        out(&o)
    );
}

#[test]
fn the_settings_reference_and_ham_config_show_list_the_key() {
    let h = Home::new("docs");
    let o = h.run(&["config", "docs"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(out(&o).contains("`run.lifecycle_flow`"), "{}", out(&o));
    let o = h.run(&["config", "show"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    let line = out(&o)
        .lines()
        .find(|l| l.contains("run.lifecycle_flow"))
        .unwrap_or_else(|| panic!("no row in {}", out(&o)))
        .to_string();
    assert!(line.contains("auto"), "{line}");
}

#[test]
fn the_three_contract_texts_of_pack_1_3_0_are_replaced_by_the_flow_that_is_set() {
    // Pack 1.4.0 changed all three texts; an unedited 1.3.0 file of any flow is replaced, whichever flow the setting names.
    for (label, old) in [
        (
            "auto",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-AGENTS.md.txt"),
        ),
        (
            "full",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-AGENTS-full.md.txt"),
        ),
        (
            "quick",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.3.0-AGENTS-quick.md.txt"),
        ),
    ] {
        for flow in LifecycleFlow::ALL {
            let h = Home::new(&format!("old-1-3-0-{label}-{}", flow.word()));
            std::fs::create_dir_all(h.contract().parent().unwrap()).unwrap();
            std::fs::write(h.contract(), old).unwrap();
            assert_eq!(
                code(&h.run(&["config", "set", "run.lifecycle_flow", flow.word()])),
                0
            );
            let o = h.run(&["rules", "init", "--no-skills"]);
            assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
            assert_eq!(
                h.installed(),
                pack::contract_text(flow),
                "{label} -> {}",
                flow.word()
            );
            assert!(
                out(&o)
                    .lines()
                    .any(|l| l.ends_with("10-lifecycle.md (was pack 1.3.0; not edited by you)")),
                "{label} -> {}: {}",
                flow.word(),
                out(&o)
            );
        }
    }
}

#[test]
fn the_three_contract_texts_of_pack_1_4_0_are_replaced_by_the_flow_that_is_set() {
    // Pack 1.5.0 changed all three texts again; an unedited 1.4.0 file of any flow is replaced, whichever flow the setting names.
    for (label, old) in [
        (
            "auto",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-AGENTS.md.txt"),
        ),
        (
            "full",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-AGENTS-full.md.txt"),
        ),
        (
            "quick",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.4.0-AGENTS-quick.md.txt"),
        ),
    ] {
        for flow in LifecycleFlow::ALL {
            let h = Home::new(&format!("old-1-4-0-{label}-{}", flow.word()));
            std::fs::create_dir_all(h.contract().parent().unwrap()).unwrap();
            std::fs::write(h.contract(), old).unwrap();
            assert_eq!(
                code(&h.run(&["config", "set", "run.lifecycle_flow", flow.word()])),
                0
            );
            let o = h.run(&["rules", "init", "--no-skills"]);
            assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
            assert_eq!(
                h.installed(),
                pack::contract_text(flow),
                "{label} -> {}",
                flow.word()
            );
            assert!(
                out(&o)
                    .lines()
                    .any(|l| l.ends_with("10-lifecycle.md (was pack 1.4.0; not edited by you)")),
                "{label} -> {}: {}",
                flow.word(),
                out(&o)
            );
        }
    }
}

#[test]
fn the_three_contract_texts_of_pack_1_7_0_are_replaced_by_the_flow_that_is_set() {
    // Pack 1.8.0 changed all three texts again; an unedited 1.7.0 file of any flow is replaced, whichever flow the setting names.
    for (label, old) in [
        (
            "auto",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.7.0-AGENTS.md.txt"),
        ),
        (
            "full",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.7.0-AGENTS-full.md.txt"),
        ),
        (
            "quick",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.7.0-AGENTS-quick.md.txt"),
        ),
    ] {
        for flow in LifecycleFlow::ALL {
            let h = Home::new(&format!("old-1-7-0-{label}-{}", flow.word()));
            std::fs::create_dir_all(h.contract().parent().unwrap()).unwrap();
            std::fs::write(h.contract(), old).unwrap();
            assert_eq!(
                code(&h.run(&["config", "set", "run.lifecycle_flow", flow.word()])),
                0
            );
            let o = h.run(&["rules", "init", "--no-skills"]);
            assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
            assert_eq!(
                h.installed(),
                pack::contract_text(flow),
                "{label} -> {}",
                flow.word()
            );
            assert!(
                out(&o)
                    .lines()
                    .any(|l| l.ends_with("10-lifecycle.md (was pack 1.7.0; not edited by you)")),
                "{label} -> {}: {}",
                flow.word(),
                out(&o)
            );
        }
    }
}

#[test]
fn the_three_contract_texts_of_pack_1_6_0_are_replaced_by_the_flow_that_is_set() {
    // Pack 1.7.0 changed all three texts again; an unedited 1.6.0 file of any flow is replaced, whichever flow the setting names.
    for (label, old) in [
        (
            "auto",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.6.0-AGENTS.md.txt"),
        ),
        (
            "full",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.6.0-AGENTS-full.md.txt"),
        ),
        (
            "quick",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.6.0-AGENTS-quick.md.txt"),
        ),
    ] {
        for flow in LifecycleFlow::ALL {
            let h = Home::new(&format!("old-1-6-0-{label}-{}", flow.word()));
            std::fs::create_dir_all(h.contract().parent().unwrap()).unwrap();
            std::fs::write(h.contract(), old).unwrap();
            assert_eq!(
                code(&h.run(&["config", "set", "run.lifecycle_flow", flow.word()])),
                0
            );
            let o = h.run(&["rules", "init", "--no-skills"]);
            assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
            assert_eq!(
                h.installed(),
                pack::contract_text(flow),
                "{label} -> {}",
                flow.word()
            );
            assert!(
                out(&o)
                    .lines()
                    .any(|l| l.ends_with("10-lifecycle.md (was pack 1.6.0; not edited by you)")),
                "{label} -> {}: {}",
                flow.word(),
                out(&o)
            );
        }
    }
}

#[test]
fn the_three_contract_texts_of_pack_1_5_0_are_replaced_by_the_flow_that_is_set() {
    // Pack 1.6.0 changed all three texts again; an unedited 1.5.0 file of any flow is replaced, whichever flow the setting names.
    for (label, old) in [
        (
            "auto",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.5.0-AGENTS.md.txt"),
        ),
        (
            "full",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.5.0-AGENTS-full.md.txt"),
        ),
        (
            "quick",
            include_str!("../../../fixtures/lifecycle/pack-earlier/1.5.0-AGENTS-quick.md.txt"),
        ),
    ] {
        for flow in LifecycleFlow::ALL {
            let h = Home::new(&format!("old-1-5-0-{label}-{}", flow.word()));
            std::fs::create_dir_all(h.contract().parent().unwrap()).unwrap();
            std::fs::write(h.contract(), old).unwrap();
            assert_eq!(
                code(&h.run(&["config", "set", "run.lifecycle_flow", flow.word()])),
                0
            );
            let o = h.run(&["rules", "init", "--no-skills"]);
            assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
            assert_eq!(
                h.installed(),
                pack::contract_text(flow),
                "{label} -> {}",
                flow.word()
            );
            assert!(
                out(&o)
                    .lines()
                    .any(|l| l.ends_with("10-lifecycle.md (was pack 1.5.0; not edited by you)")),
                "{label} -> {}: {}",
                flow.word(),
                out(&o)
            );
        }
    }
}
