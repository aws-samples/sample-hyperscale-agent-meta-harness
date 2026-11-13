//! A repository download is not lost because one file has an odd name: the job runs the real archive filter over an archive built at run
//! time, finishes with an item, and the odd file is listed as dropped with its reason. The item manifest passes its own validation (a file
//! path of at most 256 bytes without a control character, a dropped entry with a path and a reason). Ordinary input only.
#![allow(clippy::unwrap_used)]

mod job_support;
mod support;

use harness_protocol::research::{ItemManifest, JobState};
use job_support::{COMMIT, Tar, github, gz, top};
use research_api::archive::{filter_archive, reason};
use research_api::fakes::{FixedArchive, FixedClock, MemBlobs, MemTable};
use research_api::jobs::{JobDeps, JobEnd, run_job};
use research_api::ports::{JobEvent, JobRecord, Table};
use support::{NOW_MS, RUN, policy};

/// Runs the job over `archive` and gives back the manifest of the stored item.
async fn download(archive: Vec<u8>) -> ItemManifest {
    let d = JobDeps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        source: FixedArchive::serving(archive),
        clock: FixedClock::at_ms(NOW_MS),
        filter: filter_archive,
    };
    d.table.put_policy(policy(RUN));
    let job = JobRecord {
        run: RUN.into(),
        agent: "direct".into(),
        job_id: "j-0123456789abcdef".into(),
        req_hash: "00".repeat(32),
        state: JobState::Queued,
        created_s: NOW_MS / 1000,
        updated_s: NOW_MS / 1000,
        url: "https://github.com/sindresorhus/is-plain-obj".into(),
        git_ref: None,
        paths: vec![],
        item: None,
        error: None,
    };
    d.table.job_create(&job).await.unwrap();
    let event = JobEvent {
        run: RUN.into(),
        agent: "direct".into(),
        job_id: job.job_id.clone(),
    };
    let end = run_job(&d, &event).await;
    let JobEnd::Succeeded(item) = end else {
        panic!("the download is lost: {end:?}")
    };
    let manifest: ItemManifest =
        serde_json::from_slice(&d.blobs.bytes(&item.manifest_key).unwrap()).unwrap();
    manifest.validate().unwrap();
    manifest
}

fn kept(m: &ItemManifest) -> Vec<&str> {
    m.files.iter().map(|f| f.path.as_str()).collect()
}

fn archive(build: impl FnOnce(&mut Tar, &str)) -> Vec<u8> {
    gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a-before.txt"), b"before\n");
        build(t, top);
        t.file(&format!("{top}/z-after.txt"), b"after\n");
    }))
}

#[tokio::test]
async fn a_file_whose_name_has_a_tab_does_not_fail_the_download() {
    let m = download(archive(|t, top| {
        t.file_bytes_name(format!("{top}/notes\tone.txt").as_bytes(), b"tab\n");
    }))
    .await;
    assert_eq!(kept(&m), ["a-before.txt", "z-after.txt"]);
    let dropped: Vec<(&str, &str)> = m
        .dropped
        .iter()
        .map(|d| (d.path.as_str(), d.reason.as_str()))
        .collect();
    assert_eq!(
        dropped,
        [(
            &format!("{}/notes?one.txt", top()) as &str,
            reason::UNSUPPORTED_CHARACTER
        )]
    );
}

#[tokio::test]
async fn a_file_with_a_path_over_the_manifest_limit_does_not_fail_the_download() {
    let long = format!(
        "{}/{}/{}.txt",
        "d".repeat(100),
        "e".repeat(100),
        "f".repeat(100)
    );
    let m = download(archive(|t, top| {
        t.pax_next(&[("path", format!("{top}/{long}").as_bytes())]);
        t.file("short", b"long path\n");
    }))
    .await;
    assert_eq!(kept(&m), ["a-before.txt", "z-after.txt"]);
    assert_eq!(m.dropped.len(), 1);
    assert_eq!(m.dropped[0].reason, reason::NAME_TOO_LONG);
    assert!(m.dropped[0].path.chars().count() <= 256);
}

#[tokio::test]
async fn a_file_with_no_name_does_not_fail_the_download() {
    let m = download(archive(|t, _| {
        t.pax_next(&[("path", b"")]);
        t.file("short", b"nameless\n");
    }))
    .await;
    assert_eq!(kept(&m), ["a-before.txt", "z-after.txt"]);
    assert_eq!(m.dropped.len(), 1);
    assert_eq!(m.dropped[0].reason, reason::EMPTY_COMPONENT);
    assert!(!m.dropped[0].path.is_empty());
}

#[tokio::test]
async fn names_with_spaces_at_their_ends_are_ordinary_names_and_are_kept_as_written() {
    let m = download(archive(|t, top| {
        t.file_bytes_name(format!("{top}/ leading.txt").as_bytes(), b"one\n");
        t.file_bytes_name(format!("{top}/trailing.txt ").as_bytes(), b"two\n");
        t.file_bytes_name(format!("{top}/dir /in side.txt").as_bytes(), b"three\n");
    }))
    .await;
    assert_eq!(
        kept(&m),
        [
            " leading.txt",
            "a-before.txt",
            "dir /in side.txt",
            "trailing.txt ",
            "z-after.txt"
        ]
    );
    assert!(m.dropped.is_empty(), "{:?}", m.dropped);
}
