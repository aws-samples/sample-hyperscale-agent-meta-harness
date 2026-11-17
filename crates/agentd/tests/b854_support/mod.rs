//! The stand-in of a read of a stopped session's storage, shared by the tests of the read (`b854_recover.rs`) and of the
//! storage's marker (`b854_marker.rs`).
//!
//! Session 1 of a run works in one agentd (the old machine) and ends; a file is then written and closed in its work tree, after the
//! last stored copy ([`stopped_session`]). A second agentd with a disk of its own and the same storage root is the new machine the
//! platform starts for the stopped session's id: in the test's process ([`new_machine`], [`machine_at`]) or as a real
//! `agentd serve` process whose search path a test can lead with a stand-in program ([`process_machine`]).
#![allow(dead_code, clippy::unwrap_used)]

use super::common::{creds, git};
use super::direct_support::{BUCKET, Direct, Opts, RUN, http, no_secrets};
use agentd::logging::MemoryLog;
use agentd::server::{App, serve};
use agentd::time::{format_rfc3339, now_unix};
use harness_protocol::direct::{
    RecoverKind, RecoveryAnswer, RecoveryBaseline, RecoveryInvocationRequest, s3 as keys,
    validate_recovery_invocation,
};
use harness_protocol::runtime::TurnOutcome;
use serde_json::Value;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const ATTEMPT: &str = "01M4ZX2QEMB4JFCA411Q5169AY";
pub const OTHER_ATTEMPT: &str = "01M4ZX2QEMB4JFCA411Q5169AZ";
pub const LATE_FILE: &str = "late.txt";
pub const LATE_TEXT: &str = "written after the last copy\n";

/// Session 1 of a run after it ended, with one more file on its storage than its last stored copy holds.
pub struct Stopped {
    pub d: Direct,
    /// The run's base.
    pub base: String,
    /// `snapshot.json` of session 1: its newest stored copy.
    pub record: Value,
}

pub async fn stopped_session() -> Stopped {
    let d = Direct::start(Opts::default()).await;
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    d.wait_healthy().await;
    let base = d
        .summary()
        .git
        .base_sha
        .clone()
        .expect("the run has a base");
    let record: Value = serde_json::from_slice(
        &d.aws
            .get(&keys::snapshot_json_key_t(RUN, 1))
            .expect("session 1 stored a copy"),
    )
    .unwrap();
    std::fs::write(d.worktree().join(LATE_FILE), LATE_TEXT).unwrap();
    // In a session's machine agentd makes the run's folder as root with the usual mask: nobody else may write it.
    std::fs::set_permissions(
        d.dir.join("mnt-workspace").join(RUN),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    Stopped { d, base, record }
}

impl Stopped {
    pub fn storage(&self) -> PathBuf {
        self.d.dir.join("mnt-workspace")
    }

    pub fn key(&self) -> String {
        keys::recovery_bundle_key(RUN, 1)
    }

    /// The request the service sends for session 1: the copy goes to the read-back key, and the saved copy is session 1's.
    pub fn request(&self, attempt: &str) -> RecoveryInvocationRequest {
        let req = RecoveryInvocationRequest {
            kind: RecoverKind::Recover,
            attempt_id: attempt.to_string(),
            run_id: RUN.to_string(),
            session: 1,
            runtime_session_id: format!("{RUN}-direct-g1"),
            region: "us-east-1".to_string(),
            credentials: creds(&format_rfc3339(now_unix() + 900)),
            object: keys::uri(BUCKET, &self.key()),
            base_sha: self.base.clone(),
            start_head: self.base.clone(),
            baseline: RecoveryBaseline {
                head_sha: self.record["headSha"].as_str().unwrap().to_string(),
                count: self.record["count"]
                    .as_u64()
                    .map(|n| u32::try_from(n).unwrap()),
                object: Some(keys::uri(BUCKET, &keys::snapshot_bundle_key_t(RUN, 1))),
            },
            budget_s: 60,
        };
        validate_recovery_invocation(&req).expect("the request is one the service may send");
        req
    }

    /// The copy in the bucket, cloned at its `snapshot` branch.
    pub fn stored_tree(&self, label: &str) -> PathBuf {
        let bytes = self.d.aws.get(&self.key()).expect("the read-back object");
        let bundle = self.d.dir.join(format!("{label}.bundle"));
        std::fs::write(&bundle, bytes).unwrap();
        let dest = self.d.dir.join(format!("{label}-clone"));
        git(
            &self.d.dir,
            &[
                "clone",
                "-q",
                "-b",
                "snapshot",
                bundle.to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );
        dest
    }
}

/// A new machine: an agentd of its own, with a disk of its own, that sees the storage at `storage`.
pub struct Machine {
    pub url: String,
    pub log: Arc<MemoryLog>,
    pub data: PathBuf,
    pub runtime: PathBuf,
}

/// The user the storage's folders of a test belong to. The test does not run as the user of a session's machine, so the machines of
/// the tests trust this user's marker.
pub fn storage_owner(s: &Stopped) -> u32 {
    std::fs::metadata(&s.d.dir).unwrap().uid()
}

/// A machine that sees the storage at `storage` and takes a marker of the user `marker_owner` alone.
pub async fn machine_for(s: &Stopped, label: &str, storage: &Path, marker_owner: u32) -> Machine {
    let mut cfg = s.d.app.cfg.clone();
    cfg.workspace_root = storage.to_path_buf();
    cfg.data_root = s.d.dir.join(format!("var-lib-{label}"));
    cfg.runtime_dir = s.d.dir.join(format!("run-{label}"));
    cfg.direct.marker_owner = marker_owner;
    let (data, runtime) = (cfg.data_root.clone(), cfg.runtime_dir.clone());
    let log = MemoryLog::new();
    let app = App::new(cfg, log.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { serve(app, listener).await.unwrap() });
    Machine {
        url: format!("http://{addr}"),
        log,
        data,
        runtime,
    }
}

pub async fn machine_at(s: &Stopped, label: &str, storage: &Path) -> Machine {
    machine_for(s, label, storage, storage_owner(s)).await
}

pub async fn new_machine(s: &Stopped, label: &str) -> Machine {
    machine_at(s, label, &s.storage()).await
}

impl Machine {
    pub async fn post(&self, body: String) -> (u16, String) {
        http("POST", &format!("{}/invocations", self.url), Some(body)).await
    }

    pub async fn recover(&self, req: &RecoveryInvocationRequest) -> RecoveryAnswer {
        let (code, text) = self.post(serde_json::to_string(req).unwrap()).await;
        assert_eq!(code, 200, "{text}");
        no_secrets(&text);
        serde_json::from_str(&text).unwrap()
    }
}

/// A new machine that is a real `agentd serve` process: its disk is its own, its storage is the stopped session's, its store is that
/// session's bucket, and `bin` leads its search path, so a test can put a stand-in for a program there.
pub async fn process_machine(s: &Stopped, bin: &Path) -> Direct {
    let (storage, store) = (s.storage(), s.d.aws.base.clone());
    Direct::start_process(Opts {
        edit_cfg: Some(Box::new(move |cfg| {
            cfg.workspace_root = storage;
            cfg.s3_endpoint = Some(store);
        })),
        agentd_env: vec![("PATH", format!("{}:/usr/bin:/bin", bin.display()))],
        ..Opts::default()
    })
    .await
}

/// Asks a machine that is a process to read the storage back and returns its answer.
pub async fn recover_on(machine: &Direct, req: &RecoveryInvocationRequest) -> RecoveryAnswer {
    let (code, text) = machine.post_raw(serde_json::to_string(req).unwrap()).await;
    assert_eq!(code, 200, "{text}");
    no_secrets(&text);
    serde_json::from_str(&text).unwrap()
}
