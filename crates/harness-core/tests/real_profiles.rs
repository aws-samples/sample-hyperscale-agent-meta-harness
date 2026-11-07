//! The committed engine profiles for Cline and OpenCode: they load
//! and validate, carry the hardened environment and hidden paths, and their K rows agree with the
//! QG-C gating rows that policy_from_ksuite uses.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_core::engine_profile::{hidden_paths_union, load_profile};
use harness_protocol::engine_profile::{EngineProfile, RowClass, StdinMode};
use std::path::PathBuf;

fn profile(name: &str) -> EngineProfile {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../engine/profiles/{name}.toml"));
    load_profile(&path).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn row<'a>(p: &'a EngineProfile, id: &str) -> &'a harness_protocol::engine_profile::ProfileRow {
    p.rows
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("{} has no row {id}", p.name))
}

#[test]
fn cline_runs_the_fork_over_acp_with_the_hardened_environment() {
    let p = profile("cline");
    assert_eq!(p.name, "cline");
    assert_eq!(p.version, "3.0.71");
    assert_eq!(p.gate.name, "QG-C");
    assert_eq!(p.binary.path, "/opt/harness/bin/cline");
    let acp = &p.transports.acp;
    assert_eq!(acp.stdin, StdinMode::Pipe);
    assert_eq!(acp.argv[0], "/opt/harness/bin/cline");
    assert!(acp.argv.contains(&"--acp".to_string()));
    let joined = acp.argv.join(" ");
    assert!(joined.contains("--data-dir {dataDir}"), "{joined}");
    assert_eq!(
        p.env.get("CLINE_NO_AUTO_UPDATE").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        p.env.get("CLINE_SESSION_BACKEND_MODE").map(String::as_str),
        Some("local")
    );
    // Patch 0006 skips the Cline-account gate only for a named non-Cline provider
    // (isSessionReady(CLINE_PROVIDER ?? authResult.providerId)); without it the first microVM turn
    // failed "Authentication required: Call authenticate before starting a session".
    assert_eq!(
        p.env.get("CLINE_PROVIDER").map(String::as_str),
        Some("bedrock")
    );
    assert!(
        p.env_remove
            .contains(&"AWS_BEARER_TOKEN_BEDROCK".to_string())
    );
    assert!(p.env_remove.contains(&"CLINE_API_KEY".to_string()));
    // Cline also discovers hooks in <ws>/.clinerules/hooks.
    for h in [
        ".cline/hooks/**",
        ".cline/plugins/**",
        ".cline/agents/**",
        ".clinerules/hooks/**",
    ] {
        assert!(p.hidden_paths.contains(&h.to_string()), "{h}");
    }
}

/// The Cline profile is shared with the old line, which answers the engine's permission requests from the role policy: its ACP
/// launch has no `--auto-approve` (the direct driver adds `--auto-approve true` to its own launch, `agentd::engine::direct::
/// direct_launch_argv`; review of 2026-10-13, S1). The model is pinned by `CLINE_MODEL`, which the profile names as `modelEnv`.
#[test]
fn cline_profile_keeps_the_old_lines_launch_and_the_model_pin_env() {
    let p = profile("cline");
    assert_eq!(
        p.transports.acp.argv,
        [
            "/opt/harness/bin/cline",
            "--acp",
            "--config",
            "{configDir}",
            "--data-dir",
            "{dataDir}"
        ]
    );
    assert!(
        !p.transports.acp.argv.iter().any(|a| a.contains("approve")),
        "an auto-approve flag in the shared profile would silence the old line's permission policy"
    );
    assert_eq!(p.transports.acp.model_env.as_deref(), Some("CLINE_MODEL"));
}

#[test]
fn cline_rows_cover_k0_to_k15_with_the_qgc_classes() {
    let p = profile("cline");
    for i in 0..=15 {
        row(&p, &format!("K{i}"));
    }
    for g in ["K0", "K1", "K5", "K6", "K10", "K11", "K12", "K13"] {
        assert_eq!(row(&p, g).class, RowClass::Gating, "{g}");
    }
    for id in ["K2", "K3", "K7", "K8", "K15"] {
        let r = row(&p, id);
        assert_eq!(r.class, RowClass::Scoped, "{id}");
        assert!(r.trigger.is_some(), "{id} needs a trigger");
    }
    for id in ["K4", "K9", "K14"] {
        assert_eq!(row(&p, id).class, RowClass::Outside, "{id}");
    }
    for id in ["K7", "K8"] {
        let r = row(&p, id);
        assert_eq!((r.pass.min_pass, r.pass.runs), (8, 9), "{id}");
    }
}

#[test]
fn opencode_runs_opencode_acp_with_its_kill_switches() {
    let p = profile("opencode");
    assert_eq!(p.name, "opencode");
    assert_eq!(p.version, "1.18.32");
    assert_eq!(p.gate.name, "QG-O");
    assert_eq!(
        p.transports.acp.argv,
        vec!["/opt/harness/bin/opencode".to_string(), "acp".to_string()]
    );
    assert!(p.transports.cli.is_none());
    for k in [
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_SHARE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_LSP_DOWNLOAD",
        "OPENCODE_DISABLE_CLAUDE_CODE",
    ] {
        assert_eq!(p.env.get(k).map(String::as_str), Some("1"), "{k}");
    }
    for h in ["opencode.json", ".opencode/**"] {
        assert!(p.hidden_paths.contains(&h.to_string()), "{h}");
    }
    for id in [
        "K0-O", "K1-O", "K3-O", "K5-O", "K6-O", "K7-O", "K10-O", "K13-O",
    ] {
        row(&p, id);
    }
}

#[test]
fn k7_rows_count_nine_exercised_runs_within_thirty() {
    for (engine, id) in [("cline", "K7"), ("opencode", "K7-O")] {
        let p = profile(engine);
        let r = row(&p, id);
        assert_eq!((r.pass.min_pass, r.pass.runs), (8, 9), "{id}");
        assert_eq!(
            r.pass.thresholds.get("maxRuns").copied(),
            Some(30.0),
            "{id}"
        );
        assert!(r.check.contains("exercised"), "{id}: {}", r.check);
    }
}

#[test]
fn the_union_of_hidden_paths_is_the_fr10_list() {
    let all = hidden_paths_union(&[profile("cline"), profile("opencode"), profile("fake-acp")]);
    for h in [
        ".cline/hooks/**",
        ".cline/plugins/**",
        ".cline/agents/**",
        "opencode.json",
        ".opencode/**",
    ] {
        assert!(all.contains(h), "{h}");
    }
}

#[test]
fn pinned_binary_hashes_are_sha256_hex_when_present() {
    for name in ["cline", "opencode"] {
        if let Some(sha) = &profile(name).binary.sha256 {
            assert_eq!(sha.len(), 64, "{name}");
            assert!(
                sha.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{name}"
            );
        }
    }
}

fn engine_lock() -> toml::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/lock.toml");
    toml::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

// engine/lock.toml holds the promoted Cline binary SHA-256 and the OpenCode pins; the
// profiles and the worker image build spec carry the same values.
#[test]
fn profile_and_image_pins_equal_engine_lock() {
    let lock = engine_lock();
    let cline_sha = lock["cline"]["binarySha256"].as_str().unwrap();
    let oc_sha = lock["opencode"]["binarySha256"].as_str().unwrap();
    let oc_asset_sha = lock["opencode"]["assetSha256"].as_str().unwrap();
    for sha in [cline_sha, oc_sha, oc_asset_sha] {
        assert_eq!(sha.len(), 64);
    }
    assert_eq!(lock["cline"]["tag"].as_str(), Some("cli-v3.0.71"));
    assert_eq!(lock["opencode"]["version"].as_str(), Some("1.18.32"));
    assert!(
        lock["cline"]["artifact"]
            .as_str()
            .unwrap()
            .ends_with(&format!("engine/cline/{cline_sha}/cline"))
    );
    assert_eq!(profile("cline").binary.sha256.as_deref(), Some(cline_sha));
    assert_eq!(profile("opencode").binary.sha256.as_deref(), Some(oc_sha));
    let spec = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../images/codebuild/worker-python.yml"),
    )
    .unwrap();
    assert!(spec.contains(&format!("OPENCODE_SHA256: {oc_sha}")));
    assert!(spec.contains(&format!("OPENCODE_ARCHIVE_SHA256: {oc_asset_sha}")));
}

// The promoted binary is built from the series the fork lock pins, so the engine lock's
// tree is the series lock's `[series] tree` itself (patches 0001-0010 promoted 2026-09-30).
#[test]
fn the_engine_lock_tree_is_the_series_tree() {
    let series: toml::Value = toml::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/cline/lock.toml"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        engine_lock()["cline"]["tree"].as_str(),
        series["series"]["tree"].as_str()
    );
}

#[test]
fn the_series_lock_and_engine_lock_agree_on_the_fork() {
    let series = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../engine/cline/lock.toml"),
    )
    .unwrap();
    let lock = engine_lock();
    for key in ["commit", "tree"] {
        let v = lock["cline"][key].as_str().unwrap();
        assert!(
            series.contains(v),
            "{key} {v} not in engine/cline/lock.toml"
        );
    }
}
