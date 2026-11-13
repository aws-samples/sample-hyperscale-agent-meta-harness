#![allow(clippy::unwrap_used)]

use agentd::direct::transcript::settings::Settings;
use std::collections::BTreeMap;
use std::time::Duration;

fn with(vars: &[(&str, &str)]) -> Settings {
    let map: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Settings::from_lookup(|k| map.get(k).cloned())
}

#[test]
fn the_defaults_are_the_numbers_of_the_spike_and_the_decision() {
    let s = with(&[]);
    assert!(s.enabled);
    assert_eq!(s.flush_every, Duration::from_secs(30));
    assert_eq!(s.flush_bytes, 256 * 1024);
    assert_eq!(s.coalesce_bytes, 8 * 1024);
    assert_eq!(s.coalesce_every, Duration::from_secs(2));
    assert_eq!(s.record_cap, 256 * 1024);
    assert_eq!(s.window, 4 * 1024);
    assert_eq!(s.pre_cap, 1024 * 1024);
    assert_eq!(s.opaque_cut, 4096);
    assert!(
        s.final_deadline <= Duration::from_secs(12),
        "inside the 15 s the driver gives the finalize hook"
    );
    assert_eq!(s, Settings::default());
}

#[test]
fn a_variable_overrides_its_value() {
    let s = with(&[
        ("AGENTD_TRANSCRIPT_FLUSH_MS", "5000"),
        ("AGENTD_TRANSCRIPT_FLUSH_BYTES", "65536"),
        ("AGENTD_TRANSCRIPT_COALESCE_BYTES", "4096"),
        ("AGENTD_TRANSCRIPT_COALESCE_MS", "500"),
        ("AGENTD_TRANSCRIPT_RECORD_CAP_BYTES", "32768"),
        ("AGENTD_TRANSCRIPT_WINDOW_BYTES", "2048"),
        ("AGENTD_TRANSCRIPT_MAX_HOLD_BYTES", "131072"),
        ("AGENTD_TRANSCRIPT_QUEUE_BYTES", "8388608"),
        ("AGENTD_TRANSCRIPT_FINAL_MS", "3000"),
        ("AGENTD_TRANSCRIPT_UPLOAD_TRIES", "3"),
        ("AGENTD_TRANSCRIPT_RETRY_MS", "50"),
    ]);
    assert_eq!(s.flush_every, Duration::from_secs(5));
    assert_eq!(s.flush_bytes, 65_536);
    assert_eq!(s.coalesce_bytes, 4096);
    assert_eq!(s.coalesce_every, Duration::from_millis(500));
    assert_eq!(s.record_cap, 32_768);
    assert_eq!(s.window, 2048);
    assert_eq!(s.max_hold, 131_072);
    assert_eq!(s.queue_bytes, 8 * 1024 * 1024);
    assert_eq!(s.final_deadline, Duration::from_secs(3));
    assert_eq!(s.upload_tries, 3);
    assert_eq!(s.retry_base, Duration::from_millis(50));
}

#[test]
fn off_switches_the_module_off_and_nothing_else_does() {
    for off in ["off", "OFF", "0", "false", "no", "disabled"] {
        assert!(!with(&[("AGENTD_TRANSCRIPT", off)]).enabled, "{off}");
    }
    for on in ["on", "1", "true", "", "anything else"] {
        assert!(with(&[("AGENTD_TRANSCRIPT", on)]).enabled, "{on:?}");
    }
}

#[test]
fn a_value_that_does_not_parse_keeps_the_default() {
    let s = with(&[
        ("AGENTD_TRANSCRIPT_FLUSH_MS", "soon"),
        ("AGENTD_TRANSCRIPT_FLUSH_BYTES", "-5"),
        ("AGENTD_TRANSCRIPT_WINDOW_BYTES", "4k"),
        ("AGENTD_TRANSCRIPT_UPLOAD_TRIES", ""),
    ]);
    assert_eq!(s, Settings::default());
}

#[test]
fn a_value_out_of_range_is_held_to_its_bound() {
    // an object never waits more than 30 s and one chunk stays under the reader's 1 MiB
    let s = with(&[
        ("AGENTD_TRANSCRIPT_FLUSH_MS", "600000"),
        ("AGENTD_TRANSCRIPT_FLUSH_BYTES", "999999999"),
        ("AGENTD_TRANSCRIPT_RECORD_CAP_BYTES", "999999999"),
    ]);
    assert_eq!(s.flush_every, Duration::from_secs(30));
    assert_eq!(s.flush_bytes, 512 * 1024);
    assert_eq!(s.record_cap, 440 * 1024);
    assert!(
        s.flush_bytes + s.record_cap + 64 * 1024 < 1024 * 1024,
        "the largest object stays under 1 MiB"
    );
    let s = with(&[
        ("AGENTD_TRANSCRIPT_FLUSH_MS", "1"),
        ("AGENTD_TRANSCRIPT_FLUSH_BYTES", "1"),
        ("AGENTD_TRANSCRIPT_RECORD_CAP_BYTES", "1"),
        ("AGENTD_TRANSCRIPT_WINDOW_BYTES", "1"),
        ("AGENTD_TRANSCRIPT_FINAL_MS", "99999999"),
        ("AGENTD_TRANSCRIPT_UPLOAD_TRIES", "0"),
        ("AGENTD_TRANSCRIPT_MAX_HOLD_BYTES", "1"),
        ("AGENTD_TRANSCRIPT_QUEUE_BYTES", "1"),
    ]);
    assert_eq!(s.flush_every, Duration::from_millis(100));
    assert_eq!(s.flush_bytes, 1024);
    assert_eq!(s.record_cap, 1024);
    assert_eq!(s.window, 256);
    assert_eq!(s.final_deadline, Duration::from_secs(12));
    assert_eq!(s.upload_tries, 1);
    assert!(s.max_hold >= 2 * s.window);
    assert_eq!(s.queue_bytes, 1024 * 1024);
    let s = with(&[("AGENTD_TRANSCRIPT_UPLOAD_TRIES", "500")]);
    assert_eq!(s.upload_tries, 20);
}
