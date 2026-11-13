//! `POST /v1/direct/runs` and the engine of the run. A request that names no engine runs Cline and is stored byte for byte as it was. A request that
//! names one is checked in one place: the engine must have a profile in the build (422 `unknown-engine`), and the (engine, model) pair must have a row with a
//! passing smoke record for that engine (422, the message names the pair). Nothing is written for a refused request. The engine is stored with the run.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request};
use control_api::direct::{EngineRefusal, engine_config_mismatch, engine_refusal};
use ham_core::models::{self, Catalog};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::common::Engine;
use serde_json::{Value, json};

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");
const SOL: &str = "us.openai.gpt-6.1-sol";
const GLM: &str = "us.zai.glm-5.3";

fn body(model: &str, engine: Option<&str>) -> Value {
    let mut b = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": model,
        "rules": []
    });
    if let Some(e) = engine {
        b["engine"] = json!(e);
    }
    b
}

async fn start(w: &World, key: &str, b: &Value) -> (u16, Value) {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

fn parsed(b: &Value) -> StartDirectRunRequest {
    serde_json::from_value(b.clone()).unwrap()
}

/// The shipped list without its OpenCode rows: a build that has only Cline rows.
fn catalog_with_only_cline_rows() -> Catalog {
    let mut v: Value = serde_json::from_str(MODELS_JSON).unwrap();
    v["models"]
        .as_array_mut()
        .unwrap()
        .retain(|m| m.get("engine").is_none());
    Catalog::parse(&v.to_string()).unwrap()
}

/// The shipped Cline rows plus a made-up OpenCode row for Sol (and one for Sonnet without a smoke record).
fn catalog_with_opencode() -> Catalog {
    let mut v: Value = serde_json::from_str(MODELS_JSON).unwrap();
    let rows = v["models"].as_array_mut().unwrap();
    rows.retain(|m| m.get("engine").is_none());
    let mut sol = rows.iter().find(|m| m["id"] == SOL).unwrap().clone();
    sol["engine"] = json!("opencode");
    sol["smokeRecordId"] = json!("b787:test:opencode:sol");
    sol.as_object_mut().unwrap().remove("default");
    sol.as_object_mut().unwrap().remove("alternateRoutes");
    let mut sonnet = rows.iter().find(|m| m["id"] == SONNET).unwrap().clone();
    sonnet["engine"] = json!("opencode");
    sonnet.as_object_mut().unwrap().remove("smokeRecordId");
    sonnet.as_object_mut().unwrap().remove("default");
    sonnet.as_object_mut().unwrap().remove("alternateRoutes");
    rows.push(sol);
    rows.push(sonnet);
    Catalog::parse(&v.to_string()).unwrap()
}

fn stored_request(w: &World) -> Value {
    let objects = w.objects.lock().unwrap();
    let (_, bytes) = objects
        .iter()
        .find(|(k, _)| k.ends_with("input/request.json"))
        .expect("the stored request");
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn b787_a_request_without_an_engine_is_stored_as_before_and_the_run_names_no_engine() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(SONNET, None)).await;
    assert_eq!(status, 201, "{v}");
    let stored = stored_request(&w);
    assert!(stored.get("engine").is_none(), "{stored}");
    let run = w.runs.lock().unwrap().values().next().cloned().unwrap();
    assert_eq!(run.engine, None);
}

#[tokio::test]
async fn b787_a_request_that_names_the_default_engine_is_stored_as_if_it_named_none() {
    // The stored request is byte for byte the request of a client that names no engine, so an image that predates the member still reads it.
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(SONNET, Some("cline"))).await;
    assert_eq!(status, 201, "{v}");
    assert!(stored_request(&w).get("engine").is_none());
    let run = w.runs.lock().unwrap().values().next().cloned().unwrap();
    assert_eq!(run.engine, None);
    let w2 = World::new();
    let (status, _) = start(&w2, "k-1", &body(SONNET, None)).await;
    assert_eq!(status, 201);
    assert_eq!(stored_request(&w), stored_request(&w2));
}

#[tokio::test]
async fn b787_an_engine_with_no_profile_is_422_unknown_engine_and_writes_nothing() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(SONNET, Some("nope"))).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "unknown-engine", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("\"nope\"") && message.contains("cline"),
        "{message}"
    );
    assert!(w.runs.lock().unwrap().is_empty());
    assert!(w.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b787_a_pair_with_no_smoke_record_for_the_engine_is_422_and_the_message_names_the_pair() {
    // `fake-acp` has a profile in every build and no row in the catalog, so it stands for an engine nobody has qualified.
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(SONNET, Some("fake-acp"))).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "model-not-smoked", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("fake-acp") && message.contains(SONNET),
        "{message}"
    );
    assert!(w.runs.lock().unwrap().is_empty());
    assert!(w.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b787_a_bad_engine_name_is_a_bad_request_before_anything_is_stored() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(SONNET, Some("Open Code"))).await;
    assert_eq!(status, 400, "{v}");
    assert!(w.runs.lock().unwrap().is_empty());
}

#[test]
fn b787_opencode_with_a_smoked_row_passes_the_check_and_a_missing_row_is_refused() {
    let c = catalog_with_opencode();
    assert!(engine_refusal(&c, &parsed(&body(SOL, Some("opencode")))).is_none());
    // A row without a smoke record, and a model with no OpenCode row at all, are both a pair without a record.
    for model in [SONNET, "us.openai.gpt-6-astra"] {
        let e: EngineRefusal =
            engine_refusal(&c, &parsed(&body(model, Some("opencode")))).expect("a refusal");
        assert_eq!((e.status, e.code), (422, "model-not-smoked"), "{model}");
        assert!(
            e.message.contains("opencode") && e.message.contains(model),
            "{}",
            e.message
        );
    }
    // An unknown model is unknown.
    let e =
        engine_refusal(&c, &parsed(&body("no.such.model", Some("opencode")))).expect("a refusal");
    assert_eq!((e.status, e.code), (422, "unknown-model"));
}

#[test]
fn b787_a_request_without_an_engine_is_checked_exactly_as_it_was() {
    let c = models::builtin_catalog();
    assert!(engine_refusal(c, &parsed(&body(SONNET, None))).is_none());
    let e = engine_refusal(c, &parsed(&body("us.vendor.unknown-model", None))).expect("a refusal");
    let old = models::builtin()
        .check("us.vendor.unknown-model")
        .unwrap_err();
    assert_eq!(
        (e.status, e.code, e.message),
        (422, old.code(), old.to_string())
    );
}

#[test]
fn b787_engine_settings_for_another_engine_than_the_run_uses_are_refused() {
    let mut b = body(SOL, Some("opencode"));
    b["engineConfig"] =
        json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": "low"}}});
    let e = engine_config_mismatch(&parsed(&b)).expect("a refusal");
    assert_eq!((e.status, e.code), (400, "invalid-engine-config"));
    assert!(
        e.message.contains("opencode") && e.message.contains("cline"),
        "{}",
        e.message
    );
    // The same member under the same engine is not this check's business, named or not.
    for engine in [Some("cline"), None] {
        let mut b = body(SONNET, engine);
        b["engineConfig"] =
            json!({"engine": "cline", "schema": 1, "settings": {"reasoning": {"effort": "low"}}});
        assert!(engine_config_mismatch(&parsed(&b)).is_none(), "{engine:?}");
    }
    // A request that names no engine runs the default one: settings of another engine are a mismatch for it too.
    let mut b = body(SONNET, None);
    b["engineConfig"] = json!({"engine": "opencode", "schema": 1, "settings": {}});
    assert!(engine_config_mismatch(&parsed(&b)).is_some());
    // The pair check does not look at the settings.
    assert!(engine_refusal(models::builtin_catalog(), &parsed(&b)).is_none());
}

#[tokio::test]
async fn b787_a_request_that_names_no_engine_and_carries_settings_of_another_gets_the_words_it_always_got()
 {
    // An engine with no key table is refused by the settings check itself, with its own words.
    let w = World::new();
    let mut b = body(SONNET, None);
    b["engineConfig"] = json!({"engine": "pi", "schema": 1, "settings": {}});
    let (status, v) = start(&w, "k-1", &b).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "invalid-engine-config");
    assert_eq!(v["problem"], "wrong_engine");
    assert!(w.runs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b787_a_request_that_names_no_engine_and_carries_opencode_settings_is_refused_naming_both_engines()
 {
    // OpenCode has a key table, so the settings check passes; the run uses the default engine, and the pair is what is refused.
    let w = World::new();
    let mut b = body(SONNET, None);
    b["engineConfig"] = json!({"engine": "opencode", "schema": 1, "settings": {}});
    let (status, v) = start(&w, "k-1", &b).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], "invalid-engine-config");
    let words = v["message"].as_str().unwrap();
    assert!(
        words.contains("opencode") && words.contains("cline"),
        "{words}"
    );
    assert!(w.runs.lock().unwrap().is_empty());
}

#[test]
fn b787_every_engine_of_the_catalog_has_a_profile_in_the_build() {
    for engine in models::builtin_catalog().engines() {
        assert!(
            harness_core::engine_profile::known_engine(engine.as_str()).is_ok(),
            "models.json lists rows for {engine}, which has no engine profile"
        );
    }
}

#[test]
fn b787_the_model_list_of_a_build_with_only_cline_rows_has_no_engine_member() {
    let c = catalog_with_only_cline_rows();
    let v = serde_json::to_value(c.list()).unwrap();
    let rows = v["models"].as_array().unwrap();
    assert_eq!(rows.len(), 6);
    assert!(rows.iter().all(|r| r.get("engine").is_none()), "{v}");
    assert_eq!(v, serde_json::to_value(models::builtin().list()).unwrap());
}

#[tokio::test]
async fn b787_the_model_list_route_shows_the_cline_rows_as_ever_then_the_opencode_rows_naming_their_engine()
 {
    let w = World::new();
    let (status, v) = chat_support::get(&w, ME, "/v1/direct/models", &[]).await;
    assert_eq!(status, 200, "{v}");
    let rows = v["models"].as_array().unwrap();
    assert_eq!(rows.len(), 11, "{v}");
    assert!(rows[..6].iter().all(|r| r.get("engine").is_none()), "{v}");
    assert!(rows[6..].iter().all(|r| r["engine"] == "opencode"), "{v}");
    let cline: Vec<&Value> = rows[..6].iter().collect();
    assert_eq!(
        Value::Array(cline.into_iter().cloned().collect()),
        serde_json::to_value(models::builtin().list().models).unwrap()
    );
    let open: Vec<&str> = rows[6..]
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(!open.contains(&GLM), "{open:?}");
    assert_eq!(open.len(), 5);
    assert_eq!(
        v,
        serde_json::to_value(models::builtin_catalog().list()).unwrap()
    );
}

#[tokio::test]
async fn b787_a_start_on_opencode_with_a_model_that_has_an_opencode_record_is_accepted_and_stored_with_the_engine()
 {
    for (i, model) in [
        "us.anthropic.claude-opus-5-5",
        SONNET,
        SOL,
        "us.openai.gpt-6-astra",
    ]
    .into_iter()
    .enumerate()
    {
        let w = World::new();
        let (status, v) = start(&w, &format!("k-{i}"), &body(model, Some("opencode"))).await;
        assert_eq!(status, 201, "{model}: {v}");
        assert_eq!(stored_request(&w)["engine"], "opencode", "{model}");
        let run = w.runs.lock().unwrap().values().next().cloned().unwrap();
        assert_eq!(run.engine, Some(Engine::OPENCODE), "{model}");
    }
}

#[tokio::test]
async fn b787_a_start_on_opencode_with_glm_is_422_model_not_smoked_and_the_message_names_the_pair()
{
    let w = World::new();
    let (status, v) = start(&w, "k-1", &body(GLM, Some("opencode"))).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "model-not-smoked", "{v}");
    let message = v["message"].as_str().unwrap();
    assert!(
        message.contains("opencode") && message.contains(GLM),
        "{message}"
    );
    assert!(w.runs.lock().unwrap().is_empty());
    assert!(w.objects.lock().unwrap().is_empty());
    // The same model on the default engine is accepted: the refusal is about the pair.
    let (status, v) = start(&w, "k-2", &body(GLM, None)).await;
    assert_eq!(status, 201, "{v}");
}

#[test]
fn b787_the_catalog_rows_of_the_cost_route_name_the_engine_of_each_row() {
    use control_api::direct_models::{models_response, models_response_of_catalog};
    // The shipped list: the six Cline rows first, then one row for each model with an OpenCode record.
    let today = models_response();
    assert_eq!(today.rows.len(), 11);
    assert!(today.rows[..6].iter().all(|r| r.engine == Engine::CLINE));
    assert!(today.rows[6..].iter().all(|r| r.engine == Engine::OPENCODE));
    assert!(
        today.rows[6..]
            .iter()
            .all(|r| !r.smoke_record_id.is_empty())
    );
    assert!(today.rows[6..].iter().all(|r| r.profile_id != GLM));
    // With an OpenCode row that has a smoke record, the row names its engine; a row without a record is not selectable and is not shown.
    let c = catalog_with_opencode();
    let rows = models_response_of_catalog(&c).rows;
    assert_eq!(
        rows.len(),
        7,
        "{:?}",
        rows.iter()
            .map(|r| (&r.profile_id, r.engine))
            .collect::<Vec<_>>()
    );
    let open: Vec<_> = rows
        .iter()
        .filter(|r| r.engine == Engine::OPENCODE)
        .collect();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].profile_id, SOL);
    assert_eq!(open[0].smoke_record_id, "b787:test:opencode:sol");
}
