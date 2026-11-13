//! The memory bound of the rewrite cap.
//! A text answer is read whole and rewritten into a second buffer, so one slot holds at most
//! the cap twice, 128 MiB; four slots hold 512 MiB at worst, 6.25 percent of the 8 GB an AgentCore Runtime session has (quota table
//! "Maximum hardware allocation per session", 2vCPU/8GB). This file holds one test, so its process sees nothing else: it fires four
//! answers of 60 MiB at once, reads and discards them, and checks that the resident rise stays inside that bound plus the test
//! rig's own copy of the document.
#![allow(clippy::unwrap_used)]

mod common;
use bytes::Bytes;
use common::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A JSON document of about `size` bytes whose tarball links point at the upstream.
fn packument(size: usize) -> Bytes {
    let entry = format!(
        r#""6.2.2":{{"name":"vite","version":"6.2.2","dist":{{"tarball":"https://{CA_HOST}/npm/packages/vite/-/vite-6.2.2.tgz"}},"padding":"{}"}},"#,
        "x".repeat(900)
    );
    let mut out = String::with_capacity(size + entry.len());
    out.push_str(r#"{"name":"vite","versions":{"#);
    while out.len() < size {
        out.push_str(&entry);
    }
    out.push_str(r#""9.9.9":{}}}"#);
    Bytes::from(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_60_mib_packuments_at_once_stay_inside_four_slots_of_two_buffers_of_the_cap() {
    let doc = packument(60 * 1024 * 1024);
    let big = doc.clone();
    let r = rig(
        handler(move |s| {
            if s.target.contains("small") {
                Reply::ok(r#"{"name":"small"}"#).header("Content-Type", "application/json")
            } else {
                Reply::ok(big.clone())
                    .header("Content-Type", "application/json")
                    .chunked()
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    // One small request first: pools, TLS tables and buffers exist before the baseline (a large one would leave the allocator's
    // high-water mark behind and hide the rise).
    let warm = r.get("/npm/small").await;
    assert_eq!(warm.status, 200, "warm-up: {}", warm.text());
    drop(warm);

    let baseline = vm_rss_kib();
    let peak = Arc::new(AtomicU64::new(baseline));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (peak.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                peak.fetch_max(vm_rss_kib(), Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let addr = r.addr();
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(&request("GET", "/npm/vite", addr, &[]))
                .await
                .unwrap();
            let (mut got, mut first) = (0u64, Vec::new());
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = s.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                if first.len() < 64 {
                    first.extend_from_slice(&buf[..n.min(64)]);
                }
                got += n as u64;
            }
            (got, String::from_utf8_lossy(&first).to_string())
        }));
    }
    let mut totals = Vec::new();
    for c in clients {
        totals.push(c.await.unwrap());
    }
    stop.store(true, Ordering::SeqCst);
    sampler.await.unwrap();
    for (got, first) in &totals {
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(*got > 59_000_000, "the whole answer arrived: {got}");
    }
    let rise_mib = peak.load(Ordering::SeqCst).saturating_sub(baseline) / 1024;
    eprintln!("4 x 60 MiB packuments at once: resident rise {rise_mib} MiB");
    // Four slots of (the cap + its rewrite) = 512 MiB, plus some slack for the connections and the allocator's rounding.
    assert!(
        rise_mib < 4 * 2 * 64 + 128,
        "resident memory rose by {rise_mib} MiB: more than four slots of two 64 MiB buffers"
    );
}
