//! The window of the Cline rows of GPT-6.1 Sol and GPT-6 Astra. The Cline engine takes the row's `contextTokens` as the most input it may
//! send and shortens a conversation at nine tenths of it. Bedrock accepted an input of 921,663 tokens for both models and refused 921,993, with
//! the output limit the engine asks for. The rows said 1,000,000 and 1,050,000, so the engine would have shortened a conversation at 900,000
//! and at 945,000 tokens: a run on GPT-6 Astra was refused before that point, and a run on GPT-6.1 Sol had about 21,700 tokens of room. Both
//! rows carry 920,000. The OpenCode rows keep the maker's figures: that engine is told the window and the output limit as two figures.
#![allow(clippy::unwrap_used)]

use ham_core::models::{self, Model, Models};
use harness_protocol::common::Engine;

const SOL_61: &str = "us.openai.gpt-6.1-sol";
const ASTRA_6: &str = "us.openai.gpt-6-astra";
/// The largest input Bedrock accepted for each of the two models, and the smallest it refused, in tokens (measured on both of their routes).
const ACCEPTED: u32 = 921_663;
const REFUSED: u32 = 921_993;
/// The share of the window at which the Cline engine shortens a conversation.
const COMPACT_AT: f64 = 0.9;

fn row<'a>(list: &'a Models, id: &str) -> &'a Model {
    list.models().iter().find(|m| m.id == id).unwrap()
}

#[test]
fn b864_a_cline_run_on_the_two_earlier_openai_models_shortens_its_conversation_before_bedrocks_input_limit()
 {
    for id in [SOL_61, ASTRA_6] {
        let m = row(models::builtin(), id);
        assert_eq!(m.context_tokens, 920_000, "{id}");
        assert!(m.context_tokens <= ACCEPTED && ACCEPTED < REFUSED);
        let trigger = f64::from(m.context_tokens) * COMPACT_AT;
        // A whole turn of room between the point where the engine shortens the conversation and the input Bedrock refuses.
        assert!(
            f64::from(ACCEPTED) - trigger > 90_000.0,
            "{id}: the engine shortens at {trigger} tokens"
        );
    }
    // The measured output limits of the rows are not touched.
    assert_eq!(row(models::builtin(), SOL_61).max_output_tokens, 131_072);
    assert_eq!(row(models::builtin(), ASTRA_6).max_output_tokens, 128_000);
}

#[test]
fn b864_a_switch_to_one_of_the_two_models_carries_the_same_window() {
    // A run that fails over to one of the two models is given the row's window, on both of the model's profiles.
    let routes = models::builtin().failover_routes("us.anthropic.claude-opus-5-5");
    let mut seen = 0;
    for r in routes
        .iter()
        .filter(|r| r.id.ends_with("openai.gpt-6.1-sol") || r.id.ends_with("openai.gpt-6-astra"))
    {
        assert_eq!(r.context_tokens, 920_000, "{}", r.id);
        seen += 1;
    }
    assert_eq!(
        seen, 4,
        "both profiles of both models are in the list of a Claude Opus 5.5 run"
    );
}

#[test]
fn b864_the_opencode_rows_of_the_two_models_keep_their_figures() {
    let list = models::builtin_catalog()
        .for_engine(Engine::OPENCODE)
        .unwrap();
    let sol = row(list, SOL_61);
    let astra = row(list, ASTRA_6);
    assert_eq!(
        (sol.context_tokens, sol.max_output_tokens),
        (1_000_000, 131_072)
    );
    assert_eq!(
        (astra.context_tokens, astra.max_output_tokens),
        (1_050_000, 128_000)
    );
}
