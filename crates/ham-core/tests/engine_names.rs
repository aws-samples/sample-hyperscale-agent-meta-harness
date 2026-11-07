//! No client code outside `ham_core::engines` names an engine. The agentd test
//! `agentd_src_outside_the_engine_directories_names_no_engine` is the model; this one is a ratchet because the client's product copy
//! already says "Cline" in places (the frozen outcome labels, the correction path, the help lines). It counts the words `cline`,
//! `opencode`, `pi`, `kiro` and `codex` (ignoring case, on word boundaries) in the sources of `ham`, `ham-core` and `control-api`, and it
//! fails when a file has more than the count recorded here or is not on the list at all. A file that must name an engine moves the word to
//! `engines.rs`; the recorded counts only ever go down.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const WORDS: [&str; 5] = ["cline", "opencode", "pi", "kiro", "codex"];

/// The words found in each source file at the commit that added this test (paths relative to `crates/`). Files that main added after it
/// (`pack.rs`, `lifecycle_cmd.rs`, the `continue` and `lifecycle` commands of `command.rs`, `registry.rs` and `cli.rs`, then `goal_card.rs` and
/// `goal_brake.rs`) were recorded at the merges of main of 2026-10-16 and 2026-10-17; their counts go down like the others. The help of the command line (`help.rs`) names the engine only through `engines::say`; its two words are the `--kiro` flag of `ham lifecycle init` and the `.kiro/skills` folder that flag links. The copy and export notices of the client wiring (TC1: `tui/keys.rs`, and two more words in `tui/app.rs` and one in `tui/chat.rs`) were recorded when that branch met this one. A change that adds an engine
/// word to a file raises its line here in the same commit, or puts the word in `engines.rs` (`engines::say`).
const RECORDED: &[(&str, usize)] = &[
    ("ham-core/src/agents.rs", 1),
    ("ham-core/src/alert.rs", 3),
    ("ham-core/src/chat.rs", 8),
    ("ham-core/src/command.rs", 24),
    ("ham-core/src/config.rs", 2),
    ("ham-core/src/failover.rs", 1),
    ("ham-core/src/goal_card.rs", 8),
    ("ham-core/src/help.rs", 2),
    ("ham-core/src/input.rs", 27),
    ("ham-core/src/messages.rs", 11),
    ("ham-core/src/meters.rs", 1),
    ("ham-core/src/models.rs", 1),
    ("ham-core/src/notice.rs", 4),
    ("ham-core/src/pack.rs", 15),
    ("ham-core/src/registry.rs", 7),
    ("ham-core/src/render.rs", 5),
    ("ham-core/src/request.rs", 2),
    ("ham-core/src/router.rs", 3),
    ("ham-core/src/rules.rs", 8),
    ("ham-core/src/run_index.rs", 3),
    ("ham-core/src/runfilter.rs", 1),
    ("ham-core/src/state.rs", 7),
    ("ham/src/api.rs", 4),
    ("ham/src/cli.rs", 4),
    ("ham/src/commands.rs", 17),
    ("ham/src/config_cmd.rs", 1),
    ("ham/src/lifecycle_cmd.rs", 11),
    ("ham/src/message_routes.rs", 2),
    ("ham/src/rules_cmd.rs", 2),
    ("ham/src/start.rs", 2),
    ("ham/src/tui/app.rs", 24),
    ("ham/src/tui/chat.rs", 2),
    ("ham/src/tui/goal_brake.rs", 1),
    ("ham/src/tui/keys.rs", 1),
    ("ham/src/tui/mod.rs", 1),
    ("ham/src/tui/ui.rs", 15),
    ("ham/src/watch.rs", 2),
    ("control-api/src/controls.rs", 6),
    ("control-api/src/direct.rs", 2),
    ("control-api/src/direct_models.rs", 4),
    ("control-api/src/handler.rs", 2),
    ("control-api/src/input.rs", 2),
    ("control-api/src/lib.rs", 2),
    ("control-api/src/messages.rs", 4),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Matches of `WORDS` in `text`: ASCII letters and digits make a word, an engine name is a whole word, ignoring case.
fn count(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut n = 0;
    for w in WORDS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(w) {
            let at = from + i;
            let end = at + w.len();
            let before = at == 0 || !word(bytes[at - 1]);
            let after = end >= bytes.len() || !word(bytes[end]);
            if before && after {
                n += 1;
            }
            from = at + w.len();
        }
    }
    n
}

fn found() -> BTreeMap<String, usize> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for c in ["ham", "ham-core", "control-api"] {
        rust_files(&crates.join(c).join("src"), &mut files);
    }
    let mut out = BTreeMap::new();
    for f in files {
        let rel = f
            .strip_prefix(crates)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if rel == "ham-core/src/engines.rs" {
            continue;
        }
        let n = count(&std::fs::read_to_string(&f).unwrap());
        if n > 0 {
            out.insert(rel, n);
        }
    }
    out
}

#[test]
fn engines_rs_is_the_one_module_that_holds_the_names() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let text = std::fs::read_to_string(crates.join("ham-core/src/engines.rs"))
        .expect("crates/ham-core/src/engines.rs holds the engine names");
    assert!(
        count(&text) >= WORDS.len(),
        "the roadmap names every engine of WORDS"
    );
}

#[test]
fn no_file_names_an_engine_more_often_than_when_the_ratchet_was_set() {
    let recorded: BTreeMap<&str, usize> = RECORDED.iter().copied().collect();
    let mut over = Vec::new();
    for (file, n) in found() {
        match recorded.get(file.as_str()) {
            Some(max) if n <= *max => {}
            Some(max) => over.push(format!("{file}: {n} engine names, recorded {max}")),
            None => over.push(format!(
                "{file}: {n} engine names in a file that had none; put the name in ham_core::engines"
            )),
        }
    }
    assert!(over.is_empty(), "{}", over.join("\n"));
}

#[test]
fn the_counter_reads_whole_words_and_ignores_case() {
    assert_eq!(count("Cline, OPENCODE and pi."), 3);
    assert_eq!(count("pipe api pin inclined clinef codexes"), 0);
    assert_eq!(count("kiro_cli kiro-cli"), 1);
}
