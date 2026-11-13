//! The reference table of the settings in `docs/user-guide.md` is the registry's own output. The guide holds the text that `ham config docs`
//! prints between two marker lines, so the page and the program cannot disagree about a key, a default, a flag or when a change takes effect.

#![allow(clippy::unwrap_used)]

use ham_core::settings::reference_markdown;

const BEGIN: &str = "<!-- ham-settings:begin -->";
const END: &str = "<!-- ham-settings:end -->";

fn guide() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/user-guide.md");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The lines strictly between the two marker lines, each with its newline.
fn block(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let begins: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == BEGIN)
        .map(|(i, _)| i)
        .collect();
    let ends: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == END)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        begins.len(),
        1,
        "docs/user-guide.md needs exactly one line `{BEGIN}`"
    );
    assert_eq!(
        ends.len(),
        1,
        "docs/user-guide.md needs exactly one line `{END}`"
    );
    assert!(begins[0] < ends[0], "`{BEGIN}` comes before `{END}`");
    lines[begins[0] + 1..ends[0]]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect()
}

#[test]
fn the_user_guide_reference_equals_the_registry_output() {
    let want = reference_markdown();
    let got = block(&guide());
    assert_eq!(
        got, want,
        "docs/user-guide.md must hold the output of `ham config docs` between the two ham-settings marker lines; replace the block with the registry text"
    );
}
