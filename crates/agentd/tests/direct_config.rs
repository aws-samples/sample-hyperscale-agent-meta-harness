//! The direct driver's settings from the environment (`agentd::config::DirectConfig`): the defaults are the contract's, the kill
//! switch lists only well-formed tool names, and a bad number falls back to the default.
#![allow(clippy::unwrap_used)]

use agentd::config::{Config, DirectConfig};
use std::collections::BTreeMap;
use std::time::Duration;

fn from(pairs: &[(&str, &str)]) -> DirectConfig {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    DirectConfig::from_vars(&|k| m.get(k).cloned())
}

#[test]
fn the_defaults_are_the_contract_s() {
    let d = from(&[]);
    assert!(
        d.disabled_tools.is_empty(),
        "sub-agents stay on unless the kill switch is set"
    );
    assert_eq!(d.events_every, Duration::from_secs(2));
    assert_eq!(d.heartbeat_every, Duration::from_secs(60));
    assert_eq!(d.upload_retry_pause, Duration::from_secs(2));
    assert_eq!(d.snapshot_interval, None);
    assert_eq!(d, DirectConfig::default());
}

#[test]
fn the_kill_switch_keeps_well_formed_tool_names_only() {
    let d = from(&[(
        "AGENTD_DISABLED_TOOLS",
        " spawn_agent, ask_question ,Bad Name,\"quoted\",,teams2 ",
    )]);
    assert_eq!(d.disabled_tools, ["spawn_agent", "ask_question", "teams2"]);
}

#[test]
fn milliseconds_are_read_and_a_bad_value_is_the_default() {
    let d = from(&[
        ("AGENTD_DIRECT_EVENTS_EVERY_MS", "100"),
        ("AGENTD_DIRECT_HEARTBEAT_EVERY_MS", "0"),
        ("AGENTD_DIRECT_UPLOAD_PAUSE_MS", "soon"),
        ("AGENTD_DIRECT_SNAPSHOT_EVERY_MS", "500"),
    ]);
    assert_eq!(d.events_every, Duration::from_millis(100));
    assert_eq!(
        d.heartbeat_every,
        Duration::from_secs(60),
        "zero would spin the flusher"
    );
    assert_eq!(d.upload_retry_pause, Duration::from_secs(2));
    assert_eq!(d.snapshot_interval, Some(Duration::from_millis(500)));
}

#[test]
fn the_server_config_carries_it() {
    let cfg =
        Config::from_vars(|k| (k == "AGENTD_DISABLED_TOOLS").then(|| "spawn_agent".to_string()));
    assert_eq!(cfg.direct.disabled_tools, ["spawn_agent"]);
}

#[test]
fn the_outage_notice_comes_after_thirty_minutes_unless_the_setting_says_otherwise() {
    // The hook of the recovery ladder's "needs your input": one notice while the run goes on waiting.
    assert_eq!(from(&[]).outage_notice, Duration::from_secs(1_800));
    assert_eq!(
        from(&[("AGENTD_DIRECT_OUTAGE_NOTICE_S", "600")]).outage_notice,
        Duration::from_secs(600)
    );
    for bad in ["0", "soon", "-5", ""] {
        assert_eq!(
            from(&[("AGENTD_DIRECT_OUTAGE_NOTICE_S", bad)]).outage_notice,
            Duration::from_secs(1_800),
            "{bad:?}"
        );
    }
}

#[test]
fn the_engine_stop_resumes_default_to_one_and_a_bad_value_is_the_default() {
    // How many times agentd resumes a prompt Cline ended itself (`cancelled` with no cancel from agentd). 0 is allowed.
    assert_eq!(from(&[]).engine_stop_resumes, 1);
    assert_eq!(
        from(&[("AGENTD_DIRECT_ENGINE_STOP_RESUMES", "0")]).engine_stop_resumes,
        0
    );
    assert_eq!(
        from(&[("AGENTD_DIRECT_ENGINE_STOP_RESUMES", " 2 ")]).engine_stop_resumes,
        2
    );
    assert_eq!(
        from(&[("AGENTD_DIRECT_ENGINE_STOP_RESUMES", "9")]).engine_stop_resumes,
        3,
        "at most three: a brake that keeps firing is not asked to continue for hours"
    );
    for bad in ["many", "-1", "", "1.5"] {
        assert_eq!(
            from(&[("AGENTD_DIRECT_ENGINE_STOP_RESUMES", bad)]).engine_stop_resumes,
            1,
            "{bad:?}"
        );
    }
}

#[test]
fn the_input_settings_default_to_the_decided_values() {
    // The server default is no wait, so a script, the release gate or an old client behaves as
    // before; three questions, a 15 minute margin, a 5 minute smallest window, one automatic and three requested fresh sessions,
    // and a 30 minute outage notice.
    let d = from(&[]);
    assert_eq!(d.input_wait, Duration::ZERO);
    assert_eq!(d.input_max_requests, 3);
    assert_eq!(d.input_margin, Duration::from_secs(900));
    assert_eq!(d.input_min_window, Duration::from_secs(300));
    assert_eq!(d.auto_sessions_max, 1);
    assert_eq!(d.user_sessions_max, 3);
    assert_eq!(d.outage_notice, Duration::from_secs(1800));
}

#[test]
fn the_input_settings_are_read_from_the_environment() {
    let d = from(&[
        ("AGENTD_DIRECT_INPUT_WAIT_S", " 600 "),
        ("AGENTD_DIRECT_INPUT_MAX_REQUESTS", "5"),
        ("AGENTD_DIRECT_INPUT_MARGIN_S", "120"),
        ("AGENTD_DIRECT_INPUT_MIN_WINDOW_S", "60"),
        ("AGENTD_DIRECT_AUTO_SESSIONS_MAX", "2"),
        ("AGENTD_DIRECT_USER_SESSIONS_MAX", "5"),
        ("AGENTD_DIRECT_OUTAGE_NOTICE_S", "600"),
    ]);
    assert_eq!(d.input_wait, Duration::from_secs(600));
    assert_eq!(d.input_max_requests, 5);
    assert_eq!(d.input_margin, Duration::from_secs(120));
    assert_eq!(d.input_min_window, Duration::from_secs(60));
    assert_eq!(d.auto_sessions_max, 2);
    assert_eq!(d.user_sessions_max, 5);
    assert_eq!(d.outage_notice, Duration::from_secs(600));
}

#[test]
fn zero_is_a_real_setting_where_it_means_off_and_a_bad_value_is_the_default() {
    // 0 switches a feature off: never wait, never ask, no automatic or requested fresh session, no margin.
    let d = from(&[
        ("AGENTD_DIRECT_INPUT_WAIT_S", "0"),
        ("AGENTD_DIRECT_INPUT_MAX_REQUESTS", "0"),
        ("AGENTD_DIRECT_INPUT_MARGIN_S", "0"),
        ("AGENTD_DIRECT_AUTO_SESSIONS_MAX", "0"),
        ("AGENTD_DIRECT_USER_SESSIONS_MAX", "0"),
    ]);
    assert_eq!(d.input_wait, Duration::ZERO);
    assert_eq!(d.input_max_requests, 0);
    assert_eq!(d.input_margin, Duration::ZERO);
    assert_eq!(d.auto_sessions_max, 0);
    assert_eq!(d.user_sessions_max, 0);
    // Zero would open a question with no time left to answer it, or raise the outage notice at once: those two keep the default.
    let d = from(&[
        ("AGENTD_DIRECT_INPUT_MIN_WINDOW_S", "0"),
        ("AGENTD_DIRECT_OUTAGE_NOTICE_S", "0"),
    ]);
    assert_eq!(d.input_min_window, Duration::from_secs(300));
    assert_eq!(d.outage_notice, Duration::from_secs(1800));
    for bad in ["soon", "-1", "", "1.5", "1e3"] {
        let d = from(&[
            ("AGENTD_DIRECT_INPUT_WAIT_S", bad),
            ("AGENTD_DIRECT_INPUT_MAX_REQUESTS", bad),
            ("AGENTD_DIRECT_INPUT_MARGIN_S", bad),
            ("AGENTD_DIRECT_INPUT_MIN_WINDOW_S", bad),
            ("AGENTD_DIRECT_AUTO_SESSIONS_MAX", bad),
            ("AGENTD_DIRECT_USER_SESSIONS_MAX", bad),
            ("AGENTD_DIRECT_OUTAGE_NOTICE_S", bad),
        ]);
        assert_eq!(
            d,
            from(&[]),
            "a bad value ({bad:?}) is the default for every input setting"
        );
    }
}

#[test]
fn the_input_settings_are_bounded() {
    let d = from(&[
        ("AGENTD_DIRECT_INPUT_WAIT_S", "999999"),
        ("AGENTD_DIRECT_INPUT_MAX_REQUESTS", "99"),
        ("AGENTD_DIRECT_INPUT_MARGIN_S", "99999"),
        ("AGENTD_DIRECT_INPUT_MIN_WINDOW_S", "99999"),
        ("AGENTD_DIRECT_AUTO_SESSIONS_MAX", "9"),
        ("AGENTD_DIRECT_USER_SESSIONS_MAX", "99"),
        ("AGENTD_DIRECT_OUTAGE_NOTICE_S", "9999999"),
    ]);
    assert_eq!(
        d.input_wait,
        Duration::from_secs(14_400),
        "at most 4 hours, the request field's bound"
    );
    assert_eq!(d.input_max_requests, 8);
    assert_eq!(d.input_margin, Duration::from_secs(3_600));
    assert_eq!(d.input_min_window, Duration::from_secs(3_600));
    assert_eq!(d.auto_sessions_max, 3);
    assert_eq!(d.user_sessions_max, 8);
    assert_eq!(d.outage_notice, Duration::from_secs(86_400));
}

#[test]
fn the_server_config_carries_the_input_settings() {
    let cfg = Config::from_vars(|k| (k == "AGENTD_DIRECT_INPUT_WAIT_S").then(|| "900".to_string()));
    assert_eq!(cfg.direct.input_wait, Duration::from_secs(900));
}
