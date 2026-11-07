//! What `ham` prints when `config.toml` names no stage URL. The setting is called `api_endpoint` everywhere a person reads about it, its value is the
//! stage URL, which ends in `/direct`, and `ham config set api_endpoint <url>` sets it. The message says the same.
#![allow(clippy::unwrap_used)]

mod support;

use std::process::{Command, Stdio};

/// `ham <args>` with an empty HOME: no config file, no AWS configuration.
fn ham_without_config(args: &[&str]) -> (Option<i32>, String) {
    let root = support::scratch("no-endpoint");
    let out = Command::new(env!("CARGO_BIN_EXE_ham"))
        .env_clear()
        .args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &root)
        .env("TMPDIR", &root)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_command_that_needs_the_stage_names_the_setting_and_the_form_of_its_url() {
    let (code, err) = ham_without_config(&["runs"]);
    assert_eq!(code, Some(1), "{err}");
    for want in [
        "api_endpoint",
        "`ham config set api_endpoint <url>`",
        "/direct",
        "config.toml",
    ] {
        assert!(err.contains(want), "{want:?} missing from: {err}");
    }
    assert!(
        !err.contains("/v1") && !err.contains("apiEndpoint"),
        "the message names neither the API path nor the spelling of the file: {err}"
    );
}
