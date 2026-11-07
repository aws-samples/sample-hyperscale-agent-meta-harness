//! Embeds the root `VERSION` file as `HAMH_VERSION`. The file is
//! `N.NN`; a missing or malformed file fails the build, there is no fallback version. `src/version_check.rs` holds the shape check and is
//! shared with the library, where `tests/version.rs` covers it.

use std::path::PathBuf;

include!("src/version_check.rs");

fn main() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("CARGO_MANIFEST_DIR is not set"));
    let file = manifest.join("../../VERSION");
    println!("cargo:rerun-if-changed=../../VERSION");
    let raw = std::fs::read_to_string(&file).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}; the backend version is the content of the root VERSION file",
            file.display()
        )
    });
    let version = check_version(&raw).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    println!("cargo:rustc-env=HAMH_VERSION={version}");
}
