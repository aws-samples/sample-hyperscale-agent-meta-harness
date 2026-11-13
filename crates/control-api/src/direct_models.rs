//! The static model list of a direct run: the maintainers' five
//! supported models, each with the one route it runs on, its context window and its smoke-record id, and the one row marked
//! default. The new line does not read the DynamoDB `CATALOG` partition or use
//! `harness-catalog`: `POST /v1/direct/runs` accepts a model if and only if it is listed here with a passing smoke record
//! (invariant 6), says which models are, and `GET /v1/direct/models` lists them with the default marked.
//!
//! The parse, the checks and the choice of a model live in `ham_core::models`, one implementation for control-api, `ham` and
//! the TUI; this module keeps the names control-api's callers and tests use. The window is required because the Cline fork
//! compacts context only when the window is known (patch 0012).

use harness_protocol::api::v1::ModelsResponse;
use harness_protocol::catalog::{CatalogRow, Lifecycle};

pub use ham_core::models::{
    Catalog, Model as DirectModel, ModelList, ModelRefusal, ModelRow, Models as DirectModels,
    ModelsError, builtin, builtin_catalog,
};

/// `GET /v1/models` on the direct stage: one row per selectable (engine, model) pair (a listed model without a smoke record is
/// not selectable and is not shown).
pub fn models_response() -> ModelsResponse {
    models_response_of_catalog(builtin_catalog())
}

/// [`models_response`] over any catalog: the rows of each engine's list, Cline first, each naming its engine.
pub fn models_response_of_catalog(catalog: &Catalog) -> ModelsResponse {
    let mut rows = Vec::new();
    for engine in catalog.engines() {
        if let Some(list) = catalog.for_engine(engine) {
            rows.extend(models_response_of(list).rows);
        }
    }
    ModelsResponse {
        rows,
        excluded: Vec::new(),
    }
}

/// [`models_response`] over any list (the built-in one is all smoked, so only another list can show the filter).
pub fn models_response_of(list: &DirectModels) -> ModelsResponse {
    let rows = list
        .models()
        .iter()
        .filter(|m| m.selectable())
        .map(|m| CatalogRow {
            profile_id: m.id.clone(),
            base_model_id: m.base_model_id.clone(),
            regions: vec![m.region.clone()],
            lifecycle: Lifecycle::Active,
            protocol: m.protocol,
            region: m.region.clone(),
            engine: m.engine,
            route_family: None,
            tool_use: true,
            tool_streaming: false,
            parallel_tools: false,
            integer_args_ok: false,
            context_tokens: m.context_tokens,
            max_output_tokens: m.max_output_tokens,
            burndown_class: "unknown".into(),
            cache_support: false,
            lease_class: "standard".into(),
            iam_resource_arns: Vec::new(),
            retention_allowed_modes: Vec::new(),
            measured_tdd_pass_rate: None,
            gate_cheat_rate: None,
            usd_per_agent_hour: None,
            smoke_record_id: m.smoke_record_id.clone().unwrap_or_default(),
            smoked_at: m.smoked_at.clone().unwrap_or_default(),
            role_max_tokens: Default::default(),
            tdd_qualification: None,
        })
        .collect();
    ModelsResponse {
        rows,
        excluded: Vec::new(),
    }
}
