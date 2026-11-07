#![allow(clippy::unwrap_used)]

mod common;
use bytes::Bytes;
use common::*;
use pkgproxy::{
    Auth, Config, Prefix, Secret, Upstream, codeartifact_upstreams, go_upstream, start,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn ok() -> Handler {
    always(Reply::ok("fine").header("Content-Type", "application/octet-stream"))
}

// -- T6 ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn before_expiry_200_after_it_401_and_the_go_store_is_unaffected() {
    let mut o = Opts::new(ok(), nothing(), ok());
    o.clock_now = 5_000;
    o.expires_at_unix = Some(5_100);
    let r = rig_with(o).await;
    for p in ["/pypi/x", "/npm/x", "/maven/x"] {
        assert_eq!(r.get(p).await.status, 200, "{p} before expiry");
    }
    assert_eq!(r.ca.count(), 3);

    r.clock.store(5_099, Ordering::SeqCst);
    assert_eq!(r.get("/pypi/x").await.status, 200, "one second before");

    r.clock.store(5_100, Ordering::SeqCst);
    let before = r.ca.count();
    for p in ["/pypi/x", "/npm/x", "/maven/x"] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 401, "{p} at expiry");
        assert!(
            resp.text().contains("package token expired"),
            "{}",
            resp.text()
        );
    }
    assert_eq!(
        r.ca.count(),
        before,
        "an expired token is not sent anywhere"
    );
    let go = r.get("/go/example.com/m/@v/list").await;
    assert_eq!(
        go.status, 200,
        "the Go store has no credential and keeps working"
    );
    r.clock.store(9_999_999, Ordering::SeqCst);
    assert_eq!(r.get("/go/example.com/m/@v/list").await.status, 200);
    let s = r.proxy.snapshot();
    assert!(s.refused >= 3 && s.statuses.get(&401) == Some(&3), "{s:?}");
}

#[tokio::test]
async fn without_an_expiry_the_credential_never_lapses() {
    let r = rig(ok(), nothing(), nothing()).await;
    r.clock.store(u64::MAX / 2, Ordering::SeqCst);
    assert_eq!(r.get("/pypi/x").await.status, 200);
}

#[tokio::test]
async fn expiry_follows_the_real_clock_when_no_test_clock_is_injected() {
    // `start` (no test seams) reads the system clock. Nothing here reaches an upstream: a credentialed prefix answers 401 before
    // it, and the `.invalid` hosts never resolve.
    fn config(expires_at_unix: Option<u64>) -> Config {
        struct Quiet;
        impl pkgproxy::Log for Quiet {
            fn line(&self, _t: &str) {}
        }
        let mut ups = codeartifact_upstreams("registry.invalid", REPO, Secret::new(token()));
        ups.push(go_upstream("https://modules.invalid"));
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstreams: ups,
            expires_at_unix,
            max_connections: 8,
            request_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(3),
            max_redirects: 5,
            log: Arc::new(Quiet),
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(now > 1_700_000_000, "the host clock is a real one: {now}");

    // Expired a long time ago: every credentialed prefix answers 401, the Go store does not.
    let expired = start(config(Some(1))).await.unwrap();
    for p in ["/pypi/x", "/npm/x", "/maven/x"] {
        let resp = get(expired.addr(), p).await;
        assert_eq!(resp.status, 401, "{p}: {}", resp.text());
    }
    let go = get(expired.addr(), "/go/example.com/m/@v/list").await;
    assert_ne!(go.status, 401, "the Go store never expires: {}", go.text());
    expired.stop().await;

    // Expires in an hour: not a 401 (the upstream name does not resolve, so the answer is a 502).
    let live = start(config(Some(now + 3600))).await.unwrap();
    let resp = get(live.addr(), "/pypi/x").await;
    assert_ne!(resp.status, 401, "{}", resp.text());
    live.stop().await;
}

// -- T7 ---------------------------------------------------------------------------------------------------------------

fn sample_config(tok: &str) -> Config {
    struct Quiet;
    impl pkgproxy::Log for Quiet {
        fn line(&self, _t: &str) {}
    }
    let mut ups = codeartifact_upstreams(CA_HOST, REPO, Secret::new(tok.to_string()));
    ups.push(go_upstream("https://bucket.s3.us-east-1.amazonaws.com"));
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstreams: ups,
        expires_at_unix: Some(1),
        max_connections: 64,
        request_timeout: Duration::from_secs(900),
        idle_timeout: Duration::from_secs(60),
        max_redirects: 5,
        log: Arc::new(Quiet),
    }
}

#[tokio::test]
async fn config_upstream_secret_and_handle_never_print_the_token() {
    let tok = token();
    let cfg = sample_config(&tok);
    let basic = b64(format!("aws:{tok}").as_bytes());
    let mut shown = vec![
        format!("{cfg:?}"),
        format!("{cfg:#?}"),
        format!("{:?}", cfg.upstreams),
        format!("{:?}", Auth::Bearer(Secret::new(tok.clone()))),
        format!("{:?}", Secret::new(tok.clone())),
        format!("{}", Secret::new(tok.clone())),
    ];
    let handle = start(cfg).await.unwrap();
    shown.push(format!("{handle:?}"));
    shown.push(format!("{:?}", handle.env()));
    shown.push(handle.maven_settings_xml());
    shown.push(format!("{:?}", handle.snapshot()));
    for s in shown {
        assert!(!s.contains(&tok), "printed the token: {s}");
        assert!(!s.contains(&basic), "printed the Basic credential: {s}");
        assert!(!s.contains("0123456789"), "{s}");
    }
    handle.stop().await;
}

#[tokio::test]
async fn nothing_the_client_can_read_or_the_log_holds_contains_the_token() {
    let r = rig(
        handler(|s| {
            if s.target.contains("boom") {
                Reply::aborted()
            } else {
                Reply::ok("x").header("Content-Type", "application/octet-stream")
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    let basic = b64(format!("aws:{}", r.token).as_bytes());
    let mut seen = Vec::new();
    for p in [
        "/pypi/x",
        "/pypi/boom",
        "/npm/x",
        "/nope",
        "/pypi/../x",
        "/maven/y",
    ] {
        let resp = r.get(p).await;
        seen.push(format!("{:?} {}", resp.headers, resp.text()));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    seen.extend(r.log.lines());
    for s in seen {
        assert!(!s.contains(&r.token) && !s.contains(&basic), "{s}");
    }
}

// -- 7.3 ----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn env_has_exactly_the_documented_keys_for_the_real_port() {
    let r = rig(nothing(), nothing(), nothing()).await;
    let p = r.addr().port();
    let env = r.proxy.env();
    let want: std::collections::BTreeMap<String, String> = [
        (
            "PIP_INDEX_URL",
            format!("http://127.0.0.1:{p}/pypi/simple/"),
        ),
        ("PIP_TRUSTED_HOST", "127.0.0.1".into()),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1".into()),
        (
            "UV_DEFAULT_INDEX",
            format!("http://127.0.0.1:{p}/pypi/simple/"),
        ),
        ("UV_INDEX_URL", format!("http://127.0.0.1:{p}/pypi/simple/")),
        ("UV_INSECURE_HOST", format!("127.0.0.1:{p}")),
        ("UV_PYTHON_DOWNLOADS", "never".into()),
        ("npm_config_registry", format!("http://127.0.0.1:{p}/npm/")),
        // The npm CLI is given tarball links on registry.npmjs.org and replaces that host with the registry above, so a lockfile
        // it writes names the public host; pnpm 12 reads its own variable and ignores `npm_config_registry`.
        ("npm_config_replace_registry_host", "npmjs".into()),
        ("pnpm_config_registry", format!("http://127.0.0.1:{p}/npm/")),
        ("npm_config_update_notifier", "false".into()),
        ("npm_config_audit", "false".into()),
        ("npm_config_fund", "false".into()),
        ("GOPROXY", format!("http://127.0.0.1:{p}/go")),
        ("GOSUMDB", "off".into()),
        ("GOFLAGS", "-mod=mod".into()),
        ("GOTOOLCHAIN", "local".into()),
        // The Maven wrapper (`mvnw`) downloads its Maven distribution with curl from the host in distributionUrl, which no
        // settings.xml mirror reaches; MVNW_REPOURL is the variable its script reads to fetch it from another repository
        // (apache/maven-wrapper only-mvnw: `$MVNW_REPOURL/org/apache/maven/...`), here the proxy's Maven prefix.
        ("MVNW_REPOURL", format!("http://127.0.0.1:{p}/maven")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(env, want);
    assert_eq!(
        r.proxy.maven_settings_xml(),
        format!(
            "<settings><mirrors><mirror><id>loop</id><mirrorOf>*</mirrorOf><url>http://127.0.0.1:{p}/maven/</url></mirror></mirrors></settings>"
        )
    );
}

// -- upstream constructors and config validation ------------------------------------------------------------------------

#[test]
fn codeartifact_and_go_upstreams_have_the_documented_bases_and_credentials() {
    let ups = codeartifact_upstreams(
        "dom-123.d.codeartifact.us-east-1.amazonaws.com",
        "packages",
        Secret::new("t".into()),
    );
    let by: std::collections::BTreeMap<_, _> = ups.iter().map(|u| (u.prefix, u)).collect();
    // The cargo endpoint of the same repository joins pypi, npm and maven.
    assert_eq!(ups.len(), 4);
    assert_eq!(
        by[&Prefix::Cargo].base,
        "https://dom-123.d.codeartifact.us-east-1.amazonaws.com/cargo/packages"
    );
    assert!(matches!(&by[&Prefix::Cargo].auth, Auth::Raw(_)));
    assert_eq!(
        by[&Prefix::Pypi].base,
        "https://dom-123.d.codeartifact.us-east-1.amazonaws.com/pypi/packages"
    );
    assert_eq!(
        by[&Prefix::Npm].base,
        "https://dom-123.d.codeartifact.us-east-1.amazonaws.com/npm/packages"
    );
    assert_eq!(
        by[&Prefix::Maven].base,
        "https://dom-123.d.codeartifact.us-east-1.amazonaws.com/maven/packages"
    );
    assert!(matches!(&by[&Prefix::Pypi].auth, Auth::Basic { user, .. } if user == "aws"));
    assert!(matches!(&by[&Prefix::Maven].auth, Auth::Basic { user, .. } if user == "aws"));
    assert!(matches!(&by[&Prefix::Npm].auth, Auth::Bearer(_)));
    let go = go_upstream("https://bucket.s3.us-east-1.amazonaws.com/");
    assert_eq!(go.prefix, Prefix::Go);
    assert_eq!(
        go.base, "https://bucket.s3.us-east-1.amazonaws.com",
        "no trailing slash"
    );
    assert!(matches!(go.auth, Auth::None));
}

fn bad(f: impl FnOnce(&mut Config)) -> std::io::Error {
    let mut c = sample_config(&token());
    f(&mut c);
    futures_block(start(c)).unwrap_err()
}

fn futures_block<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

#[test]
fn start_refuses_a_config_that_would_leak_or_listen_wide() {
    for (name, e) in [
        ("0.0.0.0", bad(|c| c.listen = "0.0.0.0:0".parse().unwrap())),
        (
            "lan address",
            bad(|c| c.listen = "10.1.2.3:0".parse().unwrap()),
        ),
        (
            "ipv6 loopback",
            bad(|c| c.listen = "[::1]:0".parse().unwrap()),
        ),
        (
            "127.0.0.2",
            bad(|c| c.listen = "127.0.0.2:0".parse().unwrap()),
        ),
        (
            "http credentialed upstream",
            bad(|c| {
                c.upstreams[0] = Upstream {
                    prefix: Prefix::Pypi,
                    base: "http://example.amazonaws.com/pypi/packages".into(),
                    auth: Auth::Basic {
                        user: "aws".into(),
                        secret: Secret::new("t".into()),
                    },
                }
            }),
        ),
        (
            "http anonymous upstream",
            bad(|c| {
                c.upstreams[3] = Upstream {
                    prefix: Prefix::Go,
                    base: "http://bucket.example/".into(),
                    auth: Auth::None,
                }
            }),
        ),
        (
            "userinfo in a base",
            bad(|c| {
                c.upstreams[0].base = "https://user@example.amazonaws.com/pypi/packages".into()
            }),
        ),
        (
            "query in a base",
            bad(|c| c.upstreams[0].base = "https://example.amazonaws.com/p?x=1".into()),
        ),
        ("empty base", bad(|c| c.upstreams[0].base = String::new())),
        (
            "duplicate prefix",
            bad(|c| {
                let dup = go_upstream("https://other.s3.us-east-1.amazonaws.com");
                c.upstreams.push(dup)
            }),
        ),
        ("zero connections", bad(|c| c.max_connections = 0)),
        (
            "zero idle timeout",
            bad(|c| c.idle_timeout = Duration::ZERO),
        ),
        (
            "zero request timeout",
            bad(|c| c.request_timeout = Duration::ZERO),
        ),
    ] {
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{name}: {e}");
        assert!(
            !e.to_string().contains("0123456789"),
            "{name}: the error text must not hold the token: {e}"
        );
    }
}

#[tokio::test]
async fn port_zero_binds_a_real_loopback_port_and_a_busy_port_is_an_error() {
    let a = start(sample_config(&token())).await.unwrap();
    let addr = a.addr();
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0);
    let mut c = sample_config(&token());
    c.listen = addr;
    let err = start(c).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    a.stop().await;
    // After stop the port is free again.
    let mut c = sample_config(&token());
    c.listen = addr;
    let again = start(c).await.unwrap();
    again.stop().await;
}

#[tokio::test]
async fn stop_closes_the_listener_and_open_connections() {
    // The default idle time is 60 s: a `stop` that waited for connections to end by themselves would take that long.
    let r = rig(nothing(), nothing(), nothing()).await;
    let addr = r.addr();
    let mut idle = TcpStream::connect(addr).await.unwrap();
    // Let the proxy accept it first: a connection still in the backlog when `stop` runs is closed with the listener.
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::timeout(Duration::from_secs(5), r.proxy.stop())
        .await
        .expect("stop returns at once, without waiting for the idle time");
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), idle.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "an open connection is closed on stop");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "nothing listens after stop"
    );
}

#[tokio::test]
async fn stop_returns_promptly_with_a_request_in_flight() {
    // A download that has sent its head and then stalls keeps its connection for up to twice the idle time (120 s here).
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::ThenStall(Bytes::from_static(b"first")))
                .header("Content-Type", "application/octet-stream"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let addr = r.addr();
    let mut busy = TcpStream::connect(addr).await.unwrap();
    busy.write_all(&request("GET", "/pypi/stalls", addr, &[]))
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !contains_str(&got, "first") {
        let n = tokio::time::timeout(Duration::from_secs(10), busy.read(&mut buf))
            .await
            .expect("the first bytes arrive")
            .unwrap();
        assert!(n > 0, "closed before the first bytes");
        got.extend_from_slice(&buf[..n]);
    }
    let _idle = TcpStream::connect(addr).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), r.proxy.stop())
        .await
        .expect("stop returns at once, with a request in flight and an idle connection open");
    let end = tokio::time::timeout(Duration::from_secs(5), busy.read(&mut buf))
        .await
        .expect("the in-flight connection is closed by stop")
        .unwrap_or(0);
    assert_eq!(end, 0);
}

// -- T8 -----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_65th_connection_gets_503_and_a_freed_slot_is_reusable() {
    let r = rig(ok(), nothing(), nothing()).await;
    let addr = r.addr();
    let mut held = Vec::new();
    for _ in 0..64 {
        let mut c = TcpStream::connect(addr).await.unwrap();
        // Make sure the proxy has accepted it: a first request would also work, but an idle connection counts as one.
        c.write_all(b"").await.unwrap();
        held.push(c);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let resp = get(addr, "/pypi/x").await;
    assert_eq!(resp.status, 503, "{}", resp.text());
    assert_eq!(r.ca.count(), 0);
    drop(held.pop());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(get(addr, "/pypi/x").await.status, 200, "a slot was freed");
}

#[tokio::test]
async fn a_small_limit_in_the_config_is_honoured() {
    let mut o = Opts::new(ok(), nothing(), nothing());
    o.max_connections = 2;
    let r = rig_with(o).await;
    let a = TcpStream::connect(r.addr()).await.unwrap();
    let b = TcpStream::connect(r.addr()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(get(r.addr(), "/pypi/x").await.status, 503);
    drop((a, b));
}

#[tokio::test]
async fn at_most_64_connections_are_answered_503_at_once_and_the_rest_are_dropped() {
    // One slot, held. The next 64 connections each wait for a 503 (an answered request); the 65th of them is dropped unanswered.
    let mut o = Opts::new(ok(), nothing(), nothing());
    o.max_connections = 1;
    let r = rig_with(o).await;
    let addr = r.addr();
    let _slot = TcpStream::connect(addr).await.unwrap();
    let mut overflow = Vec::new();
    for _ in 0..64 {
        overflow.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut extra = TcpStream::connect(addr).await.unwrap();
    let _ = extra.write_all(&request("GET", "/pypi/x", addr, &[])).await;
    let dropped = read_response(&mut extra, false).await;
    assert_eq!(
        dropped.status, 0,
        "the 65th overflow connection gets no answer: {dropped:?}"
    );
    // The first 64 are still answered.
    let mut first = overflow.remove(0);
    first
        .write_all(&request("GET", "/pypi/x", addr, &[]))
        .await
        .unwrap();
    assert_eq!(read_response(&mut first, false).await.status, 503);
    assert_eq!(r.ca.count(), 0);
}

#[tokio::test]
async fn a_request_in_flight_may_be_silent_for_twice_the_idle_time_but_not_longer() {
    // Idle time 1.5 s. The CodeArtifact answer is a redirect after 1.0 s and the S3 answer comes 1.0 s later: each upstream wait is
    // inside the idle time, the client hears nothing for 2.0 s. The connection must stay open (its limit with a request in flight
    // is 3.0 s); a limit of the idle time alone would close it at 1.5 s.
    let mut o = Opts::new(
        always(
            Reply::redirect(302, &format!("https://{S3_HOST}/bucket/key"))
                .delayed(Duration::from_millis(1000)),
        ),
        always(
            Reply::ok("WHEEL-BYTES")
                .header("Content-Type", "application/octet-stream")
                .delayed(Duration::from_millis(1000)),
        ),
        nothing(),
    );
    o.idle_timeout = Duration::from_millis(1500);
    let r = rig_with(o).await;
    let t = Instant::now();
    let resp = r.get("/pypi/slow-redirect").await;
    assert!(
        t.elapsed() > Duration::from_millis(1900),
        "TRIGGER PRESENT: the client was silent for more than the idle time ({:?})",
        t.elapsed()
    );
    assert_eq!((resp.status, resp.text().as_str()), (200, "WHEEL-BYTES"));
}

#[tokio::test]
async fn an_idle_connection_is_closed_after_the_idle_timeout() {
    let mut o = Opts::new(ok(), nothing(), nothing());
    o.idle_timeout = Duration::from_millis(400);
    let r = rig_with(o).await;
    let mut c = TcpStream::connect(r.addr()).await.unwrap();
    let t = Instant::now();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    assert_eq!(n, 0);
    assert!(
        t.elapsed() >= Duration::from_millis(300),
        "closed too early: {:?}",
        t.elapsed()
    );
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "closed too late: {:?}",
        t.elapsed()
    );

    // A keep-alive connection that served a request is also closed once idle.
    let mut c = TcpStream::connect(r.addr()).await.unwrap();
    let raw = format!(
        "GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        r.addr().port()
    );
    c.write_all(raw.as_bytes()).await.unwrap();
    let resp = read_response(&mut c, false).await;
    assert_eq!(resp.status, 200);
    let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_slow_but_live_transfer_is_not_cut_by_the_idle_timeout() {
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::Trickle {
                    gap: Duration::from_millis(250),
                    count: 6,
                })
                .header("Content-Type", "application/octet-stream"),
        ),
        nothing(),
        nothing(),
    );
    o.idle_timeout = Duration::from_millis(600);
    let r = rig_with(o).await;
    let resp = r.get("/pypi/slow").await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body, b"tttttt",
        "1.5 s of transfer with 0.25 s gaps survives a 0.6 s idle limit"
    );
    assert!(resp.complete);
}

#[tokio::test]
async fn a_slow_request_head_is_cut_off() {
    let mut o = Opts::new(ok(), nothing(), nothing());
    o.idle_timeout = Duration::from_millis(500);
    let r = rig_with(o).await;
    let mut c = TcpStream::connect(r.addr()).await.unwrap();
    c.write_all(b"GET /pypi/x HTT").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
        .await
        .expect("cut off in time")
        .unwrap_or(0);
    // Either a 408 or a plain close is fine; what matters is that the connection does not stay open.
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(n == 0 || head.starts_with("HTTP/1.1 4"), "{head:?}");
    assert_eq!(r.ca.count(), 0);
}

#[tokio::test]
async fn a_client_that_stops_reading_a_download_is_cut_off_and_frees_its_slot() {
    // Two clients ask for a 2 GB body and never read it. The socket buffers fill, the proxy's writes stall, nothing moves on the
    // connection, and the proxy closes it after twice the idle time instead of holding the slot for ever.
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::Gen {
                    total: 2_000_000_000,
                })
                .header("Content-Type", "application/octet-stream"),
        ),
        nothing(),
        nothing(),
    );
    o.max_connections = 2;
    o.idle_timeout = Duration::from_millis(400);
    let r = rig_with(o).await;
    let addr = r.addr();
    let mut stuck = Vec::new();
    for _ in 0..2 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&request("GET", "/pypi/huge", addr, &[]))
            .await
            .unwrap();
        stuck.push(s);
    }
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let resp = get(addr, "/nope").await;
    assert_eq!(
        resp.status,
        403,
        "both slots were reclaimed (a 503 means they are still held): {}",
        resp.text()
    );
    drop(stuck);
}
