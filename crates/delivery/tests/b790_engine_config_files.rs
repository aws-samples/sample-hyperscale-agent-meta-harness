//! A pull request never carries an engine's own configuration: `opencode.jsonc` is refused like `opencode.json`, in both
//! modes of the validator.
#![allow(clippy::unwrap_used)]

use delivery::validator::{EngineConfigMode, Protected, protected_in};

#[test]
fn b790_opencode_jsonc_is_protected_like_opencode_json_in_both_modes() {
    for mode in [EngineConfigMode::All, EngineConfigMode::ExecutableOnly] {
        for path in [
            "opencode.json",
            "opencode.jsonc",
            ".opencode/agent/x.md",
            ".opencode/plugins/p.js",
        ] {
            assert_eq!(
                protected_in(mode, path),
                Some(Protected::EngineConfig),
                "{mode:?} {path}"
            );
        }
    }
}

#[test]
fn b790_names_that_only_look_like_it_stay_deliverable() {
    for path in [
        "docs/opencode.json.md",
        "src/opencode.jsonc.rs",
        "my-opencode.jsonc",
        "opencode.js",
    ] {
        assert_eq!(protected_in(EngineConfigMode::All, path), None, "{path}");
    }
}
