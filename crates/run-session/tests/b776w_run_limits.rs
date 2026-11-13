//! Wave 2 (backend lambdas): a run's research policy lives as long as the run, the prompt names as many URLs as one row holds, and the
//! number of Instances sessions has no ceiling of its own.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DEFAULT_MAX_RUN_S, MAX_MAX_RUN_S};
use harness_protocol::research::{MAX_PROMPT_URLS, ResearchLimits, ResearchProvider};
use harness_protocol::target::RunTarget;
use run_session::compute::from_lookup;
use run_session::ops::{Op, ResearchStage, handle};
use run_session::prepare::prompt_urls;
use serde_json::{Value, json};
use support::fakes::*;

fn stage() -> ResearchStage {
    ResearchStage {
        provider: ResearchProvider::Tavily,
        limits: ResearchLimits::default(),
    }
}

fn target() -> RunTarget {
    serde_json::from_value(json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"})).unwrap()
}

/// `prepare` for a run whose stored request carries `extra`, and the expiry of the policy row it wrote.
async fn expiry_with(extra: Value) -> u64 {
    let mem = Mem::default();
    *mem.0.research_cfg.lock().unwrap() = Some(stage());
    mem.seed_with_members(
        target(),
        "Add percent_change(a, b) with tests",
        MODEL,
        extra,
    );
    let op: Op = serde_json::from_value(json!({"op": "prepare", "runId": RUN})).unwrap();
    handle(&mem.deps(), &op).await.unwrap();
    let rows = mem.0.research_rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    rows[0].validate().unwrap();
    rows[0].expires_at
}

#[tokio::test]
async fn b776w_research_stays_on_for_as_long_as_the_run_may_last() {
    let start = T0 / 1000;
    // Nothing named: the stage's 24 hours, and an hour more. The old row ended 8.5 hours after the start, while the run went on.
    assert_eq!(
        expiry_with(json!({})).await,
        start + u64::from(DEFAULT_MAX_RUN_S) + 3600
    );
    // Three days named: three days and an hour.
    assert_eq!(
        expiry_with(json!({"maxRunS": 3 * 86_400})).await,
        start + 3 * 86_400 + 3600
    );
    // A run shorter than one session keeps the row for a whole session: the longest session is 28,800 seconds.
    assert_eq!(
        expiry_with(json!({"maxRunS": 1_800})).await,
        start + 28_800 + 3600
    );
}

#[tokio::test]
async fn b776w_a_run_total_that_is_zero_or_too_long_cannot_move_the_expiry_off_the_run() {
    let start = T0 / 1000;
    // Control-api refuses both; the row still takes the default for 0 and never lives past the longest run.
    assert_eq!(
        expiry_with(json!({"maxRunS": 0})).await,
        start + u64::from(DEFAULT_MAX_RUN_S) + 3600
    );
    assert_eq!(
        expiry_with(json!({"maxRunS": u64::from(MAX_MAX_RUN_S) * 4})).await,
        start + u64::from(MAX_MAX_RUN_S) + 3600
    );
}

#[test]
fn b776w_the_prompt_names_up_to_150_urls_and_the_151st_is_left_out() {
    assert_eq!(MAX_PROMPT_URLS, 150);
    let prompt: String = (0..200)
        .map(|i| format!("see https://example.org/page/{i}\n"))
        .collect();
    let urls = prompt_urls(&prompt, 2048);
    assert_eq!(urls.len(), 150);
    assert_eq!(urls[0], "https://example.org/page/0");
    assert_eq!(urls[149], "https://example.org/page/149");
}

#[test]
fn b776w_the_instances_cap_has_a_floor_and_no_ceiling() {
    let env = |pairs: Vec<(&'static str, &'static str)>| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    };
    for big in ["1001", "5000", "4294967295"] {
        let (stage, _) = from_lookup(env(vec![("COMPUTE_MAX_ACTIVE", big)])).unwrap();
        assert_eq!(stage.max_active, Some(big.parse().unwrap()), "{big}");
    }
    for bad in ["0", "-1", "many", "4294967296"] {
        let e = from_lookup(env(vec![("COMPUTE_MAX_ACTIVE", bad)])).unwrap_err();
        assert!(e.contains("COMPUTE_MAX_ACTIVE"), "{bad}: {e}");
    }
}
