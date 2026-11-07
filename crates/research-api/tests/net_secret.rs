//! The Tavily key: how it is held, cached and re-read. The AWS adapter runs against recorded HTTP exchanges
//! (aws-smithy StaticReplayClient); no test reaches the network and no real key exists here: every value is built at run time.

#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::research::ResearchErrorCode as C;
use research_api::ports::Fail;
use research_api::secret::{
    AwsSecretSource, CachedSecret, Clock, SecretSource, SecretString, StoredKey,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

fn value(tag: &str) -> String {
    ["stand", "-in-", tag, "-0123456789abcdef"].concat()
}

// ---- SecretString ----------------------------------------------------------------------------------------------------------------------

#[test]
fn a_secret_prints_as_redacted_everywhere() {
    #[derive(Debug)]
    struct Holder {
        key: SecretString,
    }
    let v = value("a");
    let s = SecretString::new(v.clone());
    assert_eq!(format!("{s:?}"), "<redacted>");
    assert_eq!(format!("{s}"), "<redacted>");
    let h = Holder { key: s.clone() };
    let shown = format!("{h:?} {h:#?}");
    assert!(!shown.contains(&v), "{shown}");
    assert!(shown.contains("<redacted>"));
    assert!(s.matches(&v) && !s.matches("other"));
    assert!(h.key.matches(&v));
    assert!(!SecretString::new(String::new()).matches(&v));
}

// ---- the cache ------------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct FakeClock(AtomicU64);

impl FakeClock {
    fn advance_s(&self, s: u64) {
        self.0.fetch_add(s * 1000, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

type Answer = Result<Option<String>, Fail>;

/// Answers from a script and counts the reads.
#[derive(Clone, Default)]
struct Script {
    answers: Arc<Mutex<VecDeque<Answer>>>,
    reads: Arc<AtomicU64>,
}

impl Script {
    fn with(answers: Vec<Answer>) -> Self {
        Self {
            answers: Arc::new(Mutex::new(answers.into())),
            reads: Arc::default(),
        }
    }
    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl SecretSource for Script {
    async fn get(&self) -> Result<Option<StoredKey>, Fail> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let next = self.answers.lock().unwrap().pop_front();
        match next {
            Some(Ok(v)) => Ok(v.map(|k| StoredKey::undated(SecretString::new(k)))),
            Some(Err(f)) => Err(f),
            None => Err(Fail::internal("script is empty")),
        }
    }
}

fn cached(script: &Script) -> (CachedSecret<Script, Arc<FakeClock>>, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::default());
    (
        CachedSecret::with_clock(script.clone(), clock.clone()),
        clock,
    )
}

#[tokio::test]
async fn a_read_is_kept_for_five_minutes() {
    let (a, b) = (value("a"), value("b"));
    let script = Script::with(vec![Ok(Some(a.clone())), Ok(Some(b.clone()))]);
    let (secret, clock) = cached(&script);
    assert!(secret.get().await.unwrap().unwrap().matches(&a));
    clock.advance_s(299);
    assert!(secret.get().await.unwrap().unwrap().matches(&a));
    assert_eq!(script.reads(), 1, "served from the cache");
    clock.advance_s(2);
    assert!(
        secret.get().await.unwrap().unwrap().matches(&b),
        "after 300 s the secret is read again"
    );
    assert_eq!(script.reads(), 2);
}

#[tokio::test]
async fn no_key_is_remembered_for_thirty_seconds_only() {
    let a = value("a");
    let script = Script::with(vec![Ok(None), Ok(Some(a.clone()))]);
    let (secret, clock) = cached(&script);
    assert!(secret.get().await.unwrap().is_none());
    clock.advance_s(29);
    assert!(secret.get().await.unwrap().is_none());
    assert_eq!(script.reads(), 1);
    clock.advance_s(2);
    assert!(
        secret.get().await.unwrap().unwrap().matches(&a),
        "a key the owner stores becomes usable within half a minute"
    );
}

#[tokio::test]
async fn a_failing_store_serves_the_last_value_and_fails_when_there_is_none() {
    let a = value("a");
    let down = || Fail::new(C::UpstreamError, "the key store did not answer");
    let script = Script::with(vec![Ok(Some(a.clone())), Err(down())]);
    let (secret, clock) = cached(&script);
    secret.get().await.unwrap();
    clock.advance_s(400);
    assert!(
        secret.get().await.unwrap().unwrap().matches(&a),
        "stale value while the store is down"
    );

    let script = Script::with(vec![Err(down())]);
    let (secret, _) = cached(&script);
    assert_eq!(secret.get().await.unwrap_err().code, C::UpstreamError);
}

#[tokio::test]
async fn a_re_read_after_an_authentication_failure_happens_at_most_every_thirty_seconds() {
    let (a, b, c) = (value("a"), value("b"), value("c"));
    let script = Script::with(vec![
        Ok(Some(a.clone())),
        Ok(Some(b.clone())),
        Ok(Some(c.clone())),
    ]);
    let (secret, clock) = cached(&script);
    assert!(secret.get().await.unwrap().unwrap().matches(&a));
    assert!(
        secret.reread_after_auth_failure().await.unwrap(),
        "first failure: a fresh read"
    );
    assert!(
        secret.get().await.unwrap().unwrap().matches(&b),
        "the re-read value is the cached one now"
    );
    clock.advance_s(29);
    assert!(
        !secret.reread_after_auth_failure().await.unwrap(),
        "inside the window: no read"
    );
    assert_eq!(script.reads(), 2);
    clock.advance_s(2);
    assert!(secret.reread_after_auth_failure().await.unwrap());
    assert!(secret.get().await.unwrap().unwrap().matches(&c));
    assert_eq!(script.reads(), 3);
}

#[tokio::test]
async fn a_failed_re_read_is_an_error_and_keeps_the_old_value() {
    let a = value("a");
    let script = Script::with(vec![
        Ok(Some(a.clone())),
        Err(Fail::new(C::UpstreamError, "down")),
    ]);
    let (secret, _) = cached(&script);
    secret.get().await.unwrap();
    assert_eq!(
        secret.reread_after_auth_failure().await.unwrap_err().code,
        C::UpstreamError
    );
    assert!(secret.get().await.unwrap().unwrap().matches(&a));
}

// ---- how long a key may be served while the store is down, and when an expired key is dropped ---------------------------------------------

/// A store that can be switched off, holding one current value.
#[derive(Clone)]
struct Switch {
    down: Arc<std::sync::atomic::AtomicBool>,
    current: Arc<Mutex<Option<String>>>,
}

impl Switch {
    fn holding(v: &str) -> Self {
        Self {
            down: Arc::default(),
            current: Arc::new(Mutex::new(Some(v.to_string()))),
        }
    }
    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
    fn set(&self, v: Option<&str>) {
        *self.current.lock().unwrap() = v.map(str::to_string);
    }
}

impl SecretSource for Switch {
    async fn get(&self) -> Result<Option<StoredKey>, Fail> {
        if self.down.load(Ordering::SeqCst) {
            return Err(Fail::new(C::UpstreamError, "the key store did not answer"));
        }
        Ok(self
            .current
            .lock()
            .unwrap()
            .clone()
            .map(|k| StoredKey::undated(SecretString::new(k))))
    }
}

#[tokio::test]
async fn a_key_is_served_while_the_store_is_down_for_one_hour_after_its_last_good_read_and_then_dropped()
 {
    let a = value("a");
    let store = Switch::holding(&a);
    let clock = Arc::new(FakeClock::default());
    let secret = CachedSecret::with_clock(store.clone(), clock.clone());
    let held = secret.get().await.unwrap().unwrap();
    store.set_down(true);

    clock.advance_s(400);
    assert!(
        secret.get().await.unwrap().unwrap().matches(&a),
        "past the cache time, the store down: the last key"
    );
    clock.advance_s(3199); // 3599 s after the good read
    assert!(
        secret.get().await.unwrap().unwrap().matches(&a),
        "still inside the hour"
    );
    assert_eq!(Arc::strong_count(&held), 2, "the cache still holds the key");

    clock.advance_s(1); // 3600 s
    let err = secret.get().await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert_eq!(
        Arc::strong_count(&held),
        1,
        "after the hour the cache no longer holds the key"
    );

    // With the store down and nothing cached the failure stays, and a store that comes back is read at once.
    clock.advance_s(40);
    assert_eq!(secret.get().await.unwrap_err().code, C::UpstreamError);
    store.set_down(false);
    clock.advance_s(40);
    assert!(secret.get().await.unwrap().unwrap().matches(&a));
}

#[tokio::test]
async fn the_hour_counts_from_the_last_good_read_not_from_the_first() {
    let (a, b) = (value("a"), value("b"));
    let store = Switch::holding(&a);
    let clock = Arc::new(FakeClock::default());
    let secret = CachedSecret::with_clock(store.clone(), clock.clone());
    secret.get().await.unwrap();
    clock.advance_s(3000);
    store.set(Some(&b));
    assert!(
        secret.get().await.unwrap().unwrap().matches(&b),
        "a good read at 3000 s starts a new hour"
    );
    store.set_down(true);
    clock.advance_s(3000); // 6000 s in all, 3000 s after the last good read
    assert!(secret.get().await.unwrap().unwrap().matches(&b));
    clock.advance_s(600); // 3600 s after the last good read
    assert_eq!(secret.get().await.unwrap_err().code, C::UpstreamError);
}

#[tokio::test]
async fn an_expired_key_is_replaced_or_dropped_at_the_next_access() {
    let (a, b) = (value("a"), value("b"));
    let store = Switch::holding(&a);
    let clock = Arc::new(FakeClock::default());
    let secret = CachedSecret::with_clock(store.clone(), clock.clone());
    let first = secret.get().await.unwrap().unwrap();
    assert_eq!(Arc::strong_count(&first), 2);

    // Past five minutes with a healthy store: the same text is read again as a new value, and the old one is let go.
    clock.advance_s(301);
    let second = secret.get().await.unwrap().unwrap();
    assert!(second.matches(&a) && !Arc::ptr_eq(&first, &second));
    assert_eq!(Arc::strong_count(&first), 1, "the expired key is not kept");

    // Past five minutes and the store now says no key: the key is gone.
    store.set(None);
    clock.advance_s(301);
    assert!(secret.get().await.unwrap().is_none());
    assert_eq!(Arc::strong_count(&second), 1, "the dropped key is not kept");

    // A new key is read when the store has one.
    store.set(Some(&b));
    clock.advance_s(31);
    assert!(secret.get().await.unwrap().unwrap().matches(&b));
}

// ---- the Secrets Manager adapter ---------------------------------------------------------------------------------------------------------

fn replay(responses: Vec<(u16, &str)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, body)| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(status)
                        .header("content-type", "application/x-amz-json-1.1")
                        .body(SdkBody::from(body.to_string()))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn source(client: &StaticReplayClient) -> AwsSecretSource {
    let conf = aws_sdk_secretsmanager::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_secretsmanager::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_secretsmanager::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    AwsSecretSource::new(
        aws_sdk_secretsmanager::Client::from_conf(conf),
        "hamh-direct-research-tavily-api-key".into(),
    )
}

fn found(secret: &str) -> String {
    serde_json::json!({
        "ARN": "arn:aws:secretsmanager:us-east-1:111122223333:secret:hamh-direct-research-tavily-api-key-AbCdEf",
        "Name": "hamh-direct-research-tavily-api-key", "VersionId": "11111111-2222-3333-4444-555555555555",
        "SecretString": secret, "VersionStages": ["AWSCURRENT"]
    })
    .to_string()
}

#[tokio::test]
async fn the_adapter_reads_the_current_version_of_the_named_secret() {
    let v = value("a");
    let client = replay(vec![(200, &found(&v))]);
    let got = source(&client).get().await.unwrap().unwrap();
    assert!(got.secret.matches(&v));
    let req = client.actual_requests().next().unwrap();
    assert_eq!(
        req.headers().get("x-amz-target").unwrap(),
        "secretsmanager.GetSecretValue"
    );
    let body: serde_json::Value = serde_json::from_slice(req.body().bytes().unwrap()).unwrap();
    assert_eq!(body["SecretId"], "hamh-direct-research-tavily-api-key");
    assert_eq!(body["VersionStage"], "AWSCURRENT");
}

#[tokio::test]
async fn an_empty_secret_a_blank_value_and_a_missing_version_are_no_key() {
    // A secret created empty has no AWSCURRENT version: Secrets Manager answers ResourceNotFoundException.
    let missing = r#"{"__type":"ResourceNotFoundException","Message":"Secrets Manager can't find the specified secret value for staging label: AWSCURRENT"}"#;
    let client = replay(vec![(400, missing)]);
    assert!(source(&client).get().await.unwrap().is_none());
    let client = replay(vec![(
        400,
        r#"{"__type":"ResourceNotFoundException","Message":"Secrets Manager can't find the specified secret."}"#,
    )]);
    assert!(source(&client).get().await.unwrap().is_none());
    for blank in ["", "   ", "\n\t "] {
        let client = replay(vec![(200, &found(blank))]);
        assert!(source(&client).get().await.unwrap().is_none(), "{blank:?}");
    }
    // A value with a trailing newline (typed into a console) is trimmed.
    let v = value("a");
    let client = replay(vec![(200, &found(&format!("{v}\n")))]);
    assert!(
        source(&client)
            .get()
            .await
            .unwrap()
            .unwrap()
            .secret
            .matches(&v)
    );
}

#[tokio::test]
async fn any_other_error_is_an_upstream_error_that_echoes_nothing() {
    let marker = "marker-text-from-the-service";
    let denied = format!(r#"{{"__type":"AccessDeniedException","Message":"{marker}"}}"#);
    let client = replay(vec![(400, &denied)]);
    let err = source(&client).get().await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(
        !err.message.contains(marker) && !format!("{err:?}").contains(marker),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_adapter_reports_the_creation_time_of_the_version_it_read() {
    // `CreatedDate` of GetSecretValue is a Unix time in seconds, with a fraction when Secrets Manager gives one.
    let v = value("a");
    for (created, want_ms) in [
        (serde_json::json!(1_792_929_600), Some(1_792_929_600_000u64)),
        (
            serde_json::json!(1_792_929_600.5),
            Some(1_792_929_600_500u64),
        ),
    ] {
        let body = serde_json::json!({
            "ARN": "arn:aws:secretsmanager:us-east-1:111122223333:secret:hamh-direct-research-tavily-api-key-AbCdEf",
            "Name": "hamh-direct-research-tavily-api-key", "VersionId": "11111111-2222-3333-4444-555555555555",
            "SecretString": v, "VersionStages": ["AWSCURRENT"], "CreatedDate": created
        })
        .to_string();
        let client = replay(vec![(200, &body)]);
        let got = source(&client).get().await.unwrap().unwrap();
        assert!(got.secret.matches(&v));
        assert_eq!(got.stored_at_ms, want_ms, "{created}");
        assert!(
            !format!("{got:?}").contains(&v),
            "a stored key prints without its text"
        );
    }
    // An answer with no creation time gives a key without one.
    let client = replay(vec![(200, &found(&v))]);
    let got = source(&client).get().await.unwrap().unwrap();
    assert_eq!(got.stored_at_ms, None);
}

#[tokio::test]
async fn reading_the_creation_time_needs_no_other_call() {
    // One GetSecretValue, nothing else: the time comes with the value, so no new permission is involved.
    let v = value("a");
    let client = replay(vec![(200, &found(&v))]);
    source(&client).get().await.unwrap();
    let targets: Vec<String> = client
        .actual_requests()
        .map(|r| r.headers().get("x-amz-target").unwrap().to_string())
        .collect();
    assert_eq!(targets, ["secretsmanager.GetSecretValue"]);
}

// ---- the version of the key that is held -----------------------------------------------------------------------------------------

/// The key and the version id the store holds.
type KeyAndVersion = (String, Option<String>);

/// A store of one current key with a Secrets Manager version id and a creation time, that can be switched off.
#[derive(Clone)]
struct Versioned {
    down: Arc<std::sync::atomic::AtomicBool>,
    current: Arc<Mutex<Option<KeyAndVersion>>>,
    created_ms: Option<u64>,
    reads: Arc<AtomicU64>,
}

impl Versioned {
    fn holding(key: &str, version: Option<&str>, created_ms: Option<u64>) -> Self {
        Self {
            down: Arc::default(),
            current: Arc::new(Mutex::new(Some((
                key.to_string(),
                version.map(str::to_string),
            )))),
            created_ms,
            reads: Arc::default(),
        }
    }
    fn set(&self, key: Option<(&str, &str)>) {
        *self.current.lock().unwrap() = key.map(|(k, v)| (k.to_string(), Some(v.to_string())));
    }
    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl SecretSource for Versioned {
    async fn get(&self) -> Result<Option<StoredKey>, Fail> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(Fail::new(C::UpstreamError, "the key store did not answer"));
        }
        Ok(self.current.lock().unwrap().clone().map(|(k, v)| {
            let stored = StoredKey::new(SecretString::new(k), self.created_ms);
            match v {
                Some(v) => stored.with_version_id(v),
                None => stored,
            }
        }))
    }
}

fn versioned(store: &Versioned) -> (CachedSecret<Versioned, Arc<FakeClock>>, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::default());
    (
        CachedSecret::with_clock(store.clone(), clock.clone()),
        clock,
    )
}

#[tokio::test]
async fn the_cache_reports_the_version_id_of_the_key_it_holds() {
    let store = Versioned::holding(&value("a"), Some("ver-1"), None);
    let (secret, _) = versioned(&store);
    assert_eq!(
        secret.version_now().await.unwrap().as_deref(),
        Some("ver-1")
    );
    assert_eq!(store.reads(), 1);
}

#[tokio::test]
async fn the_version_follows_the_cache_it_is_old_for_five_minutes_and_then_the_new_one() {
    let store = Versioned::holding(&value("a"), Some("ver-1"), None);
    let (secret, clock) = versioned(&store);
    assert_eq!(
        secret.version_now().await.unwrap().as_deref(),
        Some("ver-1")
    );
    store.set(Some((&value("b"), "ver-2")));
    clock.advance_s(299);
    assert_eq!(
        secret.version_now().await.unwrap().as_deref(),
        Some("ver-1"),
        "the key held is still the old one, and so is its version"
    );
    assert_eq!(store.reads(), 1);
    clock.advance_s(2);
    assert_eq!(
        secret.version_now().await.unwrap().as_deref(),
        Some("ver-2"),
        "past five minutes the question itself reads the secret again"
    );
    assert!(secret.get().await.unwrap().unwrap().matches(&value("b")));
}

#[tokio::test]
async fn no_key_a_key_without_a_version_id_and_a_dropped_key_have_no_version() {
    let store = Versioned::holding(&value("a"), None, None);
    let (secret, _) = versioned(&store);
    assert_eq!(
        secret.version_now().await.unwrap(),
        None,
        "the key has no id"
    );
    assert!(secret.get().await.unwrap().is_some());

    let store = Versioned::holding(&value("a"), Some("ver-1"), None);
    let (secret, clock) = versioned(&store);
    assert!(secret.version_now().await.unwrap().is_some());
    *store.current.lock().unwrap() = None;
    clock.advance_s(301);
    assert_eq!(
        secret.version_now().await.unwrap(),
        None,
        "the store has no key now, so none is held and none has a version"
    );
}

#[tokio::test]
async fn a_store_that_is_down_keeps_the_old_version_with_the_old_key_for_one_hour() {
    let store = Versioned::holding(&value("a"), Some("ver-1"), None);
    let (secret, clock) = versioned(&store);
    secret.version_now().await.unwrap();
    store.set_down(true);
    clock.advance_s(400);
    assert_eq!(
        secret.version_now().await.unwrap().as_deref(),
        Some("ver-1"),
        "the key served while the store is down is the one with this version"
    );
    clock.advance_s(3200);
    assert_eq!(
        secret.version_now().await.unwrap_err().code,
        C::UpstreamError
    );
}

#[tokio::test]
async fn a_re_read_after_a_refusal_updates_the_version() {
    let store = Versioned::holding(&value("a"), Some("ver-1"), None);
    let (secret, _) = versioned(&store);
    secret.version_now().await.unwrap();
    store.set(Some((&value("b"), "ver-2")));
    assert!(secret.reread_after_auth_failure().await.unwrap());
    assert_eq!(
        secret.version_now().await.unwrap().as_deref(),
        Some("ver-2")
    );
}

#[tokio::test]
async fn asking_for_the_version_leaves_the_creation_time_for_the_metrics() {
    let store = Versioned::holding(&value("a"), Some("ver-1"), Some(1_792_929_600_000));
    let (secret, _) = versioned(&store);
    secret.version_now().await.unwrap();
    assert_eq!(
        secret.take_load().await,
        Some(1_792_929_600_000),
        "take_load still hands the creation time of the read on, once"
    );
    assert_eq!(secret.take_load().await, None);
}

#[tokio::test]
async fn the_adapter_reports_the_version_id_of_the_current_version_and_it_prints_without_the_key() {
    let v = value("a");
    let client = replay(vec![(200, &found(&v))]);
    let got = source(&client).get().await.unwrap().unwrap();
    assert_eq!(
        got.version_id.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
    assert!(!format!("{got:?}").contains(&v));
    // An answer without a version id gives a key without one.
    let body = serde_json::json!({"SecretString": v, "VersionStages": ["AWSCURRENT"]}).to_string();
    let client = replay(vec![(200, &body)]);
    let got = source(&client).get().await.unwrap().unwrap();
    assert_eq!(got.version_id, None);
}
