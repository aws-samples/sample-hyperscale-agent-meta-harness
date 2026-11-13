//! The settings of the research module: every one comes from an `AGENTD_RESEARCH_*` environment variable,
//! with the defaults of the design, and a value that cannot be used keeps its default instead of switching the module off by accident.
#![allow(clippy::unwrap_used)]

use agentd::direct::research::settings::Settings;
use std::collections::BTreeMap;
use std::time::Duration;

fn with(pairs: &[(&str, &str)]) -> Settings {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Settings::from_lookup(&|k| map.get(k).cloned())
}

#[test]
fn the_defaults_are_the_numbers_of_adr_0175() {
    let s = with(&[]);
    assert!(s.enabled);
    assert_eq!(s.port, 3129, "not 3128, the package proxy's port");
    assert_eq!(s.max_page_bytes, 512 * 1024);
    assert_eq!(s.max_archive_bytes, 64 * 1024 * 1024);
    assert_eq!(s.max_unpacked_bytes, 200 * 1024 * 1024);
    assert_eq!(s.max_files, 20_000);
    assert_eq!(s.max_file_bytes, 1024 * 1024);
    assert_eq!(s.max_run_bytes, 300 * 1024 * 1024);
    assert_eq!(s.wait, Duration::from_secs(90));
    assert_eq!(s.max_inflight, 8);
    assert_eq!(s.retries, 3);
}

#[test]
fn each_variable_overrides_its_own_setting() {
    let s = with(&[
        ("AGENTD_RESEARCH_PORT", "4000"),
        ("AGENTD_RESEARCH_MAX_PAGE_BYTES", "1000"),
        ("AGENTD_RESEARCH_MAX_ARCHIVE_BYTES", "2000"),
        ("AGENTD_RESEARCH_MAX_UNPACKED_BYTES", "3000"),
        ("AGENTD_RESEARCH_MAX_FILES", "7"),
        ("AGENTD_RESEARCH_MAX_FILE_BYTES", "4000"),
        ("AGENTD_RESEARCH_MAX_RUN_BYTES", "5000"),
        ("AGENTD_RESEARCH_MIN_FREE_BYTES", "6000"),
        ("AGENTD_RESEARCH_WAIT_S", "5"),
        ("AGENTD_RESEARCH_POLL_MS", "50"),
        ("AGENTD_RESEARCH_RETRIES", "1"),
        ("AGENTD_RESEARCH_RETRY_PAUSE_MS", "20"),
        ("AGENTD_RESEARCH_MAX_INFLIGHT", "2"),
    ]);
    assert_eq!(
        (
            s.port,
            s.max_page_bytes,
            s.max_archive_bytes,
            s.max_unpacked_bytes
        ),
        (4000, 1000, 2000, 3000)
    );
    assert_eq!(
        (
            s.max_files,
            s.max_file_bytes,
            s.max_run_bytes,
            s.min_free_bytes
        ),
        (7, 4000, 5000, 6000)
    );
    assert_eq!(s.wait, Duration::from_secs(5));
    assert_eq!(s.poll_every, Duration::from_millis(50));
    assert_eq!(s.retries, 1);
    assert_eq!(s.retry_pause, Duration::from_millis(20));
    assert_eq!(s.max_inflight, 2);
}

#[test]
fn a_value_that_cannot_be_used_keeps_the_default() {
    let d = with(&[]);
    for bad in ["", "abc", "-5", "1.5", "99999999999999999999999"] {
        let s = with(&[
            ("AGENTD_RESEARCH_MAX_PAGE_BYTES", bad),
            ("AGENTD_RESEARCH_MAX_FILES", bad),
            ("AGENTD_RESEARCH_PORT", bad),
            ("AGENTD_RESEARCH_WAIT_S", bad),
        ]);
        assert_eq!(s, d, "value {bad:?}");
    }
}

#[test]
fn a_cap_of_zero_is_not_a_cap() {
    let d = with(&[]);
    let s = with(&[
        ("AGENTD_RESEARCH_MAX_PAGE_BYTES", "0"),
        ("AGENTD_RESEARCH_MAX_RUN_BYTES", "0"),
        ("AGENTD_RESEARCH_MAX_FILES", "0"),
        ("AGENTD_RESEARCH_MAX_INFLIGHT", "0"),
    ]);
    assert_eq!(s, d);
}

#[test]
fn port_zero_means_any_free_port() {
    assert_eq!(with(&[("AGENTD_RESEARCH_PORT", "0")]).port, 0);
}

#[test]
fn the_module_is_switched_off_by_off_false_or_zero_in_any_case() {
    for off in ["off", "OFF", "Off", "false", "0", "no"] {
        assert!(
            !with(&[("AGENTD_RESEARCH_MODE", off)]).enabled,
            "mode {off:?}"
        );
    }
    for on in ["on", "auto", "yes", "1", "", "anything"] {
        assert!(with(&[("AGENTD_RESEARCH_MODE", on)]).enabled, "mode {on:?}");
    }
}

#[test]
fn the_wait_in_milliseconds_wins_over_the_wait_in_seconds_and_zero_means_do_not_block() {
    let s = with(&[
        ("AGENTD_RESEARCH_WAIT_S", "5"),
        ("AGENTD_RESEARCH_WAIT_MS", "250"),
    ]);
    assert_eq!(s.wait, Duration::from_millis(250));
    assert_eq!(
        with(&[("AGENTD_RESEARCH_WAIT_S", "0")]).wait,
        Duration::ZERO
    );
}
