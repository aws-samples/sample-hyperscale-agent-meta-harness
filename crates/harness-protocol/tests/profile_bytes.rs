//! Guard (green before and after the `[mcp]` table of protocol 1.40.0): an engine profile that carries no `[mcp]` table is written back as the same
//! bytes it was written as before the table existed. Agentd, control-api and the K suite read profiles, and a new optional member must not change
//! what an older profile serializes to (the wire rule: a new member is optional and left out when empty).
#![allow(clippy::unwrap_used)]

use harness_protocol::engine_profile::EngineProfile;
use serde_json::json;
use sha2::{Digest, Sha256};

fn profile() -> EngineProfile {
    serde_json::from_value(json!({
        "schemaVersion": 1,
        "name": "demo",
        "version": "1.0.0",
        "binary": {"path": "demo-engine", "sha256": "ab".repeat(32)},
        "transports": {
            "acp": {"argv": ["demo-engine", "--acp"], "stdin": "pipe", "cancel": {"method": "acp-cancel", "killAfterS": 5}, "modelEnv": "DEMO_MODEL"},
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
    }))
    .unwrap()
}

#[test]
fn a_profile_without_an_mcp_table_serializes_to_the_bytes_it_always_did() {
    let bytes = serde_json::to_vec(&profile()).unwrap();
    let sha: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        sha,
        "ea915f1d9c6276261c0e87ddb95ae40cbfd43e81d755af365082dab9e2ba5730",
        "the serialized profile changed ({} bytes): a profile without an [mcp] table must write back the same bytes",
        bytes.len()
    );
}
