//! What the goal loop needs from an engine: the engine's own goal plugin, the observer plugin that reports each
//! engine run, the setting that fixes the plugin mode, the names of the state and rounds files, and the tool that completes a goal.
//! Everything that names the engine is here, so `crate::direct::goal` stays engine-neutral. `None` means this engine has no goal
//! plugin: the goal module is then off for the run.

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
    /// The tool the plugin gives the model to complete the goal; its `rawOutput.completed` is true only when the plugin accepted it.
    pub completion_tool: &'static str,
    /// The tool the model uses to start a helper agent. A call of it that has not been answered is a wait for delegated work: the brake
    /// inside a round holds back while one is open. `None` when the engine has no such tool.
    pub delegate_tool: Option<&'static str>,
}

const CLINE: GoalEngine = GoalEngine {
    label: "Cline",
    plugins: &["goal", "harness-rounds"],
    env: &[("CLINE_PLUGIN_MODE", "in_process")],
    state_file: "plugins/goal/goals.json",
    rounds_file: "plugins/harness-rounds/rounds.jsonl",
    completion_tool: "mark_goal_complete",
    delegate_tool: Some("spawn_agent"),
};

/// The goal plugin of `engine` (the profile name of the request), or `None`.
pub fn goal_engine(engine: &str) -> Option<&'static GoalEngine> {
    match engine {
        "cline" => Some(&CLINE),
        _ => None,
    }
}
