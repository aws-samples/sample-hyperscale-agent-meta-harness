//! A delivered zip made by the real packager (`delivery::packager::Packager::package`) from a real git repository, for the
//! `ham clone` tests that must not model the zip by hand: the packager's `git archive` and its raw copy decide what `src/` holds
//! (names, bytes, modes, symlinks), and `ham clone` has to accept exactly that. Only the two ports the packager reads and writes
//! (S3 objects and the artifact item) are in memory; git runs for real, with its configuration isolated.
#![allow(dead_code, clippy::unwrap_used)]

use super::clonefix::{Item, git, write_item};
use super::support;
use delivery::error::DeliveryError;
use delivery::git::Git as DeliveryGit;
use delivery::packager::{PackageRequest, Packager};
use delivery::ports::{Artifacts, Blobs, PutMeta};
use harness_protocol::store::{ArtifactItem, s3};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A run id the packager accepts (a ULID).
pub const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
pub const REPO: &str = "python-calc";
const BUCKET: &str = "hamh-live-harness-test";

#[derive(Default)]
struct MemBlobs(Mutex<BTreeMap<String, Vec<u8>>>);

impl Blobs for MemBlobs {
    async fn get_to_file(&self, key: &str, path: &Path) -> Result<Option<u64>, DeliveryError> {
        let found = self.0.lock().unwrap().get(key).cloned();
        Ok(found.map(|b| {
            std::fs::write(path, &b).unwrap();
            b.len() as u64
        }))
    }
    async fn put_file(&self, key: &str, path: &Path, _meta: &PutMeta) -> Result<(), DeliveryError> {
        self.0
            .lock()
            .unwrap()
            .insert(key.into(), std::fs::read(path).unwrap());
        Ok(())
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, DeliveryError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

struct NoArtifacts;

impl Artifacts for NoArtifacts {
    async fn put_artifact(&self, _item: &ArtifactItem) -> Result<(), DeliveryError> {
        Ok(())
    }
}

/// What the packager delivered.
pub struct Delivered {
    pub dir: PathBuf,
    /// The source repository (a work tree on branch `harness/<RUN>`).
    pub repo: PathBuf,
    /// The zip the packager wrote, saved to disk.
    pub zip: PathBuf,
    pub head: String,
    pub top: String,
}

/// Commits `first` then `second` on `harness/<RUN>` (with conversions as the repository's own `.gitattributes` say), hands the
/// verified bundle to the packager and returns the zip it makes.
pub async fn deliver(label: &str, first: &[Item], second: &[Item]) -> Delivered {
    let dir = support::scratch(label);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let branch = format!("harness/{RUN}");
    git(&repo, &["init", "-q", "--initial-branch", &branch]);
    for (items, msg) in [(first, "first"), (second, "second")] {
        for it in items {
            write_item(&repo, it);
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    }
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let base = git(&repo, &["rev-parse", "HEAD~1"]);
    let verified = dir.join("verified.bundle");
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            verified.to_str().unwrap(),
            &branch,
        ],
    );

    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let packager = Packager {
        blobs: MemBlobs::default(),
        artifacts: NoArtifacts,
        git: DeliveryGit::new("git", &home),
        bucket: BUCKET.into(),
        work_root: dir.join("scratch"),
    };
    packager.blobs.0.lock().unwrap().insert(
        s3::verified_bundle_key(RUN),
        std::fs::read(&verified).unwrap(),
    );
    // The packager makes one kind of zip: a direct run's, with what Cline reported (`PackageRequest.direct`).
    let request = serde_json::json!({
        "runId": RUN, "repo": REPO, "baseSha": base, "headSha": head,
        "objective": "Add percent_change(a, b) with tests",
        "direct": {"outcome": "reported_done", "label": "Cline reported done", "modelId": "us.anthropic.claude-opus-5-5",
                   "rules": [], "headSource": "final"}
    });
    let request: PackageRequest = serde_json::from_value(request).unwrap();
    packager.package(&request).await.unwrap();

    let key = s3::deliverable_key(RUN, REPO, &head[..7]);
    let zip = dir.join("delivered.zip");
    std::fs::write(&zip, packager.blobs.0.lock().unwrap().get(&key).unwrap()).unwrap();
    Delivered {
        top: delivery::packager::top_dir(REPO, RUN, &head),
        dir,
        repo,
        zip,
        head,
    }
}
