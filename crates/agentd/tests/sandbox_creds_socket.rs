//! The credential socket authorises its callers itself in the `landlock` mode: Landlock does not stop a connect to a pathname
//! Unix socket, so the socket reads the caller's user id from the connection and answers a stranger with a refusal that never reaches the store.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::creds::{CredStore, RefreshContext, Refresher};
use agentd::creds_socket::{Callers, fetch_credentials, serve_credentials_for};
use agentd::sandbox::Mode;
use agentd::time::{ManualClock, format_rfc3339};
use common::{CANARY_NONCE, CANARY_SECRET, creds, scratch};
use harness_protocol::api::internal::{CredsRequest, CredsResponse, ProcessCredentials};
use std::sync::Arc;

const T0: i64 = 1_790_000_000;

/// A broker that is down: a store that is asked for a refresh answers with "refresh", so the words of the answer say whether the store was asked.
struct DownRefresher;

impl Refresher for DownRefresher {
    fn refresh<'a>(
        &'a self,
        _ctx: &'a RefreshContext,
        _current: &'a ProcessCredentials,
        _req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        Box::pin(async { Err("broker stopped".to_string()) })
    }
}

fn own_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").unwrap().uid()
}

/// A store whose credentials are 30 seconds from expiry with a broker that is down: any question put to it fails with the word "refresh".
async fn store_that_fails_when_asked() -> Arc<CredStore> {
    let store = CredStore::new(Arc::new(DownRefresher), ManualClock::new(T0));
    store
        .install(
            RefreshContext {
                run_id: "run1".into(),
                agent_id: "a1".into(),
                runtime_session_id: "run1-a1-session-000000000000000000001".into(),
                internal_api_url: "https://internal.example/live".into(),
                region: "us-west-2".into(),
            },
            creds(&format_rfc3339(T0 + 30)),
            CANARY_NONCE.into(),
        )
        .await;
    store
}

#[test]
fn the_uid_mode_admits_anyone_and_the_landlock_mode_admits_agentds_own_user_alone() {
    assert_eq!(Callers::for_mode(Mode::Uid), Callers::Anyone);
    assert_eq!(Callers::for_mode(Mode::Landlock), Callers::Uid(own_uid()));
}

#[test]
fn a_user_check_admits_that_user_refuses_every_other_and_refuses_when_the_connection_says_nothing()
{
    assert!(Callers::Anyone.allows(Some(7)));
    assert!(Callers::Anyone.allows(None));
    assert!(Callers::Uid(7).allows(Some(7)));
    assert!(!Callers::Uid(7).allows(Some(8)));
    assert!(!Callers::Uid(7).allows(Some(0)));
    assert!(
        !Callers::Uid(7).allows(None),
        "a connection that does not say who called fails closed"
    );
}

#[tokio::test]
async fn a_caller_of_another_user_is_refused_and_the_store_is_never_asked() {
    let dir = scratch("cg1");
    let path = dir.join("creds.sock");
    let store = store_that_fails_when_asked().await;
    let wrong = own_uid().wrapping_add(1);
    let _server = serve_credentials_for(&path, store, None, Callers::Uid(wrong))
        .await
        .unwrap();
    let err = fetch_credentials(&path).await.unwrap_err();
    assert!(err.contains("not authorised"), "{err}");
    assert!(
        !err.contains("refresh") && !err.contains("broker"),
        "the store was asked: {err}"
    );
    assert!(!err.contains(CANARY_SECRET), "{err}");
}

#[tokio::test]
async fn a_caller_of_the_allowed_user_gets_the_credentials() {
    let dir = scratch("cg2");
    let path = dir.join("creds.sock");
    let store = CredStore::new(Arc::new(DownRefresher), ManualClock::new(T0));
    store
        .install(
            RefreshContext {
                run_id: "run1".into(),
                agent_id: "a1".into(),
                runtime_session_id: "run1-a1-session-000000000000000000001".into(),
                internal_api_url: "https://internal.example/live".into(),
                region: "us-west-2".into(),
            },
            creds(&format_rfc3339(T0 + 3600)),
            CANARY_NONCE.into(),
        )
        .await;
    let _server = serve_credentials_for(&path, store, None, Callers::Uid(own_uid()))
        .await
        .unwrap();
    let json = fetch_credentials(&path).await.unwrap();
    let parsed: ProcessCredentials = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.secret_access_key, CANARY_SECRET);
    assert!(
        !json.contains(CANARY_NONCE),
        "the nonce never leaves agentd"
    );
}

#[tokio::test]
async fn the_everyone_setting_keeps_the_answer_of_the_uid_mode() {
    let dir = scratch("cg3");
    let path = dir.join("creds.sock");
    let store = store_that_fails_when_asked().await;
    let _server = serve_credentials_for(&path, store, None, Callers::Anyone)
        .await
        .unwrap();
    let err = fetch_credentials(&path).await.unwrap_err();
    assert!(
        err.contains("refresh"),
        "the store was asked as before: {err}"
    );
}
