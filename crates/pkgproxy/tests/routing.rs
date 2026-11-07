#![allow(clippy::unwrap_used)]

mod common;
use bytes::Bytes;
use common::*;

fn json_ok() -> Reply {
    Reply::ok("{}").header("Content-Type", "application/json")
}

#[tokio::test]
async fn each_prefix_reaches_its_upstream_with_path_query_and_credential() {
    let r = rig(always(json_ok()), nothing(), always(Reply::ok("v1.0.0\n"))).await;
    let tok = r.token.clone();

    assert_eq!(
        r.get("/pypi/simple/requests/?a=1&b=x%20y").await.status,
        200
    );
    assert_eq!(r.get("/npm/@babel%2fcore").await.status, 200);
    assert_eq!(r.get("/npm/left-pad").await.status, 200);
    assert_eq!(
        r.get("/maven/org/example/lib/1.0/lib-1.0.pom").await.status,
        200
    );
    let go = r.get("/go/example.com/mod/@v/list").await;
    assert_eq!((go.status, go.text().as_str()), (200, "v1.0.0\n"));

    let ca = r.ca.seen();
    let targets: Vec<&str> = ca.iter().map(|s| s.target.as_str()).collect();
    assert_eq!(
        targets,
        vec![
            "/pypi/packages/simple/requests/?a=1&b=x%20y",
            "/npm/packages/@babel%2fcore",
            "/npm/packages/left-pad",
            "/maven/packages/org/example/lib/1.0/lib-1.0.pom",
        ],
        "path, query and the percent-encoded %2f reach the upstream as the client sent them"
    );
    assert_eq!(
        ca[0].header("authorization"),
        Some(basic_for(&tok).as_str())
    );
    assert_eq!(
        ca[1].header("authorization"),
        Some(format!("Bearer {tok}").as_str())
    );
    assert_eq!(
        ca[2].header("authorization"),
        Some(format!("Bearer {tok}").as_str())
    );
    assert_eq!(
        ca[3].header("authorization"),
        Some(basic_for(&tok).as_str())
    );

    let g = r.go.seen();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].target, "/example.com/mod/@v/list");
    assert_eq!(
        g[0].header("authorization"),
        None,
        "the Go store is anonymous: no credential goes to it"
    );
    assert_eq!(r.s3.count(), 0);
}

#[tokio::test]
async fn the_clients_credentials_cookies_and_proxy_headers_are_dropped() {
    let r = rig(always(json_ok()), nothing(), nothing()).await;
    let client_cred = b64(b"client:value");
    let resp = get_with(
        r.addr(),
        "/pypi/simple/",
        &[
            ("Authorization", &format!("Basic {client_cred}")),
            ("Cookie", "session=abc"),
            ("Proxy-Authorization", "Basic abc"),
            ("Proxy-Connection", "keep-alive"),
            ("X-Forwarded-For", "10.0.0.1"),
            ("X-Amz-Security-Token", "nope"),
            ("Accept", "text/html"),
            ("User-Agent", "pip/24 test"),
            ("If-None-Match", "\"abc\""),
            ("If-Modified-Since", "Wed, 21 Oct 2026 07:28:00 GMT"),
            ("Range", "bytes=0-1"),
            ("If-Range", "\"abc\""),
            ("Accept-Encoding", "gzip, br"),
        ],
    )
    .await;
    assert_eq!(resp.status, 200);
    let seen = &r.ca.seen()[0];
    assert_eq!(
        seen.headers_named("authorization"),
        vec![basic_for(&r.token).as_str()],
        "exactly one Authorization header, and it is the proxy's"
    );
    for name in [
        "cookie",
        "proxy-authorization",
        "proxy-connection",
        "x-forwarded-for",
        "x-amz-security-token",
    ] {
        assert_eq!(seen.header(name), None, "{name} reached the upstream");
    }
    let all = format!("{:?}", seen.headers);
    assert!(
        !all.contains(&client_cred),
        "the client's credential reached the upstream"
    );
    assert_eq!(seen.header("accept"), Some("text/html"));
    assert_eq!(seen.header("user-agent"), Some("pip/24 test"));
    assert_eq!(seen.header("if-none-match"), Some("\"abc\""));
    assert_eq!(
        seen.header("if-modified-since"),
        Some("Wed, 21 Oct 2026 07:28:00 GMT")
    );
    assert_eq!(seen.header("range"), Some("bytes=0-1"));
    assert_eq!(seen.header("if-range"), Some("\"abc\""));
    assert_eq!(
        seen.header("accept-encoding"),
        Some("identity"),
        "the proxy forces identity so it can rewrite text"
    );
}

#[tokio::test]
async fn only_the_listed_response_headers_come_back_and_never_the_credential() {
    // An octet stream: a rewritten (textual) answer drops Accept-Ranges, so this one is not text.
    let upstream = Reply::ok("{}")
        .header("Content-Type", "application/octet-stream")
        .header("ETag", "\"v1\"")
        .header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
        .header("Cache-Control", "max-age=60")
        .header("Accept-Ranges", "bytes")
        .header("Content-Disposition", "attachment; filename=x.json")
        .header("Set-Cookie", "a=b")
        .header("WWW-Authenticate", "Basic realm=x")
        .header("X-Amzn-Trace-Id", "Root=1")
        .header("Server", "fake");
    let r = rig(always(upstream), nothing(), nothing()).await;
    let resp = r.get("/pypi/simple/").await;
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(resp.header("etag"), Some("\"v1\""));
    assert_eq!(
        resp.header("last-modified"),
        Some("Wed, 21 Oct 2026 07:28:00 GMT")
    );
    assert_eq!(resp.header("cache-control"), Some("max-age=60"));
    assert_eq!(resp.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        resp.header("content-disposition"),
        Some("attachment; filename=x.json")
    );
    for name in [
        "set-cookie",
        "www-authenticate",
        "x-amzn-trace-id",
        "server",
    ] {
        assert_eq!(resp.header(name), None, "{name} must not be forwarded");
    }
    let everything = format!("{:?} {}", resp.headers, resp.text());
    assert!(!everything.contains(&r.token));
    assert!(!everything.contains(&b64(format!("aws:{}", r.token).as_bytes())));
}

#[tokio::test]
async fn head_is_sent_upstream_as_head_and_comes_back_without_a_body() {
    let r = rig(
        always(
            Reply::status(200)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "1234"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = head(r.addr(), "/maven/org/x/y/1.0/y-1.0.jar").await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-length"), Some("1234"));
    assert!(resp.body.is_empty());
    let seen = &r.ca.seen()[0];
    assert_eq!(seen.method, "HEAD");
    assert_eq!(seen.target, "/maven/packages/org/x/y/1.0/y-1.0.jar");
}

#[tokio::test]
async fn partial_content_passes_through_with_its_status_and_content_range() {
    let mut partial = Reply::ok("01234")
        .header("Content-Type", "application/octet-stream")
        .header("Content-Range", "bytes 0-4/10")
        .header("Accept-Ranges", "bytes");
    partial.status = 206;
    let r = rig(always(partial), nothing(), nothing()).await;
    let resp = get_with(r.addr(), "/maven/a/b.bin", &[("Range", "bytes=0-4")]).await;
    assert_eq!(resp.status, 206);
    assert_eq!(resp.header("content-range"), Some("bytes 0-4/10"));
    assert_eq!(resp.body, b"01234");
    assert_eq!(r.ca.seen()[0].header("range"), Some("bytes=0-4"));
}

#[tokio::test]
async fn upstream_statuses_pass_through() {
    let r = rig(
        handler(|s| match s.target.as_str() {
            t if t.ends_with("/missing") => Reply::status(404).header("Content-Type", "text/plain"),
            t if t.ends_with("/same") => Reply::status(304).header("ETag", "\"v1\""),
            t if t.ends_with("/denied") => Reply::status(403),
            _ => Reply::status(500),
        }),
        nothing(),
        nothing(),
    )
    .await;
    assert_eq!(r.get("/pypi/missing").await.status, 404);
    let same = r.get("/pypi/same").await;
    assert_eq!((same.status, same.header("etag")), (304, Some("\"v1\"")));
    assert_eq!(r.get("/pypi/denied").await.status, 403);
    assert_eq!(r.get("/pypi/boom").await.status, 500);
}

#[tokio::test]
async fn a_percent_encoded_path_reaches_the_upstream_as_the_client_wrote_it() {
    let r = rig(always(json_ok()), nothing(), always(Reply::ok("x"))).await;
    for (path, upstream) in [
        ("/npm/%40scope%2fpkg", "/npm/packages/%40scope%2fpkg"),
        ("/npm/a%7Eb", "/npm/packages/a%7Eb"),
        (
            "/pypi/simple/torch-2.1.0%2Bcpu-cp311-linux_x86_64.whl",
            "/pypi/packages/simple/torch-2.1.0%2Bcpu-cp311-linux_x86_64.whl",
        ),
        ("/pypi/simple/a+b/", "/pypi/packages/simple/a+b/"),
        ("/pypi/simple/x?", "/pypi/packages/simple/x?"),
        ("/maven/a/b%2Bc/d", "/maven/packages/a/b%2Bc/d"),
    ] {
        assert_eq!(r.get(path).await.status, 200, "{path}");
        assert_eq!(r.ca.seen().last().unwrap().target, upstream, "{path}");
    }
    assert_eq!(
        r.get("/go/github.com/!burnt!sushi/toml/@v/v1.0.0.mod")
            .await
            .status,
        200
    );
    assert_eq!(
        r.go.seen().last().unwrap().target,
        "/github.com/!burnt!sushi/toml/@v/v1.0.0.mod"
    );
}

#[tokio::test]
async fn a_bare_prefix_and_a_trailing_slash_reach_the_base() {
    let r = rig(always(json_ok()), nothing(), nothing()).await;
    assert_eq!(r.get("/pypi").await.status, 200);
    assert_eq!(r.get("/pypi/").await.status, 200);
    let seen = r.ca.seen();
    assert_eq!(seen[0].target, "/pypi/packages");
    assert_eq!(seen[1].target, "/pypi/packages/");
}

#[tokio::test]
async fn the_counters_and_log_lines_describe_every_answer_and_never_a_secret() {
    let r = rig(
        handler(|s| {
            if s.target.contains("missing") {
                Reply::status(404)
            } else {
                Reply::ok("hello").header("Content-Type", "application/octet-stream")
            }
        }),
        nothing(),
        always(Reply::ok("gogogo")),
    )
    .await;
    r.get("/pypi/a").await;
    r.get("/pypi/missing").await;
    r.get("/npm/b").await;
    r.get("/go/c").await;
    r.get("/nope/x").await; // refused: unknown prefix
    let post = send_raw(
        r.addr(),
        &request("POST", "/pypi/a", r.addr(), &[("Content-Length", "0")]),
        false,
    )
    .await;
    assert_eq!(post.status, 405);

    // The log line is written when the body ends; give the last one a moment.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let s = r.proxy.snapshot();
    assert_eq!(s.requests, 6);
    assert_eq!(s.refused, 2, "the unknown prefix and the POST");
    assert_eq!(
        s.by_prefix.get("pypi"),
        Some(&2),
        "only requests that got as far as a configured prefix count; the refused POST did not: {s:?}"
    );
    assert_eq!(s.by_prefix.len(), 3, "{s:?}");
    assert_eq!(s.by_prefix.get("npm"), Some(&1));
    assert_eq!(s.by_prefix.get("go"), Some(&1));
    assert_eq!(s.statuses.get(&200), Some(&3));
    assert_eq!(s.statuses.get(&404), Some(&1));
    assert_eq!(s.statuses.get(&403), Some(&1), "the unknown prefix");
    assert_eq!(s.statuses.get(&405), Some(&1), "the POST");
    assert!(
        s.bytes >= 5 + 5 + 6,
        "bytes counts the body bytes sent to the client: {s:?}"
    );

    let re = |l: &str| {
        let parts: Vec<&str> = l.split(' ').collect();
        parts.len() == 5
            && parts[0] == "pkgproxy"
            && ["pypi", "npm", "maven", "go", "-"].contains(&parts[1])
            && parts[2].parse::<u16>().is_ok()
            && parts[3].parse::<u64>().is_ok()
            && parts[4].parse::<u64>().is_ok()
    };
    let lines = r.log.lines();
    assert_eq!(lines.len(), 6, "{lines:?}");
    for l in &lines {
        assert!(re(l), "log line shape: {l:?}");
        assert!(!l.contains(&r.token));
    }
}

#[tokio::test]
async fn keep_alive_reuses_the_client_and_the_upstream_connection() {
    let r = rig(
        always(Reply::ok("ok").header("Content-Type", "application/octet-stream")),
        nothing(),
        nothing(),
    )
    .await;
    let addr = r.addr();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    for i in 0..3 {
        let raw = format!(
            "GET /pypi/f{i} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            addr.port()
        );
        tokio::io::AsyncWriteExt::write_all(&mut s, raw.as_bytes())
            .await
            .unwrap();
        let resp = read_response(&mut s, false).await;
        assert_eq!((resp.status, resp.body.as_slice()), (200, b"ok".as_slice()));
    }
    assert_eq!(r.ca.count(), 3);
    assert_eq!(
        r.ca.connections(),
        1,
        "three requests used one upstream connection"
    );
}

#[tokio::test]
async fn two_pipelined_requests_get_two_answers_in_order() {
    let r = rig(
        handler(|s| Reply::ok(s.target.clone()).header("Content-Type", "application/octet-stream")),
        nothing(),
        nothing(),
    )
    .await;
    let addr = r.addr();
    let one = format!(
        "GET /pypi/one HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        addr.port()
    );
    let two = format!(
        "GET /pypi/two HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut s, format!("{one}{two}").as_bytes())
        .await
        .unwrap();
    let mut all = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::io::AsyncReadExt::read_to_end(&mut s, &mut all),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&all).to_string();
    assert_eq!(text.matches("HTTP/1.1 200").count(), 2, "{text}");
    let (i, j) = (
        text.find("/pypi/packages/one").unwrap(),
        text.find("/pypi/packages/two").unwrap(),
    );
    assert!(i < j, "answers arrive in request order: {text}");
}

#[tokio::test]
async fn trailers_from_the_upstream_never_reach_the_client() {
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::WithTrailers {
                    body: Bytes::from_static(b"payload"),
                    trailers: vec![
                        ("X-Extra".into(), "yes".into()),
                        ("Set-Cookie".into(), "a=b".into()),
                    ],
                })
                .header("Content-Type", "application/octet-stream")
                .header("Trailer", "X-Extra, Set-Cookie")
                .chunked(),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let addr = r.addr();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(
        &mut s,
        &request("GET", "/pypi/t", addr, &[("TE", "trailers")]),
    )
    .await
    .unwrap();
    let mut all = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::io::AsyncReadExt::read_to_end(&mut s, &mut all),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&all).to_ascii_lowercase();
    assert!(text.contains("payload"), "{text}");
    for leaked in ["x-extra", "set-cookie", "trailer:"] {
        assert!(
            !text.contains(leaked),
            "{leaked} reached the client: {text}"
        );
    }
    // The upstream really sent them (the fake saw the request and answered with a Trailer header).
    assert_eq!(r.ca.count(), 1);
}

// -- The Basic credential and the client's own Authorization (mutants S07 and R08 of the 2026-10-13 review) -----------------

/// A token whose `aws:<token>` Basic encoding holds both `+` and `/`, the two characters the standard base64 alphabet has and the
/// URL-safe one does not. A real CodeArtifact token is a long opaque string, so its encoding almost always holds them.
fn token_with_plus_and_slash_in_its_encoding() -> String {
    [">>>>", ">>>>", ">>>>", "????", "????", "????"].concat()
}

#[tokio::test]
async fn the_basic_credential_uses_the_standard_base64_alphabet_for_a_token_that_needs_plus_and_slash()
 {
    let tok = token_with_plus_and_slash_in_its_encoding();
    // TRIGGER PRESENT: the encoding of this token differs between the standard and the URL-safe alphabet.
    let std_enc = b64(format!("aws:{tok}").as_bytes());
    let url_safe = std_enc.replace('+', "-").replace('/', "_");
    assert_ne!(std_enc, url_safe);
    let mut o = Opts::new(always(json_ok()), nothing(), nothing());
    o.token = Some(tok.clone());
    let r = rig_with(o).await;
    assert_eq!(r.get("/pypi/simple/x/").await.status, 200);
    assert_eq!(r.get("/maven/org/x/x-1.0.pom").await.status, 200);
    assert_eq!(r.get("/npm/x").await.status, 200);
    let ca = r.ca.seen();
    assert_eq!(ca.len(), 3);
    let want = format!("Basic {std_enc}");
    assert_eq!(ca[0].header("authorization"), Some(want.as_str()), "pypi");
    assert_eq!(ca[1].header("authorization"), Some(want.as_str()), "maven");
    assert_eq!(
        ca[2].header("authorization"),
        Some(format!("Bearer {tok}").as_str()),
        "npm sends the token itself, not an encoding of it"
    );
}

#[tokio::test]
async fn the_clients_authorization_and_cookie_never_reach_any_upstream_on_any_path() {
    // The credentialed prefixes overwrite `Authorization`, so only the anonymous Go prefix and the redirect hops show whether the
    // client's own header was dropped first: a hop to S3 gets the forwarded headers, and must get no `Authorization`.
    let client_auth = format!("Basic {}", b64(b"client:value"));
    let extra = [
        ("Authorization", client_auth.as_str()),
        ("Cookie", "session=abc"),
        ("Proxy-Authorization", "Basic abc"),
    ];
    let to_s3 = Reply::redirect(302, &format!("https://{S3_HOST}/bucket/key"));
    let r = rig(
        handler(move |s| {
            if s.target.starts_with("/pypi/packages/to-s3") {
                to_s3.clone()
            } else {
                json_ok()
            }
        }),
        always(json_ok()),
        always(json_ok()),
    )
    .await;
    for path in [
        "/pypi/simple/x/",
        "/npm/x",
        "/maven/org/x/x-1.0.pom",
        "/go/example.com/m/@v/list",
        "/pypi/to-s3",
    ] {
        assert_eq!(get_with(r.addr(), path, &extra).await.status, 200, "{path}");
    }
    let mut all = Vec::new();
    all.extend(r.ca.seen());
    all.extend(r.s3.seen());
    all.extend(r.go.seen());
    assert_eq!(all.len(), 6, "five requests and one redirect hop");
    for s in &all {
        let shown = format!("{:?}", s.headers);
        assert!(
            !shown.contains(&b64(b"client:value")),
            "{}: {shown}",
            s.target
        );
        assert_eq!(s.header("cookie"), None, "{}", s.target);
        assert_eq!(s.header("proxy-authorization"), None, "{}", s.target);
    }
    assert_eq!(r.go.seen()[0].header("authorization"), None);
    assert_eq!(r.s3.seen()[0].header("authorization"), None);
    for s in r.ca.seen() {
        assert_eq!(s.headers_named("authorization").len(), 1, "{}", s.target);
    }
}
