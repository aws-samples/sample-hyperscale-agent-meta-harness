//! The direct driver end to end, in process: `POST /invocations` with `kind: "direct"`
//! against a fake AWS (S3 and the internal API) and the fake ACP engine. One test per way the one prompt can end, the setup
//! failures, the uploads, the server's refusals, the optional nudge and the package proxy seam. No AWS call is made.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::{Deps, ProxyLauncher, ProxyRun, ProxyStart};
use agentd::time::{format_rfc3339, now_unix};
use common::{CANARY_NONCE, CANARY_SECRET, CANARY_TOKEN, canary_key_id, git, scratch};
use direct_support::{BUCKET, Direct, MODEL, Opts, PROMPT, PutFailure, RUN, no_secrets, rule, sha};
use harness_protocol::api::internal::ProcessCredentials;
use harness_protocol::api::v1::ContinueSettings;
use harness_protocol::common::Usage;
use harness_protocol::direct::{DirectOutcome, DirectPackages, DirectSummary, s3 as keys};
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{PackagesToken, PingStatus, RuleDelivery, RuleLayer, TurnOutcome};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn bundle_uri() -> String {
    keys::uri(BUCKET, &keys::bundle_key(RUN))
}

fn nudge(text: &str, max: u32) -> Option<ContinueSettings> {
    Some(ContinueSettings {
        text: text.into(),
        sha256: sha(text.as_bytes()),
        max_nudges: max,
    })
}

// ---- the run that reports done ------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn one_prompt_to_the_engine_ends_cline_reported_done_with_everything_delivered() {
    let d = Direct::start(Opts::default()).await;
    let (ack, resp) = d.invoke().await;
    assert_eq!(
        (
            ack.run_id.as_str(),
            ack.agent_id.as_str(),
            ack.turn,
            ack.accepted
        ),
        (RUN, "direct", 1, true)
    );
    d.wait_healthy().await;

    // The result and the turn-complete.
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    let sha_head = resp.last_commit_sha.clone().unwrap();
    assert_eq!(sha_head.len(), 40);
    assert_eq!(resp.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    let c = &complete[0];
    assert_eq!(
        (c.run_id.as_str(), c.agent_id.as_str(), c.turn),
        (RUN, "direct", 1)
    );
    assert_eq!(c.finish_reason, "end_turn");
    assert_eq!(c.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    assert_eq!(
        c.snapshot_uri, None,
        "worker-api refuses a snapshot uri on the direct agent"
    );
    assert_eq!(c.last_commit_sha.as_deref(), Some(sha_head.as_str()));
    assert_eq!(c.outcome, Some(TurnOutcome::Completed));
    assert_eq!(
        c.summary.as_deref(),
        Some("Added percent_change with tests.")
    );
    assert_eq!(c.error, None);
    let manifest = c.rules_manifest.clone().unwrap();
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.entries[0].layer, RuleLayer::Tenant);
    assert_eq!(manifest.entries[0].delivery, RuleDelivery::Native);
    assert_eq!(manifest.entries[0].source, "00-my-rules.md");
    assert_eq!(
        c.rules_digest.as_deref(),
        Some(sha(&serde_json::to_vec(&manifest).unwrap()).as_str())
    );
    let usage = c.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (120, 45));

    // summary.json.
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.label, "Cline reported done");
    assert_eq!(s.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        s.cline_message.as_deref(),
        Some("Added percent_change with tests.")
    );
    assert_eq!(s.model.id, MODEL);
    assert_eq!(s.model.pinned.as_deref(), Some(MODEL));
    // The version is the one the engine profile declares (engine/profiles/cline.toml): the series is rebased on cli-v3.0.71, so
    // the 3.0.65 this row pinned on the branch's old base is no longer true on the merge head.
    let declared = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../engine/profiles/cline.toml"),
    )
    .unwrap()
    .lines()
    .find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"'))
    .expect("the cline profile declares its version")
    .to_string();
    assert_eq!(declared, "3.0.71");
    assert_eq!(
        (s.engine.name.as_str(), s.engine.version.as_deref()),
        ("cline", Some(declared.as_str()))
    );
    let wrapper = d.dir.join("engine-bin/fake-acp-engine");
    assert_eq!(
        s.engine.binary_sha256,
        Some(sha(&std::fs::read(wrapper).unwrap()))
    );
    assert_eq!(s.rules.len(), 1);
    assert_eq!(s.rules[0].name, "00-my-rules.md");
    assert!(s.rules_preflight.ran);
    assert_eq!(s.request.sha256, sha(&d.request_json));
    assert_eq!(s.request.prompt_bytes, PROMPT.len() as u64);
    assert!(!s.nudge.requested && !s.nudge.honored);
    assert_eq!(
        s.usage,
        Some(Usage {
            input_tokens: 120,
            output_tokens: 45,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0
        })
    );
    assert_eq!(s.tool_calls, 2);
    assert_eq!(s.git.head_sha.as_deref(), Some(sha_head.as_str()));
    assert!(
        s.git.committed_by_harness,
        "the fake engine left its edits uncommitted"
    );
    assert_eq!(s.git.files_changed, 2);
    assert_eq!(s.git.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    assert!(s.git.bundle_bytes.unwrap() > 0);
    assert_eq!(s.git.base_sha.as_deref().map(str::len), Some(40));
    assert!(!s.packages.enabled);
    assert_eq!(s.timing.started_at.len(), 20);

    // The bundle holds branch harness/<run>/direct with the harness commit.
    let bundle = d.bundle_file().expect("the bundle in the bucket");
    let wt = d.worktree();
    git(&wt, &["bundle", "verify", bundle.to_str().unwrap()]);
    let heads = git(&wt, &["bundle", "list-heads", bundle.to_str().unwrap()]);
    assert!(
        heads.contains(&format!("refs/heads/harness/{RUN}/direct")),
        "{heads}"
    );
    assert!(heads.contains(&sha_head), "{heads}");
    let body = git(&wt, &["log", "-1", "--format=%B", &sha_head]);
    for want in [
        "harness: turn 1",
        "Ham-Role: direct",
        "Ham-Agent: direct",
        "Ham-Turn: 1",
    ] {
        assert!(body.contains(want), "{want} not in {body}");
    }
    let files = git(&wt, &["ls-tree", "-r", "--name-only", &sha_head]);
    assert!(
        files.contains("src/calc.py") && files.contains("tests/test_calc.py"),
        "{files}"
    );
    assert!(
        files.contains(".cline/hooks/pre.sh"),
        "the repo's own hook is restored: {files}"
    );
    assert!(git(&wt, &["show", &format!("{sha_head}:src/calc.py")]).contains("def percent_change"));

    // Events: turn.started first with the rules digest, turn.ended last, the engine's calls between.
    let events = d.aws.events();
    assert_eq!(events.first().unwrap().0, EventType::TurnStarted);
    assert_eq!(events[0].1["rulesDigest"], c.rules_digest.clone().unwrap());
    assert_eq!(
        events[0].1,
        serde_json::json!({"turn": 1, "engine": "cline", "model": MODEL, "role": "direct", "rulesDigest": c.rules_digest.clone().unwrap()})
    );
    assert_eq!(d.events_of(EventType::TurnStarted).len(), 1);
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended.len(), 1);
    assert_eq!(events.last().unwrap().0, EventType::TurnEnded);
    assert_eq!(ended[0]["outcome"], "completed");
    assert_eq!(ended[0]["stopReason"], "end_turn");
    assert_eq!(ended[0]["lastCommitSha"], sha_head.as_str());
    assert_eq!(ended[0]["directOutcome"], "reported_done");
    assert_eq!(ended[0]["label"], "Cline reported done");
    assert_eq!(d.events_of(EventType::ToolSummary).len(), 2);
    assert!(
        d.events_of(EventType::TurnProgress)
            .iter()
            .any(|p| p["kind"] == "message")
    );
    let seqs: Vec<u64> = d.aws.envelopes().iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert!(
        d.aws
            .envelopes()
            .iter()
            .all(|e| e.agent_id.as_deref() == Some("direct"))
    );

    // Heartbeats name the direct agent and turn 1.
    let beats = d.aws.heartbeats();
    assert!(!beats.is_empty());
    assert!(
        beats
            .iter()
            .all(|b| b.run_id == RUN && b.agent_id == "direct" && b.turn == 1)
    );

    // result.json is on disk and no secret is anywhere a person reads.
    for line in d.log.lines() {
        no_secrets(&line);
    }
    assert!(d.log.lines().iter().any(|l| l.starts_with(&format!(
        "direct turn started: run={RUN} agent=direct engine=cline"
    ))));
    assert!(d.log.lines().iter().any(|l| l.starts_with(&format!(
        "direct turn ended: run={RUN} outcome=reported_done"
    ))));
    assert!(
        !d.app.cfg.runtime_dir.join("creds.sock").exists(),
        "the credential socket is torn down"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_gets_the_prompt_unchanged_in_one_session_at_the_repository_root() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    // Exactly one prompt, the user's bytes: no prefix, no trim, no wrapper.
    assert_eq!(d.prompts_received(), vec![PROMPT.to_string()]);
    let log = d.fake_engine_log();
    let new: serde_json::Value = log
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["method"] == "session/new")
        .unwrap();
    // No `_meta`: the engine's system prompt stands. The cwd is the git top level of the worktree.
    assert!(new["params"].get("_meta").is_none(), "{new}");
    assert_eq!(new["params"]["mcpServers"], serde_json::json!([]));
    let top = git(&d.worktree(), &["rev-parse", "--show-toplevel"]);
    assert_eq!(new["params"]["cwd"], top.as_str());
    let probe = d.engine_probe();
    assert_eq!(probe.lines().next().unwrap(), top);
    // No model was set by a config option: the pin came from the launch environment.
    assert!(!log.contains("session/set_config_option"));
}

/// Maven (and the Maven the wrapper downloads) reads `${user.home}/.m2/settings.xml`, and Java takes `user.home` from the password database
/// (`/home/agent`), not from `$HOME`, which agentd points at the run's data directory. The mirror file agentd writes there was ignored in the
/// first live Maven run of stage direct (2026-10-14): Cline found that builds reached for Maven Central and timed out until it set
/// `MAVEN_OPTS=-Duser.home=$HOME`. `MAVEN_ARGS` (Maven 3.9) names the file explicitly.
#[tokio::test(flavor = "multi_thread")]
async fn maven_is_told_where_the_mirror_file_is_because_its_user_home_is_not_home() {
    let d = Direct::start(Opts {
        packages_token: true,
        edit_cfg: Some(Box::new(with_packages)),
        ..Opts::default()
    })
    .await;
    let seen = Arc::new(Seen::default());
    let deps = Deps {
        proxy: Arc::new(FakeLauncher(seen.clone())),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let probe = d.engine_probe();
    let env: BTreeMap<&str, &str> = probe
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .filter_map(|l| l.split_once('='))
        .collect();
    let settings = d.agent_data().join("home/.m2/settings.xml");
    assert!(settings.is_file());
    assert_eq!(
        env["MAVEN_ARGS"],
        format!("--settings {}", settings.display())
    );
}

/// The environment the fake engine printed (`--- env` section of its probe).
fn probe_env(d: &Direct) -> BTreeMap<String, String> {
    d.engine_probe()
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The engine's cargo resolves crates.io dependencies through the loopback package proxy. agentd writes the
/// proxy's `config.toml` into a cargo home of the engine's own home directory, names that directory in `CARGO_HOME`, and sets the build
/// switches for a session of 2 vCPU. The proxy holds the token; nothing here does.
#[tokio::test(flavor = "multi_thread")]
async fn cargo_is_given_its_home_the_registry_config_and_the_build_switches() {
    let d = Direct::start(Opts {
        packages_token: true,
        edit_cfg: Some(Box::new(with_packages)),
        ..Opts::default()
    })
    .await;
    let seen = Arc::new(Seen::default());
    let deps = Deps {
        proxy: Arc::new(FakeLauncher(seen.clone())),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let env = probe_env(&d);
    let cargo_home = d.agent_data().join("home/.cargo");
    assert_eq!(env["CARGO_HOME"], cargo_home.display().to_string());
    assert_eq!(
        std::fs::read_to_string(cargo_home.join("config.toml")).unwrap(),
        FAKE_CARGO_TOML,
        "the proxy's file, as it is"
    );
    for (k, v) in [
        ("CARGO_BUILD_JOBS", "2"),
        ("CARGO_PROFILE_DEV_DEBUG", "0"),
        ("CARGO_PROFILE_TEST_DEBUG", "0"),
        ("CARGO_INCREMENTAL", "0"),
        ("RUST_BACKTRACE", "1"),
        ("CARGO_NET_GIT_FETCH_WITH_CLI", "false"),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
    // The package token is in no cargo variable and no cargo file.
    let token = d.req.packages_token.as_ref().unwrap().expose().to_string();
    for (k, v) in &env {
        if k.starts_with("CARGO") {
            assert!(!v.contains(&token), "{k}");
            assert!(!k.contains("TOKEN") && !k.contains("CREDENTIAL"), "{k}");
        }
    }
    assert!(
        !std::fs::read_to_string(cargo_home.join("config.toml"))
            .unwrap()
            .contains(&token)
    );
}

/// The home of a run that is invoked again can hold what the engine left. A link at `.cargo` is not written through: the run goes on
/// without a cargo config, says so in the log, and the file the link points at is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_cargo_directory_the_engine_left_as_a_link_is_not_written_through() {
    let d = Direct::start(Opts {
        packages_token: true,
        edit_cfg: Some(Box::new(with_packages)),
        ..Opts::default()
    })
    .await;
    let outside = scratch("cargo-planted-outside");
    std::fs::write(outside.join("config.toml"), "keep").unwrap();
    let home = d.agent_data().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::os::unix::fs::symlink(&outside, home.join(".cargo")).unwrap();
    let seen = Arc::new(Seen::default());
    let deps = Deps {
        proxy: Arc::new(FakeLauncher(seen.clone())),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        std::fs::read_to_string(outside.join("config.toml")).unwrap(),
        "keep"
    );
    assert!(!probe_env(&d).contains_key("CARGO_HOME"));
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn cargo config not placed:")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_proxy_cargo_gets_the_build_switches_and_no_registry() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let env = probe_env(&d);
    assert_eq!(env.get("CARGO_BUILD_JOBS").map(String::as_str), Some("2"));
    assert_eq!(env.get("CARGO_INCREMENTAL").map(String::as_str), Some("0"));
    assert!(
        !env.contains_key("CARGO_HOME"),
        "no proxy, no config to name: cargo keeps its default home"
    );
    assert!(!d.agent_data().join("home/.cargo/config.toml").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_deployments_job_count_reaches_cargo() {
    let d = Direct::start(Opts {
        edit_cfg: Some(Box::new(|c| c.direct.cargo_jobs = 4)),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(
        probe_env(&d).get("CARGO_BUILD_JOBS").map(String::as_str),
        Some("4")
    );
}

/// The checkout is on the session's 1 GiB managed storage, which reports no use, so a cargo `target/` there filled it. The engine's
/// cargo keeps its target folder on the root volume (a folder beside the run's other agentd folders), with or without a package proxy.
#[tokio::test(flavor = "multi_thread")]
async fn cargos_target_folder_is_on_the_root_volume_outside_the_checkout() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let env = probe_env(&d);
    let want = d.agent_data().join("cargo-target");
    assert_eq!(
        env.get("CARGO_TARGET_DIR").map(String::as_str),
        Some(want.display().to_string().as_str())
    );
    assert!(want.is_dir(), "agentd made the folder");
    let workspace = &d.app.cfg.workspace_root;
    assert!(
        !want.starts_with(workspace),
        "{} is under the checkout mount {}",
        want.display(),
        workspace.display()
    );
    assert!(
        want.starts_with(&d.app.cfg.data_root),
        "{} is not under the data root {}",
        want.display(),
        d.app.cfg.data_root.display()
    );
    assert!(
        !std::fs::symlink_metadata(&want)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_target_setting_checkout_leaves_cargo_its_own_target_folder() {
    let d = Direct::start(Opts {
        edit_cfg: Some(Box::new(|c| {
            c.direct.cargo_target = agentd::direct::cargo::TargetSetting::Checkout;
        })),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert!(!probe_env(&d).contains_key("CARGO_TARGET_DIR"));
    assert!(!d.agent_data().join("cargo-target").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_target_setting_can_name_another_absolute_folder() {
    let other = scratch("cargo-target-named").join("folder");
    let named = other.clone();
    let d = Direct::start(Opts {
        edit_cfg: Some(Box::new(move |c| {
            c.direct.cargo_target = agentd::direct::cargo::TargetSetting::Dir(named);
        })),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(
        probe_env(&d).get("CARGO_TARGET_DIR").map(String::as_str),
        Some(other.display().to_string().as_str())
    );
    assert!(other.is_dir());
    assert!(!d.agent_data().join("cargo-target").exists());
}

/// Whatever sits at the folder's name when the run is invoked again, a link is never written through: the run goes on without the
/// variable, says so in the log, and what the link points at is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_at_the_target_folders_name_is_not_written_through() {
    let d = Direct::start(Opts::default()).await;
    let victim = scratch("cargo-target-victim");
    std::fs::create_dir_all(d.agent_data()).unwrap();
    std::os::unix::fs::symlink(&victim, d.agent_data().join("cargo-target")).unwrap();
    d.invoke().await;
    assert!(!probe_env(&d).contains_key("CARGO_TARGET_DIR"));
    assert_eq!(std::fs::read_dir(&victim).unwrap().count(), 0);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn cargo target folder not placed")),
        "{:?}",
        d.log.lines()
    );
}

// ---- a full session disk -----------------------------------------------------------------------------------------------

/// The engine answers error -32603 and the root volume has less free space than the deployment's low mark: the run ends as the harness's own
/// stop, and its label, detail and events name the disk. (The low mark is set far above any real disk so the test needs no full disk.)
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_error_with_the_root_volume_short_ends_the_run_naming_the_disk() {
    let d = Direct::start(Opts {
        scenario: "rpc_error",
        edit_cfg: Some(Box::new(|c| c.direct.disk_low_mb = 100_000_000)),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{s:?}");
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    assert!(
        s.label.starts_with(
            "Cline stopped: the harness ended the run (the session disk is full, workspace "
        ),
        "{}",
        s.label
    );
    let detail = s.detail.as_deref().unwrap();
    assert!(
        detail.starts_with("The harness ended the run: the session disk is full"),
        "{detail}"
    );
    assert!(detail.contains("MiB free"), "{detail}");
    assert!(s.fault_hint.is_none(), "{s:?}");
    // The cause is in the events once, and no retry or switch was announced.
    let progress = d.events_of(EventType::TurnProgress);
    let named: Vec<&serde_json::Value> = progress
        .iter()
        .filter(|v| v["kind"] == "notify" && v["reason"] == "disk_full")
        .collect();
    assert_eq!(named.len(), 1, "{progress:?}");
    assert!(
        !progress
            .iter()
            .any(|v| v["kind"] == "provider-retry" || v["kind"] == "provider-switch")
    );
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended[0]["directOutcome"], "stopped_other");
    assert!(
        ended[0]["label"]
            .as_str()
            .unwrap()
            .contains("the session disk is full")
    );
    d.wait_healthy().await;
}

/// The checkout mount is measured by walking it: 2 MiB of ballast under the workspace root and a limit of 1 MiB.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_error_with_the_checkout_mount_over_its_limit_ends_the_run_naming_the_disk() {
    let d = Direct::start(Opts {
        scenario: "rpc_error",
        edit_cfg: Some(Box::new(|c| c.direct.workspace_limit_mb = 1)),
        ..Opts::default()
    })
    .await;
    let ws = d.app.cfg.workspace_root.clone();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("ballast.bin"), vec![1u8; 2 << 20]).unwrap();
    d.invoke().await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{s:?}");
    let detail = s.detail.as_deref().unwrap();
    assert!(
        detail.contains("the session disk is full, workspace "),
        "{detail}"
    );
    assert!(detail.contains("against a stop mark of 1 MiB"), "{detail}");
}

/// A deployment switches both checks off with 0: the same error is the provider's, as before.
#[tokio::test(flavor = "multi_thread")]
async fn with_both_checks_switched_off_the_same_error_stays_a_provider_error() {
    let d = Direct::start(Opts {
        scenario: "rpc_error",
        edit_cfg: Some(Box::new(|c| {
            c.direct.disk_low_mb = 0;
            c.direct.workspace_limit_mb = 0;
        })),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedProviderError, "{s:?}");
    assert_eq!(s.label, DirectOutcome::StoppedProviderError.label());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_proxy_maven_is_given_no_settings_argument() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let probe = d.engine_probe();
    assert!(!probe.lines().any(|l| l.starts_with("MAVEN_ARGS=")));
}

/// mypy 2.x keeps its incremental cache in SQLite databases under `./.mypy_cache`, which in a session is the repository on the session
/// storage (/mnt/workspace); it crashed there in the first live runs of stage direct (2026-10-14) and ran once its cache moved to /tmp.
#[tokio::test(flavor = "multi_thread")]
async fn mypys_cache_lives_on_the_engines_local_temp_directory_not_in_the_repository() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let probe = d.engine_probe();
    let env: BTreeMap<&str, &str> = probe
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .filter_map(|l| l.split_once('='))
        .collect();
    let tmp = env["TMPDIR"];
    assert_eq!(env["MYPY_CACHE_DIR"], format!("{tmp}/mypy-cache"));
    assert!(tmp.starts_with(d.agent_data().to_str().unwrap()), "{tmp}");
    assert!(!env["MYPY_CACHE_DIR"].contains("mnt-workspace"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_runs_with_the_launch_environment_the_contract_names() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let probe = d.engine_probe();
    let env: BTreeMap<&str, &str> = probe
        .lines()
        .skip_while(|l| *l != "--- env")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .filter_map(|l| l.split_once('='))
        .collect();
    let config = d.agent_data().join("config");
    for (k, v) in [
        ("CLINE_PROVIDER", "bedrock"),
        ("CLINE_NO_AUTO_UPDATE", "1"),
        ("CLINE_SESSION_BACKEND_MODE", "local"),
        ("CLINE_MODEL", MODEL),
        ("AWS_PROFILE", "agent"),
        ("AWS_REGION", "us-east-1"),
        ("AWS_DEFAULT_REGION", "us-east-1"),
        ("AWS_EC2_METADATA_DISABLED", "true"),
        ("AWS_SDK_LOAD_CONFIG", "1"),
        ("PYTHONDONTWRITEBYTECODE", "1"),
        ("USER", "agent"),
    ] {
        assert_eq!(env.get(k), Some(&v), "{k}");
    }
    assert_eq!(env["HOME"], d.agent_data().join("home").to_str().unwrap());
    assert_eq!(
        env["CLINE_GLOBAL_SETTINGS_PATH"],
        config.join("locked/global-settings.json").to_str().unwrap()
    );
    // The offline switches of the old line are not set: the engine installs what its tests need.
    for k in [
        "UV_OFFLINE",
        "PIP_NO_INDEX",
        "npm_config_offline",
        "pnpm_config_offline",
        "GOPROXY",
        "npm_config_fetch_retries",
    ] {
        assert!(!env.contains_key(k), "{k} is set");
    }
    for k in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "CLINE_API_KEY",
    ] {
        assert!(!env.contains_key(k), "{k} is set");
    }
    no_secrets(&probe);
    // The configuration the engine reads: the provider file has the window and the output limit and no compaction key.
    let providers: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(d.agent_data().join("data/settings/providers.json")).unwrap(),
    )
    .unwrap();
    let settings = &providers["providers"]["bedrock"]["settings"];
    assert_eq!(settings["model"], MODEL);
    assert_eq!(settings["contextWindow"], 1_000_000);
    assert_eq!(settings["maxTokens"], 128_000);
    assert!(settings.get("compaction").is_none());
    assert_eq!(settings["aws"]["region"], "us-east-1");
    assert_eq!(settings["aws"]["authentication"], "iam");
    // Sub-agents stay on (an empty kill-switch list), telemetry stays off.
    let global = std::fs::read_to_string(config.join("locked/global-settings.json")).unwrap();
    let global: serde_json::Value = serde_json::from_str(&global).unwrap();
    assert_eq!(global["telemetryOptOut"], true);
    assert_eq!(global["disabledTools"], serde_json::json!([]));
    assert!(!config.join("global-settings.json").exists());
    let aws = std::fs::read_to_string(config.join("aws/config")).unwrap();
    assert!(
        aws.contains("credential_process") && aws.contains("creds --socket"),
        "{aws}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rules_are_read_only_files_in_a_read_only_directory_before_the_engine_starts() {
    let body = "Always write the failing test first.\r\n\u{e9}\n";
    let d = Direct::start(Opts {
        rules: vec![rule("00-my-rules.md", body), rule("10-more.md", "x\n")],
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let probe = d.engine_probe();
    let rules: Vec<&str> = probe
        .lines()
        .skip_while(|l| *l != "--- rules")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .collect();
    assert!(
        rules
            .iter()
            .any(|l| l.starts_with("-r--r--r--") && l.ends_with("00-my-rules.md")),
        "{rules:?}"
    );
    assert!(
        rules
            .iter()
            .any(|l| l.starts_with("-r--r--r--") && l.ends_with("10-more.md")),
        "{rules:?}"
    );
    let dirline = probe
        .lines()
        .skip_while(|l| *l != "--- rules dir")
        .nth(1)
        .unwrap();
    assert!(dirline.starts_with("dr-xr-xr-x"), "{dirline}");
    assert_eq!(
        std::fs::read(d.agent_data().join("config/rules/00-my-rules.md")).unwrap(),
        body.as_bytes()
    );
    // The repository's hooks were set aside while the engine ran and are back in the commit.
    let cline_dir: Vec<&str> = probe
        .lines()
        .skip_while(|l| *l != "--- cline dir")
        .skip(1)
        .collect();
    assert!(
        !cline_dir.iter().any(|l| l.contains("hooks")),
        "{cline_dir:?}"
    );
    let s = d.summary();
    assert_eq!(s.rules.len(), 2);
    assert_eq!(s.rules[0].bytes, Some(body.len() as u64));
    assert!(s.rules_preflight.ran && s.rules_preflight.ms.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_no_rules_skips_the_preflight_and_sends_no_digest() {
    let d = Direct::start(Opts {
        rules: vec![],
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let s = d.summary();
    assert!(!s.rules_preflight.ran);
    assert!(s.rules.is_empty());
    let started = d.events_of(EventType::TurnStarted);
    assert!(started[0].get("rulesDigest").is_none());
    let c = &d.aws.complete()[0];
    assert!(c.rules_digest.is_none() && c.rules_manifest.is_none());
    assert!(!d.agent_data().join("config/rules").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_kill_switch_setting_lists_the_tools_switched_off() {
    let d = Direct::start(Opts {
        edit_cfg: Some(Box::new(|c| {
            c.direct.disabled_tools = vec!["spawn_agent".into()]
        })),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let global =
        std::fs::read_to_string(d.agent_data().join("config/locked/global-settings.json")).unwrap();
    let global: serde_json::Value = serde_json::from_str(&global).unwrap();
    assert_eq!(global["disabledTools"], serde_json::json!(["spawn_agent"]));
    assert!(d.engine_probe().contains("\"disabledTools\""));
}

// ---- every other end of the one prompt ----------------------------------------------------------------------------------------

struct Row {
    scenario: &'static str,
    env: Vec<(&'static str, String)>,
    outcome: DirectOutcome,
    turn: TurnOutcome,
    finish: &'static str,
}

async fn run_row(
    row: Row,
    tweak: impl FnOnce(&mut Opts),
) -> (Direct, harness_protocol::runtime::InvocationResponse) {
    let mut opts = Opts {
        scenario: row.scenario,
        engine_env: row.env.clone(),
        ..Opts::default()
    };
    tweak(&mut opts);
    let d = Direct::start(opts).await;
    let (_, resp) = d.invoke().await;
    let s = d.summary();
    assert_eq!(s.outcome, row.outcome, "{s:?}");
    assert_eq!(s.label, row.outcome.label());
    assert_eq!(s.turn_outcome, row.turn);
    assert_eq!(resp.outcome, row.turn);
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["directOutcome"], row.outcome.as_str());
    assert_eq!(ended[0]["label"], row.outcome.label());
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].finish_reason, row.finish, "{:?}", complete[0]);
    assert_eq!(complete[0].outcome, Some(row.turn));
    // Whatever the end, the work in the tree is committed, bundled and delivered.
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some(), "bundle");
    assert_eq!(
        complete[0].bundle_uri.as_deref(),
        Some(bundle_uri().as_str())
    );
    // A stop other than reported-done carries its specifics (when it has any) as the turn's error; reported-done carries none.
    assert_eq!(
        complete[0].error,
        if row.outcome == DirectOutcome::ReportedDone {
            None
        } else {
            s.detail.clone()
        }
    );
    d.wait_healthy().await;
    for line in d.log.lines() {
        no_secrets(&line);
    }
    (d, resp)
}

#[tokio::test(flavor = "multi_thread")]
async fn row_3_a_turn_ended_without_a_message_is_stopped_no_message() {
    run_row(
        Row {
            scenario: "end_turn_empty",
            env: vec![],
            outcome: DirectOutcome::StoppedNoMessage,
            turn: TurnOutcome::Completed,
            finish: "end_turn",
        },
        |_| {},
    )
    .await;
}

// ---- row 16: the engine ended its turn with a message and the tree equals the base --------------------------------------------

const NO_CHANGES_LABEL: &str = "Cline stopped: it ended its turn without changing a file";

/// The A6 defect: Cline read the repository, wrote that the task could not be done,
/// changed no file and ended its turn with `end_turn`. The harness called that "Cline reported done". It still delivers the (unchanged)
/// tree, and it now names the stop by what it measured: no file differs between the base and the delivered head.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_ended_its_turn_with_a_message_and_changed_no_file_is_not_cline_reported_done() {
    let d = Direct::start(Opts {
        scenario: "gave_up_no_change",
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    let s = d.summary();

    // The facts: the engine's own stop reason, its message, and a tree that equals the base.
    assert_eq!(s.stop_reason.as_deref(), Some("end_turn"));
    assert!(
        s.cline_message
            .as_deref()
            .is_some_and(|m| m.contains("cannot be done")),
        "{s:?}"
    );
    assert_eq!(s.git.files_changed, 0);
    assert!(s.git.head_sha.is_some());
    assert_eq!(s.git.head_sha, s.git.base_sha);
    assert!(!s.git.committed_by_harness);

    // The words: the frozen label of the new row, never the one that says Cline reported done.
    assert_eq!(
        serde_json::to_value(s.outcome).unwrap(),
        "stopped_no_changes",
        "{s:?}"
    );
    assert_eq!(s.label, NO_CHANGES_LABEL);
    assert_ne!(s.label, "Cline reported done");
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["directOutcome"], "stopped_no_changes");
    assert_eq!(ended[0]["label"], NO_CHANGES_LABEL);
    assert!(d.log.lines().iter().any(|l| l.starts_with(&format!(
        "direct turn ended: run={RUN} outcome=stopped_no_changes"
    ))));

    // The engine finished its prompt normally, so telemetry and the turn's error do not change; the tree is still delivered.
    assert_eq!(s.turn_outcome, TurnOutcome::Completed);
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    assert_eq!(s.detail, None);
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].error, None);
    assert_eq!(complete[0].finish_reason, "end_turn");
    assert_eq!(
        complete[0].bundle_uri.as_deref(),
        Some(bundle_uri().as_str())
    );
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some(), "bundle");
}

/// The rule counts files; it does not read the message. A reply that says "nothing needed changing" leaves the same tree and gets the
/// same words, so the harness never has to tell a give-up from a finished task by its prose.
#[tokio::test(flavor = "multi_thread")]
async fn the_rule_counts_changed_files_and_does_not_read_the_message() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    d.wait_healthy().await;
    let s = d.summary();
    assert_eq!(
        s.cline_message.as_deref(),
        Some("Nothing needed changing; the function already exists.")
    );
    assert_eq!(s.git.files_changed, 0);
    assert_eq!(
        serde_json::to_value(s.outcome).unwrap(),
        "stopped_no_changes"
    );
    assert_eq!(s.label, NO_CHANGES_LABEL);
}

#[tokio::test(flavor = "multi_thread")]
async fn row_4_max_tokens() {
    run_row(
        Row {
            scenario: "max_tokens",
            env: vec![],
            outcome: DirectOutcome::StoppedMaxTokens,
            turn: TurnOutcome::Completed,
            finish: "max_tokens",
        },
        |_| {},
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn row_5_max_turn_requests() {
    run_row(
        Row {
            scenario: "max_turn_requests",
            env: vec![],
            outcome: DirectOutcome::StoppedMaxRequests,
            turn: TurnOutcome::Completed,
            finish: "max_turn_requests",
        },
        |_| {},
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn row_6_refusal() {
    run_row(
        Row {
            scenario: "refusal",
            env: vec![],
            outcome: DirectOutcome::StoppedRefusal,
            turn: TurnOutcome::Completed,
            finish: "refusal",
        },
        |_| {},
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn row_14_an_unknown_stop_reason() {
    let (d, _) = run_row(
        Row {
            scenario: "weird_stop",
            env: vec![],
            outcome: DirectOutcome::StoppedOther,
            turn: TurnOutcome::Completed,
            finish: "brand_new_reason",
        },
        |_| {},
    )
    .await;
    assert!(d.summary().detail.unwrap().contains("brand_new_reason"));
}

#[tokio::test(flavor = "multi_thread")]
async fn row_8_the_engine_cancelling_itself_is_not_read_as_a_user_stop() {
    let (d, resp) = run_row(
        Row {
            scenario: "cancelled_unsolicited",
            env: vec![],
            outcome: DirectOutcome::StoppedEngineCancelled,
            turn: TurnOutcome::Failed,
            finish: "cancelled",
        },
        |_| {},
    )
    .await;
    // Two edits completed and then the engine stopped itself: nothing at the tail shows the loop guard or the mistake limit, and a cut
    // stream never ends `cancelled`, so the hint claims no cut stream and no transport fault.
    let hint = resp.fault_hint.unwrap();
    assert_eq!(hint.code.as_deref(), Some("unexplained"));
    assert_eq!(hint.transport, None);
    // The edits it made before stopping are still delivered.
    assert!(d.summary().git.committed_by_harness);
}

#[tokio::test(flavor = "multi_thread")]
async fn row_10_a_json_rpc_error_answering_the_prompt_is_the_provider_failing() {
    let (d, resp) = run_row(
        Row {
            scenario: "rpc_error",
            env: vec![],
            outcome: DirectOutcome::StoppedProviderError,
            turn: TurnOutcome::Failed,
            finish: "failed",
        },
        |_| {},
    )
    .await;
    let s = d.summary();
    assert!(s.detail.as_deref().unwrap().contains("-32603"), "{s:?}");
    let hint = resp.fault_hint.unwrap();
    assert_eq!((hint.http_status, hint.retry_after_s), (Some(429), Some(7)));
    assert_eq!(s.fault_hint, Some(hint));
    assert_eq!(s.stop_reason, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn row_11_an_engine_that_dies_during_the_prompt() {
    let (d, _) = run_row(
        Row {
            scenario: "writes_files",
            env: vec![("FAKE_ACP_DIE_ON_PROMPT", "3".into())],
            outcome: DirectOutcome::StoppedEngineDied,
            turn: TurnOutcome::Failed,
            finish: "failed",
        },
        |_| {},
    )
    .await;
    let s = d.summary();
    assert_eq!(s.stop_reason, None);
    assert!(
        s.git.committed_by_harness,
        "the files it wrote before dying are delivered"
    );
}

/// How long the engine stand-in stays silent in row 12. The test does not time the stop with a stopwatch: its clock ran across
/// `Direct::start`, whose `fake_engine_dir` runs `cargo build -p ham-e2e --bin fake-acp-engine` in every test process. The stop itself takes
/// about 7 s, but the test took 40 s in a coverage shard that built the engine first
/// and 125 to 146 s on the first run in a fresh target dir. The stand-in sleeps for an hour, so only agentd's silence
/// stop can end the run, and the stand-in's own log shows that the engine was killed asleep.
const ROW_12_SILENT_S: u64 = 3600;

#[tokio::test(flavor = "multi_thread")]
async fn row_12_silence_past_the_limit_stops_the_run_and_the_engine() {
    let run = run_row(
        Row {
            scenario: "writes_files",
            env: vec![("FAKE_ACP_SILENCE_S", ROW_12_SILENT_S.to_string())],
            outcome: DirectOutcome::StoppedSilence,
            turn: TurnOutcome::Failed,
            finish: "failed",
        },
        |o| o.provider_silence_s = Some(1),
    );
    // The timeout turns a stop that never comes into a failure instead of an hour-long hang; it asserts nothing about speed.
    let (d, resp) = tokio::time::timeout(Duration::from_secs(900), run)
        .await
        .expect("agentd's silence stop ended the run long before the engine's own silence did");
    assert_eq!(resp.fault_hint.unwrap().code.as_deref(), Some("stall"));
    assert!(d.summary().detail.unwrap().contains("no engine output"));
    // The engine never woke: its log ends at the start of its silence, because agentd killed it asleep.
    let log = d.fake_engine_log();
    let asleep = format!("silent for {ROW_12_SILENT_S} s");
    assert_eq!(
        log.lines().last(),
        Some(asleep.as_str()),
        "the engine was stopped while it was still silent: {log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn row_13_the_deadline_stops_the_run() {
    let (d, _) = run_row(
        Row {
            scenario: "end_turn_done",
            env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
            outcome: DirectOutcome::StoppedTimeout,
            turn: TurnOutcome::TimedOut,
            finish: "timed_out",
        },
        |o| o.timeout_s = 15,
    )
    .await;
    assert!(d.fake_engine_log().contains("notification session/cancel"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_7_and_9_a_cancel_from_the_user_stops_cline_and_keeps_the_work() {
    let mut opts = Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        ..Opts::default()
    };
    opts.timeout_s = 60;
    let d = Direct::start(opts).await;
    d.aws.cancel_on(EventType::TurnStarted);
    let t0 = Instant::now();
    let (_, resp) = d.invoke().await;
    assert!(t0.elapsed() < Duration::from_secs(30));
    assert_eq!(resp.outcome, TurnOutcome::Cancelled);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedByUser);
    assert_eq!(s.label, "Cline stopped: you stopped the run");
    assert_eq!(s.stop_reason.as_deref(), Some("cancelled"));
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
    assert!(d.fake_engine_log().contains("notification session/cancel"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_the_engine_echoes_reaches_no_summary_event_or_turn_complete() {
    let d = Direct::start(Opts {
        scenario: "leaky",
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let s = d.summary();
    assert!(
        s.cline_message.as_deref().unwrap().contains("<redacted>"),
        "{:?}",
        s.cline_message
    );
    assert!(s.cline_message.as_deref().unwrap().contains("the key is"));
    let everything = format!(
        "{}\n{}\n{}\n{}",
        serde_json::to_string(&d.aws.events()).unwrap(),
        serde_json::to_string(&d.aws.complete()).unwrap(),
        serde_json::to_string(&resp).unwrap(),
        String::from_utf8_lossy(&d.aws.get(&keys::summary_key(RUN)).unwrap()),
    );
    no_secrets(&everything);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_minted_by_a_refresh_during_the_run_is_redacted_like_the_first_ones() {
    // The invocation's credentials expire inside `REFRESH_WINDOW_S`, so agentd's first use of them (before the engine starts) refreshes
    // them through `/internal/creds`. Of the three values the engine then prints (`leaky_error`: in its message and in the provider error
    // that ends the prompt), the secret key and the nonce were minted by that refresh and never stood in the invocation, and the session
    // token is the invocation's, which the refresh rotated out. All three are secrets of this run: none may leave in the final message, an
    // event, the detail, the error or the stop digest.
    let mut d = Direct::start(Opts {
        scenario: "leaky_error",
        ..Opts::default()
    })
    .await;
    let now = now_unix();
    // Built at run time: no secret-shaped literal sits in the source.
    let original_secret = ["original", "secret", "agentd-must-never-print"].join("-");
    let original_nonce = ["original", "nonce", "agentd-must-never-print"].join("-");
    let refreshed_token = ["refreshed", "token", "agentd-must-never-print"].join("-");
    d.req.credentials = ProcessCredentials {
        version: 1,
        access_key_id: canary_key_id(),
        secret_access_key: original_secret.clone(),
        session_token: CANARY_TOKEN.into(),
        expiration: format_rfc3339(now + 300),
    };
    d.req.nonce = original_nonce.clone();
    d.aws.refresh_answers(
        ProcessCredentials {
            version: 1,
            access_key_id: format!("ASIA{}", "REFRESHEDTESTKEY"),
            secret_access_key: CANARY_SECRET.into(),
            session_token: refreshed_token.clone(),
            expiration: format_rfc3339(now + 3600),
        },
        CANARY_NONCE,
    );
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert!(
        d.aws.refreshes() >= 1,
        "the premise: the credentials were refreshed during the run"
    );
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedProviderError, "{s:?}");

    // The final message: the new secret key, the rotated-out token and the new nonce are each `<redacted>`.
    let message = s.cline_message.clone().unwrap();
    assert_eq!(
        message,
        "the key is <redacted> and the token <redacted>, nonce <redacted>."
    );
    // The detail (the engine's error text) and the error built from it.
    let detail = s.detail.clone().unwrap();
    assert_eq!(
        detail,
        "engine error -32603: the provider refused the request signed with <redacted> and <redacted> under nonce <redacted>"
    );
    assert_eq!(s.error.as_deref(), Some(detail.as_str()));
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].error.as_deref(), Some(detail.as_str()));
    assert_eq!(resp.error.as_deref(), Some(detail.as_str()));
    // The event text: the progress line of the message carries the redacted words.
    let messages = d.aws.progress("message");
    assert!(!messages.is_empty(), "{:?}", d.aws.progress_kinds());
    for (_, payload) in &messages {
        let text = payload["text"].as_str().unwrap();
        assert!(text.contains("<redacted>"), "{text:?}");
    }
    // Nothing anyone reads carries a raw value: the new ones, the rotated-out one, and the ones the engine never printed.
    let digest = d
        .aws
        .get(&keys::stop_digest_key(RUN, 1))
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .expect("the stop digest is in the bucket");
    let everything = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        serde_json::to_string(&d.aws.events()).unwrap(),
        serde_json::to_string(&complete).unwrap(),
        serde_json::to_string(&resp).unwrap(),
        String::from_utf8_lossy(&d.aws.get(&keys::summary_key(RUN)).unwrap()),
        digest,
        d.log.lines().join("\n"),
    );
    no_secrets(&everything);
    for value in [&original_secret, &original_nonce, &refreshed_token] {
        assert!(!everything.contains(value.as_str()), "leaked: {value}");
    }
}

// ---- setup failures ------------------------------------------------------------------------------------------------------------

async fn failed_setup(opts: Opts, detail_starts: &str) -> Direct {
    let d = Direct::start(opts).await;
    let (_, resp) = d.invoke().await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedSetupFailed, "{s:?}");
    // The label names the engine of the run: "Cline did not start" for Cline, the profile name for an engine the display table does not know.
    let engine = harness_protocol::common::Engine::new(&s.engine.name).unwrap();
    assert_eq!(
        s.label,
        DirectOutcome::StoppedSetupFailed.label_for_engine(engine, None)
    );
    let expected = if s.engine.name == "cline" {
        "Cline did not start".to_string()
    } else {
        format!("{} did not start", s.engine.name)
    };
    assert_eq!(s.label, expected);
    assert!(
        s.detail.as_deref().unwrap().starts_with(detail_starts),
        "{:?} does not start {detail_starts}",
        s.detail
    );
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    assert!(
        d.prompts_received().is_empty(),
        "no model call is made after a setup failure"
    );
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].outcome, Some(TurnOutcome::Failed));
    assert!(
        complete[0]
            .error
            .as_deref()
            .unwrap()
            .starts_with(detail_starts)
    );
    d.wait_healthy().await;
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_request_whose_hash_differs_ends_before_any_worktree_exists() {
    let d = failed_setup(
        Opts {
            tamper_request: true,
            ..Opts::default()
        },
        "request.json sha256 differs",
    )
    .await;
    assert!(
        !d.dir.join("mnt-workspace").join(RUN).exists(),
        "no worktree"
    );
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_none(),
        "nothing to bundle"
    );
    assert_eq!(d.aws.complete()[0].bundle_uri, None);
    // Only summary.json and the turn-complete are written; turn.started carries no digest.
    assert!(d.aws.get(&keys::summary_key(RUN)).is_some());
    assert!(
        d.events_of(EventType::TurnStarted)[0]
            .get("rulesDigest")
            .is_none()
    );
    assert!(d.summary().git.head_sha.is_none());
    assert!(
        !d.agent_data().join("data/fake-acp.log").exists(),
        "the engine never started"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_missing_repository_bundle_ends_before_the_engine_starts() {
    let d = failed_setup(
        Opts {
            no_bundle: true,
            ..Opts::default()
        },
        "worktree:",
    )
    .await;
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_an_engine_with_no_profile_does_not_start() {
    failed_setup(
        Opts {
            engine: "nope",
            ..Opts::default()
        },
        "engine profile nope",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_an_engine_binary_that_cannot_start() {
    let d = failed_setup(
        Opts {
            rules: vec![],
            edit_cfg: Some(Box::new(|c| c.engine_path = vec!["/nonexistent".into()])),
            ..Opts::default()
        },
        "engine start:",
    )
    .await;
    // The tree is still delivered (nothing changed, so the head is the base).
    let s = d.summary();
    assert_eq!(s.git.head_sha, s.git.base_sha);
    assert!(!s.git.committed_by_harness);
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_an_engine_that_exits_before_it_answers_says_how_it_ended() {
    let d = failed_setup(
        Opts {
            rules: vec![],
            engine_env: vec![("WRAP_EXIT", "7".into())],
            ..Opts::default()
        },
        "initialize:",
    )
    .await;
    let detail = d.summary().detail.unwrap();
    assert!(
        detail.contains("exit status 7") && detail.contains("it cannot start"),
        "{detail}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_secret_shapes_in_the_stderr_of_an_engine_that_exits_reach_neither_the_result_nor_the_wire_nor_the_log()
 {
    // The engine prints what an engine that fails to start may print; the run records the exit and one scrubbed line.
    let key = ["AS", "IA", "TESTTESTTESTTEST"].concat();
    let sig = "0123456789abcdef".repeat(4);
    // Password-shaped stand-ins are built at run time: the repository holds no password literal.
    let pw = ["hunter", "2hunter2"].concat();
    let url_pw = ["pa55", "w0rd"].concat();
    let payload = format!(
        "booting engine\nusing key {key}\npassword={pw}\nfetch https://user:{url_pw}@example.com/x\n\
         GET https://b.s3.amazonaws.com/o?X-Amz-Signature={sig}&X-Amz-Credential=zzz\n\
         Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789\nsession {CANARY_TOKEN}\nfatal: start failed\n"
    );
    let file = scratch("stderr-payload").join("payload.txt");
    std::fs::write(&file, payload).unwrap();
    let d = failed_setup(
        Opts {
            rules: vec![],
            engine_env: vec![("WRAP_STDERR_FILE", file.display().to_string())],
            ..Opts::default()
        },
        "initialize:",
    )
    .await;
    let detail = d.summary().detail.unwrap();
    let wire = serde_json::to_string(&d.aws.complete()).unwrap();
    let log = d.log.lines().join("\n");
    for (where_, text) in [
        ("summary", detail.as_str()),
        ("wire", wire.as_str()),
        ("log", log.as_str()),
    ] {
        for secret in [
            key.as_str(),
            pw.as_str(),
            url_pw.as_str(),
            sig.as_str(),
            "X-Amz-Credential=zzz",
            "abcdefghijklmnopqrstuvwxyz0123456789",
            CANARY_TOKEN,
        ] {
            assert!(
                !text.contains(secret),
                "{secret} leaked into the {where_}: {text}"
            );
        }
    }
    assert!(detail.contains("exit status 2"), "{detail}");
    assert!(detail.contains("fatal: start failed"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_huge_stderr_is_bounded_and_keeps_its_end() {
    let d = failed_setup(
        Opts {
            rules: vec![],
            engine_env: vec![("WRAP_STDERR_HUGE", "1".into())],
            ..Opts::default()
        },
        "initialize:",
    )
    .await;
    let detail = d.summary().detail.unwrap();
    assert!(detail.chars().count() < 600, "{}", detail.chars().count());
    assert!(detail.contains("exit status 1"), "{detail}");
    assert!(detail.contains("FINAL-WORDS: boom"), "{detail}");
    assert!(!detail.contains("noise line 0 "), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_rule_file_the_request_check_refuses() {
    let mut bad = rule("00-my-rules.md", "x\n");
    bad.sha256 = sha(b"other");
    failed_setup(
        Opts {
            rules: vec![bad],
            ..Opts::default()
        },
        "rules:",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_user_rule_the_engine_will_not_load_fails_the_preflight_before_any_model_call() {
    for (mode, word) in [
        ("missing", "missing"),
        ("changed", "differs"),
        ("clash", "shadowed"),
    ] {
        let d = failed_setup(
            Opts {
                engine_env: vec![("FAKE_ACP_RULES_MODE", mode.into())],
                ..Opts::default()
            },
            "rules preflight:",
        )
        .await;
        let s = d.summary();
        assert!(
            s.detail.as_deref().unwrap().contains(word),
            "{mode}: {:?}",
            s.detail
        );
        assert!(s.rules_preflight.ran);
        // A failure after the repository was restored still commits and delivers what the tree holds.
        assert!(d.aws.get(&keys::bundle_key(RUN)).is_some(), "{mode}");
        assert_eq!(s.git.head_sha, s.git.base_sha);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_listing_command_that_fails_or_prints_garbage_fails_the_preflight() {
    for mode in ["fail", "garbage"] {
        failed_setup(
            Opts {
                engine_env: vec![("FAKE_ACP_RULES_MODE", mode.into())],
                ..Opts::default()
            },
            "rules preflight:",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn row_1_a_model_the_engine_does_not_report_back_fails_the_pin_check_before_the_prompt() {
    let d = failed_setup(
        Opts {
            engine_env: vec![("FAKE_ACP_MODEL", "some.other-model".into())],
            ..Opts::default()
        },
        "model pin:",
    )
    .await;
    let s = d.summary();
    assert_eq!(s.model.pinned.as_deref(), Some("some.other-model"));
    assert!(s.detail.unwrap().contains(MODEL));
}

// ---- uploads ---------------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_upload_that_fails_twice_is_tried_again_and_delivered() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_puts(&keys::bundle_key(RUN), 2);
    let (_, resp) = d.invoke().await;
    assert_eq!(d.aws.put_attempts(&keys::bundle_key(RUN)), 3);
    assert_eq!(resp.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    assert!(d.summary().git.bundle_uri.is_some());
    assert_eq!(d.aws.complete()[0].error, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_upload_that_never_works_is_reported_and_leaves_no_bundle_uri() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_puts(&keys::bundle_key(RUN), u32::MAX);
    let (_, resp) = d.invoke().await;
    // One try and three more (2, 4, 8 s in production).
    assert_eq!(d.aws.put_attempts(&keys::bundle_key(RUN)), 4);
    assert_eq!(resp.bundle_uri, None);
    let c = &d.aws.complete()[0];
    assert_eq!(c.bundle_uri, None);
    assert_eq!(c.last_commit_sha, None);
    assert!(
        c.error.as_deref().unwrap().contains("upload:"),
        "{:?}",
        c.error
    );
    // run-session falls back to a snapshot, so the summary still says what it can.
    let s = d.summary();
    assert_eq!(s.git.bundle_uri, None);
    assert_eq!(s.git.head_sha, None);
    assert_eq!(s.git.bundle_bytes, None);
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
}

// ---- the end-of-run objects are written once (`If-None-Match: *`) ---------------------------------------------------------------

/// The engine holds the session's credentials and the worker role may `PutObject` under the run's agent prefix, so a process it
/// leaves behind could write `summary.json` before agentd does. agentd writes the object create-only: a key that already holds an
/// object is a conflict, recorded once, and the run does not read "Cline reported done".
#[tokio::test(flavor = "multi_thread")]
async fn a_summary_object_that_already_exists_is_a_conflict_and_is_replaced_by_agentds_own() {
    let d = Direct::start(Opts::default()).await;
    let forged = b"{\"forged\":true}".to_vec();
    d.aws.put(&keys::summary_key(RUN), forged.clone());
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    // One create-only put, refused (nothing failed before it, so no HEAD), then the replacing put.
    assert_eq!(d.aws.put_attempts(&keys::summary_key(RUN)), 2);
    assert_eq!(d.aws.head_attempts(&keys::summary_key(RUN)), 0);
    let stored = d.aws.get(&keys::summary_key(RUN)).unwrap();
    assert_ne!(
        stored, forged,
        "agentd's own summary replaced the foreign object"
    );
    let own: DirectSummary = serde_json::from_slice(&stored).unwrap();
    assert_eq!(own.run_id, RUN);
    assert_eq!(own.outcome, DirectOutcome::StoppedOther, "{own:?}");
    assert!(
        own.label
            .starts_with("Cline stopped: the harness ended the run ("),
        "{}",
        own.label
    );
    assert!(own.label.contains("turns/1/summary.json"), "{}", own.label);
    assert!(own.git.bundle_uri.is_some(), "the bundle was agentd's own");
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["directOutcome"], "stopped_other");
    let label = ended[0]["label"].as_str().unwrap();
    assert!(
        label.starts_with("Cline stopped: the harness ended the run ("),
        "{label}"
    );
    assert!(label.contains("turns/1/summary.json"), "{label}");
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].outcome, Some(TurnOutcome::Failed));
    let error = complete[0].error.as_deref().unwrap();
    assert!(
        error.contains("already existed") && error.contains("summary.json"),
        "{error}"
    );
    let conflicts = d.aws.progress("upload-conflict");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0].1["object"], "turns/1/summary.json");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn summary replaced:")),
        "{:?}",
        d.log.lines()
    );
    // The report step's precondition is agentd's bundle and summary in the bucket; no report module runs here, so none is stored.
    assert!(
        d.aws.get(&keys::test_report_key(RUN)).is_none(),
        "{:?}",
        d.aws.keys()
    );
    let everything = format!(
        "{}\n{}\n{}\n{}",
        serde_json::to_string(&d.aws.events()).unwrap(),
        serde_json::to_string(&d.aws.complete()).unwrap(),
        serde_json::to_string(&resp).unwrap(),
        d.log.lines().join("\n"),
    );
    no_secrets(&everything);
}

/// The bundle is the delivered head. A bundle object that was there before agentd's write is a conflict, recorded once; agentd then
/// writes its own bundle over it, so the object run-session delivers is the head agentd committed, and the run still ends as a
/// conflict. agentd's `summary.json` says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_object_that_already_exists_is_a_conflict_and_is_replaced_by_agentds_own() {
    let d = Direct::start(Opts::default()).await;
    let marker = b"not a bundle".to_vec();
    d.aws.put(&keys::bundle_key(RUN), marker.clone());
    let (_, resp) = d.invoke().await;
    assert_eq!(d.aws.put_attempts(&keys::bundle_key(RUN)), 2);
    assert_eq!(d.aws.head_attempts(&keys::bundle_key(RUN)), 0);
    let own_bundle = std::fs::read(d.agent_data().join("turns/1/bundle")).unwrap();
    let stored = d.aws.get(&keys::bundle_key(RUN)).unwrap();
    assert_ne!(stored, marker, "the foreign object was written over");
    assert_eq!(stored, own_bundle, "the object is agentd's own bundle");
    // agentd's own bundle is the delivered head again.
    assert_eq!(resp.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    assert!(resp.last_commit_sha.is_some());
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther);
    assert!(
        s.label
            .starts_with("Cline stopped: the harness ended the run ("),
        "{}",
        s.label
    );
    assert!(s.label.contains("turns/1/bundle"), "{}", s.label);
    assert_eq!(s.git.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    assert_eq!(s.git.head_sha, resp.last_commit_sha);
    assert_eq!(
        s.git.bundle_bytes,
        Some(u64::try_from(own_bundle.len()).unwrap())
    );
    let c = &d.aws.complete()[0];
    assert_eq!(c.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
    assert_eq!(c.last_commit_sha, resp.last_commit_sha);
    assert_eq!(c.outcome, Some(TurnOutcome::Failed));
    assert!(
        c.error.as_deref().unwrap().contains("already existed"),
        "{:?}",
        c.error
    );
    let conflicts = d.aws.progress("upload-conflict");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0].1["object"], "turns/1/bundle");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn bundle replaced:")),
        "{:?}",
        d.log.lines()
    );
    assert!(d.aws.get(&keys::test_report_key(RUN)).is_none());
}

/// When agentd cannot write its bundle over the foreign object, nothing names that object as the delivered head and the final error
/// says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_bundle_that_cannot_be_written_over_is_not_named_as_the_head() {
    let d = Direct::start(Opts::default()).await;
    let marker = b"not a bundle".to_vec();
    d.aws.put(&keys::bundle_key(RUN), marker.clone());
    d.aws.fail_overwrites(&keys::bundle_key(RUN), u32::MAX);
    let (_, resp) = d.invoke().await;
    // The refused create-only put, then the replacing put and its three more tries.
    assert_eq!(d.aws.put_attempts(&keys::bundle_key(RUN)), 5);
    assert_eq!(d.aws.get(&keys::bundle_key(RUN)), Some(marker));
    assert_eq!(resp.bundle_uri, None);
    assert_eq!(resp.last_commit_sha, None);
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther);
    assert_eq!(s.git.bundle_uri, None);
    assert_eq!(s.git.head_sha, None);
    let error = d.aws.complete()[0].error.clone().unwrap();
    assert!(error.contains("already existed"), "{error}");
    assert!(error.contains("could not be written over"), "{error}");
    assert_eq!(d.aws.progress("upload-conflict").len(), 1);
}

/// Same for `summary.json`: the foreign object stays, the run is a conflict, and the final error says agentd could not replace it.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_summary_that_cannot_be_written_over_is_reported_as_such() {
    let d = Direct::start(Opts::default()).await;
    let forged = b"{\"forged\":true}".to_vec();
    d.aws.put(&keys::summary_key(RUN), forged.clone());
    d.aws.fail_overwrites(&keys::summary_key(RUN), u32::MAX);
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(d.aws.put_attempts(&keys::summary_key(RUN)), 5);
    assert_eq!(d.aws.get(&keys::summary_key(RUN)), Some(forged));
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended[0]["directOutcome"], "stopped_other");
    let error = d.aws.complete()[0].error.clone().unwrap();
    assert!(error.contains("already existed"), "{error}");
    assert!(error.contains("could not be written over"), "{error}");
    assert_eq!(d.aws.progress("upload-conflict").len(), 1);
}

/// A put that reached S3 and was committed, whose answer was lost (a 5xx after the commit, a connection that broke), meets its own
/// object with the retry's 412. The object is agentd's own, so it counts as stored: no conflict event, the delivered head stays, and
/// the run reads as Cline left it. The check is the checksum agentd sent with the put (`x-amz-checksum-sha256`), read back with a HEAD.
#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_put_whose_answer_was_lost_counts_as_stored_because_the_object_is_agentds_own() {
    for how in [PutFailure::Status500, PutFailure::ResetBody] {
        let d = Direct::start(Opts::default()).await;
        d.aws.fail_puts_after_store(&keys::bundle_key(RUN), 1, how);
        let (_, resp) = d.invoke().await;
        // TRIGGER PRESENT: the first put was committed and its answer lost, so the retry put again and met the stored object.
        assert_eq!(d.aws.put_attempts(&keys::bundle_key(RUN)), 2, "{how:?}");
        assert_eq!(d.aws.head_attempts(&keys::bundle_key(RUN)), 1, "{how:?}");
        assert_eq!(
            d.aws.progress("upload-conflict").len(),
            0,
            "{how:?}: no conflict"
        );
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{how:?}: {resp:?}");
        assert_eq!(resp.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
        assert!(resp.last_commit_sha.is_some(), "{how:?}");
        let s = d.summary();
        assert_eq!(s.outcome, DirectOutcome::ReportedDone, "{how:?}");
        assert_eq!(s.git.bundle_uri.as_deref(), Some(bundle_uri().as_str()));
        assert_eq!(s.git.head_sha, resp.last_commit_sha);
        let c = &d.aws.complete()[0];
        assert_eq!(c.error, None, "{how:?}");
        assert_eq!(c.last_commit_sha, resp.last_commit_sha);
        assert!(
            d.aws.checksum_of(&keys::bundle_key(RUN)).is_some(),
            "the object carries the checksum agentd sent"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summary_put_whose_answer_was_lost_counts_as_stored_because_the_object_is_agentds_own() {
    for how in [PutFailure::Status500, PutFailure::ResetBody] {
        let d = Direct::start(Opts::default()).await;
        d.aws.fail_puts_after_store(&keys::summary_key(RUN), 1, how);
        let (_, resp) = d.invoke().await;
        d.wait_healthy().await;
        assert_eq!(d.aws.put_attempts(&keys::summary_key(RUN)), 2, "{how:?}");
        assert_eq!(d.aws.head_attempts(&keys::summary_key(RUN)), 1, "{how:?}");
        assert_eq!(d.aws.progress("upload-conflict").len(), 0, "{how:?}");
        assert_eq!(resp.outcome, TurnOutcome::Completed, "{how:?}: {resp:?}");
        assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone, "{how:?}");
        let ended = d.events_of(EventType::TurnEnded);
        assert_eq!(ended[0]["directOutcome"], "reported_done", "{how:?}");
        let c = &d.aws.complete()[0];
        assert_eq!(c.error, None, "{how:?}");
    }
}

/// The check compares the object with what agentd sent: a lost answer followed by a foreign writer replacing the committed object
/// is still a conflict, and agentd's own summary ends up in the bucket.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_answer_followed_by_a_foreign_overwrite_is_still_a_conflict() {
    let d = Direct::start(Opts::default()).await;
    let forged = b"{\"forged\":true}".to_vec();
    d.aws.fail_put_after_store_then_overwrite(
        &keys::summary_key(RUN),
        PutFailure::Status500,
        &forged,
    );
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    // The stored-then-lost put, the retry (412), the replacing put; one HEAD told the object apart.
    assert_eq!(d.aws.put_attempts(&keys::summary_key(RUN)), 3);
    assert_eq!(d.aws.head_attempts(&keys::summary_key(RUN)), 1);
    let stored = d.aws.get(&keys::summary_key(RUN)).unwrap();
    assert_ne!(stored, forged);
    let own: DirectSummary = serde_json::from_slice(&stored).unwrap();
    assert_eq!(own.outcome, DirectOutcome::StoppedOther);
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let conflicts = d.aws.progress("upload-conflict");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0].1["object"], "turns/1/summary.json");
}

/// An object that is not agentd's own bytes is a conflict whatever checksum it carries: none, or the composite value of a multipart upload.
#[tokio::test(flavor = "multi_thread")]
async fn an_object_with_another_checksum_is_not_taken_for_agentds_own() {
    for composite in [None, Some("47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=-3")] {
        let d = Direct::start(Opts::default()).await;
        let forged = b"{\"forged\":true}".to_vec();
        match composite {
            Some(sum) => d
                .aws
                .put_with_checksum(&keys::summary_key(RUN), forged.clone(), sum),
            None => d.aws.put(&keys::summary_key(RUN), forged.clone()),
        }
        // The first put fails before it reaches S3, so the retry's 412 is the one that must be told apart.
        d.aws.fail_puts(&keys::summary_key(RUN), 1);
        let (_, resp) = d.invoke().await;
        d.wait_healthy().await;
        assert_eq!(
            d.aws.put_attempts(&keys::summary_key(RUN)),
            3,
            "{composite:?}"
        );
        assert_eq!(
            d.aws.head_attempts(&keys::summary_key(RUN)),
            1,
            "{composite:?}"
        );
        assert_ne!(d.aws.get(&keys::summary_key(RUN)).unwrap(), forged);
        assert_eq!(resp.outcome, TurnOutcome::Failed, "{composite:?}");
        assert_eq!(d.aws.progress("upload-conflict").len(), 1, "{composite:?}");
    }
}

/// When the object cannot be told apart (the HEAD is refused), the 412 is neither a conflict nor a success: it is an upload that
/// failed, and nothing is replaced.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_answer_whose_object_cannot_be_checked_is_an_upload_failure_and_nothing_is_replaced()
{
    let d = Direct::start(Opts::default()).await;
    d.aws
        .fail_puts_after_store(&keys::bundle_key(RUN), 1, PutFailure::Status500);
    d.aws.heads_answer(403);
    let (_, resp) = d.invoke().await;
    // The first try, and three more; each retry meets the 412 and a HEAD that is refused.
    assert_eq!(d.aws.put_attempts(&keys::bundle_key(RUN)), 4);
    assert_eq!(d.aws.head_attempts(&keys::bundle_key(RUN)), 3);
    assert_eq!(d.aws.progress("upload-conflict").len(), 0);
    assert_eq!(resp.bundle_uri, None);
    let c = &d.aws.complete()[0];
    assert!(
        c.error.as_deref().unwrap().contains("upload:"),
        "{:?}",
        c.error
    );
    assert_eq!(d.summary().outcome, DirectOutcome::ReportedDone);
    assert!(
        d.aws.checksum_of(&keys::bundle_key(RUN)).is_some(),
        "the object stays agentd's own"
    );
}

// ---- the server ----------------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn refusals_name_the_rule_and_never_echo_the_input() {
    let d = Direct::start(Opts::default()).await;
    // Not a DirectInvocationRequest: a position only.
    let (code, body) = d.post_raw(r#"{"kind":"direct","runId":"x","CANARY-FIELD":"canary-token-agentd-must-never-print"}"#.into()).await;
    assert_eq!(code, 400);
    assert!(
        body.contains("not a DirectInvocationRequest") && body.contains("line"),
        "{body}"
    );
    assert!(
        !body.contains("CANARY") && !body.contains("canary"),
        "{body}"
    );
    // Parses, breaks a rule.
    let mut bad = d.req.clone();
    bad.agent_id = "a1".into();
    let (code, body) = d.post_raw(serde_json::to_string(&bad).unwrap()).await;
    assert_eq!(code, 400);
    assert!(body.contains("agentId must be"), "{body}");
    let mut bad = d.req.clone();
    bad.timeout_s = 0;
    let (code, body) = d.post_raw(serde_json::to_string(&bad).unwrap()).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("timeoutS"));
    // Too large.
    let (code, body) = d
        .post_raw(format!(
            "{{\"kind\":\"direct\",\"pad\":\"{}\"}}",
            "x".repeat(70_000)
        ))
        .await;
    assert_eq!(code, 400);
    assert!(body.contains("65536"), "{body}");
    assert_eq!(
        d.ping().await,
        PingStatus::Healthy,
        "a refused invocation takes no turn slot"
    );
    assert!(d.aws.complete().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_without_a_kind_is_refused_as_not_a_direct_request() {
    let d = Direct::start(Opts::default()).await;
    let (code, body) = d.post_raw(r#"{"runId":"r1"}"#.into()).await;
    assert_eq!(code, 400);
    assert!(body.contains("not a DirectInvocationRequest"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_is_busy_while_the_turn_runs_and_a_second_invocation_gets_409() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 8,
        ..Opts::default()
    })
    .await;
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(d.ping().await, PingStatus::HealthyBusy);
    let (code, body) = d.post().await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("already running"));
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::TimedOut);
    d.wait_healthy().await;
}

// ---- the optional nudge --------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn by_default_one_prompt_is_sent_even_when_the_engine_did_not_say_done() {
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received(), vec![PROMPT.to_string()]);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert!(!s.nudge.requested && !s.nudge.honored);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nudge_file_with_a_count_of_zero_is_requested_and_not_honored() {
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        nudge: nudge("Continue.", 0),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received().len(), 1);
    let s = d.summary();
    assert!(s.nudge.requested && !s.nudge.honored);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nudge_is_sent_after_an_end_turn_that_did_not_say_done_and_stops_at_done() {
    let d = Direct::start(Opts {
        scenario: "nudge_done",
        nudge: nudge("Re-read my request. Reply DONE if it is all done.", 3),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(
        d.prompts_received(),
        vec![
            PROMPT.to_string(),
            "Re-read my request. Reply DONE if it is all done.".to_string()
        ]
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        s.cline_message.as_deref(),
        Some("DONE: everything is finished.")
    );
    assert!(s.nudge.requested && s.nudge.honored);
    // One turn.started and one turn.ended however many prompts the turn held.
    assert_eq!(d.events_of(EventType::TurnStarted).len(), 1);
    assert_eq!(d.events_of(EventType::TurnEnded).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_nudge_stops_when_two_prompts_in_a_row_changed_nothing() {
    let d = Direct::start(Opts {
        scenario: "nudge_idle",
        nudge: nudge("Keep going.", 5),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(
        d.prompts_received(),
        vec![PROMPT.to_string(), "Keep going.".to_string()]
    );
    assert_eq!(d.summary().git.files_changed, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_nudge_stops_at_its_count_while_the_tree_keeps_changing() {
    let d = Direct::start(Opts {
        scenario: "nudge_busy",
        nudge: nudge("Keep going.", 2),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    // The first prompt and two nudges; every reply wrote a new file and none said DONE.
    assert_eq!(d.prompts_received().len(), 3);
    let s = d.summary();
    assert_eq!(s.git.files_changed, 3);
    assert_eq!(
        s.outcome,
        DirectOutcome::ReportedDone,
        "the last reply ended its turn with a message"
    );
}

// ---- the session's git identity ------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_run_leaves_the_engine_a_repository_it_can_commit_in_without_configuration() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let repo = d.dir.join("mnt-workspace").join(RUN).join("direct/repo");
    assert!(repo.join(".git").exists(), "{}", repo.display());
    let get = |key: &str| {
        let o = std::process::Command::new("git")
            .args(["config", "--local", "--get", key])
            .current_dir(&repo)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    assert_eq!(get("user.name"), "ham agent");
    assert_eq!(get("user.email"), "agent@harness.invalid");
}

// ---- the package proxy seam ------------------------------------------------------------------------------------------------------

/// What a proxy start was given: host, repository, token, expiry.
type Started = (String, String, String, Option<u64>);

#[derive(Default)]
struct Seen {
    started: Mutex<Vec<Started>>,
    /// The Go module store URL each start was given (`AGENTD_GOPROXY` of the deployment).
    goproxy: Mutex<Vec<Option<String>>>,
    stopped: Mutex<bool>,
}

struct FakeLauncher(Arc<Seen>);
struct FakeRun(Arc<Seen>);

/// What the fake proxy hands agentd as the cargo config file (the real text is `pkgproxy::Handle::cargo_config_toml`).
const FAKE_CARGO_TOML: &str = "[source.crates-io]\nreplace-with = \"hamh-proxy\"\n\n[registries.hamh-proxy]\nindex = \"sparse+http://127.0.0.1:3128/cargo/\"\n";

impl ProxyRun for FakeRun {
    fn env(&self) -> BTreeMap<String, String> {
        [
            ("PIP_INDEX_URL", "http://127.0.0.1:3128/pypi/simple/"),
            ("npm_config_registry", "http://127.0.0.1:3128/npm/"),
            ("GOPROXY", "http://127.0.0.1:3128/go"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }
    fn maven_settings_xml(&self) -> String {
        "<settings><mirrors><mirror><id>loop</id><mirrorOf>*</mirrorOf><url>http://127.0.0.1:3128/maven/</url></mirror></mirrors></settings>".into()
    }
    fn cargo_config_toml(&self) -> String {
        FAKE_CARGO_TOML.into()
    }
    fn stats(&self) -> DirectPackages {
        DirectPackages {
            enabled: true,
            requests: 7,
            bytes: 1234,
            by_prefix: [("pypi".to_string(), 5), ("npm".to_string(), 2)].into(),
            statuses: [(200, 6), (404, 1)].into(),
            refused: 0,
        }
    }
    fn stop(self: Box<Self>) -> agentd::BoxFuture<'static, ()> {
        Box::pin(async move { *self.0.stopped.lock().unwrap() = true })
    }
}

/// `ProxyStart` carries the token as the wire type itself (report 7.4): `expose` is the one way to its text, and the buffer is
/// wiped when the start is dropped. A plain `String` here would not compile.
fn _proxy_start_carries_a_wiped_token(s: &ProxyStart) -> &PackagesToken {
    &s.token
}

impl ProxyLauncher for FakeLauncher {
    fn start(&self, s: ProxyStart) -> agentd::BoxFuture<'_, Result<Box<dyn ProxyRun>, String>> {
        self.0.started.lock().unwrap().push((
            s.host.clone(),
            s.repository.clone(),
            s.token.expose().to_string(),
            s.expires_at,
        ));
        self.0.goproxy.lock().unwrap().push(s.goproxy.clone());
        let seen = self.0.clone();
        Box::pin(async move { Ok(Box::new(FakeRun(seen)) as Box<dyn ProxyRun>) })
    }
}

fn with_packages(c: &mut agentd::config::Config) {
    c.packages = Some(agentd::packages::PackagesIndex {
        domain: "hamh-direct".into(),
        owner: "111111111111".into(),
        repository: "packages".into(),
        api_endpoint: None,
    });
}

/// The deployment's Go module store (`AGENTD_GOPROXY`, the stage's `hamh-<stage>-gomod-<account>` bucket URL) is the proxy's `go`
/// upstream; without it the proxy has no Go upstream and a `go get` answers 403.
const GOMOD_STORE: &str = "https://hamh-direct-gomod-111111111111.s3.us-east-1.amazonaws.com";

async fn proxy_start_goproxy(store: Option<&'static str>) -> Vec<Option<String>> {
    let d = Direct::start(Opts {
        packages_token: true,
        edit_cfg: Some(Box::new(move |c: &mut agentd::config::Config| {
            with_packages(c);
            c.goproxy = store.map(str::to_string);
        })),
        ..Opts::default()
    })
    .await;
    let seen = Arc::new(Seen::default());
    let deps = Deps {
        proxy: Arc::new(FakeLauncher(seen.clone())),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    seen.goproxy.lock().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_go_module_store_of_the_deployment_is_the_proxys_go_upstream() {
    assert_eq!(
        proxy_start_goproxy(Some(GOMOD_STORE)).await,
        vec![Some(GOMOD_STORE.to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deployment_with_no_go_module_store_starts_the_proxy_without_a_go_upstream() {
    assert_eq!(proxy_start_goproxy(None).await, vec![None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_package_proxy_starts_for_a_run_with_a_token_and_the_engine_reaches_it_through_the_environment()
 {
    let d = Direct::start(Opts {
        packages_token: true,
        edit_cfg: Some(Box::new(with_packages)),
        ..Opts::default()
    })
    .await;
    let seen = Arc::new(Seen::default());
    let deps = Deps {
        proxy: Arc::new(FakeLauncher(seen.clone())),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let started = seen.started.lock().unwrap().clone();
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].0,
        "hamh-direct-111111111111.d.codeartifact.us-east-1.amazonaws.com"
    );
    assert_eq!(started[0].1, "packages");
    assert_eq!(
        started[0].2,
        d.req.packages_token.as_ref().unwrap().expose()
    );
    assert_eq!(started[0].3, d.req.packages_token_expires_at);
    assert!(
        *seen.stopped.lock().unwrap(),
        "the proxy is stopped at the end"
    );
    // The engine sees the proxy's variables, and none of the offline switches.
    let probe = d.engine_probe();
    for want in [
        "PIP_INDEX_URL=http://127.0.0.1:3128/pypi/simple/",
        "npm_config_registry=http://127.0.0.1:3128/npm/",
        "GOPROXY=http://127.0.0.1:3128/go",
    ] {
        assert!(probe.lines().any(|l| l == want), "{want}");
    }
    assert!(
        !probe
            .lines()
            .any(|l| l.starts_with("UV_OFFLINE=") || l.starts_with("PIP_NO_INDEX="))
    );
    // The Maven mirror file is there and holds no secret.
    let settings = std::fs::read_to_string(d.agent_data().join("home/.m2/settings.xml")).unwrap();
    assert!(settings.contains("127.0.0.1:3128/maven/"));
    // The counters are in the summary.
    let s = d.summary();
    assert!(s.packages.enabled);
    assert_eq!((s.packages.requests, s.packages.bytes), (7, 1234));
    assert_eq!(s.packages.by_prefix.get("pypi"), Some(&5));
    assert_eq!(s.packages.statuses.get(&404), Some(&1));
    // The token is in no file of the session and in no log line.
    let token = d.req.packages_token.as_ref().unwrap().expose().to_string();
    assert_ne!(token, CANARY_TOKEN);
    for root in [d.agent_data(), d.dir.join("mnt-workspace")] {
        let out = std::process::Command::new("grep")
            .args(["-r", "-l", "-F", &token])
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "token found in {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    for line in d.log.lines() {
        assert!(!line.contains(&token), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_token_or_an_index_no_proxy_starts() {
    for (token, index) in [(false, true), (true, false)] {
        let d = Direct::start(Opts {
            packages_token: token,
            edit_cfg: index
                .then(|| Box::new(with_packages) as Box<dyn FnOnce(&mut agentd::config::Config)>),
            ..Opts::default()
        })
        .await;
        let seen = Arc::new(Seen::default());
        let deps = Deps {
            proxy: Arc::new(FakeLauncher(seen.clone())),
            ..Deps::default()
        };
        agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
        assert!(seen.started.lock().unwrap().is_empty());
        assert!(!d.summary().packages.enabled);
    }
}

// ---- the periodic snapshot ---------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_is_uploaded_while_the_prompt_runs_and_the_repository_is_untouched() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "4".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(700))
        })),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let s = d.summary();
    assert!(s.snapshots.count >= 1, "{:?}", s.snapshots);
    assert!(s.snapshots.last_at.is_some());
    let json: serde_json::Value = serde_json::from_slice(
        &d.aws
            .get(&keys::snapshot_json_key(RUN))
            .expect("snapshot.json"),
    )
    .unwrap();
    assert_eq!(json["schemaVersion"], 1);
    assert_eq!(json["baseSha"], s.git.base_sha.clone().unwrap());
    let head = json["headSha"].as_str().unwrap().to_string();
    let bundle = d.dir.join("snapshot.bundle");
    std::fs::write(
        &bundle,
        d.aws
            .get(&keys::snapshot_bundle_key(RUN))
            .expect("snapshot.bundle"),
    )
    .unwrap();
    let wt = d.worktree();
    git(&wt, &["bundle", "verify", bundle.to_str().unwrap()]);
    // Cloning the bundle proves it is complete: its history reaches the base and its tree holds the engine's edits.
    let clone = d.dir.join("snapshot-clone");
    git(
        &d.dir,
        &[
            "clone",
            "-q",
            bundle.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let snap = "refs/remotes/origin/snapshot";
    assert_eq!(git(&clone, &["rev-parse", snap]), head);
    assert!(git(&clone, &["show", &format!("{snap}:src/calc.py")]).contains("def percent_change"));
    assert!(
        git(&clone, &["show", &format!("{snap}:tests/test_calc.py")]).contains("percent_change")
    );
    assert_eq!(
        git(&clone, &["rev-parse", &format!("{snap}^")]),
        s.git.base_sha.clone().unwrap()
    );
    assert!(git(&clone, &["log", "-1", "--format=%an", snap]).contains("ham harness"));
    // The snapshot's parent is the repository's HEAD at the time; the repository has no snapshot ref.
    assert!(git(&wt, &["branch", "--list", "snapshot"]).is_empty());
    // The turn-complete never names the snapshot: worker-api refuses it on the direct agent.
    assert_eq!(d.aws.complete()[0].snapshot_uri, None);
    // The final bundle is still the full result.
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_that_cannot_upload_is_counted_and_never_ends_the_turn() {
    let d = Direct::start(Opts {
        scenario: "writes_files",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.snapshot_interval = Some(Duration::from_millis(500))
        })),
        ..Opts::default()
    })
    .await;
    d.aws.fail_puts("snapshot.bundle", u32::MAX);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    let s = d.summary();
    assert!(s.snapshots.failures >= 1, "{:?}", s.snapshots);
    assert_eq!(s.snapshots.count, 0);
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
}

// ---- review of 2026-10-13, stream agentd: the lines that mutation showed nothing tested --------------------------------------

/// The engine's launch: the old line keeps the profile's argv, the direct launch adds `--auto-approve true` after `--acp`.
#[tokio::test(flavor = "multi_thread")]
async fn the_direct_launch_adds_auto_approve_to_the_engine_argv() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let probe = d.engine_probe();
    let argv: Vec<&str> = probe
        .lines()
        .skip_while(|l| *l != "--- argv")
        .skip(1)
        .take_while(|l| !l.starts_with("---"))
        .collect();
    let at = argv
        .iter()
        .position(|a| *a == "--auto-approve")
        .unwrap_or_else(|| panic!("no --auto-approve in {argv:?}"));
    assert_eq!(argv.get(at + 1), Some(&"true"), "{argv:?}");
    assert_eq!(
        argv.iter().filter(|a| **a == "--auto-approve").count(),
        1,
        "{argv:?}"
    );
}

/// Review S2 (M15): the deadline counts from the moment the invocation was accepted. A repository that takes 5 s to restore
/// leaves a 7 s run only 2 s of prompt, so the microVM's own limit still holds after a slow setup.
#[tokio::test(flavor = "multi_thread")]
async fn the_deadline_counts_from_the_invocation_not_from_the_prompt() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_HOLD_PROMPTS", "1".into())],
        timeout_s: 7,
        ..Opts::default()
    })
    .await;
    d.aws
        .delay_gets(&keys::repo_bundle_key(RUN), Duration::from_secs(5));
    let t0 = Instant::now();
    let (_, resp) = d.invoke().await;
    let took = t0.elapsed();
    // TRIGGER PRESENT: the restore really was slow.
    assert!(took >= Duration::from_secs(7), "{took:?}");
    assert_eq!(resp.outcome, TurnOutcome::TimedOut, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedTimeout);
    // The prompt had what was left of 7 s, not 7 s of its own.
    assert!(s.timing.prompt_s.unwrap() <= 3, "{:?}", s.timing);
    assert!(
        took < Duration::from_secs(10),
        "the run outlived its deadline by the setup time: {took:?}"
    );
}

/// Review S3 (M46): a clean filter the engine writes into `.git/config` (and selects in `.git/info/attributes`) never runs in
/// agentd's own `git add`, because the end sequence puts both files back first.
#[tokio::test(flavor = "multi_thread")]
async fn a_clean_filter_written_into_the_repository_config_never_runs_when_agentd_commits() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_FILTER_SETTING", "1".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    let home = d.agent_data().join("home");
    // TRIGGER PRESENT: the engine did write the filter, and the files it changed are the repository's own.
    let seen = std::fs::read_to_string(home.join("filter-setting-seen")).unwrap();
    assert!(seen.contains("clean-filter.sh"), "{seen}");
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(d.summary().git.committed_by_harness);
    assert!(
        !home.join("filter-used").exists(),
        "agentd's git add ran the engine's clean filter"
    );
    let config = std::fs::read_to_string(d.worktree().join(".git/config")).unwrap();
    assert!(!config.contains("filter"), "{config}");
    assert!(
        !d.worktree().join(".git/info/attributes").exists(),
        "the attribute that selects the filter is gone"
    );
}

/// Review (b630-secrets-2): the engine replaces `.git` by a file that names a directory of its own and defines a clean filter there.
/// Putting `.git/config` back cannot work (there is no `.git/config`), and agentd's `git add -A` would run the engine's program outside every
/// guard. The end sequence refuses instead: no commit, no bundle, and the run ends as the harness's own stop with the reason.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_the_engine_redirected_gets_no_commit_and_no_bundle_from_agentd() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_GIT_REDIRECT", "1".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    let home = d.agent_data().join("home");
    // TRIGGER PRESENT: the engine did redirect the repository, and git read the filter through the file it wrote.
    let seen = std::fs::read_to_string(home.join("filter-setting-seen")).unwrap();
    assert!(seen.contains("clean-filter.sh"), "{seen}");
    assert!(
        std::fs::symlink_metadata(d.worktree().join(".git"))
            .unwrap()
            .file_type()
            .is_file(),
        "`.git` is a file naming the engine's directory"
    );
    assert!(
        !home.join("filter-used").exists(),
        "agentd's git ran the engine's clean filter"
    );
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    assert!(resp.last_commit_sha.is_none(), "{resp:?}");
    assert!(resp.bundle_uri.is_none(), "{resp:?}");
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_none(),
        "a bundle of a repository agentd could not trust was uploaded"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedOther, "{s:?}");
    assert!(!s.git.committed_by_harness);
    assert!(
        s.label
            .starts_with("Cline stopped: the harness ended the run ("),
        "{}",
        s.label
    );
}

/// Review S3 (M45): build outputs the engine leaves (`__pycache__/`, `*.pyc`) are listed in `.git/info/exclude` before the
/// engine starts, so the commit never carries them.
#[tokio::test(flavor = "multi_thread")]
async fn build_outputs_the_engine_leaves_are_not_committed() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_BUILD_OUTPUTS", "1".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    // TRIGGER PRESENT: the files are in the tree when agentd commits.
    assert!(d.agent_data().join("home/build-outputs-seen").exists());
    assert!(
        d.worktree()
            .join("src/__pycache__/calc.cpython-312.pyc")
            .exists()
    );
    assert!(d.worktree().join("src/stray.pyc").exists());
    let head = resp.last_commit_sha.unwrap();
    let files = git(&d.worktree(), &["ls-tree", "-r", "--name-only", &head]);
    assert!(
        !files.contains("__pycache__") && !files.contains(".pyc"),
        "{files}"
    );
    assert!(files.contains("src/calc.py"));
}

/// Review N4: the bundle is written under agentd's own data directory. A `turns` link the engine placed in the worktree root
/// (which the engine user owns) is not followed by root.
#[tokio::test(flavor = "multi_thread")]
async fn the_bundle_is_written_under_agentds_directory_not_through_a_link_the_engine_placed() {
    let d = Direct::start(Opts {
        engine_env: vec![("WRAP_REDIRECT_TURNS", "1".into())],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    // TRIGGER PRESENT: the link is where the engine put it.
    let placed_link = d.dir.join("mnt-workspace").join(RUN).join("direct/turns");
    assert!(
        std::fs::symlink_metadata(&placed_link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the engine placed a link"
    );
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        std::fs::read_dir(d.agent_data().join("home/redirect"))
            .unwrap()
            .count(),
        0
    );
    assert!(d.agent_data().join("turns/1/bundle").is_file());
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
}

/// Review S4 (M28), the part a normal user can see: the configuration directory is protected after the rules are placed and
/// before the engine is started. The kernel's side of it is `direct_uid_split` (root only).
#[tokio::test(flavor = "multi_thread")]
async fn the_configuration_directory_is_protected_between_the_rules_and_the_engine_start() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let lines = d.log.lines();
    let at = |prefix: &str| {
        lines
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no log line starting {prefix:?} in {lines:#?}"))
    };
    let placed = at("direct turn rules placed:");
    let protected = at("direct turn config directory protected:");
    let starting = at("direct turn engine starting");
    assert!(
        placed < protected && protected < starting,
        "{placed} {protected} {starting}"
    );
    // Without root there is no engine user to hand the directory over from; the line names the directory's real owner.
    assert!(
        lines[protected]
            .starts_with("direct turn config directory protected: given_to_root=false owner_uid="),
        "{}",
        lines[protected]
    );
}

/// Review N2 (M33): a turn-complete the internal API refuses is posted again, and a run that never gets through still ends with
/// its result on disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_complete_the_api_refuses_twice_is_posted_again() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_complete(2);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    assert_eq!(d.aws.complete_attempts(), 3);
    assert_eq!(d.aws.complete().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_complete_that_never_gets_through_is_tried_four_times_and_the_result_is_still_written()
 {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_complete(u32::MAX);
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
    assert_eq!(d.aws.complete_attempts(), 4, "one post and three more");
    assert!(d.aws.complete().is_empty());
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn turn-complete failed:"))
    );
    assert!(d.result_path().is_file());
}

/// Review N2 (M35): a `result.json` left by an earlier attempt of the same turn is removed when the invocation is accepted, so
/// nobody reads the old answer while the new turn runs.
#[tokio::test(flavor = "multi_thread")]
async fn an_older_result_is_removed_when_the_invocation_is_accepted() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3".into())],
        ..Opts::default()
    })
    .await;
    let stale = d.result_path();
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, br#"{"stale":true}"#).unwrap();
    let (code, text) = d.post().await;
    assert_eq!(code, 200, "{text}");
    assert!(
        !stale.exists(),
        "the old result.json is still there while the new turn runs"
    );
    let resp = d.wait_result().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed);
}

/// Review N2 (M49): the heartbeat has its own period, not the events period.
#[tokio::test(flavor = "multi_thread")]
async fn the_heartbeat_follows_its_own_period() {
    let d = Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3".into())],
        edit_cfg: Some(Box::new(|c| {
            c.direct.events_every = Duration::from_millis(50);
            c.direct.heartbeat_every = Duration::from_millis(500);
        })),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let beats = d.aws.heartbeats().len();
    // About 3 s at 500 ms: 7 or 8. At the 50 ms events period it would be over 60.
    assert!((4..=20).contains(&beats), "{beats} heartbeats");
    // Events do go out at their own, faster period.
    assert!(
        d.aws.batches().len() > beats,
        "{} batches",
        d.aws.batches().len()
    );
}

/// Review N7: a package proxy whose start panics ends as "no packages", the same as one that returns an error.
struct PanickingLauncher;

impl ProxyLauncher for PanickingLauncher {
    fn start(&self, _s: ProxyStart) -> agentd::BoxFuture<'_, Result<Box<dyn ProxyRun>, String>> {
        Box::pin(async move { panic!("the proxy start panicked") })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_proxy_that_panics_at_start_does_not_end_the_run() {
    let d = Direct::start(Opts {
        packages_token: true,
        timeout_s: 30,
        edit_cfg: Some(Box::new(with_packages)),
        ..Opts::default()
    })
    .await;
    let deps = Deps {
        proxy: Arc::new(PanickingLauncher),
        ..Deps::default()
    };
    let resp = agentd::direct::run_with(&d.app, d.req.clone(), &deps).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn packages not available:"))
    );
    assert!(!d.summary().packages.enabled);
}

// ---- the provider retry ---------------------------------------------------

fn retries(n: u32) -> direct_support::CfgEdit {
    Box::new(move |c| {
        c.direct.provider_retries = n;
        c.direct.provider_retry_pause = Duration::from_millis(20);
    })
}

fn count_in_engine_log(d: &Direct, method: &str) -> usize {
    d.fake_engine_log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["method"] == method)
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_that_ends_in_the_engines_timeout_is_resumed_once_on_the_same_session_and_summary_json_lists_both_tries()
 {
    let d = Direct::start(Opts {
        scenario: "timeout_then_done",
        edit_cfg: Some(retries(3)),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.prompts_received(),
        vec![
            PROMPT.to_string(),
            agentd::direct::retry::RESUME_PROMPT.to_string()
        ],
        "the user's text first, the fixed sentence second"
    );
    assert_eq!(
        count_in_engine_log(&d, "session/new"),
        1,
        "one engine session"
    );
    assert_eq!(
        count_in_engine_log(&d, "initialize"),
        1,
        "one engine process"
    );
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(
        s.cline_message.as_deref(),
        Some("Checked the tree; the work is in place.")
    );
    assert_eq!(s.attempts.len(), 2);
    assert_eq!(s.attempts[0].retry_class.as_deref(), Some("transient"));
    assert!(
        s.attempts[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("timed out")
    );
    assert!(s.attempts[0].backoff_s.is_some());
    assert_eq!(s.attempts[1].n, 2);
    // The work the first try wrote is delivered.
    assert!(s.git.committed_by_harness);
    // Narration: one progress event, one log line; the turn ended once, as the engine's answer.
    let retry_events: Vec<_> = d
        .events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "provider-retry")
        .collect();
    assert_eq!(retry_events.len(), 1, "{retry_events:?}");
    assert_eq!(retry_events[0]["attempt"], 2);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn provider failure: try 2 of 4 follows in")),
        "{:?}",
        d.log.lines()
    );
    assert_eq!(d.events_of(EventType::TurnEnded).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_keeps_timing_out_ends_the_run_after_the_last_retry_with_every_try_listed()
{
    let d = Direct::start(Opts {
        scenario: "timeout_always",
        edit_cfg: Some(retries(3)),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    assert_eq!(d.prompts_received().len(), 4);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedProviderError);
    assert_eq!(s.attempts.len(), 4);
    assert!(
        s.attempts
            .iter()
            .all(|a| a.retry_class.as_deref() == Some("transient"))
    );
    assert_eq!(s.attempts[3].backoff_s, None);
    // The classifier reads the engine's own idle-timeout wording as a stall (`AcpError::fault_hint`).
    assert_eq!(resp.fault_hint.unwrap().code.as_deref(), Some("stall"));
    // What the first try wrote is still delivered with the failure.
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_setting_at_zero_sends_one_prompt_as_in_v1() {
    let d = Direct::start(Opts {
        scenario: "timeout_then_done",
        edit_cfg: Some(retries(0)),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received(), vec![PROMPT.to_string()]);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedProviderError);
    assert!(s.attempts.is_empty());
}

// ---- provider failover, the second slice ----------------------------------------------------------------------------------

const OPUS_GLOBAL: &str = "global.anthropic.claude-opus-5-5";
const SONNET_GLOBAL: &str = "global.anthropic.claude-sonnet-5-5";

fn route_spec(id: &str, window: u32, out: u32) -> harness_protocol::direct::DirectModelSpec {
    harness_protocol::direct::DirectModelSpec {
        id: id.into(),
        region: "us-east-1".into(),
        protocol: harness_protocol::common::Protocol::Converse,
        context_tokens: window,
        max_output_tokens: out,
        default_reasoning_effort: None,
    }
}

/// Quick failover timings: the resumes, the dwell and the waits are tens of milliseconds, and no probe leaves the test.
fn quick_failover() -> direct_support::CfgEdit {
    Box::new(|c| {
        c.direct.provider_retries = 3;
        c.direct.provider_retry_pause = Duration::from_millis(20);
        c.direct.failover_wait = Duration::from_millis(100);
        c.direct.failover_wait_cap = Duration::from_millis(200);
        c.direct.failover_probe = false;
    })
}

fn models_asked_of_the_engine(d: &Direct) -> Vec<String> {
    d.fake_engine_log()
        .lines()
        .filter_map(|l| l.strip_prefix("in "))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["method"] == "session/set_config_option")
        .map(|v| v["params"]["value"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_stays_down_on_the_model_is_switched_to_the_other_profile_and_summary_json_says_so()
 {
    let d = Direct::start(Opts {
        scenario: "provider_down_then_done",
        edit_cfg: Some(quick_failover()),
        failover: vec![route_spec(OPUS_GLOBAL, 1_000_000, 128_000)],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    // The user's prompt and three resumes on the model, then one resume after the switch, all on the one engine session.
    assert_eq!(d.prompts_received().len(), 5);
    assert_eq!(count_in_engine_log(&d, "session/new"), 1);
    assert_eq!(count_in_engine_log(&d, "initialize"), 1);
    assert_eq!(models_asked_of_the_engine(&d), [OPUS_GLOBAL]);
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::ReportedDone);
    assert_eq!(s.switches.len(), 1);
    assert_eq!(s.switches[0].to, OPUS_GLOBAL);
    assert_eq!(
        s.switches[0].kind,
        harness_protocol::direct::DirectSwitchKind::Route
    );
    assert_eq!(s.final_model.as_deref(), Some(OPUS_GLOBAL));
    assert_eq!(
        s.model.id, MODEL,
        "the model the user chose stays the preference"
    );
    assert_eq!(s.failover.as_ref().unwrap().routes, 2);
    assert!(
        s.detail
            .as_deref()
            .unwrap()
            .starts_with(&format!("Ran on {OPUS_GLOBAL} after 1 switch.")),
        "{:?}",
        s.detail
    );
    assert_eq!(s.attempts.len(), 5);
    assert_eq!(s.attempts[4].model_id.as_deref(), Some(OPUS_GLOBAL));
    let switches: Vec<_> = d
        .events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "provider-switch")
        .collect();
    assert_eq!(switches.len(), 1, "{switches:?}");
    assert_eq!(switches[0]["to"], OPUS_GLOBAL);
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn provider switch 1:")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_the_provider_refuses_is_left_at_once_and_the_work_is_delivered_from_the_next() {
    let d = Direct::start(Opts {
        scenario: "route_refused_then_done",
        edit_cfg: Some(quick_failover()),
        failover: vec![route_spec(OPUS_GLOBAL, 1_000_000, 128_000)],
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.prompts_received().len(),
        2,
        "no resume on the refused route"
    );
    let s = d.summary();
    assert_eq!(s.switches[0].class, "unavailable");
    assert_eq!(s.attempts[0].retry_class.as_deref(), Some("unavailable"));
}

#[tokio::test(flavor = "multi_thread")]
async fn with_routes_the_engines_output_limit_and_window_are_the_smallest_of_the_routes() {
    let d = Direct::start(Opts {
        scenario: "provider_down_then_done",
        edit_cfg: Some(quick_failover()),
        failover: vec![
            route_spec(OPUS_GLOBAL, 900_000, 100_000),
            route_spec(SONNET_GLOBAL, 1_050_000, 128_000),
        ],
        ..Opts::default()
    })
    .await;
    let _ = d.invoke().await;
    let providers: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(d.agent_data().join("data/settings/providers.json")).unwrap(),
    )
    .unwrap();
    let settings = &providers["providers"]["bedrock"]["settings"];
    assert_eq!(settings["maxTokens"], 100_000);
    assert_eq!(settings["contextWindow"], 900_000);
    assert_eq!(
        d.summary().failover.unwrap().effective_max_output_tokens,
        100_000
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_routes_the_limits_are_the_models_own_and_nothing_switches() {
    let d = Direct::start(Opts {
        scenario: "provider_down_then_done",
        edit_cfg: Some(quick_failover()),
        ..Opts::default()
    })
    .await;
    let _ = d.invoke().await;
    let providers: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(d.agent_data().join("data/settings/providers.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        providers["providers"]["bedrock"]["settings"]["maxTokens"],
        128_000
    );
    assert!(models_asked_of_the_engine(&d).is_empty());
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedProviderError);
    assert!(s.switches.is_empty() && s.failover.is_none() && s.final_model.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_switch_off_ignores_the_routes_of_the_request() {
    let d = Direct::start(Opts {
        scenario: "provider_down_then_done",
        edit_cfg: Some(Box::new(|c| {
            c.direct.provider_retries = 3;
            c.direct.provider_retry_pause = Duration::from_millis(20);
            c.direct.failover = false;
        })),
        failover: vec![route_spec(OPUS_GLOBAL, 1_000_000, 128_000)],
        ..Opts::default()
    })
    .await;
    let _ = d.invoke().await;
    assert!(models_asked_of_the_engine(&d).is_empty());
    assert_eq!(d.prompts_received().len(), 4);
    assert!(d.summary().switches.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_refuses_the_only_other_route_leaves_the_run_waiting_on_the_model_until_its_deadline_and_the_summary_says_so()
 {
    let d = Direct::start(Opts {
        scenario: "provider_down_always",
        edit_cfg: Some(quick_failover()),
        engine_env: vec![("FAKE_ACP_REFUSE_MODELS", OPUS_GLOBAL.to_string())],
        failover: vec![route_spec(OPUS_GLOBAL, 1_000_000, 128_000)],
        timeout_s: 3,
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::TimedOut, "{resp:?}");
    let s = d.summary();
    assert!(s.switches.is_empty(), "a refused switch is not a switch");
    assert_eq!(s.outcome, DirectOutcome::StoppedTimeout);
    assert!(
        s.detail
            .as_deref()
            .unwrap_or_default()
            .contains("Every route was down for the last"),
        "{:?}",
        s.detail
    );
}

// ---- Cline's own stops are not provider failures ---------------------------------------------------------------

/// The eight failover routes of a run on the user's model (`MODEL`, Opus 5.5 us.): the order and ids of the catalog's list for Opus.
fn eight_routes() -> Vec<harness_protocol::direct::DirectModelSpec> {
    [
        OPUS_GLOBAL,
        SONNET_GLOBAL,
        "us.anthropic.claude-sonnet-5-5",
        "us.openai.gpt-6.1-sol",
        "global.openai.gpt-6.1-sol",
        "us.openai.gpt-6-astra",
        "global.openai.gpt-6-astra",
        "us.zai.glm-5.3",
    ]
    .iter()
    .map(|id| route_spec(id, 1_000_000, 128_000))
    .collect()
}

fn engine_stop_events(d: &Direct) -> Vec<serde_json::Value> {
    d.events_of(EventType::TurnProgress)
        .into_iter()
        .filter(|v| v["kind"] == "engine-stop")
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a12_cline_stopping_itself_with_eight_failover_routes_keeps_the_model_the_user_chose() {
    let d = Direct::start(Opts {
        scenario: "loop_guard_always",
        edit_cfg: Some(quick_failover()),
        failover: eight_routes(),
        ..Opts::default()
    })
    .await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Failed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedEngineCancelled);
    assert_eq!(
        d.prompts_received().len(),
        2,
        "the prompt and one resume, nothing else"
    );
    assert!(
        models_asked_of_the_engine(&d).is_empty(),
        "the engine was never asked to change model: {:?}",
        models_asked_of_the_engine(&d)
    );
    assert!(s.switches.is_empty(), "{:?}", s.switches);
    assert_eq!(
        s.final_model, None,
        "the run ended on the model the user chose"
    );
    assert_eq!(s.model.id, MODEL);
    assert_eq!(s.attempts.len(), 2);
    assert!(
        s.attempts
            .iter()
            .all(|a| a.retry_class.as_deref() == Some("engine_stop")
                && a.model_id.as_deref() == Some(MODEL))
    );
    assert!(
        !s.detail.as_deref().unwrap_or_default().contains("Ran on"),
        "{:?}",
        s.detail
    );
    let failover = s.failover.as_ref().unwrap();
    assert_eq!(
        (failover.routes, failover.passes, failover.probes),
        (9, 0, 0)
    );
    let stops = engine_stop_events(&d);
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0]["hint"], "loop-guard");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn engine stop") && l.contains("loop-guard")),
        "{:?}",
        d.log.lines()
    );
    // Whatever Cline left is delivered with the stop.
    assert!(d.aws.get(&keys::bundle_key(RUN)).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mistake_limit_with_failover_routes_also_keeps_the_chosen_model() {
    let d = Direct::start(Opts {
        scenario: "mistake_limit_always",
        edit_cfg: Some(quick_failover()),
        failover: eight_routes(),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let s = d.summary();
    assert_eq!(s.outcome, DirectOutcome::StoppedEngineCancelled);
    assert!(models_asked_of_the_engine(&d).is_empty());
    assert!(s.switches.is_empty());
    assert_eq!(engine_stop_events(&d)[0]["hint"], "mistake-limit");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unexplained_stop_without_a_probe_is_an_engine_stop_and_the_setting_at_zero_resumes_nothing()
 {
    let d = Direct::start(Opts {
        scenario: "cancelled_no_progress",
        edit_cfg: Some(quick_failover()),
        failover: eight_routes(),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received().len(), 2);
    assert_eq!(engine_stop_events(&d)[0]["hint"], "unexplained");
    assert!(models_asked_of_the_engine(&d).is_empty());
    let zero = Direct::start(Opts {
        scenario: "cancelled_no_progress",
        edit_cfg: Some(Box::new(|c| {
            c.direct.provider_retry_pause = Duration::from_millis(20);
            c.direct.engine_stop_resumes = 0;
            c.direct.failover_probe = false;
        })),
        failover: eight_routes(),
        ..Opts::default()
    })
    .await;
    zero.invoke().await;
    assert_eq!(zero.prompts_received().len(), 1);
    assert_eq!(
        zero.summary().outcome,
        DirectOutcome::StoppedEngineCancelled
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_provider_retry_off_cline_s_stop_is_not_resumed_either() {
    // `AGENTD_DIRECT_PROVIDER_RETRIES=0` means one prompt, as before the failover slices; the engine-stop resume follows it.
    let d = Direct::start(Opts {
        scenario: "cancelled_unsolicited",
        edit_cfg: Some(retries(0)),
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    assert_eq!(d.prompts_received().len(), 1);
    assert_eq!(d.summary().outcome, DirectOutcome::StoppedEngineCancelled);
}

// ---- a done the harness could not record ----------------------------------------------------------------------------

/// The harness owns the end state of the run, so a done it could not record reads as the harness's own stop. A directory where `summary.json`
/// goes makes the write fail the way a refused write does.
#[tokio::test(flavor = "multi_thread")]
async fn a_done_whose_summary_cannot_be_written_does_not_read_as_a_done() {
    let d = Direct::start(Opts::default()).await;
    std::fs::create_dir_all(d.agent_data().join("turns/1/summary.json")).unwrap();
    let (_, resp) = d.invoke().await;
    d.wait_healthy().await;
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn summary not written:")),
        "{:?}",
        d.log.lines()
    );
    let ended = d.events_of(EventType::TurnEnded);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["directOutcome"], "stopped_other");
    let label = ended[0]["label"].as_str().unwrap();
    assert!(
        label.starts_with(
            "Cline stopped: the harness ended the run (agentd could not write summary.json"
        ),
        "{label}"
    );
    assert_eq!(resp.outcome, TurnOutcome::Failed);
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    assert_eq!(complete[0].outcome, Some(TurnOutcome::Failed));
    assert!(
        complete[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("summary.json")),
        "{:?}",
        complete[0].error
    );
    assert!(
        d.aws.get(&keys::summary_key(RUN)).is_none(),
        "nothing was uploaded"
    );
}
