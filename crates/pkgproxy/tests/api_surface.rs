//! Builder A codes against these signatures while Builder B fills the bodies; a signature change needs a
//! contract amendment, so this file fails when one moves. The secret-handling parts that need no network are
//! real from C0 (T7).

#![allow(clippy::unwrap_used)]

use pkgproxy::{
    Auth, Config, Handle, Log, Prefix, Secret, StatsSnapshot, Upstream, codeartifact_upstreams,
    go_upstream, start,
};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

struct Quiet;
impl Log for Quiet {
    fn line(&self, _text: &str) {}
}

/// A token stand-in built at run time, so no token-shaped literal sits in the source.
fn token() -> String {
    ["tok", "test", "0123456789", "not", "real"].join("-")
}

#[test]
fn the_signatures_are_the_frozen_ones() {
    let _: fn(&Handle) -> SocketAddr = Handle::addr;
    let _: fn(&Handle) -> BTreeMap<String, String> = Handle::env;
    let _: fn(&Handle) -> String = Handle::maven_settings_xml;
    // The engine's cargo config file (crates.io replaced by the proxy's registry; no secret), like the Maven settings.
    let _: fn(&Handle) -> String = Handle::cargo_config_toml;
    let _: fn(&Handle) -> StatsSnapshot = Handle::snapshot;
    let _: fn(&str, &str, Secret) -> Vec<Upstream> = codeartifact_upstreams;
    let _: fn(&str) -> Upstream = go_upstream;
    // `start` and `Handle::stop` are async; naming them proves they exist with these arguments.
    let _ = |cfg: Config| start(cfg);
    let _ = |h: Handle| h.stop();
}

#[test]
fn a_config_has_the_frozen_members() {
    let cfg = Config {
        listen: "127.0.0.1:3128".parse().unwrap(),
        upstreams: vec![Upstream {
            prefix: Prefix::Pypi,
            base: "https://example.d.codeartifact.us-east-1.amazonaws.com/pypi/packages".into(),
            auth: Auth::Basic {
                user: "aws".into(),
                secret: Secret::new(token()),
            },
        }],
        expires_at_unix: Some(1_790_000_000),
        max_connections: 64,
        request_timeout: Duration::from_secs(900),
        idle_timeout: Duration::from_secs(60),
        max_redirects: 5,
        log: Arc::new(Quiet),
    };
    assert_eq!(cfg.upstreams.len(), 1);
    assert_eq!(cfg.upstreams[0].prefix, Prefix::Pypi);
}

#[test]
fn the_five_prefixes_are_ordered_and_comparable() {
    // `Cargo` joins after the four of the contract, so the order of the first four is unchanged.
    let all = [
        Prefix::Pypi,
        Prefix::Npm,
        Prefix::Maven,
        Prefix::Go,
        Prefix::Cargo,
    ];
    let mut sorted = all;
    sorted.sort();
    assert_eq!(sorted, all);
    assert_ne!(Prefix::Npm, Prefix::Maven);
    assert_ne!(Prefix::Go, Prefix::Cargo);
    // `Raw` sends the secret as the whole Authorization value (CodeArtifact's cargo endpoint).
    let _ = (
        Auth::None,
        Auth::Bearer(Secret::new("x".into())),
        Auth::Raw(Secret::new("x".into())),
    );
}

#[test]
fn a_secret_never_prints_through_debug_display_or_a_containing_type() {
    let secret = Secret::new(token());
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(format!("{secret}"), "<redacted>");
    let up = Upstream {
        prefix: Prefix::Npm,
        base: "https://example.test/npm/packages".into(),
        auth: Auth::Bearer(Secret::new(token())),
    };
    let basic = Auth::Basic {
        user: "aws".into(),
        secret: Secret::new(token()),
    };
    let cfg = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstreams: vec![up],
        expires_at_unix: None,
        max_connections: 64,
        request_timeout: Duration::from_secs(900),
        idle_timeout: Duration::from_secs(60),
        max_redirects: 5,
        log: Arc::new(Quiet),
    };
    for shown in [
        format!("{:?}", cfg),
        format!("{:#?}", cfg),
        format!("{:?}", cfg.upstreams[0]),
        format!("{:?}", basic),
    ] {
        assert!(
            !shown.contains(&token()),
            "Debug printed the token: {shown}"
        );
        assert!(!shown.contains("0123456789"), "{shown}");
    }
}

#[test]
fn stats_start_empty() {
    let s = StatsSnapshot::default();
    assert_eq!((s.requests, s.bytes, s.refused), (0, 0, 0));
    assert!(s.by_prefix.is_empty() && s.statuses.is_empty());
    let mut t = s.clone();
    t.statuses.insert(200, 3);
    t.by_prefix.insert("pypi".into(), 3);
    assert_ne!(s, t);
}
