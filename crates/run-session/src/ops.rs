//! The ops of the Step Functions machine, their inputs and the dependencies they run on.

use crate::error::RunSessionError;
use crate::finalize::{self, FinalizeInput};
use crate::name_run::{self, Namer};
use crate::plan_delivery::{self, PlannedDelivery};
use crate::ports::{Blobs, PackageTokens, Res, Sessions, Store, Tokens};
use crate::prepare;
use crate::slices::{self, LoopState};
use crate::start_session;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worker_api::handler::Minter;

/// The stage's research settings: the search provider the route will use and the per-run limits copied into each run's policy row.
/// A stage without them (`Config.research` is `None`) writes no row, so research is off for every run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResearchStage {
    pub provider: harness_protocol::research::ResearchProvider,
    pub limits: harness_protocol::research::ResearchLimits,
}

/// Settings of one deployment (the Lambda's environment, see `crate::lambda::config_from_env`).
#[derive(Clone, Debug)]
pub struct Config {
    /// `hamh-direct-harness-<account>`.
    pub bucket: String,
    /// `https://<id>.execute-api.<region>.amazonaws.com/direct`: agentd's refresh and events target.
    pub internal_api_url: String,
    /// The stage Region: S3, the internal API and CodeArtifact (`DirectInvocationRequest.region`).
    pub region: String,
    /// The python worker runtime, read from SSM at start.
    pub runtime_arn: String,
    /// `harness-worker-session` session tag `tenant`.
    pub tenant: String,
    /// The worker runtime's `maxLifetime` (`WORKER_MAX_LIFETIME_S`).
    pub max_lifetime_s: u64,
    /// `arn:aws:states:...:stateMachine:hamh-direct-run`: the machine whose end `run-ended` reaps.
    pub run_state_machine_arn: String,
    /// The research route's settings; `None` leaves research off.
    pub research: Option<ResearchStage>,
    /// The utility model of the helper calls: the row of `ham_core::utility::UtilityModels` the deploy chose.
    /// `None` is the setting `off` (or a value this build cannot use): no helper call is made and no run is named by a model.
    pub utility: Option<ham_core::utility::UtilityModel>,
    /// The helper functions the utility model serves here: the run name, the run tags, or both. The name alone
    /// when the variable is absent, so a function deployed before the tags keeps its behaviour. A purpose that is not in the list does not
    /// run: no read, no call, no write, no log line.
    pub utility_purposes: Vec<ham_core::utility::UtilityPurpose>,
    /// The compute settings: the stage's policy for a run that leaves the machine to it and the cap of active Instances sessions. The default
    /// is the microVM and no cap. The Instances runtime's ARN and the capacity provider's id are read from SSM when a session starts, not held here.
    pub compute: crate::compute::ComputeStage,
    /// A shorter prompt deadline for every slice than the microVM's (`HAMH_SLICE_DEADLINE_S`, a test knob): a live proof of the slice
    /// loop with slices of 10 minutes. `None` (the variable unset, which is every production deployment) leaves the full slice.
    pub slice_deadline_s: Option<u32>,
    /// Unix milliseconds now; a function so a test can fix the clock.
    pub clock_ms: fn() -> u64,
}

impl Config {
    pub fn now_ms(&self) -> u64 {
        (self.clock_ms)()
    }

    /// Whether the utility model serves `purpose` in this function (`UTILITY_PURPOSES`).
    pub fn serves(&self, purpose: ham_core::utility::UtilityPurpose) -> bool {
        self.utility_purposes.contains(&purpose)
    }
}

/// Unix milliseconds from the system clock.
pub fn system_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct Deps<S, T, M, R, B, P> {
    pub store: S,
    pub tokens: T,
    pub minter: M,
    pub sessions: R,
    pub blobs: B,
    pub packages: P,
    /// The utility model's client and limits for the helper ops (`name-run`).
    pub namer: Namer,
    pub cfg: Config,
}

/// A Step Functions failure object as a Catch writes it (`ResultPath`), keys in PascalCase.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct StatesFailure {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub cause: Option<String>,
}

impl StatesFailure {
    /// A Catch wrote it (`Init` starts the state with `{}`).
    pub fn is_set(&self) -> bool {
        self.error.is_some() || self.cause.is_some()
    }
}

/// `{turn, failure}`: what the `StartSession` state left (`Init` sets both to null).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct SessionState {
    /// The `TurnCompleteRequest` JSON worker-api passed to `SendTaskSuccess`.
    #[serde(default)]
    pub turn: Option<Value>,
    #[serde(default)]
    pub failure: Option<StatesFailure>,
}

/// The input of one op: `{"op": "<kebab-case name>", ...}` in camelCase members.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Op {
    Prepare {
        run_id: String,
    },
    StartSession {
        run_id: String,
        task_token: String,
        staged: Value,
        #[serde(default)]
        slice: Option<u32>,
    },
    /// How did the slice that just ended end, and does the run go on with another?
    NextSlice {
        run_id: String,
        staged: Value,
        session: SessionState,
        #[serde(rename = "loop", default)]
        loop_state: LoopState,
    },
    PlanDelivery {
        run_id: String,
        staged: Value,
        session: SessionState,
        #[serde(rename = "loop", default)]
        loop_state: LoopState,
    },
    Finalize {
        run_id: String,
        plan: Box<PlannedDelivery>,
        #[serde(default)]
        validated: Value,
        #[serde(default)]
        packaged: Value,
        #[serde(default)]
        pr_link: Value,
        #[serde(default)]
        pr_failure: Option<StatesFailure>,
    },
    MarkBlocked {
        run_id: String,
        #[serde(default)]
        state: Value,
    },
    /// Names the run. The machine's first state sends it as an asynchronous call and never waits for it.
    NameRun {
        run_id: String,
    },
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Prepare { .. } => "prepare",
            Op::StartSession { .. } => "start-session",
            Op::NextSlice { .. } => "next-slice",
            Op::PlanDelivery { .. } => "plan-delivery",
            Op::Finalize { .. } => "finalize",
            Op::MarkBlocked { .. } => "mark-blocked",
            Op::NameRun { .. } => "name-run",
        }
    }

    pub fn run_id(&self) -> &str {
        match self {
            Op::Prepare { run_id }
            | Op::StartSession { run_id, .. }
            | Op::NextSlice { run_id, .. }
            | Op::PlanDelivery { run_id, .. }
            | Op::Finalize { run_id, .. }
            | Op::MarkBlocked { run_id, .. }
            | Op::NameRun { run_id } => run_id,
        }
    }
}

/// Runs one op; the answer is the JSON the machine stores (the Lambda's payload).
pub async fn handle<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, op: &Op) -> Res<Value>
where
    S: Store,
    T: Tokens,
    M: Minter,
    R: Sessions,
    B: Blobs,
    P: PackageTokens,
{
    if !harness_protocol::common::is_ulid(op.run_id()) {
        return Err(RunSessionError::Invalid(format!(
            "runId {:?} is not a run id",
            op.run_id()
        )));
    }
    match op {
        Op::Prepare { run_id } => to_value(prepare::prepare(deps, run_id).await?),
        Op::StartSession {
            run_id,
            task_token,
            staged,
            slice,
        } => to_value(
            start_session::start_session(deps, run_id, task_token, staged, slice.unwrap_or(1))
                .await?,
        ),
        Op::NextSlice {
            run_id,
            staged,
            session,
            loop_state,
        } => to_value(slices::next_slice(deps, run_id, staged, session, loop_state).await?),
        Op::PlanDelivery {
            run_id,
            staged,
            session,
            loop_state,
        } => {
            to_value(plan_delivery::plan_delivery(deps, run_id, staged, session, loop_state).await?)
        }
        Op::Finalize {
            run_id,
            plan,
            validated,
            packaged,
            pr_link,
            pr_failure,
        } => to_value(
            finalize::finalize(
                deps,
                &FinalizeInput {
                    run_id,
                    plan,
                    validated,
                    packaged,
                    pr_link,
                    pr_failure: pr_failure.as_ref().filter(|f| f.is_set()),
                },
            )
            .await?,
        ),
        Op::MarkBlocked { run_id, state } => {
            to_value(finalize::mark_blocked(deps, run_id, state).await?)
        }
        Op::NameRun { run_id } => name_run::name_run(deps, run_id).await,
    }
}

pub(crate) fn to_value<T: Serialize>(v: T) -> Res<Value> {
    serde_json::to_value(v).map_err(|e| RunSessionError::Invalid(format!("op answer: {e}")))
}

/// The Step Functions failure as the reason a person reads: the Lambda's `errorMessage` when the cause is its error
/// document, else the cause, else the error name.
pub fn failure_reason(f: &StatesFailure) -> String {
    let cause = f.cause.as_deref().unwrap_or_default();
    serde_json::from_str::<Value>(cause)
        .ok()
        .and_then(|v| {
            v.get("errorMessage")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if cause.is_empty() {
                f.error.clone().unwrap_or_default()
            } else {
                cause.to_string()
            }
        })
}
