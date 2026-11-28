//! What an ask of the credential store sends after a refresh that failed, and the kind of a refresh that failed: one that
//! may be another answer a moment later ([`CredError::RefreshFailedForNow`]) or one that stays what it is
//! ([`CredError::RefreshFailed`]). The kinds are read through the real refresher against a stand-in service. No AWS call is made.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::creds::{
    CredError, CredStore, HttpRefresher, REFRESH_WINDOW_S, RefreshContext, RefreshFailure,
    Refresher,
};
use agentd::time::{ManualClock, format_rfc3339};
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{CANARY_NONCE, creds};
use harness_protocol::api::internal::{CredsRequest, CredsResponse, ProcessCredentials};
use std::sync::{Arc, Mutex};

const T0: i64 = 1_790_000_000;

fn ctx(internal_api_url: &str) -> RefreshContext {
    RefreshContext {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "run1-a1-session-000000000000000000001".into(),
        internal_api_url: internal_api_url.into(),
        region: "us-west-2".into(),
    }
}

/// Fails the first refresh in its own words and answers every later one; keeps what each ask carried and was signed with.
#[derive(Default)]
struct FailsFirst {
    asks: Mutex<Vec<(CredsRequest, ProcessCredentials)>>,
}

impl Refresher for FailsFirst {
    fn refresh<'a>(
        &'a self,
        _ctx: &'a RefreshContext,
        current: &'a ProcessCredentials,
        req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        let n = {
            let mut asks = self.asks.lock().unwrap();
            asks.push((req, current.clone()));
            asks.len()
        };
        let result = if n == 1 {
            Err("the stand-in gave no answer".to_string())
        } else {
            Ok(CredsResponse {
                credentials: creds(&format_rfc3339(T0 + 7200)),
                next_nonce: format!("nonce-{n}"),
            })
        };
        Box::pin(async move { result })
    }
}

/// The store keeps the credentials and the nonce it had when a refresh fails, so the next ask sends the first ask again: the
/// same nonce, signed with the same credentials. When that ask is answered, the store takes the credentials and the nonce it
/// brought, and the refresh after it carries that nonce.
#[tokio::test]
async fn b989_an_ask_after_a_refresh_that_failed_sends_the_same_ask_and_takes_the_answer() {
    let r = Arc::new(FailsFirst::default());
    let clock = ManualClock::new(T0);
    let store = CredStore::new(r.clone(), clock.clone());
    let held = creds(&format_rfc3339(T0 + REFRESH_WINDOW_S - 1));
    store
        .install(
            ctx("https://internal.example/live"),
            held.clone(),
            CANARY_NONCE.into(),
        )
        .await;

    let failed = store.current().await.unwrap_err();
    // A refresher that does not say the kind of its failure fails with one that stays what it is.
    assert_eq!(
        failed,
        CredError::RefreshFailed("the stand-in gave no answer".into())
    );
    assert!(!failed.temporary());

    let got = store.current().await.unwrap();
    assert_eq!(got.expiration, format_rfc3339(T0 + 7200));
    clock.set(T0 + 7200 - REFRESH_WINDOW_S + 1);
    store.current().await.unwrap();

    let asks = r.asks.lock().unwrap();
    assert_eq!(asks.len(), 3);
    assert_eq!(asks[0].0.nonce, CANARY_NONCE);
    assert_eq!(asks[0].1, held);
    assert_eq!(asks[1], asks[0], "the second ask is the first ask again");
    assert_eq!(
        asks[2].0.nonce, "nonce-2",
        "the nonce the answered ask brought"
    );
    assert_eq!(asks[2].1, got, "signed with the credentials it brought");
}

/// A refresher that says the kind of its failure: the store hands the kind on, with the sentence a person reads unchanged.
struct Says(RefreshFailure);

impl Refresher for Says {
    fn refresh<'a>(
        &'a self,
        _ctx: &'a RefreshContext,
        _current: &'a ProcessCredentials,
        _req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        let sentence = self.0.sentence.clone();
        Box::pin(async move { Err(sentence) })
    }

    fn refresh_told<'a>(
        &'a self,
        _ctx: &'a RefreshContext,
        _current: &'a ProcessCredentials,
        _req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, RefreshFailure>> {
        let failure = self.0.clone();
        Box::pin(async move { Err(failure) })
    }
}

#[tokio::test]
async fn b989_the_store_hands_on_the_kind_a_refresher_says_and_keeps_the_sentence() {
    for temporary in [true, false] {
        let store = CredStore::new(
            Arc::new(Says(RefreshFailure {
                sentence: "the service was busy".into(),
                temporary,
            })),
            ManualClock::new(T0),
        );
        store
            .install(
                ctx("https://internal.example/live"),
                creds(&format_rfc3339(T0 + 60)),
                CANARY_NONCE.into(),
            )
            .await;
        let failed = store.current().await.unwrap_err();
        assert_eq!(failed.temporary(), temporary);
        assert_eq!(
            failed,
            if temporary {
                CredError::RefreshFailedForNow("the service was busy".into())
            } else {
                CredError::RefreshFailed("the service was busy".into())
            }
        );
        assert_eq!(
            failed.to_string(),
            "credential refresh failed: the service was busy"
        );
    }
    // The kinds that are no refresh at all stay what they are.
    assert!(!CredError::NotInstalled.temporary());
    assert!(!CredError::BadExpiration.temporary());
}

/// A stand-in service that answers every post to `/internal/creds` with `status` and `body`. Returns its base URL.
async fn service(status: u16, body: &str) -> String {
    let body = body.to_string();
    let router = Router::new().route(
        "/internal/creds",
        post(move || {
            let body = body.clone();
            async move { (StatusCode::from_u16(status).unwrap(), body).into_response() }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

/// How a refresh of the real refresher fails against a service at `base`.
async fn failure_at(base: &str) -> CredError {
    let store = CredStore::new(Arc::new(HttpRefresher::new()), ManualClock::new(T0));
    store
        .install(
            ctx(base),
            creds(&format_rfc3339(T0 + 60)),
            CANARY_NONCE.into(),
        )
        .await;
    store.current().await.unwrap_err()
}

#[tokio::test]
async fn b989_a_refresh_answered_with_a_5xx_or_a_429_may_be_another_answer_a_moment_later() {
    for status in [500_u16, 502, 503, 504, 429] {
        let failed = failure_at(&service(status, "{}").await).await;
        assert_eq!(
            failed,
            CredError::RefreshFailedForNow(format!("creds refresh returned {status}"))
        );
        assert!(failed.temporary(), "{status}");
        assert_eq!(
            failed.to_string(),
            format!("credential refresh failed: creds refresh returned {status}")
        );
    }
}

#[tokio::test]
async fn b989_a_refresh_answered_with_any_other_status_stays_what_it_is() {
    // A refusal (403), a nonce or a session the service has moved on from (409), and requests the service will not take.
    for status in [400_u16, 401, 403, 404, 409, 413] {
        let failed = failure_at(&service(status, "{}").await).await;
        assert_eq!(
            failed,
            CredError::RefreshFailed(format!("creds refresh returned {status}"))
        );
        assert!(!failed.temporary(), "{status}");
    }
}

#[tokio::test]
async fn b989_an_answer_200_that_is_no_refresh_stays_what_it_is() {
    let failed = failure_at(&service(200, "{\"no\": \"refresh\"}").await).await;
    assert!(
        matches!(&failed, CredError::RefreshFailed(said) if said.starts_with("creds refresh response is not a CredsResponse")),
        "{failed:?}"
    );
    assert!(!failed.temporary());
}

#[tokio::test]
async fn b989_a_refresh_that_gets_no_answer_may_be_another_answer_a_moment_later() {
    // A port nothing listens on: the listener is closed before the refresh is asked.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let failed = failure_at(&base).await;
    assert!(
        matches!(&failed, CredError::RefreshFailedForNow(said) if said.starts_with("transport: ")),
        "{failed:?}"
    );
    assert!(failed.temporary());
}

/// The research route reads a refresh that failed as a failure that may pass, whichever of the two kinds the store names: as it
/// read every failed refresh before the store told the kinds apart.
#[tokio::test]
async fn b989_the_research_route_reads_both_kinds_of_a_failed_refresh_as_before() {
    use agentd::direct::research::route::{FailureKind, HttpRoute, Route};
    for temporary in [true, false] {
        let store = CredStore::new(
            Arc::new(Says(RefreshFailure {
                sentence: "the service was busy".into(),
                temporary,
            })),
            ManualClock::new(T0),
        );
        store
            .install(
                ctx("https://internal.example/live"),
                creds(&format_rfc3339(T0 + 60)),
                CANARY_NONCE.into(),
            )
            .await;
        let route = HttpRoute::new(
            store,
            "https://internal.example/live".into(),
            "us-west-2".into(),
        );
        let failed = route
            .call("/research/search", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            failed.kind,
            FailureKind::Transient,
            "temporary: {temporary}"
        );
        assert_eq!(
            failed.error.message,
            "the session credentials could not be refreshed"
        );
    }
}

/// The refresher's own `refresh`, which says no kind, keeps its form: the same sentence for a failure and the answer of the service.
#[tokio::test]
async fn b989_the_refreshers_refresh_without_the_kind_gives_the_same_sentence_and_the_same_answer()
{
    let held = creds(&format_rfc3339(T0 + 60));
    let ask = || CredsRequest {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "run1-a1-session-000000000000000000001".into(),
        nonce: CANARY_NONCE.into(),
    };
    let failed = HttpRefresher::new()
        .refresh(&ctx(&service(503, "{}").await), &held, ask())
        .await;
    assert_eq!(failed, Err("creds refresh returned 503".to_string()));

    let answer = CredsResponse {
        credentials: creds(&format_rfc3339(T0 + 7200)),
        next_nonce: "next".into(),
    };
    let body = serde_json::to_string(&answer).unwrap();
    let got = HttpRefresher::new()
        .refresh(&ctx(&service(200, &body).await), &held, ask())
        .await;
    assert_eq!(got, Ok(answer));
}
