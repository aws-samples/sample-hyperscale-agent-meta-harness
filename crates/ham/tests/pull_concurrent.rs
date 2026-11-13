//! Downloads of one artifact that overlap (BATR1 rehearsal defect D2, 2026-10-17): the dashboard downloads the zip on delivery and Enter on
//! the zip row of the Artifacts screen starts a second download; two `ham pull` shells do the same. Both used to append to one
//! `<file>.part`; the second kept writing through its old handle after the first renamed the file, so the zip on disk was twice its size
//! (3 of 5 TUI runs, 23 of 24 pairs of shells) while the first download had printed `sha256 matched`.
//!
//! What must hold now: one writer per `.part` (a lock file beside it), the second download waits and then keeps the file the first one
//! verified, a holder that dies hands its `.part` to the waiter (resume), a holder that makes no progress ends the wait with a clean
//! error, the size and the SHA-256 are checked against the artifact before the rename, and a directory that cannot lock files gets a
//! download into its own unique file. Nothing is left behind but the zip.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use ham::api::{Api, Retry};
use ham::http::{ControlPlane, Response, TransportError};
use ham::https::HttpsPlane;
use ham::pull::{PullError, download, download_unlocked, download_waiting};
use ham::sigv4::Signer;
use ham_core::download::{CHUNK_BYTES, etag_path, lock_path, part_path};
use harness_protocol::api::v1::ArtifactView;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use support::{Fake, REGION, creds, sha256_hex, zip_bytes};
use tokio::sync::watch;

fn https(fake: &Fake) -> HttpsPlane {
    HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap()
}

fn retry() -> Retry {
    Retry {
        attempts: 3,
        base: Duration::from_millis(10),
    }
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(https(fake))).with_retry(retry())
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

/// A delivered run with a zip of `n` bytes; its artifact, its bytes and the destination file name.
async fn delivered(fake: &Fake, label: &str, n: usize) -> (String, ArtifactView, Vec<u8>, String) {
    let run = new_run(fake, label).await;
    let bytes = zip_bytes(n);
    fake.mem.deliverable(&run, bytes.clone());
    let a = api(fake)
        .list_artifacts(&run)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let name = format!("python-calc-{run}-abc1234.zip");
    (run, a, bytes, name)
}

fn etag_of_the_object(fake: &Fake) -> String {
    fake.mem
        .inner
        .lock()
        .unwrap()
        .objects
        .values()
        .next()
        .unwrap()
        .1
        .clone()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// What another process holding the destination looks like: the lock file, locked.
fn hold(dest: &Path) -> std::fs::File {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(dest))
        .unwrap();
    f.try_lock().unwrap();
    f
}

/// A control plane that lets the probe of an object through and holds each data-range GET: it announces that it holds one, then waits
/// for `wait_for` (or its time limit) before it goes on. Two of them, one per download, make the old interleaving happen on purpose:
/// the first download waits until the second has opened its `.part`, and the second waits until the first has renamed the file.
struct Plane {
    inner: HttpsPlane,
    parked: watch::Sender<bool>,
    wait_for: Option<(watch::Receiver<bool>, Duration)>,
}

impl ControlPlane for Plane {
    fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Response, TransportError>> + Send {
        self.inner.call(method, path, headers, body)
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        if range.is_some_and(|r| r != (0, 0)) {
            self.parked.send_replace(true);
            if let Some((rx, limit)) = &self.wait_for {
                let mut rx = rx.clone();
                let _ = tokio::time::timeout(*limit, rx.wait_for(|v| *v)).await;
            }
        }
        self.inner.fetch(url, range, if_match).await
    }
}

/// The two planes of an overlap: the first download's, the second download's and the signal that the first one is done.
fn overlap(fake: &Fake) -> (Api<Plane>, Api<Plane>, watch::Sender<bool>) {
    let (b_parked_tx, b_parked_rx) = watch::channel(false);
    let (a_parked_tx, _a_parked_rx) = watch::channel(false);
    let (a_done_tx, a_done_rx) = watch::channel(false);
    let first = Plane {
        inner: https(fake),
        parked: a_parked_tx,
        wait_for: Some((b_parked_rx, Duration::from_millis(1500))),
    };
    let second = Plane {
        inner: https(fake),
        parked: b_parked_tx,
        wait_for: Some((a_done_rx, Duration::from_secs(60))),
    };
    (
        Api::new(Arc::new(first)).with_retry(retry()),
        Api::new(Arc::new(second)).with_retry(retry()),
        a_done_tx,
    )
}

#[tokio::test]
async fn two_downloads_of_one_artifact_at_once_leave_one_correct_file() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "twice", 60_000).await;
    let dir = support::scratch("pull-twice");
    let (first, second, a_done) = overlap(&fake);
    let one = async {
        let r = download(&first, &run, &a, &dir).await;
        a_done.send_replace(true);
        r
    };
    let (ra, rb) = tokio::join!(one, download(&second, &run, &a, &dir));
    let on_disk = std::fs::read(dir.join(&name)).unwrap();
    assert_eq!(
        on_disk.len(),
        bytes.len(),
        "the zip is {} bytes; the artifact lists {} (a second writer appended its body through the old handle)",
        on_disk.len(),
        bytes.len()
    );
    assert_eq!(sha256_hex(&on_disk), a.sha256);
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert_eq!(
        ra.ranges.len() + rb.ranges.len(),
        1,
        "the bytes of the object are fetched once; the second download keeps the verified file"
    );
    assert_eq!(
        names(&dir),
        vec![name],
        "nothing but the zip is left: no .part, no .etag, no lock file"
    );
}

#[tokio::test]
async fn a_download_waits_for_the_one_that_holds_the_file_and_keeps_what_it_made() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "waits", 30_000).await;
    let dir = support::scratch("pull-waits");
    let dest = dir.join(&name);
    let held = hold(&dest);
    let waiter = {
        let (api, run, a, dir) = (api(&fake), run.clone(), a.clone(), dir.clone());
        tokio::spawn(async move { download(&api, &run, &a, &dir).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !waiter.is_finished(),
        "the download goes on while another download holds the file"
    );
    assert!(
        fake.mem.inner.lock().unwrap().ranges.is_empty(),
        "no byte is fetched while the file is held"
    );
    // The holder finishes: the verified file is in place, the lock is released.
    std::fs::write(&dest, &bytes).unwrap();
    drop(held);
    let got = tokio::time::timeout(Duration::from_secs(20), waiter)
        .await
        .expect("the waiter ends once the lock is free")
        .unwrap()
        .unwrap();
    assert!(got.ranges.is_empty(), "the waiter fetched nothing: {got:?}");
    assert_eq!(got.resumed_from, a.size_bytes);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(names(&dir), vec![name]);
}

#[tokio::test]
async fn a_holder_that_dies_hands_its_part_to_the_waiter() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "dies", 60_000).await;
    let dir = support::scratch("pull-dies");
    let dest = dir.join(&name);
    let held = hold(&dest);
    // What the dead holder wrote before it died: a prefix and its ETag.
    std::fs::write(part_path(&dest), &bytes[..40_000]).unwrap();
    std::fs::write(etag_path(&dest), etag_of_the_object(&fake)).unwrap();
    let waiter = {
        let (api, run, a, dir) = (api(&fake), run.clone(), a.clone(), dir.clone());
        tokio::spawn(async move { download(&api, &run, &a, &dir).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiter.is_finished(), "the waiter waits for the holder");
    drop(held);
    let got = tokio::time::timeout(Duration::from_secs(20), waiter)
        .await
        .expect("the waiter ends once the lock is free")
        .unwrap()
        .unwrap();
    assert_eq!(got.resumed_from, 40_000, "the waiter resumed the .part");
    assert_eq!(got.ranges, vec![(40_000, 59_999)]);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(names(&dir), vec![name]);
}

#[tokio::test]
async fn a_holder_that_makes_no_progress_ends_the_wait_with_a_clean_error() {
    let fake = Fake::start().await;
    let (run, a, _, name) = delivered(&fake, "stuck", 20_000).await;
    let dir = support::scratch("pull-stuck");
    let dest = dir.join(&name);
    let _held = hold(&dest);
    let e = download_waiting(
        &api(&fake),
        &run,
        &a,
        &dir,
        CHUNK_BYTES,
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, PullError::Busy(_)), "{e:?}");
    let text = e.to_string();
    assert!(
        text.contains(&name) && text.contains("another download"),
        "the error names the file and what holds it: {text}"
    );
    assert!(
        !dest.exists() && !part_path(&dest).exists(),
        "the waiter wrote nothing: {:?}",
        names(&dir)
    );
    assert!(
        lock_path(&dest).exists(),
        "the lock file belongs to the holder; the waiter leaves it"
    );
}

#[tokio::test]
async fn a_holder_that_keeps_writing_is_waited_for_beyond_the_idle_limit() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "grows", 20_000).await;
    let dir = support::scratch("pull-grows");
    let dest = dir.join(&name);
    let held = hold(&dest);
    let waiter = {
        let (api, run, a, dir) = (api(&fake), run.clone(), a.clone(), dir.clone());
        tokio::spawn(async move {
            download_waiting(
                &api,
                &run,
                &a,
                &dir,
                CHUNK_BYTES,
                Duration::from_millis(400),
            )
            .await
        })
    };
    // The holder's .part grows every 150 ms for 1.2 s: three times the idle limit in all, never 400 ms without a byte.
    let mut part = OpenOptions::new()
        .create(true)
        .append(true)
        .open(part_path(&dest))
        .unwrap();
    for i in 0..8 {
        part.write_all(&bytes[i * 100..(i + 1) * 100]).unwrap();
        part.flush().unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter is still waiting after write {i}: {:?}",
            names(&dir)
        );
    }
    drop(part);
    std::fs::write(&dest, &bytes).unwrap();
    drop(held);
    let got = tokio::time::timeout(Duration::from_secs(20), waiter)
        .await
        .expect("the waiter ends once the lock is free")
        .unwrap()
        .unwrap();
    assert!(got.ranges.is_empty(), "{got:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
}

#[tokio::test]
async fn a_lock_file_left_by_a_killed_pull_does_not_block_and_is_removed() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "stale-lock", 20_000).await;
    let dir = support::scratch("pull-stale-lock");
    let dest = dir.join(&name);
    drop(hold(&dest));
    assert!(lock_path(&dest).exists());
    let got = download(&api(&fake), &run, &a, &dir).await.unwrap();
    assert_eq!(got.ranges.len(), 1);
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(names(&dir), vec![name], "the stale lock file is gone");
}

#[tokio::test]
async fn the_leftovers_of_a_verified_destination_are_cleaned_up() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "leftovers", 20_000).await;
    let dir = support::scratch("pull-leftovers");
    let dest = dir.join(&name);
    std::fs::write(&dest, &bytes).unwrap();
    std::fs::write(part_path(&dest), b"half a download").unwrap();
    std::fs::write(etag_path(&dest), "\"old\"").unwrap();
    drop(hold(&dest));
    let got = download(&api(&fake), &run, &a, &dir).await.unwrap();
    assert!(got.ranges.is_empty(), "the verified file is kept: {got:?}");
    assert_eq!(names(&dir), vec![name]);
}

#[tokio::test]
async fn a_destination_of_the_wrong_size_is_replaced_by_a_verified_download() {
    // What the old defect left behind: the zip twice over.
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "doubled", 20_000).await;
    let dir = support::scratch("pull-doubled");
    let dest = dir.join(&name);
    let mut twice = bytes.clone();
    twice.extend_from_slice(&bytes);
    std::fs::write(&dest, &twice).unwrap();
    let got = download(&api(&fake), &run, &a, &dir).await.unwrap();
    assert!(!got.ranges.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(names(&dir), vec![name]);
}

#[tokio::test]
async fn a_directory_that_cannot_lock_files_gets_a_download_into_its_own_unique_file() {
    let fake = Fake::start().await;
    let (run, a, bytes, name) = delivered(&fake, "unlocked", 60_000).await;
    let dir = support::scratch("pull-unlocked");
    let dest = dir.join(&name);
    // A lock-capable download's resumable prefix is not touched by the unlocked one.
    std::fs::write(part_path(&dest), b"keep me").unwrap();
    let (first, second, a_done) = overlap(&fake);
    let one = async {
        let r = download_unlocked(&first, &run, &a, &dir, CHUNK_BYTES).await;
        a_done.send_replace(true);
        r
    };
    let (ra, rb) = tokio::join!(one, download_unlocked(&second, &run, &a, &dir, CHUNK_BYTES));
    ra.unwrap();
    rb.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);
    assert_eq!(
        std::fs::read(part_path(&dest)).unwrap(),
        b"keep me",
        "the resumable .part of another download is left alone"
    );
    assert_eq!(
        names(&dir),
        vec![name.clone(), format!("{name}.part")],
        "no unique file of the unlocked downloads is left"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_ham_pull_processes_leave_one_correct_zip_twenty_times_of_twenty() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "shells").await;
    let bytes = zip_bytes(50_000);
    let (_, sha) = fake.mem.deliverable(&run, bytes.clone());
    // A slow store makes the two shells overlap the way a real one does.
    fake.mem.inner.lock().unwrap().range_delay = Duration::from_millis(120);
    let name = format!("python-calc-{run}-abc1234.zip");
    let mut bad: Vec<String> = Vec::new();
    for attempt in 1..=20 {
        let h = home::home(&fake, &format!("shells-{attempt}"));
        let args = ["pull", run.as_str(), "--json"];
        let (a, b) = tokio::join!(home::ham(&h, &args), home::ham(&h, &args));
        let mut problems: Vec<String> = Vec::new();
        for (who, o) in [("a", &a), ("b", &b)] {
            if o.status.code() != Some(0) {
                problems.push(format!(
                    "{who} exited {:?}: {}",
                    o.status.code(),
                    home::text(&o.stderr).trim()
                ));
            }
        }
        let on_disk: Option<PathBuf> = Some(h.downloads.join(&name)).filter(|p| p.is_file());
        match on_disk.map(|p| std::fs::read(p).unwrap()) {
            Some(got) if got.len() == bytes.len() && sha256_hex(&got) == sha => {}
            Some(got) => problems.push(format!(
                "the zip is {} bytes, not {}",
                got.len(),
                bytes.len()
            )),
            None => problems.push("no zip".into()),
        }
        if names(&h.downloads) != vec![name.clone()] {
            problems.push(format!("leftovers: {:?}", names(&h.downloads)));
        }
        if !problems.is_empty() {
            bad.push(format!("attempt {attempt}: {}", problems.join("; ")));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of 20 pairs of shells went wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
