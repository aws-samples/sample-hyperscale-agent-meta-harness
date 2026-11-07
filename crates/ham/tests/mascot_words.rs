//! What the repository says about the ham mascot. The mascot is an original pixel drawing with no logo or text: it is not a bird.
//! No file a person or a coding agent reads names it a bird of any kind, and the files that describe the art say what it is (and what it is
//! not: no logo, no text) in the same words.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file under `dir` whose name satisfies `keep`, without the build output and the version control directory.
fn files_under(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !matches!(name.as_str(), "target" | ".git" | "node_modules") {
                files_under(&path, keep, out);
            }
        } else if keep(&path) {
            out.push(path);
        }
    }
}

/// The files that describe the mascot to a reader: the README, the documents, every `AGENTS.md`, the module that draws the mascot and the
/// module that places it, and the tests that name it.
fn user_facing() -> Vec<PathBuf> {
    let root = root();
    let mut out = vec![root.join("README.md")];
    files_under(&root.join("docs"), &|_| true, &mut out);
    files_under(
        &root,
        &|p| p.file_name().is_some_and(|n| n == "AGENTS.md"),
        &mut out,
    );
    for rel in [
        "crates/ham/src/tui/mascot.rs",
        "crates/ham/src/tui/head.rs",
        "crates/ham/tests/mascot_art.rs",
        "crates/ham/tests/mascot_svg.rs",
        "crates/ham/tests/tui_mascot_screen.rs",
        "crates/ham/tests/tui_head.rs",
    ] {
        out.push(root.join(rel));
    }
    out.sort();
    out.dedup();
    out
}

/// The positions of the whole word `word` (or `word` and `s`) in `text`, lower-cased.
fn whole_word(text: &str, word: &str) -> Vec<usize> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find(word) {
        let start = from + at;
        let mut end = start + word.len();
        if bytes.get(end) == Some(&b's') {
            end += 1;
        }
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            found.push(start);
        }
        from = start + word.len();
    }
    found
}

#[test]
fn no_file_that_describes_the_mascot_calls_it_a_bird() {
    // The word is built from pieces so that this file does not name it either.
    let word = ["cr", "ow"].concat();
    let mut hits = Vec::new();
    let files = user_facing();
    assert!(files.len() > 10, "found only {} files to read", files.len());
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for at in whole_word(&text, &word) {
            let line = text[..at].matches('\n').count() + 1;
            hits.push(format!("{}:{line}", path.display()));
        }
    }
    assert!(
        hits.is_empty(),
        "the mascot is not a bird; these places still call it one: {hits:#?}"
    );
}

#[test]
fn the_word_finder_ignores_other_words_that_hold_those_letters() {
    let word = ["cr", "ow"].concat();
    assert!(whole_word("a crowd gathers", &word).is_empty());
    assert!(whole_word("the crowded table", &word).is_empty());
    assert_eq!(whole_word(&format!("The {word}s sing"), &word).len(), 1);
    assert_eq!(whole_word(&format!("{word}."), &word).len(), 1);
}

/// `text` with the comment markers of Rust doc lines and every run of white space reduced to one space.
fn flat(text: &str) -> String {
    text.lines()
        .map(|l| {
            l.trim_start()
                .trim_start_matches("//!")
                .trim_start_matches('-')
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What every file that describes the art says about it, word for word.
const WHAT_IT_IS: &str = "an original pixel drawing with no logo or text";

#[test]
fn the_files_that_describe_the_art_say_what_it_is() {
    for rel in [
        "docs/images/AGENTS.md",
        "crates/ham/src/tui/AGENTS.md",
        "crates/ham/src/tui/mascot.rs",
        "docs/user-guide.md",
    ] {
        let text = std::fs::read_to_string(root().join(rel)).unwrap();
        assert!(
            flat(&text).contains(WHAT_IT_IS),
            "{rel} does not say: {WHAT_IT_IS}"
        );
    }
}
