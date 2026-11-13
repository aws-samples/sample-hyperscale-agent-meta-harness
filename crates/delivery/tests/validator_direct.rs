//! `bundle-validator` for a direct run:
//! `ValidateRequest.engineConfig` is `all` (the default: every path under `.cline/`, `.clinerules/`, `.opencode/` and
//! `opencode.json` is engine configuration) or `executable-only`, where only what the engine would execute is:
//! hooks, plugins and agents. Cline writes `.clinerules/*.md` or `.cline/rules/*.md` as ordinary project notes, and a
//! finished run must not be discarded for one. Workflow files and the PR-check buildspec stay protected in both modes.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::git::Git;
use delivery::validator::{
    EngineConfigMode, Gitleaks, Limits, Protected, Rule, ValidateRequest, Validator, protected,
    protected_in,
};
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::json;
use std::path::PathBuf;

#[test]
fn executable_only_protects_hooks_plugins_agents_and_the_other_engines_files() {
    for p in [
        ".cline/hooks/PreToolUse",
        ".cline/plugins/p.js",
        ".cline/agents/a.md",
        ".clinerules/hooks/x",
        "opencode.json",
        ".opencode/agent/x.md",
    ] {
        assert_eq!(
            protected_in(EngineConfigMode::ExecutableOnly, p),
            Some(Protected::EngineConfig),
            "{p}"
        );
    }
}

#[test]
fn executable_only_lets_rule_notes_and_settings_through() {
    for p in [
        ".clinerules/style.md",
        ".clinerules/notes/todo.md",
        ".cline/rules/x.md",
        ".cline/settings.json",
        ".cline/hooks",
        ".cline/hooksfile",
        ".clinerules/hooksnotes.md",
        "docs/.cline/hooks/x",
    ] {
        assert_eq!(
            protected_in(EngineConfigMode::ExecutableOnly, p),
            None,
            "{p}"
        );
    }
}

#[test]
fn workflows_and_the_buildspec_are_protected_in_both_modes() {
    for mode in [EngineConfigMode::All, EngineConfigMode::ExecutableOnly] {
        assert_eq!(
            protected_in(mode, ".github/workflows/ci.yml"),
            Some(Protected::Workflow),
            "{mode:?}"
        );
        assert_eq!(
            protected_in(mode, "buildspec.yml"),
            Some(Protected::Buildspec),
            "{mode:?}"
        );
    }
}

#[test]
fn all_mode_is_the_old_rule_and_the_default() {
    assert_eq!(EngineConfigMode::default(), EngineConfigMode::All);
    for p in [
        ".cline/hooks/PreToolUse",
        ".cline/rules/x.md",
        ".clinerules/style.md",
        "opencode.json",
        ".opencode/agent/x.md",
        ".github/workflows/ci.yml",
        "buildspec.yml",
        "src/calc.py",
    ] {
        assert_eq!(protected_in(EngineConfigMode::All, p), protected(p), "{p}");
    }
}

#[test]
fn the_request_reads_engine_config_and_leaves_the_default_off_the_wire() {
    let base = json!({"runId": RUN, "headSha": "a".repeat(40), "allowedGlobs": ["**"]});
    let r: ValidateRequest = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(r.engine_config, EngineConfigMode::All);
    assert!(
        serde_json::to_value(&r)
            .unwrap()
            .get("engineConfig")
            .is_none(),
        "the default is not written, so an old line's request keeps its bytes"
    );
    let mut with = base.clone();
    with["engineConfig"] = json!("executable-only");
    let r: ValidateRequest = serde_json::from_value(with).unwrap();
    assert_eq!(r.engine_config, EngineConfigMode::ExecutableOnly);
    assert_eq!(
        serde_json::to_value(&r).unwrap()["engineConfig"],
        "executable-only"
    );
    let mut bad = base;
    bad["engineConfig"] = json!("none");
    assert!(serde_json::from_value::<ValidateRequest>(bad).is_err());
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    base: String,
    v: Validator<MemBlobs, Gitleaks>,
}

fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[&[("src/calc.py", "def add(a, b):\n    return a + b\n")]],
    );
    let v = Validator {
        blobs: MemBlobs::default(),
        scanner: Gitleaks {
            bin: std::env::var_os("GITLEAKS_BIN")
                .map_or_else(|| PathBuf::from("gitleaks"), PathBuf::from),
            home: home.clone(),
        },
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: root.join("scratch"),
    };
    World {
        _tmp: tmp,
        root,
        work,
        base: shas[0].clone(),
        v,
    }
}

impl World {
    fn turn(&self, files: &[(&str, Option<&str>)]) -> String {
        commit(&self.work, files, "agent turn");
        let f = self.root.join("verified.bundle");
        let head = verified_bundle(&self.work, RUN, &f);
        self.v
            .blobs
            .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
        head
    }
    fn req(&self, head: &str, mode: EngineConfigMode) -> ValidateRequest {
        ValidateRequest {
            run_id: RUN.into(),
            base_sha: Some(self.base.clone()),
            head_sha: head.into(),
            allowed_globs: vec!["**".into(), "!.git/**".into()],
            limits: Limits::default(),
            source_bundle_uri: None,
            source_bundle_version_id: None,
            source_bundle_sha256: None,
            engine_config: mode,
            third_party: Default::default(),
            target: Default::default(),
            source_run_ids: Vec::new(),
        }
    }
}

#[tokio::test]
async fn a_rule_note_passes_for_a_direct_run_and_fails_in_all_mode() {
    let w = world();
    let head = w.turn(&[
        (".clinerules/notes.md", Some("# notes Cline kept\n")),
        (
            "src/calc.py",
            Some("def add(a, b):\n    return a + b + 0\n"),
        ),
    ]);
    let direct =
        w.v.validate(&w.req(&head, EngineConfigMode::ExecutableOnly))
            .await
            .unwrap();
    assert!(direct.pass, "{:?}", direct.violations);
    let all =
        w.v.validate(&w.req(&head, EngineConfigMode::All))
            .await
            .unwrap();
    assert!(!all.pass);
    assert_eq!(all.violations[0].rule, Rule::EngineConfig);
}

#[tokio::test]
async fn a_hook_fails_in_both_modes() {
    let w = world();
    let head = w.turn(&[(".cline/hooks/PreToolUse", Some("#!/bin/sh\ntrue\n"))]);
    for mode in [EngineConfigMode::All, EngineConfigMode::ExecutableOnly] {
        let out = w.v.validate(&w.req(&head, mode)).await.unwrap();
        assert!(!out.pass, "{mode:?}");
        assert_eq!(
            out.violations
                .iter()
                .map(|v| (v.rule, v.path.clone()))
                .collect::<Vec<_>>(),
            vec![(Rule::EngineConfig, Some(".cline/hooks/PreToolUse".into()))],
            "{mode:?}"
        );
    }
}
