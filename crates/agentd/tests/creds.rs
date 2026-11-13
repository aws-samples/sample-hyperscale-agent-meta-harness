//! Credential store, refresh and the credential_process helper.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::creds::{CredError, CredStore, REFRESH_WINDOW_S, RefreshContext, Refresher};
use agentd::creds_socket::{fetch_credentials, serve_credentials};
use agentd::secrets::SecretSet;
use agentd::time::{ManualClock, format_rfc3339, parse_rfc3339};
use common::{CANARY_NONCE, CANARY_SECRET, CANARY_TOKEN, canary_key_id, creds, scratch};
use harness_protocol::api::internal::{CredsRequest, CredsResponse, ProcessCredentials};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

const T0: i64 = 1_790_000_000;

fn ctx() -> RefreshContext {
    RefreshContext {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "run1-a1-session-000000000000000000001".into(),
        internal_api_url: "https://internal.example/live".into(),
        region: "us-west-2".into(),
    }
}

#[derive(Default)]
struct FakeRefresher {
    calls: Mutex<Vec<CredsRequest>>,
    fail: bool,
    next_expiration: i64,
}

impl Refresher for FakeRefresher {
    fn refresh<'a>(
        &'a self,
        _ctx: &'a RefreshContext,
        current: &'a ProcessCredentials,
        req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(req);
            calls.len()
        };
        let result = if self.fail {
            Err("broker stopped".to_string())
        } else {
            // A refresh mints a whole new credential: the key id, the secret key and the session token all change (STS).
            let mut c = current.clone();
            c.access_key_id = format!("ASIAREFRESHED{n:07}");
            c.secret_access_key = format!("refreshed-secret-{n}-agentd-must-never-print");
            c.session_token = format!("refreshed-token-{n}-agentd-must-never-print");
            c.expiration = format_rfc3339(self.next_expiration);
            Ok(CredsResponse {
                credentials: c,
                next_nonce: format!("nonce-{n}"),
            })
        };
        Box::pin(async move { result })
    }
}

#[test]
fn rfc3339_round_trips_and_rejects_garbage() {
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("2026-09-25T05:00:00Z"), Some(1_790_312_400));
    assert_eq!(
        parse_rfc3339("2026-09-25T05:00:00.123Z"),
        Some(1_790_312_400)
    );
    assert_eq!(
        parse_rfc3339("2026-09-25T05:00:00+00:00"),
        Some(1_790_312_400)
    );
    assert_eq!(format_rfc3339(1_790_312_400), "2026-09-25T05:00:00Z");
    for bad in [
        "",
        "2026-09-25",
        "2026-13-01T00:00:00Z",
        "2026-09-25T25:00:00Z",
        "not a time",
        "2026-09-25T05:00:00+02:00",
    ] {
        assert_eq!(parse_rfc3339(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn not_installed_is_an_error() {
    let store = CredStore::new(Arc::new(FakeRefresher::default()), ManualClock::new(T0));
    assert_eq!(store.current().await, Err(CredError::NotInstalled));
}

#[tokio::test]
async fn fresh_credentials_are_served_from_memory_without_refresh() {
    let r = Arc::new(FakeRefresher::default());
    let store = CredStore::new(r.clone(), ManualClock::new(T0));
    store
        .install(
            ctx(),
            creds(&format_rfc3339(T0 + 3600)),
            CANARY_NONCE.into(),
        )
        .await;
    let got = store.current().await.unwrap();
    assert_eq!(got.access_key_id, canary_key_id());
    assert!(r.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inside_the_window_it_refreshes_with_the_nonce_and_rotates_it() {
    let r = Arc::new(FakeRefresher {
        next_expiration: T0 + 7200,
        ..Default::default()
    });
    let clock = ManualClock::new(T0);
    let store = CredStore::new(r.clone(), clock.clone());
    store
        .install(
            ctx(),
            creds(&format_rfc3339(T0 + REFRESH_WINDOW_S - 1)),
            CANARY_NONCE.into(),
        )
        .await;
    let got = store.current().await.unwrap();
    assert_eq!(got.access_key_id, "ASIAREFRESHED0000001");
    // Next refresh uses the rotated nonce.
    clock.set(T0 + 7200 - REFRESH_WINDOW_S + 1);
    store.current().await.unwrap();
    let calls = r.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].nonce, CANARY_NONCE);
    assert_eq!(calls[1].nonce, "nonce-1");
    assert_eq!(calls[0].run_id, "run1");
    assert_eq!(calls[0].agent_id, "a1");
    assert_eq!(
        calls[0].runtime_session_id,
        "run1-a1-session-000000000000000000001"
    );
}

#[tokio::test]
async fn a_refresh_adds_the_new_secret_token_and_nonce_to_the_watched_set_and_keeps_the_old_ones() {
    let r = Arc::new(FakeRefresher {
        next_expiration: T0 + 7200,
        ..Default::default()
    });
    let clock = ManualClock::new(T0);
    let store = CredStore::new(r.clone(), clock.clone());
    store
        .install(
            ctx(),
            creds(&format_rfc3339(T0 + REFRESH_WINDOW_S - 1)),
            CANARY_NONCE.into(),
        )
        .await;
    // The set a run redacts with, watched after the install: what the store holds now is in it at once.
    let set = SecretSet::new(vec!["from-the-invocation-only".to_string()]);
    store.watch_secrets(set.clone()).await;
    let installed = [CANARY_SECRET, CANARY_TOKEN, CANARY_NONCE];
    let has = |set: &SecretSet, value: &str| set.snapshot().iter().any(|s| s == value);
    for v in installed {
        assert!(has(&set, v), "{v} is not in the set after the install");
    }
    assert!(has(&set, "from-the-invocation-only"));

    // The first refresh: the new secret key, session token and nonce are added the moment they exist; the old ones stay.
    let got = store.current().await.unwrap();
    assert_eq!(
        got.secret_access_key,
        "refreshed-secret-1-agentd-must-never-print"
    );
    for v in [
        "refreshed-secret-1-agentd-must-never-print",
        "refreshed-token-1-agentd-must-never-print",
        "nonce-1",
    ] {
        assert!(has(&set, v), "{v} is not in the set after the refresh");
    }
    for v in installed {
        assert!(has(&set, v), "{v} was removed by the refresh");
    }

    // A second refresh grows the set again and removes nothing: a value the engine printed before a rotation is still a secret.
    clock.set(T0 + 7200 - REFRESH_WINDOW_S + 1);
    store.current().await.unwrap();
    for v in [
        "refreshed-secret-2-agentd-must-never-print",
        "refreshed-token-2-agentd-must-never-print",
        "nonce-2",
        "refreshed-secret-1-agentd-must-never-print",
        "refreshed-token-1-agentd-must-never-print",
        "nonce-1",
        CANARY_SECRET,
        CANARY_TOKEN,
        CANARY_NONCE,
    ] {
        assert!(
            has(&set, v),
            "{v} is not in the set after the second refresh"
        );
    }
    assert_eq!(set.snapshot().len(), 10, "{:?}", set.snapshot().len());

    // Redaction reads the set as it is now: every value of 8 characters or more, old and new, is replaced.
    let text = format!(
        "old {CANARY_SECRET} then refreshed-secret-1-agentd-must-never-print then refreshed-token-2-agentd-must-never-print, nonce-2 stays short"
    );
    assert_eq!(
        set.redact(&text),
        "old <redacted> then <redacted> then <redacted>, nonce-2 stays short"
    );
    // Debug prints a count, never a value.
    let debug = format!("{set:?}");
    for v in [CANARY_SECRET, "refreshed-secret-1", "nonce-1"] {
        assert!(!debug.contains(v), "{debug}");
    }
    assert!(debug.contains("10"), "{debug}");
}

#[tokio::test]
async fn refresh_failure_fails_closed_even_before_expiry() {
    let r = Arc::new(FakeRefresher {
        fail: true,
        ..Default::default()
    });
    let store = CredStore::new(r, ManualClock::new(T0));
    store
        .install(ctx(), creds(&format_rfc3339(T0 + 60)), CANARY_NONCE.into())
        .await;
    let err = store.current().await.unwrap_err();
    assert!(matches!(err, CredError::RefreshFailed(_)));
    let text = err.to_string();
    for s in [CANARY_SECRET, CANARY_TOKEN, CANARY_NONCE] {
        assert!(!text.contains(s));
    }
}

#[tokio::test]
async fn an_unparsable_expiration_is_rejected() {
    let store = CredStore::new(Arc::new(FakeRefresher::default()), ManualClock::new(T0));
    store
        .install(ctx(), creds("tomorrow"), CANARY_NONCE.into())
        .await;
    assert_eq!(store.current().await, Err(CredError::BadExpiration));
    store.clear().await;
    assert_eq!(store.current().await, Err(CredError::NotInstalled));
}

#[tokio::test]
async fn the_socket_serves_credential_process_json_with_owner_only_permissions() {
    let dir = scratch("sock");
    let path = dir.join("creds.sock");
    let store = CredStore::new(Arc::new(FakeRefresher::default()), ManualClock::new(T0));
    store
        .install(
            ctx(),
            creds(&format_rfc3339(T0 + 3600)),
            CANARY_NONCE.into(),
        )
        .await;
    let _server = serve_credentials(&path, store.clone()).await.unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let json = fetch_credentials(&path).await.unwrap();
    let parsed: ProcessCredentials = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.secret_access_key, CANARY_SECRET);
    assert!(
        !json.contains(CANARY_NONCE),
        "the nonce never leaves agentd"
    );
}

#[tokio::test]
async fn the_socket_fails_closed_when_refresh_fails() {
    let dir = scratch("sock-fail");
    let path = dir.join("creds.sock");
    let store = CredStore::new(
        Arc::new(FakeRefresher {
            fail: true,
            ..Default::default()
        }),
        ManualClock::new(T0),
    );
    store
        .install(ctx(), creds(&format_rfc3339(T0 + 30)), CANARY_NONCE.into())
        .await;
    let _server = serve_credentials(&path, store).await.unwrap();
    let err = fetch_credentials(&path).await.unwrap_err();
    assert!(err.contains("refresh"), "{err}");
    assert!(fetch_credentials(&dir.join("missing.sock")).await.is_err());
}

#[tokio::test]
async fn agentd_creds_subcommand_prints_json_or_fails_closed() {
    let dir = scratch("creds-bin");
    let ok = dir.join("ok.sock");
    let bad = dir.join("bad.sock");
    let good = CredStore::new(Arc::new(FakeRefresher::default()), ManualClock::new(T0));
    good.install(
        ctx(),
        creds(&format_rfc3339(agentd::time::now_unix() + 3600)),
        CANARY_NONCE.into(),
    )
    .await;
    let failing = CredStore::new(
        Arc::new(FakeRefresher {
            fail: true,
            ..Default::default()
        }),
        ManualClock::new(T0),
    );
    failing
        .install(ctx(), creds(&format_rfc3339(T0 + 30)), CANARY_NONCE.into())
        .await;
    let _a = serve_credentials(&ok, good).await.unwrap();
    let _b = serve_credentials(&bad, failing).await.unwrap();
    let exe = env!("CARGO_BIN_EXE_agentd");
    let out = tokio::process::Command::new(exe)
        .args(["creds", "--socket"])
        .arg(&ok)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: ProcessCredentials = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed.version, 1);
    let out = tokio::process::Command::new(exe)
        .args(["creds", "--socket"])
        .arg(&bad)
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let out = tokio::process::Command::new(exe)
        .args(["creds"])
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
