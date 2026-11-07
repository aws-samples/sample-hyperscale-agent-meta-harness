//! The usage line of the delivery card (client polish 2): `GET /v1/runs/{id}` answers `cost`, priced from the
//! engine-reported usage its turn recorded; the card reads nothing else.
//! The card shows the tokens and the price of what the run used, or no line when the stage reported none: a "Tokens 0  USD 0.00" line
//! for a run that used 1.2 million tokens is a fake zero (D's observation on the stage digest 181af04b).
#![allow(clippy::unwrap_used)]

use ham::pull::card;
use ham_core::render::CardUsage;
use harness_protocol::api::v1::RunDetail;
use serde_json::{Value, json};

const RUN: &str = "01M4Y2ZZS7BQH5WAKKZ3H57PQ0";

fn detail(tokens_used: u64, cost_usd: f64, cost: Option<Value>) -> RunDetail {
    let mut v = json!({
        "summary": {"runId": RUN, "status": "complete", "phase": "delivering", "target": {"mode": "s3-only"},
                    "createdAt": "2026-10-14T20:00:00Z", "ownerPrincipalArn": "arn:aws:sts::1:assumed-role/Admin/x"},
        "tokensUsed": tokens_used, "costUsd": cost_usd
    });
    if let Some(c) = cost {
        v["cost"] = c;
    }
    serde_json::from_value(v).unwrap()
}

fn line(model: &str, usage: [u64; 4], usd: f64) -> Value {
    json!({"modelId": model, "engine": "cline", "usd": usd, "usage": {
        "inputTokens": usage[0], "outputTokens": usage[1],
        "cacheReadInputTokens": usage[2], "cacheWriteInputTokens": usage[3]}})
}

fn cost(lines: Vec<Value>, unpriced: &[&str]) -> Value {
    let mut c = json!({"pricesVersion": "2026-10-01", "lines": lines, "turnsWithoutUsage": 0});
    if !unpriced.is_empty() {
        c["unpricedModels"] = json!(unpriced);
    }
    c
}

#[test]
fn the_tokens_of_a_direct_run_are_the_sum_of_the_usage_its_cost_lines_carry() {
    // The four kinds of token add up as control-api counts them for a budget.
    let d = detail(
        0,
        1.54,
        Some(cost(
            vec![line(
                "us.anthropic.claude-sonnet-5",
                [100_000, 100_000, 1_000_000, 0],
                1.54,
            )],
            &[],
        )),
    );
    assert_eq!(
        card(Some(&d), None).usage,
        Some(CardUsage {
            tokens: 1_200_000,
            usd: Some(1.54)
        })
    );
    // Two models: the lines add up, including the cache write tokens.
    let d = detail(
        0,
        0.75,
        Some(cost(
            vec![
                line("us.anthropic.claude-sonnet-5", [10, 20, 30, 40], 0.5),
                line("us.openai.gpt-6", [1, 2, 3, 4], 0.25),
            ],
            &[],
        )),
    );
    assert_eq!(
        card(Some(&d), None).usage,
        Some(CardUsage {
            tokens: 110,
            usd: Some(0.75)
        })
    );
}

#[test]
fn no_usage_anywhere_is_no_usage_and_not_zero() {
    // The run's turn ended without engine-reported usage: no cost, no counter.
    assert_eq!(card(Some(&detail(0, 0.0, None)), None).usage, None);
    // The detail could not be read at all.
    assert_eq!(card(None, None).usage, None);
    // A usage record that is all zeros says nothing was used or nothing was reported; the card does not claim either.
    let zero = detail(0, 0.0, Some(cost(vec![line("m", [0, 0, 0, 0], 0.0)], &[])));
    assert_eq!(card(Some(&zero), None).usage, None);
}

#[test]
fn a_model_with_no_price_row_gives_tokens_and_an_unknown_cost() {
    let d = detail(
        0,
        0.0,
        Some(cost(
            vec![line("us.vendor.new-model", [10, 20, 30, 40], 0.0)],
            &["us.vendor.new-model"],
        )),
    );
    assert_eq!(
        card(Some(&d), None).usage,
        Some(CardUsage {
            tokens: 100,
            usd: None
        })
    );
}

#[test]
fn the_other_card_facts_do_not_depend_on_the_usage() {
    let c = card(None, None);
    assert_eq!(c.local_path, "not downloaded");
    assert_eq!((c.presigned_url, c.expires_at), (None, None));
}
