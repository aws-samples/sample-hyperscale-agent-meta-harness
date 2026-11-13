//! The tool-idle limit of an engine: how long a tool call may run with the engine sending no line at all before the driver
//! cancels the prompt and treats the engine as unresponsive. The limit is generic (`acp::Activity`, the driver); the default per engine is
//! this table. An engine with no entry has no limit, so its runs are what they were. `AGENTD_DIRECT_TOOL_IDLE_S` replaces the default for
//! every engine; `0` switches the limit off.

use super::opencode;
use std::time::Duration;

/// The longest limit: a day, which is the longest run.
pub const MAX_TOOL_IDLE_S: u64 = 86_400;

/// The limit for `engine`: the setting when there is one (`0` is none), else the engine's own default.
pub fn tool_idle(engine: &str, setting: Option<u64>) -> Option<Duration> {
    match setting {
        Some(0) => None,
        Some(n) => Some(Duration::from_secs(n.min(MAX_TOOL_IDLE_S))),
        None => default_of(engine),
    }
}

fn default_of(engine: &str) -> Option<Duration> {
    match engine {
        opencode::ENGINE => Some(Duration::from_secs(opencode::TOOL_IDLE_S)),
        _ => None,
    }
}
