//! The words of the user correction path that name the engine: the plain title and explanation of each reason a run
//! asks for input, the opening line of the prompt a fresh session starts from, and the title of the digest section that holds the
//! engine's last message. They live here, and not in `direct::input`, because the core names no engine.
//! The client shows the same titles and explanations (`ham_core::input::reason_words`).
//!
//! Nothing here says whether the work is good: a reason is a fact about how the stop looked.

use harness_protocol::direct::NeedsInputReason;

/// The first line of the prompt a fresh session starts from.
pub const NEW_SESSION_OPENING: &str = "You are continuing a task in this work tree. An earlier Cline session on this task stopped before the work was done.";

/// The title of section 6 of the digest.
pub const LAST_MESSAGE_TITLE: &str = "Cline's last message";

/// What the person can do when the session disk is full, as the question of the card and the end of the notice. It names the
/// engine, so it lives here. It says that the engine may not be able to answer while the disk is full.
pub fn disk_full_question() -> &'static str {
    "Send a message with `ham say` that asks Cline to delete build output or temporary files (Cline may not be able to answer while the disk is full), or end the run with `ham finish` and keep what exists."
}

/// What the person can do when the engine failed on every route while every route answered a test request, as the question of
/// the card and the end of the notice. It names the engine, so it lives here. The engine may not be
/// able to answer while the machine is in that state.
pub fn engine_fault_question() -> &'static str {
    "Send a message with `ham say` (Cline may not be able to answer), or end the run with `ham finish` and keep what exists."
}

/// The plain title of a reason (at most 120 characters).
pub fn reason_title(reason: NeedsInputReason) -> &'static str {
    use NeedsInputReason as R;
    match reason {
        R::EngineStop => "Cline stopped its own prompt",
        R::NoChanges => "Cline ended without changing files",
        R::NoMessage => "Cline sent an empty message",
        R::OutputLimit => "The model reached its output limit",
        R::RequestLimit => "Cline reached its request limit",
        R::Refused => "The answer was refused or blocked",
        R::ReplayRejected => "The provider rejected the saved conversation",
        R::ContextFull => "The conversation exceeds the context window",
        R::ProviderError => "The model provider failed",
        R::CapHit => "Automatic recovery reached its prompt cap",
        R::EngineDied => "The Cline process stopped",
        R::Silence => "Cline stopped sending output",
        R::OtherStop => "Cline gave an unknown stop reason",
        R::ProviderOutage => "Every model route is down",
    }
}

/// One or two plain sentences saying what the stop was (the same words as the card in the client).
pub fn reason_explain(reason: NeedsInputReason) -> &'static str {
    use NeedsInputReason as R;
    match reason {
        R::EngineStop => {
            "Cline ended the prompt itself, twice. Its guard stops after 5 identical tool calls; its mistake limit stops after 6 failed turns. The provider is fine. Both counters belong to the session: another message in the same session stops again after one repeated call, and a fresh session starts them from zero."
        }
        R::NoChanges => {
            "Cline ended normally with a message, but no file differs from the start. Read its message: it may answer a question or give up on an impossible task."
        }
        R::NoMessage => "Cline ended its turn with an empty message. You can send another prompt.",
        R::OutputLimit => {
            "The model reached its maximum output length for one answer. You can send another prompt."
        }
        R::RequestLimit => {
            "Cline reached its model-request limit for one prompt. You can start a fresh session."
        }
        R::Refused => {
            "The model refused, or the provider's filter blocked the answer. You can change your prompt."
        }
        R::ReplayRejected => {
            "The harness already tried a fresh session once. More prompts in the same session fail the same way; choose a fresh session."
        }
        R::ContextFull => {
            "The conversation no longer fits the model's context window. The harness already tried a fresh session once; you can request another."
        }
        R::ProviderError => {
            "Retries and route switches could not fix the provider failure. You can finish with the existing work."
        }
        R::CapHit => {
            "The harness stopped after 12 prompts in a row without a new tool call or changed file. You can supply another prompt."
        }
        R::EngineDied => {
            "The automatic fresh session did not help. Continuing is impossible; a fresh session starts a new process on the same files."
        }
        R::Silence => {
            "Cline sent no output for 15 minutes, again after one resume. The process may be stuck; a fresh session starts a new process on the same files."
        }
        R::OtherStop => {
            "The harness does not know Cline's stop reason. You can finish with the existing work."
        }
        R::ProviderOutage => {
            "All routes have been down for 30 minutes. No answer is needed; probing continues until the time limit. `ham cancel` stops the run and delivers what exists."
        }
    }
}
