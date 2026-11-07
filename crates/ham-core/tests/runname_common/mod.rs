//! Shared helpers of the run-name tests (`runname_plan`, `runname_judge`, `runname_seed`, `utility`): the fixtures of
//! `tests/fixtures/runname/`, the corpus, and credential-shaped stand-ins that are built at run time from pieces, so no source
//! line is shaped like a credential.

#![allow(dead_code, clippy::unwrap_used)]

use ham_core::utility::UtilityReply;
use serde_json::Value;
use std::path::PathBuf;

/// A prompt that the names of the tests are grounded in.
pub const PROMPT: &str = "Add exponential backoff retry to the S3 uploader in uploader.py so that uploads survive throttling";
/// A valid name for [`PROMPT`] (seven words).
pub const NAME: &str = "Add exponential backoff retry to S3 uploader";

/// A catalog of the shape the utility model has: one row, the default, with a smoke record.
pub const CATALOG: &str = r#"{
  "schemaVersion": 1,
  "about": "a catalog for the tests",
  "default": "haiku",
  "models": [
    {
      "id": "us.anthropic.claude-haiku-5-5",
      "name": "Claude Haiku 5.5",
      "vendor": "anthropic",
      "alias": "haiku",
      "baseModelId": "anthropic.claude-haiku-5-5",
      "protocol": "converse",
      "region": "us-east-1",
      "additionalModelRequestFields": { "thinking": { "type": "disabled" } },
      "smokeRecordId": "s6:fixture/smoke-utility-fixture.json",
      "smokedAt": "2026-10-20T07:00:00Z"
    }
  ]
}"#;

pub fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runname")
}

pub fn asset_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join(name)
}

pub fn read_fixture(rel: &str) -> String {
    std::fs::read_to_string(fixture_dir().join(rel)).unwrap()
}

pub fn read_json(rel: &str) -> Value {
    serde_json::from_str(&read_fixture(rel)).unwrap()
}

/// One prompt of the corpus (`corpus.json`, the 27 benign prompts of the research probes).
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub category: String,
    pub prompt: String,
}

pub fn corpus() -> Vec<Entry> {
    read_json("corpus.json")
        .as_array()
        .unwrap()
        .iter()
        .map(|e| Entry {
            id: e["id"].as_str().unwrap().to_string(),
            category: e["category"].as_str().unwrap().to_string(),
            prompt: e["prompt"].as_str().unwrap().to_string(),
        })
        .collect()
}

pub fn prompt_of(id: &str) -> String {
    corpus()
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("no corpus entry {id}"))
        .prompt
}

/// A reply that ended on the stop sequence, as every good reply of the live probes did.
pub fn reply(text: &str) -> UtilityReply {
    UtilityReply::new("stop_sequence", text)
}

/// `<title>` and the name, without the closing tag: the stop sequence is not part of the reply text.
pub fn tagged(name: &str) -> UtilityReply {
    reply(&format!("<title>{name}"))
}

/// The number of cases of a property test: `PROPTEST_CASES` when it is set, else `default`. `ProptestConfig::with_cases` ignores
/// that variable, so a longer run on a build host only takes effect through this function.
pub fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn rep(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

/// An AWS access key id shape, from pieces.
pub fn aws_key_id() -> String {
    format!("{}{}{}", "AK", "IA", "QWERTYUIOPASDFGH")
}

/// A GitHub token shape, from pieces.
pub fn github_token() -> String {
    format!("{}{}_{}", "g", "hp", rep('a', 36))
}

/// A bearer credential shape (a scheme word and a random-looking body), from pieces.
pub fn bearer_token() -> String {
    format!(
        "{} {}",
        ["Bear", "er"].concat(),
        "Zx9Kq7Lm2Nb4Vc6Td8Fg1Hj3Pw5Rs0Ya7Ue"
    )
}

/// Every window of `len` characters of `s`.
pub fn windows(s: &str, len: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .windows(len)
        .map(|w| w.iter().collect::<String>())
        .collect()
}
