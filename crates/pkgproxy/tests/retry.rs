//! A transient failure of the upstream does not reach the engine's package tool. The proxy sends an idempotent request (GET and HEAD
//! are the only methods it accepts) again, at most twice, after a jittered wait, when the upstream failed before it sent a usable
//! answer: a connection reset or refused, or a 429, 502, 503 or 504 head. It does not retry a wait that timed out (that would triple a
//! 60 s wait), a name that did not resolve, a certificate it does not trust, or any other status, and the last upstream answer goes
//! through unchanged when the retries run out. The credential goes only to the upstream's own origin on every try, as before.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn octets() -> (&'static str, &'static str) {
    ("Content-Type", "application/octet-stream")
}

/// An upstream that answers `first` to its first `fails` requests and a 200 with `ok` after.
fn fails_then_ok(fails: usize, first: impl Fn() -> Reply + Send + Sync + 'static) -> Handler {
    let n = Arc::new(AtomicUsize::new(0));
    handler(move |_| {
        if n.fetch_add(1, Ordering::SeqCst) < fails {
            first()
        } else {
            Reply::ok("ok").header(octets().0, octets().1)
        }
    })
}

#[tokio::test]
async fn a_503_head_is_retried_and_the_client_sees_the_200_that_follows() {
    let r = rig(
        fails_then_ok(1, || Reply::status(503)),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/simple/x").await;
    assert_eq!((resp.status, resp.body.as_slice()), (200, b"ok".as_slice()));
    assert_eq!(r.ca.count(), 2, "one failed try and one that answered");
    assert_eq!(
        r.proxy.snapshot().statuses.get(&200),
        Some(&1),
        "the client was answered once"
    );
}

#[tokio::test]
async fn a_connection_dropped_before_the_answer_is_retried() {
    let r = rig(fails_then_ok(1, Reply::aborted), nothing(), nothing()).await;
    let resp = r.get("/npm/left-pad").await;
    assert_eq!((resp.status, resp.body.as_slice()), (200, b"ok".as_slice()));
    assert_eq!(r.ca.count(), 2);
}

#[tokio::test]
async fn only_429_502_503_and_504_are_retried() {
    for (status, retried) in [
        (429u16, true),
        (502, true),
        (503, true),
        (504, true),
        (500, false),
        (404, false),
        (403, false),
        (401, false),
        (400, false),
        (304, false),
    ] {
        let r = rig(
            fails_then_ok(1, move || Reply::status(status)),
            nothing(),
            nothing(),
        )
        .await;
        let resp = r.get("/pypi/simple/x").await;
        if retried {
            assert_eq!(resp.status, 200, "{status}");
            assert_eq!(r.ca.count(), 2, "{status} is retried");
        } else {
            assert_eq!(resp.status, status, "{status} passes through");
            assert_eq!(r.ca.count(), 1, "{status} is not retried");
        }
    }
}

#[tokio::test]
async fn the_retries_stop_at_two_and_the_last_upstream_answer_goes_through_unchanged() {
    let r = rig(
        always(Reply::status(503).header("Content-Type", "text/plain")),
        nothing(),
        nothing(),
    )
    .await;
    let t = Instant::now();
    let resp = r.get("/pypi/simple/x").await;
    let took = t.elapsed();
    assert_eq!(resp.status, 503, "the upstream's own answer");
    assert_eq!(r.ca.count(), 3, "the first try and two retries, no more");
    assert!(
        took >= Duration::from_millis(700),
        "the waits were taken: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(r.proxy.snapshot().statuses.get(&503), Some(&1));
}

#[tokio::test]
async fn a_reset_that_does_not_stop_ends_as_the_same_502_as_before_after_three_tries() {
    let r = rig(always(Reply::aborted()), nothing(), nothing()).await;
    let resp = r.get("/pypi/x").await;
    assert_eq!(resp.status, 502);
    assert!(
        resp.text().starts_with("upstream error:"),
        "{}",
        resp.text()
    );
    assert!(!resp.text().contains(CA_HOST), "no host in the error");
    assert_eq!(r.ca.count(), 3);
    assert_eq!(r.get("/nope").await.status, 403, "still serving");
}

#[tokio::test]
async fn the_waits_are_jittered_and_the_first_is_short() {
    // The first wait is 250 ms plus up to half of it, the second twice that: 0.75 s to 1.125 s for both.
    let mut took = Vec::new();
    for _ in 0..4 {
        let r = rig(always(Reply::status(503)), nothing(), nothing()).await;
        let t = Instant::now();
        r.get("/pypi/x").await;
        took.push(t.elapsed());
    }
    for t in &took {
        assert!(
            *t >= Duration::from_millis(700) && *t < Duration::from_millis(2500),
            "{took:?}"
        );
    }
    let min = took.iter().min().unwrap();
    let max = took.iter().max().unwrap();
    assert!(
        *max > *min,
        "four runs did not all wait the same time: {took:?}"
    );
}

#[tokio::test]
async fn a_wait_for_the_head_that_times_out_is_not_retried() {
    let mut o = Opts::new(
        always(Reply::ok("late").delayed(Duration::from_secs(30))),
        nothing(),
        nothing(),
    );
    o.idle_timeout = Duration::from_millis(400);
    let r = rig_with(o).await;
    let t = Instant::now();
    let resp = r.get("/pypi/slow").await;
    assert_eq!(resp.status, 502);
    assert!(
        resp.text().starts_with("upstream error: timeout"),
        "{}",
        resp.text()
    );
    assert!(
        t.elapsed() < Duration::from_millis(1200),
        "one wait, not three: {:?}",
        t.elapsed()
    );
    assert_eq!(r.ca.count(), 1);
}

#[tokio::test]
async fn a_head_request_is_retried_like_a_get() {
    let r = rig(
        fails_then_ok(1, || Reply::status(502)),
        nothing(),
        nothing(),
    )
    .await;
    let resp = head(r.addr(), "/pypi/simple/x").await;
    assert_eq!(resp.status, 200);
    assert_eq!(r.ca.count(), 2);
    assert!(r.ca.seen().iter().all(|s| s.method == "HEAD"));
}

#[tokio::test]
async fn the_credential_goes_to_the_upstream_origin_on_every_try_and_to_no_one_else() {
    // CodeArtifact fails once, then redirects to the file host, which fails once and then serves: the token is on both CodeArtifact
    // requests and on none to the file host.
    let ca_seen = Arc::new(AtomicUsize::new(0));
    let ca = {
        let n = ca_seen.clone();
        handler(move |_| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::status(503)
            } else {
                Reply::status(302).header("Location", &format!("https://{S3_HOST}/blob/file.whl"))
            }
        })
    };
    let r = rig(ca, fails_then_ok(1, || Reply::status(503)), nothing()).await;
    let resp = r.get("/pypi/files/x.whl").await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(r.ca.count(), 2);
    assert_eq!(r.s3.count(), 2);
    let want = basic_for(&r.token);
    for s in r.ca.seen() {
        assert_eq!(s.header("authorization"), Some(want.as_str()), "{s:?}");
    }
    for s in r.s3.seen() {
        assert_eq!(s.header("authorization"), None, "{s:?}");
    }
}

#[tokio::test]
async fn a_retry_is_one_log_line_without_the_token_or_the_host() {
    let r = rig(
        fails_then_ok(1, || Reply::status(503)),
        nothing(),
        nothing(),
    )
    .await;
    assert_eq!(r.get("/pypi/simple/x").await.status, 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let lines = r.log.lines();
    let retry: Vec<&String> = lines.iter().filter(|l| l.contains(" retry ")).collect();
    assert_eq!(retry.len(), 1, "{lines:?}");
    assert!(
        retry[0].starts_with("pkgproxy pypi retry 1 "),
        "{}",
        retry[0]
    );
    assert!(retry[0].contains("503"), "{}", retry[0]);
    for l in &lines {
        assert!(!l.contains(&r.token) && !l.contains(CA_HOST), "{l}");
    }
    assert!(
        lines.iter().any(|l| l.starts_with("pkgproxy pypi 200 ")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn the_request_deadline_still_bounds_the_tries() {
    let mut o = Opts::new(always(Reply::status(503)), nothing(), nothing());
    o.request_timeout = Duration::from_millis(300);
    let r = rig_with(o).await;
    let t = Instant::now();
    let resp = r.get("/pypi/x").await;
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    assert!(resp.status == 503 || resp.status == 502, "{}", resp.status);
    assert!(r.ca.count() <= 3);
}
