//! Edge and fuzz cases around the request parser: huge headers, many headers, a slow client, junk bytes, and a seeded fuzz of
//! request targets that checks the one invariant that matters: what reaches the upstream is under the configured base and
//! carries no dot segment, empty segment, NUL or backslash.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn ok() -> Handler {
    always(Reply::ok("fine").header("Content-Type", "application/octet-stream"))
}

#[tokio::test]
async fn a_huge_header_is_refused_and_the_proxy_keeps_serving() {
    let r = rig(ok(), nothing(), nothing()).await;
    let addr = r.addr();
    for size in [20 * 1024, 100 * 1024, 1024 * 1024] {
        let big = "a".repeat(size);
        let raw = format!(
            "GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Big: {big}\r\nConnection: close\r\n\r\n",
            addr.port()
        );
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(raw.as_bytes()).await; // the proxy may close before it has read everything
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
            .await
            .expect("closed or answered");
        let head = String::from_utf8_lossy(&out[..out.len().min(15)]).to_string();
        assert!(
            out.is_empty() || head.starts_with("HTTP/1.1 4"),
            "{size}: {head:?}"
        );
    }
    assert_eq!(
        r.ca.count(),
        0,
        "an oversized request never reaches the upstream"
    );
    assert_eq!(r.get("/pypi/x").await.status, 200);
}

#[tokio::test]
async fn too_many_headers_are_refused() {
    let r = rig(ok(), nothing(), nothing()).await;
    let addr = r.addr();
    let mut raw = format!(
        "GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n",
        addr.port()
    );
    for i in 0..500 {
        raw.push_str(&format!("X-H{i}: v\r\n"));
    }
    raw.push_str("Connection: close\r\n\r\n");
    let mut s = TcpStream::connect(addr).await.unwrap();
    let _ = s.write_all(raw.as_bytes()).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
        .await
        .expect("closed or answered");
    assert!(out.is_empty() || String::from_utf8_lossy(&out).starts_with("HTTP/1.1 4"));
    assert_eq!(r.ca.count(), 0);
    assert_eq!(r.get("/pypi/x").await.status, 200);
}

#[tokio::test]
async fn binary_junk_and_a_bad_http_version_do_not_reach_the_upstream_or_crash_the_proxy() {
    let r = rig(ok(), nothing(), nothing()).await;
    let addr = r.addr();
    let junk: Vec<Vec<u8>> = vec![
        vec![0u8; 64],
        (0..=255u8).collect(),
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03".to_vec(), // a TLS ClientHello at a plain port
        b"GET /pypi/x HTTP/9.9\r\nHost: x\r\n\r\n".to_vec(),
        b"GET /pypi/x\r\n\r\n".to_vec(),
        b"\r\n\r\n\r\n".to_vec(),
        b"GET /pypi/x HTTP/1.1\r\nHost 127.0.0.1\r\n\r\n".to_vec(),
        b"GET /pypi/\xff\xfe HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_vec(),
        b"GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: -1\r\n\r\n".to_vec(),
        b"GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".to_vec(),
    ];
    for j in junk {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(&j).await;
        let _ = s.shutdown().await;
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
            .await
            .expect("closed or answered");
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            out.is_empty() || text.starts_with("HTTP/1.1 4") || text.starts_with("HTTP/1.1 5"),
            "{text:?}"
        );
        assert!(
            !text.starts_with("HTTP/1.1 200"),
            "junk was served: {text:?}"
        );
    }
    assert_eq!(r.ca.count(), 0);
    assert_eq!(r.get("/pypi/x").await.status, 200);
}

#[tokio::test]
async fn a_client_that_sends_half_a_request_and_closes_changes_nothing() {
    let r = rig(ok(), nothing(), nothing()).await;
    for _ in 0..20 {
        let mut s = TcpStream::connect(r.addr()).await.unwrap();
        s.write_all(b"GET /pypi/x HTTP/1.1\r\nHost: 127.0.0")
            .await
            .unwrap();
        drop(s);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.ca.count(), 0);
    assert_eq!(r.get("/pypi/x").await.status, 200);
}

#[tokio::test]
async fn a_peer_that_is_not_loopback_is_closed_without_an_answer() {
    // `peer_is_loopback` is the check the accept loop applies; the listener only binds 127.0.0.1, so this is the unit form.
    for (addr, want) in [
        ("127.0.0.1:5000", true),
        ("127.255.255.254:1", true),
        ("[::1]:5000", true),
        ("10.0.0.5:5000", false),
        ("169.254.169.254:80", false),
        ("192.168.1.1:80", false),
        ("[::ffff:10.0.0.1]:80", false),
        ("[::ffff:127.0.0.1]:80", true),
        ("[fe80::1]:80", false),
    ] {
        let a: std::net::SocketAddr = addr.parse().unwrap();
        assert_eq!(pkgproxy::testing::peer_is_loopback(a), want, "{addr}");
    }
}

// -- seeded fuzz of the request target ---------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn percent_decode_once(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[tokio::test]
async fn fuzzed_request_targets_never_reach_the_upstream_outside_the_base() {
    let r = rig(
        handler(|_| Reply::ok("fine").header("Content-Type", "application/octet-stream")),
        nothing(),
        always(Reply::ok("go").header("Content-Type", "application/octet-stream")),
    )
    .await;
    let atoms = [
        "pypi", "npm", "maven", "go", "simple", "a", "b", "..", ".", "%2e%2e", "%2E", "%2f", "%2F",
        "%5c", "%00", "%", "%zz", "%41", "@scope", "%40scope", "x y", "é", "%C3%A9", "~", "!", "'",
        "(", ")", "*", ";", ":", "@", "=", "+", "$", ",", "-", "_", "?", "#", "&", "\\", "%255c",
        "%252e", "....", "...", "a.b", "-", "0",
    ];
    let seps = ["/", "/", "/", "//", "%2f", "/./", "/../"];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut served = 0;
    for case in 0..1500 {
        let mut path = String::from("/");
        path.push_str(rng.pick(&["pypi", "pypi", "npm", "maven", "go", "pypi", "bad"]));
        for _ in 0..(rng.next() % 6) {
            path.push_str(rng.pick(&seps));
            path.push_str(rng.pick(&atoms));
        }
        if rng.next().is_multiple_of(4) {
            path.push('?');
            path.push_str(rng.pick(&atoms));
        }
        // Spaces and non-ASCII are not valid in a request target; the parser refuses them with 400 or closes.
        let before = r.upstream_requests();
        let (ca_before, go_before) = (r.ca.count(), r.go.count());
        let raw = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            r.addr().port()
        );
        let resp = send_raw(r.addr(), raw.as_bytes(), false).await;
        let after = r.upstream_requests();
        if resp.status == 200 {
            served += 1;
            assert_eq!(after, before + 1, "case {case}: {path}");
            // The new request, at whichever fake got it.
            let target = if r.go.count() > go_before {
                assert_eq!(r.ca.count(), ca_before, "case {case}: {path:?}");
                r.go.seen().last().unwrap().target.clone()
            } else {
                r.ca.seen().last().unwrap().target.clone()
            };
            let (p, _) = target.split_once('?').unwrap_or((&target, ""));
            let decoded = percent_decode_once(p)
                .unwrap_or_else(|| panic!("case {case}: undecodable target {target} for {path}"));
            assert!(
                !decoded.contains('\0') && !decoded.contains('\\'),
                "case {case}: {path:?} -> {target:?}"
            );
            assert!(
                !decoded.contains("//"),
                "case {case}: {path:?} -> {target:?}"
            );
            assert!(
                decoded.split('/').all(|seg| seg != "." && seg != ".."),
                "case {case}: {path:?} -> {target:?}"
            );
            // What went upstream is the base plus the raw text the client sent after the prefix, byte for byte.
            // A fragment never leaves the client (the URI parser drops it); everything after `#` is not part of the target.
            let target_text = path.split('#').next().unwrap_or("");
            let (raw_path, raw_query) = target_text
                .split_once('?')
                .map_or((target_text, None), |(a, b)| (a, Some(b)));
            let (base, rest) = ["/pypi", "/npm", "/maven", "/go"]
                .iter()
                .find_map(|pre| raw_path.strip_prefix(pre).map(|rest| (*pre, rest)))
                .unwrap_or_else(|| {
                    panic!("case {case}: {path:?} served without a configured prefix")
                });
            let upstream_base = if base == "/go" {
                String::new()
            } else {
                format!("{base}/packages")
            };
            let mut want = format!("{upstream_base}{rest}");
            if want.is_empty() {
                want.push('/');
            }
            if let Some(q) = raw_query {
                want.push('?');
                want.push_str(q);
            }
            assert_eq!(target, want, "case {case}: {path:?}");
        } else {
            assert_eq!(
                after, before,
                "case {case}: {path} answered {} but reached an upstream",
                resp.status
            );
        }
    }
    assert!(
        served > 50,
        "the fuzz must exercise the served path too: {served}"
    );
}
