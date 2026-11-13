//! The container signals: the list that decides whether a repository keeps its session on the microVM, because its checks start
//! containers and the Instances machine cannot run them. Everything here is pure: the grammar of one signal, the default list, the setting text,
//! the match of a signal against the files of a repository and the two messages. The repo-gateway reads the repository and the stage's
//! `start-session` decides; both call this module.
#![allow(clippy::unwrap_used)]

use ham_core::container_signals::{
    Check, DEFAULT_SIGNALS, Found, auto_note, find_in, instances_refusal, not_checked_refusal,
    parse_setting, parse_signal, resolve, setting_text,
};
use std::convert::Infallible;

/// A repository held in memory: the path and the content of each file.
struct Repo(Vec<(&'static str, &'static str)>);

impl Repo {
    fn find(&self, list: Option<&[String]>) -> Option<Found> {
        let signals = resolve(list).unwrap();
        let paths: Vec<String> = self.0.iter().map(|(p, _)| (*p).to_string()).collect();
        let mut read = |batch: &[&str]| -> Result<Vec<Vec<u8>>, Infallible> {
            Ok(batch
                .iter()
                .map(|p| {
                    self.0
                        .iter()
                        .find(|(q, _)| q == p)
                        .map(|(_, c)| c.as_bytes().to_vec())
                        .unwrap()
                })
                .collect())
        };
        find_in(&signals, &paths, &mut read).unwrap()
    }
}

fn lines(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn b632_every_default_signal_is_a_valid_line_and_the_list_names_each_language_and_compose() {
    let all = resolve(None).unwrap();
    assert_eq!(all.len(), DEFAULT_SIGNALS.len());
    for line in DEFAULT_SIGNALS {
        let one = parse_signal(line).unwrap();
        assert_eq!(
            one.line(),
            *line,
            "a default line is already in normal form"
        );
    }
    let joined = DEFAULT_SIGNALS.join("\n");
    for needed in [
        "pom.xml",
        "*.gradle",
        "go.mod",
        "pyproject.toml",
        "requirements",
        "package.json",
        "*.csproj",
        "docker-compose.yml",
        "compose.yaml",
        ".ham/test-command contains docker",
    ] {
        assert!(joined.contains(needed), "the default list has {needed}");
    }
}

#[test]
fn b632_a_testcontainers_dependency_is_found_in_the_build_file_of_each_language() {
    let cases: [(&str, &str); 14] = [
        ("pom.xml", "<artifactId>testcontainers</artifactId>"),
        (
            "app/build.gradle",
            "testImplementation 'org.testcontainers:postgresql:1.19'",
        ),
        (
            "build.gradle.kts",
            "testImplementation(\"org.testcontainers:kafka\")",
        ),
        ("gradle/libs.versions.toml", "testcontainers = \"1.19.0\""),
        (
            "go.mod",
            "github.com/testcontainers/testcontainers-go v0.27.0",
        ),
        ("requirements-dev.txt", "testcontainers[postgres]==4.0"),
        ("pyproject.toml", "dependencies = [\"testcontainers\"]"),
        (
            "package.json",
            "{\"devDependencies\": {\"@testcontainers/postgresql\": \"10\"}}",
        ),
        (
            "src/Api.Tests/Api.Tests.csproj",
            "<PackageReference Include=\"Testcontainers.PostgreSql\" />",
        ),
        (
            "Directory.Packages.props",
            "<PackageVersion Include=\"Testcontainers\" Version=\"3\" />",
        ),
        ("crates/db/Cargo.toml", "testcontainers = \"0.15\""),
        ("Gemfile", "gem 'testcontainers'"),
        (
            "composer.json",
            "{\"require-dev\": {\"testcontainers/testcontainers\": \"1\"}}",
        ),
        (
            "build.sbt",
            "\"com.dimafeng\" %% \"testcontainers-scala\" % \"0.41\"",
        ),
    ];
    for (path, content) in cases {
        let repo = Repo(vec![("README.md", "hello"), (path, content)]);
        let found = repo
            .find(None)
            .unwrap_or_else(|| panic!("{path} is a signal"));
        assert_eq!(found.path, path, "{path}");
        assert!(
            found.signal.ends_with("contains testcontainers"),
            "{path}: {}",
            found.signal
        );
    }
}

#[test]
fn b632_a_compose_file_is_a_signal_by_its_name_alone() {
    for name in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "docker-compose.test.yml",
        "compose.yml",
        "compose.yaml",
        "deploy/compose.override.yaml",
    ] {
        let repo = Repo(vec![
            ("src/main.rs", "fn main() {}"),
            (name, "services: {}"),
        ]);
        let found = repo
            .find(None)
            .unwrap_or_else(|| panic!("{name} is a signal"));
        assert_eq!(found.path, name);
        assert!(!found.signal.contains("contains"), "{}", found.signal);
    }
}

#[test]
fn b632_the_project_test_command_counts_only_when_it_mentions_docker() {
    let with = Repo(vec![(".ham/test-command", "docker compose run tests\n")]);
    let found = with.find(None).unwrap();
    assert_eq!(found.path, ".ham/test-command");
    assert_eq!(found.signal, ".ham/test-command contains docker");
    let without = Repo(vec![(".ham/test-command", "cargo test --workspace\n")]);
    assert_eq!(without.find(None), None);
    // The file is read from the root of the repository only.
    let nested = Repo(vec![("sub/.ham/test-command", "docker run x")]);
    assert_eq!(nested.find(None), None);
}

#[test]
fn b632_a_repository_without_a_signal_finds_nothing() {
    let repo = Repo(vec![
        ("README.md", "We use docker and testcontainers in the docs"),
        ("package.json", "{\"dependencies\": {\"express\": \"4\"}}"),
        ("pom.xml", "<artifactId>junit</artifactId>"),
        ("docs/docker-compose.md", "not a compose file"),
        ("src/compose.rs", "fn main() {}"),
        (
            "go.sum",
            "github.com/testcontainers/testcontainers-go v0.1.0 h1:xx",
        ),
    ]);
    assert_eq!(
        repo.find(None),
        None,
        "documents, sources and lock files are no signal"
    );
}

#[test]
fn b632_content_is_compared_without_regard_to_case_and_a_name_without_a_slash_matches_at_any_depth()
{
    let repo = Repo(vec![(
        "services/billing/pom.xml",
        "<dependency>TestContainers-Core</dependency>",
    )]);
    assert_eq!(repo.find(None).unwrap().path, "services/billing/pom.xml");
    // A pattern with a slash starts at the root of the repository: `sub/.ham/test-command` is not `.ham/test-command`.
    let custom = lines(&["ci/run.sh contains docker"]);
    let at_root = Repo(vec![("ci/run.sh", "docker build .")]);
    assert!(at_root.find(Some(&custom)).is_some());
    let deeper = Repo(vec![("x/ci/run.sh", "docker build .")]);
    assert_eq!(deeper.find(Some(&custom)), None);
    // `**` crosses folders and `*` stays inside one.
    let star = lines(&["deploy/**/*.yml"]);
    assert!(
        Repo(vec![("deploy/a/b/c.yml", "")])
            .find(Some(&star))
            .is_some()
    );
    assert_eq!(
        Repo(vec![("deploy/a/b/c.yaml", "")]).find(Some(&star)),
        None
    );
    let one = lines(&["deploy/*.yml"]);
    assert_eq!(Repo(vec![("deploy/a/c.yml", "")]).find(Some(&one)), None);
    assert!(Repo(vec![("deploy/c.yml", "")]).find(Some(&one)).is_some());
}

#[test]
fn b632_the_first_signal_of_the_list_and_the_first_path_in_order_are_reported() {
    let repo = Repo(vec![
        ("z/compose.yaml", ""),
        ("a/package.json", "testcontainers"),
        ("a/compose.yml", ""),
    ]);
    // package.json is earlier in the default list than the compose names; within one signal the paths are taken in sorted order.
    let found = repo.find(None).unwrap();
    assert_eq!(found.path, "a/package.json");
    let compose_only = lines(&["compose.yml, compose.yaml"]);
    assert_eq!(
        repo.find(Some(&compose_only)).unwrap().path,
        "a/compose.yml"
    );
}

#[test]
fn b632_an_empty_list_finds_nothing_a_custom_list_replaces_the_default_and_default_brings_it_back()
{
    let repo = Repo(vec![
        ("docker-compose.yml", "services: {}"),
        ("Taskfile.yml", "docker: yes"),
    ]);
    assert!(repo.find(None).is_some());
    assert_eq!(
        repo.find(Some(&[])),
        None,
        "an empty list turns every signal off"
    );
    let custom = lines(&["Taskfile.yml contains docker"]);
    let found = repo.find(Some(&custom)).unwrap();
    assert_eq!(found.path, "Taskfile.yml");
    let both = lines(&["default", "Taskfile.yml contains docker"]);
    assert_eq!(
        resolve(Some(&both)).unwrap().len(),
        DEFAULT_SIGNALS.len() + 1
    );
    assert_eq!(repo.find(Some(&both)).unwrap().path, "docker-compose.yml");
    let only_custom = lines(&["Makefile contains docker"]);
    assert_eq!(
        repo.find(Some(&only_custom)),
        None,
        "the default signals are not in force when the list does not say default"
    );
}

#[test]
fn b632_the_grammar_normalizes_a_line_and_refuses_what_it_cannot_read() {
    let one = parse_signal("  ./pom.xml ,  build.gradle   contains   Testcontainers ").unwrap();
    assert_eq!(
        one.line(),
        "./pom.xml, build.gradle contains Testcontainers"
    );
    for bad in [
        "",
        "   ",
        "contains docker",
        "pom.xml contains",
        "pom.xml contains   ",
        "my file.txt",
        "a,,b",
        "default",
        "none",
        "pom.xml\u{7} contains x",
    ] {
        assert!(parse_signal(bad).is_err(), "{bad:?} is not a signal");
    }
    let msg = parse_signal("pom.xml contains").unwrap_err();
    assert!(msg.contains("pom.xml contains"), "{msg}");
}

#[test]
fn b632_the_setting_text_has_semicolons_the_word_none_and_the_word_default() {
    assert_eq!(parse_setting("none").unwrap(), Vec::<String>::new());
    assert_eq!(parse_setting(" NONE ").unwrap(), Vec::<String>::new());
    assert_eq!(parse_setting("default").unwrap(), vec!["default"]);
    assert_eq!(
        parse_setting("default; Gemfile contains testcontainers ;docker-compose.yml").unwrap(),
        vec![
            "default",
            "Gemfile contains testcontainers",
            "docker-compose.yml"
        ]
    );
    for bad in ["", " ; ", "none; pom.xml", "pom.xml contains", "a b"] {
        assert!(parse_setting(bad).is_err(), "{bad:?}");
    }
    let list = parse_setting("default; Gemfile contains testcontainers").unwrap();
    assert_eq!(
        setting_text(&list),
        "default; Gemfile contains testcontainers"
    );
    assert_eq!(setting_text(&[]), "none");
}

#[test]
fn b632_the_refusal_names_the_signal_the_path_and_both_ways_to_change_the_choice() {
    let found = Found {
        signal: "docker-compose.yml".into(),
        path: "deploy/docker-compose.yml".into(),
    };
    let msg = instances_refusal(&found);
    assert!(msg.contains("deploy/docker-compose.yml"), "{msg}");
    assert!(msg.contains("\"docker-compose.yml\""), "{msg}");
    assert!(msg.contains("--compute microvm"), "{msg}");
    assert!(msg.contains("auto"), "{msg}");
    assert!(msg.contains("run.container_signals"), "{msg}");
    assert!(msg.contains("none"), "{msg}");
    assert!(!msg.contains('\n'), "one line");
    let note = auto_note(&found);
    assert!(
        note.contains("microVM") && note.contains("deploy/docker-compose.yml"),
        "{note}"
    );
    let unchecked = not_checked_refusal();
    assert!(unchecked.contains("--compute microvm"), "{unchecked}");
}

#[test]
fn b632_the_check_has_the_wire_shape_the_gateway_writes_and_the_session_start_reads() {
    let none: Check = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(none, Check { found: None });
    assert_eq!(serde_json::to_value(&none).unwrap(), serde_json::json!({}));
    let hit = Check {
        found: Some(Found {
            signal: "go.mod contains testcontainers".into(),
            path: "go.mod".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&hit).unwrap(),
        serde_json::json!({"found": {"signal": "go.mod contains testcontainers", "path": "go.mod"}})
    );
}
