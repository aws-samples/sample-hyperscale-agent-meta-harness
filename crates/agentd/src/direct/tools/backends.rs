//! The list of tools the server offers. Each tool package adds exactly one line to the list in [`all`] and nothing else, so a
//! conflict between two packages is resolved by keeping every line.

use crate::direct::hooks::{HookEnv, StartupCtx};
use crate::direct::tools::backend::ToolBackend;
use crate::direct::tools::settings::Settings;
use std::collections::BTreeSet;

// Each tool's backend is a file of this directory that the package declares here, so that `mod.rs` is not touched by it. The
// declaration may move to `mod.rs` once the packages are merged; the path `agentd::direct::tools::backends::rg` is then `tools::rg`.
/// `code_search` and its path confinement.
#[path = "rg.rs"]
pub mod rg;

/// The backends of this run: one per tool of the table, minus the tools `AGENTD_TOOLS_DISABLE` names. A backend that cannot work in this run
/// (a search with its route absent) is left out by its own line; the server lists only what it is given.
pub fn all(_env: &HookEnv, _startup: &StartupCtx<'_>) -> Vec<Box<dyn ToolBackend>> {
    let settings = Settings::from_env();
    let list: Vec<Box<dyn ToolBackend>> = vec![
        // one line per tool, in the order of the table (`harness_protocol::tools::TOOLS`)
        Box::new(rg::CodeSearch::new().with_limits(rg::Limits {
            wall: settings.search_wall,
            ..rg::Limits::default()
        })),
    ];
    retain_enabled(list, &settings)
}

/// `list` without the tools the settings disable and without a second backend for a tool that already has one (the first stays).
pub fn retain_enabled(
    list: Vec<Box<dyn ToolBackend>>,
    settings: &Settings,
) -> Vec<Box<dyn ToolBackend>> {
    let mut seen = BTreeSet::new();
    list.into_iter()
        .filter(|b| settings.allows(b.name().as_str()) && seen.insert(b.name()))
        .collect()
}
