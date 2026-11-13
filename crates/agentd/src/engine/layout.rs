//! Where the harness puts the files an engine reads (the user's rules, the run's skills and the locked settings), how that place is protected,
//! which settings are locked, which kill switches the launch must carry and where the repository can shadow what was placed.
//! The decisions are per engine and live here; the modules that act (`crate::direct`) ask by engine name and never name an engine.
//!
//! Three protection strategies are named. The engine's own directory is `--config` for one engine and `XDG_CONFIG_HOME` for another, so a
//! strategy says where the harness's files go and who may write there:
//!
//! - [`Protection::RootOwnsConfigDir`]: the engine writes nothing under its config directory, so root owns the whole directory and the
//!   harness's files sit inside it (Cline, unchanged).
//! - [`Protection::SeparateRoot`]: the engine keeps state in its config directory, which therefore stays the engine's. The harness's files
//!   go to a root-owned directory beside it (`harness-config`), and the engine is pointed at that directory by its own config.
//! - [`Protection::InPlace`]: an engine with no arm; the files go inside its config directory as before and nothing is taken from it.

use super::direct::{refuse_symlink, set_mode};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// The environment variable that names the root of the harness's files for the engine: the directory whose `skills/` holds the run's skills and
/// whose `rules/` holds the user's rules, whatever the engine. Every engine's launch carries it; the shipped rules and the lifecycle pack's
/// scanner kit are reached through it.
pub const CONFIG_ROOT_ENV: &str = "HAMH_ENGINE_CONFIG_ROOT";

/// The name of the separate root, a sibling of the engine's config directory.
const SEPARATE_ROOT_NAME: &str = "harness-config";

/// How the place of the harness's files is protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    RootOwnsConfigDir,
    SeparateRoot,
    InPlace,
}

/// The protection strategy of `engine`.
pub fn protection_of(engine: &str) -> Protection {
    match engine {
        "cline" => Protection::RootOwnsConfigDir,
        "opencode" => Protection::SeparateRoot,
        _ => Protection::InPlace,
    }
}

/// Where the harness's files for one engine go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigLayout {
    pub protection: Protection,
    /// The engine's own configuration directory (the engine's `--config`, or what its `XDG_CONFIG_HOME` names).
    pub config_dir: PathBuf,
    /// The root of the harness's files ([`CONFIG_ROOT_ENV`]): the config directory itself, or a separate directory.
    pub root: PathBuf,
    /// The user's rules, one file each.
    pub rules: PathBuf,
    /// The run's skills, one folder each.
    pub skills: PathBuf,
    /// The settings the engine user cannot rewrite.
    pub locked: PathBuf,
}

/// The layout of `engine` for its config directory `config_dir`.
pub fn config_layout(engine: &str, config_dir: &Path) -> ConfigLayout {
    let protection = protection_of(engine);
    let root = match protection {
        Protection::SeparateRoot => config_dir.with_file_name(SEPARATE_ROOT_NAME),
        Protection::RootOwnsConfigDir | Protection::InPlace => config_dir.to_path_buf(),
    };
    ConfigLayout {
        protection,
        config_dir: config_dir.to_path_buf(),
        rules: root.join("rules"),
        skills: root.join("skills"),
        locked: root.join("locked"),
        root,
    }
}

impl ConfigLayout {
    /// The locked global settings file.
    pub fn locked_settings(&self) -> PathBuf {
        self.locked.join("global-settings.json")
    }

    /// Makes the root when it is a directory of its own (mode 0755, never through a link). A layout inside the config directory needs nothing.
    pub fn prepare(&self) -> io::Result<()> {
        if self.protection != Protection::SeparateRoot {
            return Ok(());
        }
        refuse_symlink(&self.root)?;
        std::fs::create_dir_all(&self.root)?;
        set_mode(&self.root, 0o755)
    }
}

/// The variable and the value that tell the engine's tools where the root is.
pub fn config_root_env(layout: &ConfigLayout) -> (&'static str, String) {
    (CONFIG_ROOT_ENV, layout.root.display().to_string())
}

/// Takes the place of the harness's files from the engine user: gives it to root when agentd drops privileges (`owner` is the engine user).
/// Returns whether it did. Cline: the whole config directory (the engine writes nothing there, so a root-owned `rules/` or `locked/` cannot be
/// renamed away). A separate root: the root only, the engine's own directory stays its own. In place: nothing.
pub async fn protect(layout: &ConfigLayout, owner: Option<(u32, u32)>) -> bool {
    if owner.is_none() {
        return false;
    }
    let dir = match layout.protection {
        Protection::RootOwnsConfigDir => &layout.config_dir,
        Protection::SeparateRoot => &layout.root,
        Protection::InPlace => return false,
    };
    super::chown_tree(&[dir.as_path()], (0, 0)).await;
    let _ = set_mode(dir, 0o755);
    true
}

/// Locks the settings the engine user must not change, and names the file in `env` where the engine reads it from. `disabled_tools` is the
/// kill switch for the engine's tools. Returns the locked file, or `None` for an engine whose protections come with its profile (its launch
/// environment) and its emitted config, not from a settings file of its own.
pub fn lock_settings(
    engine: &str,
    layout: &ConfigLayout,
    disabled_tools: &[String],
    env: &mut BTreeMap<String, String>,
) -> io::Result<Option<PathBuf>> {
    match engine {
        "cline" => super::direct::lock_cline_settings(layout, disabled_tools, env),
        _ => Ok(None),
    }
}

// ---- the switches that are off for every engine ------------------------------------------------------------------------------

/// What the launch of an engine must carry so that telemetry, self-update and the like are off: environment variables of the launch, and keys
/// of the locked settings file. The tests prove each against the profile and the locked file; `check_kill_switches` refuses a launch without them.
#[derive(Debug, PartialEq, Eq)]
pub struct KillSwitches {
    pub env: &'static [(&'static str, &'static str)],
    pub locked: &'static [(&'static str, bool)],
}

const CLINE_SWITCHES: KillSwitches = KillSwitches {
    env: &[("CLINE_NO_AUTO_UPDATE", "1")],
    locked: &[("telemetryOptOut", true), ("autoUpdateEnabled", false)],
};

/// OpenCode's switches are the ones the profile sets; whether an environment switch has an effect on a given release is proved by the
/// egress capture, not by this table.
const OPENCODE_SWITCHES: KillSwitches = KillSwitches {
    env: &[
        ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
        ("OPENCODE_DISABLE_CLAUDE_CODE", "1"),
    ],
    locked: &[],
};

/// The fake ACP engine is a test double: it has no telemetry, so nothing is required of its launch.
const FAKE_SWITCHES: KillSwitches = KillSwitches {
    env: &[],
    locked: &[],
};

/// The kill switches of `engine`; `None` for an engine the build has no table for.
pub fn kill_switches(engine: &str) -> Option<&'static KillSwitches> {
    match engine {
        "cline" => Some(&CLINE_SWITCHES),
        "opencode" => Some(&OPENCODE_SWITCHES),
        "fake-acp" => Some(&FAKE_SWITCHES),
        _ => None,
    }
}

/// Refuses a launch that lacks a kill switch of its engine, with the name of the missing one. An engine with no table is refused too: telemetry
/// cannot be shown off for it. `locked_file` is the file [`lock_settings`] wrote.
pub fn check_kill_switches(
    engine: &str,
    env: &BTreeMap<String, String>,
    locked_file: Option<&Path>,
) -> Result<(), String> {
    let k = kill_switches(engine).ok_or_else(|| {
        format!("kill switches: the harness has no table for engine {engine}, so telemetry and self-update cannot be shown off for it")
    })?;
    for (var, want) in k.env {
        match env.get(*var) {
            Some(v) if v == want => {}
            got => {
                return Err(format!(
                    "kill switches: {var} must be {want} in the launch environment of {engine}, it is {}",
                    got.map_or("missing", String::as_str)
                ));
            }
        }
    }
    if k.locked.is_empty() {
        return Ok(());
    }
    let path = locked_file
        .ok_or_else(|| format!("kill switches: the locked settings of {engine} are missing"))?;
    let doc: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or_else(|| format!("kill switches: the locked settings of {engine} cannot be read"))?;
    for (key, want) in k.locked {
        if doc.get(*key) != Some(&Value::Bool(*want)) {
            return Err(format!(
                "kill switches: {key} must be {want} in the locked settings of {engine}"
            ));
        }
    }
    Ok(())
}

// ---- how a placed rule or skill is checked -----------------------------------------------------------------------------------

/// How the harness checks that the engine will load what was placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementCheck {
    /// The engine lists the rules and skills it will load; the preflight compares the listing with the files placed.
    Listing,
    /// The engine has no listing; the files on the disk are checked (`crate::direct::placement`).
    Files,
}

/// The check of `engine`: its own listing when it has one, the files otherwise.
pub fn placement_check(engine: &str) -> PlacementCheck {
    if super::direct::rules_listing_argv(engine, "", Path::new("")).is_some() {
        PlacementCheck::Listing
    } else {
        PlacementCheck::Files
    }
}

/// The folders of the repository (relative to its root) in which a file of the same name as a user rule shadows that rule.
pub fn repo_rule_dirs(engine: &str) -> &'static [&'static str] {
    match engine {
        "cline" => &[".clinerules"],
        _ => &[],
    }
}

/// The folders of the repository (relative to its root) in which a skill folder of the same name as a requested skill shadows it.
pub fn repo_skill_dirs(engine: &str) -> &'static [&'static str] {
    match engine {
        "opencode" => &[".opencode/skills", ".claude/skills", ".agents/skills"],
        _ => &[],
    }
}
