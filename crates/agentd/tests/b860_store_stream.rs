//! Storing a copy of the work must not hold agentd's other work.
//!
//! The measurement of the save's cost found the one place where agentd itself stopped answering: an upload read the whole bundle into
//! memory on a thread of the runtime and hashed it twice there (once for the record, once for the signature). With a bundle of 363 MB
//! the runtime woke up to 1.2 s late, and agentd held hundreds of megabytes; the credential socket, the tool server, the event flusher
//! and the reader of the engine's messages run on those threads. A download held the whole object in memory too, and its body had no
//! time limit at all.
//!
//! What these tests hold, said narrowly: the store and the download never take a runtime thread's processor for long, and agentd's
//! memory does not grow with the object. They hold no latency in milliseconds and no contention on the storage: the bench and the live
//! proof do.
//!
//! Each test must run in a process of its own. `cargo nextest` gives every test one; plain `cargo test` runs the tests of a file in
//! one process and can fail them for that alone: `b860_store_memory_stays_flat` and `b860_get_to_file_streams` read the peak memory
//! of the process, and `b860_retry_sends_the_same_bytes` and `b860_create_only_put_keeps_its_rules` read a counter of the whole
//! process (`agentd::s3::digests_made`).
//!
//! The time limits in these tests are of two kinds. A limit that is the test's finding (a request is given up after its no-progress
//! limit, long before its whole-time limit) is asserted. A limit that only keeps a stand-in or a request from waiting for good has
//! seconds of room, so that a host under load, where a thread may not run for hundreds of milliseconds, fails no test.
#![allow(clippy::unwrap_used)]

mod b860_support;
mod common;
mod direct_support;

use agentd::s3::S3Objects;
use b860_support::bucket::{Bucket, Plan};
use b860_support::{MIB, big_file, hash_of, peak_memory, thread_cpu};
use common::{creds, scratch};
use direct_support::{Direct, Opts, RUN};
use harness_protocol::direct::s3 as keys;
use harness_protocol::runtime::TurnOutcome;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The size of the object the two bounds are held with.
const LARGE: u64 = 256 * MIB;
/// The most processor time a runtime thread may use between two readings of a 5 ms ticker on that thread.
const HELD_AT_MOST: Duration = Duration::from_millis(100);
/// The most the peak memory of the process may grow while a large object is stored or fetched.
const GROWS_AT_MOST: u64 = 32 * MIB;

fn stand_in_creds() -> harness_protocol::api::internal::ProcessCredentials {
    creds("2099-01-01T00:00:00Z")
}

/// The answer of `work`; a test that hangs fails here.
async fn within<T>(limit: Duration, work: impl Future<Output = T>) -> T {
    tokio::time::timeout(limit, work)
        .await
        .expect("the test ended within its limit")
}

/// A task on the runtime of the test that wakes every 5 ms and reads the processor clock of the thread it runs on. On a
/// current-thread runtime that is the thread every other task of the runtime runs on, the connection's task included, so the greatest
/// step between two readings is the longest any of them kept that thread busy.
struct Ticker {
    worst_us: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Ticker {
    /// Starts the ticker and lets it read the clock once before anything else runs.
    async fn start() -> Self {
        let worst_us = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (worst, stopped) = (worst_us.clone(), stop.clone());
        let task = tokio::spawn(async move {
            let mut last = thread_cpu();
            let mut tick = tokio::time::interval(Duration::from_millis(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let now = thread_cpu();
                let step = u64::try_from(now.saturating_sub(last).as_micros()).unwrap_or(u64::MAX);
                worst.fetch_max(step, Ordering::SeqCst);
                last = now;
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self {
            worst_us,
            stop,
            task,
        }
    }

    /// One more reading after the work has ended, then the greatest step between two readings.
    async fn end(self) -> Duration {
        self.stop.store(true, Ordering::SeqCst);
        self.task.await.unwrap();
        Duration::from_micros(self.worst_us.load(Ordering::SeqCst))
    }
}

/// The store of a 256 MiB file on a runtime of one thread. Before, one poll of the store read the whole file and hashed it twice on
/// that thread.
#[tokio::test]
async fn b860_store_never_holds_a_runtime_thread() {
    within(Duration::from_secs(300), async {
        let dir = scratch("b860-store-cpu");
        let file = big_file(&dir, "snapshot.bundle", LARGE);
        let bucket = Bucket::start(Plan::default());
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(240));
        let ticker = Ticker::start().await;
        let pin = objects
            .put_file_receipt(
                &bucket.uri("runs/R/snapshot.bundle"),
                &stand_in_creds(),
                file.path(),
            )
            .await
            .unwrap();
        let held = ticker.end().await;
        assert!(
            held <= HELD_AT_MOST,
            "TRIGGER PRESENT: while a file of {LARGE} bytes was stored, the runtime thread was busy for {held:?} between two readings \
             of a 5 ms ticker"
        );
        // The bytes that arrived are the file, and the record of the write names them.
        let puts = bucket.puts();
        assert_eq!(puts.len(), 1, "{puts:?}");
        assert_eq!(puts[0].bytes, LARGE);
        assert_eq!(puts[0].signed_sha256, puts[0].sha256_hex);
        assert_eq!(pin.sha256, puts[0].sha256_hex);
        assert_eq!(pin.version_id.as_deref(), Some("stand-in-1"));
    })
    .await;
}

/// The same store: agentd's memory does not grow with the file. Before, the file was read whole (and the body was a second copy).
#[tokio::test]
async fn b860_store_memory_stays_flat() {
    within(Duration::from_secs(300), async {
        let dir = scratch("b860-store-mem");
        let file = big_file(&dir, "snapshot.bundle", LARGE);
        let bucket = Bucket::start(Plan::default());
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(240));
        let before = peak_memory();
        let pin = objects
            .put_file_receipt(
                &bucket.uri("runs/R/snapshot.bundle"),
                &stand_in_creds(),
                file.path(),
            )
            .await
            .unwrap();
        let grew = peak_memory().saturating_sub(before);
        assert!(
            grew <= GROWS_AT_MOST,
            "TRIGGER PRESENT: storing a file of {LARGE} bytes raised the peak memory of the process by {grew} bytes"
        );
        assert_eq!(pin.sha256, bucket.puts()[0].sha256_hex);
    })
    .await;
}

/// A download of 256 MiB: the same two bounds, the file holds the bytes that were served, and no part file is left. Before, the whole
/// answer was held in memory and written by a runtime thread.
#[tokio::test]
async fn b860_get_to_file_streams() {
    within(Duration::from_secs(300), async {
        let dir = scratch("b860-get");
        let dest = dir.join("restore/repo.bundle");
        let bucket = Bucket::start(Plan {
            get_bytes: LARGE,
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(240));
        let before = peak_memory();
        let ticker = Ticker::start().await;
        objects
            .get_to_file(
                &bucket.uri("runs/R/input/repo.bundle"),
                &stand_in_creds(),
                &dest,
            )
            .await
            .unwrap();
        let held = ticker.end().await;
        let grew = peak_memory().saturating_sub(before);
        let fetched = b860_support::TestFile(dest.clone());
        let (sha, bytes) = hash_of(fetched.path());
        let served = bucket.gets();
        assert_eq!(served.len(), 1, "{served:?}");
        assert_eq!((bytes, sha.as_str()), (LARGE, served[0].sha256_hex.as_str()));
        assert!(
            grew <= GROWS_AT_MOST,
            "TRIGGER PRESENT: fetching an object of {LARGE} bytes raised the peak memory of the process by {grew} bytes"
        );
        assert!(
            held <= HELD_AT_MOST,
            "TRIGGER PRESENT: while an object of {LARGE} bytes was fetched, the runtime thread was busy for {held:?} between two \
             readings of a 5 ms ticker"
        );
        assert!(
            !dir.join("restore/repo.bundle.part").exists(),
            "no part file is left"
        );
    })
    .await;
}

// ---- the limits of a streamed request ------------------------------------------------------------------------------------------------

/// The bucket takes one megabyte of an upload and then reads nothing more. The file is larger than the buffers of both ends of the
/// connection, so the body cannot have ended before the stall. The request is given up when nothing moved for the no-progress limit.
/// A bucket that reads slowly is not given up by that limit: its request ends at the whole-time limit.
#[tokio::test]
async fn b860_put_gives_up_when_nothing_moves() {
    within(Duration::from_secs(120), async {
        let dir = scratch("b860-put-stall");
        let file = big_file(&dir, "snapshot.bundle", 64 * MIB);
        let stalled = Bucket::start(Plan {
            stop_reading_after: Some(MIB),
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(stalled.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(60))
            .with_no_progress(Duration::from_secs(2));
        let began = Instant::now();
        let e = objects
            .put_file_receipt(
                &stalled.uri("runs/R/snapshot.bundle"),
                &stand_in_creds(),
                file.path(),
            )
            .await
            .expect_err("the bucket stopped reading");
        assert!(
            stalled.holding(),
            "TRIGGER PRESENT is not shown: the bucket did not stop reading"
        );
        assert!(e.contains("no progress for 2 s"), "{e}");
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "the stalled request was given up long before its whole-time limit: {:?}",
            began.elapsed()
        );
        // 64 KiB every 10 ms: the upload moves, and 64 MiB of it do not end within six seconds (they need ten). The sender sees a
        // slow reader in steps: a connection takes more bytes only when a third of its send buffer is free again (up to 4 MiB on
        // this system), so at this pace the count of the bytes that left moves every 70 to 210 ms. The no-progress limit here is two
        // seconds: ten of those steps, so a stand-in that a loaded host holds up is not taken for a stall, and a third of the
        // whole-time limit, so a limit that counted from the start of the request would show.
        let slow = Bucket::start(Plan {
            read_slowly: Some((64 * 1024, Duration::from_millis(10))),
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(slow.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(6))
            .with_no_progress(Duration::from_secs(2));
        let e = objects
            .put_file_receipt(
                &slow.uri("runs/R/snapshot.bundle"),
                &stand_in_creds(),
                file.path(),
            )
            .await
            .expect_err("64 MiB at 64 KiB every 10 ms do not end within 6 s");
        assert!(
            e.contains("timed out") && !e.contains("no progress"),
            "a bucket that reads slowly is not given up by the no-progress limit: {e}"
        );
        assert!(
            slow.moved() >= MIB,
            "the upload moved: {} bytes",
            slow.moved()
        );
    })
    .await;
}

/// The bucket sends the head of an answer and one megabyte of a body of 64 MiB, and then nothing. Before, the download waited without
/// a limit: the limit of a request ended when the head of the answer had come.
#[tokio::test]
async fn b860_get_gives_up_when_nothing_arrives() {
    within(Duration::from_secs(120), async {
        let dir = scratch("b860-get-stall");
        let dest = dir.join("repo.bundle");
        let bucket = Bucket::start(Plan {
            get_bytes: 64 * MIB,
            stop_sending_after: Some(MIB),
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(60))
            .with_no_progress(Duration::from_secs(2));
        let began = Instant::now();
        let e = objects
            .get_to_file(
                &bucket.uri("runs/R/input/repo.bundle"),
                &stand_in_creds(),
                &dest,
            )
            .await
            .expect_err("the bucket stopped sending");
        assert!(
            bucket.holding(),
            "TRIGGER PRESENT is not shown: the bucket did not stop sending"
        );
        assert!(e.contains("no progress for 2 s"), "{e}");
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "{:?}",
            began.elapsed()
        );
        assert!(!dest.exists(), "a download that did not end leaves no file");
        assert!(
            !dir.join("repo.bundle.part").exists(),
            "a download that did not end leaves no part file"
        );
    })
    .await;
}

/// A body that moves has the time its size needs, whatever the whole-time limit of the request: the limit of a request was made for
/// a request, and before this change the body of a download had no limit at all. Two megabytes, sent over about five seconds, come
/// through a client whose requests have 3 s (the head of the answer must come within that time, so it is seconds, not a moment).
#[tokio::test]
async fn b860_a_download_that_moves_has_the_time_its_size_needs() {
    within(Duration::from_secs(120), async {
        let dir = scratch("b860-get-slow");
        let dest = dir.join("repo.bundle");
        let bucket = Bucket::start(Plan {
            get_bytes: 2 * MIB,
            send_slowly: Some((64 * 1024, Duration::from_millis(150))),
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(3));
        let began = Instant::now();
        objects
            .get_to_file(
                &bucket.uri("runs/R/input/repo.bundle"),
                &stand_in_creds(),
                &dest,
            )
            .await
            .unwrap();
        assert!(
            began.elapsed() > Duration::from_secs(3),
            "TRIGGER PRESENT is not shown: the body came within the limit of a request: {:?}",
            began.elapsed()
        );
        let (sha, bytes) = hash_of(&dest);
        // The stand-in notes a GET after the pause that follows its last piece: the note is waited for.
        let noted = Instant::now();
        while bucket.gets().is_empty() && noted.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let served = bucket.gets();
        assert_eq!(served.len(), 1, "{served:?}");
        assert_eq!(
            (bytes, sha.as_str()),
            (2 * MIB, served[0].sha256_hex.as_str())
        );
        // The restore of a session is one download without a second try. Over a path of half a megabyte a second a repository of
        // 2 GiB took 68 minutes to come before the body had a limit, and came through: it still has that time, and more.
        assert!(
            agentd::http::body_time(2048 * MIB) >= Duration::from_secs(68 * 60),
            "TRIGGER PRESENT: a download of 2 GiB that moves at half a megabyte a second is cut after {:?}",
            agentd::http::body_time(2048 * MIB)
        );
        assert_eq!(
            agentd::http::body_time(2 * MIB),
            Duration::from_secs(62),
            "30 s and one second for each 64 KiB"
        );
    })
    .await;
}

/// The head of the answer to a download has the whole-time limit of its request, as it had before the body was streamed. Nothing
/// of a GET moves before its head comes, so the no-progress limit must not count that wait: a head that the bucket sends after
/// 40 s came through before, and a restore has no second try. Here the head comes after 5 s to a client whose no-progress limit is
/// 2 s and whose requests have 60 s, and the download comes through. A head that does not come within the request's limit ends
/// the request with `timed out` and leaves no file.
#[tokio::test]
async fn b860_the_head_of_a_download_has_the_requests_limit() {
    within(Duration::from_secs(120), async {
        let dir = scratch("b860-get-head");
        let dest = dir.join("repo.bundle");
        let bucket = Bucket::start(Plan {
            get_bytes: MIB,
            head_after: Some(Duration::from_secs(5)),
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(60))
            .with_no_progress(Duration::from_secs(2));
        let got = objects
            .get_to_file(
                &bucket.uri("runs/R/input/repo.bundle"),
                &stand_in_creds(),
                &dest,
            )
            .await;
        assert!(
            got.is_ok(),
            "TRIGGER PRESENT: a download whose head came after 5 s was given up, though its request has 60 s: {got:?}"
        );
        let (sha, bytes) = hash_of(&dest);
        let served = bucket.gets();
        assert_eq!((bytes, sha.as_str()), (MIB, served[0].sha256_hex.as_str()));

        // A head that does not come within the limit of its request.
        let late = Bucket::start(Plan {
            get_bytes: MIB,
            head_after: Some(Duration::from_secs(20)),
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(late.endpoint.clone()))
            .with_request_timeout(Duration::from_secs(2))
            .with_no_progress(Duration::from_secs(10));
        let dest = dir.join("late.bundle");
        let e = objects
            .get_to_file(
                &late.uri("runs/R/input/repo.bundle"),
                &stand_in_creds(),
                &dest,
            )
            .await
            .expect_err("the head did not come within 2 s");
        assert!(e.contains("timed out") && !e.contains("no progress"), "{e}");
        assert!(!dest.exists(), "a download that did not start leaves no file");
        assert!(
            !dir.join("late.bundle.part").exists(),
            "a download that did not start leaves no part file"
        );
    })
    .await;
}

/// A download whose file cannot get its name fails and leaves no part file behind: the part file is removed on every way out but
/// the rename that went through. Here a folder that holds a file stands where the download is to go.
#[tokio::test]
async fn b860_a_download_that_cannot_be_named_leaves_no_part_file() {
    within(Duration::from_secs(120), async {
        let dir = scratch("b860-get-name");
        let dest = dir.join("repo.bundle");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("in-the-way"), "a file\n").unwrap();
        let bucket = Bucket::start(Plan {
            get_bytes: MIB,
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()));
        let e = objects
            .get_to_file(
                &bucket.uri("runs/R/input/repo.bundle"),
                &stand_in_creds(),
                &dest,
            )
            .await
            .expect_err("a folder that holds a file stands where the download is to go");
        assert!(dest.join("in-the-way").exists(), "the folder is as it was");
        assert!(
            !dir.join("repo.bundle.part").exists(),
            "TRIGGER PRESENT: the download failed ({e}) and left its part file behind"
        );
    })
    .await;
}

/// The first answer is 500, the second 200: both tries sent the same bytes under the same hash, and the file was hashed once.
#[tokio::test]
async fn b860_retry_sends_the_same_bytes() {
    within(Duration::from_secs(120), async {
        let dir = scratch("b860-retry");
        let file = big_file(&dir, "snapshot.bundle", 3 * MIB);
        let (sha, _) = hash_of(file.path());
        let bucket = Bucket::start(Plan {
            answer_500: 1,
            ..Plan::default()
        });
        let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()));
        let hashed_before = agentd::s3::digests_made();
        let pin = objects
            .put_file_retrying_receipt(
                &bucket.uri("runs/R/snapshot.bundle"),
                &stand_in_creds(),
                file.path(),
                3,
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        let puts = bucket.puts();
        assert_eq!(
            puts.iter().map(|p| p.status).collect::<Vec<_>>(),
            vec![500, 200],
            "{puts:?}"
        );
        for put in &puts {
            assert_eq!(
                (put.bytes, put.sha256_hex.as_str()),
                (3 * MIB, sha.as_str())
            );
            assert_eq!(put.signed_sha256, sha);
        }
        assert_eq!(pin.sha256, sha);
        assert_eq!(pin.version_id.as_deref(), Some("stand-in-2"));
        assert_eq!(
            agentd::s3::digests_made() - hashed_before,
            1,
            "the file is hashed once for all the tries"
        );
    })
    .await;
}

/// The rules of a create-only write hold with a streamed body of several frames: a free key is stored with the checksum of the bytes,
/// a key another writer took is left alone, and an object that a lost answer committed is told from another writer's by its checksum.
/// `tests/s3_create_only.rs` holds every case of those rules; this test holds that the stream did not change them and that the file is
/// hashed once for all the tries of one write.
#[tokio::test(flavor = "multi_thread")]
async fn b860_create_only_put_keeps_its_rules() {
    use agentd::s3::PutNew;
    use direct_support::{BUCKET, FakeAws, PutFailure, sha256_b64};
    within(Duration::from_secs(120), async {
        let aws = FakeAws::start().await;
        let dir = scratch("b860-create-only");
        // Five frames of 64 KiB, under the 2 MiB a request of the fake store may carry.
        let file = big_file(&dir, "bundle", 300 * 1024);
        let (sha, _) = hash_of(file.path());
        let bytes = std::fs::read(file.path()).unwrap();
        let s3 = S3Objects::new("us-west-2".into(), Some(aws.base.clone()))
            .with_request_timeout(Duration::from_secs(5));
        let c = stand_in_creds();
        let pause = Duration::from_millis(10);
        let uri = |key: &str| format!("s3://{BUCKET}/{key}");

        // A free key.
        let free = "runs/R1/agents/direct/turns/1/bundle";
        let hashed_before = agentd::s3::digests_made();
        let (put, pin) = s3
            .put_file_new_retrying_receipt(&uri(free), &c, file.path(), 3, pause)
            .await
            .unwrap();
        assert_eq!(put, PutNew::Stored);
        assert_eq!(pin.unwrap().sha256, sha);
        assert_eq!(aws.get(free).as_deref(), Some(bytes.as_slice()));
        assert_eq!(aws.checksum_of(free), Some(sha256_b64(&bytes)));

        // A key another writer took before agentd wrote.
        let taken = "runs/R2/agents/direct/turns/1/bundle";
        aws.put(taken, b"another writer's object".to_vec());
        let (put, pin) = s3
            .put_file_new_retrying_receipt(&uri(taken), &c, file.path(), 3, pause)
            .await
            .unwrap();
        assert_eq!((put, pin), (PutNew::Exists, None));
        assert_eq!(
            aws.get(taken).as_deref(),
            Some(&b"another writer's object"[..])
        );

        // The object is committed and its answer is lost: the retry meets a 412, and the HEAD shows agentd's own checksum.
        let lost = "runs/R3/agents/direct/turns/1/bundle";
        aws.fail_puts_after_store(lost, 1, PutFailure::ResetBody);
        let (put, pin) = s3
            .put_file_new_retrying_receipt(&uri(lost), &c, file.path(), 3, pause)
            .await
            .unwrap();
        assert_eq!(put, PutNew::Stored);
        assert_eq!(pin.unwrap().sha256, sha);
        assert_eq!(aws.put_attempts(lost), 2);
        assert_eq!(aws.head_attempts(lost), 1);
        assert_eq!(
            agentd::s3::digests_made() - hashed_before,
            3,
            "one hash for each of the three writes, whatever the number of their tries"
        );
    })
    .await;
}

// ---- a whole turn: a git that is ended takes what it started with it ---------------------------------------------------------------

/// Every version of `snapshot.json` in the bucket, oldest first.
fn records(d: &Direct) -> Vec<serde_json::Value> {
    d.aws
        .versions_of(&keys::snapshot_json_key(RUN))
        .iter()
        .map(|(_, b)| serde_json::from_slice(b).unwrap())
        .collect()
}

/// Waits until `seen` holds, at most `limit`; `false` when it never did.
async fn wait_for(limit: Duration, mut seen: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while !seen() {
        if began.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// A stand-in for git, in a folder that leads the search path of `agentd serve`. While the file `mark` is there, `bundle create` starts
/// in the private repository of the copies starts a helper of its own (a `sleep`, as `git bundle create` starts `git pack-objects`),
/// writes the helper's process id to the file `pids` and waits for it. Every other call is the real git, the bundle of the session
/// included.
fn git_with_a_helper(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("helper-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mark = dir.join("bundle-create-starts-a-helper");
    let pids = dir.join("helper-pids");
    let script = format!(
        r#"#!/bin/sh
bundle=0
for a in "$@"; do if [ "$a" = bundle ]; then bundle=1; fi; done
if [ "$bundle" = 1 ] && [ -e '{mark}' ]; then
  case "$GIT_DIR" in
    */snapshot.git)
      sleep 600 &
      echo "$!" >> '{pids}'
      wait
      exit 1 ;;
  esac
fi
exec '{real}' "$@"
"#,
        mark = mark.display(),
        pids = pids.display(),
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, mark, pids)
}

/// The process ids in the file `pids`, one per line.
fn helpers(pids: &Path) -> Vec<u32> {
    std::fs::read_to_string(pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Ends, when the test ends, the helpers that the stand-in for git started and that still run, whatever way the test ends: a
/// helper is a `sleep 600`, and a test that failed before its last lines left it running for ten minutes.
struct EndHelpers(PathBuf);

impl Drop for EndHelpers {
    fn drop(&mut self) {
        for pid in helpers(&self.0) {
            if runs(pid) {
                let _ = std::process::Command::new("kill")
                    .args(["-s", "KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}

/// Whether the process `pid` runs (a process that has ended and waits to be collected does not).
fn runs(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // `<pid> (<name>) <state> ...`: the state follows the last `)`.
        stat.rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

/// A look of the copy task is in `git bundle create`, which has started a helper, when the engine stops: the copy task is aborted and
/// its look is dropped. Then the copy of the end is in `git bundle create` when its budget gives it up. The engine is silent for
/// 20 s, of which a look has 15 s to reach its `git bundle create`, and the copy of the end has a budget of 5 s to reach its own
/// through about six starts of git: room for a loaded host. What the test finds is asserted with its own limit: 2 s after the turn
/// ended no helper runs. Before, only the git that agentd
/// had started was ended, and what that git had started ran on: on a large repository `git pack-objects` packed for seconds beside the
/// end of the session.
#[tokio::test(flavor = "multi_thread")]
async fn b860_abort_ends_git_and_what_git_started() {
    let scratch = scratch("b860-git-group");
    let (bin, mark, pids) = git_with_a_helper(&scratch);
    let _no_helper_is_left = EndHelpers(pids.clone());
    let d = Direct::start_process(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "20".into())],
        agentd_env: vec![
            ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "300".to_string()),
            ("AGENTD_DIRECT_SNAPSHOT_FLOOR_MS", "50".to_string()),
            ("AGENTD_DIRECT_FINAL_COPY_BUDGET_MS", "5000".to_string()),
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
        ],
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        wait_for(Duration::from_secs(30), || !records(&d).is_empty()).await,
        "the copy after the engine's first tool call"
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // From now on `git bundle create` starts a helper and waits for it, and the tree holds a file that no copy has.
    std::fs::write(&mark, "set\n").unwrap();
    std::fs::write(
        d.worktree().join("late.txt"),
        "written after the last copy\n",
    )
    .unwrap();
    assert!(
        wait_for(Duration::from_secs(15), || !helpers(&pids).is_empty()).await,
        "TRIGGER PRESENT is not shown: no look of the copy task reached git bundle create while the engine worked"
    );
    let in_the_look = helpers(&pids);
    // The engine stops; the copy task is aborted in that look; the copy of the end is given up in its own git bundle create.
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let all = helpers(&pids);
    assert!(
        all.len() > in_the_look.len(),
        "TRIGGER PRESENT is not shown: the copy of the end did not reach git bundle create: {all:?}"
    );
    let gone = wait_for(Duration::from_secs(2), || !all.iter().any(|pid| runs(*pid))).await;
    // What a git of this test started and the product did not end is ended when the test ends, by `EndHelpers`.
    let left: Vec<u32> = all.iter().copied().filter(|pid| runs(*pid)).collect();
    assert!(
        gone,
        "TRIGGER PRESENT: 2 s after the turn ended, {} of {} helpers that a git of the copies had started still ran (the look's: \
         {in_the_look:?}; all: {all:?}; left: {left:?})",
        left.len(),
        all.len()
    );
}
