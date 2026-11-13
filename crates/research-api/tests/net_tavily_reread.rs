//! Two callers of one keyed client whose key was rotated: the one whose own re-read is skipped (the window is open because the other
//! caller just re-read) still looks at the key now cached and retries once when it differs from the key that was refused.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_keys.rs"]
mod net_keys;

use harness_protocol::research::SearchRequest;
use net_fake::{FakeServer, Reply};
use net_keys::{FakeClock, Script, key};
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::ports::Provider;
use research_api::secret::CachedSecret;
use research_api::tavily::TavilyClient;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

fn search(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_caller_whose_re_read_was_skipped_retries_with_the_key_another_caller_loaded() {
    let (old, new) = (key("old"), key("new"));
    let good = new.clone();
    // The refusal of the old key is slow, so both calls are in flight with the old key before either re-reads.
    let server = FakeServer::start(move |r| {
        if r.header("authorization") == Some(format!("Bearer {good}").as_str()) {
            Reply::json(200, r#"{"results":[{"url":"https://example.org/a","title":"A","content":"c"}],"request_id":"r"}"#)
        } else {
            Reply::Delayed(Duration::from_millis(150), Box::new(Reply::json(401, "{}")))
        }
    })
    .await;
    let mut hosts = BTreeMap::new();
    hosts.insert("api.tavily.com".to_string(), server.addr);
    let http = HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(5),
            max_response_bytes: 1024 * 1024,
        },
        TestNet { hosts },
    )
    .unwrap();
    let script = Script::keys(&[&old, &new]);
    let secret = CachedSecret::with_clock(script.clone(), Arc::new(FakeClock::default()));
    let client = TavilyClient::keyed(http, "https://api.tavily.com".into(), secret);

    let (first, second) = (search("one"), search("two"));
    let (a, b) = tokio::join!(client.search(&first), client.search(&second));
    assert_eq!(a.unwrap().results.len(), 1);
    assert_eq!(
        b.unwrap().results.len(),
        1,
        "the second caller must not give up on a key that was already replaced"
    );
    assert_eq!(
        script.reads(),
        2,
        "one normal read and one re-read for both callers"
    );
    let seen: Vec<String> = server
        .requests()
        .iter()
        .map(|r| r.header("authorization").unwrap().to_string())
        .collect();
    assert_eq!(
        seen.iter()
            .filter(|a| **a == format!("Bearer {old}"))
            .count(),
        2
    );
    assert_eq!(
        seen.iter()
            .filter(|a| **a == format!("Bearer {new}"))
            .count(),
        2
    );
}
