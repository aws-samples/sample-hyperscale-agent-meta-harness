//! The metrics of the Tavily key: the Embedded Metric Format lines, the events the Tavily client reports for each kind of
//! answer, the age of the stored key, and the rule that no line holds the key. The client runs against the scripted fake Tavily on 127.0.0.1;
//! the key store is a script; the clocks are fakes. No test reaches the network, and every key value is built at run time.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;
#[path = "support/net_keys.rs"]
mod net_keys;

use harness_protocol::research::{ResearchErrorCode as C, SearchRequest};
use net_fake::{FakeServer, Recorded, Reply};
use net_keys::{FakeClock, Script, key};
use proptest::prelude::*;
use research_api::fakes::FixedClock;
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::metrics::{DIMENSION, EmfMetrics, Event, Metrics, NAMESPACE, NoMetrics, name};
use research_api::ports::{Clock, Provider};
use research_api::secret::CachedSecret;
use research_api::tavily::TavilyClient;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 2026-10-25 12:00:00 UTC in epoch milliseconds.
const NOW_MS: u64 = 1_792_929_600_000;
const DAY_MS: u64 = 86_400_000;

// ---- stand-ins -----------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Event>>>);

impl Recorder {
    fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

impl Metrics for Recorder {
    fn record(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

/// A writer a test can read back, and can make fail.
#[derive(Clone, Default)]
struct Buf {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.bytes.lock().unwrap().clone()).unwrap()
    }
    fn lines(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct BrokenPipe;

impl Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the log is closed"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("the log is closed"))
    }
}

/// A wall clock a test moves after the sink took it.
#[derive(Clone)]
struct Wall(Arc<FixedClock>);

impl Wall {
    fn at(ms: u64) -> Self {
        Self(Arc::new(FixedClock::at_ms(ms)))
    }
}

impl Clock for Wall {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

fn emf(buf: &Buf, wall: &Wall) -> EmfMetrics<Buf, Wall> {
    EmfMetrics::new("direct", wall.clone(), buf.clone()).unwrap()
}

// ---- the lines ---------------------------------------------------------------------------------------------------------------------

#[test]
fn the_namespace_dimension_and_metric_names_are_the_strings_the_alarms_use() {
    // infra/lib/stacks/research-alarms.ts holds the same five strings; infra/test/research-alarms.test.ts reads this file and compares.
    assert_eq!(NAMESPACE, "Hamh/Research");
    assert_eq!(DIMENSION, "Stage");
    assert_eq!(name::KEY_REFUSED, "TavilyKeyRefused");
    assert_eq!(name::RATE_LIMITED, "TavilyRateLimited");
    assert_eq!(name::PLAN_LIMIT, "TavilyPlanLimit");
    assert_eq!(name::KEY_AGE_DAYS, "TavilyKeyAgeDays");
}

#[test]
fn each_event_is_one_embedded_metric_format_line_with_its_name_unit_and_value() {
    let wall = Wall::at(NOW_MS);
    let buf = Buf::default();
    let sink = emf(&buf, &wall);
    let stored = NOW_MS - 3 * DAY_MS - DAY_MS / 4;
    let cases = [
        (Event::KeyRefused, name::KEY_REFUSED, "Count", json!(1)),
        (Event::RateLimited, name::RATE_LIMITED, "Count", json!(1)),
        (Event::PlanLimit, name::PLAN_LIMIT, "Count", json!(1)),
        (
            Event::KeyLoaded {
                stored_at_ms: stored,
            },
            name::KEY_AGE_DAYS,
            "None",
            json!(3.25),
        ),
    ];
    for (event, _, _, _) in &cases {
        sink.record(*event);
    }
    let text = buf.text();
    assert!(text.ends_with('\n'), "every line ends with a newline");
    assert_eq!(text.lines().count(), 4, "one line per event: {text}");
    for (line, (_, metric, unit, value)) in buf.lines().iter().zip(&cases) {
        let aws = &line["_aws"];
        assert_eq!(
            aws["Timestamp"].as_u64(),
            Some(NOW_MS),
            "the time of the line, as an integer in milliseconds: {line}"
        );
        let directives = aws["CloudWatchMetrics"].as_array().unwrap();
        assert_eq!(directives.len(), 1, "{line}");
        let directive = &directives[0];
        assert_eq!(directive["Namespace"], "Hamh/Research", "{line}");
        assert_eq!(directive["Dimensions"], json!([["Stage"]]), "{line}");
        assert_eq!(
            directive["Metrics"],
            json!([{ "Name": metric, "Unit": unit }]),
            "{line}"
        );
        assert_eq!(line["Stage"], "direct", "{line}");
        assert_eq!(&line[*metric], value, "{line}");
        let mut members: Vec<&str> = line
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        members.sort_unstable();
        let mut expected = vec!["Stage", "_aws", *metric];
        expected.sort_unstable();
        assert_eq!(
            members, expected,
            "a line holds the stage and one number only"
        );
    }
}

#[test]
fn the_age_of_a_key_is_the_time_since_its_version_was_stored_in_days() {
    let wall = Wall::at(NOW_MS);
    let buf = Buf::default();
    let sink = emf(&buf, &wall);
    // Stored now, stored a day ago, stored 30.5 days ago, and stored in the future (clock skew reads as zero, never negative).
    for stored_at_ms in [
        NOW_MS,
        NOW_MS - DAY_MS,
        NOW_MS - 30 * DAY_MS - DAY_MS / 2,
        NOW_MS + 3_600_000,
    ] {
        sink.record(Event::KeyLoaded { stored_at_ms });
    }
    let ages: Vec<f64> = buf
        .lines()
        .iter()
        .map(|l| l[name::KEY_AGE_DAYS].as_f64().unwrap())
        .collect();
    assert_eq!(ages, [0.0, 1.0, 30.5, 0.0]);
    // Time passes: the same stored time reads older, and the line carries the time it was written.
    wall.0.advance_s(3600);
    let buf = Buf::default();
    let sink = emf(&buf, &wall);
    sink.record(Event::KeyLoaded {
        stored_at_ms: NOW_MS - DAY_MS,
    });
    let line = &buf.lines()[0];
    let age = line[name::KEY_AGE_DAYS].as_f64().unwrap();
    assert!((age - (1.0 + 1.0 / 24.0)).abs() < 1e-9, "{age}");
    assert_eq!(line["_aws"]["Timestamp"].as_u64(), Some(NOW_MS + 3_600_000));
}

#[test]
fn the_stage_dimension_value_is_checked() {
    let ok = |s: &str| EmfMetrics::new(s, Wall::at(NOW_MS), Buf::default()).is_ok();
    assert!(ok("direct") && ok("test-2") && ok("a"));
    let too_long = "x".repeat(33);
    for bad in [
        "",
        "Direct",
        "a b",
        "a/b",
        "a\"b",
        "stage_1",
        too_long.as_str(),
    ] {
        assert!(!ok(bad), "{bad:?}");
    }
}

#[test]
fn a_log_that_cannot_be_written_never_fails_the_caller() {
    let sink = EmfMetrics::new("direct", Wall::at(NOW_MS), BrokenPipe).unwrap();
    sink.record(Event::KeyRefused);
    sink.record(Event::KeyLoaded {
        stored_at_ms: NOW_MS,
    });
}

#[test]
fn the_default_sink_reports_nothing_and_the_modules_keep_their_distance() {
    NoMetrics.record(Event::PlanLimit);
    // The metrics module takes no key, response or address from any other module, and the secret module takes no sink: the numbers are
    // reported by the Tavily client, outside the code that holds the key's text.
    let metrics = include_str!("../src/metrics.rs");
    for line in metrics.lines().filter(|l| l.starts_with("use ")) {
        for banned in ["secret", "http", "tavily", "aws", "scan"] {
            assert!(
                !line.contains(banned),
                "metrics.rs imports {banned}: {line}"
            );
        }
    }
    let secret = include_str!("../src/secret.rs");
    assert!(
        !secret.contains("crate::metrics") && !secret.contains("Metrics"),
        "secret.rs must not report anything"
    );
}

// ---- the Tavily client reports --------------------------------------------------------------------------------------------------------

fn http_for(addr: SocketAddr) -> HttpClient {
    let mut hosts = BTreeMap::new();
    hosts.insert("api.tavily.com".to_string(), addr);
    HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(3),
            max_response_bytes: 1024 * 1024,
        },
        TestNet { hosts },
    )
    .unwrap()
}

type Keyed = TavilyClient<Script, Arc<FakeClock>>;

fn keyed(server: &FakeServer, script: &Script, sink: Arc<dyn Metrics>) -> (Keyed, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::default());
    let secret = CachedSecret::with_clock(script.clone(), clock.clone());
    let client = TavilyClient::keyed(
        http_for(server.addr),
        "https://api.tavily.com".into(),
        secret,
    )
    .with_metrics(sink);
    (client, clock)
}

fn search(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        ..Default::default()
    }
}

fn ok_answer() -> Reply {
    Reply::json(200, r#"{"results":[],"request_id":"r"}"#)
}

fn status_only(status: u16) -> impl Fn(&Recorded) -> Reply + Send + Sync + 'static {
    move |_| Reply::json(status, r#"{"detail":{"error":"x"}}"#)
}

#[tokio::test]
async fn every_refused_answer_counts_once_a_401_and_a_403_alike() {
    for status in [401u16, 403] {
        let server = FakeServer::start(status_only(status)).await;
        let rec = Recorder::default();
        // The same key after the re-read: no retry, one answer, one count.
        let (client, _) = keyed(&server, &Script::keys(&[&key("a")]), Arc::new(rec.clone()));
        let err = client.search(&search("q")).await.unwrap_err();
        assert_eq!(err.code, C::NoKey, "{status}");
        assert_eq!(server.count(), 1, "{status}");
        assert_eq!(rec.events(), [Event::KeyRefused], "{status}");
    }
}

#[tokio::test]
async fn a_rotation_counts_the_refusal_and_then_reports_the_age_of_the_new_key() {
    let (old, new) = (key("old"), key("new"));
    let (t_old, t_new) = (NOW_MS - 40 * DAY_MS, NOW_MS - 60_000);
    let hits = Arc::new(AtomicUsize::new(0));
    let good = new.clone();
    let counted = hits.clone();
    let server = FakeServer::start(move |r| {
        counted.fetch_add(1, Ordering::SeqCst);
        if r.header("authorization") == Some(format!("Bearer {good}").as_str()) {
            ok_answer()
        } else {
            Reply::json(401, r#"{"detail":{"error":"Unauthorized"}}"#)
        }
    })
    .await;
    let rec = Recorder::default();
    let (client, _) = keyed(
        &server,
        &Script::dated(&[(&old, t_old), (&new, t_new)]),
        Arc::new(rec.clone()),
    );
    client.search(&search("q")).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        rec.events(),
        [
            Event::KeyLoaded {
                stored_at_ms: t_old
            },
            Event::KeyRefused,
            Event::KeyLoaded {
                stored_at_ms: t_new
            },
        ]
    );
}

#[tokio::test]
async fn a_429_counts_as_rate_limited_once_per_answer() {
    let server = FakeServer::start(|_| {
        Reply::bytes(
            429,
            &[("Retry-After", "7")],
            br#"{"detail":{"error":"slow down"}}"#,
        )
    })
    .await;
    let rec = Recorder::default();
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]), Arc::new(rec.clone()));
    for _ in 0..3 {
        let err = client.search(&search("q")).await.unwrap_err();
        assert_eq!(err.code, C::UpstreamRateLimited);
        assert_eq!(err.retry_after_s, Some(7));
    }
    assert_eq!(rec.events(), [Event::RateLimited; 3]);
}

#[tokio::test]
async fn a_432_and_a_433_count_as_a_plan_limit_and_stay_upstream_errors() {
    for status in [432u16, 433] {
        let server = FakeServer::start(status_only(status)).await;
        let rec = Recorder::default();
        let (client, _) = keyed(&server, &Script::keys(&[&key("a")]), Arc::new(rec.clone()));
        let err = client.search(&search("q")).await.unwrap_err();
        assert_eq!(err.code, C::UpstreamError, "{status}");
        assert!(err.message.contains(&status.to_string()), "{}", err.message);
        assert_eq!(rec.events(), [Event::PlanLimit], "{status}");
        let err = client
            .extract(&["https://example.org/a".to_string()], None)
            .await
            .unwrap_err();
        assert_eq!(err.code, C::UpstreamError, "{status}");
        assert_eq!(rec.events(), [Event::PlanLimit; 2], "{status}");
    }
}

#[tokio::test]
async fn other_answers_count_nothing() {
    for status in [200u16, 400, 404, 422, 500, 502, 503] {
        let server = FakeServer::start(move |_| {
            if status == 200 {
                ok_answer()
            } else {
                Reply::json(status, r#"{"detail":{"error":"x"}}"#)
            }
        })
        .await;
        let rec = Recorder::default();
        let (client, _) = keyed(&server, &Script::keys(&[&key("a")]), Arc::new(rec.clone()));
        let _ = client.search(&search("q")).await;
        assert_eq!(server.count(), 1, "{status}");
        assert!(rec.events().is_empty(), "{status}: {:?}", rec.events());
    }
}

#[tokio::test]
async fn a_status_counts_even_when_the_body_repeats_the_key() {
    // The answer is discarded unread (it repeats the key), but Tavily did answer 432.
    let k = key("echoed");
    let server = FakeServer::start(|r| {
        let auth = r.header("authorization").unwrap_or("").to_string();
        Reply::json(432, &json!({"detail": {"error": auth}}).to_string())
    })
    .await;
    let rec = Recorder::default();
    let (client, _) = keyed(&server, &Script::keys(&[&k]), Arc::new(rec.clone()));
    let err = client.search(&search("q")).await.unwrap_err();
    assert!(err.message.contains("repeats the key"), "{}", err.message);
    assert_eq!(rec.events(), [Event::PlanLimit]);
}

#[tokio::test]
async fn the_age_is_reported_when_the_key_is_read_from_the_store_and_not_while_it_is_cached() {
    let server = FakeServer::start(|_| ok_answer()).await;
    let rec = Recorder::default();
    let (t1, t2) = (NOW_MS - 10 * DAY_MS, NOW_MS - DAY_MS);
    let script = Script::dated(&[(&key("a"), t1), (&key("a"), t2)]);
    let (client, clock) = keyed(&server, &script, Arc::new(rec.clone()));
    client.search(&search("one")).await.unwrap();
    client.search(&search("two")).await.unwrap();
    clock.advance_s(299);
    client.search(&search("three")).await.unwrap();
    assert_eq!(
        rec.events(),
        [Event::KeyLoaded { stored_at_ms: t1 }],
        "one read in the first five minutes"
    );
    clock.advance_s(2);
    client.search(&search("four")).await.unwrap();
    assert_eq!(
        rec.events(),
        [
            Event::KeyLoaded { stored_at_ms: t1 },
            Event::KeyLoaded { stored_at_ms: t2 }
        ],
        "the next read after the cache expired"
    );
    assert_eq!(script.reads(), 2);
}

#[tokio::test]
async fn nothing_is_reported_without_a_stored_key_or_a_version_time_or_a_store() {
    let server = FakeServer::start(|_| ok_answer()).await;
    // No key stored: the call fails as `no_key` and sends nothing.
    let rec = Recorder::default();
    let (client, _) = keyed(
        &server,
        &Script::with(vec![Ok(None)]),
        Arc::new(rec.clone()),
    );
    assert_eq!(
        client.search(&search("q")).await.unwrap_err().code,
        C::NoKey
    );
    assert_eq!((server.count(), rec.events()), (0, vec![]));
    // The store is down.
    let (client, _) = keyed(
        &server,
        &Script::with(vec![Err(research_api::ports::Fail::upstream("down"))]),
        Arc::new(rec.clone()),
    );
    assert!(client.search(&search("q")).await.is_err());
    assert!(rec.events().is_empty());
    // A key whose version time the store did not give: the call works, no age is reported.
    let (client, _) = keyed(&server, &Script::keys(&[&key("a")]), Arc::new(rec.clone()));
    client.search(&search("q")).await.unwrap();
    assert!(rec.events().is_empty());
}

#[tokio::test]
async fn a_keyless_client_reports_nothing_whatever_tavily_answers() {
    for status in [401u16, 403, 429, 432, 433] {
        let server = FakeServer::start(status_only(status)).await;
        let rec = Recorder::default();
        let client: TavilyClient<Script, Arc<FakeClock>> =
            TavilyClient::keyless(http_for(server.addr), "https://api.tavily.com".into())
                .with_metrics(Arc::new(rec.clone()));
        assert!(client.search(&search("q")).await.is_err(), "{status}");
        assert!(rec.events().is_empty(), "{status}: {:?}", rec.events());
    }
}

#[tokio::test]
async fn a_client_without_a_sink_works_as_before() {
    let server = FakeServer::start(status_only(432)).await;
    let secret =
        CachedSecret::with_clock(Script::keys(&[&key("a")]), Arc::new(FakeClock::default()));
    let client = TavilyClient::keyed(
        http_for(server.addr),
        "https://api.tavily.com".into(),
        secret,
    );
    assert_eq!(
        client.search(&search("q")).await.unwrap_err().code,
        C::UpstreamError
    );
}

// ---- the cache hands the version time on once -----------------------------------------------------------------------------------------

#[tokio::test]
async fn the_cache_hands_on_the_version_time_of_each_read_once() {
    let (t1, t2, t3) = (NOW_MS - 3 * DAY_MS, NOW_MS - 2 * DAY_MS, NOW_MS - DAY_MS);
    let script = Script::dated(&[(&key("a"), t1), (&key("b"), t2), (&key("c"), t3)]);
    let clock = Arc::new(FakeClock::default());
    let secret = CachedSecret::with_clock(script.clone(), clock.clone());
    assert_eq!(secret.take_load().await, None, "nothing was read yet");
    secret.get().await.unwrap();
    assert_eq!(secret.take_load().await, Some(t1));
    assert_eq!(secret.take_load().await, None, "handed on once");
    secret.get().await.unwrap();
    assert_eq!(
        secret.take_load().await,
        None,
        "a cached key is no new load"
    );
    clock.advance_s(301);
    secret.get().await.unwrap();
    assert_eq!(secret.take_load().await, Some(t2), "a refresh is a load");
    assert!(secret.reread_after_auth_failure().await.unwrap());
    assert_eq!(
        secret.take_load().await,
        Some(t3),
        "so is the re-read after a refusal"
    );
}

#[tokio::test]
async fn the_cache_hands_on_nothing_for_no_key_an_undated_key_or_a_failed_read() {
    let clock = Arc::new(FakeClock::default());
    let none = CachedSecret::with_clock(Script::with(vec![Ok(None)]), clock.clone());
    none.get().await.unwrap();
    assert_eq!(none.take_load().await, None);
    let undated = CachedSecret::with_clock(Script::keys(&[&key("a")]), clock.clone());
    undated.get().await.unwrap();
    assert_eq!(undated.take_load().await, None);
    let down = CachedSecret::with_clock(
        Script::with(vec![Err(research_api::ports::Fail::upstream("down"))]),
        clock,
    );
    assert!(down.get().await.is_err());
    assert_eq!(down.take_load().await, None);
}

// ---- no line holds the key ----------------------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn no_line_holds_the_key_whatever_tavily_answers(
        tag in "[a-z0-9]{1,12}",
        statuses in proptest::collection::vec(
            prop_oneof![Just(200u16), Just(401u16), Just(403u16), Just(429u16), Just(432u16), Just(433u16), Just(500u16)],
            1..6,
        ),
        age_days in 0u64..400,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let secret_key = key(&tag);
            let statuses = Mutex::new(statuses.into_iter().cycle());
            let server = FakeServer::start(move |r| {
                // Every failing answer repeats what it was sent, the key included.
                let status = statuses.lock().unwrap().next().unwrap();
                let echo = r.header("authorization").unwrap_or("").to_string();
                if status == 200 {
                    ok_answer()
                } else {
                    Reply::json(status, &json!({"detail": {"error": echo}}).to_string())
                }
            })
            .await;
            let buf = Buf::default();
            let wall = Wall::at(NOW_MS);
            let sink: Arc<dyn Metrics> = Arc::new(emf(&buf, &wall));
            let script = Script::dated(&[(&secret_key, NOW_MS - age_days * DAY_MS)]);
            let (client, _) = keyed(&server, &script, sink);
            for i in 0..5 {
                let _ = client.search(&search(&format!("q{i}"))).await;
            }
            let text = buf.text();
            // The first read of the key writes its age, so there is a line to look at.
            prop_assert!(text.lines().count() >= 1, "no line was written");
            for line in text.lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                prop_assert!(v["_aws"]["CloudWatchMetrics"].is_array());
            }
            prop_assert!(!text.contains(&secret_key), "the key is in a line: {}", text);
            for banned in ["Bearer", "authorization", "api.tavily.com", "127.0.0.1", "detail"] {
                prop_assert!(!text.contains(banned), "{} is in a line: {}", banned, text);
            }
            Ok::<(), TestCaseError>(())
        })?;
    }
}
