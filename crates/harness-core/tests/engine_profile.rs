//! Engine-profile loader and validation.

use harness_core::engine_profile::{
    PLACEHOLDERS, ProfileError, fixtures_dir, hidden_paths_union, load_profile, load_profiles,
    parse_profile, resolve_launch,
};
use harness_protocol::common::Transport;
use harness_protocol::engine_profile::{CancelMethod, RowClass, StdinMode, Trigger};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const MINIMAL: &str = r#"
schemaVersion = 1
name = "demo"
version = "1.0.0"
hiddenPaths = [".demo"]

[binary]
path = "demo-engine"

[transports.acp]
argv = ["demo-engine", "--acp", "--cwd", "{workspace}"]
stdin = "pipe"
cancel = { method = "acp-cancel", killAfterS = 5 }

[gate]
name = "QG-D"

[[rows]]
id = "K0"
class = "gating"
check = "handshake"
pass = { minPass = 1, runs = 1 }
"#;

/// MINIMAL with `extra` inserted before the first table (so top-level keys stay top-level and an
/// `[env]` table precedes the others) and each `(from, to)` replaced once.
fn with(extra: &str, replace: &[(&str, &str)]) -> String {
    let mut s = MINIMAL.replacen("[binary]", &format!("{extra}[binary]"), 1);
    for (from, to) in replace {
        assert!(s.contains(from), "fixture lacks {from}");
        s = s.replacen(from, to, 1);
    }
    s
}

fn err(text: &str) -> ProfileError {
    parse_profile(text).expect_err("profile should be rejected")
}

#[test]
fn the_fake_acp_profile_loads_and_validates() {
    let p = load_profile(&repo().join("engine/profiles/fake-acp.toml")).unwrap();
    assert_eq!(p.name, "fake-acp");
    assert_eq!(p.schema_version, 1);
    assert_eq!(p.version, "0.1.0");
    assert_eq!(p.gate.name, "QG-FAKE");
    assert_eq!(p.transports.acp.stdin, StdinMode::Pipe);
    assert_eq!(p.transports.acp.cancel.method, CancelMethod::AcpCancel);
    assert!(p.transports.cli.is_none());
    let ids: Vec<&str> = p.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["K0", "K1"]);
    assert!(
        p.rows.iter().all(|r| r.local),
        "every fake row runs with --target local"
    );
    let k1 = p.rows.iter().find(|r| r.id == "K1").unwrap();
    assert_eq!(
        (k1.class, k1.trigger),
        (RowClass::Gating, Some(Trigger::T5))
    );
    assert!(
        fixtures_dir(&repo(), &p.name).join("transcripts").is_dir(),
        "fixtures/engines/fake-acp/transcripts"
    );
}

#[test]
fn the_fake_acp_launch_resolves_its_placeholders() {
    let p = load_profile(&repo().join("engine/profiles/fake-acp.toml")).unwrap();
    let vars = BTreeMap::from([
        ("workspace", "/w/repo".to_string()),
        ("dataDir", "/w/data".to_string()),
        (
            "fixtures",
            fixtures_dir(Path::new("/r"), "fake-acp")
                .display()
                .to_string(),
        ),
    ]);
    let l = resolve_launch(&p, Transport::Acp, &vars).unwrap();
    assert_eq!(
        l.argv,
        [
            "fake-acp-engine",
            "--transcripts",
            "/r/fixtures/engines/fake-acp/transcripts",
            "--workspace",
            "/w/repo"
        ]
    );
    assert_eq!(
        l.env.get("FAKE_ACP_LOG").map(String::as_str),
        Some("/w/data/fake-acp.log")
    );
    assert_eq!(l.stdin, StdinMode::Pipe);
    assert_eq!(l.cancel.kill_after_s, 5);
    assert_eq!(
        resolve_launch(&p, Transport::Cli, &vars),
        Err(ProfileError::TransportMissing(Transport::Cli))
    );
    let partial = BTreeMap::from([("workspace", "/w".to_string())]);
    assert_eq!(
        resolve_launch(&p, Transport::Acp, &partial),
        Err(ProfileError::MissingVar("fixtures".into()))
    );
}

#[test]
fn load_profiles_reads_every_profile_and_unions_hidden_paths() {
    let ps = load_profiles(&repo().join("engine/profiles")).unwrap();
    assert!(ps.iter().any(|p| p.name == "fake-acp"));
    let names: Vec<&str> = ps.iter().map(|p| p.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    let demo = parse_profile(MINIMAL).unwrap();
    let fake = ps.iter().find(|p| p.name == "fake-acp").unwrap().clone();
    let union = hidden_paths_union(&[demo, fake]);
    assert!(union.contains(".demo"));
    assert!(union.contains(".fake-acp"));
}

#[test]
fn load_profile_checks_the_file_stem_and_reports_io_errors() {
    let dir = PathBuf::from(
        std::env::var("TMPDIR").unwrap_or_else(|_| env!("CARGO_TARGET_TMPDIR").to_string()),
    )
    .join(format!("engine-profile-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wrong = dir.join("other.toml");
    std::fs::write(&wrong, MINIMAL).unwrap();
    assert_eq!(
        load_profile(&wrong),
        Err(ProfileError::NameMismatch {
            name: "demo".into(),
            stem: "other".into()
        })
    );
    assert!(matches!(
        load_profile(&dir.join("absent.toml")),
        Err(ProfileError::Io { .. })
    ));
    assert!(matches!(
        load_profiles(&dir.join("absent")),
        Err(ProfileError::Io { .. })
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_minimal_profile_parses() {
    let p = parse_profile(MINIMAL).unwrap();
    assert_eq!(p.name, "demo");
    assert!(p.env.is_empty() && p.env_remove.is_empty() && p.config_emitters.is_empty());
}

#[test]
fn malformed_toml_and_unknown_fields_are_parse_errors() {
    assert!(matches!(err("schemaVersion = "), ProfileError::Parse(_)));
    assert!(matches!(
        err(&with("telemetry = true\n", &[])),
        ProfileError::Parse(_)
    ));
    // transports.acp is required by the type.
    let no_acp = with(
        "",
        &[
            ("[transports.acp]", "[transports.cli]"),
            ("stdin = \"pipe\"", "stdin = \"null\""),
        ],
    );
    assert!(matches!(err(&no_acp), ProfileError::Parse(_)));
}

#[test]
fn schema_version_and_names_are_checked() {
    assert_eq!(
        err(&with("", &[("schemaVersion = 1", "schemaVersion = 2")])),
        ProfileError::SchemaVersion(2)
    );
    assert_eq!(
        err(&with("", &[("name = \"demo\"", "name = \"Demo Engine\"")])),
        ProfileError::Name("Demo Engine".into())
    );
    assert_eq!(
        err(&with("", &[("name = \"QG-D\"", "name = \"gate\"")])),
        ProfileError::GateName("gate".into())
    );
    assert_eq!(
        err(&with("", &[("version = \"1.0.0\"", "version = \"\"")])),
        ProfileError::Version
    );
}

#[test]
fn launches_are_checked() {
    assert_eq!(
        err(&with(
            "",
            &[(
                "argv = [\"demo-engine\", \"--acp\", \"--cwd\", \"{workspace}\"]",
                "argv = []"
            )]
        )),
        ProfileError::EmptyArgv(Transport::Acp)
    );
    assert_eq!(
        err(&with("", &[("stdin = \"pipe\"", "stdin = \"null\"")])),
        ProfileError::Stdin(Transport::Acp)
    );
    let cli_pipe = format!(
        "{MINIMAL}\n[transports.cli]\nargv = [\"demo\", \"-p\"]\nstdin = \"pipe\"\ncancel = {{ method = \"sigterm\", killAfterS = 5 }}\n"
    );
    assert_eq!(err(&cli_pipe), ProfileError::Stdin(Transport::Cli));
    assert_eq!(
        err(&with("", &[("killAfterS = 5", "killAfterS = 0")])),
        ProfileError::KillAfter(Transport::Acp)
    );
}

#[test]
fn placeholders_are_checked() {
    assert_eq!(PLACEHOLDERS.len(), 8);
    assert_eq!(
        err(&with("", &[("{workspace}", "{home}")])),
        ProfileError::Placeholder {
            field: "transports.acp.argv".into(),
            token: "home".into()
        }
    );
    assert_eq!(
        err(&with("", &[("{workspace}", "{workspace")])),
        ProfileError::Unclosed {
            field: "transports.acp.argv".into()
        }
    );
    let env = with("[env]\nLOG = \"{nowhere}/x\"\n", &[]);
    assert_eq!(
        err(&env),
        ProfileError::Placeholder {
            field: "env.LOG".into(),
            token: "nowhere".into()
        }
    );
}

#[test]
fn credentials_never_enter_the_engine_environment() {
    for key in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
    ] {
        let text = with(&format!("[env]\n{key} = \"x\"\n"), &[]);
        assert_eq!(err(&text), ProfileError::CredentialEnv(key.into()), "{key}");
    }
    assert_eq!(
        err(&with("[env]\n\"lower-case\" = \"x\"\n", &[])),
        ProfileError::EnvKey("lower-case".into())
    );
    let launch_env = with(
        "",
        &[(
            "stdin = \"pipe\"",
            "stdin = \"pipe\"\nenv = { AWS_SESSION_TOKEN = \"t\" }",
        )],
    );
    assert_eq!(
        err(&launch_env),
        ProfileError::CredentialEnv("AWS_SESSION_TOKEN".into())
    );
}

#[test]
fn hidden_paths_must_be_repo_relative() {
    for bad in ["/outside/file", "../outside", "a/../../b", ""] {
        let text = with(
            "",
            &[(
                "hiddenPaths = [\".demo\"]",
                &format!("hiddenPaths = [{bad:?}]"),
            )],
        );
        assert_eq!(err(&text), ProfileError::HiddenPath(bad.into()), "{bad}");
    }
}

#[test]
fn rows_are_checked() {
    assert_eq!(
        err(&with("", &[("id = \"K0\"", "id = \"X9\"")])),
        ProfileError::RowId("X9".into())
    );
    assert_eq!(
        err(&with(
            "",
            &[(
                "pass = { minPass = 1, runs = 1 }",
                "pass = { minPass = 2, runs = 1 }"
            )]
        )),
        ProfileError::PassRule("K0".into())
    );
    assert_eq!(
        err(&with(
            "",
            &[(
                "pass = { minPass = 1, runs = 1 }",
                "pass = { minPass = 0, runs = 0 }"
            )]
        )),
        ProfileError::PassRule("K0".into())
    );
    assert_eq!(
        err(&with("", &[("class = \"gating\"", "class = \"scoped\"")])),
        ProfileError::ScopedWithoutTrigger("K0".into())
    );
    let dup = format!(
        "{MINIMAL}\n[[rows]]\nid = \"K0\"\nclass = \"gating\"\ncheck = \"again\"\npass = {{ minPass = 1, runs = 1 }}\n"
    );
    assert_eq!(err(&dup), ProfileError::DuplicateRow("K0".into()));
    let none = format!("rows = []\n{}", MINIMAL.split("[[rows]]").next().unwrap());
    assert_eq!(err(&none), ProfileError::NoRows);
}

#[test]
fn a_binary_digest_must_be_lowercase_sha256() {
    assert_eq!(
        err(&with(
            "",
            &[(
                "path = \"demo-engine\"",
                "path = \"demo-engine\"\nsha256 = \"ABC\""
            )]
        )),
        ProfileError::Sha256("ABC".into())
    );
    let ok = with(
        "",
        &[(
            "path = \"demo-engine\"",
            &format!("path = \"demo-engine\"\nsha256 = \"{}\"", "a".repeat(64)),
        )],
    );
    assert!(parse_profile(&ok).is_ok());
}

#[test]
fn errors_render_readable_messages() {
    assert_eq!(ProfileError::NoRows.to_string(), "profile has no K rows");
    assert!(
        ProfileError::MissingVar("model".into())
            .to_string()
            .contains("model")
    );
}

#[test]
fn a_launch_names_the_variable_that_pins_the_model_at_session_new() {
    let vars = BTreeMap::from([("workspace", "/w".to_string())]);
    let p = parse_profile(&with(
        "",
        &[(
            "stdin = \"pipe\"",
            "stdin = \"pipe\"\nmodelEnv = \"DEMO_MODEL\"",
        )],
    ))
    .unwrap();
    let l = resolve_launch(&p, Transport::Acp, &vars).unwrap();
    assert_eq!(l.model_env.as_deref(), Some("DEMO_MODEL"));
    let none = resolve_launch(&parse_profile(MINIMAL).unwrap(), Transport::Acp, &vars).unwrap();
    assert_eq!(none.model_env, None);
    assert_eq!(
        err(&with(
            "",
            &[(
                "stdin = \"pipe\"",
                "stdin = \"pipe\"\nmodelEnv = \"demo-model\""
            )]
        )),
        ProfileError::EnvKey("demo-model".into())
    );
    assert_eq!(
        err(&with(
            "",
            &[(
                "stdin = \"pipe\"",
                "stdin = \"pipe\"\nmodelEnv = \"AWS_SESSION_TOKEN\""
            )]
        )),
        ProfileError::CredentialEnv("AWS_SESSION_TOKEN".into())
    );
}

#[test]
fn cline_pins_the_model_at_session_new_through_cline_model() {
    // Fork patch 0005 reads CLINE_MODEL at session/new.
    let vars = BTreeMap::from([
        ("workspace", "/w/repo".to_string()),
        ("dataDir", "/w/data".to_string()),
        ("configDir", "/w/config".to_string()),
        ("fixtures", "/r/fixtures/engines/x".to_string()),
    ]);
    for (name, want) in [
        ("cline", Some("CLINE_MODEL")),
        ("fake-acp", Some("FAKE_ACP_MODEL")),
        ("opencode", None),
    ] {
        let p = load_profile(&repo().join(format!("engine/profiles/{name}.toml"))).unwrap();
        let l = resolve_launch(&p, Transport::Acp, &vars).unwrap();
        assert_eq!(l.model_env.as_deref(), want, "{name}");
    }
}
