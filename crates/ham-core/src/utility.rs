//! The utility model: the small, cheap model that the solution itself calls for helper text outside the coding engine, starting
//! with the run name. It is not one of the user's models: a person never picks it, `models.json` never lists it, and it has its own
//! catalog (`assets/utility-models.json`, read with the rules of `engine/catalog/models.json`) and its own switch (`HAMH_UTILITY_MODEL` at deploy, `UTILITY_MODEL`
//! in a Lambda). This module is the pure half: the catalog parser and its resolution, the Converse request body, the reply and the error
//! classes, and [`bedrock_rejects`], the one function every fake of the service uses to refuse what the service refuses. The call itself
//! (signing, sending, timing) is the caller's: `run-session` over `agentd::http::SignedHttp`.
//!
//! A helper's text is display text. It never changes what a run does and it is never an outcome label (root invariant 1). A function whose
//! output would change what a run does does not take the utility model; it names its own model and its own gate.
//!
//! Each helper function is a [`UtilityPurpose`] and has its own prompt, so it has its own smoke record: the top-level record of a catalog row
//! is the record of the run name, and the optional `purposes` object of a row holds the records of the others. A purpose is served
//! only by a row that holds a record for that purpose; it never falls through to another purpose's record or to another row.
//!
//! Two pieces are shared by every helper, so each helper sends the same safe text and reads a reply with the same safety checks. They sit
//! at the end of this module: [`excerpt`] and [`is_short`], the part of a prompt a helper may send (credentials hidden, control bytes
//! stripped, cut, angle brackets escaped), and [`guard_raw`], the checks on the whole raw text of a reply. The run name (`runname`) and the
//! run tags (`runtag`) both call them.

use crate::models::{self, AlternateRoute};
use crate::redact::redact_tokens;
use crate::sanitize::{marker, sanitize};
use harness_protocol::common::Protocol;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;
use std::time::Duration;

/// The catalog this build ships: one row today, Claude Haiku 5.5, with the records of its passing smoke runs beside it
/// (`assets/smoke-utility-us.anthropic.claude-haiku-5-5.json` for the run name, `assets/smoke-utility-tags-us.anthropic.claude-haiku-5-5.json`
/// for the run tags).
const CATALOG_JSON: &str = include_str!("../assets/utility-models.json");

/// The smoke records this build holds, by file name: the files beside the catalog that a record id may name. A file that is missing here is
/// a compile error (`include_str!`), and a record id that names a file not listed here fails [`UtilityModels::builtin`].
const RECORDS: [(&str, &str); 2] = [
    (
        "smoke-utility-us.anthropic.claude-haiku-5-5.json",
        include_str!("../assets/smoke-utility-us.anthropic.claude-haiku-5-5.json"),
    ),
    (
        "smoke-utility-tags-us.anthropic.claude-haiku-5-5.json",
        include_str!("../assets/smoke-utility-tags-us.anthropic.claude-haiku-5-5.json"),
    ),
];

/// The Bedrock routes the live service is known to answer for the utility purpose. [`bedrock_rejects`] treats any other id as unknown;
/// a caller whose catalog holds other routes names them with [`bedrock_rejects_with`].
pub const KNOWN_ROUTE_IDS: [&str; 2] = [
    "us.anthropic.claude-haiku-5-5",
    "global.anthropic.claude-haiku-5-5",
];

/// One utility model and its route. Like a `models.json` row, but without the window and output limit (the utility call sets its own caps).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UtilityModel {
    /// The Bedrock inference profile id the call names (the bare model id is refused by the service).
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// The short name `HAMH_UTILITY_MODEL` accepts: 1-24 of `a-z`, `0-9`, `-`.
    pub alias: String,
    pub base_model_id: String,
    pub protocol: Protocol,
    pub region: String,
    /// Sent as `additionalModelRequestFields` (for Haiku 5.5 `{"thinking": {"type": "disabled"}}`).
    #[serde(default)]
    pub additional_model_request_fields: Option<Value>,
    /// The record of the passing smoke run; absent or blank means the row is listed but not selectable (root invariant 6).
    #[serde(default)]
    pub smoke_record_id: Option<String>,
    #[serde(default)]
    pub smoked_at: Option<String>,
    /// The model's other inference profiles, each with its own smoke record (the same shape as in `models.json`).
    #[serde(default)]
    pub alternate_routes: Vec<AlternateRoute>,
    /// The smoke records of the purposes beyond the run name, keyed by purpose word. The top-level record above is the run-name record, so a row
    /// without this object serves run names only.
    #[serde(default)]
    pub purposes: PurposeRecords,
}

/// The passing smoke record of one purpose: the record id (ending in the name of a JSON file beside the catalog) and the time its run started.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PurposeRecord {
    pub smoke_record_id: String,
    pub smoked_at: String,
}

/// The `purposes` object of a row: one optional record per purpose word. An unknown word, a repeated word and anything but an object are
/// refused (the derived reader of a struct would take an empty list for an empty object). `run-name` may be repeated here only as an exact
/// copy of the row's top-level record (the TypeScript reader of the infra reads the top level only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PurposeRecords {
    pub run_name: Option<PurposeRecord>,
    pub run_tags: Option<PurposeRecord>,
}

impl<'de> Deserialize<'de> for PurposeRecords {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};
        const WORDS: &[&str] = &["run-name", "run-tags"];
        struct Records;
        impl<'de> Visitor<'de> for Records {
            type Value = PurposeRecords;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object with a smoke record per purpose word")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PurposeRecords, A::Error> {
                let mut out = PurposeRecords::default();
                while let Some(key) = map.next_key::<String>()? {
                    let (word, slot): (&'static str, &mut Option<PurposeRecord>) =
                        match key.as_str() {
                            "run-name" => ("run-name", &mut out.run_name),
                            "run-tags" => ("run-tags", &mut out.run_tags),
                            other => return Err(A::Error::unknown_field(other, WORDS)),
                        };
                    if slot.is_some() {
                        return Err(A::Error::duplicate_field(word));
                    }
                    *slot = Some(map.next_value()?);
                }
                Ok(out)
            }
        }
        d.deserialize_map(Records)
    }
}

fn non_blank(s: &str) -> bool {
    !s.trim().is_empty()
}

/// The name of the file a record id ends in: the text after the last `/`, which must be a plain `.json` file name.
fn record_file(id: &str) -> Option<&str> {
    let file = id.rsplit('/').next()?;
    let plain = file.len() > ".json".len()
        && file.ends_with(".json")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    plain.then_some(file)
}

impl UtilityModel {
    /// A passing smoke record exists, so the model may be used.
    pub fn selectable(&self) -> bool {
        self.smoke_record_id
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// A passing smoke record exists for `purpose`, so the model may be used for it. [`UtilityModel::selectable`] is this for the run name.
    pub fn selectable_for(&self, purpose: UtilityPurpose) -> bool {
        self.record_id_for(purpose).is_some_and(non_blank)
    }

    /// The id of the smoke record of `purpose`: the top-level record for the run name, the `purposes` entry for the others.
    pub fn record_id_for(&self, purpose: UtilityPurpose) -> Option<&str> {
        match purpose {
            UtilityPurpose::RunName => self.smoke_record_id.as_deref(),
            UtilityPurpose::RunTags => self
                .purposes
                .run_tags
                .as_ref()
                .map(|r| r.smoke_record_id.as_str()),
        }
    }

    /// The time the smoke run of `purpose` started.
    pub fn smoked_at_for(&self, purpose: UtilityPurpose) -> Option<&str> {
        match purpose {
            UtilityPurpose::RunName => self.smoked_at.as_deref(),
            UtilityPurpose::RunTags => self
                .purposes
                .run_tags
                .as_ref()
                .map(|r| r.smoked_at.as_str()),
        }
    }

    /// The row runs in the Region of the stage that calls it.
    pub fn check_region(&self, stage_region: &str) -> Result<(), UtilityError> {
        if self.region == stage_region {
            Ok(())
        } else {
            Err(UtilityError::Region {
                id: self.id.clone(),
                model_region: self.region.clone(),
                stage_region: stage_region.to_string(),
            })
        }
    }

    /// The row's own route and its alternates, in that order.
    pub fn route_ids(&self) -> Vec<&str> {
        std::iter::once(self.id.as_str())
            .chain(self.alternate_routes.iter().map(|a| a.id.as_str()))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogDoc {
    schema_version: u32,
    #[serde(default)]
    #[allow(dead_code)]
    about: Option<String>,
    /// The id or alias of the row an unset setting means.
    default: String,
    models: Vec<UtilityModel>,
}

/// The catalog file could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UtilityCatalogError {
    #[error("utility-models.json: {0}")]
    Parse(String),
    #[error("utility-models.json: {0}")]
    Invalid(String),
}

/// A setting that names no usable utility model, or a row that does not fit the stage.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UtilityError {
    #[error(
        "{origin} is {value:?}, which is not a utility model; {}",
        choices_hint(choices)
    )]
    Unknown {
        origin: String,
        value: String,
        choices: Vec<String>,
    },
    #[error(
        "{origin} names the utility model {id}, which has no passing smoke record for {}, so it is not selectable",
        purpose.as_str()
    )]
    NotSmoked {
        origin: String,
        id: String,
        purpose: UtilityPurpose,
    },
    #[error(
        "the utility model {id} runs in Region {model_region}, but the stage runs in {stage_region}"
    )]
    Region {
        id: String,
        model_region: String,
        stage_region: String,
    },
}

/// What to say a setting may be: the aliases that serve the purpose, or only `off` when no row does.
fn choices_hint(choices: &[String]) -> String {
    if choices.is_empty() {
        "use off".to_string()
    } else {
        format!("use one of: {}, or off", choices.join(", "))
    }
}

/// What a setting means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtilityChoice<'a> {
    Model(&'a UtilityModel),
    /// The setting is `off`: no helper call is made.
    Disabled,
}

/// The parsed catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtilityModels {
    models: Vec<UtilityModel>,
    default: usize,
}

fn valid_alias(a: &str) -> bool {
    (1..=24).contains(&a.len())
        && a.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !a.starts_with('-')
}

fn blank_or_spaced(id: &str) -> bool {
    id.trim().is_empty() || id.chars().any(char::is_whitespace)
}

/// The rules of the `purposes` object of one row: every record has a smoke record id that ends in a plain `.json` file name and a start
/// time, and `run-name` is only an exact copy of the top-level record (which must then exist).
fn check_purposes(m: &UtilityModel) -> Result<(), UtilityCatalogError> {
    let invalid = |why: String| {
        Err(UtilityCatalogError::Invalid(format!(
            "model {}: {why}",
            m.id
        )))
    };
    let given = [
        (UtilityPurpose::RunName, m.purposes.run_name.as_ref()),
        (UtilityPurpose::RunTags, m.purposes.run_tags.as_ref()),
    ];
    for (purpose, record) in given {
        let Some(record) = record else { continue };
        let word = purpose.as_str();
        if !non_blank(&record.smoke_record_id) {
            return invalid(format!(
                "purposes.{word} has a blank smokeRecordId; a purpose is served only with a passing smoke record"
            ));
        }
        if !non_blank(&record.smoked_at) {
            return invalid(format!("purposes.{word} has a blank smokedAt"));
        }
        if record_file(&record.smoke_record_id).is_none() {
            return invalid(format!(
                "purposes.{word}: the smoke record id {:?} must end in a plain .json file name",
                record.smoke_record_id
            ));
        }
    }
    if let Some(copy) = &m.purposes.run_name {
        let same = m.smoke_record_id.as_deref() == Some(copy.smoke_record_id.as_str())
            && m.smoked_at.as_deref() == Some(copy.smoked_at.as_str());
        if !same {
            return invalid(
                "purposes.run-name must repeat the top-level smokeRecordId and smokedAt exactly, and the row needs them (the infra reads the top level only)"
                    .to_string(),
            );
        }
    }
    Ok(())
}

impl UtilityModels {
    /// Parses and checks a catalog document. The rules are those of `models.json` (unique ids and aliases, a default that is a
    /// selectable row, an alternate route that is another profile of the same model and has its own smoke record), plus: the
    /// protocol is `converse`, and no id, alias or route is one of the coding models.
    pub fn parse(text: &str) -> Result<UtilityModels, UtilityCatalogError> {
        let doc: CatalogDoc =
            serde_json::from_str(text).map_err(|e| UtilityCatalogError::Parse(e.to_string()))?;
        let invalid = |m: String| Err(UtilityCatalogError::Invalid(m));
        if doc.schema_version != 1 {
            return invalid(format!(
                "schemaVersion is {}; this build reads 1",
                doc.schema_version
            ));
        }
        if doc.models.is_empty() {
            return invalid("the file lists no models".into());
        }
        let mut ids: Vec<&str> = Vec::new();
        for m in &doc.models {
            if blank_or_spaced(&m.id) {
                return invalid(format!(
                    "model id {:?} must be non-empty with no whitespace",
                    m.id
                ));
            }
            if ids.contains(&m.id.as_str()) {
                return invalid(format!("model {} is listed twice", m.id));
            }
            ids.push(&m.id);
            if m.protocol != Protocol::Converse {
                return invalid(format!(
                    "model {}: the protocol must be converse, the only one the utility call speaks",
                    m.id
                ));
            }
            if m.region.trim().is_empty() {
                return invalid(format!("model {} needs a Region", m.id));
            }
            if m.base_model_id.trim().is_empty() {
                return invalid(format!("model {} needs its baseModelId", m.id));
            }
            check_purposes(m)?;
        }
        let mut routes: Vec<&str> = ids.clone();
        for m in &doc.models {
            for a in &m.alternate_routes {
                if blank_or_spaced(&a.id) {
                    return invalid(format!(
                        "model {}: alternate route id {:?} must be non-empty with no whitespace",
                        m.id, a.id
                    ));
                }
                if routes.contains(&a.id.as_str()) {
                    return invalid(format!(
                        "model {}: alternate route {} repeats a model or another route",
                        m.id, a.id
                    ));
                }
                routes.push(&a.id);
                if models::base_model(&a.id) != models::base_model(&m.id) {
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
            if !valid_alias(&m.alias) {
                return invalid(format!(
                    "model {}: alias {:?} must be 1-24 lower-case letters, digits or '-', not starting with '-'",
                    m.id, m.alias
                ));
            }
            if aliases.contains(&m.alias.as_str()) {
                return invalid(format!("alias {:?} is used by two models", m.alias));
            }
            if ids.contains(&m.alias.as_str()) {
                return invalid(format!(
                    "alias {:?} of model {} is also a model id",
                    m.alias, m.id
                ));
            }
            aliases.push(&m.alias);
        }
        // The utility model is nobody's coding model: nothing a person can pick shares its name.
        let coding = models::builtin();
        for (kind, name) in routes
            .iter()
            .map(|r| ("id or route", *r))
            .chain(aliases.iter().map(|a| ("alias", *a)))
        {
            if coding.find(name).is_some() || coding.find_route(name).is_some() {
                return invalid(format!(
                    "{kind} {name:?} is a coding model of models.json; the utility catalog must not reuse it"
                ));
            }
        }
        let want = doc.default.trim();
        let found = doc
            .models
            .iter()
            .position(|m| m.id == want || m.alias == want.to_ascii_lowercase());
        let Some(default) = found else {
            return invalid(format!("default {want:?} names no row"));
        };
        if !doc.models[default].selectable() {
            return invalid(format!(
                "the default model {} has no passing smoke record",
                doc.models[default].id
            ));
        }
        Ok(UtilityModels {
            models: doc.models,
            default,
        })
    }

    /// [`UtilityModels::parse`] for a catalog that is about to be used: it also asks `records` for the text of the file each record id
    /// names (the file name after the last `/`, a plain `.json` name; `None` means the file is not beside the catalog) and refuses a record
    /// that is missing, is not a JSON object, or does not describe the row and the purpose that cite it. A record of the run name or of a
    /// purpose must be a passing one (`result` is `pass`) for the row's own route and Region, carry the id that cites it and the start time
    /// the row holds, and, for a purpose other than the run name, name that purpose in `purposeKey`. The record of an alternate route is
    /// only looked for. [`UtilityModels::builtin`] is parsed this way, so a catalog row can never cite a record this build does not hold.
    pub fn parse_checked(
        text: &str,
        records: &dyn Fn(&str) -> Option<String>,
    ) -> Result<UtilityModels, UtilityCatalogError> {
        let catalog = UtilityModels::parse(text)?;
        for m in &catalog.models {
            for purpose in UtilityPurpose::ALL {
                let (Some(id), Some(at)) = (m.record_id_for(purpose), m.smoked_at_for(purpose))
                else {
                    continue;
                };
                check_record(m, purpose, id, at, records)?;
            }
            for alt in &m.alternate_routes {
                let word = "an alternate route";
                let file = record_file(&alt.smoke_record_id).ok_or_else(|| {
                    UtilityCatalogError::Invalid(format!(
                        "model {}: the smoke record id {:?} of {word} {} must end in a plain .json file name",
                        m.id, alt.smoke_record_id, alt.id
                    ))
                })?;
                if records(file).is_none() {
                    return Err(UtilityCatalogError::Invalid(format!(
                        "model {}: the smoke record {file} of {word} {} is not beside utility-models.json",
                        m.id, alt.id
                    )));
                }
            }
        }
        Ok(catalog)
    }

    /// The catalog this build ships (`assets/utility-models.json`, compiled in), parsed once with [`UtilityModels::parse_checked`] against the
    /// smoke records this build embeds, so a row can never cite a record that is missing, failed, or describes another row or purpose. The
    /// files are committed and `tests/utility_builtin.rs` and `tests/utility_purposes.rs` read them, so a failure here is a build defect.
    pub fn builtin() -> &'static UtilityModels {
        static CATALOG: OnceLock<UtilityModels> = OnceLock::new();
        CATALOG.get_or_init(|| {
            UtilityModels::parse_checked(CATALOG_JSON, &|file| {
                RECORDS
                    .iter()
                    .find(|(name, _)| *name == file)
                    .map(|(_, text)| (*text).to_string())
            })
            .unwrap_or_else(|e| {
                panic!("the committed crates/ham-core/assets/utility-models.json is unusable: {e}")
            })
        })
    }

    /// Every listed model, in file order.
    pub fn models(&self) -> &[UtilityModel] {
        &self.models
    }

    /// The row an unset setting means.
    pub fn default_model(&self) -> &UtilityModel {
        &self.models[self.default]
    }

    /// The row a typed `id` (exact) or `alias` (any case) names, selectable or not, after trimming white space.
    pub fn find(&self, input: &str) -> Option<&UtilityModel> {
        let t = input.trim();
        self.models.iter().find(|m| m.id == t).or_else(|| {
            let lower = t.to_ascii_lowercase();
            self.models.iter().find(|m| m.alias == lower)
        })
    }

    /// The selectable models as `alias = id` words, for an error message.
    pub fn choices(&self) -> Vec<String> {
        self.choices_for(UtilityPurpose::RunName)
    }

    /// The models that hold a passing smoke record for `purpose`, as `alias = id` words.
    pub fn choices_for(&self, purpose: UtilityPurpose) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.selectable_for(purpose))
            .map(|m| format!("{} = {}", m.alias, m.id))
            .collect()
    }

    /// What the deploy setting `HAMH_UTILITY_MODEL` means for the run name: unset, blank or only spaces is the default row; `off` (any
    /// case) is [`UtilityChoice::Disabled`]; an id or alias of a selectable row is that row; anything else is an error that names the
    /// setting and never falls through to the default. [`UtilityModels::resolve_for`] is the same for another purpose.
    pub fn resolve(&self, setting: Option<&str>) -> Result<UtilityChoice<'_>, UtilityError> {
        self.resolve_for(UtilityPurpose::RunName, setting)
    }

    /// [`UtilityModels::resolve`] for a setting that has another name (`UTILITY_MODEL` in a Lambda).
    pub fn resolve_named(
        &self,
        setting: Option<&str>,
        origin: &str,
    ) -> Result<UtilityChoice<'_>, UtilityError> {
        self.resolve_for_named(UtilityPurpose::RunName, setting, origin)
    }

    /// [`UtilityModels::resolve`] for `purpose`. A row is a choice only with a passing smoke record for that purpose: a row that has
    /// only another purpose's record is [`UtilityError::NotSmoked`] for this one, an unset setting whose default row has none is the
    /// same error, and an unknown setting lists only the aliases that do serve the purpose.
    pub fn resolve_for(
        &self,
        purpose: UtilityPurpose,
        setting: Option<&str>,
    ) -> Result<UtilityChoice<'_>, UtilityError> {
        self.resolve_for_named(purpose, setting, "HAMH_UTILITY_MODEL")
    }

    /// [`UtilityModels::resolve_for`] for a setting that has another name.
    pub fn resolve_for_named(
        &self,
        purpose: UtilityPurpose,
        setting: Option<&str>,
        origin: &str,
    ) -> Result<UtilityChoice<'_>, UtilityError> {
        let not_smoked = |m: &UtilityModel| UtilityError::NotSmoked {
            origin: origin.to_string(),
            id: m.id.clone(),
            purpose,
        };
        let t = setting.map(str::trim).unwrap_or_default();
        if t.is_empty() {
            let m = self.default_model();
            return if m.selectable_for(purpose) {
                Ok(UtilityChoice::Model(m))
            } else {
                Err(not_smoked(m))
            };
        }
        if t.eq_ignore_ascii_case("off") {
            return Ok(UtilityChoice::Disabled);
        }
        match self.find(t) {
            Some(m) if m.selectable_for(purpose) => Ok(UtilityChoice::Model(m)),
            Some(m) => Err(not_smoked(m)),
            None => Err(UtilityError::Unknown {
                origin: origin.to_string(),
                value: t.to_string(),
                choices: self
                    .models
                    .iter()
                    .filter(|m| m.selectable_for(purpose))
                    .map(|m| m.alias.clone())
                    .collect(),
            }),
        }
    }
}

/// The checks of one record of [`UtilityModels::parse_checked`].
fn check_record(
    m: &UtilityModel,
    purpose: UtilityPurpose,
    id: &str,
    smoked_at: &str,
    records: &dyn Fn(&str) -> Option<String>,
) -> Result<(), UtilityCatalogError> {
    let word = purpose.as_str();
    let invalid = |why: String| {
        Err(UtilityCatalogError::Invalid(format!(
            "model {}: the {word} record {why}",
            m.id
        )))
    };
    let Some(file) = record_file(id) else {
        return invalid(format!("id {id:?} must end in a plain .json file name"));
    };
    let Some(text) = records(file) else {
        return invalid(format!("{file} is not beside utility-models.json"));
    };
    let Ok(Value::Object(record)) = serde_json::from_str::<Value>(&text) else {
        return invalid(format!("{file} is not a JSON object"));
    };
    let text_of = |key: &str| record.get(key).and_then(Value::as_str);
    let expect = |key: &str, want: &str| -> Result<(), UtilityCatalogError> {
        match text_of(key) {
            Some(got) if got == want => Ok(()),
            got => invalid(format!(
                "{file}: {key} is {} but the row says {want:?}",
                got.map_or("missing".to_string(), |g| format!("{g:?}"))
            )),
        }
    };
    expect("result", "pass")?;
    expect("smokeRecordId", id)?;
    expect("profileId", &m.id)?;
    expect("region", &m.region)?;
    expect("startedAt", smoked_at)?;
    match (purpose, text_of("purposeKey")) {
        (UtilityPurpose::RunName, None) => {}
        (_, Some(key)) if key == word => {}
        (_, got) => {
            return invalid(format!(
                "{file}: purposeKey is {} but the record is cited for {word}",
                got.map_or("missing".to_string(), |g| format!("{g:?}"))
            ));
        }
    }
    Ok(())
}

/// A purpose the utility model serves. Every new helper function extends this enum, so the compiler walks the author through the caps
/// that make the purpose cheap and bounded: the most input it may send, the most output it may ask for, and its time. It also walks the
/// author to the smoke record of the purpose ([`UtilityModel::record_id_for`]): a purpose is served only by a row that holds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UtilityPurpose {
    /// The 5 to 7 word name of a run.
    RunName,
    /// The few topic and tech tags of a run.
    RunTags,
}

impl UtilityPurpose {
    /// The purposes the build knows.
    pub const ALL: [UtilityPurpose; 2] = [UtilityPurpose::RunName, UtilityPurpose::RunTags];

    /// The word logs, metrics, the `purposes` object of the catalog and the setting `UTILITY_PURPOSES` carry.
    pub fn as_str(self) -> &'static str {
        match self {
            UtilityPurpose::RunName => "run-name",
            UtilityPurpose::RunTags => "run-tags",
        }
    }

    /// The purpose a word names (exactly [`UtilityPurpose::as_str`]).
    pub fn from_word(word: &str) -> Option<UtilityPurpose> {
        UtilityPurpose::ALL.into_iter().find(|p| p.as_str() == word)
    }

    /// The most characters the user turn may hold (`runname::MAX_USER_TEXT_CHARS`; for tags the 40 vocabulary lines and the longest
    /// escaped excerpt, 3,007 characters that are all `&`, about 16,900 in all).
    pub fn max_input_chars(self) -> usize {
        match self {
            UtilityPurpose::RunName => 12_100,
            UtilityPurpose::RunTags => 17_500,
        }
    }

    /// The `maxTokens` of the request (`runname::MAX_TOKENS`, `runtag::judge::MAX_TOKENS`).
    pub fn max_output_tokens(self) -> u32 {
        match self {
            UtilityPurpose::RunName => 64,
            UtilityPurpose::RunTags => crate::runtag::judge::MAX_TOKENS,
        }
    }

    /// The most attempts at one call (one retry for a transient fault).
    pub fn attempts(self) -> u32 {
        match self {
            UtilityPurpose::RunName | UtilityPurpose::RunTags => 2,
        }
    }

    /// The time one attempt may take.
    pub fn attempt_timeout(self) -> Duration {
        match self {
            UtilityPurpose::RunName | UtilityPurpose::RunTags => Duration::from_secs(6),
        }
    }

    /// The time the whole operation may take, retries included. The tag branch runs beside the name branch inside the 30 seconds of the
    /// `name-run` operation, so its own budget is 20.
    pub fn op_deadline(self) -> Duration {
        match self {
            UtilityPurpose::RunName => Duration::from_secs(30),
            UtilityPurpose::RunTags => Duration::from_secs(20),
        }
    }
}

/// The Converse request body for one helper call. No `temperature`, `topP` or `top_k` (Haiku 5.5 refuses them), no assistant turn
/// (prefill is refused), no tools. `additionalModelRequestFields` comes from the catalog row and is left out when the row has none.
pub fn request_body(
    model: &UtilityModel,
    system: &str,
    user: &str,
    max_tokens: u32,
    stop: &[&str],
) -> Value {
    let mut inference = Map::new();
    inference.insert("maxTokens".into(), json!(max_tokens));
    if !stop.is_empty() {
        inference.insert("stopSequences".into(), json!(stop));
    }
    let mut body = Map::new();
    body.insert("system".into(), json!([{ "text": system }]));
    body.insert(
        "messages".into(),
        json!([{ "role": "user", "content": [{ "text": user }] }]),
    );
    body.insert("inferenceConfig".into(), Value::Object(inference));
    if let Some(extra) = model
        .additional_model_request_fields
        .as_ref()
        .filter(|v| !v.is_null() && v.as_object().is_none_or(|o| !o.is_empty()))
    {
        body.insert("additionalModelRequestFields".into(), extra.clone());
    }
    Value::Object(body)
}

/// What a Converse answer carries that a helper needs: the stop reason, the text blocks joined, the token counts and the service's own
/// latency. Reasoning blocks are counted and never read as text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UtilityReply {
    pub stop_reason: String,
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: Option<u64>,
    pub reasoning_blocks: usize,
}

impl UtilityReply {
    pub fn new(stop_reason: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            stop_reason: stop_reason.into(),
            text: text.into(),
            ..Self::default()
        }
    }
}

/// Reads the body of a successful `converse` answer. A body that is not JSON, or not an object, is a [`UtilityFault::Service`]; an
/// object with no `output.message.content` list is a [`UtilityFault::NoText`]. A list with no text block (a reply cut inside its
/// reasoning, or one the service filtered) is a reply with empty text: the stop reason says why.
pub fn parse_reply(bytes: &[u8]) -> Result<UtilityReply, UtilityFault> {
    let v: Value = serde_json::from_slice(bytes).map_err(|_| UtilityFault::Service)?;
    let obj = v.as_object().ok_or(UtilityFault::Service)?;
    let content = obj
        .get("output")
        .and_then(|o| o.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .ok_or(UtilityFault::NoText)?;
    let mut text = String::new();
    let mut reasoning_blocks = 0;
    for block in content {
        if let Some(t) = block.get("text").and_then(Value::as_str) {
            text.push_str(t);
        } else if block.get("reasoningContent").is_some() {
            reasoning_blocks += 1;
        }
    }
    let number = |outer: &str, inner: &str| {
        obj.get(outer)
            .and_then(|o| o.get(inner))
            .and_then(Value::as_u64)
    };
    Ok(UtilityReply {
        stop_reason: obj
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        text,
        input_tokens: number("usage", "inputTokens").unwrap_or(0),
        output_tokens: number("usage", "outputTokens").unwrap_or(0),
        latency_ms: number("metrics", "latencyMs"),
        reasoning_blocks,
    })
}

/// How a helper call failed, in the classes the retry rule needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum UtilityFault {
    #[error("the call timed out")]
    Timeout,
    #[error("the service throttled the call")]
    Throttle,
    #[error("the service failed the call")]
    Service,
    #[error("the call is refused for a reason a retry does not change (configuration)")]
    Config,
    #[error("the answer carried no text")]
    NoText,
}

/// The class of an `x-amzn-errortype` header: the text before the first `:` (Bedrock adds a namespace after it), lower-cased, without
/// a leading namespace before `#` and without a trailing "exception".
fn error_class(header: &str) -> String {
    let head = header.split(':').next().unwrap_or("");
    let head = head.rsplit('#').next().unwrap_or("");
    let lower = head.trim().to_ascii_lowercase();
    lower
        .strip_suffix("exception")
        .map(str::to_string)
        .unwrap_or(lower)
}

impl UtilityFault {
    /// The class of a failed HTTP answer. The error class of the header wins when it is one this code knows (Bedrock answers a model
    /// that is not ready with 429); otherwise the status decides: 429 throttles, 408 times out, 5xx fails the service, the rest of 4xx
    /// is a configuration fault, and any other non-success is a service fault.
    pub fn from_http(status: u16, error_type: Option<&str>) -> UtilityFault {
        match error_type.map(error_class).as_deref() {
            Some("throttling" | "toomanyrequests") => return UtilityFault::Throttle,
            Some("modeltimeout") => return UtilityFault::Timeout,
            Some("modelnotready" | "serviceunavailable" | "internalserver") => {
                return UtilityFault::Service;
            }
            Some("accessdenied" | "validation" | "resourcenotfound") => {
                return UtilityFault::Config;
            }
            _ => {}
        }
        match status {
            429 => UtilityFault::Throttle,
            408 => UtilityFault::Timeout,
            500..=599 => UtilityFault::Service,
            400..=499 => UtilityFault::Config,
            _ => UtilityFault::Service,
        }
    }

    /// The fixed word logs carry (`runname::Reason` has the same words).
    pub fn code(self) -> &'static str {
        match self {
            UtilityFault::Timeout => "timeout",
            UtilityFault::Throttle => "throttle",
            UtilityFault::Service => "service_error",
            UtilityFault::Config => "config_error",
            UtilityFault::NoText => "no_text",
        }
    }

    /// A transient fault is worth one more attempt on the same route; a configuration fault and an empty answer are not.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            UtilityFault::Timeout | UtilityFault::Throttle | UtilityFault::Service
        )
    }
}

fn sampling_problem(fields: &Value) -> Option<String> {
    let obj = fields.as_object()?;
    ["temperature", "topP", "top_p", "topK", "top_k"]
        .iter()
        .find(|k| obj.contains_key(**k))
        .map(|k| {
            let shown = match *k {
                "topP" => "top_p",
                "topK" => "top_k",
                other => other,
            };
            format!("`{shown}` is deprecated for this model.")
        })
}

/// What the live service refuses for a utility call, so that a fake of it refuses the same (an earlier permissive fake let a bad
/// value through and broke a live run): `request` is the Converse input, with an optional `modelId` beside the body members.
/// Refused: any `temperature`, `topP` or `top_k` (in `inferenceConfig` or the extra fields, whatever the value); a last assistant turn
/// (prefill); an effort of `xhigh` or `max` while thinking is disabled; no messages; the bare model id (on-demand throughput is not
/// supported for a profile-only model); an unknown id. The answer is the service's wording. A request without a `modelId` is checked
/// for its body only. The ids the service knows are [`KNOWN_ROUTE_IDS`]; see [`bedrock_rejects_with`] for another set.
pub fn bedrock_rejects(request: &Value) -> Option<String> {
    bedrock_rejects_with(request, &KNOWN_ROUTE_IDS)
}

/// [`bedrock_rejects`] for a caller whose catalog names the ids the fake serves.
pub fn bedrock_rejects_with(request: &Value, known_ids: &[&str]) -> Option<String> {
    if let Some(id) = request.get("modelId") {
        let id = id.as_str().unwrap_or_default();
        if !known_ids.contains(&id) {
            return Some(if known_ids.iter().any(|k| models::base_model(k) == id) {
                format!(
                    "Invocation of model ID {id} with on-demand throughput isn't supported. Retry your request with the ID or ARN of an inference profile that contains this model."
                )
            } else {
                "The provided model identifier is invalid.".to_string()
            });
        }
    }
    let messages = request.get("messages").and_then(Value::as_array);
    match messages {
        Some(m) if !m.is_empty() => {
            if m.last().and_then(|l| l.get("role")).and_then(Value::as_str) == Some("assistant") {
                return Some("This model does not support assistant message prefill. The conversation must end with a user message.".into());
            }
        }
        _ => return Some("messages: at least one message is required.".into()),
    }
    if let Some(p) = request.get("inferenceConfig").and_then(sampling_problem) {
        return Some(p);
    }
    let extra = request.get("additionalModelRequestFields");
    if let Some(p) = extra.and_then(sampling_problem) {
        return Some(p);
    }
    let thinking_off = extra
        .and_then(|e| e.get("thinking"))
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("disabled");
    let effort = extra
        .and_then(|e| e.get("output_config"))
        .and_then(|o| o.get("effort"))
        .and_then(Value::as_str);
    match effort {
        Some(e @ ("xhigh" | "max")) if thinking_off => Some(format!(
            "output_config.effort '{e}' is not supported when thinking is disabled."
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// The excerpt: what a helper may send of a prompt
// ---------------------------------------------------------------------------------------------------------------------------------

/// A prompt of this many characters or fewer goes to the model whole.
pub const EXCERPT_WHOLE_CHARS: usize = 3000;
/// The head of a longer prompt that the model sees ...
pub const EXCERPT_HEAD_CHARS: usize = 2000;
/// ... and its tail.
pub const EXCERPT_TAIL_CHARS: usize = 1000;
/// What stands where the middle of a long prompt was left out.
pub const EXCERPT_CUT_MARK: &str = "\n[...]\n";
/// A prompt of fewer words than this asks no model (Latin and other spaced scripts).
pub const MIN_PROMPT_WORDS: usize = 4;
/// A prompt of fewer non-blank characters than this asks no model when it is mostly Chinese, Japanese or Thai.
pub const MIN_PROMPT_UNSPACED_CHARS: usize = 8;

/// Chinese, Japanese and Thai are written without spaces between words: their text is counted in characters.
fn is_unspaced(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30ff | 0x3400..=0x9fff | 0x0e00..=0x0e7f | 0x20000..=0x2fa1f
    )
}

/// The characters of `s` that are not blanks.
pub(crate) fn non_space_chars(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// At least half of the characters of `s` (blanks left out) are of an unspaced script, and there is at least one.
pub(crate) fn unspaced_majority(s: &str) -> bool {
    let (mut all, mut unspaced) = (0usize, 0usize);
    for c in s.chars().filter(|c| !c.is_whitespace()) {
        all += 1;
        if is_unspaced(c) {
            unspaced += 1;
        }
    }
    all > 0 && unspaced * 2 >= all
}

/// The words of `s`: blank-separated pieces that hold a letter or a digit.
pub fn word_count(s: &str) -> usize {
    s.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count()
}

/// The prompt as a helper, its judge and the name seed see it: control bytes and escape sequences stripped, then credential shapes hidden
/// (in that order, so an escape byte inside a key cannot keep the redactor from seeing it: hiding first would leave a key that a colour
/// reset, a hyperlink escape, a backspace or a NUL cuts in two, because the redactor reads characters and the stripper then joins the
/// pieces).
pub fn redacted_prompt(prompt: &str) -> String {
    redact_tokens(&sanitize(prompt))
}

/// `text` whole up to [`EXCERPT_WHOLE_CHARS`] characters, otherwise its first [`EXCERPT_HEAD_CHARS`] and last [`EXCERPT_TAIL_CHARS`]
/// around [`EXCERPT_CUT_MARK`]. Characters, not bytes.
fn cut_excerpt(text: &str) -> String {
    let n = text.chars().count();
    if n <= EXCERPT_WHOLE_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(EXCERPT_HEAD_CHARS).collect();
    let tail: String = text.chars().skip(n - EXCERPT_TAIL_CHARS).collect();
    format!("{head}{EXCERPT_CUT_MARK}{tail}")
}

/// `&`, `<` and `>` as entities, so a prompt cannot close the tag a helper wraps it in or forge a reply tag.
fn escape_angle(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// The part of a prompt that a helper may send to the utility model, and the counts that decide whether to ask at all.
///
/// The counts are taken on the whole redacted text, before the cut and before the escape (an escape adds characters, and a cut hides
/// words). [`is_short`] reads only the counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excerpt {
    /// The prompt with control bytes stripped and credentials hidden, in that order, then cut (the whole text up to
    /// [`EXCERPT_WHOLE_CHARS`] characters, otherwise its first [`EXCERPT_HEAD_CHARS`] and last [`EXCERPT_TAIL_CHARS`] around
    /// [`EXCERPT_CUT_MARK`]) and then escaped (`&`, `<`, `>`). It holds no wrapper tag and no instruction line: each helper adds its own.
    /// It never holds `<` or `>`.
    pub escaped: String,
    /// The words of the redacted text ([`word_count`]).
    pub words: usize,
    /// Zero when the redacted text is not mostly an unspaced script (Chinese, Japanese, Thai); otherwise the number of its non-blank
    /// characters, which is what [`is_short`] counts for such a text. "Mostly" means at least half of the non-blank characters, the
    /// rule of the name side.
    pub unspaced_chars: usize,
}

/// Builds the [`Excerpt`] of `prompt`: at most 3,007 characters before the escape (2,000, the 7-character mark and 1,000), whatever the
/// size of the prompt.
pub fn excerpt(prompt: &str) -> Excerpt {
    let text = redacted_prompt(prompt);
    let words = word_count(&text);
    let unspaced_chars = if unspaced_majority(&text) {
        non_space_chars(&text)
    } else {
        0
    };
    Excerpt {
        escaped: escape_angle(&cut_excerpt(&text)),
        words,
        unspaced_chars,
    }
}

/// True when the prompt is too short for a helper to ask a model about: fewer than [`MIN_PROMPT_WORDS`] words, or, for a text that is
/// mostly an unspaced script, fewer than [`MIN_PROMPT_UNSPACED_CHARS`] non-blank characters. The helper keeps its fallback and makes no call.
pub fn is_short(ex: &Excerpt) -> bool {
    if ex.unspaced_chars > 0 {
        ex.unspaced_chars < MIN_PROMPT_UNSPACED_CHARS
    } else {
        ex.words < MIN_PROMPT_WORDS
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// The raw-reply guard
// ---------------------------------------------------------------------------------------------------------------------------------

/// A word longer than this is not text a helper may keep (a token, a path, an encoded blob).
pub const GUARD_MAX_WORD_CHARS: usize = 40;

/// Why [`guard_raw`] refuses a reply. The words are those of the run-name reason codes (`runname::Reason`), so a log field and a metric
/// carry the same word whichever helper refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GuardReason {
    /// An escape sequence or control character (`sanitize` would change the text), or a bidirectional override.
    ControlChars,
    /// A zero-width, tag or other invisible character (`sanitize::marker`).
    InvisibleChars,
    /// A credential shape (`redact_tokens` would change the text).
    CredentialShape,
    /// A blank-separated word of more than [`GUARD_MAX_WORD_CHARS`] characters.
    LongToken,
}

impl GuardReason {
    /// The fixed code.
    pub fn as_str(self) -> &'static str {
        match self {
            GuardReason::ControlChars => "control_chars",
            GuardReason::InvisibleChars => "invisible_chars",
            GuardReason::CredentialShape => "credential_shape",
            GuardReason::LongToken => "long_token",
        }
    }
}

/// The character and credential checks on the whole raw text of a reply, before anything is extracted from it: control characters and
/// bidirectional overrides, invisible characters, a credential shape. The first that fails is the answer, in this order. A reply with an
/// escape byte, a hidden character or a token shape is broken or steered, so these checks refuse and never clean.
///
/// This is [`guard_raw`] without its word rule. A reply that is a list whose items are checked one by one (the run tags: raw length,
/// grammar and word shape per item) has no use for it: the model writes the JSON list either as `["a", "b"]` or compact as `["a","b"]`,
/// and the compact list is one blank-separated word of 80 to 110 characters, which the word rule refused in 16 of 152 live replies. The
/// tag judge (`runtag::judge`) runs this itself on the whole text; run names keep [`guard_raw`].
pub fn guard_chars(raw: &str) -> Result<(), GuardReason> {
    if sanitize(raw) != raw {
        return Err(GuardReason::ControlChars);
    }
    if raw.chars().any(|c| marker(c).is_some()) {
        return Err(GuardReason::InvisibleChars);
    }
    if redact_tokens(raw) != raw {
        return Err(GuardReason::CredentialShape);
    }
    Ok(())
}

/// The safety checks on the whole raw text of a reply that is prose (the run name): [`guard_chars`], then the word rule. The first that
/// fails is the answer, in this order: control characters, invisible characters, a credential shape, a long word. A reply with an escape
/// byte, a hidden character or a token shape is broken or steered, so these checks refuse and never clean.
///
/// The length of a reply is the helper's own limit (the name judge refuses over 400 characters before calling this). The same four checks
/// read a stored run name back (`runname::check_stored`), so a change here changes what an already stored name may be: add a check
/// only with a test that reads the stored names.
pub fn guard_raw(raw: &str) -> Result<(), GuardReason> {
    guard_chars(raw)?;
    if raw
        .split_whitespace()
        .any(|w| w.chars().count() > GUARD_MAX_WORD_CHARS)
    {
        return Err(GuardReason::LongToken);
    }
    Ok(())
}
