//! The research module opens no outbound path of its own: the VM firewall is unchanged, and the only
//! ways out of the module are the two signed clients that already existed for the internal API and the bucket. This test is the proof by construction that nothing in
//! the module's sources can add a path: it reads them, as `engine_neutral.rs` reads the sources for engine names.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

/// The module's source files with their comment lines removed.
fn sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/direct/research");
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            let code: String = std::fs::read_to_string(&p)
                .unwrap()
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            out.push((p.file_name().unwrap().to_string_lossy().into_owned(), code));
        }
    }
    out.sort();
    assert!(out.len() >= 8, "the module's files were not found");
    out
}

fn files_with(needle: &str) -> Vec<String> {
    sources()
        .into_iter()
        .filter(|(_, code)| code.contains(needle))
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn only_the_route_and_the_store_use_the_signed_client() {
    assert_eq!(files_with("SignedHttp"), ["route.rs", "store.rs"]);
}

#[test]
fn no_file_of_the_module_opens_a_connection_in_any_other_way() {
    for needle in [
        "TcpStream",
        "UdpSocket",
        "UnixStream",
        "hyper_util::client",
        "hyper::client",
        "Client::builder",
        "reqwest",
        "ureq",
        "hyper_rustls",
        "aws_sdk",
        "aws_config",
    ] {
        assert!(
            files_with(needle).is_empty(),
            "{needle} appears in {:?}",
            files_with(needle)
        );
    }
}

#[test]
fn the_module_starts_no_process() {
    for needle in [
        "Command::new",
        "std::process",
        "tokio::process",
        "libc::system",
    ] {
        assert!(
            files_with(needle).is_empty(),
            "{needle} appears in {:?}",
            files_with(needle)
        );
    }
}

#[test]
fn the_only_listener_is_bound_to_127_0_0_1_in_mod_rs() {
    assert_eq!(files_with("TcpListener::bind"), ["mod.rs"]);
    let mod_rs = sources()
        .into_iter()
        .find(|(n, _)| n == "mod.rs")
        .unwrap()
        .1;
    for line in mod_rs.lines().filter(|l| l.contains("TcpListener::bind")) {
        assert!(
            line.contains("\"127.0.0.1\""),
            "a listener not on the loopback address: {line}"
        );
    }
    for wild in ["0.0.0.0", "UNSPECIFIED"] {
        assert!(
            files_with(wild).is_empty(),
            "{wild} appears in {:?}",
            files_with(wild)
        );
    }
}
