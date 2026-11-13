//! The pin of the OpenCode release is one fact kept in four places: `engine/lock.toml`, `engine/sources/opencode.toml`,
//! `engine/profiles/opencode.toml` and `images/codebuild/worker-python.yml`. These tests read the real files and fail when any of
//! the four names another release, another asset or another hash. `pin_problems` is the check; the tests that feed it a changed copy
//! show that it reports each kind of difference. The profile tests pin which K rows a local run judges and that the pinned release
//! has the goal command (2.2.1 or later).
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_core::engine_profile::load_profile;
use harness_protocol::engine_profile::EngineProfile;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn profile(name: &str) -> EngineProfile {
    let path = root().join(format!("engine/profiles/{name}.toml"));
    load_profile(&path).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn toml_file(rel: &str) -> toml::Value {
    let path = root().join(rel);
    toml::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

fn recipe() -> String {
    std::fs::read_to_string(root().join("images/codebuild/worker-python.yml")).unwrap()
}

/// Every difference between the OpenCode pin of the lock and the other three places.
fn pin_problems(
    profile: &EngineProfile,
    lock: &toml::Value,
    sources: &toml::Value,
    recipe: &str,
) -> Vec<String> {
    let lk = &lock["opencode"];
    let text = |v: &toml::Value, key: &str| v[key].as_str().unwrap_or("").to_string();
    let (version, asset, asset_sha, binary_sha) = (
        text(lk, "version"),
        text(lk, "asset"),
        text(lk, "assetSha256"),
        text(lk, "binarySha256"),
    );
    let mut out = Vec::new();
    let mut expect = |what: &str, got: &str, want: &str| {
        if got != want {
            out.push(format!("{what} is {got:?}, engine/lock.toml has {want:?}"));
        }
    };
    expect(
        "engine/profiles/opencode.toml version",
        &profile.version,
        &version,
    );
    expect(
        "engine/profiles/opencode.toml binary.sha256",
        profile.binary.sha256.as_deref().unwrap_or(""),
        &binary_sha,
    );
    expect(
        "engine/sources/opencode.toml version",
        sources["version"].as_str().unwrap_or(""),
        &version,
    );
    let arm = &sources["assets"]["linux-arm64"];
    expect(
        "engine/sources/opencode.toml linux-arm64 url",
        arm["url"].as_str().unwrap_or(""),
        &asset,
    );
    expect(
        "engine/sources/opencode.toml linux-arm64 sha256",
        arm["sha256"].as_str().unwrap_or(""),
        &asset_sha,
    );
    expect(
        "engine/sources/opencode.toml linux-arm64 member_sha256",
        arm["member_sha256"].as_str().unwrap_or(""),
        &binary_sha,
    );
    let url_for_version = format!("/download/v{version}/opencode-linux-arm64.tar.gz");
    if !asset.ends_with(&url_for_version) {
        out.push(format!(
            "engine/lock.toml asset {asset:?} does not end in {url_for_version:?}"
        ));
    }
    for (var, want) in [
        ("OPENCODE_URL", &asset),
        ("OPENCODE_ARCHIVE_SHA256", &asset_sha),
        ("OPENCODE_SHA256", &binary_sha),
    ] {
        let line = recipe
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{var}: ")))
            .unwrap_or("");
        if line != want {
            out.push(format!("images/codebuild/worker-python.yml {var} is {line:?}, engine/lock.toml has {want:?}"));
        }
    }
    let k0 = profile.rows.iter().find(|r| r.id == "K0-O");
    if !k0.is_some_and(|r| r.check.contains(&format!("agentInfo.version {version}"))) {
        out.push(format!(
            "the K0-O row of the profile does not name agentInfo.version {version}"
        ));
    }
    out
}

fn real() -> (EngineProfile, toml::Value, toml::Value, String) {
    (
        profile("opencode"),
        toml_file("engine/lock.toml"),
        toml_file("engine/sources/opencode.toml"),
        recipe(),
    )
}

#[test]
fn b792_the_profile_the_lock_the_sources_and_the_recipe_name_one_release() {
    let (p, lock, sources, recipe) = real();
    assert_eq!(
        pin_problems(&p, &lock, &sources, &recipe),
        Vec::<String>::new()
    );
}

#[test]
fn b792_a_profile_version_that_differs_from_the_lock_is_reported() {
    let (mut p, lock, sources, recipe) = real();
    p.version = "0.0.1".into();
    let found = pin_problems(&p, &lock, &sources, &recipe);
    assert!(
        found
            .iter()
            .any(|f| f.starts_with("engine/profiles/opencode.toml version")),
        "{found:?}"
    );
}

#[test]
fn b792_a_profile_binary_hash_that_differs_from_the_lock_is_reported() {
    let (mut p, lock, sources, recipe) = real();
    p.binary.sha256 = Some("0".repeat(64));
    let found = pin_problems(&p, &lock, &sources, &recipe);
    assert!(
        found.iter().any(|f| f.contains("binary.sha256")),
        "{found:?}"
    );
}

#[test]
fn b792_a_recipe_that_downloads_another_release_is_reported() {
    let (p, lock, sources, recipe) = real();
    let version = lock["opencode"]["version"].as_str().unwrap();
    let changed = recipe.replace(&format!("/download/v{version}/"), "/download/v0.0.1/");
    assert_ne!(
        changed, recipe,
        "the recipe holds the download URL of the pin"
    );
    let found = pin_problems(&p, &lock, &sources, &changed);
    assert!(
        found.iter().any(|f| f.contains("OPENCODE_URL")),
        "{found:?}"
    );
}

#[test]
fn b792_a_recipe_hash_that_differs_from_the_lock_is_reported() {
    let (p, lock, sources, recipe) = real();
    let archive = lock["opencode"]["assetSha256"].as_str().unwrap();
    let binary = lock["opencode"]["binarySha256"].as_str().unwrap();
    let found = pin_problems(
        &p,
        &lock,
        &sources,
        &recipe.replace(archive, &"1".repeat(64)),
    );
    assert!(
        found.iter().any(|f| f.contains("OPENCODE_ARCHIVE_SHA256")),
        "{found:?}"
    );
    let found = pin_problems(
        &p,
        &lock,
        &sources,
        &recipe.replace(binary, &"2".repeat(64)),
    );
    assert!(
        found.iter().any(|f| f.contains("OPENCODE_SHA256")),
        "{found:?}"
    );
}

#[test]
fn b792_a_sources_file_that_differs_from_the_lock_is_reported() {
    let (p, lock, mut sources, recipe) = real();
    sources["version"] = toml::Value::String("0.0.1".into());
    sources["assets"]["linux-arm64"]["sha256"] = toml::Value::String("3".repeat(64));
    let found = pin_problems(&p, &lock, &sources, &recipe);
    assert!(
        found
            .iter()
            .any(|f| f.contains("sources/opencode.toml version")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|f| f.contains("linux-arm64 sha256")),
        "{found:?}"
    );
}

#[test]
fn b792_a_k0_row_that_names_another_version_is_reported() {
    let (mut p, lock, sources, recipe) = real();
    for r in p.rows.iter_mut().filter(|r| r.id == "K0-O") {
        r.check =
            "Local binary: ACP initialize and session/new succeed; agentInfo.version 0.0.1".into();
    }
    let found = pin_problems(&p, &lock, &sources, &recipe);
    assert!(found.iter().any(|f| f.contains("K0-O")), "{found:?}");
}

#[test]
fn b792_the_pinned_release_has_the_goal_command_from_2_2_1() {
    let version = profile("opencode").version;
    let parts: Vec<u32> = version.split('.').map(|n| n.parse().unwrap()).collect();
    assert_eq!(parts.len(), 3, "{version}");
    assert!(
        (parts[0], parts[1], parts[2]) >= (2, 2, 1),
        "OpenCode {version} predates the /goal command of 2.2.1; the pin stays at 2.2.1 or later"
    );
}

#[test]
fn b792_the_opencode_profile_marks_the_handshake_and_the_cancel_row_local() {
    let local: Vec<String> = profile("opencode")
        .rows
        .iter()
        .filter(|r| r.local)
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(local, ["K0-O", "K5-O"]);
}

#[test]
fn b792_the_cancel_row_keeps_the_ten_second_limit() {
    let p = profile("opencode");
    let row = p.rows.iter().find(|r| r.id == "K5-O").unwrap();
    assert_eq!(
        row.pass.thresholds.get("cancelWithinS").copied(),
        Some(10.0)
    );
    assert_eq!(profile("opencode").transports.acp.cancel.kill_after_s, 5);
}

#[test]
fn b792_the_cline_profile_marks_only_its_handshake_row_local() {
    let local: Vec<String> = profile("cline")
        .rows
        .iter()
        .filter(|r| r.local)
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(local, ["K0"]);
}

#[test]
fn b792_the_profile_declares_no_kill_switch_name_the_pinned_binary_dropped() {
    // The 2.2 line of OpenCode no longer holds these two names.
    let p = profile("opencode");
    for gone in ["OPENCODE_DISABLE_SHARE", "OPENCODE_DISABLE_LSP_DOWNLOAD"] {
        assert!(
            !p.env.contains_key(gone),
            "{gone} is a name the pinned binary does not hold"
        );
    }
    for kept in [
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_CLAUDE_CODE",
    ] {
        assert_eq!(p.env.get(kept).map(String::as_str), Some("1"), "{kept}");
    }
}
