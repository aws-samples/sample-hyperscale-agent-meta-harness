//! The agent table of the dashboard: the agents of the run's view, grouped
//! Working and Completed.
//!
//! An agent's group comes from harness events only: the run is Completed once its status is
//! `complete`, and Working before that. The direct line runs one agent, `direct`.

use crate::state::RunView;
use harness_protocol::goal::GoalStatus;

/// The dashboard's agent groups, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentGroup {
    Working,
    Completed,
}

pub const GROUPS: [AgentGroup; 2] = [AgentGroup::Working, AgentGroup::Completed];

impl AgentGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentGroup::Working => "working",
            AgentGroup::Completed => "completed",
        }
    }

    /// The group heading on the dashboard.
    pub fn title(self) -> &'static str {
        match self {
            AgentGroup::Working => "Working",
            AgentGroup::Completed => "Completed",
        }
    }
}

/// The dashboard group of the run's agents.
pub fn group_of(v: &RunView) -> AgentGroup {
    if v.status == Some(GoalStatus::Complete) {
        AgentGroup::Completed
    } else {
        AgentGroup::Working
    }
}

/// One row of the agent table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub agent_id: String,
    pub engine: Option<String>,
    pub model_id: Option<String>,
    pub group: AgentGroup,
    pub turns: u32,
    /// The latest summary.
    pub last: String,
}

/// The agents the view shows, ordered by group, then id.
pub fn rows(v: &RunView) -> Vec<AgentRow> {
    let mut out: Vec<AgentRow> = v
        .agents
        .values()
        .map(|a| AgentRow {
            agent_id: a.agent_id.clone(),
            engine: a.engine.clone(),
            model_id: a.model_id.clone(),
            group: group_of(v),
            turns: a.turns,
            last: a.last_summary.clone(),
        })
        .collect();
    out.sort_by(|a, b| (a.group, &a.agent_id).cmp(&(b.group, &b.agent_id)));
    out
}

/// Rows per group, in group order, groups with rows only.
pub fn group_counts(rows: &[AgentRow]) -> Vec<(AgentGroup, usize)> {
    GROUPS
        .into_iter()
        .map(|g| (g, rows.iter().filter(|r| r.group == g).count()))
        .filter(|(_, n)| *n > 0)
        .collect()
}
