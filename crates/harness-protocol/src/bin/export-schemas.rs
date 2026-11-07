//! Writes the contract artifacts to `schemas/` (or the directory given as the only argument).
//! usage: cargo run -p harness-protocol --bin export-schemas [-- <dir>]

use harness_protocol::export::{all_schemas, render};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    // nosemgrep: args-os -- A developer tool: its optional first argument sets the directory the schema files are written to, and it is not used for a security decision.
    let dir = std::env::args_os().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas"),
        PathBuf::from,
    );
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("export-schemas: {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    let files = all_schemas();
    for (name, value) in &files {
        let path = dir.join(name);
        if let Err(e) = std::fs::write(&path, render(value)) {
            eprintln!("export-schemas: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    println!(
        "export-schemas: wrote {} files to {}",
        files.len(),
        dir.display()
    );
    ExitCode::SUCCESS
}
