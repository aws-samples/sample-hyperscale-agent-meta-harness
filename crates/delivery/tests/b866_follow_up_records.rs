//! A probe of the unchanged bundle validator. Before the first prompt of a follow-up of a run that reported done, the worker makes
//! one commit of the harness on the seed that takes the earlier request's lifecycle records out of the work tree (`crates/agentd`,
//! `direct::earlier_records`). The validator judges the delivered bundle from the staged base, which for a follow-up is the seed, so it
//! sees those records as removed files. No rule of the validator is changed for it: these tests show that such a bundle validates as it
//! is, for the zip target and for a repository target, and that a follow-up in which the engine changed nothing validates too.
//!
//! The requests are built as JSON, the way run-session sends them for a direct run.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::git::Git;
use delivery::validator::{Gitleaks, ValidateOutput, ValidateRequest, Validator};
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::json;
use std::path::PathBuf;

/// The subject of the commit the worker makes (`agentd::direct::earlier_records::SUBJECT`).
const SUBJECT: &str = "harness: earlier request's records removed from the work tree";

/// The records of the earlier request, as its engine left them under the hidden records folder.
const RECORDS: [(&str, &str); 4] = [
    (
        ".ham/lifecycle/GOAL.md",
        "# Goal\n\n1. `python3 -m pytest -q` -> exit 0\n",
    ),
    (
        ".ham/lifecycle/LOG.md",
        "RUN | python3 -m pytest -q | exit 0 | pass 1 fail 0 | 1 passed\n",
    ),
    (
        ".ham/lifecycle/red/01-split-words.txt",
        "E   assert ['Hello'] == ['hello']\n",
    ),
    (".ham/research/01-split-and-lower.md", "# Split and lower\n"),
];

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    /// The commit the earlier run delivered: the staged base of the follow-up.
    seed: String,
    v: Validator<MemBlobs, Gitleaks>,
}

/// A repository whose head is the seed of a follow-up (the code, the earlier records and the files that stay), and a validator over it.
fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut files = vec![
        (
            "src/words.py",
            "def split_words(text):\n    return text.lower().split()\n",
        ),
        (".ham/context/INDEX.md", "# Context index\n"),
        (".ham/test-command", "python3 -m pytest -q\n"),
    ];
    files.extend(RECORDS);
    let (work, shas) = work_repo(&root, "work", &[files.as_slice()]);
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
        seed: shas[0].clone(),
        v,
    }
}

impl World {
    /// The commit of the harness on the seed: the earlier records leave the tree and nothing else changes.
    fn harness_commit(&self) -> String {
        let gone: Vec<(&str, Option<&str>)> = RECORDS.iter().map(|(p, _)| (*p, None)).collect();
        commit(&self.work, &gone, SUBJECT)
    }

    /// The verified bundle of the head in the bucket.
    fn stage(&self) -> String {
        let f = self.root.join("verified.bundle");
        let head = verified_bundle(&self.work, RUN, &f);
        self.v
            .blobs
            .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
        head
    }

    /// The request run-session sends for a direct run, with `target` when one is given.
    fn request(&self, head: &str, target: Option<&str>) -> ValidateRequest {
        let mut v = json!({
            "runId": RUN, "headSha": head, "baseSha": self.seed,
            "allowedGlobs": ["**", "!.git/**"], "engineConfig": "executable-only",
        });
        if let Some(t) = target {
            v["target"] = json!(t);
        }
        serde_json::from_value(v).unwrap()
    }

    async fn validate(&self, head: &str, target: Option<&str>) -> ValidateOutput {
        self.v.validate(&self.request(head, target)).await.unwrap()
    }
}

/// The targets a direct run is validated for: a zip, a pull request, and a request without the member (what an older caller sends).
const TARGETS: [Option<&str>; 3] = [Some("zip"), Some("pull-request"), None];

#[tokio::test]
async fn b866_the_validator_accepts_a_follow_up_whose_first_commit_takes_records_out() {
    let w = world();
    let harness = w.harness_commit();
    // TRIGGER PRESENT: the first commit after the seed removes the four records and nothing else.
    assert_eq!(
        git(&w.work, &["diff", "--name-status", &w.seed, &harness]),
        RECORDS
            .iter()
            .map(|(p, _)| format!("D\t{p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // One commit of the engine on top: the follow-up's own work.
    commit(
        &w.work,
        &[(
            "src/words.py",
            Some("def split_words(text):\n    return text.upper().split()\n"),
        )],
        "Return the words in upper case",
    );
    let head = w.stage();
    for target in TARGETS {
        let out = w.validate(&head, target).await;
        assert!(out.pass, "{target:?}: {:?}", out.violations);
        assert!(
            out.violations.is_empty(),
            "{target:?}: {:?}",
            out.violations
        );
        assert_eq!(
            out.changed_files, 5,
            "{target:?}: four records removed and one file changed"
        );
        assert_eq!(out.head_sha, head);
        assert_eq!(out.base_sha.as_deref(), Some(w.seed.as_str()));
    }
}

#[tokio::test]
async fn b866_the_validator_accepts_a_follow_up_that_ended_on_the_harness_commit() {
    // The engine changed no file: the delivered head is the harness commit itself.
    let w = world();
    let harness = w.harness_commit();
    let head = w.stage();
    assert_eq!(head, harness);
    for target in TARGETS {
        let out = w.validate(&head, target).await;
        assert!(out.pass, "{target:?}: {:?}", out.violations);
        assert_eq!(out.changed_files, 4, "{target:?}: the four records removed");
    }
}
