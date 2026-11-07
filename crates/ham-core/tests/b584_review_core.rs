//! The fixes after the independent review, the pure parts: the run index route is no run, a POST that only fetches an address
//! changes nothing, how an HTTP answer is judged, the two backend settings are security rows, and the words for a backend without a version.
#![allow(clippy::unwrap_used)]

use ham_core::backends::{
    Answer, BackendInfo, BackendView, Health, Local, Route, changes_something, judge_answer,
    learns_from, route_of,
};
use ham_core::registry::{SECURITY_KEYS, is_security_key};
use ham_core::settings::ROWS;

#[test]
fn b584_the_run_index_backfill_is_no_run_called_index() {
    assert_eq!(route_of("POST", "/v1/runs/index"), Route::Other);
    assert_eq!(
        route_of("POST", "/v1/runs/index?nextToken=abc%3D"),
        Route::Other
    );
    assert_eq!(route_of("GET", "/v1/runs/index"), Route::Other);
    // A real run id and the routes below it stay runs.
    assert_eq!(
        route_of("GET", "/v1/runs/01K6A0000000000000000000A1"),
        Route::Run("01K6A0000000000000000000A1")
    );
    assert_eq!(
        route_of("POST", "/v1/runs/index2/cancel"),
        Route::Run("index2")
    );
}

#[test]
fn b584_a_post_that_only_fetches_an_address_changes_nothing() {
    for (m, p) in [
        ("GET", "/v1/runs/R1"),
        ("get", "/v1/whoami"),
        ("HEAD", "/v1/runs/R1"),
        ("POST", "/v1/runs/R1/artifacts/zip/download-url"),
        ("POST", "/v1/runs/R1/snapshot/download-url"),
        ("POST", "/v1/runs/R1/snapshot/download-url?x=1"),
    ] {
        assert!(!changes_something(m, p), "{m} {p}");
    }
    // Exactly the two shapes; any other route, a longer path, another method and an unknown method change something.
    for (m, p) in [
        ("POST", "/v1/direct/runs"),
        ("POST", "/v1/runs/R1/cancel"),
        ("POST", "/v1/runs/R1/input"),
        ("POST", "/v1/runs/R1/messages"),
        ("POST", "/v1/runs/R1/retry"),
        ("POST", "/v1/runs/R1/resume"),
        ("POST", "/v1/tags/edit"),
        ("POST", "/v1/admin/repos"),
        ("POST", "/v1/runs/index"),
        ("DELETE", "/v1/runs/R1/snapshot/download-url"),
        ("PUT", "/v1/runs/R1/artifacts/zip/download-url"),
        ("POST", "/v1/runs/R1/download-url"),
        ("POST", "/v1/runs/R1/other/download-url"),
        ("POST", "/v1/runs/R1/snapshot/download-url/extra"),
        ("POST", "/v1/runs/R1/artifacts/zip/other/download-url"),
        ("POST", "/v1/runs/R1/artifacts//download-url"),
        ("FETCH", "/v1/runs/R1"),
    ] {
        assert!(changes_something(m, p), "{m} {p}");
    }
}

#[test]
fn b584_an_http_answer_is_judged_by_its_status_and_by_who_sent_it() {
    let gateway = br#"{"message":"User: arn:aws:sts::1:assumed-role/X/y is not authorized to perform: execute-api:Invoke"}"#;
    // Fine: reads that found something, a plain not-found, a conflict, a validation error, and the service's own refusal of a caller.
    for (status, service) in [
        (200, true),
        (202, true),
        (204, false),
        (400, true),
        (404, true),
        (404, false),
        (409, true),
        (422, true),
        (403, true),
        (401, true),
    ] {
        assert_eq!(
            judge_answer(status, service, b"{}"),
            Answer::Fine,
            "{status} {service}"
        );
    }
    // A lapsed sign-in is told apart from the other gateway refusals by its words.
    assert_eq!(
        judge_answer(
            403,
            false,
            br#"{"message":"The security token included in the request is expired"}"#
        ),
        Answer::SignInLapsed
    );
    // The word in the body of the service's own answer says nothing about the sign-in.
    assert_eq!(
        judge_answer(403, true, br#"{"message":"the grant expired"}"#),
        Answer::Fine
    );
    // A refusal at the gateway (no version header): wrong profile, unauthorised role.
    for status in [401, 403] {
        match judge_answer(status, false, gateway) {
            Answer::Refused(text) => {
                assert!(text.starts_with(&format!("HTTP {status}")), "{text}");
                assert!(
                    text.contains("not authorized to perform: execute-api:Invoke"),
                    "{text}"
                );
                assert!(text.contains("profile"), "it says what to check: {text}");
            }
            other => panic!("{status}: {other:?}"),
        }
    }
    // A throttle or a timeout: the backend is slow, its rows may be old.
    for (status, service) in [
        (429, false),
        (429, true),
        (408, false),
        (504, false),
        (504, true),
    ] {
        assert_eq!(
            judge_answer(status, service, b"{}"),
            Answer::Slow,
            "{status} {service}"
        );
    }
    // A server failure, from the gateway or from the service.
    for (status, service) in [
        (500, true),
        (500, false),
        (502, false),
        (503, false),
        (503, true),
    ] {
        match judge_answer(status, service, gateway) {
            Answer::Failed(text) => assert!(text.starts_with(&format!("HTTP {status}")), "{text}"),
            other => panic!("{status} {service}: {other:?}"),
        }
    }
    // The text is one bounded, clean line: both spellings of the message member, a plain body, controls and newlines, a long body, no body.
    let Answer::Failed(t) = judge_answer(502, false, br#"{"Message":"Bad gateway"}"#) else {
        panic!()
    };
    assert_eq!(t, "HTTP 502: Bad gateway");
    let Answer::Failed(t) = judge_answer(
        503,
        false,
        b"<html>\n<body>Service\x07 Unavailable</body></html>",
    ) else {
        panic!()
    };
    assert!(!t.contains('\n') && !t.contains('\x07'), "{t:?}");
    let long = "x".repeat(500);
    let Answer::Failed(t) = judge_answer(500, true, long.as_bytes()) else {
        panic!()
    };
    assert!(t.len() < 260, "{}", t.len());
    let Answer::Failed(t) = judge_answer(502, false, b"") else {
        panic!()
    };
    assert_eq!(t, "HTTP 502");
}

#[test]
fn b584_a_refused_backend_gets_its_own_banner_that_does_not_say_not_reachable() {
    let view = BackendView {
        backends: vec![BackendInfo {
            alias: "spare".into(),
            health: Health::Refused("HTTP 403: not authorized; check the profile".into()),
            version: None,
            stale: false,
            customer_content: false,
            profile: Some("spare".into()),
        }],
    };
    assert_eq!(
        view.banners(),
        ["spare: access refused: HTTP 403: not authorized; check the profile"]
    );
}

#[test]
fn b584_the_two_backend_settings_decide_where_requests_go_so_they_are_security_rows() {
    assert_eq!(
        SECURITY_KEYS,
        [
            "api_endpoint",
            "aws_profile",
            "region",
            "download_dir",
            "default_backend",
            "customer_content"
        ]
    );
    for name in [
        "default_backend",
        "customer_content",
        "defaultBackend",
        "customerContent",
    ] {
        assert!(is_security_key(name), "{name}");
    }
    let flagged: std::collections::BTreeSet<&str> =
        ROWS.iter().filter(|r| r.security).map(|r| r.name).collect();
    let want: std::collections::BTreeSet<&str> = SECURITY_KEYS.iter().copied().collect();
    assert_eq!(
        flagged, want,
        "the registry and the list of the app name the same keys"
    );
}

#[test]
fn b584_the_words_for_a_backend_without_a_version_and_for_a_follow_up_aimed_elsewhere() {
    let text = Local::Untagged {
        alias: "old".into(),
        min: "5.00".into(),
    }
    .to_string();
    assert!(
        text.contains("old")
            && text.contains("no version")
            && text.contains("v5.00")
            && text.contains("read-only"),
        "{text}"
    );
    let text = Local::ContinueElsewhere {
        run_id: "R1".into(),
        owner: "work".into(),
        named: "spare".into(),
    }
    .to_string();
    assert!(
        text.contains("R1") && text.contains("work") && text.contains("spare"),
        "{text}"
    );
}

#[test]
fn b584_only_a_list_and_a_post_about_a_run_teach_the_run_map() {
    assert!(learns_from(Route::List, "GET"));
    assert!(learns_from(Route::Start, "POST"));
    assert!(learns_from(Route::Run("R1"), "POST"));
    assert!(learns_from(Route::Run("R1"), "post"));
    // Events and transcript pages are polled every tick and teach nothing, so their bodies are not read for it.
    assert!(!learns_from(Route::Run("R1"), "GET"));
    assert!(!learns_from(Route::Start, "GET"));
    assert!(!learns_from(Route::Tags, "POST"));
    assert!(!learns_from(Route::Other, "GET"));
    assert!(!learns_from(Route::Other, "POST"));
}
