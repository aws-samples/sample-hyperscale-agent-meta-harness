//! Absolute-form targets, dot segments (plain and percent-encoded), runs of `%2f`, a foreign `Host`, `CONNECT`, `POST` and an
//! unknown prefix are all refused with the stated status, and none of them reaches an upstream.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

fn raw(addr: std::net::SocketAddr, line: &str, host: Option<&str>, extra: &str) -> Vec<u8> {
    let mut s = format!("{line}\r\n");
    if let Some(h) = host {
        s.push_str(&format!("Host: {h}\r\n"));
    } else {
        s.push_str(&format!("Host: 127.0.0.1:{}\r\n", addr.port()));
    }
    s.push_str("Connection: close\r\n");
    s.push_str(extra);
    s.push_str("\r\n");
    s.into_bytes()
}

async fn expect(r: &Rig, line: &str, host: Option<&str>, extra: &str, want: u16) {
    let resp = send_raw(r.addr(), &raw(r.addr(), line, host, extra), false).await;
    assert_eq!(
        resp.status,
        want,
        "{line} (Host {host:?}) answered {} {}",
        resp.status,
        resp.text()
    );
    assert_eq!(
        r.upstream_requests(),
        0,
        "{line}: a refused request must not reach any upstream"
    );
}

fn allow_all() -> Handler {
    always(Reply::ok("reached").header("Content-Type", "application/octet-stream"))
}

#[tokio::test]
async fn absolute_form_authority_form_and_asterisk_targets_are_400() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    let p = r.addr().port();
    for line in [
        "GET http://192.0.2.1/decoy/data/ HTTP/1.1".to_string(),
        format!("GET http://127.0.0.1:{p}/pypi/simple/ HTTP/1.1"),
        "GET https://decoy.example.com/pypi/simple/ HTTP/1.1".to_string(),
        "GET * HTTP/1.1".to_string(),
        "GET pypi/simple HTTP/1.1".to_string(),
        "GET  HTTP/1.1".to_string(),
    ] {
        let resp = send_raw(r.addr(), &raw(r.addr(), &line, None, ""), false).await;
        assert!(
            resp.status == 400,
            "{line} answered {} (want 400): {}",
            resp.status,
            resp.text()
        );
    }
    assert_eq!(r.upstream_requests(), 0);
}

#[tokio::test]
async fn dot_segments_encoded_slashes_nul_backslash_and_bad_escapes_are_400() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    for path in [
        "/pypi/../npm/x",
        "/pypi/./x",
        "/pypi/a/../b",
        "/pypi/%2e%2e/npm/x",
        "/pypi/%2E%2E/x",
        "/pypi/.%2e/x",
        "/pypi/%2e/x",
        "/pypi/a%2f..%2fb",
        "/pypi/a%2F%2e%2e%2Fb",
        "/pypi//simple",
        "/pypi/a//b",
        "/pypi/a%2f%2fb",
        "/pypi/a%2F%2Fb",
        "/pypi/a/%2fb",
        "/pypi/%00",
        "/pypi/a%00b",
        "/pypi/a%5cb",
        "/pypi/a%5Cb",
        "/pypi/%zz",
        "/pypi/%",
        "/pypi/%4",
        "/pypi/%ff%fe",
        "/npm/..",
        "/go/..%2f..",
        "/maven/x/%2e%2e%2f%2e%2e%2fother/file",
    ] {
        expect(&r, &format!("GET {path} HTTP/1.1"), None, "", 400).await;
    }
}

#[tokio::test]
async fn an_oversized_path_or_query_is_400() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    let ok_path = format!("/pypi/{}", "a".repeat(2048 - "/pypi/".len()));
    assert_eq!(ok_path.len(), 2048);
    assert_eq!(
        r.get(&ok_path).await.status,
        200,
        "2,048 bytes is the limit and is allowed"
    );
    let before = r.upstream_requests();
    let long_path = format!("/pypi/{}", "a".repeat(2049 - "/pypi/".len()));
    assert_eq!(r.get(&long_path).await.status, 400);
    let ok_query = format!("/pypi/x?{}", "q".repeat(1024));
    assert_eq!(
        r.get(&ok_query).await.status,
        200,
        "1,024 bytes of query is allowed"
    );
    let long_query = format!("/pypi/x?{}", "q".repeat(1025));
    assert_eq!(r.get(&long_query).await.status, 400);
    assert_eq!(r.upstream_requests(), before + 1);
}

#[tokio::test]
async fn a_foreign_missing_or_portless_host_header_is_400() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    let p = r.addr().port();
    for host in [
        "decoy.example.com".to_string(),
        "decoy.example.com:80".to_string(),
        "127.0.0.1".to_string(),
        format!("localhost:{p}"),
        format!("127.0.0.2:{p}"),
        format!("127.0.0.1:{}", p.wrapping_add(1)),
        format!("[::1]:{p}"),
        format!("127.0.0.1:{p}.decoy.com"),
        format!("decoy.com#127.0.0.1:{p}"),
        String::new(),
    ] {
        expect(&r, "GET /pypi/simple/ HTTP/1.1", Some(&host), "", 400).await;
    }
    // No Host at all.
    let resp = send_raw(
        r.addr(),
        b"GET /pypi/simple/ HTTP/1.1\r\nConnection: close\r\n\r\n",
        false,
    )
    .await;
    assert_eq!(resp.status, 400);
    // HTTP/1.0 without Host.
    let resp = send_raw(r.addr(), b"GET /pypi/simple/ HTTP/1.0\r\n\r\n", false).await;
    assert_eq!(resp.status, 400);
    // Two Host headers.
    let two = format!(
        "GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nHost: decoy.example.com\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(send_raw(r.addr(), two.as_bytes(), false).await.status, 400);
    assert_eq!(r.upstream_requests(), 0);
}

#[tokio::test]
async fn methods_other_than_get_and_head_are_405_with_an_allow_header() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    let p = r.addr().port();
    for (method, target) in [
        ("POST", "/pypi/simple/"),
        ("PUT", "/pypi/x"),
        ("DELETE", "/npm/x"),
        ("PATCH", "/maven/x"),
        ("OPTIONS", "/go/x"),
        ("OPTIONS", "*"),
        ("TRACE", "/pypi/x"),
        ("CONNECT", "192.0.2.1:80"),
        ("CONNECT", "decoy.example.com:443"),
        ("PROPFIND", "/maven/x"),
    ] {
        let req = format!(
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let resp = send_raw(r.addr(), req.as_bytes(), false).await;
        assert_eq!(resp.status, 405, "{method} {target}: {}", resp.text());
        let allow = resp
            .header("allow")
            .unwrap_or_default()
            .to_ascii_uppercase();
        assert!(
            allow.contains("GET") && allow.contains("HEAD"),
            "Allow: {allow}"
        );
    }
    assert_eq!(r.upstream_requests(), 0);
}

#[tokio::test]
async fn an_unknown_empty_or_decorated_prefix_is_403() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    for path in [
        "/",
        "/decoy/x",
        "/PYPI/simple/",
        "/Pypi/x",
        "/pypi2/x",
        "/pypix",
        "/npmx/x",
        "/%70ypi/simple/",
        "/pypi%2fsimple",
        "/health",
        "/decoy/data/",
        "/s3/x",
        "/go2/x",
    ] {
        expect(&r, &format!("GET {path} HTTP/1.1"), None, "", 403).await;
    }
}

#[tokio::test]
async fn a_request_with_a_body_is_400_and_never_forwarded() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    let p = r.addr().port();
    let with_len = format!(
        "GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
    );
    assert_eq!(
        send_raw(r.addr(), with_len.as_bytes(), false).await.status,
        400
    );
    let chunked = format!(
        "GET /pypi/x HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
    );
    assert_eq!(
        send_raw(r.addr(), chunked.as_bytes(), false).await.status,
        400
    );
    assert_eq!(r.upstream_requests(), 0);
}

#[tokio::test]
async fn refusals_are_counted_and_logged_without_a_secret() {
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    for line in [
        "GET /decoy/x HTTP/1.1",
        "GET /pypi/../x HTTP/1.1",
        "GET http://x/ HTTP/1.1",
    ] {
        send_raw(r.addr(), &raw(r.addr(), line, None, ""), false).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let s = r.proxy.snapshot();
    assert_eq!(s.refused, 3, "{s:?}");
    assert_eq!(s.requests, 3);
    assert_eq!(s.statuses.get(&403), Some(&1));
    assert_eq!(s.statuses.get(&400), Some(&2));
    for l in r.log.lines() {
        assert!(l.starts_with("pkgproxy "), "{l}");
        assert!(
            !l.contains("decoy") && !l.contains("192.0.2") && !l.contains(&r.token),
            "no path, host or token in a log line: {l}"
        );
    }
}

#[tokio::test]
async fn paths_that_depend_on_how_an_upstream_decodes_or_trims_are_400() {
    // The raw path goes upstream, so a path that is harmless once decoded but means something else to a backend that decodes
    // twice, strips `;params`, trims blanks or normalises Unicode must not get through: package paths have no such characters.
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    for path in [
        "/pypi/..;/other-repo/simple/",
        "/pypi/.;/x",
        "/pypi/a;b",
        "/pypi/a%3bb",
        "/pypi/%252e%252e/other",
        "/pypi/%252f",
        "/pypi/a%25b",
        "/pypi/%20../x",
        "/pypi/..%20/x",
        "/pypi/a%20b",
        "/pypi/a%0d%0ab",
        "/pypi/a%0ab",
        "/pypi/a%09b",
        "/pypi/a%7fb",
        "/pypi/%EF%BC%8E%EF%BC%8E/x",
        "/pypi/caf%C3%A9",
        "/pypi/a%3fb",
        "/pypi/a%23b",
        "/pypi/a%22b",
        "/pypi/a%3Cb",
        "/pypi/a%3eb",
        "/pypi/a%7Cb",
        "/pypi/a%5Eb",
        "/pypi/a%60b",
        "/pypi/a%7Bb",
        "/pypi/a%7Db",
        "/pypi/a%5Bb",
        "/pypi/...",
        "/pypi/a/..../b",
        "/npm/%252e",
        "/maven/x/%2e%2e;/y",
    ] {
        expect(&r, &format!("GET {path} HTTP/1.1"), None, "", 400).await;
    }
}

#[tokio::test]
async fn the_go_prefix_takes_no_query_because_the_module_protocol_has_none() {
    // A bucket answers `?list-type=2`, `?acl` and `?versionId=` on the same path: only plain object reads may go through.
    let r = rig(allow_all(), allow_all(), allow_all()).await;
    for path in [
        "/go/example.com/m/@v/list?list-type=2",
        "/go/?list-type=2&prefix=other",
        "/go/x?acl",
        "/go/x?versionId=1",
        "/go/x?",
    ] {
        expect(&r, &format!("GET {path} HTTP/1.1"), None, "", 400).await;
    }
    assert_eq!(r.get("/go/example.com/m/@v/list").await.status, 200);
    assert_eq!(
        r.get("/pypi/simple/x/?a=1").await.status,
        200,
        "other prefixes keep their query"
    );
}
