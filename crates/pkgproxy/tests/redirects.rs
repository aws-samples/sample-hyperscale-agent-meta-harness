//! Any other target answers `502 redirect refused`.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

fn file() -> Reply {
    Reply::ok("WHEEL-BYTES").header("Content-Type", "application/octet-stream")
}

#[tokio::test]
async fn a_redirect_to_an_amazonaws_host_is_followed_without_the_credential() {
    for status in [301u16, 302, 303, 307, 308] {
        let r = rig(
            always(Reply::redirect(
                status,
                &format!("https://{S3_HOST}/bucket/key.whl?X-Amz-Signature=abc&X-Amz-Expires=300"),
            )),
            always(file()),
            nothing(),
        )
        .await;
        let resp = r.get("/pypi/files/x.whl").await;
        assert_eq!(
            (resp.status, resp.text().as_str()),
            (200, "WHEEL-BYTES"),
            "status {status}"
        );
        let ca = r.ca.seen();
        assert_eq!(ca.len(), 1);
        assert_eq!(
            ca[0].header("authorization"),
            Some(basic_for(&r.token).as_str())
        );
        let s3 = r.s3.seen();
        assert_eq!(s3.len(), 1);
        assert_eq!(
            s3[0].target,
            "/bucket/key.whl?X-Amz-Signature=abc&X-Amz-Expires=300"
        );
        assert_eq!(
            s3[0].header("authorization"),
            None,
            "the credential must never go to the redirect target ({status})"
        );
        assert_eq!(s3[0].header("cookie"), None);
        assert_eq!(
            resp.header("location"),
            None,
            "the signed Location is not handed to the client"
        );
        assert!(!format!("{:?}", resp.headers).contains("X-Amz-Signature"));
    }
}

#[tokio::test]
async fn a_relative_or_same_host_redirect_keeps_the_credential_on_the_original_host_only() {
    let r = rig(
        handler(|s| match s.target.as_str() {
            "/npm/packages/start" => Reply::redirect(302, "/npm/packages/final"),
            "/npm/packages/start2" => {
                Reply::redirect(302, &format!("https://{CA_HOST}/npm/packages/final"))
            }
            "/npm/packages/start3" => Reply::redirect(302, "final"),
            "/npm/packages/final" => file(),
            _ => Reply::status(404),
        }),
        nothing(),
        nothing(),
    )
    .await;
    for p in ["/npm/start", "/npm/start2", "/npm/start3"] {
        let resp = r.get(p).await;
        assert_eq!(
            (resp.status, resp.text().as_str()),
            (200, "WHEEL-BYTES"),
            "{p}"
        );
    }
    for s in r.ca.seen() {
        assert_eq!(
            s.header("authorization"),
            Some(format!("Bearer {}", r.token).as_str()),
            "the original host keeps the credential on {}",
            s.target
        );
    }
    assert_eq!(r.ca.count(), 6);
}

#[tokio::test]
async fn redirect_targets_outside_the_rule_answer_502_and_are_never_contacted() {
    let bad = [
        "http://pkgproxy-files.s3.us-east-1.amazonaws.com/x".to_string(),
        "https://192.0.2.1/".to_string(),
        "https://192.0.2.1.amazonaws.com.decoy.example/x".to_string(),
        "https://decoy.example.com/x".to_string(),
        "https://127.0.0.1/x".to_string(),
        "https://[::1]/x".to_string(),
        "https://localhost/x".to_string(),
        "https://amazonaws.com/x".to_string(),
        "https://decoyamazonaws.com/x".to_string(),
        format!("https://{S3_HOST}.decoy.example/x"),
        format!("https://decoy.example/{S3_HOST}/x"),
        format!("https://decoy.example#{S3_HOST}"),
        format!("https://user@{S3_HOST}/x"),
        format!("https://{}@{S3_HOST}/x", ["user", "secret"].join(":")),
        format!("https://{S3_HOST}@decoy.example/x"),
        format!("https://{S3_HOST}:8443/x"),
        format!("https://{S3_HOST}:80/x"),
        "//decoy.example.com/x".to_string(),
        "ftp://pkgproxy-files.s3.us-east-1.amazonaws.com/x".to_string(),
        "file:///tmp/x".to_string(),
        "gopher://x.amazonaws.com/".to_string(),
        "mailto:a@b.amazonaws.com".to_string(),
        "https://x.amazonaws.com\\@decoy.example/".to_string(),
        "https://x.amazonaws.com%2f@decoy.example/".to_string(),
        "https://".to_string(),
        "https:///x".to_string(),
        "".to_string(),
    ];
    for loc in bad {
        let r = rig(
            always(Reply::redirect(302, &loc)),
            always(file()),
            always(file()),
        )
        .await;
        let resp = r.get("/pypi/files/x.whl").await;
        let want_502_or_pass_through = resp.status == 502 || (loc.is_empty() && resp.status == 302);
        assert!(
            want_502_or_pass_through,
            "Location {loc:?} answered {} {}",
            resp.status,
            resp.text()
        );
        if resp.status == 502 {
            assert!(
                resp.text().contains("redirect refused"),
                "{loc:?}: {}",
                resp.text()
            );
        }
        assert_eq!(
            r.s3.count(),
            0,
            "{loc:?}: the redirect target must not be contacted"
        );
        assert_eq!(r.go.count(), 0);
        assert_eq!(r.ca.count(), 1, "{loc:?}: only the original request");
        let shown = format!("{:?} {}", resp.headers, resp.text());
        assert!(
            !shown.contains(&r.token) && !shown.contains("192.0.2"),
            "{loc:?}: {shown}"
        );
    }
}

#[tokio::test]
async fn an_uppercase_redirect_host_is_matched_case_insensitively() {
    let r = rig(
        always(Reply::redirect(
            302,
            &format!("https://{}/bucket/k", S3_HOST.to_ascii_uppercase()),
        )),
        always(file()),
        nothing(),
    )
    .await;
    assert_eq!(r.get("/pypi/f").await.status, 200);
    assert_eq!(r.s3.count(), 1);
}

#[tokio::test]
async fn at_most_five_redirects_are_followed() {
    // Hop n redirects to hop n+1 on the S3 fake; the chain ends in a file.
    let chain = |len: usize| {
        handler(move |s| {
            let n: usize = s.target.trim_start_matches("/hop/").parse().unwrap_or(0);
            if n >= len {
                file()
            } else {
                Reply::redirect(302, &format!("https://{S3_HOST}/hop/{}", n + 1))
            }
        })
    };
    let r = rig(
        always(Reply::redirect(302, &format!("https://{S3_HOST}/hop/1"))),
        chain(5),
        nothing(),
    )
    .await;
    // The CA answer is redirect 1; hops 1 to 4 are redirects 2 to 5; hop 5 serves: five redirects are allowed.
    assert_eq!(r.get("/pypi/f").await.status, 200);
    assert_eq!(r.s3.count(), 5);

    let r = rig(
        always(Reply::redirect(302, &format!("https://{S3_HOST}/hop/1"))),
        chain(6),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/f").await;
    assert_eq!(resp.status, 502, "{}", resp.text());
    assert!(
        resp.text().contains("too many redirects"),
        "{}",
        resp.text()
    );
    assert_eq!(r.s3.count(), 5, "the sixth redirect is not followed");
}

#[tokio::test]
async fn a_redirect_loop_ends_in_502_after_the_limit() {
    let r = rig(
        always(Reply::redirect(
            302,
            &format!("https://{CA_HOST}/pypi/packages/loop"),
        )),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/loop").await;
    assert_eq!(resp.status, 502);
    assert_eq!(
        r.ca.count(),
        6,
        "the original request and five followed redirects"
    );
}

#[tokio::test]
async fn max_redirects_in_the_config_is_honoured() {
    let mut o = Opts::new(
        always(Reply::redirect(302, &format!("https://{S3_HOST}/hop/1"))),
        always(Reply::redirect(302, &format!("https://{S3_HOST}/hop/2"))),
        nothing(),
    );
    o.max_redirects = 1;
    let r = rig_with(o).await;
    assert_eq!(r.get("/pypi/f").await.status, 502);
    assert_eq!(r.s3.count(), 1);
}

#[tokio::test]
async fn a_redirect_from_the_go_store_is_followed_without_a_credential() {
    let r = rig(
        nothing(),
        always(Reply::ok("mod-bytes").header("Content-Type", "text/plain")),
        always(Reply::redirect(
            301,
            &format!("https://{S3_HOST}/other-region/x"),
        )),
    )
    .await;
    let resp = r.get("/go/example.com/m/@v/list").await;
    assert_eq!((resp.status, resp.text().as_str()), (200, "mod-bytes"));
    assert_eq!(r.s3.seen()[0].header("authorization"), None);
}

#[tokio::test]
async fn a_3xx_without_a_location_is_passed_on_as_it_is() {
    let r = rig(
        always(Reply::status(302).header("Content-Type", "text/plain")),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/f").await;
    assert_eq!(resp.status, 302);
    assert_eq!(resp.header("location"), None);
}

#[tokio::test]
async fn a_redirected_head_stays_a_head_and_a_304_is_not_a_redirect() {
    let r = rig(
        handler(|s| {
            if s.target.ends_with("/h") {
                Reply::redirect(302, &format!("https://{S3_HOST}/h"))
            } else {
                Reply::status(304).header("ETag", "\"x\"")
            }
        }),
        always(
            Reply::status(200)
                .header("Content-Length", "11")
                .header("Content-Type", "application/octet-stream"),
        ),
        nothing(),
    )
    .await;
    let resp = head(r.addr(), "/pypi/h").await;
    assert_eq!(
        (resp.status, resp.header("content-length")),
        (200, Some("11"))
    );
    assert_eq!(r.s3.seen()[0].method, "HEAD");
    assert_eq!(r.get("/pypi/etag").await.status, 304);
}

#[tokio::test]
async fn an_explicit_default_port_is_accepted() {
    let r = rig(
        always(Reply::redirect(302, &format!("https://{S3_HOST}:443/x"))),
        always(file()),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/f").await;
    assert_eq!(
        resp.status,
        200,
        "an explicit :443 is the default port: {}",
        resp.text()
    );
}

#[tokio::test]
async fn a_redirect_back_to_the_upstream_host_must_stay_inside_the_configured_repository() {
    // The credential is the token for the whole CodeArtifact domain; only the path the configuration names may receive it.
    for (location, why) in [
        (
            format!("https://{CA_HOST}/npm/packages/x"),
            "another format of the same repository",
        ),
        (
            format!("https://{CA_HOST}/pypi/other-repo/x"),
            "another repository",
        ),
        (
            format!("https://{CA_HOST}/pypi/packages-old/x"),
            "a name that only starts like the base",
        ),
        (
            format!("https://{CA_HOST}/pypi/packages/../other/x"),
            "a dot segment",
        ),
        (
            format!("https://{CA_HOST}/pypi/packages/%2e%2e/other/x"),
            "an encoded dot segment",
        ),
        (
            format!("https://{CA_HOST}/pypi/packages/a%2f%2e%2e%2f%2e%2e/x"),
            "an encoded slash and dots",
        ),
        (
            format!("https://{CA_HOST}/pypi/packages//x"),
            "an empty segment",
        ),
        (format!("https://{CA_HOST}/"), "the root"),
        (
            "/pypi/other-repo/x".to_string(),
            "a relative path outside the base",
        ),
        ("../other/x".to_string(), "a relative climb"),
    ] {
        let target = location.clone();
        let r = rig(
            handler(move |s| {
                if s.target == "/pypi/packages/start" {
                    Reply::redirect(302, &target)
                } else {
                    file()
                }
            }),
            nothing(),
            nothing(),
        )
        .await;
        let resp = r.get("/pypi/start").await;
        assert_eq!(
            resp.status,
            502,
            "{why}: Location {location:?} answered {} {}",
            resp.status,
            resp.text()
        );
        assert!(
            resp.text().contains("redirect refused"),
            "{why}: {}",
            resp.text()
        );
        assert_eq!(
            r.ca.count(),
            1,
            "{why}: the credential must not be sent to {location:?}"
        );
    }
}

#[tokio::test]
async fn a_redirect_inside_the_repository_is_followed_with_the_credential() {
    for location in [
        format!("https://{CA_HOST}/pypi/packages/deeper/x?y=1"),
        "/pypi/packages/deeper/x".to_string(),
        "deeper/x".to_string(),
        "./deeper/x".to_string(),
        format!("https://{CA_HOST}/pypi/packages"),
    ] {
        let target = location.clone();
        let r = rig(
            handler(move |s| {
                if s.target == "/pypi/packages/start" {
                    Reply::redirect(302, &target)
                } else {
                    file()
                }
            }),
            nothing(),
            nothing(),
        )
        .await;
        let resp = r.get("/pypi/start").await;
        assert_eq!(resp.status, 200, "Location {location:?}: {}", resp.text());
        assert_eq!(r.ca.count(), 2);
        assert_eq!(
            r.ca.seen()[1].header("authorization"),
            Some(basic_for(&r.token).as_str())
        );
    }
}
