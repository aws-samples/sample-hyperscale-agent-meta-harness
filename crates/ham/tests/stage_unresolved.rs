//! A stage address whose host does not resolve (a DNS failure on connect): the transport says so in plain words, names the host and the two ways
//! forward (`./setup.sh --status` shows the current endpoint, `ham config set api_endpoint <url>` sets it), and says the stage may have been
//! removed or replaced. Every other way a connection fails (refused, a handshake that ends, a timeout) keeps the words it had, and so does a failed
//! lookup of a presigned object URL, which is not the stage's address.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, ApiError, Retry};
use ham::http::{ControlPlane, TransportError};
use ham::https::{HttpsPlane, is_dns_failure};
use ham::sigv4::Signer;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use support::{REGION, creds};

/// A name under `.invalid`, which never resolves (RFC 6761).
const GONE: &str = "https://ham-stage-gone.invalid/direct";
const GONE_HOST: &str = "ham-stage-gone.invalid";

/// The words of the hint, one by one: each must be in the text a person reads.
const HINT: [&str; 4] = [
    "may have been removed or replaced",
    "`./setup.sh --status`",
    "`ham config set api_endpoint <url>`",
    "does not resolve",
];

fn plane(endpoint: &str) -> HttpsPlane {
    HttpsPlane::new(endpoint, Signer::new(REGION, creds())).unwrap()
}

fn assert_no_hint(text: &str) {
    for word in ["removed or replaced", "setup.sh", "api_endpoint"] {
        assert!(!text.contains(word), "{word:?} in: {text}");
    }
}

#[tokio::test]
async fn a_stage_host_that_does_not_resolve_says_the_stage_may_have_been_removed_or_replaced() {
    let e = plane(GONE)
        .call("GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, TransportError::Unresolved(host) if host == GONE_HOST),
        "{e:?}"
    );
    assert_eq!(
        e.to_string(),
        "the stage address does not resolve (ham-stage-gone.invalid). The stage may have been removed or replaced. Run `./setup.sh --status` to see the current endpoint, or `ham config set api_endpoint <url>` to set it."
    );
    for want in HINT {
        assert!(e.to_string().contains(want), "{want:?} in: {e}");
    }
    // The words give the host the lookup failed for and nothing of the request: no path, no API route, no spelling of the file's key.
    let text = e.to_string();
    assert!(
        !text.contains("/v1") && !text.contains("/direct") && !text.contains("apiEndpoint"),
        "{text}"
    );
}

#[tokio::test]
async fn an_unresolved_stage_is_retried_like_any_network_error() {
    let e = plane(GONE)
        .call("GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap_err();
    assert!(
        e.is_retryable(),
        "a resolver that fails once may answer at the next try: {e:?}"
    );
}

#[tokio::test]
async fn the_api_error_every_command_prints_carries_the_same_words() {
    let api = Api::new(Arc::new(plane(GONE))).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    let e = api.whoami().await.unwrap_err();
    assert!(
        matches!(&e, ApiError::Transport(TransportError::Unresolved(_))),
        "{e:?}"
    );
    for want in HINT {
        assert!(e.to_string().contains(want), "{want:?} in: {e}");
    }
}

#[tokio::test]
async fn a_refused_connection_gets_no_hint() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let e = plane(&format!("http://127.0.0.1:{port}/live"))
        .call("GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(&e, TransportError::Network(_)), "{e:?}");
    assert_no_hint(&e.to_string());
}

#[tokio::test]
async fn a_handshake_that_ends_gets_no_hint() {
    // A server that accepts the connection and closes it: the TLS handshake fails after the name resolved.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    let e = plane(&format!("https://127.0.0.1:{port}/live"))
        .call("GET", "/v1/runs", &[], Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(&e, TransportError::Network(_)), "{e:?}");
    assert_no_hint(&e.to_string());
}

#[test]
fn a_timeout_keeps_its_words() {
    let e = TransportError::Timeout(
        "GET https://abc.execute-api.us-east-1.amazonaws.com/direct/v1/runs".into(),
    );
    assert_eq!(
        e.to_string(),
        "timed out: GET https://abc.execute-api.us-east-1.amazonaws.com/direct/v1/runs"
    );
    assert_no_hint(&e.to_string());
}

#[tokio::test]
async fn a_lookup_that_fails_for_a_presigned_object_url_is_not_the_stage_address() {
    let e = plane(GONE)
        .fetch(
            "https://objects.ham-stage-gone.invalid/k?X-Amz-Signature=s",
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(&e, TransportError::Network(_)), "{e:?}");
    let text = e.to_string();
    assert!(
        text.starts_with("network: GET https://objects.ham-stage-gone.invalid/k: "),
        "{text}"
    );
    assert!(!text.contains("X-Amz-Signature"), "{text}");
    assert_no_hint(&text);
}

/// A link of a stand-in error chain: its own text and the next link.
#[derive(Debug)]
struct Link {
    text: &'static str,
    next: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text)
    }
}

impl std::error::Error for Link {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.next.as_deref().map(|e| e as _)
    }
}

/// `texts` as a chain, the first text on the outside.
fn chain(texts: &[&'static str]) -> Link {
    let mut inner: Option<Box<dyn std::error::Error + Send + Sync>> = None;
    for text in texts.iter().skip(1).rev() {
        inner = Some(Box::new(Link { text, next: inner }));
    }
    Link {
        text: texts[0],
        next: inner,
    }
}

#[test]
fn the_classifier_finds_a_resolver_failure_in_a_chain_and_nothing_else() {
    // What hyper-util's connector builds for a failed lookup: the client error, its "dns error" link and the platform's text.
    for platform in [
        "failed to lookup address information: nodename nor servname provided, or not known",
        "failed to lookup address information: Name or service not known",
        "failed to lookup address information: Temporary failure in name resolution",
    ] {
        let dns = chain(&["client error (Connect)", "dns error", platform]);
        assert!(is_dns_failure(&dns), "{platform}");
    }
    // The platform's text alone is enough: the connector's own label may change.
    let std_only = chain(&[
        "client error (Connect)",
        "failed to lookup address information: Name or service not known",
    ]);
    assert!(is_dns_failure(&std_only));
    // Other ways a connection fails.
    for other in [
        chain(&[
            "client error (Connect)",
            "tcp connect error",
            "Connection refused (os error 111)",
        ]),
        chain(&[
            "client error (Connect)",
            "invalid peer certificate: UnknownIssuer",
        ]),
        chain(&[
            "client error (Connect)",
            "tcp connect error",
            "Network is unreachable (os error 101)",
        ]),
        chain(&[
            "client error (SendRequest)",
            "connection closed before message completed",
        ]),
        // The label counts only as a whole link, never as a part of other words.
        chain(&["client error (Connect)", "no dns error budget is left"]),
    ] {
        assert!(!is_dns_failure(&other), "{other}");
    }
}
