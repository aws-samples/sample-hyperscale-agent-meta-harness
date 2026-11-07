//! Protocol 1.40.0, wire B of the tool layer: the registration template the Cline profile names (`engine/profiles/templates/mcp-cline.json`).
//! agentd fills two placeholders once it has bound the tool server's port and writes the result where the engine reads its tool servers from.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use std::path::PathBuf;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn template_text() -> String {
    std::fs::read_to_string(repo().join("engine/profiles/templates/mcp-cline.json")).unwrap()
}

/// What agentd does with the template: replace each `{name}` it knows.
fn render(text: &str, name: &str, url: &str) -> String {
    text.replace("{toolsName}", name).replace("{toolsUrl}", url)
}

#[test]
fn the_template_renders_streamable_http() {
    let doc: Value = serde_json::from_str(&render(
        &template_text(),
        "ham",
        "http://127.0.0.1:3130/mcp",
    ))
    .unwrap();
    let servers = doc["mcpServers"].as_object().unwrap();
    assert_eq!(servers.keys().collect::<Vec<_>>(), ["ham"]);
    let entry = &servers["ham"];
    assert_eq!(
        entry["transport"],
        json!({"type": "streamableHttp", "url": "http://127.0.0.1:3130/mcp"})
    );
    assert!(
        entry.get("url").is_none(),
        "a bare url member is the legacy form"
    );
    assert_eq!(entry["timeout"], json!(120));
    assert_eq!(
        entry.as_object().unwrap().len(),
        2,
        "transport and timeout, nothing else"
    );
    assert_eq!(doc.as_object().unwrap().len(), 1);
}

#[test]
fn the_template_holds_exactly_the_two_placeholders_agentd_fills() {
    let text = template_text();
    // A stack of the open braces: a JSON brace holds a whole object, a placeholder holds a bare word.
    let mut tokens: Vec<&str> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for (i, c) in text.char_indices() {
        match c {
            '{' => open.push(i),
            '}' => {
                let start = open.pop().unwrap();
                let inner = &text[start + 1..i];
                if !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    tokens.push(inner);
                }
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "braces balance");
    tokens.sort_unstable();
    assert_eq!(tokens, ["toolsName", "toolsUrl"]);
    // Rendered with the real values nothing is left over, and the result is plain JSON.
    let rendered = render(&text, "ham", "http://127.0.0.1:3130/mcp");
    assert!(!rendered.contains("{tools"), "{rendered}");
}

#[test]
fn the_cline_profile_names_this_template() {
    let profile = std::fs::read_to_string(repo().join("engine/profiles/cline.toml")).unwrap();
    assert!(
        profile
            .lines()
            .any(|l| l.trim() == "template = \"engine/profiles/templates/mcp-cline.json\""),
        "cline.toml names the template"
    );
}
