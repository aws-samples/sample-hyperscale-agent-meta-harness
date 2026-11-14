//! Two copies of one run never share a file. A worker that numbers the copies of a run within the run gives each its own number; a
//! worker older than that starts again at 1 in every session, and then the file that is already there holds another commit: it is kept,
//! and the new copy is named with its commit as well.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::Api;
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham_core::snapshot::SNAPSHOT_REF;
use home::{ham, home, text};
use serde_json::json;
use std::sync::Arc;
use support::{Fake, REGION, creds};

const FIRST: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const SECOND: &str = "c8611bc0ffee03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

fn bundle(head: &str, filler: usize) -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n-{BASE} base subject\n{head} {SNAPSHOT_REF}\n\nPACK")
        .into_bytes();
    b.extend((0..filler).map(|i| (i % 251) as u8));
    b
}

async fn start(fake: &Fake) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": "Add a percent_change function", "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .start_direct_run(&req, "k-b842-names")
    .await
    .unwrap()
    .run_id
}

/// Puts a snapshot (bundle and record) under the run's turn prefix, as agentd does.
fn save_snapshot(fake: &Fake, run: &str, head: &str, object: &[u8], count: u32) {
    let prefix = harness_protocol::direct::s3::turn_prefix(run);
    let mut g = fake.mem.inner.lock().unwrap();
    g.objects.insert(
        format!("{prefix}snapshot.bundle"),
        (object.to_vec(), format!("\"etag-{}\"", &head[..6])),
    );
    g.objects.insert(
        format!("{prefix}snapshot.json"),
        (
            json!({"schemaVersion": 1, "headSha": head, "baseSha": BASE, "at": "2026-11-14T07:03:31Z", "count": count})
                .to_string()
                .into_bytes(),
            "\"etag-j\"".into(),
        ),
    );
}

fn files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn b842_a_second_copy_with_the_number_of_the_first_does_not_replace_it() {
    let fake = Fake::start().await;
    let run = start(&fake).await;
    let h = home(&fake, "b842-snapshot-names");

    let first = bundle(FIRST, 9_000);
    save_snapshot(&fake, &run, FIRST, &first, 1);
    let o = ham(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let plain = format!("{run}-snapshot-1.bundle");
    assert_eq!(files(&h.downloads), vec![plain.clone()]);

    // The same copy again: one file, the same bytes.
    let o = ham(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(files(&h.downloads), vec![plain.clone()]);

    // Another copy of the run that carries the same number (a later session of a worker that counts within the session).
    let second = bundle(SECOND, 12_000);
    save_snapshot(&fake, &run, SECOND, &second, 1);
    let o = ham(&h, &["pull", &run, "--snapshot"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let other = format!("{run}-snapshot-1-{}.bundle", &SECOND[..12]);
    assert_eq!(files(&h.downloads), vec![other.clone(), plain.clone()]);
    assert_eq!(
        std::fs::read(h.downloads.join(&plain)).unwrap(),
        first,
        "the first copy is still there"
    );
    assert_eq!(std::fs::read(h.downloads.join(&other)).unwrap(), second);
    assert!(
        text(&o.stdout).contains(&other),
        "the command names the file it wrote: {}",
        text(&o.stdout)
    );
}
