//! The stage settings of the review round: `AGENTD_DIRECT_REVIEW_ROUND` (default on; `0`, `off`, `false` and `no` switch it off for every
//! run) and `AGENTD_DIRECT_REVIEW_GUARD_S` (default 1200 s; 60 to 28,800; anything else is the default). The request's `review` member lays over
//! these for one run (`tests/direct_review.rs`).
#![allow(clippy::unwrap_used)]

use agentd::config::DirectConfig;
use std::collections::HashMap;
use std::time::Duration;

fn from(pairs: &[(&str, &str)]) -> DirectConfig {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    DirectConfig::from_vars(&|k| m.get(k).cloned())
}

#[test]
fn the_round_is_on_by_default_and_the_guard_is_twenty_minutes() {
    let d = from(&[]);
    assert!(d.review_round);
    assert_eq!(d.review_guard, Duration::from_secs(1200));
    assert_eq!(d, DirectConfig::default());
}

#[test]
fn the_switch_takes_the_words_the_other_stage_switches_take() {
    for off in ["0", "off", "OFF", "false", "False", "no", " no "] {
        assert!(
            !from(&[("AGENTD_DIRECT_REVIEW_ROUND", off)]).review_round,
            "{off:?}"
        );
    }
    for on in ["1", "on", "true", "yes", "", "auto", "maybe"] {
        assert!(
            from(&[("AGENTD_DIRECT_REVIEW_ROUND", on)]).review_round,
            "{on:?} leaves the round on"
        );
    }
}

#[test]
fn the_guard_takes_whole_seconds_from_60_to_28800_and_anything_else_is_the_default() {
    for (given, want) in [("60", 60), ("600", 600), (" 900 ", 900), ("28800", 28_800)] {
        assert_eq!(
            from(&[("AGENTD_DIRECT_REVIEW_GUARD_S", given)]).review_guard,
            Duration::from_secs(want),
            "{given:?}"
        );
    }
    for bad in ["59", "0", "28801", "-5", "20m", "1.5", "", "many"] {
        assert_eq!(
            from(&[("AGENTD_DIRECT_REVIEW_GUARD_S", bad)]).review_guard,
            Duration::from_secs(1200),
            "{bad:?}"
        );
    }
}
