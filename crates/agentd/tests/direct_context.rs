//! The context directory of a fresh session: the original prompt and the digest of the
//! earlier session, written where the new Cline session can read them and nothing the harness delivers can contain them.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::context::{
    AUDIT_UPLOAD_RETRIES, ContextDir, TASK_FILE, audit_uri, sha256_hex, upload_audit_copy,
};
use agentd::direct::snapshot::Snapshotter;
use agentd::s3::S3Objects;
use agentd::time::{format_rfc3339, now_unix};
use common::{creds, git, python_calc, scratch};
use direct_support::{BUCKET, FakeAws, RUN};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

/// The digest budget (`DIGEST_BUDGET_BYTES`): 200,000 bytes besides the task.
const DIGEST_BUDGET: usize = harness_protocol::direct::DIGEST_BUDGET_BYTES;

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn the_directory_is_made_under_the_agent_data_with_mode_0755_and_the_owner_of_agentd() {
    let data = scratch("ctx-create");
    let ctx = ContextDir::create(&data).unwrap();
    assert_eq!(ctx.path(), data.join("context"));
    assert!(ctx.path().is_dir());
    assert_eq!(mode(ctx.path()), 0o755);
    assert_eq!(
        std::fs::metadata(ctx.path()).unwrap().uid(),
        std::fs::metadata(&data).unwrap().uid(),
        "created by the process that made the agent data, never by the engine's user"
    );
    // A second call reuses it, and puts the mode back if something loosened it.
    std::fs::set_permissions(ctx.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let again = ContextDir::create(&data).unwrap();
    assert_eq!(again.path(), ctx.path());
    assert_eq!(mode(again.path()), 0o755);
}

#[test]
fn a_context_path_that_is_a_link_or_a_file_is_refused() {
    let data = scratch("ctx-refuse");
    let elsewhere = data.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data.join("context")).unwrap();
    assert!(ContextDir::create(&data).is_err(), "a link");
    std::fs::remove_file(data.join("context")).unwrap();
    std::fs::write(data.join("context"), "x").unwrap();
    assert!(ContextDir::create(&data).is_err(), "a file");
}

#[test]
fn the_task_file_holds_the_prompt_byte_for_byte_with_mode_0644() {
    let data = scratch("ctx-task");
    let ctx = ContextDir::create(&data).unwrap();
    let prompt = "Add a percent_change(a, b)  \r\n\twith tests (\u{e9} \u{1F600})\n\n  last line without a newline";
    let path = ctx.write_task(prompt).unwrap();
    assert_eq!(path, data.join("context").join(TASK_FILE));
    assert_eq!(std::fs::read(&path).unwrap(), prompt.as_bytes());
    assert_eq!(mode(&path), 0o644);
}

#[test]
fn the_digest_file_is_written_with_its_size_and_hash_and_mode_0644() {
    let data = scratch("ctx-digest");
    let ctx = ContextDir::create(&data).unwrap();
    // SHA-256 of "abc" (FIPS 180-2 test vector).
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let f = ctx.write_digest(2, "abc").unwrap();
    assert_eq!(f.path, data.join("context/fresh-session-2.md"));
    assert_eq!(f.path, ctx.digest_path(2));
    assert_eq!(f.bytes, 3);
    assert_eq!(f.sha256, sha256_hex(b"abc"));
    assert_eq!(std::fs::read_to_string(&f.path).unwrap(), "abc");
    assert_eq!(mode(&f.path), 0o644);
}

#[test]
fn nothing_in_the_directory_can_be_written_by_the_group_or_others() {
    let data = scratch("ctx-perm");
    let ctx = ContextDir::create(&data).unwrap();
    ctx.write_task("task").unwrap();
    ctx.write_digest(2, "digest").unwrap();
    assert_eq!(mode(ctx.path()) & 0o022, 0);
    for n in names(ctx.path()) {
        assert_eq!(mode(&ctx.path().join(&n)) & 0o022, 0, "{n}");
    }
}

#[test]
fn a_digest_over_the_budget_is_refused_and_one_at_the_budget_is_written() {
    let data = scratch("ctx-budget");
    let ctx = ContextDir::create(&data).unwrap();
    assert!(ctx.write_digest(2, &"d".repeat(DIGEST_BUDGET)).is_ok());
    let err = ctx
        .write_digest(3, &"d".repeat(DIGEST_BUDGET + 1))
        .unwrap_err();
    assert!(err.contains("200000") || err.contains("200,000"), "{err}");
    assert!(!ctx.digest_path(3).exists(), "nothing was written");
}

#[test]
fn writing_a_digest_again_replaces_it_and_leaves_no_temporary_file() {
    let data = scratch("ctx-replace");
    let ctx = ContextDir::create(&data).unwrap();
    ctx.write_digest(2, "first").unwrap();
    ctx.write_digest(2, "second").unwrap();
    ctx.write_digest(3, "third").unwrap();
    assert_eq!(
        std::fs::read_to_string(ctx.digest_path(2)).unwrap(),
        "second"
    );
    assert_eq!(
        names(ctx.path()),
        vec!["fresh-session-2.md", "fresh-session-3.md"]
    );
}

#[tokio::test]
async fn nothing_in_the_directory_reaches_a_snapshot_or_the_work_tree() {
    let dir = scratch("ctx-outside");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let ctx = ContextDir::create(&data).unwrap();
    ctx.write_task("the original prompt").unwrap();
    ctx.write_digest(2, "the digest").unwrap();
    assert!(ctx.task_path().is_file() && ctx.digest_path(2).is_file());
    assert!(!ctx.path().starts_with(&repo), "outside the work tree");
    let mut snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    assert_eq!(
        snap.snapshot().await.unwrap(),
        None,
        "the tree is unchanged"
    );
    assert_eq!(
        git(&repo, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

#[test]
fn the_audit_copy_goes_under_the_agents_direct_prefix_of_the_run() {
    assert_eq!(
        audit_uri("bkt", "01M4XTESTRUNDIRECT00000001", 2),
        "s3://bkt/runs/01M4XTESTRUNDIRECT00000001/agents/direct/turns/1/fresh-session-2.md"
    );
}

#[tokio::test]
async fn the_audit_copy_is_uploaded_and_a_failed_put_is_tried_again() {
    let aws = FakeAws::start().await;
    let s3 = S3Objects::new("us-west-2".into(), Some(aws.base.clone()));
    let c = creds(&format_rfc3339(now_unix() + 3600));
    let data = scratch("ctx-upload");
    let ctx = ContextDir::create(&data).unwrap();
    let f = ctx
        .write_digest(2, "the digest of the earlier session")
        .unwrap();
    let uri = audit_uri(BUCKET, RUN, 2);
    let key = format!("runs/{RUN}/agents/direct/turns/1/fresh-session-2.md");
    aws.fail_puts("fresh-session-2", 2);
    upload_audit_copy(&s3, &uri, &c, &f.path, Duration::from_millis(1))
        .await
        .unwrap();
    assert_eq!(
        aws.get(&key).unwrap(),
        b"the digest of the earlier session".to_vec()
    );
    assert_eq!(
        aws.put_attempts(&key),
        3,
        "two failures, then the put that worked"
    );
}

#[tokio::test]
async fn the_audit_upload_gives_up_after_its_retries_and_names_the_s3_error() {
    let aws = FakeAws::start().await;
    let s3 = S3Objects::new("us-west-2".into(), Some(aws.base.clone()));
    let c = creds(&format_rfc3339(now_unix() + 3600));
    let data = scratch("ctx-upload-fail");
    let ctx = ContextDir::create(&data).unwrap();
    let f = ctx.write_digest(2, "digest").unwrap();
    let uri = audit_uri(BUCKET, RUN, 2);
    let key = format!("runs/{RUN}/agents/direct/turns/1/fresh-session-2.md");
    aws.fail_puts("fresh-session-2", u32::MAX);
    let err = upload_audit_copy(&s3, &uri, &c, &f.path, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(
        err.contains("500") && err.contains("InternalError"),
        "{err}"
    );
    assert_eq!(aws.put_attempts(&key), 1 + AUDIT_UPLOAD_RETRIES);
    assert!(aws.get(&key).is_none());
}

#[tokio::test]
async fn a_missing_file_is_an_upload_error_not_a_panic() {
    let aws = FakeAws::start().await;
    let s3 = S3Objects::new("us-west-2".into(), Some(aws.base.clone()));
    let c = creds(&format_rfc3339(now_unix() + 3600));
    let data = scratch("ctx-upload-missing");
    let err = upload_audit_copy(
        &s3,
        &audit_uri(BUCKET, RUN, 2),
        &c,
        &data.join("context/none.md"),
        Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(err.contains("none.md"), "{err}");
}
