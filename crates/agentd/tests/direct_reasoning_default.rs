//! Reasoning on by default, applied by agentd: the model spec of the invocation carries the level the
//! model's catalog row gives (`defaultReasoningEffort`); with no engine settings, or `reasoning.effort = "default"`, the engine is launched with
//! `--thinking <level>`; the user's own level wins; `off` launches with no argument. `summary.json` records the level and where it came from
//! (`engine.reasoningEffort`, `engine.reasoningSource`), and one log line says it. A spec with no level is the launch of 1.45.0, byte for byte
//! (`direct_engine_config.rs` and the 13 goldens hold that).
//!
//! Each test runs the whole agent in process against the fake AWS and the fake ACP engine, with the product's module set.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use direct_support::{Direct, Opts, RUN, sha};
use harness_protocol::direct::{DirectOutcome, s3 as keys};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use serde_json::{Value, json};

fn with_members(d: &mut Direct, members: Value) {
    let mut v: Value = serde_json::from_slice(&d.request_json).unwrap();
    for (k, val) in members.as_object().unwrap() {
        v[k] = val.clone();
    }
    let bytes = serde_json::to_vec(&v).unwrap();
    d.aws.put(&keys::request_key(RUN), bytes.clone());
    d.req.request.sha256 = sha(&bytes);
    d.request_json = bytes;
}

async fn run(d: &Direct) -> InvocationResponse {
    agentd::direct::run_with(&d.app, d.req.clone(), &agentd::direct::Deps::default()).await
}

fn argv_of(d: &Direct) -> Vec<String> {
    let probe = d.engine_probe();
    let mut inside = false;
    let mut out = Vec::new();
    for line in probe.lines() {
        if let Some(h) = line.strip_prefix("--- ") {
            inside = h == "argv";
        } else if inside {
            out.push(line.to_string());
        }
    }
    out
}

fn thinking(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--thinking")
        .map(|w| w[1].clone())
        .collect()
}

fn member(effort: &str) -> Value {
    json!({"engineConfig": {"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": effort}}}})
}

/// A started run whose model spec carries `level` (or none) and whose request carries `effort` in its engine settings (or none).
async fn started(level: Option<&str>, effort: Option<&str>) -> Direct {
    let mut d = Direct::start(Opts::default()).await;
    d.req.model.default_reasoning_effort = level.map(str::to_string);
    if let Some(e) = effort {
        with_members(&mut d, member(e));
    }
    let resp = run(&d).await;
    assert_eq!(
        resp.outcome,
        TurnOutcome::Completed,
        "{resp:?}\n{}",
        d.engine_trace()
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    d
}

fn recorded(d: &Direct) -> (Option<String>, Option<String>) {
    let s = d.summary();
    (s.engine.reasoning_effort, s.engine.reasoning_source)
}

fn reasoning_log(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter(|l| l.contains("direct reasoning"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_engine_settings_the_engine_is_launched_with_the_level_of_the_model_spec() {
    for level in ["low", "medium", "high", "xhigh"] {
        let d = started(Some(level), None).await;
        assert_eq!(thinking(&argv_of(&d)), vec![level.to_string()], "{level}");
        assert_eq!(
            recorded(&d),
            (Some(level.to_string()), Some("default".to_string())),
            "{level}"
        );
        let log = reasoning_log(&d);
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(
            log[0].contains(level) && log[0].contains("model"),
            "{log:?}"
        );
        // No settings were sent, so none are reported as placed.
        assert!(d.aws.progress("engine-config").is_empty());
        assert!(
            serde_json::to_value(d.summary())
                .unwrap()
                .get("engineConfig")
                .is_none()
        );
    }
    println!(
        "REASONING-DEFAULT TRIGGER PRESENT: a spec level reached the engine argv with no engine settings"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_key_default_means_the_same_as_no_key() {
    let d = started(Some("xhigh"), Some("default")).await;
    assert_eq!(thinking(&argv_of(&d)), vec!["xhigh".to_string()]);
    assert_eq!(recorded(&d), (Some("xhigh".into()), Some("default".into())));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_own_level_wins_over_the_model_spec_and_is_read_back() {
    for (user, spec) in [
        ("high", "xhigh"),
        ("xhigh", "high"),
        ("low", "xhigh"),
        ("medium", "high"),
    ] {
        let d = started(Some(spec), Some(user)).await;
        assert_eq!(
            thinking(&argv_of(&d)),
            vec![user.to_string()],
            "{user} over {spec}"
        );
        assert_eq!(
            recorded(&d),
            (Some(user.to_string()), Some("explicit".to_string())),
            "{user} over {spec}"
        );
        // The user's settings are placed and reported the way they were before.
        assert_eq!(d.aws.progress("engine-config").len(), 1);
        assert!(reasoning_log(&d)[0].contains(user));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_user_who_names_a_level_with_no_spec_level_still_gets_it() {
    let d = started(None, Some("xhigh")).await;
    assert_eq!(thinking(&argv_of(&d)), vec!["xhigh".to_string()]);
    assert_eq!(
        recorded(&d),
        (Some("xhigh".into()), Some("explicit".into()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn off_launches_the_engine_with_no_reasoning_argument_and_says_so() {
    let d = started(Some("xhigh"), Some("off")).await;
    let argv = argv_of(&d);
    assert!(thinking(&argv).is_empty(), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--auto-approve"), "{argv:?}");
    assert_eq!(recorded(&d), (None, Some("off".to_string())));
    let log = reasoning_log(&d);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("off"), "{log:?}");
    println!(
        "REASONING-DEFAULT SWITCH PRESENT: reasoning.effort = off removed the argument the spec level would have added"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spec_with_no_level_is_the_launch_of_1_45_0() {
    for effort in [None, Some("default")] {
        let d = started(None, effort).await;
        let argv = argv_of(&d);
        assert!(thinking(&argv).is_empty(), "{argv:?}");
        assert_eq!(recorded(&d), (None, None));
        assert!(reasoning_log(&d).is_empty(), "{:?}", reasoning_log(&d));
        let summary = serde_json::to_value(d.summary()).unwrap();
        assert!(
            summary["engine"].get("reasoningEffort").is_none(),
            "{summary}"
        );
        assert!(
            summary["engine"].get("reasoningSource").is_none(),
            "{summary}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_level_is_for_the_model_the_run_started_on() {
    // The engine process is launched once, so a switch to a route of another model keeps the start model's level; the spec of a failover route
    // is not read for the launch.
    let mut d = Direct::start(Opts::default()).await;
    d.req.model.default_reasoning_effort = Some("high".into());
    for r in &mut d.req.failover {
        r.default_reasoning_effort = Some("xhigh".into());
    }
    let resp = run(&d).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(thinking(&argv_of(&d)), vec!["high".to_string()]);
    assert_eq!(recorded(&d).0.as_deref(), Some("high"));
}
