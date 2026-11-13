//! Model catalog rows. A model is selectable
//! only through a row with a passing smoke record for its (model, protocol, Region, engine).

use crate::common::{Engine, Protocol};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum Lifecycle {
    #[serde(rename = "ACTIVE")]
    Active,
    #[serde(rename = "LEGACY")]
    Legacy,
}

/// Mantle route family from the live probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum RouteFamily {
    #[serde(rename = "/v1")]
    V1,
    #[serde(rename = "/openai/v1")]
    OpenaiV1,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRow {
    /// Prefixed inference profile ID (never a bare model ID).
    pub profile_id: String,
    pub base_model_id: String,
    pub regions: Vec<String>,
    pub lifecycle: Lifecycle,
    pub protocol: Protocol,
    /// The pinned Region for this row.
    pub region: String,
    pub engine: Engine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_family: Option<RouteFamily>,
    pub tool_use: bool,
    pub tool_streaming: bool,
    pub parallel_tools: bool,
    pub integer_args_ok: bool,
    pub context_tokens: u32,
    pub max_output_tokens: u32,
    pub burndown_class: String,
    pub cache_support: bool,
    pub lease_class: String,
    /// IAM resource ARNs; three-part for global profiles.
    pub iam_resource_arns: Vec<String>,
    pub retention_allowed_modes: Vec<String>,
    #[serde(default)]
    pub measured_tdd_pass_rate: Option<f64>,
    #[serde(default)]
    pub gate_cheat_rate: Option<f64>,
    #[serde(default)]
    pub usd_per_agent_hour: Option<f64>,
    pub smoke_record_id: String,
    #[schemars(extend("format" = "date-time"))]
    pub smoked_at: String,
    /// The output-token cap per role slug on a Cline row, written to the fork's
    /// `providers.json`; absent on OpenCode rows.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_max_tokens: BTreeMap<String, u32>,
    /// The TDD qualification on the row's engine; absent until the catalog job records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tdd_qualification: Option<TddQualification>,
}

/// `python-calc` red-then-green runs of one model on one engine through the deployed stack,
/// and the roles it may drive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TddQualification {
    pub fixture: String,
    pub engine: Engine,
    /// Runs whose gates were green and whose evaluator returned `met`.
    pub passed: u32,
    pub runs: u32,
    /// The deployment's `imageDigest` the runs used.
    pub image_digest: String,
    /// The evidence record the runs were read from.
    pub record: String,
    pub run_ids: Vec<String>,
    /// Role slugs the model qualifies for.
    pub roles: Vec<String>,
    /// Role slugs it does not, each with its reason (`not-tdd-qualified:<passed>/<runs>`).
    pub refused: Vec<RoleRefusal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoleRefusal {
    pub role: String,
    pub reason: String,
}

impl CatalogRow {
    /// The store sort key under `CATALOG`: `<profileId>#<protocol>#<region>#<engine>`.
    pub fn sort_key(&self) -> String {
        crate::store::keys::catalog_sk(&self.profile_id, self.protocol, &self.region, self.engine)
    }
}

/// A model the generator excluded, with the exact reason (for example the ValidationException text).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExcludedModel {
    pub profile_id: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    pub reason: String,
}
