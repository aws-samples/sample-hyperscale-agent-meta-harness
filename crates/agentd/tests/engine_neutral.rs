//! /: `agentd` core names no engine. A case-insensitive word-boundary search for `cline` or
//! `opencode` in `crates/agentd/src`, outside `src/engine/` and `src/cli_transport/`, finds 0
//! lines.
//!
//! The tools of an engine are named in `src/engine/tools.rs` and nowhere else. The same search for the tool names of the qualified engine
//! (`b789_agentd_src_outside_the_engine_directories_names_no_engine_tool`) finds 0 lines.
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

/// The tool names of the qualified engine, case-sensitive and whole words (`editor` only in quotes: it is also a plain word).
const TOOL_NAMES: &str = r#"\b(apply_patch|fetch_web_content|read_files|run_commands|search_codebase|spawn_agent|mark_goal_complete)\b|"editor""#;

/// `(path relative to src, line number, line)` of every match of the engine words outside the engine directories.
fn matches_in(src: &Path) -> Vec<(String, usize, String)> {
    matches_of(src, &Regex::new(r"(?i)\b(cline|opencode)\b").unwrap())
}

/// `(path relative to src, line number, line)` of every match of `word` outside the engine directories.
fn matches_of(src: &Path, word: &Regex) -> Vec<(String, usize, String)> {
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

#[test]
fn b789_agentd_src_outside_the_engine_directories_names_no_engine_tool() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let found = matches_of(&src, &Regex::new(TOOL_NAMES).unwrap());
    let listing: Vec<String> = found
        .iter()
        .map(|(f, n, l)| format!("{f}:{n}: {l}"))
        .collect();
    assert!(
        found.is_empty(),
        "{} tool-name lines outside src/engine/ and src/cli_transport/ (they belong in src/engine/tools.rs):\n{}",
        found.len(),
        listing.join("\n")
    );
}

#[test]
fn b789_the_tool_scan_sees_each_tool_name_and_skips_the_engine_directories() {
    let dir = common::scratch("engine-neutral-tools");
    std::fs::create_dir_all(dir.join("engine")).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("engine/a.rs"), "name: \"spawn_agent\",\n").unwrap();
    std::fs::write(
        dir.join("sub/c.rs"),
        "ok\n// a title like run_commands: ls\nlet t = \"apply_patch\";\nlet u = \"editor\";\nthe editor is a word\n",
    )
    .unwrap();
    // A longer identifier is another name; the notice text and a doc comment count like code.
    std::fs::write(
        dir.join("d.rs"),
        "spawn_agents and my_read_files are other names\n\"do not start helper agents (spawn_agent) again\"\nmark_goal_complete\nfetch_web_content search_codebase read_files\n",
    )
    .unwrap();
    let found = matches_of(&dir, &Regex::new(TOOL_NAMES).unwrap());
    let keys: Vec<(String, usize)> = found.iter().map(|(f, n, _)| (f.clone(), *n)).collect();
    assert_eq!(
        keys,
        vec![
            ("d.rs".to_string(), 2),
            ("d.rs".to_string(), 3),
            ("d.rs".to_string(), 4),
            ("sub/c.rs".to_string(), 2),
            ("sub/c.rs".to_string(), 3),
            ("sub/c.rs".to_string(), 4),
        ]
    );
}
