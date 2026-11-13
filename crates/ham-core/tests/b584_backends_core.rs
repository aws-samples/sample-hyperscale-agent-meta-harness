//! Steps 2 to 8, the pure parts: routes, the run id map, the probe decision, the merge of list pages (with its watermark), the health view
//! and its banners, the version rule, the title tag, the customer-content check and the idempotency key.
#![allow(clippy::unwrap_used)]

use ham_core::backends::{
    BackendDef, BackendInfo, BackendView, Decision, Health, Local, Lookup, Page, Probe, Route,
    RunMap, Support, TitleVersion, customer_content_refusal, decide, merge_all, merge_first,
    route_of, support, title_version,
};

fn def(alias: &str, cc: bool) -> BackendDef {
    BackendDef {
        alias: alias.into(),
        api_endpoint: Some(format!("https://{alias}.example/direct")),
        region: None,
        aws_profile: Some(alias.into()),
        customer_content: cc,
    }
}

// ---- routes

#[test]
fn b584_routes_name_what_a_call_is_about() {
    assert_eq!(route_of("GET", "/v1/runs"), Route::List);
    assert_eq!(route_of("GET", "/v1/runs?nextToken=abc%3D"), Route::List);
    assert_eq!(
        route_of("GET", "/v1/runs?q=01K6A&since=2026-11-04T00:00:00Z"),
        Route::List
    );
    assert_eq!(
        route_of("GET", "/v1/runs/01K6A0000000000000000000A1"),
        Route::Run("01K6A0000000000000000000A1")
    );
    assert_eq!(
        route_of("GET", "/v1/runs/01K6A0000000000000000000A1/events?shard=3"),
        Route::Run("01K6A0000000000000000000A1")
    );
    assert_eq!(
        route_of("POST", "/v1/runs/01K6A0000000000000000000A1/cancel"),
        Route::Run("01K6A0000000000000000000A1")
    );
    assert_eq!(route_of("POST", "/v1/direct/runs"), Route::Start);
    assert_eq!(route_of("POST", "/v1/tags/edit"), Route::Tags);
    for p in ["/v1/whoami", "/v1/models", "/v1/repos", "/v1/search?q=x"] {
        assert_eq!(route_of("GET", p), Route::Other, "{p}");
    }
    assert_eq!(route_of("POST", "/v1/repos/admin"), Route::Other);
}

// ---- the run id map

#[test]
fn b584_the_map_remembers_the_backend_of_a_run_and_refuses_an_id_in_two() {
    let mut m = RunMap::default();
    assert_eq!(m.lookup("R1"), Lookup::Unknown);
    m.learn("work", "R1");
    m.learn("work", "R1");
    assert_eq!(m.lookup("R1"), Lookup::One("work".into()));
    m.learn("spare", "R1");
    assert_eq!(
        m.lookup("R1"),
        Lookup::Ambiguous(vec!["work".into(), "spare".into()])
    );
    m.learn("work", "R2");
    assert_eq!(m.backend_of("R2"), Some("work"));
    assert_eq!(
        m.backend_of("R1"),
        None,
        "an id in two backends has no owner"
    );
    assert_eq!(m.len(), 2);
}

// ---- the decision after asking every backend about an unknown id

#[test]
fn b584_a_probe_finds_one_owner_or_says_why_it_cannot() {
    let p = |a: &str, x: Probe| (a.to_string(), x);
    let cant = |s: &str| Probe::CannotTell(s.to_string());
    // one has it, the other one says no: routed there
    assert_eq!(
        decide(&[p("a", Probe::NotHere), p("b", Probe::Has)]),
        Decision::Owner("b".into())
    );
    // one has it, another could not answer: run ids are unique, so the owner stands
    assert_eq!(
        decide(&[p("a", cant("down")), p("b", Probe::Has)]),
        Decision::Owner("b".into())
    );
    // two have it: refused
    assert_eq!(
        decide(&[p("a", Probe::Has), p("b", Probe::Has)]),
        Decision::Ambiguous(vec!["a".into(), "b".into()])
    );
    // nobody has it and everybody answered
    assert_eq!(
        decide(&[p("a", Probe::NotHere), p("b", Probe::NotHere)]),
        Decision::NotFound
    );
    // nobody has it and one could not answer: never "no such run"
    assert_eq!(
        decide(&[p("a", Probe::NotHere), p("b", cant("sign in again"))]),
        Decision::CannotTell {
            checked: vec!["a".into()],
            unresolved: vec![("b".into(), "sign in again".into())],
        }
    );
}

// ---- the merge of list pages

fn page(alias: &str, rows: &[(&str, &str)], more: bool) -> Page<String> {
    Page {
        alias: alias.into(),
        rows: rows
            .iter()
            .map(|(t, id)| ((*t).to_string(), (*id).to_string(), format!("{alias}:{id}")))
            .collect(),
        more,
    }
}

#[test]
fn b584_complete_pages_merge_newest_first_with_the_run_id_as_the_tie_breaker() {
    let a = page(
        "a",
        &[
            ("2026-11-04T10:00:03Z", "A3"),
            ("2026-11-04T10:00:01Z", "A1"),
        ],
        false,
    );
    let b = page(
        "b",
        &[
            ("2026-11-04T10:00:03Z", "B3"),
            ("2026-11-04T10:00:02Z", "B2"),
        ],
        false,
    );
    let m = merge_first(vec![a, b]);
    let ids: Vec<&str> = m.rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["B3", "A3", "B2", "A1"]);
    assert_eq!(m.rows[0].backend, "b");
    assert!(!m.has_more);
    assert_eq!(m.held, 0);
    assert_eq!(m.watermark, None);
}

#[test]
fn b584_a_short_page_never_looks_complete_when_a_backend_has_more() {
    // a has more runs behind its page and its page reaches back to :05; b is complete. Everything of b is older than :05 is fine, but a's
    // missing rows could be anywhere older than :05, so only rows newer than :05 are final.
    let a = page(
        "a",
        &[
            ("2026-11-04T10:00:09Z", "A9"),
            ("2026-11-04T10:00:05Z", "A5"),
        ],
        true,
    );
    let b = page(
        "b",
        &[
            ("2026-11-04T10:00:07Z", "B7"),
            ("2026-11-04T10:00:04Z", "B4"),
        ],
        false,
    );
    let m = merge_first(vec![a, b]);
    let ids: Vec<&str> = m.rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(
        ids,
        ["A9", "B7"],
        "rows at or before the watermark second are held back"
    );
    assert_eq!(m.watermark.as_deref(), Some("2026-11-04T10:00:05"));
    assert_eq!(m.held, 2);
    assert!(m.has_more, "held rows mean there is more");
}

#[test]
fn b584_with_two_backends_that_have_more_the_newer_frontier_decides() {
    let a = page(
        "a",
        &[
            ("2026-11-04T10:00:09Z", "A9"),
            ("2026-11-04T09:00:00Z", "A0"),
        ],
        true,
    );
    let b = page(
        "b",
        &[
            ("2026-11-04T10:00:08Z", "B8"),
            ("2026-11-04T10:00:06Z", "B6"),
        ],
        true,
    );
    let m = merge_first(vec![a, b]);
    let ids: Vec<&str> = m.rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["A9", "B8"]);
    assert_eq!(m.watermark.as_deref(), Some("2026-11-04T10:00:06"));
}

#[test]
fn b584_a_second_shared_by_the_frontier_is_held_because_the_next_page_may_add_to_it() {
    let a = page(
        "a",
        &[
            ("2026-11-04T10:00:05Z", "A5b"),
            ("2026-11-04T10:00:05Z", "A5a"),
        ],
        true,
    );
    let b = page("b", &[("2026-11-04T10:00:05Z", "B5")], false);
    let m = merge_first(vec![a, b]);
    assert!(m.rows.is_empty());
    assert_eq!(m.held, 3);
    assert!(m.has_more);
}

#[test]
fn b584_an_empty_page_with_more_certifies_nothing() {
    let a = page("a", &[], true);
    let b = page("b", &[("2026-11-04T10:00:07Z", "B7")], false);
    let m = merge_first(vec![a, b]);
    assert!(m.rows.is_empty());
    assert_eq!(m.held, 1);
    assert!(m.has_more);
}

#[test]
fn b584_a_single_page_is_passed_through_untouched() {
    let a = page(
        "a",
        &[
            ("2026-11-04T10:00:09Z", "A9"),
            ("2026-11-04T10:00:05Z", "A5"),
        ],
        true,
    );
    let m = merge_first(vec![a]);
    assert_eq!(m.rows.len(), 2);
    assert_eq!(m.held, 0);
    assert!(m.has_more, "its own next token still says more");
}

#[test]
fn b584_the_whole_read_merges_everything_and_has_no_watermark() {
    let a = page("a", &[("2026-11-04T10:00:09Z", "A9")], false);
    let b = page(
        "b",
        &[
            ("2026-11-04T10:00:10Z", "B10"),
            ("2026-11-04T10:00:01Z", "B1"),
        ],
        false,
    );
    let rows = merge_all(vec![a, b]);
    let ids: Vec<&str> = rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["B10", "A9", "B1"]);
}

#[test]
fn b584_fractions_of_a_second_do_not_split_a_second() {
    let a = page(
        "a",
        &[
            ("2026-11-04T10:00:05.900Z", "A5"),
            ("2026-11-04T10:00:03Z", "A3"),
        ],
        true,
    );
    let b = page("b", &[("2026-11-04T10:00:05Z", "B5")], false);
    let m = merge_first(vec![a, b]);
    assert_eq!(m.watermark.as_deref(), Some("2026-11-04T10:00:03"));
    assert_eq!(m.rows.len(), 2);
}

// ---- health, banners and the title tag

fn info(alias: &str, health: Health, version: Option<&str>) -> BackendInfo {
    BackendInfo {
        alias: alias.into(),
        health,
        version: version.map(str::to_string),
        stale: false,
        customer_content: false,
        profile: Some(alias.into()),
    }
}

#[test]
fn b584_one_banner_for_each_failing_backend_names_the_fix() {
    let view = BackendView {
        backends: vec![
            info("work", Health::Up, Some("5.05")),
            info(
                "spare",
                Health::SignedOut {
                    fix: "aws sso login --profile spare".into(),
                },
                None,
            ),
            info(
                "lab",
                Health::Down("network: connection refused".into()),
                None,
            ),
            info("slow", Health::Slow, Some("5.05")),
        ],
    };
    let lines = view.banners();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(
        lines[0].starts_with("spare: sign in needed")
            && lines[0].contains("aws sso login --profile spare"),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with("lab: not reachable") && lines[1].contains("connection refused"),
        "{lines:?}"
    );
    assert!(
        lines[2].starts_with("slow: did not answer in time"),
        "{lines:?}"
    );
    assert!(view.shows_backend_column());
    let one = BackendView {
        backends: vec![info("default", Health::Up, None)],
    };
    assert!(one.banners().is_empty());
    assert!(
        !one.shows_backend_column(),
        "a backend column needs two backends"
    );
}

#[test]
fn b584_the_title_says_mixed_when_the_known_versions_differ() {
    let v = |a: &[(&str, Option<&str>)]| {
        title_version(
            &a.iter()
                .map(|(n, v)| info(n, Health::Up, *v))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        v(&[("a", Some("5.05")), ("b", Some("5.05"))]),
        TitleVersion::One("5.05".into())
    );
    assert_eq!(
        v(&[("a", Some("5.05")), ("b", Some("5.04"))]),
        TitleVersion::Mixed
    );
    assert_eq!(
        v(&[("a", Some("5.05")), ("b", None)]),
        TitleVersion::One("5.05".into())
    );
    assert_eq!(v(&[("a", None), ("b", None)]), TitleVersion::Unknown);
    assert_eq!(TitleVersion::Mixed.word(), "mixed");
}

// ---- the version rule

#[test]
fn b584_a_backend_below_the_minimum_or_of_another_major_refuses_writes() {
    // client 5.05, minimum 5.03
    assert_eq!(support("5.05", "5.03", Some("5.05")), Support::Full);
    assert_eq!(
        support("5.05", "5.03", Some("5.09")),
        Support::Full,
        "a newer counter is allowed"
    );
    assert_eq!(support("5.05", "5.03", Some("5.03")), Support::Full);
    assert!(matches!(
        support("5.05", "5.03", Some("5.02")),
        Support::ReadOnly { .. }
    ));
    assert!(matches!(
        support("5.05", "5.03", Some("4.99")),
        Support::ReadOnly { .. }
    ));
    assert!(matches!(
        support("5.05", "5.03", Some("6.00")),
        Support::UnknownMajor { .. }
    ));
    assert!(
        matches!(
            support("5.05", "5.03", Some("v5.05")),
            Support::UnknownMajor { .. }
        ),
        "a header that is no version"
    );
    assert_eq!(support("5.05", "5.03", None), Support::Unknown);
    // numbers, not text: 10.01 is newer than 9.99
    assert!(matches!(
        support("10.01", "10.00", Some("9.99")),
        Support::ReadOnly { .. }
    ));
}

// ---- customer content and errors

#[test]
fn b584_a_start_marked_as_customer_content_needs_a_backend_declared_safe() {
    assert!(customer_content_refusal(&def("work", true), true).is_none());
    assert!(customer_content_refusal(&def("work", false), false).is_none());
    assert!(customer_content_refusal(&def("work", true), false).is_none());
    let refused = customer_content_refusal(&def("spare", false), true).expect("refused");
    assert_eq!(
        refused,
        Local::CustomerContent {
            alias: "spare".into()
        }
    );
    let words = refused.to_string();
    assert!(
        words.contains("spare") && words.contains("customerContent"),
        "{words}"
    );
}

#[test]
fn b584_every_local_refusal_names_the_aliases_and_the_way_forward() {
    let aliases = vec!["default".to_string(), "work".to_string()];
    let m = Local::NoBackendChosen {
        aliases: aliases.clone(),
    }
    .to_string();
    assert!(
        m.contains("--backend") && m.contains("default, work") && m.contains("default_backend"),
        "{m}"
    );
    let m = Local::UnknownAlias {
        alias: "nope".into(),
        aliases: aliases.clone(),
    }
    .to_string();
    assert!(m.contains("nope") && m.contains("default, work"), "{m}");
    let m = Local::Ambiguous {
        run_id: "R1".into(),
        aliases: aliases.clone(),
    }
    .to_string();
    assert!(
        m.contains("R1") && m.contains("default, work") && m.contains("--backend"),
        "{m}"
    );
    let m = Local::CannotTell {
        run_id: "R1".into(),
        checked: vec!["default".into()],
        unresolved: vec![(
            "work".into(),
            "sign in needed: aws sso login --profile work".into(),
        )],
    }
    .to_string();
    assert!(
        m.contains("not found on default")
            && m.contains("could not check work")
            && m.contains("aws sso login"),
        "{m}"
    );
    assert!(!m.to_lowercase().contains("no such run"), "{m}");
    let m = Local::ReadOnly {
        alias: "work".into(),
        version: "5.02".into(),
        min: "5.03".into(),
    }
    .to_string();
    assert!(
        m.contains("work") && m.contains("5.02") && m.contains("5.03") && m.contains("read-only"),
        "{m}"
    );
    let m = Local::UnknownMajor {
        alias: "work".into(),
        version: "6.00".into(),
    }
    .to_string();
    assert!(
        m.contains("work") && m.contains("6.00") && m.contains("update"),
        "{m}"
    );
    let m = Local::MixedBackends {
        aliases: aliases.clone(),
    }
    .to_string();
    assert!(m.contains("default, work"), "{m}");
    let m = Local::BackendDown {
        alias: "lab".into(),
        reason: "network: connection refused".into(),
    }
    .to_string();
    assert!(
        m.contains("backend lab") && m.contains("connection refused"),
        "{m}"
    );
}

#[test]
fn b584_small_pieces_of_the_model() {
    use ham_core::backends::{aliases_of, is_alias, is_write};
    let mut m = RunMap::default();
    assert!(m.is_empty());
    assert_eq!(m.len(), 0);
    m.learn("a", "R1");
    assert!(!m.is_empty());
    assert!(!is_write("GET") && !is_write("get") && is_write("POST") && is_write("DELETE"));
    assert!(
        is_alias("work-2_a")
            && !is_alias("")
            && !is_alias("-a")
            && !is_alias("A")
            && !is_alias("a b")
    );
    assert!(
        Support::ReadOnly {
            version: "4.0".into(),
            min: "5.00".into()
        }
        .refuses_writes()
    );
    assert!(
        Support::UnknownMajor {
            version: "9.00".into()
        }
        .refuses_writes()
    );
    assert!(!Support::Full.refuses_writes() && !Support::Unknown.refuses_writes());
    let view = BackendView {
        backends: vec![info("a", Health::Up, Some("5.05"))],
    };
    assert!(view.info("a").is_some() && view.info("b").is_none());
    let cfg = ham_core::config::parse_config("configVersion = 1\n\n[[backend]]\nalias = \"w\"\napiEndpoint = \"https://w.example/direct\"\n").unwrap();
    assert_eq!(aliases_of(&cfg), ["default", "w"]);
    let m = Local::AllFailed {
        reasons: "a: down; b: slow".into(),
    }
    .to_string();
    assert!(
        m.contains("no backend answered") && m.contains("a: down"),
        "{m}"
    );
    assert_eq!(TitleVersion::Unknown.word(), "?.??");
    assert_eq!(TitleVersion::One("5.05".into()).word(), "5.05");
}
