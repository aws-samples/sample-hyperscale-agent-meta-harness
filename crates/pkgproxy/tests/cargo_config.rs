//! The registry `config.json` of the `cargo` prefix. CodeArtifact's answer names its own host in `dl` and `api` and says
//! `auth-required: true`; cargo would go around the proxy (the egress list drops that host and the engine has no token). The proxy
//! therefore answers `GET /cargo/config.json` itself: a new object with `dl` pointing back at the proxy and `auth-required` false, and
//! nothing else of what the upstream sent. Only an unencoded, whole 200 that is a JSON object is accepted; any other answer is a fixed
//! error of the proxy and never the upstream's body or headers. The reply is made fresh for each request and never kept, so a failed
//! fetch is never remembered. The client's conditional and range headers are not sent upstream, because the bytes are no longer the
//! upstream's.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

/// The `config.json` of the stage's cargo repository (measured 2026-10-25), with the fake's host.
fn upstream_json() -> String {
    format!(
        "{{\"dl\":\"https://{CA_HOST}/cargo/packages/crates/{{crate}}/{{version}}\",\"api\":\"https://{CA_HOST}/cargo/packages/-\",\"auth-required\":true}}"
    )
}

fn json_reply(body: impl Into<bytes::Bytes>) -> Reply {
    Reply::ok(body)
        .header("Content-Type", "application/json")
        .header("ETag", "\"cfg-upstream\"")
        .header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
        .header("Cache-Control", "max-age=3600")
}

fn parsed(resp: &Resp) -> Value {
    serde_json::from_slice(&resp.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", resp.text()))
}

#[tokio::test]
async fn the_config_is_rewritten_to_point_back_at_the_proxy_and_ask_for_no_credential() {
    let r = rig(always(json_reply(upstream_json())), nothing(), nothing()).await;
    let resp = r.get("/cargo/config.json").await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert_eq!(
        resp.header("content-length").map(str::to_string),
        Some(resp.body.len().to_string()),
        "the length is the rewritten body's"
    );
    let v = parsed(&resp);
    let port = r.addr().port();
    assert_eq!(
        v["dl"],
        format!("http://127.0.0.1:{port}/cargo/crates/{{crate}}/{{version}}"),
        "the download template names the proxy's own port and keeps cargo's markers"
    );
    assert_eq!(
        v["auth-required"],
        Value::Bool(false),
        "cargo must not look for a token"
    );
    assert!(v.get("api").is_none(), "the engine cannot publish: {v}");
    assert_eq!(v.as_object().unwrap().len(), 2, "{v}");
    let all = resp.text();
    assert!(
        !all.contains(CA_HOST) && !all.contains(&r.token),
        "the upstream host and the token stay out of the answer: {all}"
    );
    // The upstream was asked for its own config with the raw token.
    let seen = r.ca.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, "/cargo/packages/config.json");
    assert_eq!(
        seen[0].headers_named("authorization"),
        vec![r.token.as_str()]
    );
}

/// The answer is built new: `dl` and `auth-required` and nothing else of what the upstream sent reaches cargo (fix round: a hostile or odd
/// reply cannot add a key, whatever cargo later learns to read, and cannot hand the engine a value the proxy sent it, the token included).
#[tokio::test]
async fn nothing_but_dl_and_auth_required_of_the_upstream_config_reaches_cargo() {
    let up = json!({
        "dl": format!("https://{CA_HOST}/cargo/packages/crates/{{crate}}/{{version}}"),
        "api": format!("https://{CA_HOST}/cargo/packages/-"),
        "auth-required": true,
        "future-key": {"nested": [1, 2, {"x": null}], "flag": true},
        "debug": token(),
        "name": "packages"
    });
    let r = rig(always(json_reply(up.to_string())), nothing(), nothing()).await;
    let resp = r.get("/cargo/config.json").await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let v = parsed(&resp);
    // The order of the keys is not part of the contract (the workspace build turns on serde_json's preserve_order through aws-smithy).
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["auth-required", "dl"], "{v}");
    assert_eq!(v["auth-required"], Value::Bool(false));
    for leaked in ["future-key", "debug", "name", "api", CA_HOST, &r.token] {
        assert!(!resp.text().contains(leaked), "{leaked} in {}", resp.text());
    }
}

#[tokio::test]
async fn a_config_with_no_dl_or_auth_flag_still_gets_both() {
    let r = rig(always(json_reply("{}")), nothing(), nothing()).await;
    let v = parsed(&r.get("/cargo/config.json").await);
    let port = r.addr().port();
    assert_eq!(
        v["dl"],
        format!("http://127.0.0.1:{port}/cargo/crates/{{crate}}/{{version}}")
    );
    assert_eq!(v["auth-required"], Value::Bool(false));
}

#[tokio::test]
async fn each_proxy_names_its_own_port() {
    let a = rig(always(json_reply(upstream_json())), nothing(), nothing()).await;
    let b = rig(always(json_reply(upstream_json())), nothing(), nothing()).await;
    assert_ne!(a.addr().port(), b.addr().port());
    for r in [&a, &b] {
        let v = parsed(&r.get("/cargo/config.json").await);
        assert!(
            v["dl"]
                .as_str()
                .unwrap()
                .starts_with(&format!("http://127.0.0.1:{}/cargo/", r.addr().port())),
            "{v}"
        );
    }
}

#[tokio::test]
async fn the_upstream_validators_are_not_passed_and_the_clients_conditionals_are_not_sent() {
    let r = rig(
        handler(|s| {
            if s.header("if-none-match").is_some() || s.header("if-modified-since").is_some() {
                Reply::status(304).header("ETag", "\"cfg-upstream\"")
            } else {
                json_reply(upstream_json())
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    let resp = get_with(
        r.addr(),
        "/cargo/config.json",
        &[
            ("If-None-Match", "\"cfg-upstream\""),
            ("If-Modified-Since", "Wed, 21 Oct 2026 07:28:00 GMT"),
            ("Range", "bytes=0-9"),
            ("If-Range", "\"cfg-upstream\""),
        ],
    )
    .await;
    assert_eq!(resp.status, 200, "a rewritten body is never answered 304");
    assert_eq!(parsed(&resp)["auth-required"], Value::Bool(false));
    for name in ["etag", "last-modified", "cache-control", "accept-ranges"] {
        assert_eq!(
            resp.header(name),
            None,
            "{name} describes the upstream's bytes, not these"
        );
    }
    let seen = &r.ca.seen()[0];
    for name in ["if-none-match", "if-modified-since", "range", "if-range"] {
        assert_eq!(seen.header(name), None, "{name} reached the upstream");
    }
}

/// A HEAD answer describes the GET it stands for: the headers of the rewritten answer (a JSON type, no validator, no length that is not its
/// own) and the same refusals, so a HEAD never succeeds where the GET would be refused.
#[tokio::test]
async fn a_head_request_describes_the_rewritten_get_and_is_refused_where_the_get_would_be() {
    let upstream_head = || {
        json_reply("")
            .header("Content-Length", "123")
            .header("Accept-Ranges", "bytes")
            .header("Content-Disposition", "attachment")
    };
    let r = rig(always(upstream_head()), nothing(), nothing()).await;
    let resp = head(r.addr(), "/cargo/config.json").await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
    assert_eq!(resp.header("content-type"), Some("application/json"));
    for name in [
        "content-length",
        "etag",
        "last-modified",
        "cache-control",
        "accept-ranges",
        "content-disposition",
    ] {
        assert_eq!(
            resp.header(name),
            None,
            "{name} describes the upstream's bytes"
        );
    }
    // Encoded, partial and too large: refused as the GET is.
    for refused in [
        upstream_head().header("Content-Encoding", "gzip"),
        upstream_head().header("Content-Range", "bytes 0-9/100"),
        json_reply("").header("Content-Length", "65537"),
    ] {
        let r = rig(always(refused), nothing(), nothing()).await;
        assert_eq!(head(r.addr(), "/cargo/config.json").await.status, 502);
    }
}

#[tokio::test]
async fn a_config_of_exactly_64_kib_is_rewritten_and_one_byte_more_is_refused() {
    let pad = |total: usize| {
        let base = upstream_json();
        // Spaces before the closing brace bring the document to exactly `total` bytes.
        let body = base.trim_end_matches('}').to_string();
        format!("{body}{}}}", " ".repeat(total - body.len() - 1))
    };
    let ok = pad(64 * 1024);
    assert_eq!(ok.len(), 64 * 1024);
    let over = pad(64 * 1024 + 1);
    assert_eq!(over.len(), 64 * 1024 + 1);
    for chunked in [false, true] {
        let mk = |body: String| {
            let mut reply = json_reply(body);
            if chunked {
                reply = reply.chunked();
            }
            reply
        };
        let r = rig(always(mk(ok.clone())), nothing(), nothing()).await;
        let resp = r.get("/cargo/config.json").await;
        assert_eq!(resp.status, 200, "chunked={chunked}: {}", resp.text());
        assert_eq!(parsed(&resp)["auth-required"], Value::Bool(false));
        let r = rig(always(mk(over.clone())), nothing(), nothing()).await;
        let resp = r.get("/cargo/config.json").await;
        assert_eq!(resp.status, 502, "chunked={chunked}");
        assert_eq!(resp.text().trim(), "response too large to rewrite");
    }
}

#[tokio::test]
async fn an_answer_that_is_not_a_json_object_is_refused_and_never_handed_to_cargo() {
    for (label, body) in [
        ("html", "<html><body>sign in</body></html>"),
        ("empty", ""),
        ("truncated", "{\"dl\":\"https://x"),
        ("array", "[1,2,3]"),
        ("string", "\"dl\""),
        ("number", "7"),
        ("null", "null"),
    ] {
        let r = rig(always(json_reply(body)), nothing(), nothing()).await;
        let resp = r.get("/cargo/config.json").await;
        assert_eq!(resp.status, 502, "{label}: {}", resp.text());
        assert_eq!(
            resp.text().trim(),
            "registry config is not a JSON object",
            "{label}"
        );
    }
}

#[tokio::test]
async fn an_encoded_answer_cannot_be_rewritten_and_is_refused() {
    let r = rig(
        always(json_reply(upstream_json()).header("Content-Encoding", "gzip")),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/cargo/config.json").await;
    assert_eq!(resp.status, 502);
    assert_eq!(resp.text().trim(), "encoded text cannot be rewritten");
}

/// Anything but an unencoded, whole 200 is a fixed answer of the proxy and never the upstream's body or headers. Cargo keys its retries
/// on the status, so a 4xx or 5xx keeps its number; a 404 is not "no such crate" for a config, so it is a 502.
#[tokio::test]
async fn any_other_answer_for_the_config_is_a_fixed_error_and_never_the_upstreams_body_or_headers()
{
    let hostile = |status: u16| {
        let mut reply = Reply::status(status)
            .header("Content-Type", "text/plain")
            .header("WWW-Authenticate", "Basic realm=\"x\"")
            .header("Set-Cookie", "a=b")
            .header("ETag", "\"e\"")
            .header("X-Amz-Request-Id", "req-1");
        if status != 204 && status != 304 {
            reply = reply.plan(Plan::Fixed("UPSTREAM-BODY".into()));
        }
        reply
    };
    let cases: [(u16, u16, &str); 14] = [
        (
            400,
            400,
            "package registry answered 400 for the registry config",
        ),
        (
            403,
            403,
            "package registry answered 403 for the registry config",
        ),
        (404, 502, "package registry has no cargo config"),
        (
            429,
            429,
            "package registry answered 429 for the registry config",
        ),
        (
            500,
            500,
            "package registry answered 500 for the registry config",
        ),
        (
            503,
            503,
            "package registry answered 503 for the registry config",
        ),
        (201, 502, "unexpected answer for the registry config"),
        (202, 502, "unexpected answer for the registry config"),
        (204, 502, "unexpected answer for the registry config"),
        (206, 502, "unexpected answer for the registry config"),
        (300, 502, "unexpected answer for the registry config"),
        (304, 502, "unexpected answer for the registry config"),
        (
            410,
            410,
            "package registry answered 410 for the registry config",
        ),
        (
            504,
            504,
            "package registry answered 504 for the registry config",
        ),
    ];
    for (up, ours, words) in cases {
        let r = rig(always(hostile(up)), nothing(), nothing()).await;
        let resp = r.get("/cargo/config.json").await;
        assert_eq!(resp.status, ours, "upstream {up}: {}", resp.text());
        assert_eq!(resp.text().trim(), words, "upstream {up}");
        for name in [
            "www-authenticate",
            "set-cookie",
            "etag",
            "x-amz-request-id",
            "content-range",
        ] {
            assert_eq!(resp.header(name), None, "{name} for upstream {up}");
        }
        // The same for HEAD, which asks the same question.
        let h = head(r.addr(), "/cargo/config.json").await;
        assert_eq!(h.status, ours, "HEAD, upstream {up}");
        assert_eq!(h.header("etag"), None);
    }
}

/// A 200 that says it is a part (a `Content-Range`) is not the whole config.
#[tokio::test]
async fn a_200_that_names_a_range_is_not_a_config_and_is_refused() {
    let r = rig(
        always(json_reply(upstream_json()).header("Content-Range", "bytes 0-9/100")),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/cargo/config.json").await;
    assert_eq!(resp.status, 502, "{}", resp.text());
    assert_eq!(
        resp.text().trim(),
        "unexpected answer for the registry config"
    );
    assert_eq!(head(r.addr(), "/cargo/config.json").await.status, 502);
}

/// The client's `Accept-Encoding` is never forwarded: the proxy asks for the identity coding, so the answer it must rewrite is plain text
/// (the real service answers plain text whatever the client offers, measured 2026-10-25), and what is not plain is refused.
#[tokio::test]
async fn the_config_is_asked_for_in_the_identity_coding_whatever_the_client_offers() {
    let r = rig(always(json_reply(upstream_json())), nothing(), nothing()).await;
    let resp = get_with(
        r.addr(),
        "/cargo/config.json",
        &[
            ("Accept-Encoding", "gzip, deflate, br"),
            ("Range", "bytes=0-9"),
        ],
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let seen = &r.ca.seen()[0];
    assert_eq!(seen.header("accept-encoding"), Some("identity"));
    assert_eq!(seen.header("range"), None);
}

/// The parse and the new body use the same admission control as a pypi or npm rewrite: when every slot is held, the config waits one idle
/// period and gets 503, which cargo retries.
#[tokio::test]
async fn the_config_waits_for_a_rewrite_slot_like_any_other_rewrite() {
    let mut o = Opts::new(
        handler(|s| {
            if s.target.ends_with("/config.json") {
                json_reply(upstream_json())
            } else {
                Reply::ok("")
                    .plan(Plan::Trickle {
                        gap: Duration::from_millis(100),
                        count: 12,
                    })
                    .header("Content-Type", "text/plain")
            }
        }),
        nothing(),
        nothing(),
    );
    o.idle_timeout = Duration::from_millis(400);
    let r = rig_with(o).await;
    let addr = r.addr();
    let mut holders = Vec::new();
    for i in 0..4 {
        holders.push(tokio::spawn(async move {
            get(addr, &format!("/pypi/slow{i}")).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let busy = get(addr, "/cargo/config.json").await;
    assert_eq!(busy.status, 503, "{}", busy.text());
    assert!(busy.text().contains("busy"));
    for h in holders {
        assert_eq!(h.await.unwrap().status, 200);
    }
    let after = get(addr, "/cargo/config.json").await;
    assert_eq!(after.status, 200, "{}", after.text());
    assert_eq!(parsed(&after)["auth-required"], Value::Bool(false));
}

#[tokio::test]
async fn a_401_for_the_config_is_answered_403_because_cargo_would_ask_for_a_credential_provider() {
    let r = rig(
        always(
            Reply::status(401)
                .header("Content-Type", "text/plain")
                .header("WWW-Authenticate", "Basic realm=\"x\""),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/cargo/config.json").await;
    assert_eq!(resp.status, 403);
    assert_eq!(resp.text().trim(), "package registry refused the token");
    assert_eq!(resp.header("www-authenticate"), None);
    let head = head(r.addr(), "/cargo/config.json").await;
    assert_eq!(head.status, 403);
    // Any other path keeps the registry's own status: cargo prints the body of a 401 there.
    let index = r.get("/cargo/it/oa/itoa").await;
    assert_eq!(index.status, 401);
    assert_eq!(index.header("www-authenticate"), None);
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_and_the_failure_is_not_kept() {
    // Three connection breaks use up the first request (one try and two retries); the next request finds the upstream well again.
    let tries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = tries.clone();
    let r = rig(
        handler(move |_| {
            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                Reply::aborted()
            } else {
                json_reply(upstream_json())
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    let first = r.get("/cargo/config.json").await;
    assert_eq!(first.status, 502, "{}", first.text());
    assert!(
        first.text().starts_with("upstream error: "),
        "{}",
        first.text()
    );
    let second = r.get("/cargo/config.json").await;
    assert_eq!(second.status, 200, "{}", second.text());
    assert_eq!(parsed(&second)["auth-required"], Value::Bool(false));
}

#[tokio::test]
async fn only_the_cargo_prefix_rewrites_a_config_json() {
    let r = rig(always(json_reply(upstream_json())), nothing(), nothing()).await;
    for p in [
        "/npm/config.json",
        "/maven/config.json",
        "/pypi/config.json",
    ] {
        let resp = r.get(p).await;
        assert_eq!(resp.status, 200, "{p}");
        assert_eq!(
            resp.text(),
            upstream_json(),
            "{p}: no cargo rewrite outside the cargo prefix"
        );
    }
}
