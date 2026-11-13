#![allow(clippy::unwrap_used)]

mod common;
use bytes::Bytes;
use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn octets() -> (&'static str, &'static str) {
    ("Content-Type", "application/octet-stream")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_200_mb_body_streams_through_with_a_bounded_memory_rise() {
    let total: u64 = 200 * 1000 * 1000;
    let (k, v) = octets();
    let r = rig(
        always(Reply::ok("").plan(Plan::Gen { total }).header(k, v)),
        nothing(),
        nothing(),
    )
    .await;
    // Warm up: one small request so the connection pools, TLS tables and buffers exist.
    let warm = rig(always(Reply::ok("w").header(k, v)), nothing(), nothing()).await;
    assert_eq!(warm.get("/pypi/w").await.status, 200);
    drop(warm);

    let baseline = vm_rss_kib();
    let peak = Arc::new(AtomicU64::new(baseline));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (peak.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                peak.fetch_max(vm_rss_kib(), Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };

    let addr = r.addr();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&request("GET", "/pypi/big.bin", addr, &[]))
        .await
        .unwrap();
    // Read and discard: head first, then count the body.
    let mut got: u64 = 0;
    let mut head_done = false;
    let mut head = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    let start = Instant::now();
    loop {
        let n = s.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        if head_done {
            got += n as u64;
        } else {
            head.extend_from_slice(&buf[..n]);
            if let Some(i) = find(&head, b"\r\n\r\n") {
                head_done = true;
                got += (head.len() - i - 4) as u64;
                let text = String::from_utf8_lossy(&head[..i]).to_string();
                assert!(text.starts_with("HTTP/1.1 200"), "{text}");
                assert!(
                    text.to_ascii_lowercase()
                        .contains(&format!("content-length: {total}")),
                    "{text}"
                );
                head.clear();
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
    sampler.await.unwrap();
    assert_eq!(got, total, "the whole body arrived");
    let rise = peak.load(Ordering::SeqCst).saturating_sub(baseline);
    eprintln!(
        "200 MB in {:?}; resident rise {} KiB",
        start.elapsed(),
        rise
    );
    assert!(
        rise < 32 * 1024,
        "resident memory rose by {rise} KiB (limit 32 MiB): the body is being buffered"
    );
    let st = r.proxy.snapshot();
    assert!(st.bytes >= total, "{st:?}");
}

#[tokio::test]
async fn an_upstream_that_never_answers_gives_502_after_the_idle_timeout() {
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
        resp.text().starts_with("upstream error:"),
        "{}",
        resp.text()
    );
    assert!(
        !resp.text().contains(CA_HOST) && !resp.text().contains("http"),
        "no URL in the error: {}",
        resp.text()
    );
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn an_upstream_that_stalls_mid_body_cuts_the_client_off_after_the_idle_timeout() {
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::ThenStall(Bytes::from_static(b"first-part")))
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "1000"),
        ),
        nothing(),
        nothing(),
    );
    o.idle_timeout = Duration::from_millis(400);
    let r = rig_with(o).await;
    let t = Instant::now();
    let resp = r.get("/pypi/stall").await;
    assert_eq!(resp.status, 200, "the head was already sent");
    assert!(
        !resp.complete,
        "the client must see a short body, never a padded or invented one"
    );
    assert!(resp.body.len() < 1000);
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[tokio::test]
async fn a_reset_in_the_middle_of_a_body_is_a_short_body_not_a_complete_one() {
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::ThenError(Bytes::from_static(b"half")))
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "100"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/reset").await;
    assert!(
        !resp.complete,
        "{} {:?} body {:?}",
        resp.status, resp.headers, resp.body
    );
    assert!(resp.body.len() < 100);
    // The proxy is still serving.
    assert_eq!(r.get("/nope").await.status, 403);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        r.log
            .lines()
            .iter()
            .any(|l| l.starts_with("pkgproxy pypi 200 ")),
        "{:?}",
        r.log.lines()
    );
}

#[tokio::test]
async fn an_upstream_that_drops_the_connection_before_answering_gives_502() {
    let r = rig(always(Reply::aborted()), nothing(), nothing()).await;
    let resp = r.get("/pypi/x").await;
    assert_eq!(resp.status, 502);
    assert!(
        resp.text().starts_with("upstream error:"),
        "{}",
        resp.text()
    );
    assert_eq!(r.get("/nope").await.status, 403, "still serving");
}

#[tokio::test]
async fn an_upstream_with_an_untrusted_certificate_gives_502_and_is_never_trusted() {
    // The fake's certificate is signed by a CA the proxy does not know: build a second rig whose proxy trusts nothing useful.
    let r = rig(
        always(Reply::ok("x").header("Content-Type", "application/octet-stream")),
        nothing(),
        nothing(),
    )
    .await;
    // Same proxy config but a different trusted root: use the testing seam directly.
    let other = Pki::new(&["unrelated.example"]);
    let log = Arc::new(MemLog::default());
    let cfg = pkgproxy::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstreams: pkgproxy::codeartifact_upstreams(CA_HOST, REPO, pkgproxy::Secret::new(token())),
        expires_at_unix: None,
        max_connections: 64,
        request_timeout: Duration::from_secs(900),
        idle_timeout: Duration::from_secs(60),
        max_redirects: 5,
        log,
    };
    let net = pkgproxy::testing::TestNet {
        roots_der: vec![other.ca_der.clone()],
        dns: std::collections::BTreeMap::from([(CA_HOST.to_string(), r.ca.addr)]),
        clock: None,
    };
    let p = pkgproxy::testing::start_with(cfg, net).await.unwrap();
    let resp = get(p.addr(), "/pypi/x").await;
    assert_eq!(resp.status, 502, "{}", resp.text());
    assert!(
        resp.text().starts_with("upstream error:"),
        "{}",
        resp.text()
    );
    assert!(!resp.text().contains(&token()));
    assert_eq!(
        r.ca.count(),
        0,
        "no request was sent over an unverified connection"
    );
    p.stop().await;
}

#[tokio::test]
async fn an_unknown_upstream_host_gives_502_without_naming_it() {
    // Resolution fails for a host with no DNS entry (the test seam does not fall back to the system resolver).
    let r = rig(
        always(Reply::redirect(
            302,
            "https://no-such-host.s3.us-east-1.amazonaws.com/x",
        )),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/x").await;
    assert_eq!(resp.status, 502);
    assert!(!resp.text().contains("no-such-host"), "{}", resp.text());
}

#[tokio::test]
async fn the_request_time_limit_ends_a_transfer_that_never_pauses() {
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::Trickle {
                    gap: Duration::from_millis(100),
                    count: 100,
                })
                .header("Content-Type", "application/octet-stream"),
        ),
        nothing(),
        nothing(),
    );
    o.request_timeout = Duration::from_millis(900);
    o.idle_timeout = Duration::from_secs(5);
    let r = rig_with(o).await;
    let t = Instant::now();
    let resp = r.get("/pypi/endless").await;
    assert!(
        !resp.complete,
        "the transfer would take 10 s; the limit is 0.9 s"
    );
    assert!(resp.body.len() < 20, "{}", resp.body.len());
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
}

#[tokio::test]
async fn a_client_that_leaves_mid_transfer_frees_its_slot_and_the_proxy_keeps_serving() {
    let (k, v) = octets();
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::Gen {
                    total: 500 * 1000 * 1000,
                })
                .header(k, v),
        ),
        nothing(),
        nothing(),
    );
    o.max_connections = 4;
    let r = rig_with(o).await;
    for _ in 0..8 {
        let addr = r.addr();
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&request("GET", "/pypi/big", addr, &[]))
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0);
        drop(s); // leave while the proxy is mid-stream
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    // All 8 abandoned transfers released their permits (limit 4): four fresh connections must work.
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(TcpStream::connect(r.addr()).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(held);
    assert_eq!(r.get("/nope").await.status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_two_parallel_downloads_all_complete_exactly() {
    let (k, v) = octets();
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::Gen { total: 3_000_000 })
                .header(k, v),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let addr = r.addr();
    let mut tasks = Vec::new();
    for i in 0..32 {
        tasks.push(tokio::spawn(async move {
            get(addr, &format!("/pypi/f{i}")).await
        }));
    }
    for t in tasks {
        let resp = t.await.unwrap();
        assert_eq!(
            (resp.status, resp.body.len(), resp.complete),
            (200, 3_000_000, true)
        );
    }
    assert_eq!(r.proxy.snapshot().requests, 32);
}
