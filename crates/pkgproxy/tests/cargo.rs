//! The `cargo` prefix: the sparse index and the crate downloads of a CodeArtifact cargo repository reach the
//! engine's `cargo` through the proxy. The upstream takes the token as the WHOLE `Authorization` value (`Bearer <token>` answers 401,
//! measured on stage direct, 2026-10-25), so the prefix has its own credential shape, `Auth::Raw`. The client's own credentials never
//! reach the upstream, the validators of the sparse protocol pass in both directions, and every rule of the other prefixes still holds
//! (path rules, expiry, redirects).
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use pkgproxy::{Auth, Prefix, Secret, codeartifact_upstreams};
use std::sync::atomic::Ordering;

fn crate_bytes() -> Reply {
    Reply::ok("CRATE-BYTES")
        .header("Content-Type", "application/gzip")
        .header("Content-Disposition", "attachment")
}

/// The `config.json` of the stage's cargo repository, as measured on 2026-10-25 (the host is the fake's).
fn upstream_config() -> Reply {
    Reply::ok(format!(
        "{{\"dl\":\"https://{CA_HOST}/cargo/packages/crates/{{crate}}/{{version}}\",\"api\":\"https://{CA_HOST}/cargo/packages/-\",\"auth-required\":true}}"
    ))
    .header("Content-Type", "application/json")
}

/// What CodeArtifact answers for a cargo path when the token is the raw `Authorization` value, and 401 for anything else.
fn codeartifact_cargo(tok: String) -> Handler {
    handler(move |s| {
        if s.header("authorization") != Some(tok.as_str()) {
            return Reply::status(401).header("WWW-Authenticate", "Basic realm=\"codeartifact\"");
        }
        Reply::ok("{\"name\":\"itoa\",\"vers\":\"1.0.0\"}\n")
            .header("Content-Type", "application/octet-stream")
            .header("ETag", "\"idx-1\"")
            .header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
    })
}

#[test]
fn codeartifact_upstreams_has_a_cargo_upstream_with_the_raw_credential() {
    let ups = codeartifact_upstreams(
        "dom-123.d.codeartifact.us-east-1.amazonaws.com",
        "packages",
        Secret::new("t".into()),
    );
    assert_eq!(ups.len(), 4);
    let cargo = ups.iter().find(|u| u.prefix == Prefix::Cargo).unwrap();
    assert_eq!(
        cargo.base,
        "https://dom-123.d.codeartifact.us-east-1.amazonaws.com/cargo/packages"
    );
    assert!(
        matches!(&cargo.auth, Auth::Raw(_)),
        "the token is the whole Authorization value"
    );
    assert_eq!(format!("{:?}", cargo.auth), "Raw(<redacted>)");
}

#[tokio::test]
async fn every_sparse_index_path_form_reaches_the_upstream_with_the_raw_token() {
    let r = rig(codeartifact_cargo(token()), nothing(), nothing()).await;
    let forms = [
        "/cargo/1/a",
        "/cargo/2/ab",
        "/cargo/3/a/abc",
        "/cargo/it/oa/itoa",
        "/cargo/se/rd/serde",
        "/cargo/pr/oc/proc-macro2",
        "/cargo/ti/ny/tiny_http",
    ];
    for p in forms {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 200, "{p}: {}", resp.text());
        assert!(resp.text().contains("itoa"), "{p}");
    }
    let seen = r.ca.seen();
    let targets: Vec<&str> = seen.iter().map(|s| s.target.as_str()).collect();
    assert_eq!(
        targets,
        vec![
            "/cargo/packages/1/a",
            "/cargo/packages/2/ab",
            "/cargo/packages/3/a/abc",
            "/cargo/packages/it/oa/itoa",
            "/cargo/packages/se/rd/serde",
            "/cargo/packages/pr/oc/proc-macro2",
            "/cargo/packages/ti/ny/tiny_http",
        ]
    );
    for s in &seen {
        assert_eq!(
            s.headers_named("authorization"),
            vec![r.token.as_str()],
            "exactly one Authorization header, the token as it is: no Bearer, no Basic ({})",
            s.target
        );
    }
}

#[tokio::test]
async fn the_client_credentials_never_reach_the_upstream_and_the_sparse_validators_do() {
    let r = rig(codeartifact_cargo(token()), nothing(), nothing()).await;
    let client_cred = b64(b"client:value");
    let resp = get_with(
        r.addr(),
        "/cargo/it/oa/itoa",
        &[
            ("Authorization", &format!("Basic {client_cred}")),
            ("Cookie", "session=abc"),
            ("Proxy-Authorization", "Basic abc"),
            ("Proxy-Connection", "keep-alive"),
            ("Accept", "*/*"),
            ("User-Agent", "cargo 1.98.1 test"),
            ("If-None-Match", "\"idx-0\""),
            ("If-Modified-Since", "Tue, 20 Oct 2026 07:28:00 GMT"),
            ("Cache-Control", "no-cache"),
            ("Accept-Encoding", "gzip"),
        ],
    )
    .await;
    assert_eq!(resp.status, 200);
    let seen = &r.ca.seen()[0];
    assert_eq!(
        seen.headers_named("authorization"),
        vec![r.token.as_str()],
        "the proxy's credential only"
    );
    for name in ["cookie", "proxy-authorization", "proxy-connection"] {
        assert_eq!(seen.header(name), None, "{name} reached the upstream");
    }
    assert!(!format!("{:?}", seen.headers).contains(&client_cred));
    assert_eq!(seen.header("if-none-match"), Some("\"idx-0\""));
    assert_eq!(
        seen.header("if-modified-since"),
        Some("Tue, 20 Oct 2026 07:28:00 GMT")
    );
    assert_eq!(seen.header("user-agent"), Some("cargo 1.98.1 test"));
    assert_eq!(seen.header("accept-encoding"), Some("identity"));
    // The validators and the type come back; nothing else of the upstream's does.
    assert_eq!(resp.header("etag"), Some("\"idx-1\""));
    assert_eq!(
        resp.header("last-modified"),
        Some("Wed, 21 Oct 2026 07:28:00 GMT")
    );
    assert_eq!(
        resp.header("content-type"),
        Some("application/octet-stream")
    );
}

#[tokio::test]
async fn a_conditional_request_that_the_upstream_answers_304_comes_back_304_with_its_validators() {
    let r = rig(
        handler(|s| {
            if s.header("if-none-match") == Some("\"idx-1\"") {
                Reply::status(304)
                    .header("ETag", "\"idx-1\"")
                    .header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
            } else {
                Reply::ok("full").header("ETag", "\"idx-1\"")
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    let again = get_with(
        r.addr(),
        "/cargo/it/oa/itoa",
        &[("If-None-Match", "\"idx-1\"")],
    )
    .await;
    assert_eq!(again.status, 304);
    assert!(again.body.is_empty());
    assert_eq!(again.header("etag"), Some("\"idx-1\""));
    assert_eq!(
        again.header("last-modified"),
        Some("Wed, 21 Oct 2026 07:28:00 GMT")
    );
    let fresh = r.get("/cargo/it/oa/itoa").await;
    assert_eq!((fresh.status, fresh.text().as_str()), (200, "full"));
}

#[tokio::test]
async fn a_crate_download_streams_a_body_larger_than_one_buffer_with_its_headers() {
    let total: u64 = 3 * 1024 * 1024 + 17;
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::Gen { total })
                .header("Content-Type", "application/gzip")
                .header("Content-Disposition", "attachment"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/cargo/crates/serde/1.0.228").await;
    assert_eq!(resp.status, 200);
    assert!(resp.complete);
    assert_eq!(resp.body.len() as u64, total);
    assert!(resp.body.iter().all(|&b| b == b'x'));
    assert_eq!(
        resp.header("content-length").map(str::to_string),
        Some(total.to_string())
    );
    assert_eq!(resp.header("content-type"), Some("application/gzip"));
    assert_eq!(
        resp.header("content-disposition"),
        None,
        "cargo does not read it, so it is not passed"
    );
    let seen = r.ca.seen();
    assert_eq!(seen[0].target, "/cargo/packages/crates/serde/1.0.228");
    assert_eq!(
        seen[0].headers_named("authorization"),
        vec![r.token.as_str()]
    );
    let head = head(r.addr(), "/cargo/crates/serde/1.0.228").await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
}

#[tokio::test]
async fn a_crate_name_with_a_dash_an_underscore_and_a_version_with_a_build_tag_is_served() {
    let r = rig(always(crate_bytes()), nothing(), nothing()).await;
    for p in [
        "/cargo/crates/proc-macro2/1.0.101",
        "/cargo/crates/tiny_http/0.12.0",
        "/cargo/crates/openssl-src/300.5.4+3.5.4",
        "/cargo/crates/foo/1.0.0-rc.1",
    ] {
        let resp = r.get(p).await;
        assert_eq!(
            (resp.status, resp.text().as_str()),
            (200, "CRATE-BYTES"),
            "{p}"
        );
    }
}

#[tokio::test]
async fn the_statuses_of_the_upstream_pass_through_and_its_challenge_does_not() {
    let r = rig(
        handler(|s| match s.target.as_str() {
            t if t.ends_with("/missing") => Reply::status(404),
            t if t.ends_with("/denied") => Reply::status(403),
            t if t.ends_with("/stale") => {
                Reply::status(401).header("WWW-Authenticate", "Basic realm=\"x\"")
            }
            _ => Reply::status(500).header("Set-Cookie", "a=b"),
        }),
        nothing(),
        nothing(),
    )
    .await;
    assert_eq!(r.get("/cargo/mi/ss/missing").await.status, 404);
    assert_eq!(r.get("/cargo/de/ni/denied").await.status, 403);
    let stale = r.get("/cargo/st/al/stale").await;
    assert_eq!(stale.status, 401);
    assert_eq!(stale.header("www-authenticate"), None);
    let other = r.get("/cargo/ot/he/other").await;
    assert_eq!(other.status, 500);
    assert_eq!(other.header("set-cookie"), None);
}

#[tokio::test]
async fn an_expired_token_answers_401_on_the_cargo_prefix_without_asking_the_upstream() {
    let mut o = Opts::new(
        handler(|s| {
            if s.target.ends_with("/config.json") {
                upstream_config()
            } else {
                crate_bytes()
            }
        }),
        nothing(),
        nothing(),
    );
    o.expires_at_unix = Some(5_100);
    o.clock_now = 5_000;
    let r = rig_with(o).await;
    assert_eq!(r.get("/cargo/config.json").await.status, 200);
    assert_eq!(r.get("/cargo/it/oa/itoa").await.status, 200);
    let before = r.ca.count();
    r.clock.store(5_100, Ordering::SeqCst);
    for p in ["/cargo/it/oa/itoa", "/cargo/crates/itoa/1.0.0"] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 401, "{p}");
        assert_eq!(resp.text().trim(), "package token expired");
    }
    // The registry config is the one path where cargo reads a 401 as "needs a credential provider" (measured with cargo 1.98.1), so
    // it is refused with a 403 and the same words.
    let config = r.get("/cargo/config.json").await;
    assert_eq!(config.status, 403);
    assert_eq!(config.text().trim(), "package token expired");
    assert_eq!(r.ca.count(), before, "an expired token is refused locally");
}

#[tokio::test]
async fn a_cargo_path_outside_the_forms_cargo_asks_for_is_refused_before_the_upstream() {
    let r = rig(always(crate_bytes()), nothing(), nothing()).await;
    for p in [
        // the registry's API base and the owner's other endpoints
        "/cargo/-/api/v1/crates/new",
        "/cargo/api/v1/crates",
        // a crate download with the wrong number of segments
        "/cargo/crates/itoa",
        "/cargo/crates/itoa/1.0.0/download",
        "/cargo/crates",
        "/cargo/crates/",
        // an index form with the wrong number of segments, or an empty or odd name
        "/cargo/it/oa",
        "/cargo/a/b/c/d/e",
        "/cargo",
        "/cargo/",
        "/cargo/config.json/",
        "/cargo/config.json/x",
        // directories that do not belong to the name
        "/cargo/xx/yy/itoa",
        "/cargo/1/ab",
        "/cargo/2/a",
        "/cargo/2/abc",
        "/cargo/3/a/bcd",
        "/cargo/3/ab/abc",
        "/cargo/It/oa/Itoa",
        // characters a crate name or version never holds
        "/cargo/it/oa/it%61",
        "/cargo/it/oa/it;a",
        "/cargo/it/oa/it@a",
        "/cargo/crates/it:oa/1.0.0",
        "/cargo/crates/itoa/1.0.0!",
    ] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 400, "{p}: {}", resp.text());
    }
    assert_eq!(r.ca.count(), 0, "none of them reached the upstream");
}

#[tokio::test]
async fn the_shared_path_rules_still_apply_to_the_cargo_prefix() {
    let r = rig(always(crate_bytes()), nothing(), nothing()).await;
    for p in [
        "/cargo/../x",
        "/cargo/it/../itoa",
        "/cargo/%2e%2e/x",
        "/cargo/it//itoa",
        "/cargo/it/oa/%00",
        "/cargo/it/oa/it%5coa",
        "/cargo/it/oa/%zz",
    ] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 400, "{p}");
    }
    let long = format!("/cargo/it/oa/{}", "a".repeat(2100));
    assert_eq!(r.get(&long).await.status, 400);
    assert_eq!(r.ca.count(), 0);
}

#[tokio::test]
async fn a_query_on_the_cargo_prefix_is_refused() {
    let r = rig(always(crate_bytes()), nothing(), nothing()).await;
    for p in [
        "/cargo/it/oa/itoa?x=1",
        "/cargo/crates/itoa/1.0.0?x=1",
        "/cargo/config.json?",
    ] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 400, "{p}");
        assert_eq!(resp.text().trim(), "no query is accepted on this prefix");
    }
    assert_eq!(r.ca.count(), 0);
}

#[tokio::test]
async fn only_get_and_head_are_accepted_on_the_cargo_prefix() {
    let r = rig(always(crate_bytes()), nothing(), nothing()).await;
    for m in ["PUT", "POST", "DELETE", "PATCH", "OPTIONS"] {
        let resp = send_raw(
            r.addr(),
            &request(m, "/cargo/-/api/v1/crates/new", r.addr(), &[]),
            false,
        )
        .await;
        assert_eq!(resp.status, 405, "{m}");
        assert_eq!(resp.header("allow"), Some("GET, HEAD"));
    }
    assert_eq!(r.ca.count(), 0);
}

#[tokio::test]
async fn a_crate_redirected_to_s3_is_followed_without_the_credential_and_other_hosts_are_refused() {
    let r = rig(
        always(Reply::redirect(
            302,
            &format!("https://{S3_HOST}/bucket/serde-1.0.228.crate?X-Amz-Signature=abc"),
        )),
        always(Reply::ok("CRATE-BYTES").header("Content-Type", "application/gzip")),
        nothing(),
    )
    .await;
    let resp = r.get("/cargo/crates/serde/1.0.228").await;
    assert_eq!((resp.status, resp.text().as_str()), (200, "CRATE-BYTES"));
    assert_eq!(
        r.ca.seen()[0].headers_named("authorization"),
        vec![r.token.as_str()]
    );
    let s3 = r.s3.seen();
    assert_eq!(s3.len(), 1);
    assert_eq!(s3[0].header("authorization"), None);
    assert_eq!(resp.header("location"), None);

    let bad = rig(
        always(Reply::redirect(302, "https://decoy.example.com/x.crate")),
        nothing(),
        nothing(),
    )
    .await;
    let resp = bad.get("/cargo/crates/serde/1.0.228").await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.text().trim(), "redirect refused");
}

#[tokio::test]
async fn the_token_is_in_no_answer_log_line_or_error_of_the_cargo_prefix() {
    let r = rig(
        handler(|s| {
            if s.target.contains("boom") {
                Reply::aborted()
            } else if s.target.ends_with("/config.json") {
                upstream_config()
            } else {
                crate_bytes()
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    let mut seen = Vec::new();
    for p in [
        "/cargo/config.json",
        "/cargo/it/oa/itoa",
        "/cargo/crates/itoa/1.0.0",
        "/cargo/crates/boom/1.0.0",
        "/cargo/-/x",
    ] {
        let resp = r.get(p).await;
        seen.push(format!("{:?} {}", resp.headers, resp.text()));
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    seen.extend(r.log.lines());
    seen.push(format!("{:?}", r.proxy.snapshot()));
    for s in seen {
        assert!(!s.contains(&r.token), "{s}");
    }
    let snap = r.proxy.snapshot();
    assert!(
        snap.by_prefix.get("cargo").copied().unwrap_or(0) >= 3,
        "{snap:?}"
    );
}

// ---- fix round of the security review: what an index or crate answer may carry, and where the token may go ----------------------------

/// Only the headers cargo's sparse protocol reads come back from an index or crate answer; the framing is the proxy's own.
#[tokio::test]
async fn an_index_or_crate_answer_carries_only_the_headers_cargo_reads() {
    let r = rig(
        always(
            Reply::ok("INDEX-LINES\n")
                .header("Content-Type", "application/octet-stream")
                .header("ETag", "\"idx\"")
                .header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
                .header("Cache-Control", "max-age=60")
                .header("Content-Disposition", "attachment; filename=\"x\"")
                .header("Accept-Ranges", "bytes")
                .header("Set-Cookie", "a=b")
                .header("WWW-Authenticate", "Basic realm=\"x\"")
                .header("X-Amz-Request-Id", "req-1")
                .header("Link", "<https://elsewhere.example/>; rel=next"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    for p in ["/cargo/it/oa/itoa", "/cargo/crates/itoa/1.0.0"] {
        let resp = r.get(p).await;
        assert_eq!(
            (resp.status, resp.text().as_str()),
            (200, "INDEX-LINES\n"),
            "{p}"
        );
        let mut names: Vec<String> = resp
            .headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .filter(|k| !matches!(k.as_str(), "date" | "content-length" | "connection"))
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["content-type", "etag", "last-modified"],
            "{p}: {:?}",
            resp.headers
        );
    }
}

/// Cargo never asks for a range; the proxy sends none up, and an answer that is a part is refused.
#[tokio::test]
async fn a_range_is_not_sent_up_and_a_partial_answer_is_refused_for_an_index_or_a_crate() {
    let r = rig(
        handler(|s| match s.target.as_str() {
            t if t.ends_with("/part") => Reply::status(206)
                .header("Content-Range", "bytes 0-3/10")
                .plan(Plan::Fixed("PART".into())),
            t if t.ends_with("/whole-says-range") => {
                Reply::ok("WHOLE").header("Content-Range", "bytes 0-4/5")
            }
            _ => Reply::ok("FULL"),
        }),
        nothing(),
        nothing(),
    )
    .await;
    let resp = get_with(
        r.addr(),
        "/cargo/it/oa/itoa",
        &[("Range", "bytes=0-1"), ("If-Range", "\"x\"")],
    )
    .await;
    assert_eq!((resp.status, resp.text().as_str()), (200, "FULL"));
    let seen = &r.ca.seen()[0];
    assert_eq!(seen.header("range"), None);
    assert_eq!(seen.header("if-range"), None);
    for p in ["/cargo/pa/rt/part", "/cargo/wh/ol/whole-says-range"] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 502, "{p}: {}", resp.text());
        assert_eq!(
            resp.text().trim(),
            "unexpected answer from the package registry"
        );
        assert_eq!(resp.header("content-range"), None);
    }
}

/// An index or crate body is opaque: never read whole for a rewrite, whatever its type says, and never changed. The real service says
/// `application/octet-stream`; a JSON or text type must not send it through the rewrite of pypi and npm (a 64 MiB cap, four at a time).
#[tokio::test]
async fn an_index_body_is_streamed_opaque_whatever_its_content_type() {
    // A line that names the upstream, as an index could: left as it is.
    let line =
        format!("{{\"name\":\"x\",\"url\":\"https://{CA_HOST}/cargo/packages/crates/x/1.0.0\"}}\n");
    for ct in [
        "application/octet-stream",
        "application/json",
        "text/plain",
        "text/html",
    ] {
        let r = rig(
            always(Reply::ok(line.clone()).header("Content-Type", ct)),
            nothing(),
            nothing(),
        )
        .await;
        let resp = r.get("/cargo/it/oa/itoa").await;
        assert_eq!(resp.text(), line, "{ct}");
        assert_eq!(resp.header("content-type"), Some(ct));
    }
}

/// Larger than the rewrite cap of 64 MiB, chunked (the real index has no length), typed as JSON: it would be a 502 if it were buffered. And
/// more of them at once than the four rewrite slots, none of which waits for one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_index_of_seventy_mib_streams_through_chunked_as_json_and_takes_no_rewrite_slot() {
    let total: u64 = 70 * 1024 * 1024;
    let r = rig(
        handler(move |s| {
            let size = if s.target.ends_with("/serde") {
                total
            } else {
                5 * 1024 * 1024
            };
            Reply::ok("")
                .plan(Plan::Gen { total: size })
                .chunked()
                .header("Content-Type", "application/json")
        }),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/cargo/se/rd/serde").await;
    assert_eq!(resp.status, 200);
    assert!(resp.complete);
    assert_eq!(resp.body.len() as u64, total);
    assert_eq!(resp.header("content-length"), None, "chunked stays chunked");
    let addr = r.addr();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        tasks.push(tokio::spawn(async move {
            get(addr, "/cargo/aw/s-/aws-sdk-ec2").await
        }));
    }
    for t in tasks {
        let resp = t.await.unwrap();
        assert_eq!((resp.status, resp.body.len()), (200, 5 * 1024 * 1024));
    }
}

/// The token goes to the upstream's own origin only, on `Auth::Raw` as on the other shapes: a redirect from any cargo path to another
/// `amazonaws.com` host is followed without it, and a redirect inside the origin keeps it.
#[tokio::test]
async fn a_cross_origin_redirect_from_a_cargo_path_never_carries_the_token() {
    let r = rig(
        handler(|s| {
            if s.target.ends_with("/config.json") {
                Reply::redirect(
                    302,
                    &format!("https://{S3_HOST}/bucket/config.json?sig=abc"),
                )
            } else if s.target.ends_with("/itoa") {
                Reply::redirect(307, &format!("https://{S3_HOST}/bucket/itoa?sig=abc"))
            } else if s.target.ends_with("/same") {
                Reply::redirect(302, "/cargo/packages/sa/me/final")
            } else if s.target.ends_with("/final") {
                Reply::ok("FINAL")
            } else {
                Reply::status(404)
            }
        }),
        handler(|s| {
            if s.target.starts_with("/bucket/config.json") {
                Reply::ok("{\"dl\":\"https://elsewhere.example/\",\"auth-required\":true}")
            } else {
                Reply::ok("INDEX-FROM-S3")
            }
        }),
        nothing(),
    )
    .await;
    let index = r.get("/cargo/it/oa/itoa").await;
    assert_eq!(
        (index.status, index.text().as_str()),
        (200, "INDEX-FROM-S3")
    );
    let config = r.get("/cargo/config.json").await;
    assert_eq!(config.status, 200, "{}", config.text());
    assert!(
        config.text().contains("\"auth-required\":false"),
        "{}",
        config.text()
    );
    assert!(
        !config.text().contains("elsewhere.example"),
        "{}",
        config.text()
    );
    let s3 = r.s3.seen();
    assert_eq!(s3.len(), 2);
    for s in &s3 {
        assert_eq!(s.header("authorization"), None, "{}", s.target);
        assert_eq!(s.header("cookie"), None);
    }
    // Inside the origin the token goes on every hop.
    let same = r.get("/cargo/sa/me/same").await;
    assert_eq!((same.status, same.text().as_str()), (200, "FINAL"));
    let own: Vec<_> =
        r.ca.seen()
            .into_iter()
            .filter(|s| s.target.ends_with("/same") || s.target.ends_with("/final"))
            .collect();
    assert_eq!(own.len(), 2);
    for s in own {
        assert_eq!(s.headers_named("authorization"), vec![r.token.as_str()]);
    }
}

/// `Auth::Raw` is a credential like the others: a value that cannot be a header refuses the start, with no secret in the text.
#[tokio::test]
async fn a_raw_credential_that_cannot_be_a_header_is_refused_without_printing_it() {
    use pkgproxy::{Config, Upstream, start};
    struct Quiet;
    impl pkgproxy::Log for Quiet {
        fn line(&self, _t: &str) {}
    }
    let bad = format!("{}\nsecond-line", token());
    let err = start(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstreams: vec![Upstream {
            prefix: Prefix::Cargo,
            base: format!("https://{CA_HOST}/cargo/packages"),
            auth: Auth::Raw(Secret::new(bad)),
        }],
        expires_at_unix: None,
        max_connections: 8,
        request_timeout: std::time::Duration::from_secs(5),
        idle_timeout: std::time::Duration::from_secs(5),
        max_redirects: 5,
        log: std::sync::Arc::new(Quiet),
    })
    .await
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!err.to_string().contains(&token()), "{err}");
}
