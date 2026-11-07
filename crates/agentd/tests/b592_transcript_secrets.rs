//! The transcript writer redacts with the run's own secret set. A secret that exists only in that set (minted by a refresh inside the
//! writer's own 60 second window, or never one of the two values the writer's keeper reads) is hidden in the stored conversation.
#![allow(clippy::unwrap_used)]

mod direct_transcript_support;

use agentd::acp::Dir;
use agentd::direct::transcript::settings::Settings;
use agentd::direct::transcript::store::ChunkStore;
use agentd::direct::transcript::writer::{Writer, WriterConfig};
use agentd::secrets::SecretSet;
use direct_transcript_support::{MemStore, RUN, ULID_A, all_records};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn quick() -> Settings {
    Settings {
        upload_tries: 3,
        retry_base: Duration::from_millis(10),
        final_deadline: Duration::from_secs(3),
        ..Settings::default()
    }
}

fn start(store: &Arc<MemStore>) -> Writer {
    let dynstore: Arc<dyn ChunkStore> = store.clone();
    Writer::start(
        WriterConfig {
            run_id: RUN.to_string(),
            engine: "Engine".to_string(),
            protocol: "1.38.0".to_string(),
            incarnation: ULID_A.to_string(),
            settings: quick(),
            secrets: Vec::new(),
        },
        dynstore,
    )
}

fn tool_done(i: usize, out: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "sess-1", "update": {
        "sessionUpdate": "tool_call_update", "toolCallId": format!("t{i}"), "status": "completed",
        "content": [{"type": "content", "content": {"type": "text", "text": out}}]}}})
}

fn all_text(store: &MemStore) -> String {
    all_records(store)
        .iter()
        .map(|(_, r)| r.to_string())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn b592_a_secret_minted_after_the_writer_started_is_redacted_from_the_run_set() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    let run_secrets = SecretSet::new(vec!["the-nonce-the-writer-never-read-00000001".to_string()]);
    w.follow_secrets(run_secrets.clone());
    // A value the writer's own keeper does not read (the refresh nonce), and one minted by a refresh just now.
    run_secrets.add(&["a-secret-minted-by-a-refresh-1234567890".to_string()]);
    w.push(
        Dir::FromEngine,
        &tool_done(
            1,
            "nonce the-nonce-the-writer-never-read-00000001 and key a-secret-minted-by-a-refresh-1234567890 end",
        ),
        false,
    );
    w.finish().await;
    let text = all_text(&store);
    assert!(!text.contains("the-nonce-the-writer-never-read"), "{text}");
    assert!(!text.contains("a-secret-minted-by-a-refresh"), "{text}");
    assert!(
        text.contains("nonce <redacted> and key <redacted> end"),
        "{text}"
    );
}

#[tokio::test(start_paused = true)]
async fn b592_a_writer_that_follows_no_set_keeps_the_redaction_it_had() {
    let store = Arc::new(MemStore::new());
    let w = start(&store);
    let secret = "wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY0123456789";
    w.add_secrets(&[secret.to_string()]);
    w.push(
        Dir::FromEngine,
        &tool_done(1, &format!("after {secret}")),
        false,
    );
    w.finish().await;
    let text = all_text(&store);
    assert!(text.contains("after <redacted>"), "{text}");
}
