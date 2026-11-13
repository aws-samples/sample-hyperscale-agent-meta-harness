//! Agentd derives the run's completion key from the first nonce of the invocation and sends it on `/internal/events` and
//! `/internal/turn-complete`, the two routes worker-api guards with it. The key is a secret of the run: no event, log line or summary holds it.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use common::CANARY_NONCE;
use direct_support::{Direct, Opts};
use harness_protocol::store::CredsBinding;

#[tokio::test(flavor = "multi_thread")]
async fn b593_every_events_and_turn_complete_post_carries_the_runs_key() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let key = CredsBinding::completion_key(CANARY_NONCE);
    let posts = d.aws.completion_keys();
    let events: Vec<_> = posts
        .iter()
        .filter(|(p, _)| p == "/internal/events")
        .collect();
    let done: Vec<_> = posts
        .iter()
        .filter(|(p, _)| p == "/internal/turn-complete")
        .collect();
    // TRIGGER PRESENT: the run posted both kinds of request.
    assert!(!events.is_empty(), "no events post: {posts:?}");
    assert!(!done.is_empty(), "no turn-complete post: {posts:?}");
    for (path, sent) in &posts {
        assert_eq!(sent.as_deref(), Some(key.as_str()), "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b593_the_key_reaches_no_event_log_line_or_summary() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let key = CredsBinding::completion_key(CANARY_NONCE);
    let events = format!("{:?}", d.aws.envelopes());
    assert!(!events.contains(&key), "the key is in an event");
    assert!(
        !d.log.lines().join("\n").contains(&key),
        "the key is in the log"
    );
    let summary = std::fs::read_to_string(d.agent_data().join("turns/1/summary.json")).unwrap();
    assert!(!summary.contains(&key), "the key is in summary.json");
}
