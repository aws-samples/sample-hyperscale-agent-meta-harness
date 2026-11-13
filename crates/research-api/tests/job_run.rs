//! The `research-job` runner: one queued job in, one stored repository item and a finished job out. The archive source and the filter are
//! stand-ins here; the filter's own rules are tested in the archive module.

mod support;

use harness_protocol::research::{
    DroppedEntry, ItemKind, ItemManifest, JobState, LicenceEvidence, ManifestFile,
    ResearchErrorCode, ResearchLimits,
};
use research_api::archive::Filtered;
use research_api::fakes::{FixedArchive, FixedClock, MemBlobs, MemTable};
use research_api::jobs::{JobDeps, JobEnd, run_job};
use research_api::ports::{Fail, JobEvent, JobRecord, Table};
use support::{NOW_MS, RUN, policy};

const COMMIT: &str = "666df7c10035f7e26f27ec214fe5ae3173435f34";
const REPO: &str = "https://github.com/sindresorhus/is-plain-obj";

fn canned(_gz: &[u8], _paths: &[String], _limits: &ResearchLimits) -> Result<Filtered, Fail> {
    Ok(Filtered {
        commit: COMMIT.into(),
        top_dir: format!("is-plain-obj-{COMMIT}"),
        tar_gz: b"filtered tar.gz bytes".to_vec(),
        files: vec![ManifestFile {
            path: "index.js".into(),
            sha256: "ab".repeat(32),
            bytes: 312,
        }],
        dropped: vec![DroppedEntry {
            path: ".github/".into(),
            reason: "excluded-dir".into(),
        }],
        licence: LicenceEvidence {
            spdx: "MIT".into(),
            source: "file:license".into(),
            evidence_sha256: Some("cd".repeat(32)),
        },
        read_bytes: 4000,
        unpacked_bytes: 312,
    })
}

/// A result whose kept files fit the lowered limits of a test but whose archive does not fit the storage budget.
fn canned_small(gz: &[u8], paths: &[String], limits: &ResearchLimits) -> Result<Filtered, Fail> {
    let mut f = canned(gz, paths, limits)?;
    f.unpacked_bytes = 5;
    Ok(f)
}

fn too_big(_gz: &[u8], _paths: &[String], _limits: &ResearchLimits) -> Result<Filtered, Fail> {
    Err(Fail::new(
        ResearchErrorCode::TooLarge,
        "the archive expands past the read cap",
    ))
}

fn deps(
    source: FixedArchive,
    filter: research_api::jobs::FilterFn,
) -> JobDeps<MemTable, MemBlobs, FixedArchive, FixedClock> {
    let d = JobDeps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        source,
        clock: FixedClock::at_ms(NOW_MS),
        filter,
    };
    d.table.put_policy(policy(RUN));
    d
}

fn queued(git_ref: Option<&str>) -> JobRecord {
    JobRecord {
        run: RUN.into(),
        agent: "direct".into(),
        job_id: "j-0123456789abcdef".into(),
        req_hash: "00".repeat(32),
        state: JobState::Queued,
        created_s: NOW_MS / 1000,
        updated_s: NOW_MS / 1000,
        url: REPO.into(),
        git_ref: git_ref.map(str::to_string),
        paths: vec![],
        item: None,
        error: None,
    }
}

fn event() -> JobEvent {
    JobEvent {
        run: RUN.into(),
        agent: "direct".into(),
        job_id: "j-0123456789abcdef".into(),
    }
}

#[tokio::test]
async fn a_queued_job_stores_the_body_then_the_manifest_and_finishes_with_the_item() {
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    d.table.job_create(&queued(Some("main"))).await.unwrap();
    let end = run_job(&d, &event()).await;
    let JobEnd::Succeeded(item) = end else {
        panic!("{end:?}")
    };
    assert_eq!((item.id.as_str(), item.kind), ("i-0001", ItemKind::Repo));
    assert_eq!(
        (item.licence.as_deref(), item.files, item.dropped),
        (Some("MIT"), Some(1), Some(1))
    );

    let order = d.blobs.order();
    assert_eq!(
        order.len(),
        3,
        "body, manifest and the completion audit object: {order:?}"
    );
    assert!(
        order[0].ends_with("items/i-0001/body.tar.gz")
            && order[1].ends_with("items/i-0001/manifest.json")
    );
    assert_eq!(
        d.blobs.bytes(&item.body_key).unwrap(),
        b"filtered tar.gz bytes"
    );
    let manifest: ItemManifest =
        serde_json::from_slice(&d.blobs.bytes(&item.manifest_key).unwrap()).unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.sha256(), item.manifest_sha256);
    assert_eq!(manifest.commit.as_deref(), Some(COMMIT));
    assert_eq!(manifest.git_ref.as_deref(), Some("main"));
    assert_eq!(manifest.origin, REPO);
    assert_eq!(manifest.files.len(), 1);
    assert_eq!(manifest.dropped[0].reason, "excluded-dir");
    assert_eq!(manifest.caps.unwrap().max_files, 20_000);
    assert_eq!(
        manifest.provider,
        harness_protocol::research::ResearchProvider::GithubArchive
    );

    let job = d
        .table
        .job_get(RUN, "j-0123456789abcdef")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.item.unwrap().id, "i-0001");
    assert_eq!(d.table.usage(RUN).await.unwrap().context_bytes, 21);
    assert_eq!(d.source.asked(), vec!["sindresorhus/is-plain-obj@main"]);

    // The completion audit object names the item and the job.
    let audit: serde_json::Value =
        serde_json::from_slice(&d.blobs.bytes(&order[2]).unwrap()).unwrap();
    assert_eq!(
        (audit["op"].as_str(), audit["status"].as_str()),
        (Some("repo"), Some("ok"))
    );
    assert_eq!(audit["items"][0], "i-0001");
    assert_eq!(audit["jobId"], "j-0123456789abcdef");
}

#[tokio::test]
async fn a_pinned_commit_must_be_the_commit_the_archive_carries() {
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    d.table
        .job_create(&queued(Some("0123456789abcdef0123456789abcdef01234567")))
        .await
        .unwrap();
    let end = run_job(&d, &event()).await;
    let JobEnd::Failed(e) = end else {
        panic!("{end:?}")
    };
    assert_eq!(e.code, ResearchErrorCode::UpstreamError);
    assert!(
        d.blobs.keys().iter().all(|k| !k.contains("/items/")),
        "nothing is stored"
    );
    let job = d
        .table
        .job_get(RUN, "j-0123456789abcdef")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.error.unwrap().code, ResearchErrorCode::UpstreamError);

    // The same ref as the carried commit passes.
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    d.table.job_create(&queued(Some(COMMIT))).await.unwrap();
    assert!(matches!(run_job(&d, &event()).await, JobEnd::Succeeded(_)));
}

#[tokio::test]
async fn download_and_filter_failures_finish_the_job_with_their_word_and_store_nothing() {
    let d = deps(
        FixedArchive::failing(Fail::new(
            ResearchErrorCode::NotFound,
            "GitHub has no such repository",
        )),
        canned,
    );
    d.table.job_create(&queued(None)).await.unwrap();
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::NotFound);

    let d = deps(FixedArchive::serving(b"gz".to_vec()), too_big);
    d.table.job_create(&queued(None)).await.unwrap();
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::TooLarge);
    assert!(d.blobs.keys().iter().all(|k| !k.contains("/items/")));
    let job = d
        .table
        .job_get(RUN, "j-0123456789abcdef")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
}

#[tokio::test]
async fn the_download_cap_of_the_policy_is_what_the_source_is_given() {
    let d = deps(FixedArchive::serving(vec![0u8; 100]), canned);
    let mut p = policy(RUN);
    p.limits.repo_download_bytes = 50;
    p.limits.repo_compressed_bytes = 50;
    d.table.put_policy(p);
    d.table.job_create(&queued(None)).await.unwrap();
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::TooLarge);
}

#[tokio::test]
async fn a_job_that_is_not_queued_or_has_no_policy_or_no_room_does_nothing_harmful() {
    // Already running (another invocation has it): skipped, nothing written.
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    let mut j = queued(None);
    j.state = JobState::Running;
    d.table.job_create(&j).await.unwrap();
    assert!(matches!(run_job(&d, &event()).await, JobEnd::Skipped(_)));
    assert!(d.blobs.keys().is_empty() && d.source.asked().is_empty());

    // No such job.
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    assert!(matches!(run_job(&d, &event()).await, JobEnd::Skipped(_)));

    // Research switched off while the job waited.
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    d.table.job_create(&queued(None)).await.unwrap();
    d.table.switch_off(RUN);
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::Disabled);
    assert!(d.source.asked().is_empty());

    // The storage budget is used up: failed with limit_bytes, nothing stored.
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned_small);
    let mut p = policy(RUN);
    p.limits.context_bytes = 10;
    p.limits.page_bytes = 10;
    p.limits.repo_unpacked_bytes = 10;
    d.table.put_policy(p);
    d.table.job_create(&queued(None)).await.unwrap();
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::LimitBytes);
    assert!(d.blobs.keys().iter().all(|k| !k.contains("/items/")));
}

#[tokio::test]
async fn a_job_that_waited_longer_than_two_minutes_fails_as_not_started_and_never_starts() {
    let d = deps(FixedArchive::serving(b"gz".to_vec()), canned);
    let mut j = queued(None);
    j.created_s = NOW_MS / 1000 - 121;
    d.table.job_create(&j).await.unwrap();
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::NotStarted);
    assert!(d.source.asked().is_empty());
    assert_eq!(
        d.table
            .job_get(RUN, "j-0123456789abcdef")
            .await
            .unwrap()
            .unwrap()
            .state,
        JobState::Failed
    );
}

/// A source that switches research off for the run while the download is under way.
struct SwitchOffWhileDownloading {
    table: MemTable,
    inner: FixedArchive,
}

impl research_api::ports::ArchiveSource for SwitchOffWhileDownloading {
    async fn download(
        &self,
        repo: &research_api::ports::GithubRepo,
        git_ref: Option<&str>,
        max_bytes: u64,
    ) -> Result<research_api::ports::Downloaded, Fail> {
        self.table.switch_off(RUN);
        self.inner.download(repo, git_ref, max_bytes).await
    }
}

#[tokio::test]
async fn research_switched_off_during_the_download_stores_nothing() {
    let table = MemTable::new();
    table.put_policy(policy(RUN));
    let d = JobDeps {
        table: table.clone(),
        blobs: MemBlobs::new(),
        source: SwitchOffWhileDownloading {
            table,
            inner: FixedArchive::serving(b"gz".to_vec()),
        },
        clock: FixedClock::at_ms(NOW_MS),
        filter: canned,
    };
    d.table.job_create(&queued(None)).await.unwrap();
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::Disabled);
    assert!(d.blobs.keys().iter().all(|k| !k.contains("/items/")));
    assert_eq!(d.table.usage(RUN).await.unwrap().context_bytes, 0);
}

/// A source that lowers the storage budget of the run while the download is under way.
struct LowerBudgetWhileDownloading {
    table: MemTable,
    inner: FixedArchive,
}

impl research_api::ports::ArchiveSource for LowerBudgetWhileDownloading {
    async fn download(
        &self,
        repo: &research_api::ports::GithubRepo,
        git_ref: Option<&str>,
        max_bytes: u64,
    ) -> Result<research_api::ports::Downloaded, Fail> {
        let mut p = policy(RUN);
        p.limits.context_bytes = 10;
        p.limits.page_bytes = 10;
        p.limits.repo_unpacked_bytes = 10;
        self.table.put_policy(p);
        self.inner.download(repo, git_ref, max_bytes).await
    }
}

#[tokio::test]
async fn the_limits_the_run_holds_after_the_download_are_the_ones_the_result_is_held_to() {
    let table = MemTable::new();
    table.put_policy(policy(RUN));
    let d = JobDeps {
        table: table.clone(),
        blobs: MemBlobs::new(),
        source: LowerBudgetWhileDownloading {
            table,
            inner: FixedArchive::serving(b"gz".to_vec()),
        },
        clock: FixedClock::at_ms(NOW_MS),
        filter: canned,
    };
    d.table.job_create(&queued(None)).await.unwrap();
    // The filtered archive (21 bytes) was fine under the limits at the start and is over the lowered ones.
    let JobEnd::Failed(e) = run_job(&d, &event()).await else {
        panic!()
    };
    assert_eq!(e.code, ResearchErrorCode::TooLarge);
    assert!(d.blobs.keys().iter().all(|k| !k.contains("/items/")));
}

/// A source that lets another writer finish the job (a lazy timeout, say) while the download is under way.
struct FinishJobWhileDownloading {
    table: MemTable,
    inner: FixedArchive,
}

impl research_api::ports::ArchiveSource for FinishJobWhileDownloading {
    async fn download(
        &self,
        repo: &research_api::ports::GithubRepo,
        git_ref: Option<&str>,
        max_bytes: u64,
    ) -> Result<research_api::ports::Downloaded, Fail> {
        let moved = self
            .table
            .job_move(
                RUN,
                "j-0123456789abcdef",
                &[JobState::Running],
                JobState::Failed,
                NOW_MS / 1000,
                None,
                None,
            )
            .await?;
        assert!(moved, "the job was running when the download began");
        self.inner.download(repo, git_ref, max_bytes).await
    }
}

#[tokio::test]
async fn a_job_another_writer_finished_while_it_ran_leaves_no_completion_audit_object() {
    let table = MemTable::new();
    table.put_policy(policy(RUN));
    let d = JobDeps {
        table: table.clone(),
        blobs: MemBlobs::new(),
        source: FinishJobWhileDownloading {
            table,
            inner: FixedArchive::serving(b"gz".to_vec()),
        },
        clock: FixedClock::at_ms(NOW_MS),
        filter: canned,
    };
    d.table.job_create(&queued(None)).await.unwrap();
    let end = run_job(&d, &event()).await;
    assert!(matches!(end, JobEnd::Skipped(_)), "{end:?}");
    // The audit object says what the job's own state says: this writer's success never became the state, so it writes none.
    assert!(
        d.blobs.keys().iter().all(|k| !k.contains("/audit/")),
        "{:?}",
        d.blobs.keys()
    );
    let job = d
        .table
        .job_get(RUN, "j-0123456789abcdef")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert!(job.item.is_none());
}
