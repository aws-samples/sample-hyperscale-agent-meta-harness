//! The texts of `ham-core` that tell a person that this `ham` or the stage is too old: each names the command `ham update`.
//!
//! A text about a `ham` that is too old says `update ham (run ham update)`. A text about a stage that is too old keeps its direction and
//! adds `(run ham update; ask the person who runs the stage if that is not you)`: `ham update` updates a stage with the caller's own AWS
//! identity, so a person who uses a stage that someone else runs cannot update it.

#![allow(clippy::unwrap_used)]

use ham_core::config::{ConfigError, parse_config, read_config, set_key};
use ham_core::settings::Code;
use ham_core::{engine_file, resume, retry};

/// How a text about a stage that is too old names the command and the person who can run it.
const STAGE: &str = "(run ham update; ask the person who runs the stage if that is not you)";

#[test]
fn b513_a_write_to_a_file_of_a_newer_ham_names_ham_update() {
    let read = read_config("configVersion = 2\n\n[ui]\nfuture = 1\n");
    let refused = set_key(&read.config, "region", "us-east-2").unwrap_err();
    assert!(matches!(refused, ConfigError::Newer { version: 2 }));
    assert_eq!(
        refused.to_string(),
        "config.toml was written by a newer ham (configVersion 2); update ham (run ham update) to change it. Nothing was written"
    );
}

#[test]
fn b513_a_run_refused_on_a_file_of_a_newer_ham_names_ham_update() {
    let refused = parse_config("configVersion = 2\n\n[run]\nfuture = 1\n")
        .unwrap_err()
        .to_string();
    assert!(
        refused.starts_with(
            "config.toml: this file was written by a newer ham (configVersion 2) and holds keys this ham does not know that may steer a run:\n"
        ),
        "{refused}"
    );
    assert!(
        refused.ends_with("\nUpdate ham (run ham update), or remove those keys"),
        "{refused}"
    );
}

#[test]
fn b513_the_warning_of_a_file_of_a_newer_ham_names_ham_update() {
    let read = read_config("configVersion = 2\n");
    let warning = read
        .findings
        .iter()
        .find(|f| f.code == Code::NewerVersion)
        .unwrap();
    assert_eq!(
        warning.message,
        "written by a newer ham (configVersion 2); this ham reads the keys it knows and changes nothing in the file; update ham (run ham update) to change it"
    );
}

#[test]
fn b513_an_engine_file_of_a_newer_schema_names_ham_update() {
    let reading = engine_file::read_text("schema = 2\n[reasoning]\neffort = \"low\"\n", "cline");
    assert_eq!(reading.problems.len(), 1, "{:?}", reading.problems);
    let problem = &reading.problems[0];
    assert_eq!(problem.code, "newer_schema");
    assert_eq!(
        problem.message,
        "the file is written for schema 2, which a newer ham reads; this ham reads schema 1: update ham (run ham update), or change the file"
    );
}

#[test]
fn b513_a_stage_without_retry_names_ham_update_and_who_can_run_it() {
    assert_eq!(
        retry::explain(404, "not-found", "no such route"),
        format!(
            "this stage does not offer retry yet: it needs a newer backend {STAGE}. Start a new run with the same prompt (n) instead"
        )
    );
}

#[test]
fn b513_a_stage_without_resume_names_ham_update_and_who_can_run_it() {
    assert_eq!(
        resume::explain(404, "not-found", "no such route"),
        format!(
            "this stage does not offer resume yet: it needs a newer backend {STAGE}. Use ham continue instead"
        )
    );
}
