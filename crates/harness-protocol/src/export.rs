//! Contract artifacts: JSON Schema 2020-12 documents and OpenAPI 3.1 documents built
//! from the same schemars definitions. `cargo run -p harness-protocol --bin export-schemas`
//! writes them to `schemas/`; a test fails when a committed file differs from fresh output.

use crate::api::internal::{
    CredsRequest, CredsResponse, EventsBatchRequest, EventsBatchResponse, HeartbeatRequest,
    HeartbeatResponse, TurnCompleteRequest, TurnCompleteResponse,
};
use crate::api::v1::{
    DownloadUrlResponse, EventsPage, InputAccepted, InputAnswer, ListArtifactsResponse,
    ListReposResponse, ListRunsResponse, ModelList, ModelsResponse, RepoAdminRequest,
    RepoAdminResponse, RunControlResponse, RunDetail, StartDirectRunRequest, StartRunResponse,
    WhoAmIResponse,
};
use crate::api::{ApiError, INTERNAL_ROUTES, Route, V1_ROUTES};
use crate::engine_profile::EngineProfile;
use crate::envelope::Envelope;
use crate::evidence::EvidenceHeader;
use crate::messages::{
    ClaimRequest, ClaimResponse, MessageResponse, MessagesPage, OutcomeRequest, OutcomeResponse,
    SealRequest, SealResponse, SendMessageRequest, SendMessageResponse, SessionAck, SessionReport,
};
use crate::orchestration::OrchestrationContract;
use crate::research::{
    FetchRequest, FetchResponse, JobRequest, JobResponse, LimitsRequest, LimitsResponse,
    RepoRequest, SearchRequest, SearchResponse,
};
use crate::runtime::RuntimeContract;
use crate::store::StoreItem;
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value, json};

pub const SCHEMA_FILES: &[&str] = &[
    "event-envelope.schema.json",
    "openapi-v1.json",
    "openapi-internal.json",
    "items.schema.json",
    "evidence.schema.json",
    "engine-profile.schema.json",
    "runtime.schema.json",
    "orchestration.schema.json",
    "tools.schema.json",
];

/// Every contract file with its content, in [`SCHEMA_FILES`] order.
pub fn all_schemas() -> Vec<(&'static str, Value)> {
    let docs = vec![
        (SCHEMA_FILES[0], json_schema::<Envelope>()),
        (
            SCHEMA_FILES[1],
            openapi("Hyperscale Agent Meta Harness public API", "/v1", V1_ROUTES),
        ),
        (
            SCHEMA_FILES[2],
            openapi(
                "Hyperscale Agent Meta Harness internal API",
                "/internal",
                INTERNAL_ROUTES,
            ),
        ),
        (SCHEMA_FILES[3], json_schema::<StoreItem>()),
        (SCHEMA_FILES[4], json_schema::<EvidenceHeader>()),
        (SCHEMA_FILES[5], json_schema::<EngineProfile>()),
        (SCHEMA_FILES[6], json_schema::<RuntimeContract>()),
        (SCHEMA_FILES[7], json_schema::<OrchestrationContract>()),
        (SCHEMA_FILES[8], crate::tools::contract_schema()),
    ];
    docs.into_iter().map(|(n, v)| (n, canonical(&v))).collect()
}

/// The value with every object's keys in sorted order. serde_json's `preserve_order` feature can be
/// switched on by any crate in a build graph; contract files must not depend on that.
pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), canonical(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Pretty JSON with a trailing newline: the committed file format.
pub fn render(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

fn json_schema<T: JsonSchema>() -> Value {
    SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value()
}

fn body(schema: Schema) -> Value {
    json!({"content": {"application/json": {"schema": schema.to_value()}}})
}

/// Request and response schemas for one operation id.
fn operation_types(g: &mut SchemaGenerator, operation_id: &str) -> (Option<Schema>, Schema) {
    match operation_id {
        "startDirectRun" => (
            Some(g.subschema_for::<StartDirectRunRequest>()),
            g.subschema_for::<StartRunResponse>(),
        ),
        "listDirectModels" => (None, g.subschema_for::<ModelList>()),
        "listRuns" => (None, g.subschema_for::<ListRunsResponse>()),
        "getRun" => (None, g.subschema_for::<RunDetail>()),
        "cancelRun" => (None, g.subschema_for::<RunControlResponse>()),
        "answerInput" => (
            Some(g.subschema_for::<InputAnswer>()),
            g.subschema_for::<InputAccepted>(),
        ),
        "sendMessage" => (
            Some(g.subschema_for::<SendMessageRequest>()),
            g.subschema_for::<SendMessageResponse>(),
        ),
        "listMessages" => (None, g.subschema_for::<MessagesPage>()),
        "getMessage" | "withdrawMessage" => (None, g.subschema_for::<MessageResponse>()),
        "getEvents" => (None, g.subschema_for::<EventsPage>()),
        "listArtifacts" => (None, g.subschema_for::<ListArtifactsResponse>()),
        "createDownloadUrl" => (None, g.subschema_for::<DownloadUrlResponse>()),
        "listModels" => (None, g.subschema_for::<ModelsResponse>()),
        "whoAmI" => (None, g.subschema_for::<WhoAmIResponse>()),
        "listRepos" => (None, g.subschema_for::<ListReposResponse>()),
        "adminRepos" => (
            Some(g.subschema_for::<RepoAdminRequest>()),
            g.subschema_for::<RepoAdminResponse>(),
        ),
        "refreshCredentials" => (
            Some(g.subschema_for::<CredsRequest>()),
            g.subschema_for::<CredsResponse>(),
        ),
        "heartbeat" => (
            Some(g.subschema_for::<HeartbeatRequest>()),
            g.subschema_for::<HeartbeatResponse>(),
        ),
        "completeTurn" => (
            Some(g.subschema_for::<TurnCompleteRequest>()),
            g.subschema_for::<TurnCompleteResponse>(),
        ),
        "postEvents" => (
            Some(g.subschema_for::<EventsBatchRequest>()),
            g.subschema_for::<EventsBatchResponse>(),
        ),
        "reportMessageSession" => (
            Some(g.subschema_for::<SessionReport>()),
            g.subschema_for::<SessionAck>(),
        ),
        "claimMessages" => (
            Some(g.subschema_for::<ClaimRequest>()),
            g.subschema_for::<ClaimResponse>(),
        ),
        "sealMessages" => (
            Some(g.subschema_for::<SealRequest>()),
            g.subschema_for::<SealResponse>(),
        ),
        "postMessageOutcome" => (
            Some(g.subschema_for::<OutcomeRequest>()),
            g.subschema_for::<OutcomeResponse>(),
        ),
        "researchSearch" => (
            Some(g.subschema_for::<SearchRequest>()),
            g.subschema_for::<SearchResponse>(),
        ),
        "researchFetch" => (
            Some(g.subschema_for::<FetchRequest>()),
            g.subschema_for::<FetchResponse>(),
        ),
        "researchRepo" => (
            Some(g.subschema_for::<RepoRequest>()),
            g.subschema_for::<JobResponse>(),
        ),
        "researchJob" => (
            Some(g.subschema_for::<JobRequest>()),
            g.subschema_for::<JobResponse>(),
        ),
        "researchLimits" => (
            Some(g.subschema_for::<LimitsRequest>()),
            g.subschema_for::<LimitsResponse>(),
        ),
        other => unreachable!("route {other} has no schema mapping"),
    }
}

fn parameters(route: &Route) -> Vec<Value> {
    let mut params: Vec<Value> = route
        .path
        .split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .map(|name| json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}}))
        .collect();
    match route.operation_id {
        "startDirectRun" => params.push(json!({
            "name": "Idempotency-Key", "in": "header", "required": true,
            "description": "Client-generated key; a repeat with the same key returns the original run.",
            "schema": {"type": "string", "minLength": 1, "maxLength": 128, "pattern": "^[!-~]{1,128}$"}
        })),
        "listRuns" => params.push(json!({"name": "nextToken", "in": "query", "required": false, "schema": {"type": "string"}})),
        "getEvents" => {
            params.push(json!({"name": "shard", "in": "query", "required": true, "schema": {"type": "integer", "minimum": 0, "maximum": 31}}));
            params.push(json!({"name": "after", "in": "query", "required": false, "description": "Last eventId already read (ULID).", "schema": {"type": "string"}}));
        }
        _ => {}
    }
    params
}

fn openapi(title: &str, server: &str, routes: &[Route]) -> Value {
    let mut settings = SchemaSettings::draft2020_12();
    settings.definitions_path = "/components/schemas".into();
    settings.meta_schema = None;
    let mut g = settings.into_generator();
    let error = g.subschema_for::<ApiError>();
    let mut paths = Map::new();
    for r in routes {
        let (request, response) = operation_types(&mut g, r.operation_id);
        let mut op = json!({
            "operationId": r.operation_id,
            "summary": r.summary,
            "security": [{"sigv4": []}],
            "responses": {
                "200": {"description": "OK", "content": body(response)["content"].clone()},
                "default": {"description": "Error", "content": body(error.clone())["content"].clone()},
            },
        });
        let params = parameters(r);
        if !params.is_empty() {
            op["parameters"] = Value::Array(params);
        }
        if let Some(req) = request {
            let mut rb = body(req);
            rb["required"] = Value::Bool(true);
            op["requestBody"] = rb;
        }
        let entry = paths.entry(r.path.to_string()).or_insert_with(|| json!({}));
        entry[r.method.to_ascii_lowercase()] = op;
    }
    json!({
        "openapi": "3.1.0",
        "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
        "info": {"title": title, "version": crate::PROTOCOL_VERSION},
        "servers": [{"url": server}],
        "paths": paths,
        "components": {
            "schemas": g.take_definitions(true),
            "securitySchemes": {
                "sigv4": {
                    "type": "apiKey",
                    "name": "Authorization",
                    "in": "header",
                    "description": "AWS Signature Version 4 (API Gateway AWS_IAM authorization).",
                    "x-amazon-apigateway-authtype": "awsSigv4"
                }
            }
        }
    })
}
