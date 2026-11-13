//! SigV4-signed HTTPS transport: every `/v1` call carries an
//! `AWS4-HMAC-SHA256` signature for service `execute-api` in the configured Region that the server
//! can recompute from the request it received, including the stage prefix, the query, the session
//! token and the `Idempotency-Key`. Plain HTTP is refused except to a loopback host.
#![allow(clippy::unwrap_used)]

mod support;

use ham::http::{ControlPlane, TransportError, redact_url};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use support::{AKID, Fake, REGION, STAGE, TOKEN, creds};

fn plane(fake: &Fake) -> HttpsPlane {
    HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap()
}

#[tokio::test]
async fn a_get_is_signed_for_execute_api_and_verifies_against_the_bytes_received() {
    let fake = Fake::start().await;
    let p = plane(&fake);
    let r = p.call("GET", "/v1/whoami", &[], Vec::new()).await.unwrap();
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["principalArn"], support::ME);

    let seen = fake.mem.seen();
    let s = seen.last().unwrap();
    assert_eq!(s.signature, Ok(()));
    assert_eq!(s.path, "/v1/whoami", "the stage prefix is part of the URL");
    let auth = &s.headers["authorization"];
    assert!(
        auth.starts_with(&format!(
            "AWS4-HMAC-SHA256 Credential={AKID}/{}/{REGION}/execute-api/aws4_request, SignedHeaders=",
            &s.headers["x-amz-date"][..8]
        )),
        "{auth}"
    );
    assert_eq!(s.headers["x-amz-security-token"], TOKEN);
}

#[tokio::test]
async fn a_query_is_signed_as_sent() {
    let fake = Fake::start().await;
    let p = plane(&fake);
    let r = p
        .call(
            "GET",
            "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/events?shard=7&after=01K5ZQ4T8M3N6P7R9S0V1W2X40",
            &[],
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 404, "the run does not exist; the signature held");
    let s = fake.mem.seen().pop().unwrap();
    assert_eq!(s.signature, Ok(()));
    assert_eq!(s.query, "shard=7&after=01K5ZQ4T8M3N6P7R9S0V1W2X40");
}

#[tokio::test]
async fn a_post_signs_its_body_content_type_and_idempotency_key() {
    let fake = Fake::start().await;
    let p = plane(&fake);
    let body = br#"{"prompt":"x","target":{"mode":"s3-only"},"model":"global.anthropic.claude-sonnet-5-5"}"#.to_vec();
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("idempotency-key".to_string(), "k-transport-1".to_string()),
    ];
    let r = p
        .call("POST", "/v1/direct/runs", &headers, body)
        .await
        .unwrap();
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let s = fake.mem.posts("/v1/direct/runs").pop().unwrap();
    assert_eq!(s.signature, Ok(()));
    assert_eq!(s.headers["idempotency-key"], "k-transport-1");
    let signed = s.headers["authorization"]
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_string();
    for h in [
        "content-type",
        "host",
        "idempotency-key",
        "x-amz-date",
        "x-amz-security-token",
    ] {
        assert!(signed.split(';').any(|x| x == h), "{h} not in {signed}");
    }
}

#[tokio::test]
async fn a_tampered_or_wrongly_scoped_request_fails_verification() {
    let fake = Fake::start().await;
    // Signed for another Region: the fake refuses the scope.
    // A Region other than the fake plane's (the home Region).
    let other = if REGION == "us-east-1" {
        "us-west-2"
    } else {
        "us-east-1"
    };
    let wrong = HttpsPlane::new(&fake.endpoint, Signer::new(other, creds())).unwrap();
    let r = wrong
        .call("GET", "/v1/whoami", &[], Vec::new())
        .await
        .unwrap();
    assert_eq!(r.status, 403);
    assert!(fake.mem.seen().last().unwrap().signature.is_err());
}

#[tokio::test]
async fn http_is_refused_except_to_loopback_and_https_endpoints_are_accepted() {
    for bad in [
        "http://example.com/live",
        "http://10.0.0.1/live",
        "ftp://example.com/",
        "not a url",
        "",
    ] {
        let e = HttpsPlane::new(bad, Signer::new(REGION, creds())).unwrap_err();
        assert!(matches!(e, TransportError::Refused(_)), "{bad}: {e:?}");
    }
    for ok in [
        "https://abc123.execute-api.us-west-2.amazonaws.com/live",
        "https://abc123.execute-api.us-west-2.amazonaws.com/live/",
        "http://127.0.0.1:9/live",
        "http://localhost:9",
    ] {
        HttpsPlane::new(ok, Signer::new(REGION, creds())).unwrap();
    }
}

#[tokio::test]
async fn a_presigned_fetch_is_unsigned_and_refuses_plain_http_off_loopback() {
    let fake = Fake::start().await;
    let p = plane(&fake);
    let e = p
        .fetch("http://example.com/objects/k?X-Amz-Signature=x", None, None)
        .await
        .unwrap_err();
    assert!(matches!(e, TransportError::Refused(_)), "{e:?}");
    let url = format!(
        "{}/objects/missing?X-Amz-Signature=fake",
        fake.endpoint.trim_end_matches(&format!("/{STAGE}"))
    );
    let r = p.fetch(&url, Some((0, 0)), None).await.unwrap();
    assert_eq!(r.status, 404);
    let s = fake.mem.seen().pop().unwrap();
    assert!(!s.headers.contains_key("authorization"), "{:?}", s.headers);
    assert_eq!(s.headers["range"], "bytes=0-0");
}

#[tokio::test]
async fn a_refused_connection_is_a_retryable_network_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let p = HttpsPlane::new(
        &format!("http://127.0.0.1:{port}/live"),
        Signer::new(REGION, creds()),
    )
    .unwrap();
    let e = p
        .call("GET", "/v1/whoami", &[], Vec::new())
        .await
        .unwrap_err();
    assert!(e.is_retryable(), "{e:?}");
}

#[test]
fn redacted_urls_keep_no_query() {
    assert_eq!(
        redact_url(
            "https://b.s3.us-west-2.amazonaws.com/runs/R/x.zip?X-Amz-Security-Token=secret&X-Amz-Signature=abc"
        ),
        "https://b.s3.us-west-2.amazonaws.com/runs/R/x.zip"
    );
    assert_eq!(redact_url("http://127.0.0.1:9/o"), "http://127.0.0.1:9/o");
    assert!(!redact_url("garbage?token=secret").contains("secret"));
}
