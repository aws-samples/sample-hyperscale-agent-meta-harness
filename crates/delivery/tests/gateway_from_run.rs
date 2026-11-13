//! `repo-gateway stage` with `fromRun`: a follow-up run starts from the head an earlier run delivered. The source
//! run's verified bundle (`runs/<source>/deliverable/verified.bundle`, a full-history bundle of `refs/heads/harness/<source>`) is loaded
//! the way `push-pr` loads it, `refs/heads/main` is set to the delivered head, and the usual `publish_base` writes the new run's
//! `input/repo.bundle`: the new run's base is the old head, with the full history behind it. Git runs for real on temporary repositories.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use delivery::error::DeliveryError;
use delivery::gateway::{FromRun, Gateway, GatewayRequest, StageRequest, StageTarget};
use delivery::git::Git;
use harness_protocol::store::s3;
use harness_protocol::store::s3::verified_bundle_key;
use harness_protocol::target::RunTarget;
use serde_json::json;
use std::path::PathBuf;

const AT: u64 = 1_790_337_600;
/// The source run: a different ULID from `RUN`, the new run.
const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    gw: Gateway<FakeCodeCommit, FakeAllowlist, MemBlobs, LocalRemotes>,
}

fn world() -> World {
    world_allowing(&[])
}

/// A world whose allowlist holds `allowed` (targets such as `cc:hamh-e2e-python-calc-20260925`).
fn world_allowing(allowed: &[&str]) -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let remotes_dir = root.join("codecommit");
    let work = root.join("work");
    let home = root.join("home");
    for d in [&remotes_dir, &work, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let gw = Gateway {
        codecommit: FakeCodeCommit::new(&remotes_dir),
        allowlist: FakeAllowlist::with(allowed),
        blobs: MemBlobs::default(),
        remotes: LocalRemotes::new(&remotes_dir),
        git: Git::new("git", &home),
        stage: "live".into(),
        owner: "team-a".into(),
        bucket: BUCKET.into(),
        work_root: work,
        clock: || AT,
        repo_names: Default::default(),
    };
    World {
        _tmp: tmp,
        root,
        gw,
    }
}

/// The source run delivered two commits on top of the empty base; returns (first, head).
fn deliver(w: &World) -> (String, String) {
    let (work, shas) = work_repo(
        &w.root,
        "source-work",
        &[
            &[("calc.py", "def add(a, b):\n    return a + b\n")],
            &[("notes/todo.md", "- percent_change\n")],
        ],
    );
    let f = w.root.join("verified.bundle");
    let head = verified_bundle(&work, SOURCE, &f);
    w.gw.blobs
        .insert(&verified_bundle_key(SOURCE), std::fs::read(&f).unwrap());
    (shas[0].clone(), head)
}

fn follow_up(head: &str) -> StageRequest {
    StageRequest {
        run_id: RUN.into(),
        target: StageTarget::Run(RunTarget::S3Only),
        base_ref: None,
        from_run: Some(FromRun {
            run_id: SOURCE.into(),
            head_sha: head.into(),
        }),
        container_signals: None,
    }
}

fn invalid<T: std::fmt::Debug>(r: Result<T, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Invalid(m)) => m,
        other => panic!("expected an invalid request, got {other:?}"),
    }
}

fn nothing_staged(w: &World) {
    assert!(
        w.gw.blobs.get(&s3::repo_bundle_key(RUN)).is_none(),
        "no input bundle was written"
    );
}

#[tokio::test]
async fn a_follow_up_stages_the_source_head_as_the_base_with_the_full_history() {
    let w = world();
    let (first, head) = deliver(&w);
    let out = w.gw.stage(&follow_up(&head)).await.unwrap();
    assert_eq!(
        out.base_sha, head,
        "the new run's base is the delivered head"
    );
    assert_eq!(out.base_ref, "refs/heads/main");
    assert_eq!(out.bundle_key, s3::repo_bundle_key(RUN));

    let body = w.gw.blobs.get(&out.bundle_key).unwrap();
    assert_eq!(out.size_bytes, body.len() as u64);
    assert_eq!(
        w.gw.blobs.meta(&out.bundle_key).unwrap().sha256_hex,
        out.sha256
    );
    let f = w.root.join("staged.bundle");
    std::fs::write(&f, &body).unwrap();
    let clone = w.root.join("clone");
    git(
        &w.root,
        &["clone", "-q", f.to_str().unwrap(), clone.to_str().unwrap()],
    );
    // agentd checks out HEAD: it is the delivered head, with both commits and both files, and no ref of the source run.
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&clone, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(git(&clone, &["rev-parse", "HEAD~1"]), first);
    assert_eq!(
        git(&clone, &["show", "HEAD:notes/todo.md"]),
        "- percent_change"
    );
    assert_eq!(git(&clone, &["show", "HEAD:calc.py"]).lines().count(), 2);
    let heads = git(&clone, &["bundle", "list-heads", f.to_str().unwrap()]);
    assert!(
        heads.contains(&format!("{head} refs/heads/main"))
            && heads.contains(&format!("{head} HEAD")),
        "{heads}"
    );
    assert!(
        !heads.contains("harness/"),
        "the source run's branch does not travel: {heads}"
    );
}

#[tokio::test]
async fn a_follow_up_touches_no_repository_allowlist_or_remote() {
    let w = world();
    let (_, head) = deliver(&w);
    w.gw.stage(&follow_up(&head)).await.unwrap();
    assert!(w.gw.codecommit.calls().is_empty());
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 0);
    assert!(w.gw.remotes.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_head_the_bundle_does_not_hold_is_refused_and_nothing_is_staged() {
    let w = world();
    let (first, _) = deliver(&w);
    // The bundle holds the delivered head; the request names the first commit, an older head of the same branch.
    let m = invalid(w.gw.stage(&follow_up(&first)).await);
    assert!(
        m.contains(&format!("harness/{SOURCE}")) && m.contains(&first),
        "{m}"
    );
    nothing_staged(&w);
}

#[tokio::test]
async fn a_source_with_no_verified_bundle_is_refused() {
    let w = world();
    let m = invalid(w.gw.stage(&follow_up(&"1".repeat(40))).await);
    assert!(m.contains(&verified_bundle_key(SOURCE)), "{m}");
    nothing_staged(&w);
}

#[tokio::test]
async fn a_bundle_that_does_not_verify_is_refused() {
    let w = world();
    w.gw.blobs.insert(
        &verified_bundle_key(SOURCE),
        b"# v2 git bundle\nnot really\n".to_vec(),
    );
    let m = invalid(w.gw.stage(&follow_up(&"1".repeat(40))).await);
    assert!(m.contains("does not verify"), "{m}");
    nothing_staged(&w);
}

fn repo_follow_up(head: &str, base_ref: Option<&str>) -> StageRequest {
    let mut r = follow_up(head);
    r.target = StageTarget::Repo(format!("cc:{FIXTURE}").parse().unwrap());
    r.base_ref = base_ref.map(str::to_string);
    r
}

#[tokio::test]
async fn an_s3_only_follow_up_names_no_base_ref() {
    let w = world();
    let (_, head) = deliver(&w);
    let mut based = follow_up(&head);
    based.base_ref = Some("main".into());
    let m = invalid(w.gw.stage(&based).await);
    assert!(m.contains("s3-only") && m.contains("baseRef"), "{m}");
    nothing_staged(&w);
    assert!(
        w.gw.codecommit.calls().is_empty(),
        "refused before any repository call"
    );
}

#[tokio::test]
async fn a_repository_follow_up_may_name_only_the_branch_of_the_run_it_continues() {
    // The base of the follow-up is the commit that run delivered, which `harness/<source>` named; any other base is refused.
    let w = world_allowing(&[&format!("cc:{FIXTURE}")]);
    let (_, head) = deliver(&w);
    let (repo_work, _) = work_repo(&w.root, "repo-work", &[&[("unrelated.txt", "x\n")]]);
    publish(&repo_work, &w.root.join("codecommit"), FIXTURE);
    for wrong in [
        "main",
        "harness/01K5ZQ4T8M3N6P7R9S0V1W2X5A",
        "refs/heads/main",
    ] {
        let m = invalid(w.gw.stage(&repo_follow_up(&head, Some(wrong))).await);
        assert!(
            m.contains("baseRef") && m.contains(&format!("harness/{SOURCE}")),
            "{wrong}: {m}"
        );
        nothing_staged(&w);
    }
    for right in [
        format!("harness/{SOURCE}"),
        format!("refs/heads/harness/{SOURCE}"),
    ] {
        let out =
            w.gw.stage(&repo_follow_up(&head, Some(&right)))
                .await
                .unwrap();
        assert_eq!(out.base_sha, head, "{right}");
    }
    w.gw.stage(&repo_follow_up(&head, None)).await.unwrap();
}

#[tokio::test]
async fn a_repository_follow_up_stages_the_delivered_head_whatever_the_repository_holds() {
    let w = world_allowing(&[&format!("cc:{FIXTURE}")]);
    let (first, head) = deliver(&w);
    // The repository has a `main` of its own that is not the delivered head, and no branch of the source run.
    let (work, _) = work_repo(&w.root, "repo-work", &[&[("unrelated.txt", "x\n")]]);
    publish(&work, &w.root.join("codecommit"), FIXTURE);
    let out = w.gw.stage(&repo_follow_up(&head, None)).await.unwrap();
    assert_eq!(
        out.base_sha, head,
        "the delivered head, not the repository's main"
    );
    assert_eq!(out.base_ref, "refs/heads/main");
    let f = w.root.join("staged.bundle");
    std::fs::write(&f, w.gw.blobs.get(&out.bundle_key).unwrap()).unwrap();
    let clone = w.root.join("clone");
    git(
        &w.root,
        &["clone", "-q", f.to_str().unwrap(), clone.to_str().unwrap()],
    );
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&clone, &["rev-parse", "HEAD~1"]), first);
    assert!(!clone.join("unrelated.txt").exists());
    // The target was checked (allowlist, then GetRepository); no branch was read from or written to the repository.
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 1);
    assert_eq!(
        w.gw.codecommit.calls(),
        [format!("GetRepository {FIXTURE}")]
    );
    assert!(
        w.gw.remotes.calls.lock().unwrap().is_empty(),
        "no fetch from the repository"
    );
}

#[tokio::test]
async fn a_repository_follow_up_off_the_allowlist_is_refused_before_the_bucket_is_read() {
    let w = world();
    let (_, head) = deliver(&w);
    let m = match w.gw.stage(&repo_follow_up(&head, None)).await {
        Err(DeliveryError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(m.contains("not on the allowlist"), "{m}");
    assert!(
        w.gw.blobs.reads.lock().unwrap().is_empty(),
        "no read of the bucket"
    );
    nothing_staged(&w);
}

#[tokio::test]
async fn a_follow_up_is_never_a_new_repository_or_a_repository_the_gateway_does_not_serve() {
    let w = world_allowing(&[&format!("cc:{FIXTURE}")]);
    let (_, head) = deliver(&w);
    let mut new_repo = follow_up(&head);
    new_repo.target = StageTarget::Run(RunTarget::NewRepo {
        repo: format!("cc:{FIXTURE}").parse().unwrap(),
    });
    let m = invalid(w.gw.stage(&new_repo).await);
    assert!(m.contains("new-repo") || m.contains("--new-repo"), "{m}");
    for target in [
        "gh:acme/widgets",
        "gl:acme/widgets",
        "cc:not-a-harness-repo",
    ] {
        let mut r = follow_up(&head);
        r.target = StageTarget::Repo(target.parse().unwrap());
        assert!(
            matches!(w.gw.stage(&r).await, Err(DeliveryError::Refused(_))),
            "{target}"
        );
    }
    assert!(
        w.gw.blobs.reads.lock().unwrap().is_empty(),
        "no read of the bucket"
    );
    nothing_staged(&w);
}

#[tokio::test]
async fn a_malformed_source_is_refused_before_any_object_is_read() {
    let w = world();
    for (run, head) in [
        ("not-a-ulid", "1".repeat(40)),
        (SOURCE, "short".to_string()),
        (SOURCE, "G".repeat(40)),
        (RUN, "1".repeat(40)),
    ] {
        let mut r = follow_up(&head);
        r.from_run = Some(FromRun {
            run_id: run.into(),
            head_sha: head.clone(),
        });
        invalid(w.gw.stage(&r).await);
    }
    assert!(
        w.gw.blobs.reads.lock().unwrap().is_empty(),
        "no read of the bucket"
    );
    nothing_staged(&w);
}

#[tokio::test]
async fn the_request_reads_from_run_off_the_wire_and_refuses_unknown_members() {
    let wire = |v: serde_json::Value| serde_json::from_value::<GatewayRequest>(v);
    let ok = wire(json!({
        "operation": "stage", "runId": RUN, "target": {"mode": "s3-only"},
        "fromRun": {"runId": SOURCE, "headSha": "a".repeat(40)}
    }))
    .unwrap();
    let GatewayRequest::Stage(r) = ok else {
        panic!("a stage request")
    };
    assert_eq!(
        r.from_run,
        Some(FromRun {
            run_id: SOURCE.into(),
            head_sha: "a".repeat(40)
        })
    );
    let plain =
        wire(json!({"operation": "stage", "runId": RUN, "target": {"mode": "s3-only"}})).unwrap();
    assert!(matches!(
        plain,
        GatewayRequest::Stage(StageRequest { from_run: None, .. })
    ));
    assert!(
        wire(json!({
            "operation": "stage", "runId": RUN, "target": {"mode": "s3-only"},
            "fromRun": {"runId": SOURCE, "headSha": "a".repeat(40), "branch": "x"}
        }))
        .is_err()
    );
}
