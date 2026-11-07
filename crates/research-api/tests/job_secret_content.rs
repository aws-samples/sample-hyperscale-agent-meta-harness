//! The other direction of the credential rule: a repository the job downloads may hold a credential, and none of it is kept. The job runs
//! the real archive filter over an archive built at run time, and the stored archive and the manifest hold the ordinary files only; the
//! environment files, the private-key files by name and the files that hold a private-key block are listed as dropped, with their reason
//! and no content. The key block is built from pieces: no line of this file is a credential.
#![allow(clippy::unwrap_used)]

mod job_support;
mod support;

use harness_protocol::research::{ItemManifest, JobState};
use job_support::{archive_of, pem_block, read_output, text};
use research_api::archive::{filter_archive, reason};
use research_api::fakes::{FixedArchive, FixedClock, MemBlobs, MemTable};
use research_api::jobs::{JobDeps, JobEnd, run_job};
use research_api::ports::{JobEvent, JobRecord, Table};
use support::{NOW_MS, RUN, policy};

#[tokio::test]
async fn credential_files_of_a_downloaded_repository_are_not_stored_and_are_listed_as_dropped() {
    let pem = pem_block();
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("README.md", text(1500)),
        ("src/lib.rs", text(2500)),
        (".env", b"SETTING=1\n".to_vec()),
        ("deploy/id_rsa", b"anything\n".to_vec()),
        ("deploy/server.pem", pem.clone()),
        ("notes/key-copy.txt", pem.clone()),
    ];
    let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let d = JobDeps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        source: FixedArchive::serving(archive_of(&borrowed)),
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
    let JobEnd::Succeeded(item) = run_job(&d, &event).await else {
        panic!("the job ends with an item")
    };

    // The stored archive holds the ordinary files and no credential, by name or by content.
    let body = d.blobs.bytes(&item.body_key).unwrap();
    let stored: Vec<String> = read_output(&body).into_iter().map(|f| f.path).collect();
    assert_eq!(stored, ["README.md", "src/lib.rs"]);
    let mut tar = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&body[..]), &mut tar).unwrap();
    assert!(
        !String::from_utf8_lossy(&tar).contains("PRIVATE KEY-----"),
        "a private-key block is in the stored archive"
    );

    // The manifest lists the same files and says why each other one was dropped, with no content.
    let manifest: ItemManifest =
        serde_json::from_slice(&d.blobs.bytes(&item.manifest_key).unwrap()).unwrap();
    manifest.validate().unwrap();
    let kept: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(kept, ["README.md", "src/lib.rs"]);
    let mut dropped: Vec<(&str, &str)> = manifest
        .dropped
        .iter()
        .map(|x| (x.path.as_str(), x.reason.as_str()))
        .collect();
    dropped.sort_unstable();
    assert_eq!(
        dropped,
        [
            (".env", reason::ENV_FILE),
            ("deploy/id_rsa", reason::PRIVATE_KEY_NAME),
            ("deploy/server.pem", reason::PRIVATE_KEY_CONTENT),
            ("notes/key-copy.txt", reason::PRIVATE_KEY_CONTENT),
        ]
    );
    assert!(
        !String::from_utf8_lossy(&d.blobs.bytes(&item.manifest_key).unwrap())
            .contains("PRIVATE KEY-----")
    );
}
