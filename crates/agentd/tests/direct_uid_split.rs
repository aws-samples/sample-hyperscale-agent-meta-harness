//! The uid split for a direct run, as the kernel enforces it: agentd
//! runs as root, gives the engine user its working directories, then makes the engine's configuration directory root's, with the
//! user's rules (0444 in 0555) and the launch settings (`locked/`, 0444 in 0555) inside. The engine user can read all of it and
//! write none of it, cannot add, remove or rename an entry of the configuration directory, and still writes its own HOME, data
//! and tmp. The uid drop needs root: CI runs as a normal user and lists this test as ignored; run it in a root container
//! (`crates/agentd/AGENTS.md`, Verification).
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::request::sha256_hex;
use agentd::direct::rules_place::place_rules;
use agentd::engine::direct::{lock_global_settings, protect_config, user_rules_dir};
use agentd::engine::{LaunchSpec, chown_tree, engine_identity, spawn};
use common::scratch;
use direct_support::{Direct, Opts};
use harness_protocol::api::v1::RuleFile;
use harness_protocol::runtime::TurnOutcome;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

fn euid() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).map(str::to_string))
        .unwrap()
}

/// `sh -c <script>` as `owner` through agentd's launch path: the exit code and what it printed.
async fn sh_as(owner: (u32, u32), script: &str, logs: &Path) -> (Option<i32>, String) {
    let spec = LaunchSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        env: BTreeMap::new(),
        cwd: PathBuf::from("/"),
        path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        stderr_log: logs.join("sh.err"),
        run_as: Some(owner),
        sandbox: None,
    };
    let mut child = spawn(&spec).unwrap();
    drop(child.stdin.take());
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    (child.wait().await.unwrap().code(), out)
}

#[tokio::test]
#[ignore = "needs root for a real uid split; run as root in a container (crates/agentd/AGENTS.md)"]
async fn the_engine_user_reads_its_configuration_and_changes_none_of_it() {
    assert_eq!(
        euid(),
        "0",
        "run this test as root: it drops to the engine user"
    );
    let owner = engine_identity(Some("nobody"))
        .await
        .expect("the engine user `nobody` resolves while running as root")
        .expect("running as root gives a uid split");
    assert_ne!(owner.0, 0);

    // The layout of start_and_prompt: agent data root's (0755), the four directories handed to the engine.
    let data = scratch("uid-split");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = data.join("r1/direct");
    let (d_data, d_config, d_home, d_tmp) = (
        agent.join("data"),
        agent.join("config"),
        agent.join("home"),
        agent.join("tmp"),
    );
    for d in [&d_data, &d_config, &d_home, &d_tmp] {
        std::fs::create_dir_all(d).unwrap();
    }
    for d in [data.join("r1"), agent.clone()] {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::create_dir_all(d_config.join("aws")).unwrap();
    std::fs::write(
        d_config.join("aws/config"),
        "[profile agent]\nregion = us-east-1\n",
    )
    .unwrap();
    std::fs::write(d_config.join("global-settings.json"), "{}").unwrap();
    chown_tree(&[&d_data, &d_config, &d_home, &d_tmp], owner).await;

    // Step 9, then the protection of the configuration directory.
    let body = "Always write the failing test first.\n";
    place_rules(
        &user_rules_dir(&d_config),
        &[RuleFile {
            path: "00-my-rules.md".into(),
            sha256: sha256_hex(body.as_bytes()),
            content: body.into(),
        }],
    )
    .unwrap();
    let mut env = BTreeMap::new();
    lock_global_settings("cline", &d_config, &["spawn_agent".to_string()], &mut env)
        .unwrap()
        .unwrap();
    assert!(protect_config("cline", &d_config, Some(owner)).await);
    let logs = scratch("uid-split-logs");

    // What the engine can read.
    for (script, want) in [
        ("cat config/rules/00-my-rules.md", body),
        (
            "grep -c spawn_agent config/locked/global-settings.json",
            "1\n",
        ),
        ("cat config/aws/config | head -1", "[profile agent]\n"),
    ] {
        let (code, out) = sh_as(owner, &format!("cd {} && {script}", agent.display()), &logs).await;
        assert_eq!((code, out.as_str()), (Some(0), want), "{script}");
    }

    // What it cannot change: every attempt exits non-zero and the bytes on disk are as agentd left them.
    for attempt in [
        "echo x >> config/rules/00-my-rules.md",
        "echo x > config/rules/00-my-rules.md",
        "mkdir config/rules/new-dir",
        "rm config/rules/00-my-rules.md",
        "mv config/rules config/rules.old",
        "echo '{\"disabledTools\":[]}' > config/locked/global-settings.json",
        "rm config/locked/global-settings.json",
        "mv config/locked config/locked.old",
        "mkdir config/anything",
        "echo '[profile agent]' > config/aws/config",
        "mkdir config/rules2",
        "chmod 0777 config/rules",
    ] {
        let (code, _) = sh_as(
            owner,
            &format!("cd {} && {attempt}", agent.display()),
            &logs,
        )
        .await;
        assert_ne!(code, Some(0), "the engine user managed: {attempt}");
    }
    assert_eq!(
        std::fs::read_to_string(d_config.join("rules/00-my-rules.md")).unwrap(),
        body
    );
    assert!(
        std::fs::read_to_string(d_config.join("locked/global-settings.json"))
            .unwrap()
            .contains("spawn_agent")
    );
    assert_eq!(
        std::fs::metadata(&d_config).unwrap().permissions().mode() & 0o777,
        0o755
    );
    use std::os::unix::fs::MetadataExt;
    for p in [
        &d_config,
        &d_config.join("rules"),
        &d_config.join("locked"),
        &d_config.join("aws/config"),
    ] {
        assert_eq!(
            std::fs::metadata(p).unwrap().uid(),
            0,
            "{} belongs to root",
            p.display()
        );
    }

    // What it can still do: write its own directories.
    for dir in ["home", "data", "tmp"] {
        let (code, _) = sh_as(
            owner,
            &format!(
                "cd {} && mkdir -p {dir}/sub && echo ok > {dir}/sub/f && cat {dir}/sub/f",
                agent.display()
            ),
            &logs,
        )
        .await;
        assert_eq!(code, Some(0), "{dir} stays the engine's");
    }
}

/// Review S4 (M28): the order of `start_and_prompt` itself, not a replay of it. A full direct run with the engine dropped to
/// `nobody`: the fake engine tries to rename, remove and add entries in its configuration directory and to write its own HOME,
/// and writes each exit code. Take the `protect_config` call out of `run_with`, or move it after the engine starts or before the
/// rules are placed, and an attempt succeeds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root for a real uid split; run as root in a container (crates/agentd/AGENTS.md)"]
async fn a_direct_run_gives_the_engine_a_configuration_directory_it_cannot_change() {
    assert_eq!(
        euid(),
        "0",
        "run this test as root: it drops to the engine user"
    );
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_CONFIG_WRITES", "1".into())],
        edit_cfg: Some(Box::new(|c| c.engine_user = Some("nobody".into()))),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l == "direct turn config directory protected: given_to_root=true owner_uid=0"),
        "{:#?}",
        d.log.lines()
    );

    // TRIGGER PRESENT: the engine ran as the engine user and made all six attempts.
    use std::os::unix::fs::MetadataExt;
    let home = d.agent_data().join("home");
    let own = std::fs::metadata(home.join("own-file")).expect("the engine wrote its own file");
    assert_ne!(own.uid(), 0, "the engine ran as root");
    let attempts = std::fs::read_to_string(home.join("config-writes.txt")).unwrap();
    let rcs: Vec<&str> = attempts.lines().collect();
    assert_eq!(rcs.len(), 6, "{attempts}");
    // What it cannot do: rename rules/, remove locked/, add to rules/, rewrite a rule, add to config/. What it can: its HOME.
    for line in &rcs[..5] {
        assert!(
            !line.starts_with("rc=0 "),
            "the engine user managed: {line}"
        );
    }
    assert!(rcs[5].starts_with("rc=0 "), "{}", rcs[5]);

    // The bytes on disk are as agentd left them, and all of it is root's.
    let config = d.agent_data().join("config");
    assert_eq!(
        std::fs::read_to_string(config.join("rules/00-my-rules.md")).unwrap(),
        "Always write the failing test first.\n"
    );
    for gone in ["rules.old", "rules/new-dir", "anything"] {
        assert!(!config.join(gone).exists(), "{gone}");
    }
    assert!(config.join("locked/global-settings.json").is_file());
    for p in [
        &config,
        &config.join("rules"),
        &config.join("rules/00-my-rules.md"),
        &config.join("locked"),
        &config.join("locked/global-settings.json"),
        &config.join("aws/config"),
    ] {
        assert_eq!(
            std::fs::metadata(p).unwrap().uid(),
            0,
            "{} belongs to root",
            p.display()
        );
    }
    assert_eq!(
        std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o755
    );
}
