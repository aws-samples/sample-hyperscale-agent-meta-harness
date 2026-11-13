//! The model list of a direct run and the choice of the model a run uses.
//!
//! The file lists the five supported models, each with the one route it runs on, its context window, its smoke-record
//! id, a short `alias` (`opus`, `sonnet`, `sol`, `astra`, `glm`) and, on exactly one row, `"default": true`. A model is
//! selectable only with a passing smoke record (invariant 6). `ham`, the TUI and control-api all read this one parse:
//! control-api refuses a request whose `model` is not a selectable `id`; `ham` resolves what the user typed.
//!
//! The model a direct run uses, strongest first:
//!
//! 1. the `--model <id|alias>` flag,
//! 2. the `HAMH_MODEL` environment variable,
//! 3. `default_model` in `config.toml` (`ham config set default_model <id|alias>`, or the TUI's "save as default"),
//! 4. the built-in default, Claude Sonnet 5.5 ([`DEFAULT_MODEL_ID`]).
//!
//! An empty or blank environment or config value is unset. A value that is set and wrong is an error that names its source: it
//! never falls through to a weaker layer, so a typo in `HAMH_MODEL` cannot silently run another model.

pub use harness_protocol::api::v1::{ModelList, ModelRow};
use harness_protocol::common::{Engine, Protocol};
use harness_protocol::direct::{DirectModelSpec, MAX_DIRECT_ROUTES, REASONING_LEVELS};
use serde::Deserialize;
use std::sync::OnceLock;

const MODELS_JSON: &str = include_str!("../../../engine/catalog/models.json");

/// The model a run uses when the user sets nothing: Claude Sonnet 5.5. The row with `"default": true` in `models.json` must be
/// this one (pinned by `tests/models.rs`).
pub const DEFAULT_MODEL_ID: &str = "global.anthropic.claude-sonnet-5-5";

/// The other inference profile of a listed model (`us.` <-> `global.`), with its own passing smoke record. A run never names it
/// (`Models::check` accepts the model's own `id` only); it is a route agentd may switch to when the model's own route fails.
/// The window and the output limit are the model's.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlternateRoute {
    pub id: String,
    pub region: String,
    pub smoke_record_id: String,
    #[serde(default)]
    pub smoked_at: Option<String>,
}

/// The reasoning row of a listed model: the level the model reasons at when the user keeps `reasoning.effort = default`, the
/// launch levels its live table shows as accepted, and the record of that table. A model is never offered at a level `accepted` does not list.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelReasoning {
    /// One of the launch levels (`harness_protocol::direct::REASONING_LEVELS`); it is one of `accepted`.
    pub default: String,
    /// The launch levels whose live turn through the pinned engine passed (`engine/catalog/reasoning-levels.json`).
    pub accepted: Vec<String>,
    /// The live table the row stands on; required.
    pub smoke_record_id: String,
    #[serde(default)]
    pub smoked_at: Option<String>,
}

/// Prefixes of cross-Region inference profile ids (`dispatcher::policy`, `agentd::direct::failover`).
const GEO_PREFIXES: [&str; 8] = ["us", "us-gov", "eu", "apac", "jp", "au", "ca", "global"];

/// The model an id names without its profile type.
pub(crate) fn base_model(id: &str) -> &str {
    match id.split_once('.') {
        Some((p, rest)) if GEO_PREFIXES.contains(&p) => rest,
        _ => id,
    }
}

/// The engine of a row that names none: the default engine, the one every row was smoked with before the list had an engine dimension.
fn default_engine() -> Engine {
    Engine::DEFAULT
}

/// One listed model and its route.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Model {
    /// The engine this row was smoked with: the name of an engine profile, the default engine when the row names none. The same `id` may be listed once for
    /// each engine, each row with its own route, window and smoke record; the aliases and the `default` flag are per engine.
    #[serde(default = "default_engine")]
    pub engine: Engine,
    /// The Bedrock inference profile or model id: what a request's `model` names.
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// The short name the flag, `HAMH_MODEL` and the config file accept: lower-case letters, digits and `-`.
    #[serde(default)]
    pub alias: Option<String>,
    /// The built-in default of the list (exactly one row of `models.json`).
    #[serde(default)]
    pub default: bool,
    pub base_model_id: String,
    pub protocol: Protocol,
    pub region: String,
    pub context_tokens: u32,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub smoke_record_id: Option<String>,
    #[serde(default)]
    pub smoked_at: Option<String>,
    /// One line `ham models` prints under the table: what to know before choosing the model (absent for most rows).
    #[serde(default)]
    pub note: Option<String>,
    /// The model's other inference profiles, each with its own smoke record (see [`AlternateRoute`]).
    #[serde(default)]
    pub alternate_routes: Vec<AlternateRoute>,
    /// The reasoning level of the model and the levels it accepts. A row without it launches the engine with no reasoning argument.
    #[serde(default)]
    pub reasoning: Option<ModelReasoning>,
    /// The model runs only in an account in provider data sharing mode, and a run on it sends its prompts and answers to the model's provider.
    /// `ham` refuses it while `run.provider_sharing` is off, and control-api refuses a request without `providerSharing: true`.
    #[serde(default)]
    pub requires_provider_sharing: bool,
}

impl Model {
    /// The level this model reasons at when the user keeps `reasoning.effort = default`: the `default` of its reasoning row, for a selectable model.
    pub fn reasoning_default(&self) -> Option<&str> {
        self.reasoning
            .as_ref()
            .filter(|_| self.selectable())
            .map(|r| r.default.as_str())
    }

    /// The row lists `level` as accepted.
    pub fn accepts_reasoning(&self, level: &str) -> bool {
        self.reasoning
            .as_ref()
            .is_some_and(|r| r.accepted.iter().any(|a| a == level))
    }

    /// A passing smoke record exists, so the model may be chosen.
    pub fn selectable(&self) -> bool {
        self.smoke_record_id
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// Whether a run whose provider data sharing is `on` may use this model: a model that needs sharing is refused while it is off, and a model
    /// that needs nothing passes either way.
    pub fn check_sharing(&self, on: bool) -> Result<(), SharingRefusal> {
        if self.requires_provider_sharing && !on {
            return Err(SharingRefusal {
                id: self.id.clone(),
                name: self.name.clone(),
            });
        }
        Ok(())
    }
}

/// A run on a model that needs provider data sharing while the run does not agree to it. `ham` makes this refusal before anything is sent
/// (its [`Display`](std::fmt::Display) text); control-api makes it again for a request that lacks `providerSharing: true` (400
/// `provider-sharing-required`, [`SharingRefusal::server_message`]), so both sides fail closed in the same words.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{name} ({id}) needs provider data sharing, and the setting is off. With it on, the prompts and answers of a run on this model go to the model's provider, which may keep them up to 30 days for safety monitoring. Turn it on with `ham config set run.provider_sharing on`, in the Settings screen, or with `--provider-sharing on` for one run. Nothing was sent."
)]
pub struct SharingRefusal {
    /// The catalog id of the model.
    pub id: String,
    /// The model's display name.
    pub name: String,
}

impl SharingRefusal {
    /// The `error.code` of control-api's 400.
    pub fn code(&self) -> &'static str {
        "provider-sharing-required"
    }

    /// The same reason in the words of the control plane, which names the request member and not the client's three ways to set it.
    pub fn server_message(&self) -> String {
        format!(
            "model {:?} ({}) needs provider data sharing, and this request does not ask for it. With it on, the prompts and answers of a run on this model go to the model's provider, which may keep them up to 30 days for safety monitoring. Send \"providerSharing\": true to agree (ham: set run.provider_sharing to on).",
            self.id, self.name
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelsDoc {
    schema_version: u32,
    #[serde(default)]
    #[allow(dead_code)]
    about: Option<String>,
    models: Vec<Model>,
}

/// The file could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelsError {
    #[error("models.json: {0}")]
    Parse(String),
    #[error("models.json: {0}")]
    Invalid(String),
}

/// Why an explicit reasoning level is refused for a model.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReasoningRefusal {
    #[error(
        "model {model:?} has no passing smoke record at reasoning level {level:?}; its accepted levels are: {}. Set reasoning.effort to one of them, or to default or off",
        accepted.join(", ")
    )]
    NotAccepted {
        model: String,
        level: String,
        accepted: Vec<String>,
    },
    #[error(
        "model {model:?} has no reasoning record, so reasoning.effort can only be default or off for it"
    )]
    NoRecord { model: String },
}

impl ReasoningRefusal {
    /// The `error.code` of the 422.
    pub fn code(&self) -> &'static str {
        "reasoning-level-not-accepted"
    }
}

/// Why a request's `model` is refused (control-api's 422).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelRefusal {
    #[error("model {id:?} is not one of the supported models: {}", known.join(", "))]
    Unknown { id: String, known: Vec<String> },
    #[error("model {id:?} has no passing smoke record, so it is not selectable")]
    NotSmoked { id: String },
    /// The model is listed, but not for this engine, or not smoked with it. The message names the pair.
    #[error(
        "engine {engine} has no passing smoke record for model {id:?}, so that pair is not selectable"
    )]
    NotSmokedForEngine { engine: String, id: String },
    /// No engine lists the model; `known` are the models with a passing smoke record for this engine (never empty).
    #[error("model {id:?} is not one of the models supported with engine {engine}: {}", known.join(", "))]
    UnknownForEngine {
        engine: String,
        id: String,
        known: Vec<String>,
    },
}

impl ModelRefusal {
    /// The `error.code` of the 422.
    pub fn code(&self) -> &'static str {
        match self {
            ModelRefusal::Unknown { .. } | ModelRefusal::UnknownForEngine { .. } => "unknown-model",
            ModelRefusal::NotSmoked { .. } | ModelRefusal::NotSmokedForEngine { .. } => {
                "model-not-smoked"
            }
        }
    }
}

fn valid_alias(a: &str) -> bool {
    (1..=24).contains(&a.len())
        && a.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !a.starts_with('-')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Models {
    models: Vec<Model>,
}

fn read_doc(text: &str) -> Result<ModelsDoc, ModelsError> {
    let doc: ModelsDoc =
        serde_json::from_str(text).map_err(|e| ModelsError::Parse(e.to_string()))?;
    if doc.schema_version != 1 {
        return Err(ModelsError::Invalid(format!(
            "schemaVersion is {}; this build reads 1",
            doc.schema_version
        )));
    }
    Ok(doc)
}

impl Models {
    /// Parses and checks a `models.json` document and returns the default engine's list: the rows that name no engine or name the default one. The rows of another
    /// engine are the business of [`Catalog::parse`], which checks them; every caller that predates the engine dimension keeps the list it had.
    pub fn parse(text: &str) -> Result<Models, ModelsError> {
        let doc = read_doc(text)?;
        let rows = doc
            .models
            .into_iter()
            .filter(|m| m.engine == Engine::DEFAULT)
            .collect();
        Models::from_rows(rows)
    }

    /// The engine this list is for (that of its first row; the default engine for a list with none).
    pub fn engine(&self) -> Engine {
        self.models.first().map_or(Engine::DEFAULT, |m| m.engine)
    }

    /// Checks the rows of ONE engine: ids, windows, alternate routes, aliases, reasoning rows and the default.
    fn from_rows(rows: Vec<Model>) -> Result<Models, ModelsError> {
        let doc = ModelsDoc {
            schema_version: 1,
            about: None,
            models: rows,
        };
        if doc.models.is_empty() {
            return Err(ModelsError::Invalid("the file lists no models".into()));
        }
        let invalid = |m: String| Err(ModelsError::Invalid(m));
        let mut seen: Vec<&str> = Vec::new();
        for m in &doc.models {
            if m.id.trim().is_empty() || m.id.chars().any(char::is_whitespace) {
                return invalid(format!(
                    "model id {:?} must be non-empty with no whitespace",
                    m.id
                ));
            }
            if seen.contains(&m.id.as_str()) {
                return invalid(format!("model {} is listed twice", m.id));
            }
            seen.push(&m.id);
            if m.context_tokens == 0 {
                return invalid(format!(
                    "model {}: contextTokens must be the route's known window, not 0",
                    m.id
                ));
            }
            if m.max_output_tokens == 0 {
                return invalid(format!(
                    "model {}: maxOutputTokens must be at least 1",
                    m.id
                ));
            }
        }
        let mut route_ids: Vec<&str> = seen.clone();
        for m in &doc.models {
            for a in &m.alternate_routes {
                if a.id.trim().is_empty() || a.id.chars().any(char::is_whitespace) {
                    return invalid(format!(
                        "model {}: alternate route id {:?} must be non-empty with no whitespace",
                        m.id, a.id
                    ));
                }
                if route_ids.contains(&a.id.as_str()) {
                    return invalid(format!(
                        "model {}: alternate route {} repeats a model or another route",
                        m.id, a.id
                    ));
                }
                route_ids.push(&a.id);
                if base_model(&a.id) != base_model(&m.id) {
                    return invalid(format!(
                        "model {}: alternate route {} is another model, not another profile of this one",
                        m.id, a.id
                    ));
                }
                if a.smoke_record_id.trim().is_empty() {
                    return invalid(format!(
                        "model {}: alternate route {} has no passing smoke record, so it is never offered",
                        m.id, a.id
                    ));
                }
                if a.region.trim().is_empty() {
                    return invalid(format!(
                        "model {}: alternate route {} needs a Region",
                        m.id, a.id
                    ));
                }
            }
        }
        let mut aliases: Vec<&str> = Vec::new();
        for m in &doc.models {
            if let Some(a) = m.alias.as_deref() {
                if !valid_alias(a) {
                    return invalid(format!(
                        "model {}: alias {a:?} must be 1-24 lower-case letters, digits or '-', not starting with '-'",
                        m.id
                    ));
                }
                if aliases.contains(&a) {
                    return invalid(format!("alias {a:?} is used by two models"));
                }
                if seen.contains(&a) {
                    return invalid(format!("alias {a:?} of model {} is also a model id", m.id));
                }
                aliases.push(a);
            }
        }
        for m in &doc.models {
            let Some(r) = &m.reasoning else { continue };
            let bad = |why: &str| invalid(format!("model {}: reasoning {why}", m.id));
            if r.smoke_record_id.trim().is_empty() {
                return bad("needs the smokeRecordId of the live table it stands on");
            }
            if r.accepted.is_empty() {
                return bad("accepts no level");
            }
            for (i, level) in r.accepted.iter().enumerate() {
                if !REASONING_LEVELS.contains(&level.as_str()) {
                    return bad(&format!(
                        "lists {level:?}, which is not a launch level ({})",
                        REASONING_LEVELS.join(", ")
                    ));
                }
                if r.accepted[..i].contains(level) {
                    return bad(&format!("lists {level:?} twice"));
                }
            }
            if !r.accepted.contains(&r.default) {
                return bad(&format!(
                    "default {:?} is not one of the accepted levels ({})",
                    r.default,
                    r.accepted.join(", ")
                ));
            }
        }
        let defaults: Vec<&Model> = doc.models.iter().filter(|m| m.default).collect();
        if defaults.len() > 1 {
            return invalid("more than one model is marked default".into());
        }
        if let Some(d) = defaults.first() {
            if !d.selectable() {
                return invalid(format!(
                    "the default model {} has no passing smoke record",
                    d.id
                ));
            }
            if d.alias.is_none() {
                return invalid(format!("the default model {} needs an alias", d.id));
            }
        }
        Ok(Models { models: doc.models })
    }

    /// Every listed model, in file order.
    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// The row marked `"default": true`.
    pub fn default_model(&self) -> Option<&Model> {
        self.models.iter().find(|m| m.default)
    }

    /// The row a typed `id` (exact) or `alias` (any case) names, selectable or not, after trimming white space.
    pub fn find(&self, input: &str) -> Option<&Model> {
        let t = input.trim();
        self.models.iter().find(|m| m.id == t).or_else(|| {
            self.models
                .iter()
                .find(|m| m.alias.as_deref() == Some(t.to_ascii_lowercase().as_str()))
        })
    }

    /// The row whose own route or one of whose alternate routes has exactly this id (a run that switched route ends on an alternate
    /// route, and the card names the model by its row).
    pub fn find_route(&self, id: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.id == id || m.alternate_routes.iter().any(|a| a.id == id))
    }

    /// The selectable model `id` names (an exact id: what a request carries).
    pub fn check(&self, id: &str) -> Result<&Model, ModelRefusal> {
        match self.models.iter().find(|m| m.id == id) {
            Some(m) if m.selectable() => Ok(m),
            Some(_) => Err(ModelRefusal::NotSmoked { id: id.to_string() }),
            None => Err(ModelRefusal::Unknown {
                id: id.to_string(),
                known: self
                    .models
                    .iter()
                    .filter(|m| m.selectable())
                    .map(|m| m.id.clone())
                    .collect(),
            }),
        }
    }

    /// Whether `level` (a launch level the user named in `reasoning.effort`) may be asked of the model `model_id` (an exact id). A model this list does
    /// not know is the business of [`Models::check`], so it passes here.
    pub fn check_reasoning(&self, model_id: &str, level: &str) -> Result<(), ReasoningRefusal> {
        let Some(m) = self.models.iter().find(|m| m.id == model_id) else {
            return Ok(());
        };
        match &m.reasoning {
            None => Err(ReasoningRefusal::NoRecord {
                model: m.id.clone(),
            }),
            Some(r) if r.accepted.iter().any(|a| a == level) => Ok(()),
            Some(r) => Err(ReasoningRefusal::NotAccepted {
                model: m.id.clone(),
                level: level.to_string(),
                accepted: r.accepted.clone(),
            }),
        }
    }

    /// The list as the server and `ham models --json` show it.
    pub fn list(&self) -> ModelList {
        ModelList {
            default: self
                .default_model()
                .map(|m| m.id.clone())
                .unwrap_or_default(),
            models: self
                .models
                .iter()
                .map(|m| ModelRow {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    vendor: m.vendor.clone(),
                    alias: m.alias.clone(),
                    protocol: m.protocol,
                    region: m.region.clone(),
                    context_tokens: m.context_tokens,
                    max_output_tokens: m.max_output_tokens,
                    selectable: m.selectable(),
                    default: m.default,
                    note: m.note.clone(),
                    // A row of the default engine names no engine, so the list of a build with only those rows is the list it always was.
                    engine: (m.engine != Engine::DEFAULT).then_some(m.engine),
                    requires_provider_sharing: m.requires_provider_sharing,
                })
                .collect(),
        }
    }

    /// The routes agentd may switch to when the run's own route fails: the other profiles of the
    /// chosen model first, then every other selectable model, the same vendor first and the rest in file order, each model's own
    /// route followed by its other profiles. Never the chosen route, never a route twice, at most [`MAX_DIRECT_ROUTES`], and only
    /// routes with a passing smoke record. `chosen` is a model id or the id of one of its alternate routes; an unknown id has none. A model that needs
    /// provider data sharing is left out; [`Models::failover_routes_for`] with sharing on includes it.
    pub fn failover_routes(&self, chosen: &str) -> Vec<DirectModelSpec> {
        self.failover_routes_for(chosen, false)
    }

    /// [`Models::failover_routes`] for a run whose request carries provider data sharing `sharing`: a model whose row has
    /// `requires_provider_sharing` is a route only for a run that is on such a model itself and has sharing on. A run on a model that needs nothing never
    /// switches to one, whatever its setting says: it chose its model, not that provider. The chosen model's own other profiles are always routes.
    pub fn failover_routes_for(&self, chosen: &str, sharing: bool) -> Vec<DirectModelSpec> {
        let Some(home) = self
            .models
            .iter()
            .find(|m| m.id == chosen || m.alternate_routes.iter().any(|a| a.id == chosen))
        else {
            return Vec::new();
        };
        let allow_sharing = sharing && home.requires_provider_sharing;
        let mut out: Vec<DirectModelSpec> = Vec::new();
        let mut push = |spec: DirectModelSpec| {
            if spec.id != chosen && !out.iter().any(|r| r.id == spec.id) {
                out.push(spec);
            }
        };
        for spec in routes_of(home) {
            push(spec);
        }
        let others = self.models.iter().filter(|m| {
            m.id != home.id && m.selectable() && (allow_sharing || !m.requires_provider_sharing)
        });
        for same_vendor in [true, false] {
            for m in others
                .clone()
                .filter(|m| (m.vendor == home.vendor) == same_vendor)
            {
                for spec in routes_of(m) {
                    push(spec);
                }
            }
        }
        out.truncate(MAX_DIRECT_ROUTES);
        out
    }

    /// The selectable models as `alias = id` words, for an error message.
    pub fn choices(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.selectable())
            .map(|m| match &m.alias {
                Some(a) => format!("{a} = {}", m.id),
                None => m.id.clone(),
            })
            .collect()
    }
}

/// The model list of every engine: one [`Models`] list for each engine that has rows in `models.json`, the default engine first. An (engine, model) pair is
/// selectable only when the engine's own list has a row for the model with a passing smoke record ([`Catalog::check`]). The lists are independent:
/// ids, aliases, the default and the failover routes of one engine never mix with another's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    /// The default engine's list first, then the other engines in the order their first row appears in the file.
    lists: Vec<Models>,
}

impl Catalog {
    /// Parses and checks a `models.json` document: the rows are grouped by `engine` (the default engine when absent) and each group is checked as a list.
    pub fn parse(text: &str) -> Result<Catalog, ModelsError> {
        let doc = read_doc(text)?;
        let mut order: Vec<Engine> = vec![Engine::DEFAULT];
        for m in &doc.models {
            if !order.contains(&m.engine) {
                order.push(m.engine);
            }
        }
        let mut lists = Vec::new();
        for engine in order {
            let rows: Vec<Model> = doc
                .models
                .iter()
                .filter(|m| m.engine == engine)
                .cloned()
                .collect();
            let list =
                Models::from_rows(rows).map_err(|e| match (engine == Engine::DEFAULT, e) {
                    (true, e) => e,
                    (false, ModelsError::Invalid(m) | ModelsError::Parse(m)) => {
                        ModelsError::Invalid(format!("engine {engine}: {m}"))
                    }
                })?;
            lists.push(list);
        }
        // A model that needs provider data sharing needs it on every engine that lists it: the agreement is about the model's provider and not about the
        // engine, so a row that leaves the mark off would let a request for the same model on another engine skip it. The list is refused when two rows of one
        // model disagree.
        for (i, a) in lists.iter().enumerate() {
            for m in a.models() {
                for b in lists.iter().skip(i + 1) {
                    if let Some(twin) = b.models().iter().find(|t| t.id == m.id)
                        && twin.requires_provider_sharing != m.requires_provider_sharing
                    {
                        return Err(ModelsError::Invalid(format!(
                            "model {}: requiresProviderSharing is {} on engine {} and {} on engine {}; a model that needs provider data sharing needs it on every engine",
                            m.id,
                            m.requires_provider_sharing,
                            a.engine(),
                            twin.requires_provider_sharing,
                            b.engine()
                        )));
                    }
                }
            }
        }
        Ok(Catalog { lists })
    }

    /// Every engine that has rows, the default engine first.
    pub fn engines(&self) -> Vec<Engine> {
        self.lists.iter().map(Models::engine).collect()
    }

    /// The list of one engine; `None` for an engine with no rows.
    pub fn for_engine(&self, engine: Engine) -> Option<&Models> {
        self.lists.iter().find(|l| l.engine() == engine)
    }

    /// The row of the model `id` in any engine's list when it needs provider data sharing. A resume or a retry starts a stored request,
    /// of which the client knows the model id alone.
    pub fn sharing_row(&self, id: &str) -> Option<&Model> {
        self.lists
            .iter()
            .flat_map(|l| l.models().iter())
            .find(|m| m.id == id && m.requires_provider_sharing)
    }

    /// The default engine's list: what every caller that predates the engine dimension reads.
    pub fn default_list(&self) -> &Models {
        &self.lists[0]
    }

    /// The selectable model `id` names for `engine` (an exact id: what a request carries). A pair is accepted only with a row of that engine that has a
    /// passing smoke record. For the default engine the answer and the words are those of [`Models::check`].
    pub fn check(&self, engine: Engine, id: &str) -> Result<&Model, ModelRefusal> {
        if engine == Engine::DEFAULT {
            return self.default_list().check(id);
        }
        let own = self.for_engine(engine);
        if let Some(m) = own.and_then(|l| l.models().iter().find(|m| m.id == id)) {
            if m.selectable() {
                return Ok(m);
            }
            return Err(ModelRefusal::NotSmokedForEngine {
                engine: engine.to_string(),
                id: id.to_string(),
            });
        }
        let listed_elsewhere = self
            .lists
            .iter()
            .any(|l| l.models().iter().any(|m| m.id == id));
        let known: Vec<String> = own
            .map(|l| {
                l.models()
                    .iter()
                    .filter(|m| m.selectable())
                    .map(|m| m.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if listed_elsewhere || known.is_empty() {
            Err(ModelRefusal::NotSmokedForEngine {
                engine: engine.to_string(),
                id: id.to_string(),
            })
        } else {
            Err(ModelRefusal::UnknownForEngine {
                engine: engine.to_string(),
                id: id.to_string(),
                known,
            })
        }
    }

    /// The list as the server and `ham models --json` show it: the rows of the default engine as ever, then the rows of the other engines, each naming its engine.
    /// `default` is the default engine's default model; the default model of another engine is the row of that engine marked `default`.
    pub fn list(&self) -> ModelList {
        let mut out = self.default_list().list();
        for l in self.lists.iter().skip(1) {
            out.models.extend(l.list().models);
        }
        out
    }

    /// The routes agentd may switch to when the run's own route fails, for a run of `engine`: [`Models::failover_routes`] of that engine's list, so a
    /// switch never goes to a pair that engine has no smoke record for. Empty for an engine with no rows.
    pub fn failover_routes(&self, engine: Engine, chosen: &str) -> Vec<DirectModelSpec> {
        self.failover_routes_for(engine, chosen, false)
    }

    /// [`Catalog::failover_routes`] for a run whose request carries provider data sharing `sharing`: [`Models::failover_routes_for`] of the engine's list,
    /// so a model that needs sharing is a route only for a run that is on such a model itself and has sharing on.
    pub fn failover_routes_for(
        &self,
        engine: Engine,
        chosen: &str,
        sharing: bool,
    ) -> Vec<DirectModelSpec> {
        self.for_engine(engine)
            .map(|l| l.failover_routes_for(chosen, sharing))
            .unwrap_or_default()
    }
}

/// The lists this build ships (`engine/catalog/models.json`, compiled in).
pub fn builtin_catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        // The file is committed and `tests/models.rs` parses it, so a failure here is a build defect.
        Catalog::parse(MODELS_JSON)
            .unwrap_or_else(|e| panic!("the committed engine/catalog/models.json is unusable: {e}"))
    })
}

/// The default engine's list this build ships (`engine/catalog/models.json`, compiled in).
pub fn builtin() -> &'static Models {
    builtin_catalog().default_list()
}

/// Which layer a chosen model came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    Flag,
    Env,
    Config,
    BuiltIn,
    /// A value typed to `ham config set default_model`, which is checked before it is stored.
    Given,
}

impl ModelSource {
    /// What the user set (shown in `ham models`, in errors and in the composer).
    pub fn label(self) -> &'static str {
        match self {
            ModelSource::Flag => "--model",
            ModelSource::Env => "HAMH_MODEL",
            ModelSource::Config => "config default_model",
            ModelSource::BuiltIn => "built-in default",
            ModelSource::Given => "the value given",
        }
    }

    /// ` (from <label>)` for an error text; nothing for a value just typed to `ham config set`.
    fn origin_text(self) -> String {
        match self {
            ModelSource::Given => String::new(),
            other => format!(" (from {})", other.label()),
        }
    }

    /// How to fix a wrong value from this source (empty for the built-in default, which cannot be wrong).
    pub fn fix_hint(self) -> &'static str {
        match self {
            ModelSource::Flag => "",
            ModelSource::Env => "; unset HAMH_MODEL, or set it to one of these",
            ModelSource::Config => {
                "; fix it with ham config set default_model <alias>, or remove it with ham config unset default_model"
            }
            ModelSource::BuiltIn | ModelSource::Given => "",
        }
    }
}

/// The two weaker layers, read by the caller (`ham` reads `HAMH_MODEL` and `config.toml`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelPrefs {
    /// `HAMH_MODEL`.
    pub env: Option<String>,
    /// `default_model` of `config.toml`.
    pub config: Option<String>,
}

/// The model a run uses and where the choice came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved<'a> {
    pub model: &'a Model,
    pub source: ModelSource,
}

/// Why no model could be chosen.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("{} needs a model id or alias (ham models lists them)", from.label())]
    Empty { from: ModelSource },
    #[error("unknown model {input:?}{}; choose one of: {}{}", from.origin_text(), known.join(", "), from.fix_hint())]
    Unknown {
        input: String,
        from: ModelSource,
        known: Vec<String>,
    },
    #[error("model {input:?}{} is listed as {id} but has no passing smoke record, so it is not selectable yet{}", from.origin_text(), from.fix_hint())]
    NotSmoked {
        input: String,
        from: ModelSource,
        id: String,
    },
    #[error("the model list names no default model")]
    NoDefault,
    /// The chosen model needs provider data sharing and the run has it off.
    #[error(transparent)]
    NeedsSharing(#[from] SharingRefusal),
}

/// The model for a run: `flag` (an explicit `--model` value, even an empty one), else `prefs.env`, else `prefs.config`, else the
/// list's default. See the module text for the order.
pub fn resolve<'a>(
    models: &'a Models,
    flag: Option<&str>,
    prefs: &ModelPrefs,
) -> Result<Resolved<'a>, ModelError> {
    let blank = |s: &&String| !s.trim().is_empty();
    let (input, source) = if let Some(f) = flag {
        (f.to_string(), ModelSource::Flag)
    } else if let Some(e) = prefs.env.as_ref().filter(blank) {
        (e.clone(), ModelSource::Env)
    } else if let Some(c) = prefs.config.as_ref().filter(blank) {
        (c.clone(), ModelSource::Config)
    } else {
        let model = models.default_model().ok_or(ModelError::NoDefault)?;
        return Ok(Resolved {
            model,
            source: ModelSource::BuiltIn,
        });
    };
    lookup(models, input, source)
}

/// `value` as typed to `ham config set default_model`: the same lookup, with no layer to blame.
pub fn resolve_given<'a>(models: &'a Models, value: &str) -> Result<Resolved<'a>, ModelError> {
    lookup(models, value.to_string(), ModelSource::Given)
}

fn lookup<'a>(
    models: &'a Models,
    input: String,
    source: ModelSource,
) -> Result<Resolved<'a>, ModelError> {
    if input.trim().is_empty() {
        return Err(ModelError::Empty { from: source });
    }
    match models.find(&input) {
        Some(model) if model.selectable() => Ok(Resolved { model, source }),
        Some(model) => Err(ModelError::NotSmoked {
            input,
            from: source,
            id: model.id.clone(),
        }),
        None => Err(ModelError::Unknown {
            input,
            from: source,
            known: models.choices(),
        }),
    }
}

/// The text of `ham models`: one line per model with its alias, id, name, window and notes, the default marked, the model the
/// next run uses marked with `*` (`selected` is its id and where the choice came from), and the ways to change the choice.
pub fn render_models(list: &ModelList, selected: Option<(&str, ModelSource)>) -> String {
    let mut out = String::from(
        "  alias   model id                            name                context  notes\n",
    );
    for m in &list.models {
        let mut notes: Vec<String> = Vec::new();
        if m.default {
            notes.push("default".into());
        }
        if let Some((id, source)) = selected
            && id == m.id
        {
            notes.push(format!("next run ({})", source.label()));
        }
        if m.requires_provider_sharing {
            notes.push("needs provider data sharing".into());
        }
        if !m.selectable {
            notes.push("pending: no passing smoke record, not selectable".into());
        }
        let mark = match selected {
            Some((id, _)) if id == m.id => '*',
            _ => ' ',
        };
        out.push_str(&format!(
            "{mark} {:<7} {:<35} {:<19} {:>8}  {}\n",
            m.alias.as_deref().unwrap_or("-"),
            m.id,
            m.name,
            m.context_tokens,
            notes.join("; ")
        ));
    }
    let noted: Vec<&ModelRow> = list.models.iter().filter(|m| m.note.is_some()).collect();
    if !noted.is_empty() {
        out.push_str("\nNotes:\n");
        for m in noted {
            out.push_str(&format!(
                "  {:<7} {}\n",
                m.alias.as_deref().unwrap_or(m.id.as_str()),
                m.note.as_deref().unwrap_or_default()
            ));
        }
    }
    if list.models.iter().any(|m| m.requires_provider_sharing) {
        out.push_str(
            "\nProvider data sharing:\n  A model marked \"needs provider data sharing\" can be chosen only with run.provider_sharing on (`ham config set run.provider_sharing on`, or --provider-sharing on for one run).\n  With it on, the prompts and answers of a run on that model go to the model's provider, which may keep them up to 30 days for safety monitoring.\n  The AWS account must also be in a mode that lets the model run (provider_data_share or aws_review); before such a run ham switches default or inherit to provider_data_share with your credentials, and `ham doctor` shows the mode.\n",
        );
    }
    out.push_str(
        "\nPick one run's model with --model <alias|id>, or set HAMH_MODEL; save a default with `ham config set default_model <alias|id>`\n(or in the TUI composer). Order: --model, HAMH_MODEL, config default_model, built-in default (Claude Sonnet 5.5).\n",
    );
    out
}

/// A model's own route followed by its alternate routes, as the session needs them. A model without a smoke record has none.
fn routes_of(m: &Model) -> Vec<DirectModelSpec> {
    if !m.selectable() {
        return Vec::new();
    }
    let spec = |id: &str, region: &str| DirectModelSpec {
        id: id.to_string(),
        region: region.to_string(),
        protocol: m.protocol,
        context_tokens: m.context_tokens,
        max_output_tokens: m.max_output_tokens,
        default_reasoning_effort: m.reasoning_default().map(str::to_string),
    };
    let mut v = vec![spec(&m.id, &m.region)];
    v.extend(m.alternate_routes.iter().map(|a| spec(&a.id, &a.region)));
    v
}
