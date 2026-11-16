//! The two paths the worker names to the engine at the start of a session, held against what the bundle validator does.
//!
//! The worker tells the engine how the run delivers: a zip takes files under `.github/workflows/` and a root `buildspec.yml`; a pull
//! request refuses a run that adds, changes or removes one, and the run ends blocked. Those words are built from one list
//! (`harness_protocol::direct::REPOSITORY_REFUSES`), and so is the rule of the default rules file. This file is the validator's side: the
//! per-path rules of this build refuse exactly the paths of the list for a pull request, and take and report them for a zip. The
//! validator reads the two paths from the list itself, and every kind of path it protects is sorted here: refused by both deliveries, or
//! a path of the list.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::git::{Change, TreeEntry};
use delivery::validator::{
    DeliveryTarget, EngineConfigMode, Limits, Protected, Rule, evaluate_for, protected_in,
};
use harness_protocol::direct::REPOSITORY_REFUSES;
use std::collections::BTreeMap;

/// A file a run could add for a path of the list: a file under the folder, or the file at the root.
fn example(listed: &str) -> String {
    if listed.ends_with('/') {
        format!("{listed}ci.yml")
    } else {
        listed.to_string()
    }
}

/// One change of the run and the tree of its head: the head holds the file unless the change removed it (`D`).
fn world(path: &str, status: char) -> (Vec<Change>, BTreeMap<String, TreeEntry>) {
    let changes = vec![Change {
        status,
        path: path.into(),
    }];
    let mut tree = BTreeMap::new();
    if status != 'D' {
        tree.insert(
            path.to_string(),
            TreeEntry {
                mode: "100644".into(),
                kind: "blob".into(),
                sha: "5".repeat(40),
                size: Some(120),
                path: path.into(),
            },
        );
    }
    (changes, tree)
}

fn judge(target: DeliveryTarget, path: &str, status: char) -> delivery::validator::Evaluated {
    let (changes, tree) = world(path, status);
    // The globs and the engine-configuration mode run-session sends for every direct run (`plan_delivery`).
    let globs = vec!["**".to_string(), "!.git/**".to_string()];
    evaluate_for(
        target,
        EngineConfigMode::ExecutableOnly,
        &changes,
        &tree,
        &globs,
        &Limits::default(),
    )
}

#[test]
fn b847_the_list_is_the_two_paths_the_validator_protects_for_a_repository() {
    assert_eq!(REPOSITORY_REFUSES, [".github/workflows/", "buildspec.yml"]);
}

/// Every kind of change git reports for a file: added, modified, deleted, and a change of type (a file that became a link). The worker's
/// block says "adds, changes or removes", and each of the four is refused.
#[test]
fn b847_a_pull_request_refuses_a_run_that_adds_changes_or_removes_a_file_of_the_list() {
    for listed in REPOSITORY_REFUSES {
        let path = example(listed);
        for status in ['A', 'M', 'D', 'T'] {
            let found = judge(DeliveryTarget::PullRequest, &path, status);
            assert_eq!(found.violations.len(), 1, "{path} ({status}): {found:?}");
            let v = &found.violations[0];
            assert!(
                matches!(v.rule, Rule::WorkflowFile | Rule::Buildspec),
                "{path} ({status}): {v:?}"
            );
            assert_eq!(v.path.as_deref(), Some(path.as_str()));
            assert!(
                v.detail
                    .contains("a delivery to a repository refuses the change"),
                "{}",
                v.detail
            );
            assert!(
                v.detail.ends_with(&format!("({status})")),
                "the detail names the kind of change: {}",
                v.detail
            );
            assert!(found.findings.is_empty(), "{path} ({status}): {found:?}");
        }
    }
}

#[test]
fn b847_a_zip_takes_a_file_of_the_list_and_reports_it() {
    for listed in REPOSITORY_REFUSES {
        let path = example(listed);
        for status in ['A', 'M', 'T'] {
            let found = judge(DeliveryTarget::Zip, &path, status);
            assert!(
                found.violations.is_empty(),
                "{path} ({status}): a zip takes the file: {found:?}"
            );
            assert_eq!(found.findings.len(), 1, "{path} ({status}): {found:?}");
            assert_eq!(found.findings[0].path.as_deref(), Some(path.as_str()));
            assert!(
                found.findings[0].detail.contains("the zip holds it"),
                "{}",
                found.findings[0].detail
            );
        }
    }
}

/// A zip run may remove such a file as well: the zip then holds nothing of it, so there is nothing to refuse and nothing to report.
#[test]
fn b847_a_zip_takes_the_removal_of_a_file_of_the_list_and_has_nothing_to_report() {
    for listed in REPOSITORY_REFUSES {
        let path = example(listed);
        let found = judge(DeliveryTarget::Zip, &path, 'D');
        assert!(found.violations.is_empty(), "{path}: {found:?}");
        assert!(found.findings.is_empty(), "{path}: {found:?}");
    }
}

/// The worker's words name no path the two kinds treat alike: an ordinary file is taken by both, a build specification below the root is
/// an ordinary file ("a root `buildspec.yml`"), and the engine's own executable configuration is refused by both, which the second rule
/// of the default rules says of every delivery.
#[test]
fn b847_the_two_kinds_differ_for_the_paths_of_the_list_only() {
    for path in ["src/app.py", "ci/buildspec.yml", ".github/dependabot.yml"] {
        for target in [DeliveryTarget::PullRequest, DeliveryTarget::Zip] {
            let found = judge(target, path, 'A');
            assert!(found.violations.is_empty(), "{path} {target:?}: {found:?}");
            assert!(found.findings.is_empty(), "{path} {target:?}: {found:?}");
        }
    }
    for target in [DeliveryTarget::PullRequest, DeliveryTarget::Zip] {
        let found = judge(target, ".cline/hooks/pre.sh", 'A');
        assert_eq!(found.violations.len(), 1, "{target:?}: {found:?}");
        assert_eq!(found.violations[0].rule, Rule::EngineConfig);
    }
}

/// A path of every kind the validator protects, and whether a zip takes a change to it. The match has no wildcard arm: a kind added to
/// the validator does not compile here until it has a path and a side. A kind that a zip takes must then be a path of the list as well
/// (the next test), or the worker's words and the default rules would name too few paths.
fn path_and_side(kind: Protected) -> (&'static str, bool) {
    match kind {
        Protected::Workflow => (".github/workflows/release.yml", true),
        Protected::Buildspec => ("buildspec.yml", true),
        Protected::EngineConfig => (".cline/hooks/pre.sh", false),
    }
}

/// Every kind of `Protected`. `path_and_side` does not compile for a new kind; add it here as well.
const KINDS: [Protected; 3] = [
    Protected::Workflow,
    Protected::Buildspec,
    Protected::EngineConfig,
];

/// Whether `path` is a path of the list: under a listed folder, or the listed file at the root.
fn listed(path: &str) -> bool {
    REPOSITORY_REFUSES.iter().any(|l| {
        if l.ends_with('/') {
            path.starts_with(*l)
        } else {
            path == *l
        }
    })
}

#[test]
fn b847_every_kind_the_validator_protects_is_refused_by_both_deliveries_or_is_a_path_of_the_list() {
    let mut a_zip_takes = 0;
    for kind in KINDS {
        let (path, zip_takes_it) = path_and_side(kind);
        assert_eq!(
            protected_in(EngineConfigMode::ExecutableOnly, path),
            Some(kind),
            "{path}"
        );
        for status in ['A', 'M', 'D', 'T'] {
            let pull_request = judge(DeliveryTarget::PullRequest, path, status);
            assert_eq!(
                pull_request.violations.len(),
                1,
                "a pull request refuses {path} ({status}): {pull_request:?}"
            );
            let zip = judge(DeliveryTarget::Zip, path, status);
            assert_eq!(
                zip.violations.is_empty(),
                zip_takes_it,
                "{path} ({status}) for a zip: {zip:?}"
            );
        }
        assert_eq!(
            listed(path),
            zip_takes_it,
            "{path}: the list names the paths a pull request refuses and a zip takes, and no other"
        );
        if zip_takes_it {
            a_zip_takes += 1;
        }
    }
    assert_eq!(
        a_zip_takes,
        REPOSITORY_REFUSES.len(),
        "one kind for each path of the list"
    );
}

/// The validator takes the two paths from the list and writes neither out a second time, so a path changed in one place cannot leave
/// the other behind.
#[test]
fn b847_the_validator_reads_the_two_paths_from_the_list() {
    let source = include_str!("../src/validator.rs");
    let start = source
        .find("pub fn protected_in(")
        .expect("the validator has `protected_in`");
    let body = &source[start..];
    let body = &body[..body.find("\n}\n").expect("the end of `protected_in`")];
    assert!(body.contains("REPOSITORY_REFUSES"), "{body}");
    for path in REPOSITORY_REFUSES {
        assert!(
            !body.contains(&format!("\"{path}\"")),
            "`protected_in` writes {path} out itself: {body}"
        );
    }
}
