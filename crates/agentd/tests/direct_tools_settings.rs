//! The settings, the backend seam and the counters of the tool server.
//!
//! Settings come from `AGENTD_TOOLS_*` environment variables like the research module's. The switch is a kill switch: an unset variable leaves the
//! module on (the stage deploy always writes a concrete word), the words that mean off switch it off, and a word that is neither is read as off,
//! because a typo in a kill switch must not leave the thing running.
#![allow(clippy::unwrap_used)]

mod common;
mod tools_support;

use agentd::direct::tools::backend::{
    CancelToken, FailureWord, ToolContext, ToolOutput, validate_input,
};
use agentd::direct::tools::backends::retain_enabled;
use agentd::direct::tools::settings::{
    DEFAULT_PORT, ENV_DISABLE, ENV_MODE, ENV_PORT, Mode, Settings,
};
use agentd::direct::tools::stats::{Counters, MAX_LATENCY_SAMPLES};
use harness_protocol::tools::ToolName;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tools_support::echo::Echo;

fn settings(vars: &[(&str, &str)]) -> Settings {
    let map: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Settings::from_lookup(&|k| map.get(k).cloned())
}

// ---- settings -------------------------------------------------------------------------------------------------------------------

#[test]
fn the_defaults_leave_the_module_on_at_port_3130_with_every_tool() {
    let s = settings(&[]);
    assert!(s.enabled);
    assert_eq!(s.mode, Mode::Default);
    assert_eq!(s.port, 3130);
    assert_eq!(DEFAULT_PORT, 3130);
    assert!(s.disabled.is_empty());
    assert!(s.allows("code_search") && s.allows("semantic_search"));
    assert_eq!(ENV_MODE, "AGENTD_TOOLS_MODE");
    assert_eq!(ENV_DISABLE, "AGENTD_TOOLS_DISABLE");
    assert_eq!(ENV_PORT, "AGENTD_TOOLS_PORT");
}

#[test]
fn the_words_that_mean_off_switch_the_module_off_and_the_words_that_mean_on_keep_it() {
    for w in ["off", "OFF", "Off", " off ", "false", "0", "no", "NO"] {
        let s = settings(&[(ENV_MODE, w)]);
        assert!(!s.enabled, "{w:?}");
        assert_eq!(s.mode, Mode::Off, "{w:?}");
    }
    for w in ["on", "ON", " On ", "true", "1", "yes"] {
        let s = settings(&[(ENV_MODE, w)]);
        assert!(s.enabled, "{w:?}");
        assert_eq!(s.mode, Mode::On, "{w:?}");
    }
    // Unset or empty is the default, which is on.
    for w in ["", "   "] {
        let s = settings(&[(ENV_MODE, w)]);
        assert!(s.enabled, "{w:?}");
        assert_eq!(s.mode, Mode::Default, "{w:?}");
    }
}

#[test]
fn a_word_that_is_neither_is_read_as_off_and_says_so() {
    for w in [
        "of", "offf", "disable", "disabled", "maybe", "2", "nope", "o ff",
    ] {
        let s = settings(&[(ENV_MODE, w)]);
        assert!(!s.enabled, "{w:?} left the kill switch open");
        assert_eq!(s.mode, Mode::Unrecognized, "{w:?}");
    }
}

#[test]
fn the_port_is_a_number_and_a_bad_one_keeps_the_default() {
    assert_eq!(settings(&[(ENV_PORT, "4000")]).port, 4000);
    assert_eq!(settings(&[(ENV_PORT, " 4001 ")]).port, 4001);
    // 0 asks for any free port at once (tests).
    assert_eq!(settings(&[(ENV_PORT, "0")]).port, 0);
    for bad in ["", "x", "-1", "70000", "3130x", "3.5"] {
        assert_eq!(settings(&[(ENV_PORT, bad)]).port, 3130, "{bad:?}");
    }
}

#[test]
fn the_disable_list_names_single_tools() {
    let s = settings(&[(ENV_DISABLE, " Semantic_Search , web_search,, ")]);
    assert!(!s.allows("semantic_search"));
    assert!(!s.allows("web_search"));
    assert!(s.allows("code_search"));
    assert_eq!(s.disabled.len(), 2);
    // An empty list, or names that are no tool, remove nothing.
    assert!(settings(&[(ENV_DISABLE, "")]).allows("code_search"));
    assert!(settings(&[(ENV_DISABLE, "nothing")]).allows("code_search"));
}

#[test]
fn disable_names_remove_single_tools_from_the_list_of_backends() {
    let (a, _) = Echo::new();
    let kept = retain_enabled(vec![a], &settings(&[(ENV_DISABLE, "semantic_search")]));
    assert_eq!(kept.len(), 1);
    let (b, _) = Echo::new();
    let kept = retain_enabled(vec![b], &settings(&[(ENV_DISABLE, "code_search")]));
    assert!(kept.is_empty());
    // A list with the same tool twice keeps the first (the server lists a name once).
    let (c, _) = Echo::new();
    let (d, _) = Echo::new();
    let kept = retain_enabled(vec![c, d], &settings(&[]));
    assert_eq!(kept.len(), 1);
}

// ---- the backend seam -----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_token_wakes_every_waiter_and_stays_cancelled() {
    let t = CancelToken::new();
    assert!(!t.is_cancelled());
    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let t = t.clone();
            tokio::spawn(async move { t.cancelled().await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(waiters.iter().all(|w| !w.is_finished()));
    t.cancel();
    for w in waiters {
        tokio::time::timeout(Duration::from_secs(1), w)
            .await
            .unwrap()
            .unwrap();
    }
    assert!(t.is_cancelled());
    // Late waiters, and a second cancel, are fine.
    t.cancel();
    tokio::time::timeout(Duration::from_millis(100), t.cancelled())
        .await
        .unwrap();
    // A token made after the cancel on a clone is the same token.
    assert!(t.clone().is_cancelled());
}

#[test]
fn the_context_carries_the_work_tree_the_context_directory_and_a_spec_builder() {
    let counters = Counters::new();
    let spec: agentd::direct::tools::backend::SpecFn =
        Arc::new(|argv, stderr| agentd::engine::LaunchSpec {
            argv,
            env: BTreeMap::from([("K".to_string(), "v".to_string())]),
            cwd: "/work".into(),
            path: vec!["/bin".into()],
            stderr_log: format!("/data/{stderr}").into(),
            run_as: Some((901, 901)),
            sandbox: None,
        });
    let ctx = ToolContext::new("/work".into(), Some("/ctx".into()), spec, counters.clone());
    assert_eq!(ctx.work_tree, std::path::PathBuf::from("/work"));
    assert_eq!(ctx.context_dir, Some(std::path::PathBuf::from("/ctx")));
    let l = ctx.launch_spec(vec!["rg".into(), "--version".into()], "rg.stderr");
    assert_eq!(l.argv, ["rg", "--version"]);
    assert_eq!(l.run_as, Some((901, 901)));
    assert_eq!(l.stderr_log, std::path::PathBuf::from("/data/rg.stderr"));
    assert!(Arc::ptr_eq(&ctx.counters, &counters));
    // The plain context (tests, the example): the engine's own user is not changed.
    let plain = ToolContext::for_directory("/w".into(), Counters::new());
    assert_eq!(plain.launch_spec(vec!["true".into()], "s").run_as, None);
    assert_eq!(plain.context_dir, None);
}

#[test]
fn input_is_validated_by_the_table_before_a_backend_sees_it() {
    assert!(validate_input(ToolName::CodeSearch, &json!({"pattern": "x"})).is_ok());
    let e = validate_input(ToolName::CodeSearch, &json!({})).unwrap_err();
    assert_eq!(e.to_string(), "pattern: is required");
    assert!(
        validate_input(
            ToolName::CodeSearch,
            &json!({"pattern": "x", "maxResults": 999})
        )
        .is_err()
    );
    assert!(validate_input(ToolName::CodeSearch, &json!("x")).is_err());
}

#[test]
fn output_constructors_say_what_they_mean() {
    assert_eq!(
        ToolOutput::text("a"),
        ToolOutput::Text {
            text: "a".into(),
            truncated: false
        }
    );
    assert_eq!(
        ToolOutput::shortened("a"),
        ToolOutput::Text {
            text: "a".into(),
            truncated: true
        }
    );
    assert_eq!(
        ToolOutput::failed(FailureWord::Timeout, "slow"),
        ToolOutput::Failed {
            word: FailureWord::Timeout,
            detail: Some("slow".into())
        }
    );
    assert_eq!(
        ToolOutput::failed_bare(FailureWord::Busy),
        ToolOutput::Failed {
            word: FailureWord::Busy,
            detail: None
        }
    );
}

// ---- the counters ---------------------------------------------------------------------------------------------------------------

#[test]
fn latencies_are_ranked_and_a_run_without_calls_has_none() {
    let c = Counters::new();
    let s = c.snapshot();
    assert_eq!((s.p50_ms, s.p95_ms), (None, None));
    c.record_latency(Duration::from_millis(7));
    let s = c.snapshot();
    assert_eq!((s.p50_ms, s.p95_ms), (Some(7), Some(7)));
    let c = Counters::new();
    for ms in (1..=20).rev() {
        c.record_latency(Duration::from_millis(ms));
    }
    let s = c.snapshot();
    // Nearest rank: the 10th of 20 and the 19th of 20.
    assert_eq!((s.p50_ms, s.p95_ms), (Some(10), Some(19)));
    // Only the newest samples are kept, so a long run does not grow without bound.
    let c = Counters::new();
    for _ in 0..(MAX_LATENCY_SAMPLES + 500) {
        c.record_latency(Duration::from_millis(1));
    }
    c.record_latency(Duration::from_millis(900));
    let s = c.snapshot();
    assert_eq!(s.p50_ms, Some(1));
}

#[test]
fn calls_are_counted_per_bare_tool_name() {
    let c = Counters::new();
    c.record_call(ToolName::CodeSearch);
    c.record_call(ToolName::CodeSearch);
    c.record_failed();
    c.record_refused();
    c.record_refused();
    c.record_truncated();
    c.record_cancelled();
    c.record_restart();
    c.record_initialize();
    c.record_list();
    c.record_protocol_error();
    c.record_unknown_protocol_header();
    let s = c.snapshot();
    assert_eq!(s.calls, BTreeMap::from([("code_search".to_string(), 2)]));
    assert_eq!(
        (
            s.failed,
            s.refused,
            s.truncated,
            s.cancelled,
            s.restarts,
            s.initializes,
            s.lists,
            s.protocol_errors,
            s.unknown_protocol_headers
        ),
        (1, 2, 1, 1, 1, 1, 1, 1, 1)
    );
    let counts = s.to_counts();
    assert_eq!(counts.calls["code_search"], 2);
    assert_eq!(
        (
            counts.failed,
            counts.refused,
            counts.truncated,
            counts.restarts
        ),
        (1, 2, 1, 1)
    );
}

#[test]
fn echo_is_a_backend_of_the_table() {
    let (b, _) = Echo::new();
    assert_eq!(b.name(), ToolName::CodeSearch);
}
