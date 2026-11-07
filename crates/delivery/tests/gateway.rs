//! `repo-gateway`:
//! `stage` bundles `baseRef` into `runs/<run>/input/repo.bundle`; `new-repo` creates
//! `hamh-e2e-new-<yyyymmdd>` with the four project tags and an empty base commit on `main`;
//! `push-pr` pushes the verified bundle's head to `refs/heads/harness/<runId>` only and opens one
//! pull request to `main` with `hamh-prcheck-required`. Every refusal comes before any repository
//! read or push. The CodeCommit fake's repositories are bare repositories, so git runs for real.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::checks::{NO_CHECK_NOTE, prcheck_rule_content};
use delivery::error::DeliveryError;
use delivery::gateway::{
    CodeCommit, EMPTY_BASE_UNIX, Gateway, GatewayRequest, NewComment, NewRepoRequest,
    PushPrRequest, StageRequest, StageTarget,
};
use delivery::git::Git;
use harness_protocol::store::s3;
use harness_protocol::store::s3::verified_bundle_key;
use harness_protocol::target::RunTarget;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const AT: u64 = 1_790_337_600; // 2026-09-25T12:00:00Z

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remotes_dir: PathBuf,
    gw: Gateway<FakeCodeCommit, FakeAllowlist, MemBlobs, LocalRemotes>,
}

fn world(allowed: &[&str]) -> World {
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
        remotes_dir,
        gw,
    }
}

/// The fixture repository published as CodeCommit would hold it: main with two commits.
fn with_fixture(w: &World) -> (PathBuf, Vec<String>) {
    let (work, shas) = work_repo(
        &w.root,
        "fixture-work",
        &[
            &[
                ("calc.py", "def add(a, b):\n    return a + b\n"),
                ("buildspec.yml", "version: 0.2\n"),
            ],
            &[(
                "tests/test_calc.py",
                "from calc import add\n\ndef test_add():\n    assert add(1, 2) == 3\n",
            )],
        ],
    );
    publish(&work, &w.remotes_dir, FIXTURE);
    (work, shas)
}

fn cc(name: &str) -> String {
    format!("cc:{name}")
}

fn stage_req(target: &str) -> StageRequest {
    StageRequest {
        run_id: RUN.into(),
        target: StageTarget::Repo(target.parse().unwrap()),
        base_ref: None,
        from_run: None,
    }
}

fn s3_only_req() -> StageRequest {
    StageRequest {
        run_id: RUN.into(),
        target: StageTarget::Run(RunTarget::S3Only),
        base_ref: None,
        from_run: None,
    }
}

fn refused<T: std::fmt::Debug>(r: Result<T, DeliveryError>) -> String {
    match r {
        Err(DeliveryError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn clone_bundle(bundle: &Path, into: &Path) -> String {
    git(
        into.parent().unwrap(),
        &[
            "clone",
            "-q",
            bundle.to_str().unwrap(),
            into.to_str().unwrap(),
        ],
    );
    git(into, &["rev-parse", "HEAD"])
}

// ---- stage --------------------------------------------------------------------------------

#[tokio::test]
async fn stage_bundles_the_default_branch_into_the_run_input() {
    let w = world(&[&cc(FIXTURE)]);
    let (_, shas) = with_fixture(&w);
    let out = w.gw.stage(&stage_req(&cc(FIXTURE))).await.unwrap();
    let key = s3::repo_bundle_key(RUN);
    assert_eq!(out.bundle_key, key);
    assert_eq!(out.bundle_uri, format!("s3://{BUCKET}/{key}"));
    assert_eq!(out.base_sha, shas[1]);
    assert_eq!(out.base_ref, "refs/heads/main");
    let body = w.gw.blobs.get(&key).unwrap();
    assert_eq!(out.size_bytes, body.len() as u64);
    let meta = w.gw.blobs.meta(&key).unwrap();
    assert_eq!(meta.sha256_hex, out.sha256);
    assert_eq!(out.sha256.len(), 64);
    // The bundle verifies with full history and clones to the base commit (agentd uses HEAD).
    let f = w.root.join("staged.bundle");
    std::fs::write(&f, &body).unwrap();
    let empty = w.root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    assert!(git_ok(
        &empty,
        &["bundle", "verify", "-q", f.to_str().unwrap()]
    ));
    assert_eq!(clone_bundle(&f, &w.root.join("clone")), shas[1]);
}

#[tokio::test]
async fn stage_reads_an_explicit_base_ref() {
    let w = world(&[&cc(FIXTURE)]);
    let (work, shas) = with_fixture(&w);
    git(&work, &["branch", "dev", &shas[0]]);
    git(
        &work,
        &[
            "push",
            "-q",
            w.remotes_dir
                .join(format!("{FIXTURE}.git"))
                .to_str()
                .unwrap(),
            "dev",
        ],
    );
    let mut req = stage_req(&cc(FIXTURE));
    req.base_ref = Some("dev".into());
    let out = w.gw.stage(&req).await.unwrap();
    assert_eq!(out.base_ref, "refs/heads/dev");
    assert_eq!(out.base_sha, shas[0]);
}

#[tokio::test]
async fn stage_of_a_missing_base_ref_is_invalid_and_writes_nothing() {
    let w = world(&[&cc(FIXTURE)]);
    with_fixture(&w);
    let mut req = stage_req(&cc(FIXTURE));
    req.base_ref = Some("nope".into());
    assert!(
        matches!(w.gw.stage(&req).await, Err(DeliveryError::Invalid(m)) if m.contains("refs/heads/nope"))
    );
    assert!(w.gw.blobs.keys().is_empty());
}

#[tokio::test]
async fn stage_outside_the_prefix_is_refused_before_any_lookup() {
    let w = world(&["cc:python-calc"]);
    let reason = refused(w.gw.stage(&stage_req("cc:python-calc")).await);
    assert_eq!(
        reason,
        "refused: cc:python-calc is outside the hamh-e2e- prefix"
    );
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 0);
    assert!(w.gw.codecommit.calls().is_empty());
    assert!(w.gw.remotes.calls.lock().unwrap().is_empty());
    assert!(w.gw.blobs.keys().is_empty());
}

#[tokio::test]
async fn stage_outside_the_allowlist_is_refused_before_any_codecommit_call() {
    let w = world(&[]);
    with_fixture(&w);
    let reason = refused(w.gw.stage(&stage_req(&cc(FIXTURE))).await);
    assert_eq!(
        reason,
        format!("refused: cc:{FIXTURE} is not on the allowlist (REPOALLOW#cc:{REGION}:{FIXTURE})")
    );
    assert!(w.gw.codecommit.calls().is_empty());
    assert!(w.gw.remotes.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stage_of_a_repository_in_another_account_or_region_is_refused_before_any_git_read() {
    for (arn, account) in [
        (
            format!("arn:aws:codecommit:{REGION}:{}:{FIXTURE}", other_account()),
            other_account(),
        ),
        (
            format!("arn:aws:codecommit:{}:{ACCOUNT}:{FIXTURE}", other_region()),
            ACCOUNT,
        ),
    ] {
        let w = world(&[&cc(FIXTURE)]);
        with_fixture(&w);
        w.gw.codecommit
            .place
            .lock()
            .unwrap()
            .insert(FIXTURE.into(), (arn.clone(), account.into()));
        let reason = refused(w.gw.stage(&stage_req(&cc(FIXTURE))).await);
        assert_eq!(
            reason,
            format!("refused: repository {arn} is outside {ACCOUNT} {REGION}")
        );
        assert_eq!(
            w.gw.codecommit.calls(),
            vec![format!("GetRepository {FIXTURE}")]
        );
        assert!(w.gw.remotes.calls.lock().unwrap().is_empty(), "no git read");
        assert!(w.gw.blobs.keys().is_empty());
    }
}

#[tokio::test]
async fn stage_of_github_and_gitlab_targets_is_refused_with_no_call() {
    let w = world(&["gh:ex-sample1/hamh-e2e-x"]);
    let gh = refused(w.gw.stage(&stage_req("gh:ex-sample1/hamh-e2e-x")).await);
    assert!(
        gh.ends_with("needs the tenant GitHub App (app mode), which is not installed"),
        "{gh}"
    );
    let gl = refused(
        w.gw.stage(&stage_req(
            "gl:testuser/hyperscale-agent-meta-harness-output",
        ))
        .await,
    );
    assert!(
        gl.contains("is a GitLab target, which is not supported"),
        "{gl}"
    );
    assert!(w.gw.codecommit.calls().is_empty());
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 0);
}

#[tokio::test]
async fn stage_of_a_missing_or_empty_repository_is_refused() {
    let w = world(&[&cc(FIXTURE), &cc("hamh-e2e-empty")]);
    let missing = refused(w.gw.stage(&stage_req(&cc(FIXTURE))).await);
    assert_eq!(
        missing,
        format!("refused: cc:{FIXTURE} does not exist in {ACCOUNT} {REGION}")
    );
    git(
        &w.remotes_dir,
        &["init", "-q", "--bare", "hamh-e2e-empty.git"],
    );
    let empty = refused(w.gw.stage(&stage_req("cc:hamh-e2e-empty")).await);
    assert_eq!(
        empty,
        "refused: cc:hamh-e2e-empty has no default branch (an empty repository); start it with --new-repo"
    );
}

#[tokio::test]
async fn an_allowlist_entry_naming_another_repository_is_refused() {
    let w = world(&[]);
    with_fixture(&w);
    w.gw.allowlist
        .mismatched
        .lock()
        .unwrap()
        .insert(cc(FIXTURE), "cc:hamh-e2e-other".into());
    let reason = refused(w.gw.stage(&stage_req(&cc(FIXTURE))).await);
    assert!(reason.ends_with("names cc:hamh-e2e-other"), "{reason}");
}

// ---- the s3-only empty base ----------------------------------------------------------------

#[tokio::test]
async fn an_s3_only_stage_writes_an_empty_base_with_no_codecommit_or_allowlist_call() {
    let w = world(&[]);
    let out = w.gw.stage(&s3_only_req()).await.unwrap();
    assert_eq!(out.base_ref, "refs/heads/main");
    assert!(w.gw.codecommit.calls().is_empty());
    assert_eq!(*w.gw.allowlist.reads.lock().unwrap(), 0);
    assert!(w.gw.remotes.calls.lock().unwrap().is_empty());
    let f = w.root.join("empty.bundle");
    std::fs::write(&f, w.gw.blobs.get(&s3::repo_bundle_key(RUN)).unwrap()).unwrap();
    let clone = w.root.join("clone");
    assert_eq!(clone_bundle(&f, &clone), out.base_sha);
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD^{tree}"]),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    );
    assert_eq!(git(&clone, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(
        git(
            &clone,
            &["show", "-s", "--format=%an <%ae> %at %cn <%ce> %ct", "HEAD"]
        ),
        format!(
            "ham-harness <ham-harness@hyperscale-agent-meta-harness.invalid> {EMPTY_BASE_UNIX} ham-harness <ham-harness@hyperscale-agent-meta-harness.invalid> {EMPTY_BASE_UNIX}"
        )
    );
    let heads = git(&clone, &["bundle", "list-heads", f.to_str().unwrap()]);
    assert!(
        heads.contains(&format!("{} refs/heads/main", out.base_sha))
            && heads.contains(&format!("{} HEAD", out.base_sha)),
        "{heads}"
    );
    assert!(
        w.gw.blobs
            .get(&format!("runs/{RUN}/input/tree.json"))
            .is_none(),
        "no tree listing is written: nothing on the direct path reads one"
    );
}

#[tokio::test]
async fn an_s3_only_stage_is_byte_identical_on_a_rerun() {
    let w = world(&[]);
    let first = w.gw.stage(&s3_only_req()).await.unwrap();
    let bundle = w.gw.blobs.get(&s3::repo_bundle_key(RUN)).unwrap();
    let second = w.gw.stage(&s3_only_req()).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(w.gw.blobs.get(&s3::repo_bundle_key(RUN)).unwrap(), bundle);
}

#[tokio::test]
async fn the_stage_target_is_a_repository_string_or_the_run_target() {
    let parse = |v: Value| serde_json::from_value::<GatewayRequest>(v);
    let s3 =
        parse(json!({"operation": "stage", "runId": RUN, "target": {"mode": "s3-only"}})).unwrap();
    assert!(matches!(
        s3,
        GatewayRequest::Stage(StageRequest {
            target: StageTarget::Run(RunTarget::S3Only),
            ..
        })
    ));
    let repo = parse(json!({"operation": "stage", "runId": RUN, "target": {"mode": "repo", "repo": format!("cc:{FIXTURE}")}})).unwrap();
    assert!(matches!(
        repo,
        GatewayRequest::Stage(StageRequest {
            target: StageTarget::Run(RunTarget::Repo { .. }),
            ..
        })
    ));
    let plain =
        parse(json!({"operation": "stage", "runId": RUN, "target": format!("cc:{FIXTURE}")}))
            .unwrap();
    assert!(matches!(
        plain,
        GatewayRequest::Stage(StageRequest {
            target: StageTarget::Repo(_),
            ..
        })
    ));
    assert!(
        parse(json!({"operation": "stage", "runId": RUN, "target": {"mode": "tarball"}})).is_err()
    );
    // A run target that names a repository stages that repository through every check.
    let w = world(&[]);
    let GatewayRequest::Stage(req) = repo else {
        unreachable!()
    };
    let reason = refused(w.gw.stage(&req).await);
    assert!(reason.contains("is not on the allowlist"), "{reason}");
    let mut s3_with_ref = s3_only_req();
    s3_with_ref.base_ref = Some("dev".into());
    assert!(matches!(
        w.gw.stage(&s3_with_ref).await,
        Err(DeliveryError::Invalid(_))
    ));
}

// ---- new-repo -----------------------------------------------------------------------------

const NEW: &str = "hamh-e2e-new-20260925";

fn new_req(target: &str) -> NewRepoRequest {
    NewRepoRequest {
        run_id: RUN.into(),
        target: target.parse().unwrap(),
    }
}

#[tokio::test]
async fn new_repo_creates_the_tagged_repository_with_an_empty_base_commit_on_main() {
    let w = world(&[&cc(NEW)]);
    let out = w.gw.new_repo(&new_req(&cc(NEW))).await.unwrap();
    assert!(out.created);
    assert_eq!(
        out.repository_arn,
        format!("arn:aws:codecommit:{REGION}:{ACCOUNT}:{NEW}")
    );
    assert_eq!(out.default_branch, "main");
    let created = w.gw.codecommit.created.lock().unwrap().clone();
    assert_eq!(created.len(), 1);
    let (name, description, tags) = &created[0];
    assert_eq!(name, NEW);
    assert!(description.contains(RUN), "{description}");
    let tags: std::collections::BTreeMap<_, _> = tags.iter().cloned().collect();
    assert_eq!(
        tags,
        [
            ("managed-by", "repo-gateway"),
            ("owner", "team-a"),
            ("project", "hyperscale-agent-meta-harness"),
            ("stage", "live")
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    );
    let bare = w.remotes_dir.join(format!("{NEW}.git"));
    let r = refs(&bare);
    assert_eq!(r.get("refs/heads/main"), Some(&out.base_sha));
    // The base commit is empty (the empty tree) and has no parent.
    assert_eq!(
        git(&bare, &["rev-parse", &format!("{}^{{tree}}", out.base_sha)]),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    );
    assert_eq!(git(&bare, &["rev-list", "--count", &out.base_sha]), "1");
    // main came from CreateBranch (the role cannot push main); the push went to the harness ref.
    let calls = w.gw.codecommit.calls();
    assert!(
        calls.contains(&format!("CreateBranch {NEW} main {}", out.base_sha)),
        "{calls:?}"
    );
    assert!(
        calls.contains(&format!("UpdateDefaultBranch {NEW} main")),
        "{calls:?}"
    );
    assert_eq!(
        r.get(&format!("refs/heads/harness/{RUN}")),
        Some(&out.base_sha)
    );
    assert_eq!(r.len(), 2, "{r:?}");
    // stage then reads main.
    let staged = w.gw.stage(&stage_req(&cc(NEW))).await.unwrap();
    assert_eq!(staged.base_sha, out.base_sha);
}

#[tokio::test]
async fn new_repo_again_returns_the_same_base_without_a_second_create() {
    let w = world(&[&cc(NEW)]);
    let first = w.gw.new_repo(&new_req(&cc(NEW))).await.unwrap();
    let second = w.gw.new_repo(&new_req(&cc(NEW))).await.unwrap();
    assert!(!second.created);
    assert_eq!(second.base_sha, first.base_sha);
    assert_eq!(w.gw.codecommit.created.lock().unwrap().len(), 1);
}

const NTIER: &str = "hamh-e2e-ntier-web-20261001";
const OTHER_RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X00";

#[tokio::test]
async fn new_repo_creates_exactly_the_named_ntier_repository_and_writes_its_allowlist_entry() {
    // `--new-repo cc:hamh-e2e-ntier-<kind>-<yyyymmdd>`; the harness allowlists what it made.
    let w = world(&[]);
    let out = w.gw.new_repo(&new_req(&cc(NTIER))).await.unwrap();
    assert!(out.created);
    assert_eq!(
        out.repository_arn,
        format!("arn:aws:codecommit:{REGION}:{ACCOUNT}:{NTIER}")
    );
    let created = w.gw.codecommit.created.lock().unwrap().clone();
    assert_eq!(
        created.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        [NTIER]
    );
    let written = w.gw.allowlist.written.lock().unwrap().clone();
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(written[0].target.to_string(), cc(NTIER));
    assert_eq!(
        written[0].added_by,
        format!("hamh-live-repo-gateway run {RUN}")
    );
    assert_eq!(written[0].added_at, "2026-09-25T12:00:00Z");
    let bare = w.remotes_dir.join(format!("{NTIER}.git"));
    let r = refs(&bare);
    assert_eq!(r.get("refs/heads/main"), Some(&out.base_sha));
    assert!(
        w.gw.codecommit
            .calls()
            .contains(&format!("CreateBranch {NTIER} main {}", out.base_sha))
    );
    // stage reads the entry the harness wrote; no operator step between.
    let staged = w.gw.stage(&stage_req(&cc(NTIER))).await.unwrap();
    assert_eq!(staged.base_sha, out.base_sha);
    // A retry of the same run writes nothing new.
    let again = w.gw.new_repo(&new_req(&cc(NTIER))).await.unwrap();
    assert!(!again.created);
    assert_eq!(again.base_sha, out.base_sha);
    assert_eq!(w.gw.allowlist.written.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn new_repo_refuses_other_names_before_any_call() {
    let w = world(&[&cc(FIXTURE)]);
    let wrong = refused(w.gw.new_repo(&new_req(&cc(FIXTURE))).await);
    assert_eq!(
        wrong,
        format!(
            "refused: cc:{FIXTURE} is not a new-repo name; use cc:hamh-e2e-new-<yyyymmdd> or cc:hamh-e2e-ntier-<kind>-<yyyymmdd>"
        )
    );
    let dated = refused(w.gw.new_repo(&new_req("cc:hamh-e2e-new-20260925-x")).await);
    assert!(dated.contains("is not a new-repo name"), "{dated}");
    let outside = refused(w.gw.new_repo(&new_req("cc:new-20260925")).await);
    assert!(
        outside.contains("outside the hamh-e2e- prefix"),
        "{outside}"
    );
    assert!(
        w.gw.codecommit.calls().is_empty(),
        "{:?}",
        w.gw.codecommit.calls()
    );
    assert!(w.gw.allowlist.written.lock().unwrap().is_empty());
}

#[tokio::test]
async fn new_repo_refuses_an_allowlist_key_that_names_another_target_before_creating() {
    let w = world(&[]);
    w.gw.allowlist
        .mismatched
        .lock()
        .unwrap()
        .insert(cc(NEW), cc(FIXTURE));
    let reason = refused(w.gw.new_repo(&new_req(&cc(NEW))).await);
    assert_eq!(
        reason,
        format!("refused: the allowlist entry REPOALLOW#cc:{REGION}:{NEW} names cc:{FIXTURE}")
    );
    assert!(w.gw.codecommit.calls().is_empty());
}

#[tokio::test]
async fn new_repo_refuses_a_repository_it_did_not_create_and_leaves_it_alone() {
    let w = world(&[]);
    let bare = w.gw.codecommit.seed(NEW, "made by hand");
    let (work, _) = work_repo(&w.root, "foreign", &[&[("README.md", "not ours\n")]]);
    git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "main:refs/heads/main"],
    );
    let before = refs(&bare);
    let reason = refused(w.gw.new_repo(&new_req(&cc(NEW))).await);
    assert_eq!(
        reason,
        format!(
            "refused: cc:{NEW} already exists and run {RUN} did not create it; name a new repository"
        )
    );
    assert_eq!(refs(&bare), before, "nothing pushed or branched");
    assert!(w.gw.allowlist.written.lock().unwrap().is_empty());
    assert!(
        !w.gw
            .codecommit
            .calls()
            .iter()
            .any(|c| c.starts_with("CreateBranch") || c.starts_with("UpdateDefaultBranch")),
        "{:?}",
        w.gw.codecommit.calls()
    );
}

#[tokio::test]
async fn new_repo_refuses_the_name_another_run_created() {
    let w = world(&[]);
    w.gw.new_repo(&new_req(&cc(NTIER))).await.unwrap();
    let mut other = new_req(&cc(NTIER));
    other.run_id = OTHER_RUN.into();
    let reason = refused(w.gw.new_repo(&other).await);
    assert_eq!(
        reason,
        format!(
            "refused: cc:{NTIER} already exists and run {OTHER_RUN} did not create it; name a new repository"
        )
    );
    let r = refs(&w.remotes_dir.join(format!("{NTIER}.git")));
    assert!(
        !r.contains_key(&format!("refs/heads/harness/{OTHER_RUN}")),
        "{r:?}"
    );
}

#[tokio::test]
async fn new_repo_refuses_a_main_it_did_not_write_even_under_its_own_description() {
    let w = world(&[]);
    let bare =
        w.gw.codecommit
            .seed(NEW, &format!("ham --new-repo for run {RUN}"));
    let (work, shas) = work_repo(&w.root, "foreign", &[&[("app.py", "print(1)\n")]]);
    git(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "main:refs/heads/main"],
    );
    let reason = refused(w.gw.new_repo(&new_req(&cc(NEW))).await);
    assert_eq!(
        reason,
        format!(
            "refused: cc:{NEW} main is {}, which is not the empty base of run {RUN}",
            shas[0]
        )
    );
    assert!(w.gw.allowlist.written.lock().unwrap().is_empty());
}

#[tokio::test]
async fn new_repo_finishes_a_repository_its_own_earlier_attempt_created() {
    // A crash after CreateRepository: the retry finds the repository with this run's description
    // and no main, and completes it.
    let w = world(&[]);
    w.gw.codecommit
        .seed(NEW, &format!("ham --new-repo for run {RUN}"));
    let out = w.gw.new_repo(&new_req(&cc(NEW))).await.unwrap();
    assert!(!out.created);
    let r = refs(&w.remotes_dir.join(format!("{NEW}.git")));
    assert_eq!(r.get("refs/heads/main"), Some(&out.base_sha));
    assert_eq!(w.gw.allowlist.written.lock().unwrap().len(), 1);
}

// ---- push-pr ------------------------------------------------------------------------------

fn push_req(head: &str, title: &str) -> PushPrRequest {
    PushPrRequest {
        run_id: RUN.into(),
        target: cc(FIXTURE).parse().unwrap(),
        head_sha: head.into(),
        title: title.into(),
        description:
            "Objective: add percent_change.\nR1 R2\nG0 G1 G2 G2b G3 G4\nEVIDENCE.md: s3://x".into(),
        comments: vec![
            "| R-ID | status |\n| R1 | proven |".into(),
            "| gate | exit |\n| G0 | 0 |".into(),
        ],
        source_ref: None,
        follow_up: None,
    }
}

/// The fixture plus one agent commit on harness/<RUN>, bundled where FinalGates stages it.
fn with_verified(w: &World) -> (String, String) {
    let (work, shas) = with_fixture(w);
    let head = commit(
        &work,
        &[(
            "calc.py",
            Some(
                "def add(a, b):\n    return a + b\n\ndef percent_change(a, b):\n    return (b - a) / a * 100\n",
            ),
        )],
        "percent_change",
    );
    let f = w.root.join("verified.bundle");
    verified_bundle(&work, RUN, &f);
    w.gw.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    (shas[1].clone(), head)
}

#[tokio::test]
async fn push_pr_pushes_only_the_harness_ref_and_opens_one_pr_with_the_check_rule() {
    let w = world(&[&cc(FIXTURE)]);
    let (base, head) = with_verified(&w);
    let out =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    let bare = w.remotes_dir.join(format!("{FIXTURE}.git"));
    let r = refs(&bare);
    assert_eq!(r.get(&format!("refs/heads/harness/{RUN}")), Some(&head));
    assert_eq!(r.get("refs/heads/main"), Some(&base), "main unchanged");
    assert_eq!(r.len(), 2, "no other ref: {r:?}");
    let prs = w.gw.codecommit.prs.lock().unwrap().clone();
    assert_eq!(prs.len(), 1);
    let (pr, info) = &prs[0];
    assert_eq!(pr.repository, FIXTURE);
    assert_eq!(pr.source_ref, format!("refs/heads/harness/{RUN}"));
    assert_eq!(pr.destination_ref, "refs/heads/main");
    assert_eq!(pr.title, "Add percent_change");
    assert!(pr.description.contains("G2b"));
    assert_eq!(pr.client_request_token, format!("hamh-pr-{RUN}"));
    assert_eq!(info.source_commit, head);
    let rules = w.gw.codecommit.rules.lock().unwrap().clone();
    assert_eq!(rules.len(), 1);
    let content: Value =
        serde_json::from_str(&rules[&("1".to_string(), "hamh-prcheck-required".to_string())])
            .unwrap();
    assert_eq!(
        content["Statements"][0]["ApprovalPoolMembers"],
        json!([format!(
            "arn:aws:sts::{ACCOUNT}:assumed-role/hamh-live-prcheck-build/*"
        )])
    );
    let comments = w.gw.codecommit.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0].before_commit_id, base);
    assert_eq!(comments[0].after_commit_id, head);
    assert!(comments[1].content.contains("G0"));
    assert_eq!(out.pull_request_id, "1");
    assert_eq!(
        out.url,
        format!(
            "https://{REGION}.console.aws.amazon.com/codesuite/codecommit/repositories/{FIXTURE}/pull-requests/1/details"
        )
    );
    assert_eq!(out.source_ref, format!("refs/heads/harness/{RUN}"));
    assert_eq!(out.head_sha, head);
    assert_eq!(
        out.approval_rules,
        vec!["hamh-prcheck-required".to_string()]
    );
    assert_eq!(out.comments, 2);
}

#[tokio::test]
async fn push_pr_again_is_idempotent() {
    let w = world(&[&cc(FIXTURE)]);
    let (_, head) = with_verified(&w);
    let first =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    let second =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    assert_eq!(second.pull_request_id, first.pull_request_id);
    assert_eq!(w.gw.codecommit.prs.lock().unwrap().len(), 1);
    assert_eq!(w.gw.codecommit.rules.lock().unwrap().len(), 1);
    assert_eq!(w.gw.codecommit.comments.lock().unwrap().len(), 2);
}

/// The fixture with no agent commit: the verified bundle's head is `main`.
fn with_verified_base(w: &World) -> String {
    let (work, shas) = with_fixture(w);
    let f = w.root.join("verified.bundle");
    verified_bundle(&work, RUN, &f);
    w.gw.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    shas[1].clone()
}

#[tokio::test]
async fn the_fake_codecommit_refuses_a_comment_whose_commits_are_equal_as_codecommit_does() {
    let w = world(&[&cc(FIXTURE)]);
    let e =
        w.gw.codecommit
            .post_comment(&NewComment {
                repository: FIXTURE.into(),
                pull_request_id: "1".into(),
                before_commit_id: "a".repeat(40),
                after_commit_id: "a".repeat(40),
                content: "| R-ID |".into(),
                client_request_token: format!("hamh-pr-{RUN}-c0"),
            })
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DeliveryError::Invalid(m) if m == EQUAL_COMMITS_REFUSAL && m.contains("BeforeCommitIdAndAfterCommitIdAreSameException")),
        "{e}"
    );
    assert!(w.gw.codecommit.comments.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pr_with_no_commits_is_opened_without_table_comments() {
    // CodeCommit opens a pull request whose source and destination commits are equal, but refuses
    // every comment on it; the push succeeds with no comment call.
    let w = world(&[&cc(FIXTURE)]);
    let head = with_verified_base(&w);
    let req = push_req(&head, "ham: Add percent_change");
    let out = w.gw.push_pr(&req).await.unwrap();
    let prs = w.gw.codecommit.prs.lock().unwrap().clone();
    assert_eq!(prs.len(), 1);
    assert_eq!(
        (
            prs[0].1.source_commit.as_str(),
            prs[0].1.destination_commit.as_str()
        ),
        (head.as_str(), head.as_str())
    );
    assert_eq!(out.approval_rules, ["hamh-prcheck-required"]);
    assert_eq!(out.head_sha, head);
    assert_eq!(
        out.comments, 0,
        "no comment is posted on a PR with no commits"
    );
    assert!(w.gw.codecommit.comments.lock().unwrap().is_empty());
    assert!(
        !w.gw
            .codecommit
            .calls()
            .iter()
            .any(|c| c.starts_with("PostCommentForPullRequest")),
        "{:?}",
        w.gw.codecommit.calls()
    );
    // A retry (the machine's, or a replay) gets the same PR and still posts nothing.
    let again = w.gw.push_pr(&req).await.unwrap();
    assert_eq!(again.pull_request_id, out.pull_request_id);
    assert_eq!(again.comments, 0);
    assert_eq!(w.gw.codecommit.prs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_existing_rule_with_other_content_fails_the_push_pr() {
    let w = world(&[&cc(FIXTURE)]);
    let (_, head) = with_verified(&w);
    w.gw.push_pr(&push_req(&head, "Add percent_change"))
        .await
        .unwrap();
    w.gw.codecommit.rules.lock().unwrap().insert(
        ("1".into(), "hamh-prcheck-required".into()),
        r#"{"Version":"2018-11-08","Statements":[{"Type":"Approvers","NumberOfApprovalsNeeded":1,"ApprovalPoolMembers":["arn:aws:sts::800000000022:assumed-role/Admin/*"]}]}"#.into(),
    );
    let err =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap_err();
    assert!(err.to_string().contains("hamh-prcheck-required"), "{err}");
}

// ---- push-pr: the pull-request check needs a root buildspec.yml -----------------------

/// What the root of the fixture repository holds for the build file the PR check runs.
#[derive(Clone, Copy, Debug)]
enum Spec {
    Missing,
    Regular,
    Executable,
    Link,
    Folder,
    Nested,
    OtherCase,
    Yaml,
}

/// The fixture on `main` with `spec` as its build file, published as CodeCommit would hold it, and (with `agent_commit`) one agent commit
/// on harness/<RUN>, bundled where FinalGates stages it. Returns (base, head); without an agent commit the two are equal.
fn with_verified_spec(w: &World, spec: Spec, agent_commit: bool) -> (String, String) {
    let (work, _) = work_repo(
        &w.root,
        "fixture-work",
        &[
            &[("calc.py", "def add(a, b):\n    return a + b\n")],
            &[("tests/test_calc.py", "from calc import add\n")],
        ],
    );
    let body = "version: 0.2\n";
    match spec {
        Spec::Missing => {}
        Spec::Regular => {
            commit(&work, &[("buildspec.yml", Some(body))], "build file");
        }
        Spec::Executable => {
            commit(&work, &[("buildspec.yml", Some(body))], "build file");
            std::fs::set_permissions(
                work.join("buildspec.yml"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            commit(&work, &[], "make the build file executable");
        }
        Spec::Link => {
            commit(&work, &[("ci/spec.yml", Some(body))], "spec");
            std::os::unix::fs::symlink("ci/spec.yml", work.join("buildspec.yml")).unwrap();
            commit(&work, &[], "link the build file");
        }
        Spec::Folder => {
            commit(&work, &[("buildspec.yml/spec.yml", Some(body))], "folder");
        }
        Spec::Nested => {
            commit(&work, &[("ci/buildspec.yml", Some(body))], "nested");
        }
        Spec::OtherCase => {
            commit(&work, &[("Buildspec.yml", Some(body))], "other case");
        }
        Spec::Yaml => {
            commit(&work, &[("buildspec.yaml", Some(body))], "yaml spelling");
        }
    }
    let base = git(&work, &["rev-parse", "HEAD"]);
    publish(&work, &w.remotes_dir, FIXTURE);
    let head = if agent_commit {
        commit(
            &work,
            &[(
                "calc.py",
                Some(
                    "def add(a, b):\n    return a + b\n\ndef percent_change(a, b):\n    return (b - a) / a * 100\n",
                ),
            )],
            "percent_change",
        )
    } else {
        base.clone()
    };
    let f = w.root.join("verified.bundle");
    verified_bundle(&work, RUN, &f);
    w.gw.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    (base, head)
}

fn approval_calls(w: &World) -> Vec<String> {
    w.gw.codecommit
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("CreatePullRequestApprovalRule"))
        .collect()
}

#[tokio::test]
async fn a_head_with_no_root_build_file_gets_no_check_rule_and_one_note() {
    let w = world(&[&cc(FIXTURE)]);
    let (base, head) = with_verified_spec(&w, Spec::Missing, true);
    let out =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    // The pull request is opened as always.
    assert_eq!(w.gw.codecommit.prs.lock().unwrap().len(), 1);
    // No rule is attached, and the output says so.
    assert!(w.gw.codecommit.rules.lock().unwrap().is_empty());
    assert!(out.approval_rules.is_empty(), "{:?}", out.approval_rules);
    assert_eq!(approval_calls(&w), Vec::<String>::new());
    // The two table comments keep their order and tokens; the note follows them, between the same two commits.
    let comments = w.gw.codecommit.comments.lock().unwrap().clone();
    let tokens: Vec<&str> = comments
        .iter()
        .map(|c| c.client_request_token.as_str())
        .collect();
    assert_eq!(
        tokens,
        [
            format!("hamh-pr-{RUN}-c0"),
            format!("hamh-pr-{RUN}-c1"),
            format!("hamh-pr-{RUN}-nocheck")
        ]
    );
    assert!(comments[0].content.contains("R-ID"));
    assert!(comments[1].content.contains("G0"));
    assert_eq!(comments[2].content, NO_CHECK_NOTE);
    assert_eq!(comments[2].before_commit_id, base);
    assert_eq!(comments[2].after_commit_id, head);
    assert_eq!(out.comments, 3, "the note counts as a posted comment");
}

#[tokio::test]
async fn a_head_with_no_root_build_file_is_delivered_again_without_a_second_note_or_a_rule() {
    let w = world(&[&cc(FIXTURE)]);
    let (_, head) = with_verified_spec(&w, Spec::Missing, true);
    let first =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    let second =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    assert_eq!(second.pull_request_id, first.pull_request_id);
    assert_eq!(w.gw.codecommit.prs.lock().unwrap().len(), 1);
    assert!(w.gw.codecommit.rules.lock().unwrap().is_empty());
    assert_eq!(w.gw.codecommit.comments.lock().unwrap().len(), 3);
    assert!(second.approval_rules.is_empty());
}

#[tokio::test]
async fn only_a_regular_root_buildspec_yml_attaches_the_check() {
    // The head's root tree decides: a regular file (executable or not) attaches the rule as before; a missing file, a link, a folder of
    // that name, a nested file and another spelling attach nothing and get the note.
    for (spec, attached) in [
        (Spec::Regular, true),
        (Spec::Executable, true),
        (Spec::Missing, false),
        (Spec::Link, false),
        (Spec::Folder, false),
        (Spec::Nested, false),
        (Spec::OtherCase, false),
        (Spec::Yaml, false),
    ] {
        let w = world(&[&cc(FIXTURE)]);
        let (_, head) = with_verified_spec(&w, spec, true);
        let out =
            w.gw.push_pr(&push_req(&head, "Add percent_change"))
                .await
                .unwrap();
        let rules = w.gw.codecommit.rules.lock().unwrap().len();
        assert_eq!(rules, usize::from(attached), "{spec:?}: rules");
        assert_eq!(
            out.approval_rules.len(),
            usize::from(attached),
            "{spec:?}: output"
        );
        let comments = w.gw.codecommit.comments.lock().unwrap().clone();
        assert_eq!(
            comments.len(),
            if attached { 2 } else { 3 },
            "{spec:?}: comments"
        );
        assert_eq!(
            comments
                .iter()
                .any(|c| c.client_request_token.ends_with("-nocheck")),
            !attached,
            "{spec:?}: note"
        );
        assert_eq!(out.comments, comments.len(), "{spec:?}: count");
    }
}

#[tokio::test]
async fn a_pr_with_no_commits_and_no_build_file_gets_neither_a_rule_nor_a_comment() {
    // CodeCommit takes no comment when the two commits are equal, so the note is skipped like the table comments.
    let w = world(&[&cc(FIXTURE)]);
    let (_, head) = with_verified_spec(&w, Spec::Missing, false);
    let out =
        w.gw.push_pr(&push_req(&head, "ham: Add percent_change"))
            .await
            .unwrap();
    assert_eq!(w.gw.codecommit.prs.lock().unwrap().len(), 1);
    assert!(out.approval_rules.is_empty());
    assert_eq!(out.comments, 0);
    assert!(w.gw.codecommit.rules.lock().unwrap().is_empty());
    assert!(
        !w.gw
            .codecommit
            .calls()
            .iter()
            .any(|c| c.starts_with("PostCommentForPullRequest")),
        "{:?}",
        w.gw.codecommit.calls()
    );
}

#[tokio::test]
async fn a_check_rule_that_an_earlier_delivery_attached_stays_and_is_reported() {
    // A retry by this gateway of a pull request whose rule an older gateway attached: the rule is never taken away silently, and the
    // output does not claim the pull request is unchecked while the rule stands.
    let w = world(&[&cc(FIXTURE)]);
    let (_, head) = with_verified_spec(&w, Spec::Missing, true);
    w.gw.push_pr(&push_req(&head, "Add percent_change"))
        .await
        .unwrap();
    w.gw.codecommit.rules.lock().unwrap().insert(
        ("1".into(), "hamh-prcheck-required".into()),
        prcheck_rule_content("live"),
    );
    let again =
        w.gw.push_pr(&push_req(&head, "Add percent_change"))
            .await
            .unwrap();
    assert_eq!(again.approval_rules, ["hamh-prcheck-required"]);
    let rules = w.gw.codecommit.rules.lock().unwrap().clone();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules[&("1".to_string(), "hamh-prcheck-required".to_string())],
        prcheck_rule_content("live")
    );
}

#[tokio::test]
async fn a_push_to_any_ref_but_the_harness_branch_is_refused_before_any_read() {
    let w = world(&[&cc(FIXTURE)]);
    let (base, head) = with_verified(&w);
    for bad in [
        "refs/heads/main",
        "main",
        "refs/heads/harness/01K5ZQ4T8M3N6P7R9S0V1W2X00",
        "refs/tags/v1",
    ] {
        let mut req = push_req(&head, "Add percent_change");
        req.source_ref = Some(bad.into());
        let reason = refused(w.gw.push_pr(&req).await);
        assert_eq!(
            reason,
            format!("refused: push to {bad}; repo-gateway pushes only refs/heads/harness/{RUN}")
        );
    }
    assert!(w.gw.codecommit.calls().is_empty());
    assert!(w.gw.blobs.reads.lock().unwrap().is_empty());
    assert!(w.gw.remotes.calls.lock().unwrap().is_empty());
    let r = refs(&w.remotes_dir.join(format!("{FIXTURE}.git")));
    assert_eq!(r.get("refs/heads/main"), Some(&base));
    assert_eq!(r.len(), 1);
}

#[tokio::test]
async fn push_pr_outside_the_allowlist_is_refused_before_the_bundle_is_read() {
    let w = world(&[]);
    let (_, head) = with_verified(&w);
    let reason = refused(w.gw.push_pr(&push_req(&head, "Add percent_change")).await);
    assert!(reason.contains("is not on the allowlist"), "{reason}");
    assert!(w.gw.blobs.reads.lock().unwrap().is_empty());
    assert!(w.gw.codecommit.calls().is_empty());
}

#[tokio::test]
async fn a_bundle_whose_harness_branch_is_not_the_head_is_invalid_and_nothing_is_pushed() {
    let w = world(&[&cc(FIXTURE)]);
    let (base, _head) = with_verified(&w);
    let other = "b".repeat(40);
    let err =
        w.gw.push_pr(&push_req(&other, "Add percent_change"))
            .await
            .unwrap_err();
    assert!(
        matches!(&err, DeliveryError::Invalid(m) if m.contains(&other)),
        "{err}"
    );
    let r = refs(&w.remotes_dir.join(format!("{FIXTURE}.git")));
    assert_eq!(r.len(), 1);
    assert_eq!(r.get("refs/heads/main"), Some(&base));
    assert!(w.gw.codecommit.prs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_or_corrupt_verified_bundle_is_invalid() {
    let w = world(&[&cc(FIXTURE)]);
    with_fixture(&w);
    let head = "a".repeat(40);
    let missing = w.gw.push_pr(&push_req(&head, "t")).await.unwrap_err();
    assert!(
        matches!(&missing, DeliveryError::Invalid(m) if m.contains(&verified_bundle_key(RUN))),
        "{missing}"
    );
    w.gw.blobs.insert(
        &verified_bundle_key(RUN),
        b"# v2 git bundle\nnot really\n".to_vec(),
    );
    let corrupt = w.gw.push_pr(&push_req(&head, "t")).await.unwrap_err();
    assert!(
        matches!(&corrupt, DeliveryError::Invalid(m) if m.contains("does not verify")),
        "{corrupt}"
    );
}

#[tokio::test]
async fn push_pr_text_over_the_codecommit_limits_is_invalid_before_any_call() {
    let w = world(&[&cc(FIXTURE)]);
    let (_, head) = with_verified(&w);
    let long = push_req(&head, &"t".repeat(151));
    assert!(matches!(
        w.gw.push_pr(&long).await,
        Err(DeliveryError::Invalid(_))
    ));
    let mut desc = push_req(&head, "t");
    desc.description = "d".repeat(10_241);
    assert!(matches!(
        w.gw.push_pr(&desc).await,
        Err(DeliveryError::Invalid(_))
    ));
    assert!(w.gw.codecommit.calls().is_empty());
}

// ---- request parsing ----------------------------------------------------------------------

#[tokio::test]
async fn the_state_machine_input_names_the_operation() {
    let stage: GatewayRequest = serde_json::from_value(json!({
        "operation": "stage", "runId": RUN, "target": format!("cc:{FIXTURE}"), "baseRef": "main"
    }))
    .unwrap();
    assert!(matches!(stage, GatewayRequest::Stage(ref s) if s.base_ref.as_deref() == Some("main")));
    let new: GatewayRequest = serde_json::from_value(json!({
        "operation": "new-repo", "runId": RUN, "target": format!("cc:{NEW}")
    }))
    .unwrap();
    assert!(matches!(new, GatewayRequest::NewRepo(_)));
    let push: GatewayRequest = serde_json::from_value(json!({
        "operation": "push-pr", "runId": RUN, "target": format!("cc:{FIXTURE}"), "headSha": "a".repeat(40),
        "title": "t", "description": "d"
    }))
    .unwrap();
    assert!(
        matches!(push, GatewayRequest::PushPr(ref p) if p.comments.is_empty() && p.source_ref.is_none())
    );
    for bad in [
        json!({"operation": "merge", "runId": RUN}),
        json!({"operation": "stage", "runId": RUN, "target": "s3-only"}),
        json!({"runId": RUN, "target": format!("cc:{FIXTURE}")}),
    ] {
        assert!(
            serde_json::from_value::<GatewayRequest>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    // handle() dispatches and returns the operation's JSON output.
    let w = world(&[&cc(FIXTURE)]);
    with_fixture(&w);
    let out = w.gw.handle(stage).await.unwrap();
    assert_eq!(out["bundleKey"], s3::repo_bundle_key(RUN));
    assert_eq!(out["baseRef"], "refs/heads/main");
}
