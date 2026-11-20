//! Per-run token cost: the engine-reported usage of a run's turns times the pinned price table
//! (`src/cost/prices.json`, USD per 1M tokens). `GET /v1/runs/{runId}` shows it and `ham` prints it on the final card.
#![allow(clippy::unwrap_used)]

use control_api::cost::{PriceTable, model_of, pinned, round_usd, run_cost, total_usd};
use control_api::direct_models::builtin;
use harness_protocol::common::{Engine, Usage};
use harness_protocol::store::{AgentItem, TurnItem};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const RUN: &str = "01M3FQCYQKGQNSYW8H5F72X81Y";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

/// The models of `engine/catalog/models.json` that have no price row yet. A model belongs here only until its row is added,
/// and the test below fails the day it is: remove it from this list then.
/// GLM 5.3, and GPT-6.2 Sol and GPT-6.2 Astra: Bedrock has published no price for the two new models, so they get no row, and a
/// run on one of them shows its cost as unknown. A row is added from Bedrock's own price, never from the maker's list price.
const UNPRICED: [&str; 3] = [
    "us.zai.glm-5.3",
    "us.openai.gpt-6.2-sol",
    "us.openai.gpt-6.2-astra",
];

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 2e-6
}

#[test]
fn the_pinned_table_prices_the_direct_models_per_million_tokens() {
    let p = pinned();
    assert_eq!(p.version, "bedrock-us-west-2-2026-10-05");
    for (model, input, write, read, output) in [
        (SONNET, 2.00, 2.50, 0.20, 10.00),
        ("us.anthropic.claude-opus-5-5", 4.40, 5.50, 0.22, 22.00),
        ("us.openai.gpt-6.1-sol", 2.20, 2.75, 0.11, 11.00),
        ("us.openai.gpt-6-astra", 11.00, 13.75, 1.10, 55.00),
    ] {
        let m = p.price(model).unwrap();
        assert_eq!(
            (m.input, m.cache_write, m.cache_read, m.output),
            (input, write, read, output),
            "{model}"
        );
        assert!(
            m.source.starts_with("http")
                || m.source.contains("Price List")
                || m.source.contains("AmazonBedrock"),
            "{model}: {}",
            m.source
        );
    }
    let u = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_read_input_tokens: 1_000_000,
        cache_write_input_tokens: 1_000_000,
    };
    assert!(close(
        p.usd(SONNET, &u).unwrap(),
        2.00 + 2.50 + 0.20 + 10.00
    ));
    // An invocation log may name a profile by ARN.
    assert_eq!(
        model_of(
            "arn:aws:bedrock:us-west-2:800000000022:inference-profile/global.anthropic.claude-sonnet-5-5"
        ),
        SONNET
    );
    assert!(
        p.usd(
            "arn:aws:bedrock:us-west-2::foundation-model/anthropic.claude-opus-5-5",
            &u
        )
        .is_none(),
        "a foundation-model ARN names the base model, which has no row: only profile ids are priced"
    );
    assert_eq!(p.usd("us.example.unpriced", &u), None);
    assert_eq!(round_usd(0.123_456_789), 0.123457);
}

/// A model with no row counts its tokens at zero USD and is named in `unpricedModels`, which for a supported model would
/// understate the run. The models are the rows of `models.json`, the one list `POST /v1/direct/runs` accepts.
#[test]
fn every_direct_model_is_priced_except_the_ones_named_as_gaps() {
    let p = pinned();
    let ids: Vec<&str> = builtin().models().iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids.len(), 8, "{ids:?}");
    for id in &ids {
        let priced = p.price(id).is_some();
        assert_eq!(
            priced,
            !UNPRICED.contains(id),
            "{id}: add its row to cost/prices.json and take it off UNPRICED (or the reverse)"
        );
    }
    for gap in UNPRICED {
        assert!(ids.contains(&gap), "{gap} is not a direct model any more");
    }
}

#[test]
fn a_price_table_is_usd_per_million_tokens_with_one_non_negative_row_per_model() {
    let row = |id: &str, p: f64| json!({"modelId": id, "input": p, "cacheWrite": 1, "cacheRead": 1, "output": 1, "source": "s"});
    let table = |models: Vec<Value>, unit: &str| {
        json!({"version": "v", "currency": "USD", "unit": unit, "source": "s", "models": models})
            .to_string()
    };
    assert!(PriceTable::parse(&table(vec![row("a", 1.0)], "1M tokens")).is_ok());
    for (t, why) in [
        (table(vec![row("a", 1.0)], "1K tokens"), "USD per 1M tokens"),
        (table(vec![row("a", -1.0)], "1M tokens"), "negative"),
        (
            table(vec![row("a", 1.0), row("a", 2.0)], "1M tokens"),
            "twice",
        ),
        ("{".to_string(), "EOF"),
    ] {
        let e = PriceTable::parse(&t).unwrap_err();
        assert!(e.contains(why), "{e}");
    }
}

fn agent(model: &str) -> AgentItem {
    serde_json::from_value(json!({
        "runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1,
        "runtimeArn": "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-x",
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline",
        "engineVersion": "3.0.71", "transport": "acp", "modelId": model,
        "pool": "direct", "status": "stopped", "version": 1
    }))
    .unwrap()
}

fn turn(n: u32, agent_id: &str, usage: Option<Value>) -> TurnItem {
    let mut v = json!({
        "runId": RUN, "agentId": agent_id, "n": n, "taskTokenCiphertext": "", "startedAt": "2026-10-13T18:00:00Z",
        "rulesDigest": "", "version": 1
    });
    if let Some(u) = usage {
        v["usage"] = u;
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn the_runs_cost_is_its_turns_usage_by_model_and_engine() {
    let usage = json!({
        "inputTokens": 52, "outputTokens": 4613, "cacheReadInputTokens": 200_632, "cacheWriteInputTokens": 41_828
    });
    let agents = vec![agent(SONNET)];
    let turns = vec![turn(1, "direct", Some(usage))];
    let c = run_cost(&agents, &turns, &BTreeMap::new(), pinned()).unwrap();
    assert_eq!(c.prices_version, "bedrock-us-west-2-2026-10-05");
    assert_eq!(
        c.lines
            .iter()
            .map(|l| (l.model_id.as_str(), l.engine))
            .collect::<Vec<_>>(),
        [(SONNET, Engine::CLINE)]
    );
    assert_eq!(
        c.lines[0].usage,
        Usage {
            input_tokens: 52,
            output_tokens: 4613,
            cache_read_input_tokens: 200_632,
            cache_write_input_tokens: 41_828
        }
    );
    // 52 x 2.00 + 41,828 x 2.50 + 200,632 x 0.20 + 4,613 x 10.00, per million tokens.
    assert!(close(c.lines[0].usd, 0.19093), "{}", c.lines[0].usd);
    assert!(close(total_usd(&c), 0.19093), "{}", total_usd(&c));
    assert_eq!(c.turns_without_usage, 0);
    assert!(c.unpriced_models.is_empty());
    // No usage anywhere: no cost yet. A turn without usage or without its agent is counted apart; an unpriced model counts at
    // zero and is named.
    assert_eq!(run_cost(&agents, &[], &BTreeMap::new(), pinned()), None);
    let two = json!({"inputTokens": 1, "outputTokens": 1, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0});
    let t2 = vec![
        turn(1, "direct", None),
        turn(2, "ghost", Some(two.clone())),
        turn(3, "direct", Some(two)),
    ];
    let c = run_cost(&[agent("us.zai.glm-5.3")], &t2, &BTreeMap::new(), pinned()).unwrap();
    assert_eq!(c.turns_without_usage, 2);
    assert_eq!(c.unpriced_models, ["us.zai.glm-5.3"]);
    assert_eq!(c.lines[0].usd, 0.0);
}

/// Claude Fable 5.2 is priced from the model page of its maker: $8 per million input tokens and $40 per million output tokens, a 5-minute cache write at
/// $10 and a cache read at $0.20. The row sits under the catalog id, like every other row.
#[test]
fn b803_claude_fable_5_2_is_priced_from_the_model_page() {
    let p = pinned();
    let price = p.price("global.anthropic.claude-fable-5-2").unwrap();
    assert_eq!(
        (
            price.input,
            price.cache_write,
            price.cache_read,
            price.output
        ),
        (8.0, 10.0, 0.2, 40.0)
    );
    let u = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
    };
    assert_eq!(
        round_usd(p.usd("global.anthropic.claude-fable-5-2", &u).unwrap()),
        48.0
    );
}

// ---- the tokens of the prompts that answered ---------------------------------------------------------------------------------

/// A Turn item as the table holds it, with the conversation summary its session's worker declared.
fn raw_turn(n: u32, agent_id: &str, transcript: Option<Value>) -> Value {
    let mut v = json!({
        "runId": RUN, "agentId": agent_id, "n": n, "taskTokenCiphertext": "", "startedAt": "2026-10-13T18:00:00Z",
        "rulesDigest": "", "version": 1
    });
    if let Some(t) = transcript {
        v["transcript"] = t;
    }
    v
}

/// The cost of a run carries the tokens of the prompts that answered, as the workers of its sessions declared them, added up
/// over exactly the turns whose usage is in the lines of the cost. One of those turns without a declared number, and there is no
/// number: the difference to the total would be wrong.
#[test]
fn b846_answered_tokens_cover_the_turns_of_the_total() {
    use control_api::cost::answered_of_turns;
    let usage = || {
        Some(
            json!({"inputTokens": 1_000_000, "outputTokens": 0, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0}),
        )
    };
    let agents = vec![agent(SONNET)];
    let declared = |pairs: &[(u32, u64)]| -> BTreeMap<(String, u32), u64> {
        pairs
            .iter()
            .map(|(n, tokens)| (("direct".to_string(), *n), *tokens))
            .collect()
    };
    let two = vec![turn(1, "direct", usage()), turn(2, "direct", usage())];
    // TRIGGER PRESENT: the two turns are the total of the cost.
    let c = run_cost(
        &agents,
        &two,
        &declared(&[(1, 813_995), (2, 913_896)]),
        pinned(),
    )
    .unwrap();
    assert_eq!(c.lines[0].usage.input_tokens, 2_000_000);
    assert_eq!(c.turns_without_usage, 0);
    // Two turns with usage and a declared number each: their sum.
    assert_eq!(c.answered_tokens, Some(1_727_891));
    // A turn with usage and no declared number: no number.
    let c = run_cost(&agents, &two, &declared(&[(1, 813_995)]), pinned()).unwrap();
    assert_eq!(c.answered_tokens, None);
    assert_eq!(
        run_cost(&agents, &two, &BTreeMap::new(), pinned())
            .unwrap()
            .answered_tokens,
        None
    );
    // A turn without usage (a lost session) adds nothing to the total and nothing to the sum, also when it declares a number.
    let with_lost = vec![
        turn(1, "direct", None),
        turn(2, "direct", usage()),
        turn(3, "direct", usage()),
    ];
    let c = run_cost(
        &agents,
        &with_lost,
        &declared(&[(1, 5_000), (2, 813_995), (3, 913_896)]),
        pinned(),
    )
    .unwrap();
    assert_eq!(c.turns_without_usage, 1);
    assert_eq!(c.answered_tokens, Some(1_727_891));
    // A turn of an agent that has no Agent item is in no line, so its number is in no sum.
    let with_ghost = vec![turn(1, "direct", usage()), turn(2, "ghost", usage())];
    let mut numbers = declared(&[(1, 813_995)]);
    numbers.insert(("ghost".to_string(), 2), 9);
    let c = run_cost(&agents, &with_ghost, &numbers, pinned()).unwrap();
    assert_eq!(c.answered_tokens, Some(813_995));
    // No turn with usage: no cost at all. A sum past the largest number: no number.
    assert_eq!(
        run_cost(
            &agents,
            &[turn(1, "direct", None)],
            &declared(&[(1, 5)]),
            pinned()
        ),
        None
    );
    let c = run_cost(&agents, &two, &declared(&[(1, u64::MAX), (2, 1)]), pinned()).unwrap();
    assert_eq!(c.answered_tokens, None);
    // The largest number itself is a number.
    let one = vec![turn(1, "direct", usage())];
    let c = run_cost(&agents, &one, &declared(&[(1, u64::MAX)]), pinned()).unwrap();
    assert_eq!(c.answered_tokens, Some(u64::MAX));

    // What the Turn items declare, read from their raw attributes: `transcript.answeredTokens`, by agent and turn. The summary of a
    // worker that predates the member is accepted and gives no number; so do a turn without a summary and a member that is no whole
    // number.
    let raw = vec![
        raw_turn(
            1,
            "direct",
            Some(json!({"lastTranscriptSeq": 2, "entries": [], "answeredTokens": 813_995})),
        ),
        raw_turn(
            2,
            "direct",
            Some(json!({"lastTranscriptSeq": 6, "entries": []})),
        ),
        raw_turn(3, "direct", None),
        raw_turn(
            4,
            "direct",
            Some(json!({"lastTranscriptSeq": 9, "entries": [], "answeredTokens": "many"})),
        ),
        raw_turn(
            5,
            "helper",
            Some(json!({"entries": [], "answeredTokens": 12})),
        ),
    ];
    let numbers = answered_of_turns(&raw);
    assert_eq!(
        numbers,
        BTreeMap::from([
            (("direct".to_string(), 1), 813_995),
            (("helper".to_string(), 5), 12)
        ])
    );
    // An older worker's turn with usage: the cost stands, without a number.
    let c = run_cost(
        &agents,
        &[turn(1, "direct", usage()), turn(2, "direct", usage())],
        &numbers,
        pinned(),
    )
    .unwrap();
    assert_eq!(c.answered_tokens, None);
    assert!(
        serde_json::to_value(&c)
            .unwrap()
            .get("answeredTokens")
            .is_none(),
        "the member is left out when there is no number"
    );
}
