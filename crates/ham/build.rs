//! Embeds the root `VERSION` file as `HAMH_VERSION`.
//!
//! The file is `<repo>/VERSION`: one line, `N.NN`, where N is the major number and NN a two-digit counter. A missing file or any
//! other content stops the build with the reason: there is no default, because a client that guesses its own version defeats the tag.
//! `crates/control-api/build.rs` reads the same file the same way. The file is no input of the worker image key or the deployment digest.

use std::path::PathBuf;

/// `^[0-9]+\.[0-9]{2}$`, without a regex crate (this crate's manifest and lock file may not change).
fn is_version(text: &str) -> bool {
    let Some((major, counter)) = text.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && counter.len() == 2
        && counter.bytes().all(|b| b.is_ascii_digit())
}

fn main() {
    println!("cargo:rerun-if-changed=../../VERSION");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|| panic!("cargo did not set CARGO_MANIFEST_DIR"));
    let path = PathBuf::from(manifest).join("../../VERSION");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. The root VERSION file holds the version `ham` shows (N.NN, for example 4.01)",
            path.display()
        )
    });
    let version = raw.trim();
    if !is_version(version) {
        panic!(
            "{} holds {version:?}, which is not a version. Write the major number, a dot and a two-digit counter, for example 4.01",
            path.display()
        );
    }
    println!("cargo:rustc-env=HAMH_VERSION={version}");
}
