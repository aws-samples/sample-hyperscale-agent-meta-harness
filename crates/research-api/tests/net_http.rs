//! The HTTPS client of the Lambdas (NET lane): header and body handling, no redirects, the response cap, timeouts, and the refusal of
//! every address that is not globally routable. The fake upstreams are scripted servers on 127.0.0.1 reached through the `testing` host
//! table; no test reaches the network.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;

use net_fake::{FakeServer, Reply, closed_addr};
use research_api::http::testing::TestNet;
use research_api::http::{
    HttpClient, HttpError, HttpOptions, HttpRequest, HttpResponse, globally_routable,
};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

fn opts() -> HttpOptions {
    HttpOptions {
        connect_timeout: Duration::from_secs(2),
        total_timeout: Duration::from_secs(5),
        max_response_bytes: 1024 * 1024,
    }
}

fn test_client(hosts: &[(&str, SocketAddr)], options: HttpOptions) -> HttpClient {
    let table: BTreeMap<String, SocketAddr> =
        hosts.iter().map(|(h, a)| (h.to_string(), *a)).collect();
    HttpClient::with_test_net(options, TestNet { hosts: table }).unwrap()
}

fn get(url: &str) -> HttpRequest {
    HttpRequest {
        method: http::Method::GET,
        url: url.to_string(),
        headers: vec![],
        body: vec![],
    }
}

#[tokio::test]
async fn a_post_reaches_the_mapped_server_and_the_answer_comes_back_with_lower_case_header_names() {
    let server = FakeServer::start(|_| {
        Reply::bytes(
            200,
            &[
                ("X-Reply", "Yes"),
                ("Retry-After", "7"),
                ("Content-Type", "application/json"),
            ],
            br#"{"ok":true}"#,
        )
    })
    .await;
    let client = test_client(&[("api.tavily.com", server.addr)], opts());
    let resp = client
        .send(HttpRequest {
            method: http::Method::POST,
            url: "https://api.tavily.com/search?x=1".into(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("X-Test".into(), "1".into()),
            ],
            body: br#"{"query":"q"}"#.to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, br#"{"ok":true}"#);
    let h = |n: &str| {
        resp.headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(h("x-reply"), Some("Yes"));
    assert_eq!(h("retry-after"), Some("7"));
    assert!(
        resp.headers
            .iter()
            .all(|(k, _)| k.chars().all(|c| !c.is_ascii_uppercase()))
    );
    let seen = &server.requests()[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.target, "/search?x=1");
    assert_eq!(seen.header("host"), Some("api.tavily.com"));
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("x-test"), Some("1"));
    assert_eq!(seen.body, br#"{"query":"q"}"#);
    // No cookie, no credential the caller did not set.
    assert!(seen.header("authorization").is_none() && seen.header("cookie").is_none());
}

#[tokio::test]
async fn a_redirect_is_returned_and_never_followed() {
    let target = FakeServer::start(|_| Reply::bytes(200, &[], b"landed")).await;
    let first = FakeServer::start(|_| {
        Reply::bytes(
            302,
            &[("Location", "https://codeload.github.com/o/r/tar.gz/HEAD")],
            b"",
        )
    })
    .await;
    let client = test_client(
        &[
            ("github.com", first.addr),
            ("codeload.github.com", target.addr),
        ],
        opts(),
    );
    let resp = client
        .send(get("https://github.com/o/r/archive/HEAD.tar.gz"))
        .await
        .unwrap();
    assert_eq!(resp.status, 302);
    let loc = resp.headers.iter().find(|(k, _)| k == "location").unwrap();
    assert_eq!(loc.1, "https://codeload.github.com/o/r/tar.gz/HEAD");
    assert_eq!(target.count(), 0, "the client did not follow the redirect");
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_refused_without_reading_the_body() {
    let server =
        FakeServer::start(|_| Reply::bytes(200, &[("Content-Length", "5000")], &[b'a'; 100])).await;
    let mut o = opts();
    o.max_response_bytes = 1000;
    let client = test_client(&[("example.org", server.addr)], o);
    let started = Instant::now();
    let err = client
        .send(get("https://example.org/big"))
        .await
        .unwrap_err();
    assert_eq!(err, HttpError::TooLarge);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "it did not wait for the rest"
    );
}

#[tokio::test]
async fn an_endless_body_stops_at_the_limit() {
    let server = FakeServer::start(|_| Reply::Endless {
        status: 200,
        chunk: 4096,
    })
    .await;
    let mut o = opts();
    o.max_response_bytes = 20_000;
    let client = test_client(&[("example.org", server.addr)], o);
    let started = Instant::now();
    let err = client
        .send(get("https://example.org/stream"))
        .await
        .unwrap_err();
    assert_eq!(err, HttpError::TooLarge);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn a_body_exactly_at_the_limit_passes() {
    let server = FakeServer::start(|_| Reply::bytes(200, &[], &[b'z'; 1000])).await;
    let mut o = opts();
    o.max_response_bytes = 1000;
    let client = test_client(&[("example.org", server.addr)], o);
    let resp = client.send(get("https://example.org/")).await.unwrap();
    assert_eq!(resp.body.len(), 1000);
}

#[tokio::test]
async fn send_limited_overrides_the_limit_and_the_timeout_for_one_call() {
    let server = FakeServer::start(|_| Reply::bytes(200, &[], &[b'z'; 3000])).await;
    let client = test_client(&[("example.org", server.addr)], opts());
    let err = client
        .send_limited(get("https://example.org/"), 2000, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(err, HttpError::TooLarge);
    let ok = client
        .send_limited(get("https://example.org/"), 4000, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(ok.body.len(), 3000);
}

#[tokio::test]
async fn a_slow_answer_times_out_and_a_closed_connection_is_a_protocol_error() {
    let slow = FakeServer::start(|_| {
        Reply::Delayed(
            Duration::from_secs(3),
            Box::new(Reply::bytes(200, &[], b"late")),
        )
    })
    .await;
    let mut o = opts();
    o.total_timeout = Duration::from_millis(300);
    let client = test_client(&[("slow.example", slow.addr)], o);
    let started = Instant::now();
    assert_eq!(
        client.send(get("https://slow.example/")).await.unwrap_err(),
        HttpError::Timeout
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    let closing = FakeServer::start(|_| Reply::Close).await;
    let client = test_client(&[("close.example", closing.addr)], opts());
    let err = client
        .send(get("https://close.example/"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, HttpError::Reset | HttpError::Protocol),
        "a connection closed before an answer: {err:?}"
    );
}

#[tokio::test]
async fn a_refused_connection_and_an_unknown_name_are_distinct_errors() {
    let dead = closed_addr().await;
    let client = test_client(&[("dead.example", dead)], opts());
    assert_eq!(
        client.send(get("https://dead.example/")).await.unwrap_err(),
        HttpError::Connect
    );
    assert_eq!(
        client
            .send(get("https://unknown.example/"))
            .await
            .unwrap_err(),
        HttpError::Dns
    );
}

#[tokio::test]
async fn the_production_client_refuses_other_schemes_and_malformed_requests() {
    let client = HttpClient::new(opts()).unwrap();
    for url in [
        "http://example.org/",
        "ftp://example.org/",
        "example.org/",
        "",
        "https://",
        "not a url",
    ] {
        let err = client.send(get(url)).await.unwrap_err();
        assert!(
            matches!(err, HttpError::InvalidRequest(_)),
            "{url:?}: {err:?}"
        );
    }
    let bad_header = HttpRequest {
        headers: vec![("bad name".into(), "v".into())],
        ..get("https://example.org/")
    };
    assert!(matches!(
        client.send(bad_header).await.unwrap_err(),
        HttpError::InvalidRequest(_)
    ));
    let bad_value = HttpRequest {
        headers: vec![("x-a".into(), "line\nbreak".into())],
        ..get("https://example.org/")
    };
    assert!(matches!(
        client.send(bad_value).await.unwrap_err(),
        HttpError::InvalidRequest(_)
    ));
}

#[tokio::test]
async fn the_production_client_never_connects_to_an_address_that_is_not_globally_routable() {
    let client = HttpClient::new(opts()).unwrap();
    // IP literals are judged before any connection; a name that resolves to loopback is judged after resolution.
    for url in [
        "https://127.0.0.1:9/",
        "https://10.1.2.3/",
        "https://[::1]:9/",
        "https://localhost:9/",
    ] {
        let err = client.send(get(url)).await.unwrap_err();
        assert_eq!(err, HttpError::Blocked, "{url}");
    }
}

#[test]
fn only_globally_routable_addresses_are_allowed() {
    let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
    for ok in [
        "8.8.8.8",
        "1.1.1.1",
        "52.216.1.1",
        "140.82.112.3",
        "172.15.255.255",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "198.17.255.255",
        "198.20.0.1",
        "2606:4700:4700::1111",
        "2600:1f18::1",
        "2a00:1450:4001::1",
        "::ffff:8.8.8.8",
    ] {
        assert!(globally_routable(ip(ok)), "{ok}");
    }
    for bad in [
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "10.255.255.255",
        "100.64.0.1",
        "100.127.255.255",
        "127.0.0.1",
        "127.255.255.254",
        "169.254.0.1",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.8",
        "192.0.2.1",
        "192.168.1.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.7",
        "203.0.113.9",
        "224.0.0.1",
        "239.255.255.255",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "fe80::1",
        "fc00::1",
        "fd12:3456::1",
        "ff02::1",
        "2001:db8::1",
        "2001::1",
        "2002:808:808::1",
        "64:ff9b::808:808",
        "100::1",
        "::ffff:10.0.0.1",
    ] {
        assert!(!globally_routable(ip(bad)), "{bad}");
    }
}

#[test]
fn errors_name_a_class_and_never_a_url_or_header() {
    for (e, class) in [
        (HttpError::Blocked, "blocked"),
        (HttpError::TooLarge, "too_large"),
        (HttpError::Timeout, "timeout"),
        (HttpError::Dns, "dns"),
        (HttpError::Tls, "tls"),
        (HttpError::Connect, "connect"),
        (HttpError::Reset, "reset"),
        (HttpError::Protocol, "protocol"),
        (HttpError::InvalidRequest("bad header"), "invalid_request"),
    ] {
        assert_eq!(e.class(), class);
        assert!(e.to_string().contains(class));
    }
}

#[test]
fn debug_output_never_shows_a_header_value_a_query_or_a_body_byte() {
    let header_value = ["Bearer ", "stand-in-credential-0123456789"].concat();
    let other_value = "stand-in-other-header-value";
    let body_marker = "stand-in-body-marker-0123456789";
    let query_marker = "stand-in-query-marker";
    let request = HttpRequest {
        method: http::Method::POST,
        url: format!("https://api.tavily.com/search?q={query_marker}"),
        headers: vec![
            ("Authorization".into(), header_value.clone()),
            ("X-Other".into(), other_value.into()),
        ],
        body: body_marker.as_bytes().to_vec(),
    };
    let response = HttpResponse {
        status: 200,
        headers: vec![
            ("set-cookie".into(), other_value.into()),
            ("x-echo".into(), header_value.clone()),
        ],
        body: body_marker.as_bytes().to_vec(),
    };
    for shown in [
        format!("{request:?}"),
        format!("{request:#?}"),
        format!("{response:?}"),
        format!("{response:#?}"),
    ] {
        for hidden in [
            header_value.as_str(),
            "stand-in-credential",
            other_value,
            body_marker,
            query_marker,
        ] {
            assert!(!shown.contains(hidden), "{hidden} is in: {shown}");
        }
        assert!(
            shown.contains(&body_marker.len().to_string()),
            "the body length is shown: {shown}"
        );
    }
    // Names and the host are enough to read a log line.
    let shown = format!("{request:?}");
    assert!(
        shown.contains("Authorization")
            && shown.contains("X-Other")
            && shown.contains("api.tavily.com"),
        "{shown}"
    );
    assert!(shown.contains("POST"), "{shown}");
    let shown = format!("{response:?}");
    assert!(
        shown.contains("set-cookie") && shown.contains("200"),
        "{shown}"
    );
    // The comparison of responses is unchanged.
    assert_eq!(response.clone(), response);
}
