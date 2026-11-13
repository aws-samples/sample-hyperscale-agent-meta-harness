//! What the goal loop needs from an engine: the engine's own goal plugin, the observer plugin that reports each
//! engine run, the setting that fixes the plugin mode, the names of the state and rounds files, and the tool that completes a goal.
//! Everything that names the engine is here, so `crate::direct::goal` stays engine-neutral. `None` means this engine has no goal
//! plugin: the goal module is then off for the run.

use super::tools::{OPENCODE_DELEGATE, QUALIFIED_DELEGATE, TitleStyle};

/// Who drives the goal loop of an engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopKind {
    /// The engine's own goal plugin queues the follow-up prompts and the harness watches (Cline).
    Plugin,
    /// The harness asks again itself after every round (OpenCode): each prompt is a round, the claim is a line with the run's nonce. No plugin is
    /// placed, no state file or rounds log is used, and the plugin fields below are empty. Why OpenCode's own `/goal` is not used, measured on v2.2.6
    /// over `opencode acp`: the command returns at once with no update at all; the loop then runs with nothing on the wire (a second prompt can attach,
    /// but the first step of the loop is never seen); the goal state (active, paused, complete) is only in the engine's own database, so the end of
    /// the loop cannot be told from a pause; and the loop pauses itself after 50 rounds, which is a cap on the work.
    Harness,
}

/// The goal plugin of one engine and the files it and the observer use. Paths are relative to the engine's data directory unless noted.
#[derive(Debug, PartialEq, Eq)]
pub struct GoalEngine {
    /// How the words a person reads name the engine (`Cline`).
    pub label: &'static str,
    /// Plugin folders agentd places under `<configuration directory>/plugins/`: the engine's goal plugin, then the harness's observer.
    pub plugins: &'static [&'static str],
    /// Environment the engine is started with. The plugin mode is set explicitly, so it never depends on whether a bootstrap file sits
    /// beside the binary (a missing one switches the engine to this mode silently, a present one to a worker thread).
    pub env: &'static [(&'static str, &'static str)],
    /// The goal plugin's state file: `pendingGoal` before the engine claims it, `activeGoals` after.
    pub state_file: &'static str,
    /// The observer's log: one JSON line when an engine run starts and one when it ends.
    pub rounds_file: &'static str,
    /// The tool the plugin gives the model to complete the goal; its `rawOutput.completed` is true only when the plugin accepted it. Empty when the
    /// engine has no such tool (the claim is a line in the model's message).
    pub completion_tool: &'static str,
    /// The tool the model uses to start a helper agent. A call of it that has not been answered is a wait for delegated work: the brake
    /// inside a round holds back while one is open. `None` when the engine has no such tool.
    pub delegate_tool: Option<&'static str>,
    /// How the engine writes the title of a tool call: the watch reads the completion and helper-agent calls by it.
    pub title: TitleStyle,
    /// Who drives the loop.
    pub kind: LoopKind,
}

const CLINE: GoalEngine = GoalEngine {
    label: "Cline",
    plugins: &["goal", "harness-rounds"],
    env: &[("CLINE_PLUGIN_MODE", "in_process")],
    state_file: "plugins/goal/goals.json",
    rounds_file: "plugins/harness-rounds/rounds.jsonl",
    completion_tool: "mark_goal_complete",
    delegate_tool: Some(QUALIFIED_DELEGATE),
    title: TitleStyle::NameColonArgs,
    kind: LoopKind::Plugin,
};

/// OpenCode: no plugin and no files; the harness drives the loop. Its native `/goal` is not used (see `crate::direct::goal::harness`).
const OPENCODE: GoalEngine = GoalEngine {
    label: "OpenCode",
    plugins: &[],
    env: &[],
    state_file: "",
    rounds_file: "",
    completion_tool: "",
    delegate_tool: Some(OPENCODE_DELEGATE),
    title: TitleStyle::NameThenCommand,
    kind: LoopKind::Harness,
};

/// The goal plugin of `engine` (the profile name of the request), or `None`.
pub fn goal_engine(engine: &str) -> Option<&'static GoalEngine> {
    match engine {
        "cline" => Some(&CLINE),
        "opencode" => Some(&OPENCODE),
        _ => None,
    }
}
