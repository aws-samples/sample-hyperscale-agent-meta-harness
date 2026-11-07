//! Protocol 1.40.0, wire B of the tool layer: the optional `[mcp]` table of an engine profile. It says how the engine learns of the harness's tool
//! server: `via = "file"` names the settings file the engine reads (rendered from a template once agentd has bound the port), `via = "none"` says
//! the engine takes no server. These tests load the committed profiles and refuse every table the root supervisor must never act on: it renders the
//! file as root and hands it to the engine user, so the path stays under the engine's own directories and the template under the templates root.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use harness_core::engine_profile::{
    MCP_MAX_RESULT_CHARS, MCP_MIN_RESULT_CHARS, MCP_TEMPLATES_ROOT, ProfileError, load_profile,
    parse_profile,
};
use harness_protocol::engine_profile::{EngineProfile, McpVia};
use std::path::PathBuf;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn profile(name: &str) -> EngineProfile {
    load_profile(&repo().join(format!("engine/profiles/{name}.toml")))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

const MINIMAL: &str = r#"
schemaVersion = 1
name = "demo"
version = "1.0.0"
hiddenPaths = [".demo"]

[binary]
path = "demo-engine"

[transports.acp]
argv = ["demo-engine", "--acp", "--cwd", "{workspace}"]
stdin = "pipe"
cancel = { method = "acp-cancel", killAfterS = 5 }

[gate]
name = "QG-D"

[[rows]]
id = "K0"
class = "gating"
check = "handshake"
pass = { minPass = 1, runs = 1 }
"#;

/// MINIMAL plus an `[mcp]` table whose lines are the given ones.
fn with_mcp(lines: &[&str]) -> String {
    format!("{MINIMAL}\n[mcp]\n{}\n", lines.join("\n"))
}

const GOOD: [&str; 5] = [
    "via = \"file\"",
    "path = \"{dataDir}/settings/tools.json\"",
    "template = \"engine/profiles/templates/mcp-cline.json\"",
    "maxResultChars = 7500",
    "shadow = []",
];

/// The table with the line that starts with `key` replaced by `line` (or removed when `line` is empty).
fn edited(key: &str, line: &str) -> String {
    let lines: Vec<&str> = GOOD
        .iter()
        .filter_map(|l| {
            if l.starts_with(key) {
                (!line.is_empty()).then_some(line)
            } else {
                Some(*l)
            }
        })
        .collect();
    with_mcp(&lines)
}

fn err(text: &str) -> ProfileError {
    parse_profile(text).expect_err("profile should be rejected")
}

#[test]
fn the_real_cline_profile_loads_with_an_mcp_table() {
    let p = profile("cline");
    let mcp = p.mcp.as_ref().expect("cline.toml carries the [mcp] table");
    assert_eq!(mcp.via, McpVia::File);
    let path = mcp.path.as_deref().unwrap();
    assert!(
        path.starts_with("{dataDir}/") && path.ends_with("settings/cline_mcp_settings.json"),
        "{path}"
    );
    assert_eq!(mcp.max_result_chars, Some(7500));
    assert!(mcp.shadow.is_empty());
    let template = mcp.template.as_deref().unwrap();
    assert_eq!(template, "engine/profiles/templates/mcp-cline.json");
    assert!(
        repo().join(template).is_file(),
        "the template the profile names exists in the repository (and so in the image)"
    );
    let reg = mcp.registration().unwrap();
    assert_eq!(
        (reg.path, reg.template, reg.max_result_chars),
        (path, template, 7500)
    );
}

#[test]
fn profiles_without_the_table_are_unchanged() {
    for name in ["fake-acp", "opencode"] {
        assert!(
            profile(name).mcp.is_none(),
            "{name} has no [mcp] table: the layer adds nothing for it"
        );
    }
}

#[test]
fn the_profile_directory_holds_the_templates_in_a_subdirectory_the_loaders_skip() {
    // The loaders read `*.toml` files only, and the image copies the whole directory (images/worker-python/Dockerfile).
    let dir = repo().join("engine/profiles");
    assert!(dir.join("templates").is_dir());
    let engines = harness_core::engine_profile::load_profiles(&dir).unwrap();
    let names: Vec<&str> = engines.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["cline", "fake-acp", "opencode"]);
    assert_eq!(harness_core::engine_profile::bundled_names(), names);
    assert_eq!(MCP_TEMPLATES_ROOT, "engine/profiles/templates");
}

#[test]
fn a_good_table_parses_and_the_none_table_is_just_the_word() {
    let p = parse_profile(&with_mcp(&GOOD)).unwrap();
    assert_eq!(p.mcp.as_ref().unwrap().via, McpVia::File);
    // {configDir} is the engine's other directory.
    parse_profile(&edited("path", "path = \"{configDir}/mcp.json\"")).unwrap();
    // The shadow list names engine built-ins to switch off.
    let p = parse_profile(&edited(
        "shadow",
        "shadow = [\"search_codebase\", \"fetch_web_content\"]",
    ))
    .unwrap();
    assert_eq!(
        p.mcp.unwrap().shadow,
        ["search_codebase", "fetch_web_content"]
    );
    let none = parse_profile(&with_mcp(&["via = \"none\""])).unwrap();
    assert_eq!(none.mcp.as_ref().unwrap().via, McpVia::None);
    assert!(none.mcp.unwrap().registration().is_none());
}

#[test]
fn the_cap_on_a_result_holds_at_both_edges() {
    assert_eq!((MCP_MIN_RESULT_CHARS, MCP_MAX_RESULT_CHARS), (1000, 16000));
    parse_profile(&edited("maxResultChars", "maxResultChars = 1000")).unwrap();
    parse_profile(&edited("maxResultChars", "maxResultChars = 16000")).unwrap();
    for bad in [0, 999, 16001, 1_000_000] {
        assert_eq!(
            err(&edited(
                "maxResultChars",
                &format!("maxResultChars = {bad}")
            )),
            ProfileError::McpMaxResultChars(bad),
            "{bad}"
        );
    }
}

#[test]
fn a_file_table_needs_path_template_and_cap_and_a_none_table_takes_nothing() {
    for key in ["path", "template", "maxResultChars"] {
        assert_eq!(
            err(&edited(key, "")),
            ProfileError::McpFields,
            "{key} missing"
        );
    }
    for extra in [
        "path = \"{dataDir}/x.json\"",
        "template = \"engine/profiles/templates/x.json\"",
        "maxResultChars = 7500",
        "shadow = [\"a\"]",
    ] {
        assert_eq!(
            err(&with_mcp(&["via = \"none\"", extra])),
            ProfileError::McpFields,
            "{extra}"
        );
    }
}

#[test]
fn an_unknown_placeholder_in_the_path_is_refused_and_names_the_field() {
    match err(&edited("path", "path = \"{nope}/settings/tools.json\"")) {
        ProfileError::Placeholder { field, token } => {
            assert_eq!((field.as_str(), token.as_str()), ("mcp.path", "nope"))
        }
        other => panic!("{other:?}"),
    }
    match err(&edited("path", "path = \"{dataDir}/{nope}.json\"")) {
        ProfileError::Placeholder { field, .. } => assert_eq!(field, "mcp.path"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_path_stays_under_the_engines_own_directories() {
    for bad in [
        "{workspace}/tools.json", // the work tree the model and git can see
        "{fixtures}/tools.json",
        "{model}/tools.json",
        "/etc/tools.json", // absolute
        "tools.json",      // relative to nothing
        "{dataDir}",       // the directory itself
        "{dataDir}/",
        "{dataDir}/../tools.json",
        "{dataDir}/a/../tools.json",
        "{dataDir}//tools.json",
        "{dataDir}/./tools.json",
        "{dataDir}/{model}/tools.json", // a known placeholder after the root: still not a plain name
        "{dataDir}/{configDir}/x.json",
        "{dataDir}x/tools.json",
        "{dataDir}/tools.json/",
    ] {
        assert_eq!(
            err(&edited("path", &format!("path = \"{bad}\""))),
            ProfileError::McpPath(bad.to_string()),
            "{bad}"
        );
    }
}

#[test]
fn the_template_is_a_plain_path_under_the_templates_root() {
    for bad in [
        "../mcp.json",
        "engine/profiles/templates/../../../etc/passwd",
        "/engine/profiles/templates/mcp.json",
        "engine/profiles/templates", // the root itself
        "engine/profiles/templates/",
        "engine/profiles/other/mcp.json", // not under the root
        "engine/profiles/templates-x/mcp.json",
        "mcp.json",
        "engine/profiles/templates/{dataDir}/mcp.json", // a placeholder is never substituted in a template path
        "engine/profiles/templates/./mcp.json",
        "engine/profiles/templates//mcp.json",
        "",
    ] {
        assert_eq!(
            err(&edited("template", &format!("template = \"{bad}\""))),
            ProfileError::McpTemplate(bad.to_string()),
            "{bad:?}"
        );
    }
    parse_profile(&edited(
        "template",
        "template = \"engine/profiles/templates/sub/mcp-x.json\"",
    ))
    .unwrap();
}

#[test]
fn shadow_entries_are_names_without_spaces() {
    for bad in [
        "\"\"",
        "\"has space\"",
        "\"tab\\there\"",
        "\"new\\nline\"",
        "\" lead\"",
        "\"trail \"",
    ] {
        assert!(
            matches!(
                err(&edited("shadow", &format!("shadow = [{bad}]"))),
                ProfileError::McpShadow(_)
            ),
            "{bad}"
        );
    }
    let many: Vec<String> = (0..33).map(|i| format!("\"t{i}\"")).collect();
    assert!(matches!(
        err(&edited(
            "shadow",
            &format!("shadow = [{}]", many.join(", "))
        )),
        ProfileError::McpShadow(_)
    ));
    let long = format!("\"{}\"", "a".repeat(65));
    assert!(matches!(
        err(&edited("shadow", &format!("shadow = [{long}]"))),
        ProfileError::McpShadow(_)
    ));
    let at_limit: Vec<String> = (0..32)
        .map(|i| format!("\"{}{i}\"", "a".repeat(30)))
        .collect();
    parse_profile(&edited(
        "shadow",
        &format!("shadow = [{}]", at_limit.join(", ")),
    ))
    .unwrap();
}

#[test]
fn an_unknown_member_or_way_in_the_table_is_a_parse_error() {
    assert!(matches!(
        err(&with_mcp(&["via = \"acp\""])),
        ProfileError::Parse(_)
    ));
    assert!(matches!(
        err(&with_mcp(&["via = \"stdio\""])),
        ProfileError::Parse(_)
    ));
    assert!(matches!(
        err(&with_mcp(&["via = \"none\"", "transport = \"stdio\""])),
        ProfileError::Parse(_)
    ));
    assert!(
        matches!(
            err(&with_mcp(&["path = \"{dataDir}/x.json\""])),
            ProfileError::Parse(_)
        ),
        "via is required"
    );
}
