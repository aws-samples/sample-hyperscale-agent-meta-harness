//! COMPAT-CAH-DATA: the variables of the engine's environment under their earlier prefix (ends at VERSION 6.00).
//!
//! The rename gave the variables the prefix `HAMH_`. Rules and skills the user edited before it name the earlier spellings (`$CAH_ATTACHMENTS_DIR`,
//! `$CAH_RESEARCH_URL`, and the others), and the default text of that release names them too. agentd sets both spellings of every variable it hands
//! the engine, with the same value, so a rules file that is kept as the user wrote it keeps working. At the removal version the second spelling goes
//! (this file with it), and the test that asserts both (`tests/direct_research_hook.rs`, `direct_attachments.rs`, `direct_tools_register.rs`) is
//! changed in the same commit.

use std::collections::BTreeMap;

/// The prefix of the variables handed to the engine.
pub const PREFIX: &str = "HAMH_";
/// The earlier prefix of the same variables.
pub const EARLIER_PREFIX: &str = "CAH_";

/// Sets `HAMH_<suffix>` and `CAH_<suffix>` to `value`.
pub fn set_both(env: &mut BTreeMap<String, String>, suffix: &str, value: String) {
    env.insert(format!("{EARLIER_PREFIX}{suffix}"), value.clone());
    env.insert(format!("{PREFIX}{suffix}"), value);
}
