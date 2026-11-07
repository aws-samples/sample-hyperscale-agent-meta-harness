//! Package index: `run-session` mints the CodeArtifact token outside the VM with its own role
//! (`packages::mint_token`, tested here against a stand-in) and hands it to agentd in the invocation; agentd keeps it in the
//! package proxy's memory. The token is never logged.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::config::Config;
use agentd::http::SignedHttp;
use agentd::packages::{PackagesIndex, mint_token, repository_host, token_duration_s, token_url};
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use common::{CANARY_TOKEN, creds};
use std::sync::{Arc, Mutex};

const HOST: &str = "hamh-live-800000000022.d.codeartifact.us-west-2.amazonaws.com";
const TOKEN: &str = "eyJ2ZXIiOjF9.tok-1_x";

fn ix(endpoint: Option<String>) -> PackagesIndex {
    PackagesIndex {
        domain: "hamh-live".into(),
        owner: "800000000022".into(),
        repository: "packages".into(),
        api_endpoint: endpoint,
    }
}

fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn the_index_comes_from_the_environment_or_is_off() {
    assert_eq!(Config::from_vars(vars(&[])).packages, None);
    let c = Config::from_vars(vars(&[
        ("AGENTD_PACKAGES_DOMAIN", "hamh-live"),
        ("AGENTD_PACKAGES_OWNER", "800000000022"),
    ]));
    assert_eq!(c.packages, Some(ix(None)));
    // A domain without its owner is off, never half-configured.
    let c = Config::from_vars(vars(&[("AGENTD_PACKAGES_DOMAIN", "hamh-live")]));
    assert_eq!(c.packages, None);
    let c = Config::from_vars(vars(&[
        ("AGENTD_PACKAGES_DOMAIN", "d1"),
        ("AGENTD_PACKAGES_OWNER", "123456789012"),
        ("AGENTD_PACKAGES_REPOSITORY", "other"),
        ("AGENTD_PACKAGES_ENDPOINT", "http://127.0.0.1:9"),
    ]));
    assert_eq!(
        c.packages,
        Some(PackagesIndex {
            domain: "d1".into(),
            owner: "123456789012".into(),
            repository: "other".into(),
            api_endpoint: Some("http://127.0.0.1:9".into()),
        })
    );
}

#[test]
fn urls_follow_the_codeartifact_api() {
    assert_eq!(
        token_url(&ix(None), "us-west-2", 1200),
        "https://codeartifact.us-west-2.amazonaws.com/v1/authorization-token\
         ?domain=hamh-live&domain-owner=800000000022&duration=1200"
    );
    assert_eq!(repository_host(&ix(None), "us-west-2"), HOST);
    assert_eq!(
        token_url(&ix(Some("http://127.0.0.1:9".into())), "us-west-2", 900),
        "http://127.0.0.1:9/v1/authorization-token\
         ?domain=hamh-live&domain-owner=800000000022&duration=900"
    );
}

#[test]
fn the_token_outlives_the_turn() {
    // CodeArtifact accepts 900..=43200; the turn cap is 5,400 s.
    assert_eq!(token_duration_s(60), 900);
    assert_eq!(token_duration_s(900), 1200);
    assert_eq!(token_duration_s(5400), 5700);
    assert_eq!(token_duration_s(u32::MAX), 43200);
}

/// One token request: its query string and headers.
type Call = (Option<String>, Vec<(String, String)>);

#[derive(Default)]
struct Seen {
    calls: Vec<Call>,
    reply: Option<(StatusCode, String)>,
}

type S = Arc<Mutex<Seen>>;

async fn server(reply: (StatusCode, &str)) -> (String, S) {
    let seen: S = Arc::new(Mutex::new(Seen {
        calls: vec![],
        reply: Some((reply.0, reply.1.to_string())),
    }));
    let app = Router::new()
        .route(
            "/v1/authorization-token",
            post(
                |State(s): State<S>, RawQuery(q): RawQuery, h: HeaderMap| async move {
                    let headers = h
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                        .collect();
                    let mut g = s.lock().unwrap();
                    g.calls.push((q, headers));
                    g.reply.clone().unwrap()
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
    h.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

#[tokio::test]
async fn the_token_is_minted_with_the_callers_credentials() {
    let body = format!("{{\"authorizationToken\":\"{TOKEN}\",\"expiration\":1790341000}}");
    let (url, seen) = server((StatusCode::OK, &body)).await;
    let creds = creds("2026-09-25T13:00:00Z");
    let t = mint_token(
        &SignedHttp::new(),
        &creds,
        &ix(Some(url)),
        "us-west-2",
        1200,
    )
    .await
    .unwrap();
    assert_eq!(t, TOKEN);
    let g = seen.lock().unwrap();
    assert_eq!(g.calls.len(), 1);
    let (query, headers) = &g.calls[0];
    assert_eq!(
        query.as_deref(),
        Some("domain=hamh-live&domain-owner=800000000022&duration=1200")
    );
    assert!(
        header(headers, "authorization").contains("/us-west-2/codeartifact/aws4_request"),
        "{headers:?}"
    );
    assert_eq!(header(headers, "x-amz-security-token"), CANARY_TOKEN);
}

#[tokio::test]
async fn a_denied_mint_names_the_reason() {
    let (url, _) = server((
        StatusCode::FORBIDDEN,
        "{\"message\":\"User: arn:aws:sts::800000000022:assumed-role/hamh-live-dispatcher/x is not authorized to perform: codeartifact:GetAuthorizationToken\"}",
    ))
    .await;
    let e = mint_token(
        &SignedHttp::new(),
        &creds("2026-09-25T13:00:00Z"),
        &ix(Some(url)),
        "us-west-2",
        900,
    )
    .await
    .unwrap_err();
    assert!(e.contains("403"), "{e}");
    assert!(
        e.contains("not authorized to perform: codeartifact:GetAuthorizationToken"),
        "{e}"
    );
}

#[tokio::test]
async fn an_unexpected_token_is_refused_without_echoing_it() {
    for body in [
        "{\"authorizationToken\":\"bad token\\nwith-newline\",\"expiration\":1}",
        "{\"authorizationToken\":\"\",\"expiration\":1}",
        "{\"expiration\":1}",
        "not json",
    ] {
        let (url, _) = server((StatusCode::OK, body)).await;
        let e = mint_token(
            &SignedHttp::new(),
            &creds("2026-09-25T13:00:00Z"),
            &ix(Some(url)),
            "us-west-2",
            900,
        )
        .await
        .unwrap_err();
        assert!(!e.contains("bad token"), "{e}");
        assert!(!e.contains("with-newline"), "{e}");
    }
}
