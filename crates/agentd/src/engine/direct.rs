//! The engine-specific parts of a direct run: what an engine says about the rules it
//! will load, and the launch setting that switches a tool off. The generic driver (`crate::direct`) asks by engine name and
//! treats `None` as "this engine has no such thing".

use super::layout::{
    ConfigLayout, Protection, config_layout, lock_settings, protect, protection_of,
};
use super::user_settings::{CommandSeconds, Effort, UserSettings};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// How the words a person reads in a run's events name the engine (`Cline`). `agentd` outside `src/engine/` names no engine,
/// so every event text that names the engine takes the name from here; an engine this build has no name for is "the engine".
pub fn engine_label(engine: &str) -> &'static str {
    match engine {
        "cline" => "Cline",
        _ => "the engine",
    }
}

/// Where the user's rules go: the engine's global rules directory, inside its configuration directory.
pub fn user_rules_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("rules")
}

/// Where the run's skills go: the engine's skills directory, inside its configuration directory.
pub fn user_skills_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("skills")
}

/// The command that lists the rules the engine will load for its current directory, as JSON. It takes `--config` and
/// refuses `--data-dir`. `None` for an engine that has no such listing (the run then has no rules preflight).
pub fn rules_listing_argv(engine: &str, program: &str, config_dir: &Path) -> Option<Vec<String>> {
    match engine {
        "cline" => Some(vec![
            program.to_string(),
            "config".to_string(),
            "rules".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            config_dir.display().to_string(),
        ]),
        _ => None,
    }
}

/// The arguments a direct run adds to the engine's launch from its profile. The profile is shared with the old line, which
/// answers the engine's permission requests itself, so a flag that stops those requests belongs to the direct launch only.
/// Cline gets `--auto-approve true` (the microVM is the boundary; no permission request reaches the client), placed after
/// `--acp` (else right after the program); a profile that already names the flag is left as it is. The reasoning level of the launch
/// (protocol 1.39.0, `reasoning.effort`; since 1.47.0 the user's own level, else the level of the model spec, see
/// `UserSettings::reasoning_launch`) is `--thinking <level>` at the end, as the probe records of package launched it. It is added only
/// when `effort` is a level; a level the profile already names is replaced, so the flag is named once and a second pass adds nothing. `off`
/// adds none and also takes out one the profile names; `default` leaves the profile as it is.
pub fn direct_launch_argv(engine: &str, mut argv: Vec<String>, effort: Effort) -> Vec<String> {
    if engine == "cline" && !argv.is_empty() {
        if !argv.iter().any(|a| a == "--auto-approve") {
            let at = argv
                .iter()
                .position(|a| a == "--acp")
                .map_or(1, |i| i + 1)
                .min(argv.len());
            argv.splice(at..at, ["--auto-approve".to_string(), "true".to_string()]);
        }
        if effort.flag().is_some() || effort == Effort::Off {
            while let Some(at) = argv.iter().position(|a| a == "--thinking") {
                argv.remove(at);
                if at < argv.len() {
                    argv.remove(at);
                }
            }
        }
        if let Some(level) = effort.flag() {
            argv.extend(["--thinking".to_string(), level.to_string()]);
        }
    }
    argv
}

/// Whether the engine has the reasoning flag the launch places (`--thinking`): Cline only. For another engine nothing is placed and nothing recorded.
pub fn has_reasoning_flag(engine: &str) -> bool {
    engine == "cline"
}

/// The engine's command time limit variable (milliseconds; fork patch 0015: 1,000 to 1,800,000, default 600,000).
pub const TIMEOUT_ENV: &str = "CLINE_RUN_COMMANDS_TIMEOUT_MS";

/// The second engine's command time limit variable (milliseconds). Its shell tool stops a command after two minutes unless this says
/// otherwise.
pub const OPENCODE_TIMEOUT_ENV: &str = "OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS";

/// The command time limit a launch of `engine` carries and the variable that holds it. Cline's fork has ten minutes as its own default, so its
/// variable is there only for a limit the user set, and a run with none has the environment it had. OpenCode's own default is two minutes, so its
/// launch always carries the variable: the user's limit, else [`CommandSeconds::DEFAULT`], the ten minutes Cline's runs end up with. `None` for an
/// engine with no such variable, and for Cline while the user set no limit.
pub fn command_limit(
    engine: &str,
    settings: &UserSettings,
) -> Option<(&'static str, CommandSeconds)> {
    match engine {
        "cline" => settings.command_timeout.map(|limit| (TIMEOUT_ENV, limit)),
        "opencode" => Some((
            OPENCODE_TIMEOUT_ENV,
            settings
                .command_timeout
                .unwrap_or_else(CommandSeconds::default_limit),
        )),
        _ => None,
    }
}

/// The environment variables the engine's launch gets from the user's engine settings: the command time limit, in milliseconds, in the variable
/// [`command_limit`] names. For Cline nothing is added for a setting the user did not make.
pub fn launch_env(engine: &str, settings: &UserSettings) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some((var, limit)) = command_limit(engine, settings) {
        env.insert(var.to_string(), limit.millis().to_string());
    }
    env
}

/// One row of the engine's rules listing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ListedRule {
    /// The rule's name: the file stem for a rules-directory file.
    pub name: String,
    /// The text, trimmed by the listing.
    pub instructions: String,
    /// The file the engine read it from.
    pub path: String,
}

/// Reads the rules listing: a JSON array of `{name, instructions, path}`. A line of text before the array (a notice the CLI printed)
/// is skipped. The error names the shape that was expected and quotes none of the output.
pub fn parse_rules_listing(stdout: &str) -> Result<Vec<ListedRule>, String> {
    parse_listing(stdout, "rules")
}

/// The command that lists the skills the engine will load for its current directory and its HOME, as JSON. Like the rules listing it takes
/// `--config` and refuses `--data-dir`. `None` for an engine with no such listing (the run then has no skills preflight).
pub fn skills_listing_argv(engine: &str, program: &str, config_dir: &Path) -> Option<Vec<String>> {
    match engine {
        "cline" => Some(vec![
            program.to_string(),
            "config".to_string(),
            "skills".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            config_dir.display().to_string(),
        ]),
        _ => None,
    }
}

/// One row of the engine's skills listing (read from the pinned engine, 2026-10-16): the name comes from the frontmatter, `instructions` is
/// the text after the frontmatter trimmed at both ends and otherwise as the file has it (line endings included), `path` is the absolute path
/// of the SKILL.md. A skill with no description has none.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ListedSkill {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub instructions: String,
    #[serde(default)]
    pub frontmatter: Value,
    pub path: String,
}

/// Reads the skills listing: a JSON array of [`ListedSkill`], with an optional line of text before it. The error names the shape that was
/// expected and quotes none of the output.
pub fn parse_skills_listing(stdout: &str) -> Result<Vec<ListedSkill>, String> {
    parse_listing(stdout, "skills")
}

/// A listing is a JSON array; `what` names it in the error ("rules", "skills").
fn parse_listing<T: serde::de::DeserializeOwned>(
    stdout: &str,
    what: &str,
) -> Result<Vec<T>, String> {
    let bad = || format!("the {what} listing is not a JSON array of {{name, instructions, path}}");
    let trimmed = stdout.trim();
    if let Ok(rows) = serde_json::from_str::<Vec<T>>(trimmed) {
        return Ok(rows);
    }
    let start = trimmed
        .match_indices('[')
        .map(|(i, _)| i)
        .find(|i| *i == 0 || trimmed.as_bytes()[*i - 1] == b'\n')
        .ok_or_else(bad)?;
    serde_json::from_str::<Vec<T>>(&trimmed[start..]).map_err(|_| bad())
}

/// The directory (inside the config directory) that holds the launch settings the engine user cannot rewrite.
const LOCKED_DIR: &str = "locked";

/// The locked global settings file ([`lock_global_settings`] writes it; `CLINE_GLOBAL_SETTINGS_PATH` names it).
pub fn locked_settings_path(config_dir: &Path) -> PathBuf {
    config_dir.join(LOCKED_DIR).join("global-settings.json")
}

/// True while agentd runs as root (the microVM); tests and the Mac run as a user and skip the ownership changes.
pub(crate) fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

/// Whether a file is given to root: agentd runs as root and does not run in the `landlock` mode. There the engine shares agentd's user and the
/// container allows no change of owner, so the protection of these files is the Landlock domain's, and the change is skipped on purpose
/// and not left to fail.
fn ownership_wanted(root: bool) -> bool {
    root && crate::sandbox::ownership_changes_allowed()
}

/// Gives `path` to root (a link itself, never what it points at) when agentd runs as root.
pub(crate) fn own_by_root(path: &Path) -> io::Result<()> {
    if ownership_wanted(running_as_root()) {
        std::os::unix::fs::lchown(path, Some(0), Some(0))?;
    }
    Ok(())
}

/// An error for a symbolic link where a protected directory belongs: root would otherwise change modes or write behind it.
pub(crate) fn refuse_symlink(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::other(format!(
            "{} is a symbolic link",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether the engine's configuration directory can belong to root for the whole run. The engine writes nothing under its
/// `--config` directory, so root can own it: the
/// engine user then cannot rename, replace or remove `rules/` and `locked/` (a root-owned child does not stop the owner of
/// its parent from renaming it away). An engine that keeps state in its config directory stays writable; its protection is another
/// strategy (`super::layout::Protection`).
pub fn config_is_read_only(engine: &str) -> bool {
    protection_of(engine) == Protection::RootOwnsConfigDir
}

/// The Landlock grant of the engine's own configuration directory: read only for an engine that writes nothing there (Cline: root owns the directory) and
/// for an engine with no arm (as it always was), read and write for an engine that keeps state in it and finds the harness's files in a root-owned
/// directory beside it ([`Protection::SeparateRoot`]) (OpenCode makes `opencode`
/// under its configuration directory at start; first live Instances run, 2026-11-08: "EACCES ... mkdir '.../direct/config/opencode'").
pub fn grant_config_dir(
    policy: crate::sandbox::policy::Policy,
    engine: &str,
    config_dir: &Path,
) -> crate::sandbox::policy::Policy {
    if protection_of(engine) == Protection::SeparateRoot {
        policy.with_read_write(config_dir)
    } else {
        policy.with_read_only(config_dir)
    }
}

/// Takes the place of the harness's files from the engine user when agentd drops privileges (`owner` is the engine user): by the strategy
/// of the engine ([`protect`]). Returns whether it did.
pub async fn protect_config(engine: &str, config_dir: &Path, owner: Option<(u32, u32)>) -> bool {
    protect(&config_layout(engine, config_dir), owner).await
}

/// Locks the engine's global settings (the per-engine hook is [`lock_settings`]; this is the entry point for a caller that holds the engine
/// name and its config directory). Returns the locked file, or `None` for an engine with no such settings file.
pub fn lock_global_settings(
    engine: &str,
    config_dir: &Path,
    disabled_tools: &[String],
    env: &mut BTreeMap<String, String>,
) -> io::Result<Option<PathBuf>> {
    lock_settings(
        engine,
        &config_layout(engine, config_dir),
        disabled_tools,
        env,
    )
}

/// Cline's locked settings: moves the engine's global settings into a read-only directory and names it in `env`, with `disabledTools` set to
/// `disabled_tools` (an empty list leaves every tool on: sub-agents stay on as in the core engine, and the list is the kill
/// switch). A file in a directory the engine owns could be replaced by the engine, and a new session reads it again
/// (reviewer probe 2026-10-13); the directory is the part that protects it.
pub(crate) fn lock_cline_settings(
    layout: &ConfigLayout,
    disabled_tools: &[String],
    env: &mut BTreeMap<String, String>,
) -> io::Result<Option<PathBuf>> {
    let original = layout.config_dir.join("global-settings.json");
    let dir = layout.locked.clone();
    let target = layout.locked_settings();
    refuse_symlink(&dir)?;
    // The settings the writer emitted (or an earlier lock left), as the base.
    let mut doc: Value = [&original, &target]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "telemetryOptOut": true, "autoUpdateEnabled": false }));
    doc["telemetryOptOut"] = json!(true);
    doc["autoUpdateEnabled"] = json!(false);
    doc["disabledTools"] = json!(disabled_tools);
    make_writable(&dir)?;
    let _ = std::fs::remove_file(&target);
    std::fs::create_dir_all(&dir)?;
    let mut text = serde_json::to_string_pretty(&doc).map_err(io::Error::other)?;
    text.push('\n');
    std::fs::write(&target, text)?;
    set_mode(&target, 0o444)?;
    set_mode(&dir, 0o555)?;
    own_by_root(&target)?;
    own_by_root(&dir)?;
    let _ = std::fs::remove_file(&original);
    env.insert(
        "CLINE_GLOBAL_SETTINGS_PATH".to_string(),
        target.display().to_string(),
    );
    Ok(Some(target))
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Lets agentd replace what a read-only directory holds (an earlier attempt's lock); a directory that is not there is fine.
fn make_writable(dir: &Path) -> io::Result<()> {
    match set_mode(dir, 0o755) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::ownership_wanted;
    use crate::sandbox::{Mode, note_process_mode};

    /// The change of owner is skipped on purpose in the `landlock` mode, not left to fail where the container allows no change. This
    /// is the only test of the library that sets the process mode.
    #[test]
    fn a_landlock_process_never_gives_a_file_to_root_and_a_uid_process_does_when_it_is_root() {
        note_process_mode(Mode::Landlock);
        assert!(!ownership_wanted(true));
        assert!(!ownership_wanted(false));
        note_process_mode(Mode::Uid);
        assert!(ownership_wanted(true));
        assert!(!ownership_wanted(false));
    }
}
