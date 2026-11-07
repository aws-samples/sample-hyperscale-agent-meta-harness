//! Compiles `engine/profiles/*.toml` into the crate (`engine_profile::BUNDLED`), so a Lambda or a
//! binary that checks an engine name knows every profile of its build: adding an engine is a
//! profile file and a rebuild, with no Rust change.

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("../../engine/profiles");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            let abs = p.canonicalize().ok()?;
            Some((stem, abs))
        })
        .collect();
    entries.sort();
    let mut out = String::from(
        "/// `engine/profiles/<name>.toml` at build time: (file stem, file text), sorted by name.\n\
         pub const BUNDLED: &[(&str, &str)] = &[\n",
    );
    for (stem, path) in &entries {
        println!("cargo:rerun-if-changed={}", path.display());
        out.push_str(&format!(
            "    ({stem:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");
    let dest =
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("bundled_profiles.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("{}: {e}", dest.display()));
}
