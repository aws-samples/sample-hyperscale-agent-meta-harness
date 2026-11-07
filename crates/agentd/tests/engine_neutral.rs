//! /: `agentd` core names no engine. A case-insensitive word-boundary search for `cline` or
//! `opencode` in `crates/agentd/src`, outside `src/engine/` and `src/cli_transport/`, finds 0
//! lines.
#![allow(clippy::unwrap_used)]

mod common;

use regex::Regex;
use std::path::{Path, PathBuf};

/// Directories of `src` where engine-specific code lives (paths relative to `src`).
const ENGINE_DIRS: [&str; 2] = ["engine/", "cli_transport/"];

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else if p.is_file() {
            out.push(p);
        }
    }
}

/// `(path relative to src, line number, line)` of every match outside the engine directories.
fn matches_in(src: &Path) -> Vec<(String, usize, String)> {
    let word = Regex::new(r"(?i)\b(cline|opencode)\b").unwrap();
    let mut all = Vec::new();
    files(src, &mut all);
    let mut found = Vec::new();
    for path in all {
        let rel = path
            .strip_prefix(src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if ENGINE_DIRS.iter().any(|d| rel.starts_with(d)) {
            continue;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        for (i, line) in text.lines().enumerate() {
            if word.is_match(line) {
                found.push((rel.clone(), i + 1, line.trim().to_string()));
            }
        }
    }
    found
}

#[test]
fn agentd_src_outside_the_engine_directories_names_no_engine() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let found = matches_in(&src);
    let listing: Vec<String> = found
        .iter()
        .map(|(f, n, l)| format!("{f}:{n}: {l}"))
        .collect();
    assert!(
        found.is_empty(),
        "{} engine-name lines outside src/engine/ and src/cli_transport/:\n{}",
        found.len(),
        listing.join("\n")
    );
}

#[test]
fn the_scan_itself_sees_a_name_and_skips_the_engine_directories() {
    let dir = common::scratch("engine-neutral");
    std::fs::create_dir_all(dir.join("engine")).unwrap();
    std::fs::create_dir_all(dir.join("cli_transport")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("engine/a.rs"), "// Cline and OpenCode live here\n").unwrap();
    std::fs::write(dir.join("cli_transport/b.rs"), "// opencode\n").unwrap();
    std::fs::write(dir.join("sub/c.rs"), "ok\n// an OPENCODE word\n").unwrap();
    std::fs::write(
        dir.join("d.rs"),
        "// .clinerules and clinerules are not words\n",
    )
    .unwrap();
    std::fs::write(dir.join("e.rs"), "x = \"cline\";\n").unwrap();
    let found = matches_in(&dir);
    let keys: Vec<(String, usize)> = found.iter().map(|(f, n, _)| (f.clone(), *n)).collect();
    assert_eq!(
        keys,
        vec![("e.rs".to_string(), 1), ("sub/c.rs".to_string(), 2)]
    );
}
