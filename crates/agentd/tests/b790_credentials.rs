//! The credential helper of the second engine. OpenCode 2.2 starts the profile's `credential_process` for every Bedrock request
//! (four requests, four starts), so a helper that took seconds would add seconds to every model call. `agentd creds --socket` is that helper:
//! it connects to the socket agentd serves and prints one line. This test measures the real helper over a stand-in store and keeps it far
//! under the cost of a model call.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::BoxFuture;
use agentd::creds::{CredStore, RefreshContext, Refresher};
use agentd::creds_socket::serve_credentials;
use agentd::time::{ManualClock, format_rfc3339};
use common::{CANARY_NONCE, creds, scratch};
use harness_protocol::api::internal::{CredsRequest, CredsResponse, ProcessCredentials};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct NoRefresh;

impl Refresher for NoRefresh {
    fn refresh<'a>(
        &'a self,
        _ctx: &'a RefreshContext,
        _current: &'a ProcessCredentials,
        _req: CredsRequest,
    ) -> BoxFuture<'a, Result<CredsResponse, String>> {
        Box::pin(async { Err("no refresh is due in this test".to_string()) })
    }
}

fn ctx() -> RefreshContext {
    RefreshContext {
        run_id: "run1".into(),
        agent_id: "a1".into(),
        runtime_session_id: "run1-a1-session-000000000000000000001".into(),
        internal_api_url: "https://internal.example/live".into(),
        region: "us-west-2".into(),
    }
}

#[tokio::test]
async fn b790_the_credential_helper_answers_a_burst_of_starts_in_milliseconds() {
    let dir = scratch("creds-latency");
    let sock = dir.join("c.sock");
    let store = CredStore::new(
        Arc::new(NoRefresh),
        ManualClock::new(agentd::time::now_unix()),
    );
    store
        .install(
            ctx(),
            creds(&format_rfc3339(agentd::time::now_unix() + 3600)),
            CANARY_NONCE.into(),
        )
        .await;
    let _server = serve_credentials(&sock, store).await.unwrap();
    let exe = env!("CARGO_BIN_EXE_agentd");
    // Eight starts one after the other, as the engine makes them for the requests of a turn; then eight at once, as parallel requests make them.
    let mut each = Vec::new();
    for _ in 0..8 {
        let t = Instant::now();
        let out = tokio::process::Command::new(exe)
            .args(["creds", "--socket"])
            .arg(&sock)
            .output()
            .await
            .unwrap();
        each.push(t.elapsed());
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let parsed: ProcessCredentials = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(parsed.version, 1);
    }
    each.sort();
    let (median, worst) = (each[each.len() / 2], *each.last().unwrap());
    eprintln!("credential helper: median {median:?}, worst {worst:?}");
    // The bounds are generous (a loaded build host runs this): they stay far below one model call and far below the seconds of a helper that
    // is slow by design.
    assert!(median < Duration::from_millis(500), "median {median:?}");
    assert!(worst < Duration::from_secs(2), "worst {worst:?}");
    let t = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let sock = sock.clone();
            tokio::spawn(async move {
                tokio::process::Command::new(exe)
                    .args(["creds", "--socket"])
                    .arg(&sock)
                    .output()
                    .await
                    .unwrap()
            })
        })
        .collect();
    for h in handles {
        assert!(h.await.unwrap().status.success());
    }
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "eight at once took {:?}",
        t.elapsed()
    );
}
