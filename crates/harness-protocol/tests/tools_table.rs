//! Protocol 1.40.0, wire B of the tool layer (trimmed to `code_search`): the table of the tools the harness's own tool server offers to an
//! engine. These tests pin the names (no engine word, the prefixed form under 64 characters), the descriptions (at most 400 characters, and
//! pinned by hash because the engine sends every description with every model request), the model-facing input schema, and the committed
//! `schemas/tools.schema.json`, which carries the descriptors and the failure words.
#![allow(clippy::unwrap_used)]

use harness_protocol::export::{all_schemas, canonical, render};
use harness_protocol::tools::{
    CodeSearchInput, DESCRIPTION_MAX, RESULT_FAILURE_WORDS, SERVER_NAME, TOOLS, ToolName,
    contract_schema, is_tool_word, tools_list_result,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The engine words a tool name or a description must not hold (design 4.4: names carry no engine word).
const ENGINE_WORDS: [&str; 5] = ["cline", "opencode", "pi", "kiro", "codex"];

fn whole_words(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn committed(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The pinned SHA-256 of the canonical (sorted keys, compact) JSON of `tools_list_result()`: the bytes a model is sent for the tool block,
/// less the engine's own prefix. Changing a description, a parameter text or a limit changes this value, and the token cost of the tool
/// block changes with it: read the size of the new text (`tool_block_bytes_are_reported` prints it) before you re-pin.
const TOOLS_LIST_SHA256: &str = "a9f8f1cad8fe67f9b9e9b2a711964266c2ecf76a215d08e9014c6f09833796f8";

#[test]
fn the_released_tool_set_is_code_search() {
    let names: Vec<&str> = TOOLS.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["code_search"],
        "a tool joins the table with its window and a protocol bump"
    );
    let all: Vec<&str> = ToolName::ALL.iter().map(|n| n.as_str()).collect();
    assert_eq!(
        all, names,
        "ToolName::ALL and TOOLS list the same tools in the same order"
    );
    assert_eq!(
        ToolName::from_word("code_search"),
        Some(ToolName::CodeSearch)
    );
    assert_eq!(ToolName::CodeSearch.as_str(), "code_search");
    for not_a_tool in [
        "",
        "web_search",
        "semantic_search",
        "ham__code_search",
        "Code_Search",
        "code-search",
    ] {
        assert_eq!(ToolName::from_word(not_a_tool), None, "{not_a_tool:?}");
    }
}

#[test]
fn names_match_the_rule_and_hold_no_engine_word() {
    assert_eq!(SERVER_NAME, "ham");
    assert!(
        is_tool_word(SERVER_NAME),
        "the server name follows the same rule"
    );
    assert!(
        !whole_words(SERVER_NAME)
            .iter()
            .any(|w| ENGINE_WORDS.contains(&w.as_str()))
    );
    for t in TOOLS {
        let name = t.name.as_str();
        assert!(is_tool_word(name), "{name}");
        // ^[a-z][a-z0-9_]{0,23}$, written out so the rule in the code cannot be the rule the test checks against itself.
        let b = name.as_bytes();
        assert!((1..=24).contains(&b.len()), "{name}");
        assert!(b[0].is_ascii_lowercase(), "{name}");
        assert!(
            b.iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_'),
            "{name}"
        );
        // The engine joins server and tool with two underscores, so a name holding two would read as a prefixed name.
        assert!(!name.contains("__"), "{name}");
        for word in whole_words(name) {
            assert!(
                !ENGINE_WORDS.contains(&word.as_str()),
                "{name} holds the engine word {word}"
            );
        }
        let prefixed = format!("{SERVER_NAME}__{name}");
        assert!(
            prefixed.len() < 64,
            "{prefixed} is {} characters; the limit is under 64",
            prefixed.len()
        );
    }
}

#[test]
fn the_name_rule_refuses_what_the_summary_must_never_hold() {
    let (too_long, longest) = ("a".repeat(25), "a".repeat(24));
    for bad in [
        "",
        "A",
        "1a",
        "_a",
        "a-b",
        "a b",
        "ham__code_search",
        "a__b",
        too_long.as_str(),
        "café",
    ] {
        assert!(!is_tool_word(bad), "{bad:?} must not be a tool word");
    }
    for good in ["a", "code_search", "web_search", longest.as_str(), "a1_b2"] {
        assert!(is_tool_word(good), "{good:?}");
    }
}

#[test]
fn descriptions_are_at_most_400_characters_and_pinned() {
    assert_eq!(DESCRIPTION_MAX, 400);
    for t in TOOLS {
        let n = t.description.chars().count();
        assert!(
            (1..=400).contains(&n),
            "{}: {n} characters",
            t.name.as_str()
        );
        for word in whole_words(t.description) {
            assert!(
                !ENGINE_WORDS.contains(&word.as_str()),
                "{} names the engine word {word}",
                t.name.as_str()
            );
        }
    }
    let compact = serde_json::to_string(&canonical(&tools_list_result())).unwrap();
    let sha: String = Sha256::digest(compact.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        sha,
        TOOLS_LIST_SHA256,
        "the tool block changed ({} bytes now). The engine sends this text with every model request: look at the token cost, then pin the new value",
        compact.len()
    );
}

/// The size of the block is printed so a person who changes the text sees the cost.
#[test]
fn tool_block_bytes_are_reported() {
    let compact = serde_json::to_string(&tools_list_result()).unwrap();
    println!("TOOL-BLOCK-BYTES {}", compact.len());
    assert!(
        compact.len() <= 2_200,
        "the stub measured 1,831 bytes for the same tool; this block is {} bytes",
        compact.len()
    );
}

#[test]
fn the_tools_list_entry_is_name_description_and_input_schema() {
    let list = tools_list_result();
    let tools = list["tools"].as_array().unwrap();
    assert_eq!(tools.len(), TOOLS.len());
    assert_eq!(list.as_object().unwrap().len(), 1, "only `tools`");
    for (entry, spec) in tools.iter().zip(TOOLS) {
        let keys: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["description", "inputSchema", "name"]);
        assert_eq!(entry["name"], spec.name.as_str());
        assert_eq!(entry["description"], spec.description);
        assert_eq!(entry["inputSchema"], (spec.input_schema)());
    }
}

#[test]
fn the_code_search_input_schema_is_a_strict_object_with_the_limits_and_defaults_a_model_needs() {
    let schema = (TOOLS[0].input_schema)();
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["additionalProperties"],
        json!(false),
        "an unknown member is refused"
    );
    assert_eq!(schema["required"], json!(["pattern"]));
    assert!(
        schema.get("$schema").is_none() && schema.get("title").is_none(),
        "bytes sent with every request: no dialect line, no title"
    );
    let p = &schema["properties"];
    let names: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "context",
            "count",
            "filesOnly",
            "fixed",
            "glob",
            "includeHidden",
            "includeIgnored",
            "maxPerFile",
            "maxResults",
            "paths",
            "pattern"
        ]
    );
    assert_eq!(
        (&p["pattern"]["minLength"], &p["pattern"]["maxLength"]),
        (&json!(1), &json!(1000))
    );
    assert_eq!(
        (
            &p["maxResults"]["minimum"],
            &p["maxResults"]["maximum"],
            &p["maxResults"]["default"]
        ),
        (&json!(1), &json!(200), &json!(50))
    );
    assert_eq!(
        (
            &p["maxPerFile"]["minimum"],
            &p["maxPerFile"]["maximum"],
            &p["maxPerFile"]["default"]
        ),
        (&json!(1), &json!(50), &json!(10))
    );
    assert_eq!(
        (&p["context"]["minimum"], &p["context"]["maximum"]),
        (&json!(0), &json!(5))
    );
    assert_eq!(p["paths"]["maxItems"], json!(10));
    assert_eq!(p["glob"]["maxItems"], json!(10));
    // An optional member is "may be left out", never "may be null": no property type is a list that holds null.
    for (name, prop) in p.as_object().unwrap() {
        let ty = &prop["type"];
        assert!(ty.is_string(), "{name}: type must be one word, found {ty}");
        assert!(
            prop["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{name} needs a description for the model"
        );
    }
}

/// The schema a model reads and the validator the server runs must agree on every bound: each bound the schema states is accepted at the
/// edge and refused one step beyond it.
#[test]
fn the_schema_bounds_are_the_bounds_the_validator_enforces() {
    let schema = (TOOLS[0].input_schema)();
    let props = schema["properties"].as_object().unwrap();
    let ok = |args: Value| CodeSearchInput::from_arguments(&args).is_ok();
    let with = |member: &str, v: Value| {
        let mut m = serde_json::Map::new();
        m.insert("pattern".into(), json!("a"));
        m.insert(member.into(), v);
        Value::Object(m)
    };
    let mut checked = 0;
    for (name, prop) in props {
        if let (Some(min), Some(max)) = (prop["minimum"].as_i64(), prop["maximum"].as_i64()) {
            assert!(ok(with(name, json!(min))), "{name} = {min} must pass");
            assert!(ok(with(name, json!(max))), "{name} = {max} must pass");
            assert!(
                !ok(with(name, json!(min - 1))),
                "{name} = {} must be refused",
                min - 1
            );
            assert!(
                !ok(with(name, json!(max + 1))),
                "{name} = {} must be refused",
                max + 1
            );
            checked += 1;
        }
        if let Some(max) = prop["maxItems"].as_u64() {
            let items = |n: u64| (0..n).map(|i| format!("p{i}")).collect::<Vec<_>>();
            assert!(
                ok(with(name, json!(items(max)))),
                "{name} with {max} entries must pass"
            );
            assert!(
                !ok(with(name, json!(items(max + 1)))),
                "{name} with {} entries must be refused",
                max + 1
            );
            checked += 1;
        }
    }
    let max_len = props["pattern"]["maxLength"].as_u64().unwrap();
    assert!(
        ok(json!({"pattern": "é".repeat(max_len as usize)})),
        "the limit counts characters, not bytes"
    );
    assert!(!ok(json!({"pattern": "a".repeat(max_len as usize + 1)})));
    assert!(!ok(json!({"pattern": ""})));
    assert_eq!(
        checked, 5,
        "maxResults, maxPerFile, context, paths and glob"
    );
}

#[test]
fn the_failure_words_are_the_closed_set_code_search_needs() {
    assert_eq!(
        RESULT_FAILURE_WORDS,
        [
            "busy",
            "disabled",
            "refused",
            "timeout",
            "unavailable",
            "usage"
        ]
    );
    for w in RESULT_FAILURE_WORDS {
        assert!(is_tool_word(w), "{w}: a failure word is one plain word");
    }
}

#[test]
fn the_contract_document_holds_the_descriptors_the_input_schemas_and_the_failure_words() {
    let doc = contract_schema();
    assert_eq!(
        doc["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(doc["title"], "ToolsContract");
    // Each tool has a descriptor definition whose name and description are constants and whose inputSchema is the one a model reads.
    for spec in TOOLS {
        let def = &doc["$defs"][format!("{}Descriptor", camel(spec.name.as_str()))];
        assert_eq!(
            def["properties"]["name"]["const"],
            spec.name.as_str(),
            "{}",
            spec.name.as_str()
        );
        assert_eq!(def["properties"]["description"]["const"], spec.description);
        assert_eq!(def["properties"]["inputSchema"], (spec.input_schema)());
        assert_eq!(
            def["required"],
            json!(["description", "inputSchema", "name"])
        );
        assert_eq!(def["additionalProperties"], json!(false));
    }
    assert_eq!(
        doc["$defs"]["failureWord"]["enum"],
        json!(RESULT_FAILURE_WORDS)
    );
    assert_eq!(doc["$defs"]["serverName"]["const"], SERVER_NAME);
    // A tools/list result is validated by the document: the list is exactly the descriptors.
    let items = &doc["properties"]["tools"]["items"]["oneOf"];
    assert_eq!(items.as_array().unwrap().len(), TOOLS.len());
}

fn camel(word: &str) -> String {
    let mut out = String::new();
    for (i, part) in word.split('_').enumerate() {
        if i == 0 {
            out.push_str(part);
        } else {
            let mut c = part.chars();
            if let Some(first) = c.next() {
                out.push(first.to_ascii_uppercase());
                out.push_str(c.as_str());
            }
        }
    }
    out
}

#[test]
fn the_generated_schema_equals_the_committed_file() {
    let (name, value) = all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "tools.schema.json")
        .expect("tools.schema.json is exported");
    assert_eq!(name, "tools.schema.json");
    assert_eq!(value, canonical(&contract_schema()));
    assert_eq!(
        committed("tools.schema.json"),
        render(&value),
        "tools.schema.json is stale: run `cargo run -p harness-protocol --bin export-schemas`"
    );
}
