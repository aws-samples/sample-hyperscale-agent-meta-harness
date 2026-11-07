//! The settings of the tool server. Every one is an `AGENTD_TOOLS_*` environment variable, read the way the
//! research module reads its own (`config.rs` is not edited, `src/direct/AGENTS.md`).
//!
//! `AGENTD_TOOLS_MODE` is the image's kill switch. The stage deploy always writes a concrete word,
//! so an unset variable only happens in tests and local runs, and there it leaves the module on, like the research module. A word
//! that is neither an off word nor an on word is read as off: a typo in a kill switch must not leave the thing running, and the module says so.

use std::collections::BTreeSet;

/// `AGENTD_TOOLS_MODE`: `off`, `false`, `0` or `no` (any case) switch the module off; `on`, `true`, `1` or `yes` switch it on; unset or empty
/// leaves it on; any other word switches it off ([`Mode::Unrecognized`]).
pub const ENV_MODE: &str = "AGENTD_TOOLS_MODE";
/// `AGENTD_TOOLS_DISABLE`: a comma list of bare tool names the server does not list (`semantic_search,web_search`).
pub const ENV_DISABLE: &str = "AGENTD_TOOLS_DISABLE";
/// `AGENTD_TOOLS_PORT`: the loopback port of the server; 0 asks for any free port.
pub const ENV_PORT: &str = "AGENTD_TOOLS_PORT";
/// The loopback port of the server (never 3128, the package proxy's, nor 3129, the research API's).
pub const DEFAULT_PORT: u16 = 3130;

/// What the mode variable said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Unset or empty: on.
    Default,
    /// An on word.
    On,
    /// An off word.
    Off,
    /// A word that is neither: off.
    Unrecognized,
}

/// What the module needs to know that is not in the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub mode: Mode,
    pub port: u16,
    /// Bare tool names (lower case) the server leaves out of its list.
    pub disabled: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: Mode::Default,
            port: DEFAULT_PORT,
            disabled: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// The settings of the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(&|k| std::env::var(k).ok())
    }

    /// The settings `get` names (a test passes a map).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let mode = match get(ENV_MODE)
            .map(|m| m.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") => Mode::Default,
            Some("on" | "true" | "1" | "yes") => Mode::On,
            Some("off" | "false" | "0" | "no") => Mode::Off,
            Some(_) => Mode::Unrecognized,
        };
        let port = get(ENV_PORT)
            .and_then(|p| p.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let disabled = get(ENV_DISABLE)
            .map(|list| {
                list.split(',')
                    .map(|n| n.trim().to_ascii_lowercase())
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            enabled: matches!(mode, Mode::Default | Mode::On),
            mode,
            port,
            disabled,
        }
    }

    /// Whether the tool `name` may be listed.
    pub fn allows(&self, name: &str) -> bool {
        !self.disabled.contains(name)
    }
}
