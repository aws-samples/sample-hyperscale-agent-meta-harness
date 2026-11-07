//! The seam of the direct driver and the sandbox. A module that starts a process of the engine's family builds its
//! launch from the context the driver gives it, so the process inherits the sandbox of the run; a launch written by hand with `sandbox: None` would
//! start an unconfined process in the `landlock` mode. This file pins the places where `sandbox: None` is written (a scan of the sources), and the seam
//! document (`src/direct/AGENTS.md`), which owns the hook contract and must say that the sandbox is part of it.
#![allow(clippy::unwrap_used)]

mod common;

use common::repo_root;
use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn agentd_sources() -> Vec<(String, Vec<String>)> {
    let root = repo_root();
    let mut files = Vec::new();
    sources(&root.join("crates/agentd/src"), &mut files);
    files.sort();
    files
        .iter()
        .map(|f| {
            (
                f.strip_prefix(&root).unwrap().display().to_string(),
                std::fs::read_to_string(f)
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect(),
            )
        })
        .collect()
}

/// The three places that write `sandbox: None`, and why each is not a process of the engine's family: the default of `CleanLaunch::new`, which
/// `StartupCtx::clean_launch` replaces through `with_sandbox` before a module sees it; the syntax check of a report step (`sh -n`, which runs nothing);
/// and the context of the tool server's conformance example and tests (`ToolContext::for_directory`).
const NONE_ALLOWED: [(&str, usize); 3] = [
    ("crates/agentd/src/direct/hooks.rs", 1),
    ("crates/agentd/src/direct/report/run.rs", 1),
    ("crates/agentd/src/direct/tools/backend.rs", 1),
];

#[test]
fn a_launch_of_the_engines_family_is_never_written_with_no_sandbox() {
    let mut found: Vec<(String, usize)> = Vec::new();
    for (file, lines) in agentd_sources() {
        let n = lines
            .iter()
            .filter(|l| !l.trim_start().starts_with("//") && l.contains("sandbox: None"))
            .count();
        if n > 0 {
            found.push((file, n));
        }
    }
    let allowed: Vec<(String, usize)> = NONE_ALLOWED
        .iter()
        .map(|(f, n)| ((*f).to_string(), *n))
        .collect();
    assert_eq!(
        found, allowed,
        "`sandbox: None` is written somewhere new: a process of the engine's family must be built from the context (`ctx.launch_spec`, `ctx.clean_launch()`)"
    );
}

#[test]
fn only_the_context_builds_a_clean_launch_and_every_launch_spec_literal_names_its_sandbox() {
    for (file, lines) in agentd_sources() {
        for (i, line) in lines.iter().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if code.contains("CleanLaunch::new(") && file != "crates/agentd/src/direct/hooks.rs" {
                panic!(
                    "{file}:{}: a CleanLaunch is built by the context, not by a module",
                    i + 1
                );
            }
            // A struct literal of a launch (not its definition, not a function that returns one).
            let literal = code.contains("LaunchSpec {")
                && !code.contains("pub struct")
                && !code.contains("fn ")
                && !code.contains("impl ");
            if literal {
                let next: String = lines[i..(i + 14).min(lines.len())].join("\n");
                assert!(
                    next.contains("sandbox:"),
                    "{file}:{}: a launch that does not say which sandbox it starts in",
                    i + 1
                );
            }
        }
    }
}

fn seam_document() -> String {
    std::fs::read_to_string(repo_root().join("crates/agentd/src/direct/AGENTS.md")).unwrap()
}

#[test]
fn the_seam_document_lists_the_sandbox_in_the_context_of_a_module() {
    let doc = seam_document();
    let row = doc
        .lines()
        .find(|l| l.contains("| `on_startup` (async) |"))
        .expect("the on_startup row of the hook table");
    assert!(
        row.contains("sandbox"),
        "the StartupCtx field list names the sandbox: {row}"
    );
    assert!(
        doc.contains("inherits the sandbox"),
        "the rule that every launch of the engine's family is built from the context is stated"
    );
}

#[test]
fn the_seam_document_says_how_the_sandbox_changes_the_report_step_the_evidence_and_the_tool_environment()
 {
    let doc = seam_document();
    for (needle, why) in [
        (
            "sandbox-exec",
            "a report step in the `landlock` mode starts through the launcher, not `setpriv`",
        ),
        (
            "for_engine_family",
            "the evidence of a running command in the `landlock` mode is the family below the engine's process",
        ),
        (
            "Landlock domain",
            "a tool's child runs in the engine's domain",
        ),
    ] {
        assert!(
            doc.contains(needle),
            "src/direct/AGENTS.md lacks `{needle}`: {why}"
        );
    }
}
