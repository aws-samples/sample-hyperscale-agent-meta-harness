//! What the tests of copies that leave out the run's base share: a repository with the bundle a run starts from, a private
//! repository of copies with the form `base`, the restore every reader makes (the base first, then the copy), and a later session of
//! a run in a new machine.
#![allow(dead_code, clippy::unwrap_used)]

use crate::common::{git, python_calc, scratch};
use crate::direct_support::{BUCKET, Direct, Opts, RUN};
use agentd::direct::snapshot::{BundleForm, Snapshotter};
use agentd::workspace::prepare_from_bundle_state;
use harness_protocol::bundle_header::{HEADER_BYTES_MAX, heads, needs};
use harness_protocol::direct::{
    DirectSlice, DirectSummary, s3 as keys, validate_direct_invocation,
};
use harness_protocol::runtime::RepoSource;
use std::path::{Path, PathBuf};

/// The first bytes of a bundle file: what a reader takes to find its header.
pub fn prefix(bundle: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(bundle).unwrap();
    bytes.truncate(usize::try_from(HEADER_BYTES_MAX).unwrap());
    bytes
}

/// The commits the bundle at `bundle` needs and does not hold.
pub fn needed(bundle: &Path) -> Vec<String> {
    needs(&prefix(bundle)).unwrap()
}

/// The bundle a run starts from, as the service stages it: `HEAD` and the branch, so one commit under two names.
pub fn stage_base(repo: &Path, out: &Path) {
    let branch = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    git(
        repo,
        &[
            "bundle",
            "create",
            "-q",
            out.to_str().unwrap(),
            "HEAD",
            &format!("refs/heads/{branch}"),
        ],
    );
}

/// A repository, the bundle a run starts from, and the commit that bundle names.
pub struct Lab {
    pub dir: PathBuf,
    pub repo: PathBuf,
    pub base_bundle: PathBuf,
    pub base: String,
}

pub fn lab(label: &str) -> Lab {
    let dir = scratch(label);
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let base_bundle = dir.join("repo.bundle");
    stage_base(&repo, &base_bundle);
    Lab {
        dir,
        repo,
        base_bundle,
        base,
    }
}

/// The private repository of a session's copies with the form `base`: the floors are the heads the base bundle's own header names.
pub async fn thin(l: &Lab, data: &str) -> Snapshotter {
    let data = l.dir.join(data);
    std::fs::create_dir_all(&data).unwrap();
    let mut s = Snapshotter::new(&data, &l.repo, &l.base).await.unwrap();
    s.set_form(BundleForm::AboveFloor);
    s.set_floors(heads(&prefix(&l.base_bundle)).unwrap());
    s
}

/// What every reader of a copy does: the base first, then the copy. The product's own restore makes the checkout, and the result
/// is checked with git: the repository is at `commit`, nothing differs from it, and every object is there.
pub async fn restored(l: &Lab, name: &str, copy: &Path, commit: &str) -> PathBuf {
    let root = l.dir.join(format!("restore-{name}"));
    let copy_needs = !needed(copy).is_empty();
    let (wt, fresh) = prepare_from_bundle_state(
        &root,
        "run",
        "direct",
        copy,
        copy_needs.then_some(l.base_bundle.as_path()),
    )
    .await
    .unwrap_or_else(|e| panic!("the copy {name} does not restore: {e}"));
    assert!(fresh);
    assert_eq!(git(&wt.repo, &["rev-parse", "HEAD"]), commit, "{name}");
    assert_eq!(git(&wt.repo, &["status", "--porcelain"]), "", "{name}");
    git(&wt.repo, &["fsck", "--no-progress", "--connectivity-only"]);
    wt.repo
}

/// `n` bytes that no compression makes smaller.
pub fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 33) as u8
        })
        .collect()
}

// ---- whole turns -----------------------------------------------------------------------------------------------------------------

/// The options of a turn whose copies have the form `base`.
pub fn base_form(mut opts: Opts) -> Opts {
    opts.edit_cfg = Some(Box::new(|cfg| {
        cfg.direct.copy_form = BundleForm::AboveFloor;
    }));
    opts
}

/// The options of a turn whose copies hold the whole history, whatever the form of the release is.
pub fn whole_form(mut opts: Opts) -> Opts {
    opts.edit_cfg = Some(Box::new(|cfg| {
        cfg.direct.copy_form = BundleForm::Whole;
    }));
    opts
}

pub fn record_of(d: &Direct, session: u32) -> serde_json::Value {
    serde_json::from_slice(
        &d.aws
            .get(&keys::snapshot_json_key_t(RUN, session))
            .expect("snapshot.json"),
    )
    .unwrap()
}

/// The summary of session `session` of the run (`Direct::summary` reads the first session's).
pub fn summary_of(d: &Direct, session: u32) -> DirectSummary {
    let bytes = d
        .aws
        .get(&keys::summary_key_t(RUN, session))
        .expect("summary.json of the session");
    let s: DirectSummary = serde_json::from_slice(&bytes).unwrap();
    s.validate().unwrap();
    s
}

pub fn stored_copy(d: &Direct, session: u32) -> Vec<u8> {
    d.aws
        .get(&keys::snapshot_bundle_key_t(RUN, session))
        .expect("snapshot.bundle")
}

/// Session 2 of the run of `first` in a new machine, restoring `object` (stored under `key`), which holds `head`. The bucket holds
/// the bundle the run started from, as it does for every session of a run.
pub async fn later_session(
    opts: Opts,
    first: &Direct,
    key: &str,
    object: Vec<u8>,
    head: &str,
    base: &str,
) -> Direct {
    let mut d2 = Direct::start(opts).await;
    d2.aws.put(
        &keys::repo_bundle_key(RUN),
        first.aws.get(&keys::repo_bundle_key(RUN)).unwrap(),
    );
    d2.aws.put(key, object);
    d2.req.turn = 2;
    d2.req.runtime_session_id = format!("{RUN}-direct-g00000002");
    d2.req.repo = RepoSource::S3Bundle {
        uri: keys::uri(BUCKET, key),
    };
    d2.req.snapshot_every_s = Some(0);
    d2.req.slice = Some(DirectSlice {
        last: true,
        base_sha: Some(base.to_string()),
        restore_head: Some(head.to_string()),
        sessions_before: Some(1),
        brief: None,
    });
    validate_direct_invocation(&d2.req).expect("the invocation of session 2 is valid");
    d2
}
