//! `ham pull <runId>`: the artifact list, a presigned URL, then 16 MiB ranged
//! GETs into `<file>.part` with an ETag sidecar, resumed from the `.part` when the ETag is
//! unchanged, SHA-256 verified against the artifact before the rename.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::https::HttpsPlane;
use ham::pull::{PullError, download, local_name, pull, select};
use ham::sigv4::Signer;
use ham_core::command::PullArtifact;
use ham_core::download::{CHUNK_BYTES, DownloadError, etag_path, part_path};
use harness_protocol::api::v1::ArtifactView;
use harness_protocol::store::{ArtifactKind, s3};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::{Fake, REGION, creds, sha256_hex, zip_bytes};

const MIB: u64 = 1024 * 1024;

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    })
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{prompt}"))
        .await
        .unwrap()
        .run_id
}

fn ranges(fake: &Fake) -> Vec<(u64, u64)> {
    fake.mem
        .inner
        .lock()
        .unwrap()
        .ranges
        .iter()
        .filter(|(_, s, e)| !(*s == 0 && *e == 0))
        .map(|(_, s, e)| (*s, *e))
        .collect()
}

#[tokio::test]
async fn a_zip_is_fetched_in_16_mib_ranges_verified_and_renamed() {
    assert_eq!(CHUNK_BYTES, 16 * MIB);
    let fake = Fake::start().await;
    let run = new_run(&fake, "big").await;
    let bytes = zip_bytes((40 * MIB) as usize);
    let (artifact, sha) = fake.mem.deliverable(&run, bytes);
    let dir = support::scratch("pull-big");
    let got = pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap();
    assert_eq!(got.artifact.artifact_id, artifact);
    assert_eq!(got.path, dir.join(format!("python-calc-{run}-abc1234.zip")));
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);
    assert!(!part_path(&got.path).exists() && !etag_path(&got.path).exists());
    assert_eq!(got.resumed_from, 0);
    let want = vec![
        (0, 16 * MIB - 1),
        (16 * MIB, 32 * MIB - 1),
        (32 * MIB, 40 * MIB - 1),
    ];
    let mut seen = ranges(&fake);
    seen.sort();
    assert_eq!(seen, want);
    assert_eq!(got.ranges, want);
    // Every object GET carried the ETag precondition and no SigV4 header (the URL is presigned).
    let etag = fake
        .mem
        .inner
        .lock()
        .unwrap()
        .objects
        .values()
        .next()
        .unwrap()
        .1
        .clone();
    for s in fake
        .mem
        .seen()
        .iter()
        .filter(|s| s.path.starts_with("/objects/"))
    {
        assert!(!s.headers.contains_key("authorization"));
        if s.headers["range"] != "bytes=0-0" {
            assert_eq!(s.headers["if-match"], etag);
        }
    }
    assert!(got.url.url.contains("X-Amz-Expires=900"));
    assert!(harness_protocol::common::is_utc_timestamp(
        &got.url.expires_at
    ));
}

#[tokio::test]
async fn a_killed_download_resumes_with_only_the_missing_ranges() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "resume").await;
    let (_, sha) = fake.mem.deliverable(&run, zip_bytes((40 * MIB) as usize));
    let dir = support::scratch("pull-resume");
    fake.faults.fail_range_at.store(32 * MIB, Ordering::SeqCst);
    let a = api(&fake).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let e = pull(&a, &run, &PullArtifact::Zip, &dir).await.unwrap_err();
    assert!(matches!(e, PullError::Fetch(_)), "{e:?}");
    assert!(
        !e.to_string().contains("X-Amz-"),
        "no presigned query in errors: {e}"
    );
    let dest = dir.join(format!("python-calc-{run}-abc1234.zip"));
    assert!(!dest.exists());
    assert_eq!(std::fs::metadata(part_path(&dest)).unwrap().len(), 32 * MIB);
    assert!(etag_path(&dest).exists());

    fake.mem.inner.lock().unwrap().ranges.clear();
    let got = pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap();
    assert_eq!(got.resumed_from, 32 * MIB);
    assert_eq!(ranges(&fake), vec![(32 * MIB, 40 * MIB - 1)]);
    assert_eq!(got.ranges, vec![(32 * MIB, 40 * MIB - 1)]);
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);
}

#[tokio::test]
async fn a_changed_object_restarts_the_download() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "changed").await;
    let (artifact, _) = fake.mem.deliverable(&run, zip_bytes((20 * MIB) as usize));
    let dir = support::scratch("pull-changed");
    fake.faults.fail_range_at.store(16 * MIB, Ordering::SeqCst);
    let one = api(&fake).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    pull(&one, &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap_err();
    // The harness replaced the object: new bytes, new ETag, new SHA-256.
    let fresh: Vec<u8> = zip_bytes((20 * MIB) as usize)
        .iter()
        .map(|b| b ^ 0x5a)
        .collect();
    let sha = sha256_hex(&fresh);
    {
        let mut g = fake.mem.inner.lock().unwrap();
        let key = s3::deliverable_key(&run, "python-calc", "abc1234");
        g.objects.insert(key, (fresh, "\"changed-etag\"".into()));
        g.artifacts
            .get_mut(&(run.clone(), artifact.clone()))
            .unwrap()
            .sha256 = sha.clone();
        g.ranges.clear();
    }
    let got = pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap();
    assert_eq!(got.resumed_from, 0);
    let mut seen = ranges(&fake);
    seen.sort();
    assert_eq!(seen, vec![(0, 16 * MIB - 1), (16 * MIB, 20 * MIB - 1)]);
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);
}

#[tokio::test]
async fn a_sha256_mismatch_keeps_no_file() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "mismatch").await;
    let (artifact, _) = fake.mem.deliverable(&run, zip_bytes(5000));
    fake.mem
        .inner
        .lock()
        .unwrap()
        .artifacts
        .get_mut(&(run.clone(), artifact))
        .unwrap()
        .sha256 = "0".repeat(64);
    let dir = support::scratch("pull-mismatch");
    let e = pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap_err();
    assert!(
        matches!(e, PullError::Download(DownloadError::Sha256Mismatch { .. })),
        "{e:?}"
    );
    let dest = dir.join(format!("python-calc-{run}-abc1234.zip"));
    assert!(!dest.exists() && !part_path(&dest).exists() && !etag_path(&dest).exists());
}

#[tokio::test]
async fn an_expired_url_is_replaced_once() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "expired").await;
    let (_, sha) = fake.mem.deliverable(&run, zip_bytes(5000));
    fake.faults.object_403.store(1, Ordering::SeqCst);
    let dir = support::scratch("pull-expired");
    let got = pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap();
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);
    assert_eq!(fake.mem.inner.lock().unwrap().presigned.len(), 2);
    assert_eq!(
        fake.mem
            .posts(&format!(
                "/v1/runs/{run}/artifacts/{}/download-url",
                got.artifact.artifact_id
            ))
            .len(),
        2
    );
}

#[tokio::test]
async fn a_file_already_verified_is_not_fetched_again_and_an_empty_object_downloads() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "again").await;
    fake.mem.deliverable(&run, zip_bytes(5000));
    let dir = support::scratch("pull-again");
    pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap();
    fake.mem.inner.lock().unwrap().ranges.clear();
    let again = pull(&api(&fake), &run, &PullArtifact::Zip, &dir)
        .await
        .unwrap();
    assert!(again.ranges.is_empty());
    assert!(fake.mem.inner.lock().unwrap().ranges.is_empty());

    let (_, sha) = fake.mem.object(
        &run,
        &format!("{}EVIDENCE.md", s3::evidence_prefix(&run)),
        ArtifactKind::Evidence,
        Vec::new(),
    );
    let got = pull(&api(&fake), &run, &PullArtifact::Evidence, &dir)
        .await
        .unwrap();
    assert_eq!(got.path, dir.join("EVIDENCE.md"));
    assert_eq!(std::fs::read(&got.path).unwrap(), Vec::<u8>::new());
    assert_eq!(sha, sha256_hex(b""));
}

#[tokio::test]
async fn an_object_whose_size_differs_from_the_artifact_is_refused() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "size").await;
    let (artifact, _) = fake.mem.deliverable(&run, zip_bytes(5000));
    fake.mem
        .inner
        .lock()
        .unwrap()
        .artifacts
        .get_mut(&(run.clone(), artifact.clone()))
        .unwrap()
        .size_bytes = 4000;
    let a = api(&fake);
    let view = a
        .list_artifacts(&run)
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.artifact_id == artifact)
        .unwrap();
    let e = download(&a, &run, &view, &support::scratch("pull-size"))
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("5000") && e.to_string().contains("4000"),
        "{e}"
    );
}

fn view(id: &str, kind: ArtifactKind, key: &str) -> ArtifactView {
    ArtifactView {
        artifact_id: id.into(),
        kind,
        s3_uri: format!("s3://b/{key}"),
        sha256: "ab".repeat(32),
        size_bytes: 1,
    }
}

#[test]
fn artifact_selection_and_local_names() {
    let list = vec![
        view(
            "01A",
            ArtifactKind::Zip,
            "runs/R/deliverable/app-R-aaaaaaa.zip",
        ),
        view(
            "01C",
            ArtifactKind::Zip,
            "runs/R/deliverable/app-R-ccccccc.zip",
        ),
        view("01B", ArtifactKind::Evidence, "runs/R/evidence/EVIDENCE.md"),
    ];
    assert_eq!(
        select(&list, &PullArtifact::Zip).unwrap().artifact_id,
        "01C",
        "newest zip"
    );
    assert_eq!(
        select(&list, &PullArtifact::Evidence).unwrap().artifact_id,
        "01B"
    );
    let e = select(&list, &PullArtifact::Partial).unwrap_err();
    assert!(matches!(e, PullError::NoArtifact(_)), "{e:?}");
    assert!(select(&[], &PullArtifact::Zip).is_err());

    // A held run's partial zip and the evidence archive.
    let held = vec![
        view(
            "01E",
            ArtifactKind::Partial,
            "runs/R/deliverable/app-R-eeeeeee-partial.zip",
        ),
        view(
            "01F",
            ArtifactKind::Evidence,
            "runs/R/deliverable/app-R-eeeeeee-evidence.zip",
        ),
    ];
    assert_eq!(
        select(&held, &PullArtifact::Partial).unwrap().artifact_id,
        "01E"
    );
    assert_eq!(
        select(&held, &PullArtifact::Evidence).unwrap().artifact_id,
        "01F"
    );
    let e = select(&held, &PullArtifact::Zip).unwrap_err().to_string();
    assert!(
        e.contains("partial 01E") && e.contains("evidence 01F"),
        "{e}"
    );
    assert_eq!(local_name("R", &held[0]), "app-R-eeeeeee-partial.zip");

    assert_eq!(local_name("R", &list[0]), "app-R-aaaaaaa.zip");
    assert_eq!(local_name("R", &list[2]), "EVIDENCE.md");
    for (key, want) in [
        ("runs/R/deliverable/..", "R-01X.zip"),
        ("runs/R/deliverable/", "R-01X.zip"),
        ("runs/R/deliverable/a b;rm.zip", "R-01X.zip"),
        ("runs/R/deliverable/.hidden.zip", "R-01X.zip"),
    ] {
        assert_eq!(
            local_name("R", &view("01X", ArtifactKind::Zip, key)),
            want,
            "{key}"
        );
    }
}

#[tokio::test]
async fn range_failures_are_named_and_a_range_403_gets_one_fresh_url() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "range-faults").await;
    let (_, sha) = fake.mem.deliverable(&run, zip_bytes((17 * MIB) as usize));
    let one = api(&fake).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });

    // A 403 on the ranges (the URL lapsed after the probe): one fresh URL, then success.
    fake.faults.range_403.store(2, Ordering::SeqCst);
    let dir = support::scratch("pull-range403");
    let got = pull(&one, &run, &PullArtifact::Zip, &dir).await.unwrap();
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);
    assert_eq!(fake.mem.inner.lock().unwrap().presigned.len(), 2);

    // Still 403 with the fresh URL: the download stops and says why.
    fake.faults.range_403.store(4, Ordering::SeqCst);
    let e = pull(
        &one,
        &run,
        &PullArtifact::Zip,
        &support::scratch("pull-range403b"),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("expired"), "{e}");

    fake.faults.range_412.store(1, Ordering::SeqCst);
    let e = pull(
        &one,
        &run,
        &PullArtifact::Zip,
        &support::scratch("pull-412"),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("the object changed"), "{e}");

    fake.faults.bad_content_range.store(1, Ordering::SeqCst);
    let e = pull(&one, &run, &PullArtifact::Zip, &support::scratch("pull-cr"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Content-Range"), "{e}");
    assert!(!e.to_string().contains("X-Amz-"), "{e}");
}

#[tokio::test]
async fn a_failed_range_is_retried_and_a_store_that_ignores_range_still_verifies() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "retry-range").await;
    let (_, sha) = fake.mem.deliverable(&run, zip_bytes((20 * MIB) as usize));
    fake.faults.fail_range_at.store(16 * MIB, Ordering::SeqCst);
    let got = pull(
        &api(&fake),
        &run,
        &PullArtifact::Zip,
        &support::scratch("pull-retry"),
    )
    .await
    .unwrap();
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);

    fake.faults.probe_whole.store(1, Ordering::SeqCst);
    let got = pull(
        &api(&fake),
        &run,
        &PullArtifact::Zip,
        &support::scratch("pull-whole"),
    )
    .await
    .unwrap();
    assert_eq!(sha256_hex(&std::fs::read(&got.path).unwrap()), sha);
    assert!(
        got.ranges.is_empty(),
        "the probe answer was the whole object"
    );

    // A missing object is named by its path, without the presigned query.
    fake.mem.inner.lock().unwrap().objects.clear();
    let e = pull(
        &api(&fake),
        &run,
        &PullArtifact::Zip,
        &support::scratch("pull-gone"),
    )
    .await
    .unwrap_err();
    assert!(
        e.to_string().contains("HTTP 404") && !e.to_string().contains("X-Amz-"),
        "{e}"
    );
}

#[test]
fn an_artifact_with_an_unusual_key_gets_a_safe_local_name() {
    let b = view("01B", ArtifactKind::Evidence, "runs/R/evidence/a b.zip");
    assert_eq!(local_name("R", &b), "R-01B.zip");
}
