//! The GitHub archive source: the archive URL, the one allowed redirect, the size cap and the status mapping.
//! The fakes are scripted servers on 127.0.0.1 behind the `testing` host table; no test reaches the network.

#![allow(clippy::unwrap_used)]

#[path = "support/net_fake.rs"]
mod net_fake;

use harness_protocol::research::ResearchErrorCode as C;
use net_fake::{FakeServer, Reply};
use research_api::github_source::HttpArchiveSource;
use research_api::http::testing::TestNet;
use research_api::http::{HttpClient, HttpOptions};
use research_api::ports::{ArchiveSource, GithubRepo};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

const LIMIT: u64 = 1_000_000;

fn repo() -> GithubRepo {
    GithubRepo {
        owner: "sindresorhus".into(),
        repo: "is-plain-obj".into(),
    }
}

fn source(hosts: &[(&str, SocketAddr)]) -> HttpArchiveSource {
    let table: BTreeMap<String, SocketAddr> =
        hosts.iter().map(|(h, a)| (h.to_string(), *a)).collect();
    let http = HttpClient::with_test_net(
        HttpOptions {
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(5),
            max_response_bytes: 1024,
        },
        TestNet { hosts: table },
    )
    .unwrap();
    HttpArchiveSource::new(http)
}

fn redirect_to(location: &str) -> Reply {
    Reply::bytes(302, &[("Location", location)], b"")
}

async fn pair(archive: Reply) -> (FakeServer, FakeServer) {
    let github = FakeServer::start(|_| {
        redirect_to("https://codeload.github.com/sindresorhus/is-plain-obj/tar.gz/HEAD")
    })
    .await;
    let codeload = FakeServer::start(move |_| archive.clone()).await;
    (github, codeload)
}

#[tokio::test]
async fn the_archive_is_fetched_through_one_redirect_to_codeload_and_comes_back_whole() {
    let payload: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let (github, codeload) = pair(Reply::bytes(
        200,
        &[("Content-Type", "application/x-gzip")],
        &payload,
    ))
    .await;
    let src = source(&[
        ("github.com", github.addr),
        ("codeload.github.com", codeload.addr),
    ]);
    let got = src.download(&repo(), None, LIMIT).await.unwrap();
    assert_eq!(got.bytes, payload);
    assert_eq!(got.final_host, "codeload.github.com");
    let first = &github.requests()[0];
    assert_eq!(
        (first.method.as_str(), first.target.as_str()),
        ("GET", "/sindresorhus/is-plain-obj/archive/HEAD.tar.gz")
    );
    assert_eq!(first.header("host"), Some("github.com"));
    let second = &codeload.requests()[0];
    assert_eq!(
        (second.method.as_str(), second.target.as_str()),
        ("GET", "/sindresorhus/is-plain-obj/tar.gz/HEAD")
    );
    assert_eq!(second.header("host"), Some("codeload.github.com"));
    // No credential or cookie goes to either host.
    for r in github.requests().iter().chain(codeload.requests().iter()) {
        for name in ["authorization", "cookie", "proxy-authorization"] {
            assert!(r.header(name).is_none(), "{name}");
        }
    }
}

#[tokio::test]
async fn a_ref_names_the_archive() {
    let (github, codeload) = pair(Reply::bytes(200, &[], b"x")).await;
    let src = source(&[
        ("github.com", github.addr),
        ("codeload.github.com", codeload.addr),
    ]);
    for (r, path) in [
        ("v1.2.3", "/sindresorhus/is-plain-obj/archive/v1.2.3.tar.gz"),
        (
            "feature/x-y",
            "/sindresorhus/is-plain-obj/archive/feature/x-y.tar.gz",
        ),
        (
            "666df7c10035f7e26f27ec214fe5ae3173435f34",
            "/sindresorhus/is-plain-obj/archive/666df7c10035f7e26f27ec214fe5ae3173435f34.tar.gz",
        ),
    ] {
        src.download(&repo(), Some(r), LIMIT).await.unwrap();
        assert_eq!(github.requests().last().unwrap().target, path);
    }
}

#[tokio::test]
async fn a_redirect_to_any_other_place_is_refused_before_a_second_request() {
    let other = FakeServer::start(|_| Reply::bytes(200, &[], b"should not be fetched")).await;
    for location in [
        "https://example.org/sindresorhus/is-plain-obj.tar.gz",
        "http://codeload.github.com/sindresorhus/is-plain-obj/tar.gz/HEAD",
        "https://codeload.github.com.example.org/x",
        "https://user@codeload.github.com/x",
        "https://codeload.github.com:8443/x",
        "/sindresorhus/is-plain-obj/tar.gz/HEAD",
        "//codeload.github.com/x",
        "ftp://codeload.github.com/x",
        "",
    ] {
        let github = FakeServer::start({
            let location = location.to_string();
            move |_| redirect_to(&location)
        })
        .await;
        let src = source(&[
            ("github.com", github.addr),
            ("codeload.github.com", other.addr),
            ("example.org", other.addr),
        ]);
        let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
        assert_eq!(err.code, C::UpstreamError, "{location:?}");
        assert!(
            err.message.contains("unexpected redirect host"),
            "{location:?}: {}",
            err.message
        );
    }
    assert_eq!(
        other.count(),
        0,
        "nothing was requested from a place other than the first answer's allowed one"
    );
}

#[tokio::test]
async fn a_redirect_without_a_location_is_an_upstream_error() {
    let github = FakeServer::start(|_| Reply::bytes(302, &[], b"")).await;
    let src = source(&[("github.com", github.addr)]);
    let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
}

#[tokio::test]
async fn a_second_redirect_is_not_followed() {
    let (github, codeload) = pair(redirect_to("https://codeload.github.com/again")).await;
    let src = source(&[
        ("github.com", github.addr),
        ("codeload.github.com", codeload.addr),
    ]);
    let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(
        err.message.contains("unexpected redirect host"),
        "{}",
        err.message
    );
    assert_eq!(codeload.count(), 1, "no third request");
}

#[tokio::test]
async fn an_answer_without_a_redirect_from_github_is_refused() {
    let github = FakeServer::start(|_| Reply::bytes(200, &[], b"archive?")).await;
    let src = source(&[("github.com", github.addr)]);
    let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
}

#[tokio::test]
async fn the_size_cap_applies_to_the_archive_whether_declared_or_streamed() {
    let (github, codeload) = pair(Reply::bytes(200, &[], &[b'z'; 5000])).await;
    let src = source(&[
        ("github.com", github.addr),
        ("codeload.github.com", codeload.addr),
    ]);
    let err = src.download(&repo(), None, 4999).await.unwrap_err();
    assert_eq!(err.code, C::TooLarge);
    let ok = src.download(&repo(), None, 5000).await.unwrap();
    assert_eq!(ok.bytes.len(), 5000);

    let endless = FakeServer::start(|_| Reply::Endless {
        status: 200,
        chunk: 8192,
    })
    .await;
    let github2 = FakeServer::start(|_| redirect_to("https://codeload.github.com/x")).await;
    let src = source(&[
        ("github.com", github2.addr),
        ("codeload.github.com", endless.addr),
    ]);
    let err = src.download(&repo(), None, 100_000).await.unwrap_err();
    assert_eq!(err.code, C::TooLarge);
}

#[tokio::test]
async fn statuses_map_to_the_research_error_words() {
    // From github.com.
    for (status, headers, code, retry) in [
        (404u16, vec![], C::NotFound, None),
        (403, vec![], C::UpstreamRateLimited, None),
        (
            429,
            vec![("Retry-After", "12")],
            C::UpstreamRateLimited,
            Some(12),
        ),
        (
            429,
            vec![("Retry-After", "999999")],
            C::UpstreamRateLimited,
            Some(3600),
        ),
        (500, vec![], C::UpstreamError, None),
        (503, vec![], C::UpstreamError, None),
    ] {
        let github = FakeServer::start(move |_| Reply::bytes(status, &headers.clone(), b"")).await;
        let src = source(&[("github.com", github.addr)]);
        let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
        assert_eq!(err.code, code, "github.com {status}");
        assert_eq!(err.retry_after_s, retry, "github.com {status}");
    }
    // From codeload.github.com.
    for (status, code) in [
        (404u16, C::NotFound),
        (403, C::UpstreamRateLimited),
        (429, C::UpstreamRateLimited),
        (502, C::UpstreamError),
    ] {
        let (github, codeload) = pair(Reply::bytes(status, &[], b"")).await;
        let src = source(&[
            ("github.com", github.addr),
            ("codeload.github.com", codeload.addr),
        ]);
        let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
        assert_eq!(err.code, code, "codeload {status}");
    }
}

#[tokio::test]
async fn the_timeout_is_configurable_and_a_dead_server_is_an_upstream_error() {
    let github = FakeServer::start(|_| redirect_to("https://codeload.github.com/x")).await;
    let slow = FakeServer::start(|_| {
        Reply::Delayed(
            Duration::from_secs(3),
            Box::new(Reply::bytes(200, &[], b"late")),
        )
    })
    .await;
    let src = source(&[
        ("github.com", github.addr),
        ("codeload.github.com", slow.addr),
    ])
    .with_timeout(Duration::from_millis(300));
    let err = src.download(&repo(), None, LIMIT).await.unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(err.message.contains("in time"), "{}", err.message);

    let dead = net_fake::closed_addr().await;
    let src = source(&[("github.com", dead)]);
    assert_eq!(
        src.download(&repo(), None, LIMIT).await.unwrap_err().code,
        C::UpstreamError
    );
}
