//! The rules preflight: before any model call the driver asks the engine which rules it will
//! load for the repository root and compares that list with the files it placed. A user rule that is missing, that reads
//! differently, or that is shadowed by a repository file of the same name ends the run `stopped_setup_failed`: the user's
//! rules are the part of the run the harness promises to deliver.

use crate::direct::request::sha256_hex;
use crate::direct::rules_place::PlacedRule;
use crate::engine::LaunchSpec;
use crate::engine::direct::{ListedRule, parse_rules_listing};
use std::path::Path;
use std::time::Duration;

/// How long the listing command may take.
pub const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(60);

fn same_path(a: &str, b: &Path) -> bool {
    if Path::new(a) == b {
        return true;
    }
    match (Path::new(a).canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Passes when every placed rule is in the listing at its own path with the same text, and no listed rule of the same name is
/// read from anywhere else. A listing names a rules-directory file by its stem or by its file name.
pub fn check_rules_listing(placed: &[PlacedRule], listing: &[ListedRule]) -> Result<(), String> {
    for p in placed {
        let stem = p.name.strip_suffix(".md").unwrap_or(&p.name);
        let named = |r: &&ListedRule| r.name == p.name || r.name == stem;
        if let Some(other) = listing
            .iter()
            .filter(named)
            .find(|r| !same_path(&r.path, &p.path))
        {
            return Err(format!(
                "rules preflight: {} is shadowed by a rule of the same name loaded from {}",
                p.name,
                crate::direct::classify::scrub_detail(&other.path)
            ));
        }
        match listing.iter().find(|r| same_path(&r.path, &p.path)) {
            None => {
                return Err(format!(
                    "rules preflight: {} is missing from the rules the engine will load",
                    p.name
                ));
            }
            Some(r) if sha256_hex(r.instructions.trim().as_bytes()) != p.text_sha256 => {
                return Err(format!(
                    "rules preflight: {} differs from the file placed",
                    p.name
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Runs the listing command as launched (`spec`), bounded by [`PREFLIGHT_TIMEOUT`], and returns its output.
pub async fn run_listing(spec: &LaunchSpec) -> Result<Vec<ListedRule>, String> {
    let argv = crate::engine::command_argv(spec).map_err(|e| format!("rules preflight: {e}"))?;
    let joined = std::env::join_paths(&spec.path).map_err(|e| format!("rules preflight: {e}"))?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .env("PATH", joined)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(PREFLIGHT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "rules preflight: the listing did not finish in 60 s".to_string())?
        .map_err(|e| format!("rules preflight: the listing did not start: {e}"))?;
    if !out.status.success() {
        let mut why = format!("rules preflight: the listing exited with {}", out.status);
        // The launcher of a Landlock sandbox says why it did not start the program; nothing else of the listing's stderr is kept.
        if spec.sandbox.is_some()
            && out.status.code() == Some(i32::from(crate::sandbox::REFUSED_EXIT))
            && let Some(line) = String::from_utf8_lossy(&out.stderr).lines().find(|l| {
                l.starts_with("agentd sandbox-exec: refused")
                    || l.starts_with("ham-nobody-exec: refused")
            })
        {
            why.push_str(": ");
            why.push_str(&crate::direct::classify::scrub_detail(line));
        }
        return Err(why);
    }
    parse_rules_listing(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| format!("rules preflight: {e}"))
}
