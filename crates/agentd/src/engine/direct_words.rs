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
/// engine that ran, so it lives here.
/// It says that the engine may not be able to answer while the disk is full.
pub fn disk_full_question(engine: &str) -> String {
    let name = named(engine);
    format!(
        "Send a message with `ham say` that asks {name} to delete build output or temporary files ({name} may not be able to answer while the disk is full), or end the run with `ham finish` and keep what exists."
    )
}

/// What the person can do when the engine failed on every route while every route answered a test request, as the question of
/// the card and the end of the notice. It names the engine that ran,
/// so it lives here. The engine may not be able to answer while the machine is in that state.
pub fn engine_fault_question(engine: &str) -> String {
    format!(
        "Send a message with `ham say` ({} may not be able to answer), or end the run with `ham finish` and keep what exists.",
        named(engine)
    )
}

/// The engine that ran, inside a sentence: its display name, or "the engine" when the caller gave no name.
fn named(engine: &str) -> String {
    if engine.trim().is_empty() {
        "the engine".to_string()
    } else {
        display(engine)
    }
}

/// What the person can do when a prompt failed while the session's storage refused writes and its files left room, as the question
/// of the card and the end of the notice. It names the engine that ran (its display name; "The engine" when the caller has none), so it
/// lives here.
pub fn storage_refused_question(engine: &str) -> String {
    let name = if engine.trim().is_empty() {
        "The engine".to_string()
    } else {
        display(engine)
    };
    format!(
        "Send a message with `ham say` to go on once the storage takes writes again ({name} may not be able to answer while it refuses them), or end the run with `ham finish` and keep what exists."
    )
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

/// [`reason_title`] for a run of `engine` (its profile name), with the display name of that engine: the client shows the title
/// of the event as the head line of the question's card. For the default engine it is the same text. A caller that names no engine
/// gets "the engine" in the name's place, as both disk questions say it.
pub fn reason_title_for(reason: NeedsInputReason, engine: &str) -> String {
    let title = reason_title(reason);
    if engine.trim().is_empty() {
        return match title.strip_prefix("Cline") {
            Some(rest) => format!("The engine{rest}"),
            None => title.replace("The Cline process", "The engine's process"),
        };
    }
    title.replace("Cline", &display(engine))
}

/// The profile name of the default engine, for a caller that builds a text with no run at hand and wants the words of [`reason_title`].
pub const DEFAULT_ENGINE: &str = "cline";

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

/// The name of an engine in the words of a notice: the display name of an engine the protocol knows (`OpenCode`), else the profile name as it is.
fn display(engine: &str) -> String {
    harness_protocol::common::Engine::new(engine)
        .map_or_else(|_| engine.to_string(), |e| e.display_name().to_string())
}

/// The notice of a run whose default command time limit could not be placed: the run goes on with the engine's own limit.
pub fn limit_not_placed(engine: &str, limit_s: u64, engine_default_s: u64) -> String {
    format!(
        "This run's command time limit of {limit_s} seconds could not be placed for {}. Commands that set no time limit of their own are stopped after {engine_default_s} seconds.",
        display(engine)
    )
}

/// The notice of a session whose engine did not confirm the command time limit in time. True when the sign is only late and when the
/// limit is not in force. Like the other two it fits the 160 characters the client shows of a notice, for every limit of the setting's range.
pub fn limit_not_confirmed(engine: &str, limit_s: u64, engine_default_s: u64) -> String {
    format!(
        "{} has not confirmed this run's command time limit of {limit_s} seconds. If it is not in force, commands that set no limit of their own stop after {engine_default_s} seconds.",
        display(engine)
    )
}

/// The notice of a command that named no limit and that the engine stopped at another number than the run's limit.
pub fn limit_not_applied(engine: &str, cut_s: u64, limit_s: u64) -> String {
    format!(
        "{} stopped a command after {cut_s} seconds, not after this run's {limit_s}: the command time limit was not applied to it.",
        display(engine)
    )
}
