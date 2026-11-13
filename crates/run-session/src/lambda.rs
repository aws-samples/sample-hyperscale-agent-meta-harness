//! The Lambda's two inputs: a Step Functions task payload (`{"op": ...}`) and the EventBridge status change of
//! `run-ended`; the environment the CDK stack sets; and the one JSON log line per invocation, which names the op, the
//! run and the outcome and never the body.

use crate::error::RunSessionError;
use crate::ops::{Config, Deps, Op, ResearchStage, handle, system_clock_ms};
use crate::ports::{Blobs, PackageTokens, Sessions, Store, Tokens};
use crate::run_ended::{is_status_change, parse_run_end, reap};
use ham_core::utility::{UtilityChoice, UtilityModel, UtilityModels, UtilityPurpose};
use lambda_runtime::Diagnostic;
use serde_json::{Value, json};
use worker_api::handler::Minter;

/// `Config` from the function environment (`TABLE_NAME` and the like are read by `bin/run-session.rs`).
pub fn config_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    runtime_arn: String,
) -> Result<Config, String> {
    let need = |k: &str| {
        get(k)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("missing environment variable {k}"))
    };
    let stage = need("HAMH_STAGE")?;
    if stage != "direct" {
        return Err(format!("HAMH_STAGE must be direct, got {stage:?}"));
    }
    let max_lifetime_s = need("WORKER_MAX_LIFETIME_S")?
        .parse::<u64>()
        .map_err(|e| format!("WORKER_MAX_LIFETIME_S: {e}"))?;
    let region = get("AWS_REGION")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| harness_protocol::target::E2E_REGION.to_string());
    // A setting this build cannot use turns the helper calls off and says so once; it never stops the function, because a run
    // does not depend on its name.
    let utility = utility_from_lookup(&get, &region);
    if let Some(warning) = &utility.warning {
        println!("{warning}");
    }
    let purposes = purposes_from_lookup(&get);
    if let Some(warning) = &purposes.warning {
        println!("{warning}");
    }
    let (compute, _) = crate::compute::from_lookup(&get)?;
    let slice_deadline_s = slice_deadline_from_lookup(&get)?;
    let provider_silence_s = provider_silence_from_lookup(&get)?;
    Ok(Config {
        bucket: need("BUCKET_NAME")?,
        internal_api_url: need("INTERNAL_API_URL")?,
        region,
        runtime_arn,
        tenant: need("TENANT")?,
        max_lifetime_s,
        run_state_machine_arn: need("RUN_STATE_MACHINE_ARN")?,
        research: research_from_lookup(&get)?,
        utility: utility.model,
        utility_purposes: purposes.purposes,
        compute,
        slice_deadline_s,
        provider_silence_s,
        clock_ms: system_clock_ms,
    })
}

/// `HAMH_SLICE_DEADLINE_S`: whole seconds, 60 to 28,800, the longest any slice's prompt may run. Absent or blank leaves the
/// full slice. A value that is not in range stops the function at start, so a typo in a live proof is not read as "no limit".
pub fn slice_deadline_from_lookup(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<u32>, String> {
    let Some(value) = get("HAMH_SLICE_DEADLINE_S").filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    match value.trim().parse::<u32>() {
        Ok(s) if (60..=28_800).contains(&s) => Ok(Some(s)),
        _ => Err(format!(
            "HAMH_SLICE_DEADLINE_S must be whole seconds from 60 to 28800, got {value:?}"
        )),
    }
}

/// The least `HAMH_PROVIDER_SILENCE_S`: a silence under a minute would cancel an engine that is only thinking.
pub const MIN_PROVIDER_SILENCE_S: u32 = 60;

/// `HAMH_PROVIDER_SILENCE_S`: whole seconds, 60 to 3,600 (the protocol's own top, `MAX_PROVIDER_SILENCE_S`), how long Cline may go without output before the
/// harness cancels the prompt. Absent or blank leaves agentd's own 900 s. A value that is not in range stops the function at start, so a typo is not read as "no limit".
pub fn provider_silence_from_lookup(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<u32>, String> {
    let Some(value) = get("HAMH_PROVIDER_SILENCE_S").filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    match value.trim().parse::<u32>() {
        Ok(s)
            if (MIN_PROVIDER_SILENCE_S..=harness_protocol::direct::MAX_PROVIDER_SILENCE_S)
                .contains(&s) =>
        {
            Ok(Some(s))
        }
        _ => Err(format!(
            "HAMH_PROVIDER_SILENCE_S must be whole seconds from {MIN_PROVIDER_SILENCE_S} to {}, got {value:?}",
            harness_protocol::direct::MAX_PROVIDER_SILENCE_S
        )),
    }
}

/// What the variable `UTILITY_MODEL` means inside the function.
#[derive(Clone, Debug, Default)]
pub struct UtilitySetting {
    /// The row to call; `None` leaves every helper call off.
    pub model: Option<UtilityModel>,
    /// The one log line (JSON) for a value that names no usable row; absent for a plain `off`.
    pub warning: Option<String>,
}

/// `UTILITY_MODEL` against the catalog this build ships. The deploy resolves the catalog default into the variable
/// (`infra/lib/config.ts`), so here an absent or blank value means the helper calls are off, as does `off`. An id or alias of a
/// selectable row whose Region is the stage's is that row; anything else is off with a warning that names the variable.
pub fn utility_from_lookup(
    get: &impl Fn(&str) -> Option<String>,
    stage_region: &str,
) -> UtilitySetting {
    let value = get("UTILITY_MODEL").unwrap_or_default();
    let setting = value.trim();
    if setting.is_empty() || setting.eq_ignore_ascii_case("off") {
        return UtilitySetting::default();
    }
    let invalid = |detail: String| UtilitySetting {
        model: None,
        warning: Some(
            json!({"function": "run-session", "event": "utility-model-invalid", "detail": detail})
                .to_string(),
        ),
    };
    match UtilityModels::builtin().resolve_named(Some(setting), "UTILITY_MODEL") {
        Ok(UtilityChoice::Model(m)) => match m.check_region(stage_region) {
            Ok(()) => UtilitySetting {
                model: Some(m.clone()),
                warning: None,
            },
            Err(e) => invalid(e.to_string()),
        },
        Ok(UtilityChoice::Disabled) => UtilitySetting::default(),
        Err(e) => invalid(e.to_string()),
    }
}

/// What the variable `UTILITY_PURPOSES` means inside the function.
#[derive(Clone, Debug, Default)]
pub struct PurposesSetting {
    /// The purposes served, in the build's order, without repeats. Never empty.
    pub purposes: Vec<UtilityPurpose>,
    /// The one log line (JSON) for a value that names a purpose this build does not know; absent otherwise.
    pub warning: Option<String>,
}

/// The longest piece of the value a warning quotes.
const PURPOSES_QUOTE_CHARS: usize = 40;

/// `UTILITY_PURPOSES`, a comma list of purpose words (`run-name`, `run-tags`). Absent, blank or only commas mean the run name alone, so a
/// function deployed before the tags keeps its behaviour. Blanks around a word and its case do not matter, an empty piece is ignored and a
/// repeated word counts once. A word the build does not know makes the whole value invalid: the function serves the run name alone (the
/// tags are off, the fail-closed side), prints one `utility-purposes-invalid` line that names the variable, and still starts, because a
/// run does not depend on its name or its tags. A value that lists `run-tags` alone turns the name off. The whole value `off` (after
/// blanks, in any case) is what the deploy sets when the utility model is switched off, where nothing is called and the purposes do not
/// matter: it means the same as an absent value and prints no line. `off` inside a list is a word the build does not know.
pub fn purposes_from_lookup(get: &impl Fn(&str) -> Option<String>) -> PurposesSetting {
    let value = get("UTILITY_PURPOSES").unwrap_or_default();
    if value.trim().eq_ignore_ascii_case("off") {
        return PurposesSetting {
            purposes: vec![UtilityPurpose::RunName],
            warning: None,
        };
    }
    let mut wanted: Vec<UtilityPurpose> = Vec::new();
    for piece in value.split(',') {
        let word = piece.trim().to_ascii_lowercase();
        if word.is_empty() {
            continue;
        }
        let Some(purpose) = UtilityPurpose::from_word(&word) else {
            let quoted: String = piece.trim().chars().take(PURPOSES_QUOTE_CHARS).collect();
            let known: Vec<&str> = UtilityPurpose::ALL.iter().map(|p| p.as_str()).collect();
            return PurposesSetting {
                purposes: vec![UtilityPurpose::RunName],
                warning: Some(
                    json!({
                        "function": "run-session",
                        "event": "utility-purposes-invalid",
                        "detail": format!(
                            "UTILITY_PURPOSES: {quoted:?} is not a purpose of this build ({}); serving run-name alone",
                            known.join(", ")
                        ),
                    })
                    .to_string(),
                ),
            };
        };
        if !wanted.contains(&purpose) {
            wanted.push(purpose);
        }
    }
    if wanted.is_empty() {
        wanted.push(UtilityPurpose::RunName);
    }
    wanted.sort_by_key(|p| UtilityPurpose::ALL.iter().position(|q| q == p));
    PurposesSetting {
        purposes: wanted,
        warning: None,
    }
}

/// The research settings: present when the function has a research table and a provider (`RESEARCH_TABLE_NAME`, `RESEARCH_PROVIDER`:
/// auto, tavily, tavily-keyless, agentcore or fixture), with the stage's default limits unless `RESEARCH_LIMITS_JSON` gives a full set. A stage without
/// them runs with research off. The word `auto` is the one research-api reads too, and the wire has no such provider: the policy row of a run
/// names `agentcore`, the provider that serves while no Tavily key is stored. The row's provider is informational; research-api chooses the provider of each
/// request, and every answer, record and audit object names the provider that served it.
pub fn research_from_lookup(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<ResearchStage>, String> {
    use harness_protocol::research::{ResearchLimits, ResearchProvider};
    let set = |k: &str| get(k).filter(|v| !v.is_empty());
    let (Some(_), Some(provider)) = (set("RESEARCH_TABLE_NAME"), set("RESEARCH_PROVIDER")) else {
        return Ok(None);
    };
    let provider = match provider.as_str() {
        "tavily" => ResearchProvider::Tavily,
        "tavily-keyless" => ResearchProvider::TavilyKeyless,
        "agentcore" | "auto" => ResearchProvider::Agentcore,
        "fixture" => ResearchProvider::Fixture,
        other => return Err(format!("RESEARCH_PROVIDER: unknown provider {other:?}")),
    };
    let limits = match set("RESEARCH_LIMITS_JSON") {
        Some(text) => serde_json::from_str::<ResearchLimits>(&text)
            .map_err(|e| format!("RESEARCH_LIMITS_JSON: {e}"))?,
        None => ResearchLimits::default(),
    };
    limits
        .validate()
        .map_err(|e| format!("RESEARCH_LIMITS_JSON: {}", e.message))?;
    Ok(Some(ResearchStage { provider, limits }))
}

/// What one invocation did, for the log line.
#[derive(Clone, Debug)]
pub struct Logged {
    pub op: String,
    pub run_id: Option<String>,
    pub error_type: Option<&'static str>,
    pub reason: Option<String>,
}

impl Logged {
    pub fn line(&self) -> String {
        let mut v = json!({
            "function": "run-session", "op": self.op, "runId": self.run_id,
            "outcome": self.error_type.unwrap_or("ok"),
        });
        if let Some(r) = &self.reason {
            v["reason"] = Value::String(r.clone());
        }
        v.to_string()
    }
}

/// Runs one invocation: a status-change event is `run-ended`, anything else is an op.
pub async fn serve_event<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    event: Value,
) -> (Result<Value, RunSessionError>, Logged)
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    if is_status_change(&event) {
        match parse_run_end(&event, &deps.cfg.run_state_machine_arn) {
            Ok(end) => {
                let run_id = end.run_id.clone();
                let r = reap(deps, &end).await;
                finish("run-ended", Some(run_id), r.map(|x| json!(x)))
            }
            Err(e) => finish("run-ended", None, Err(e)),
        }
    } else {
        match serde_json::from_value::<Op>(event.clone()) {
            Ok(op) => {
                let (name, run_id) = (op.name(), op.run_id().to_string());
                finish(name, Some(run_id), handle(deps, &op).await)
            }
            Err(e) => finish(
                event.get("op").and_then(Value::as_str).unwrap_or("unknown"),
                event
                    .get("runId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Err(RunSessionError::Invalid(format!("input: {e}"))),
            ),
        }
    }
}

fn finish(
    op: &str,
    run_id: Option<String>,
    r: Result<Value, RunSessionError>,
) -> (Result<Value, RunSessionError>, Logged) {
    let log = Logged {
        op: op.to_string(),
        run_id,
        error_type: r.as_ref().err().map(RunSessionError::error_type),
        reason: r.as_ref().err().map(ToString::to_string),
    };
    (r, log)
}

/// What the Lambda runtime calls for one invocation: runs it, prints its one log line and maps an error to the
/// `errorType` and `errorMessage` the machine's Retry and Catch rules (and `blocked_reason`) read.
pub async fn handle_payload<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    payload: Value,
) -> Result<Value, Diagnostic>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    let (result, log) = serve_event(deps, payload).await;
    println!("{}", log.line());
    result.map_err(|e| Diagnostic {
        error_type: e.error_type().into(),
        error_message: e.to_string(),
    })
}
