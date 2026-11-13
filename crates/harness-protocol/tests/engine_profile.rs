//! Engine profile wire type: camelCase field names, unknown fields
//! rejected, and the exported schema names the required keys.

use harness_protocol::engine_profile::{
    CancelMethod, EngineMcp, EngineProfile, McpRegistration, McpVia, RowClass, StdinMode,
};
use harness_protocol::export::all_schemas;
use serde_json::json;

fn sample() -> serde_json::Value {
    json!({
        "schemaVersion": 1,
        "name": "demo",
        "version": "1.0.0",
        "binary": {"path": "demo-engine"},
        "transports": {
            "acp": {"argv": ["demo-engine", "--acp"], "stdin": "pipe", "cancel": {"method": "acp-cancel", "killAfterS": 5}},
            "cli": {"argv": ["demo-engine", "-p", "{promptFile}"], "stdin": "null", "cancel": {"method": "sigterm", "killAfterS": 5}}
        },
        "env": {"DEMO_TELEMETRY": "off"},
        "envRemove": ["AWS_BEARER_TOKEN_BEDROCK"],
        "configEmitters": [{"path": "{configDir}/settings.json", "template": "engine/demo/settings.json.tmpl", "format": "json"}],
        "hiddenPaths": [".demo"],
        "gate": {"name": "QG-D"},
        "rows": [
            {"id": "K0", "class": "gating", "check": "handshake", "pass": {"minPass": 1, "runs": 1}},
            {"id": "K7", "class": "scoped", "trigger": "T1", "local": true, "check": "reject and continue",
             "pass": {"minPass": 8, "runs": 9, "thresholds": {"maxRejectsPerTurn": 6.0}}}
        ]
    })
}

#[test]
fn a_profile_round_trips_through_json() {
    let p: EngineProfile = serde_json::from_value(sample()).unwrap();
    assert_eq!(p.transports.acp.stdin, StdinMode::Pipe);
    assert_eq!(
        p.transports.cli.as_ref().unwrap().cancel.method,
        CancelMethod::Sigterm
    );
    assert_eq!(p.rows[1].class, RowClass::Scoped);
    assert!(!p.rows[0].local);
    assert_eq!(p.rows[1].pass.thresholds["maxRejectsPerTurn"], 6.0);
    let back = serde_json::to_value(&p).unwrap();
    assert_eq!(serde_json::from_value::<EngineProfile>(back).unwrap(), p);
}

#[test]
fn unknown_fields_are_rejected() {
    let mut v = sample();
    v["telemetry"] = json!(true);
    assert!(serde_json::from_value::<EngineProfile>(v).is_err());
    let mut v = sample();
    v["transports"]["acp"]["shell"] = json!(true);
    assert!(serde_json::from_value::<EngineProfile>(v).is_err());
}

#[test]
fn the_exported_schema_requires_the_profile_keys() {
    let (_, schema) = all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "engine-profile.schema.json")
        .unwrap();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for key in [
        "schemaVersion",
        "name",
        "version",
        "binary",
        "transports",
        "gate",
        "rows",
    ] {
        assert!(required.contains(&key), "{key} not required");
    }
    assert_eq!(schema["additionalProperties"], json!(false));
    assert!(schema["properties"]["hiddenPaths"].is_object());
}

// ---- the `[mcp]` table (protocol 1.40.0, wire B of the tool layer) ------------------------------------------------------

fn mcp_file() -> serde_json::Value {
    json!({
        "via": "file",
        "path": "{dataDir}/settings/cline_mcp_settings.json",
        "template": "engine/profiles/templates/mcp-cline.json",
        "maxResultChars": 7500
    })
}

#[test]
fn a_profile_without_the_table_reads_and_writes_back_unchanged() {
    let p: EngineProfile = serde_json::from_value(sample()).unwrap();
    assert!(p.mcp.is_none());
    let back = serde_json::to_value(&p).unwrap();
    assert!(
        back.get("mcp").is_none(),
        "no `mcp` member appears for a profile that had none"
    );
    assert_eq!(serde_json::from_value::<EngineProfile>(back).unwrap(), p);
}

#[test]
fn the_mcp_table_round_trips_in_camel_case() {
    let mut v = sample();
    v["mcp"] = mcp_file();
    let p: EngineProfile = serde_json::from_value(v.clone()).unwrap();
    let mcp = p.mcp.as_ref().unwrap();
    assert_eq!(mcp.via, McpVia::File);
    assert_eq!(
        mcp.path.as_deref(),
        Some("{dataDir}/settings/cline_mcp_settings.json")
    );
    assert_eq!(
        mcp.template.as_deref(),
        Some("engine/profiles/templates/mcp-cline.json")
    );
    assert_eq!(mcp.max_result_chars, Some(7500));
    assert!(mcp.shadow.is_empty());
    assert_eq!(serde_json::to_value(&p).unwrap()["mcp"], mcp_file());
    // An empty `shadow` in the file is the same table; it is left out when written back, like every other empty member.
    let mut with_empty_shadow = v.clone();
    with_empty_shadow["mcp"]["shadow"] = json!([]);
    assert_eq!(
        serde_json::from_value::<EngineProfile>(with_empty_shadow).unwrap(),
        p
    );
    // The table that declares "no registration" is just the word.
    let mut none = sample();
    none["mcp"] = json!({"via": "none"});
    let q: EngineProfile = serde_json::from_value(none.clone()).unwrap();
    assert_eq!(q.mcp.as_ref().unwrap().via, McpVia::None);
    assert_eq!(
        serde_json::to_value(&q).unwrap()["mcp"],
        json!({"via": "none"})
    );
}

#[test]
fn the_mcp_table_refuses_unknown_members_and_unknown_ways() {
    for edit in [
        |m: &mut serde_json::Value| m["transport"] = json!("stdio"),
        |m: &mut serde_json::Value| m["via"] = json!("acp"),
        |m: &mut serde_json::Value| m["via"] = json!("stdio"),
        |m: &mut serde_json::Value| m["maxResultChars"] = json!("7500"),
        |m: &mut serde_json::Value| m["shadow"] = json!("search_codebase"),
    ] {
        let mut v = sample();
        let mut m = mcp_file();
        edit(&mut m);
        v["mcp"] = m;
        assert!(serde_json::from_value::<EngineProfile>(v).is_err());
    }
}

#[test]
fn the_registration_is_the_file_way_with_everything_it_needs_or_nothing() {
    let parse = |v: serde_json::Value| serde_json::from_value::<EngineMcp>(v).unwrap();
    let file = parse(mcp_file());
    let reg: McpRegistration = file.registration().unwrap();
    assert_eq!(reg.path, "{dataDir}/settings/cline_mcp_settings.json");
    assert_eq!(reg.template, "engine/profiles/templates/mcp-cline.json");
    assert_eq!(reg.max_result_chars, 7500);
    assert!(reg.shadow.is_empty());
    assert!(parse(json!({"via": "none"})).registration().is_none());
    // A file table that lacks a part has no registration; validation (harness-core) is what refuses such a profile.
    for missing in ["path", "template", "maxResultChars"] {
        let mut v = mcp_file();
        v.as_object_mut().unwrap().remove(missing);
        assert!(parse(v).registration().is_none(), "{missing}");
    }
}

#[test]
fn the_exported_schema_documents_the_mcp_table() {
    let (_, schema) = all_schemas()
        .into_iter()
        .find(|(n, _)| *n == "engine-profile.schema.json")
        .unwrap();
    assert!(
        schema["properties"]["mcp"].is_object(),
        "mcp is a member of the profile"
    );
    assert!(
        !schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "mcp"),
        "a profile without the table stays valid"
    );
    let def = &schema["$defs"]["EngineMcp"];
    assert_eq!(def["additionalProperties"], json!(false));
    assert_eq!(def["required"], json!(["via"]));
    assert_eq!(def["properties"]["maxResultChars"]["minimum"], json!(1000));
    assert_eq!(def["properties"]["maxResultChars"]["maximum"], json!(16000));
    // A documented enum is a `oneOf` of constants, a plain one an `enum`; read the words from whichever the generator wrote.
    let via = &schema["$defs"]["McpVia"];
    let mut ways: Vec<&str> = match via["enum"].as_array() {
        Some(words) => words.iter().map(|v| v.as_str().unwrap()).collect(),
        None => via["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["const"].as_str().unwrap())
            .collect(),
    };
    ways.sort_unstable();
    assert_eq!(ways, ["file", "none"]);
}
