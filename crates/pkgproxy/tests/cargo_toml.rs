//! The `config.toml` that goes into the engine's `CARGO_HOME`: cargo's `crates.io` is replaced by the proxy's registry. Like the
//! Maven `settings.xml` it holds no secret; it is the same text for the engine's session and for the report steps.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

#[tokio::test]
async fn the_cargo_config_file_for_the_engine_names_the_proxy_and_holds_no_secret() {
    let r = rig(nothing(), nothing(), nothing()).await;
    let port = r.addr().port();
    let toml = r.proxy.cargo_config_toml();
    assert_eq!(
        toml,
        format!(
            "[source.crates-io]\nreplace-with = \"hamh-proxy\"\n\n[registries.hamh-proxy]\nindex = \"sparse+http://127.0.0.1:{port}/cargo/\"\n\n[net]\nretry = 3\n\n[term]\ncolor = \"never\"\n"
        )
    );
    assert!(
        toml.contains("/cargo/\""),
        "a sparse registry url ends in a slash"
    );
    assert!(!toml.contains(&r.token) && !toml.contains("token") && !toml.contains("credential"));
}

#[tokio::test]
async fn the_cargo_config_names_the_real_port_of_each_proxy() {
    let a = rig(nothing(), nothing(), nothing()).await;
    let b = rig(nothing(), nothing(), nothing()).await;
    for r in [&a, &b] {
        let toml = r.proxy.cargo_config_toml();
        assert!(
            toml.contains(&format!(
                "index = \"sparse+http://127.0.0.1:{}/cargo/\"",
                r.addr().port()
            )),
            "{toml}"
        );
    }
    assert_ne!(a.proxy.cargo_config_toml(), b.proxy.cargo_config_toml());
}
