//! The request a direct run fetches: `runs/<run>/input/request.json`, checked against the
//! SHA-256 the invocation carries, parsed with unknown members refused, and every rule file checked again here (without
//! `ham-core`: agentd trusts neither a stale client nor the Lambda that stored the file). A failure ends the run
//! `stopped_setup_failed` before any model call; the messages name the rule and the file, never the content.

use crate::engine::user_settings::UserSettings;
use harness_protocol::api::v1::{RuleFile, StartDirectRunRequest};
use harness_protocol::direct::{DirectRuleRef, RunLimits, check_engine_config, check_limits};
use harness_protocol::runtime::{RuleDelivery, RuleEntry, RuleLayer, RulesManifest};
use sha2::{Digest, Sha256};

/// The most rule files in a run and the longest rule file name in bytes (`.md` included): the protocol's one definition, the one `ham` and
/// control-api use. A rule file has no size of its own.
pub use harness_protocol::direct::{MAX_RULE_FILES, MAX_RULE_NAME_BYTES};

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Checks the bytes against `want_sha256` and parses them. The error is the `detail` of the failed run.
pub fn parse_request(bytes: &[u8], want_sha256: &str) -> Result<StartDirectRunRequest, String> {
    if sha256_hex(bytes) != want_sha256 {
        return Err("request.json sha256 differs".to_string());
    }
    serde_json::from_slice(bytes).map_err(|e| {
        // A serde message can quote the input; the position says enough.
        format!(
            "request.json is not a StartDirectRunRequest (line {}, column {})",
            e.line(),
            e.column()
        )
    })
}

/// A flat ASCII `.md` name: `[A-Za-z0-9._-]{1,255}`, ends `.md`, does not start with `.` or `-`, and is not `AGENTS.md` in
/// any letter case (the engine would load such a file as a repository instruction file).
pub fn valid_rule_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_RULE_NAME_BYTES
        && name.ends_with(".md")
        && name.len() > ".md".len()
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !name.eq_ignore_ascii_case("AGENTS.md")
}

/// Every rule file, as `ham-core` checked it on the Mac: names, hashes, count and case-folded duplicates.
pub fn check_rules(rules: &[RuleFile]) -> Result<(), String> {
    if rules.len() > MAX_RULE_FILES {
        return Err(format!(
            "rules: {} files; the limit is {MAX_RULE_FILES}",
            rules.len()
        ));
    }
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(rules.len());
    for r in rules {
        if !valid_rule_name(&r.path) {
            return Err(format!(
                "rules: {:?} is not a flat ASCII .md name",
                crate::direct::classify::scrub_detail(&r.path)
            ));
        }
        if sha256_hex(r.content.as_bytes()) != r.sha256 {
            return Err(format!(
                "rules: {} has a sha256 that is not its content's",
                r.path
            ));
        }
        if !seen.insert(r.path.to_ascii_lowercase()) {
            return Err(format!(
                "rules: {} appears twice (names are compared without case)",
                r.path
            ));
        }
    }
    Ok(())
}

/// The `skills` member of the stored request, checked again with the validator `ham` and control-api use:
/// counts, names, paths, sizes, hashes and the entry file. The message names
/// the skill, the file and the rule, never the content.
pub fn check_skills_member(skills: &[harness_protocol::direct::Skill]) -> Result<(), String> {
    harness_protocol::direct::check_skills(skills).map_err(|e| e.to_string())
}

/// The `limits` member of the stored request, checked again with the one check `ham` and control-api call
/// (`harness_protocol::direct::check_limits`). The message names the member and the rule, never the value.
pub fn check_limits_member(limits: Option<&RunLimits>) -> Result<(), String> {
    match limits {
        Some(l) => check_limits(l).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

/// The `engineConfig` member of the stored request (protocol 1.39.0), checked again with the one check `ham` and control-api call
/// (`harness_protocol::direct::check_engine_config`: agentd trusts neither a stale client nor a hand-made request) and typed. A request with no
/// member has no settings ([`UserSettings::default`], nothing is placed). The member must be for the engine this run uses: settings for another
/// engine would be set and never applied, so the run does not start. The message names the key and the rule, never the value.
pub fn engine_settings(
    start: &StartDirectRunRequest,
    engine: &str,
) -> Result<UserSettings, String> {
    let Some(cfg) = &start.engine_config else {
        return Ok(UserSettings::default());
    };
    check_engine_config(cfg).map_err(|e| e.to_string())?;
    if cfg.engine != engine {
        return Err(
            "engine config: the settings are for another engine than the one this run uses"
                .to_string(),
        );
    }
    UserSettings::from_checked(cfg)
}

/// The manifest of the rules a run was given: each a tenant rule the engine loads itself.
pub fn rules_manifest(rules: &[RuleFile]) -> RulesManifest {
    RulesManifest {
        entries: rules
            .iter()
            .map(|r| RuleEntry {
                layer: RuleLayer::Tenant,
                source: r.path.clone(),
                sha256: r.sha256.clone(),
                delivery: RuleDelivery::Native,
            })
            .collect(),
        compiled_sha256: None,
    }
}

/// SHA-256 of the manifest's JSON.
pub fn rules_digest(manifest: &RulesManifest) -> String {
    sha256_hex(&serde_json::to_vec(manifest).unwrap_or_default())
}

/// The rules as `summary.json` lists them.
pub fn rule_refs(rules: &[RuleFile]) -> Vec<DirectRuleRef> {
    rules
        .iter()
        .map(|r| DirectRuleRef {
            name: r.path.clone(),
            sha256: sha256_hex(r.content.as_bytes()),
            bytes: Some(r.content.len() as u64),
        })
        .collect()
}
