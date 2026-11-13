// COMPAT-CAH-DATA: this file names the earlier variable spellings that the engine environment carries beside the new ones until VERSION 6.00.
//! The tool hook writes the engine's registration: after it has bound the server's port it renders the
//! profile's template, removes the stale lock and temporary files next to the target, gives the file and its directory to the engine's user,
//! and writes it again at the end of a session or an engine stop when the engine edited or deleted it. The tests drive the module through
//! `ToolsHook::start_with` and the driver's events, with the real `[mcp]` table and template of the `cline` profile and the echo backend.
//!
//! Cline re-reads the file at each new session and rewrites it after every connect, so the file is the part of the
//! engine's configuration that the engine owns and that agentd keeps right. The engine runs as the same user as the file's owner and has a
//! shell, so nothing here is a boundary against the engine; what is a boundary is that agentd, which runs as root, never writes through a link
//! the engine planted, and the tests plant them.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod hook_support;
mod tools_support;

use agentd::direct::hooks::{EngineStop, SessionEnd};
use agentd::direct::tools::ToolsHook;
use agentd::direct::tools::settings::Settings;
use hook_support::{
    FILE, Rig, engine_stopped, exists, prompt_sent, session_ended, session_started,
};
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tools_support::echo::Echo;
use tools_support::{refuses, request, rpc, text_of};

fn any_port() -> Settings {
    Settings {
        port: 0,
        ..Settings::default()
    }
}

/// An owner for the file that is not what the process would give it by itself, so that the call which hands the file over is visible: this
/// process's user, and the first supplementary group that is not its primary one. As root any ids do.
fn other_owner() -> (u32, u32) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let ids = |key: &str| -> Vec<u32> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|g| {
                g.split_whitespace()
                    .filter_map(|x| x.parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let (uid, gid) = (ids("Uid:")[1], ids("Gid:")[1]);
    if uid == 0 {
        return (64_321, 64_322);
    }
    let gid = ids("Groups:")
        .into_iter()
        .find(|g| *g != gid)
        .unwrap_or(gid);
    (uid, gid)
}

fn hook_text(rig: &Rig) -> String {
    rig.file_text()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_file_is_rendered_after_the_port_is_bound_and_given_to_the_engine() {
    let owner = other_owner();
    let mut rig = Rig::with_owner(Some(owner)).await;
    let dir = rig.settings_dir();
    // What Cline leaves behind: its lock (a directory, as the lock library makes it), a lock file, a temporary file; and files that are
    // not its registration's and must stay.
    std::fs::create_dir(dir.join(format!("{FILE}.lock"))).unwrap();
    std::fs::write(dir.join(format!("{FILE}.lock.old")), "x").unwrap();
    std::fs::write(dir.join(format!("{FILE}.tmp.4242")), "x").unwrap();
    std::fs::write(dir.join("other_settings.json.lock"), "keep").unwrap();
    std::fs::write(dir.join(format!("{FILE}.keep")), "keep").unwrap();

    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;

    let doc: Value = serde_json::from_str(&hook_text(&rig)).unwrap();
    let port = hook.port().expect("the server is bound");
    let entry = &doc["mcpServers"]["ham"];
    assert_eq!(
        entry["transport"],
        json!({"type": "streamableHttp", "url": format!("http://127.0.0.1:{port}/mcp")}),
        "the file names the port the module bound"
    );
    assert_eq!(entry["timeout"], json!(120));
    assert_eq!(doc["mcpServers"].as_object().unwrap().len(), 1);
    assert_eq!(
        rig.engine_env.get("HAMH_TOOLS_URL").map(String::as_str),
        Some(format!("http://127.0.0.1:{port}/mcp").as_str()),
        "the engine's environment names the same address"
    );
    assert_eq!(
        rig.engine_env.get("CAH_TOOLS_URL"), // COMPAT-CAH-DATA: the earlier spelling, for rules the user edited before the rename
        rig.engine_env.get("HAMH_TOOLS_URL"),
        "the earlier spelling names the same address"
    );
    // The server behind that address is up and is this module's (the file was written after the self check, not before).
    let answer = rpc(rig.addr(), &request(json!(1), "ping", json!({}))).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    // The stale files are gone, the others are not.
    assert!(
        !exists(&dir.join(format!("{FILE}.lock"))),
        "the lock directory"
    );
    assert!(!exists(&dir.join(format!("{FILE}.lock.old"))));
    assert!(!exists(&dir.join(format!("{FILE}.tmp.4242"))));
    assert!(exists(&dir.join("other_settings.json.lock")));
    assert!(exists(&dir.join(format!("{FILE}.keep"))));
    assert!(exists(&dir.join("providers.json")));
    // Owned by the engine's user: the file and the directory it sits in.
    let (file, parent) = (
        std::fs::metadata(rig.file()).unwrap(),
        std::fs::metadata(&dir).unwrap(),
    );
    assert_eq!((file.uid(), file.gid()), owner, "the file");
    assert_eq!((parent.uid(), parent.gid()), owner, "its directory");
    assert!(file.file_type().is_file());
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_asks_for_nothing_the_template_does_not_say() {
    // The rendered file is the template with its two placeholders filled and nothing else: the same bytes as the profile's template read
    // and replaced by hand.
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let template = std::fs::read_to_string(
        common::repo_root().join("engine/profiles/templates/mcp-cline.json"),
    )
    .unwrap();
    let want = template.replace("{toolsName}", "ham").replace(
        "{toolsUrl}",
        &format!("http://127.0.0.1:{}/mcp", rig.file_port()),
    );
    assert_eq!(rig.file_text(), want);
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_in_use_falls_back_and_the_file_names_the_new_port() {
    let mut rig = Rig::new().await;
    // Something else holds the port the module was asked for.
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = taken.local_addr().unwrap().port();
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(Settings {
        port: busy,
        ..Settings::default()
    });
    rig.start(&hook, vec![echo]).await;
    let port = rig.file_port();
    assert_ne!(
        port, busy,
        "the file must not name a port that another process holds"
    );
    assert_eq!(hook.port(), Some(port));
    let answer = rpc(rig.addr(), &request(json!(1), "ping", json!({}))).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        rig.engine_env.get("HAMH_TOOLS_URL").unwrap(),
        &format!("http://127.0.0.1:{port}/mcp")
    );
    drop(taken);
    hook.stop_for_tests().await;
}

/// A port that was free a moment ago, so that a test can see later whether the module left a server on it.
async fn a_free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn render_failure_adds_no_tool_and_the_run_goes_on() {
    // Four ways the file cannot be written; in each the module answers Ok (the run goes on), logs one line, leaves no server running, sets
    // no variable and says in the summary note why it is off.
    for case in [
        "not-a-directory",
        "linked-directory",
        "no-template",
        "bad-template",
    ] {
        let mut rig = Rig::new().await;
        let settings_dir = rig.settings_dir();
        let elsewhere = common::scratch("elsewhere");
        match case {
            // The settings directory is a file.
            "not-a-directory" => {
                std::fs::remove_dir_all(&settings_dir).unwrap();
                std::fs::write(&settings_dir, "i am a file").unwrap();
            }
            // The settings directory is a link: agentd writes nothing through it.
            "linked-directory" => {
                std::fs::remove_dir_all(&settings_dir).unwrap();
                std::os::unix::fs::symlink(&elsewhere, &settings_dir).unwrap();
            }
            // The templates root does not hold the profile's template.
            "no-template" => {
                rig.mcp.template = Some("engine/profiles/templates/nope.json".to_string());
            }
            // The template is not JSON once it is filled.
            "bad-template" => {
                let root = common::scratch("bad-templates");
                std::fs::create_dir_all(root.join("engine/profiles/templates")).unwrap();
                std::fs::write(
                    root.join("engine/profiles/templates/mcp-cline.json"),
                    "{ not json {toolsName} {toolsUrl}",
                )
                .unwrap();
                rig.set_templates_root(&root);
            }
            _ => unreachable!(),
        }
        let busy = a_free_port().await;
        let (echo, _) = Echo::new();
        let hook = ToolsHook::new(Settings {
            port: busy,
            ..Settings::default()
        });
        rig.start(&hook, vec![echo]).await;

        assert!(
            !rig.engine_env.contains_key("HAMH_TOOLS_URL")
                && !rig.engine_env.contains_key("CAH_TOOLS_URL"), // COMPAT-CAH-DATA
            "{case}: no variable"
        );
        assert!(
            refuses(format!("127.0.0.1:{busy}").parse().unwrap()).await,
            "{case}: no server left on the port"
        );
        let note = hook.note().expect("the module decided: off");
        assert!(!note.enabled, "{case}");
        assert!(
            note.counts.is_none() && note.list.is_empty(),
            "{case}: {note:?}"
        );
        let reason = note.reason.clone().expect("a reason word");
        assert!(
            reason.chars().all(|c| c.is_ascii_lowercase() || c == '-') && reason.len() <= 40,
            "{case}: {reason}"
        );
        let lines = rig.tools_log();
        assert_eq!(
            lines.iter().filter(|l| l.contains("not started")).count(),
            1,
            "{case}: one line, {lines:?}"
        );
        assert!(
            !exists(&settings_dir.join(FILE)),
            "{case}: no registration file"
        );
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "{case}: nothing was written through the link"
        );
        hook.stop_for_tests().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edited_or_deleted_file_is_restored_before_the_next_session() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let right = rig.file_text();
    session_started(&hook, "s1", 1);

    // The engine edits the file (Cline rewrites it after a connect), and the session ends: the file is back.
    std::fs::write(rig.file(), "{\"mcpServers\":{}}\n").unwrap();
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    assert_eq!(rig.file_text(), right, "an edit is undone at SessionEnded");

    // The engine deletes it, and the engine stops: the file is back.
    std::fs::remove_file(rig.file()).unwrap();
    engine_stopped(&hook, EngineStop::Restarted);
    assert_eq!(
        rig.file_text(),
        right,
        "a deletion is undone at EngineStopped"
    );

    // Both events do the same, so a deletion is undone at SessionEnded and an edit at EngineStopped.
    std::fs::remove_file(rig.file()).unwrap();
    session_ended(&hook, "s1", 1, SessionEnd::RunEnded);
    assert_eq!(rig.file_text(), right);
    std::fs::write(rig.file(), "x").unwrap();
    engine_stopped(&hook, EngineStop::Reaped);
    assert_eq!(rig.file_text(), right);

    // An unchanged file is not written again: its modification time stays.
    let before = std::fs::metadata(rig.file()).unwrap().modified().unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    engine_stopped(&hook, EngineStop::Restarted);
    let after = std::fs::metadata(rig.file()).unwrap().modified().unwrap();
    assert_eq!(before, after, "a right file is left alone");
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restored_file_belongs_to_the_engine_again() {
    let owner = other_owner();
    let mut rig = Rig::with_owner(Some(owner)).await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    std::fs::remove_file(rig.file()).unwrap();
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    let md = std::fs::metadata(rig.file()).unwrap();
    assert_eq!(
        (md.uid(), md.gid()),
        owner,
        "the rewritten file is handed over too"
    );
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_planted_by_the_engine_is_never_written_through() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let right = rig.file_text();
    let outside = common::scratch("victim");
    let victim = outside.join("victim.txt");
    std::fs::write(&victim, "precious").unwrap();

    // The file is replaced by a link to a file outside: the link goes, the target is untouched.
    std::fs::remove_file(rig.file()).unwrap();
    std::os::unix::fs::symlink(&victim, rig.file()).unwrap();
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    assert!(
        !std::fs::symlink_metadata(rig.file())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(rig.file_text(), right);

    // The file is replaced by a directory: it is cleared and the file is back.
    std::fs::remove_file(rig.file()).unwrap();
    std::fs::create_dir(rig.file()).unwrap();
    engine_stopped(&hook, EngineStop::Restarted);
    assert_eq!(rig.file_text(), right);

    // A temporary file the engine linked at the name agentd would use is not followed either.
    for name in [format!("{FILE}.agentd-new"), format!("{FILE}.tmp.1")] {
        std::os::unix::fs::symlink(&victim, rig.settings_dir().join(&name)).unwrap();
    }
    std::fs::remove_file(rig.file()).unwrap();
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    assert_eq!(rig.file_text(), right);

    // The settings directory itself is swapped for a link to another directory: nothing is written into the other directory.
    let other = common::scratch("other-dir");
    let real = rig.data_dir.join("settings-real");
    std::fs::rename(rig.settings_dir(), &real).unwrap();
    std::os::unix::fs::symlink(&other, rig.settings_dir()).unwrap();
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    engine_stopped(&hook, EngineStop::Restarted);
    assert!(
        std::fs::read_dir(&other).unwrap().next().is_none(),
        "nothing went through the swapped directory"
    );
    assert!(
        rig.tools_log()
            .iter()
            .any(|l| l.contains("could not be restored")),
        "{:?}",
        rig.tools_log()
    );
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_port_in_the_file_stays_valid_across_an_engine_restart() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let (port, text) = (rig.file_port(), rig.file_text());
    session_started(&hook, "s1", 1);
    // A fresh session: the old session ends, the engine's process group is killed and started again, the new session begins.
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    engine_stopped(&hook, EngineStop::Restarted);
    session_started(&hook, "s2", 2);
    assert_eq!(rig.file_port(), port, "the same server");
    assert_eq!(rig.file_text(), text, "the same file");
    assert_eq!(hook.port(), Some(port));
    // The server did not notice: the same call works on a fresh connection, and its counters are the same server's.
    let answer = rpc(rig.addr(), &request(json!(7), "tools/list", json!({}))).await;
    assert_eq!(answer.status, 200);
    let called = rpc(rig.addr(), &tools_support::search(json!(8), "echo:again")).await;
    assert_eq!(text_of(&called.json()), "echo: again");
    assert_eq!(hook.stats().unwrap().restarts, 0);
    hook.stop_for_tests().await;
}

#[test]
fn the_module_writes_no_file_outside_the_registration_paths() {
    // The rendered path comes from the profile and is relative to the engine's own directories: the helper that resolves it refuses anything
    // else, whatever a profile says (the profile loader refuses these too; this is the second check, at the place that writes).
    use agentd::direct::tools::register::target;
    let (data, config) = (Path::new("/data"), Path::new("/config"));
    assert!(target("{dataDir}/settings/cline_mcp_settings.json", data, config).is_ok());
    assert!(target("{configDir}/mcp.json", data, config).is_ok());
    for bad in [
        "/etc/cline_mcp_settings.json",
        "{dataDir}/../escape.json",
        "{dataDir}/a//b.json",
        "{workspace}/mcp.json",
        "{dataDir}/",
        "{dataDir}",
        "relative/mcp.json",
        "{dataDir}/settings/./x.json",
    ] {
        assert!(target(bad, data, config).is_err(), "{bad}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_prompt_of_a_session_puts_the_file_right_before_the_engine_reads_it() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let right = rig.file_text();
    // A process of the old engine that outlived the group kill rewrites the file after the engine stopped; the next session's first prompt is
    // what makes the engine read it, and the module is told just before that prompt is written.
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    engine_stopped(&hook, EngineStop::Restarted);
    std::fs::write(rig.file(), "{\"mcpServers\":{}}\n").unwrap();
    session_started(&hook, "s2", 2);
    assert_ne!(
        rig.file_text(),
        right,
        "the file is damaged until the prompt"
    );
    prompt_sent(&hook, "s2", 2);
    assert_eq!(rig.file_text(), right);
    // The second prompt of the same session is not a connect: the file is left as it is.
    std::fs::write(rig.file(), "x").unwrap();
    prompt_sent(&hook, "s2", 3);
    assert_eq!(rig.file_text(), "x");
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_the_engine_made_unreadable_or_gave_to_another_group_is_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let owner = other_owner();
    let mut rig = Rig::with_owner(Some(owner)).await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let right = rig.file_text();
    // The same bytes, but the engine's own user cannot write it (it rewrites the file after every connect) or cannot read it any more.
    for mode in [0o444, 0o200, 0o000] {
        std::fs::set_permissions(rig.file(), std::fs::Permissions::from_mode(mode)).unwrap();
        session_ended(&hook, "s1", 1, SessionEnd::Replaced);
        let md = std::fs::metadata(rig.file()).unwrap();
        assert_eq!(
            md.permissions().mode() & 0o600,
            0o600,
            "mode {mode:o} was put right: readable and writable by its owner"
        );
        assert_eq!(rig.file_text(), right, "mode {mode:o}");
    }
    // The same bytes, owned by a group that is not the engine's (a process of this user can pick any group it is in).
    let me = std::fs::metadata(rig.settings_dir()).unwrap();
    let other_gid = {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        status
            .lines()
            .find_map(|l| l.strip_prefix("Groups:"))
            .map(|g| {
                g.split_whitespace()
                    .filter_map(|x| x.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into_iter()
            .find(|g| *g != owner.1)
    };
    if let Some(gid) = other_gid.filter(|_| me.uid() != 0) {
        std::os::unix::fs::chown(rig.file(), None, Some(gid)).unwrap();
        session_ended(&hook, "s1", 1, SessionEnd::Replaced);
        assert_eq!(
            std::fs::metadata(rig.file()).unwrap().gid(),
            owner.1,
            "handed back to the engine's group"
        );
    }
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_is_removed_once_the_engine_is_gone_and_not_while_it_may_still_run() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    let lock = rig.settings_dir().join(format!("{FILE}.lock"));
    std::fs::create_dir(&lock).unwrap();
    // A session ended but the engine runs on: the lock may be its own, mid-update.
    session_ended(&hook, "s1", 1, SessionEnd::Replaced);
    assert!(
        exists(&lock),
        "an engine that may be running keeps its lock"
    );
    // Its process group was killed: the lock is stale, and the next session's first prompt would wait for it.
    engine_stopped(&hook, EngineStop::Restarted);
    assert!(!exists(&lock));
    assert!(
        rig.tools_log()
            .iter()
            .any(|l| l.contains("1 stale file(s) removed")),
        "{:?}",
        rig.tools_log()
    );
    hook.stop_for_tests().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_the_engine_filled_is_scanned_for_a_bounded_number_of_entries() {
    let mut rig = Rig::new().await;
    let (echo, _) = Echo::new();
    let hook = ToolsHook::new(any_port());
    rig.start(&hook, vec![echo]).await;
    for n in 0..6_000 {
        std::fs::write(rig.settings_dir().join(format!("{FILE}.tmp.{n}")), "").unwrap();
    }
    let t = std::time::Instant::now();
    engine_stopped(&hook, EngineStop::Reaped);
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    let left = std::fs::read_dir(rig.settings_dir())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        })
        .count();
    // One pass looks at 4,096 entries: most of the 6,000 are gone, not all of them.
    assert!(left > 1_000 && left < 2_100, "left {left}");
    hook.stop_for_tests().await;
}

#[test]
fn a_template_that_renders_longer_than_the_file_limit_is_refused() {
    use agentd::direct::tools::register::{MAX_RENDERED, render};
    let fits = format!(
        "{{\"a\": \"{{toolsName}} {{toolsUrl}}{}\"}}",
        "x".repeat(MAX_RENDERED / 2)
    );
    assert!(render(&fits, "ham", "http://127.0.0.1:1/mcp").is_ok());
    let big = format!(
        "{{\"a\": \"{{toolsName}} {{toolsUrl}}{}\"}}",
        "x".repeat(MAX_RENDERED)
    );
    assert!(render(&big, "ham", "http://127.0.0.1:1/mcp").is_err());
    // A placeholder left over, or text that is not JSON, is refused too.
    assert!(
        render("{\"a\": \"{toolsNope}\"}", "ham", "u").is_err(),
        "a placeholder the module does not fill"
    );
    assert!(render("{\"a\": \"{toolsName\"", "ham", "u").is_err());
    assert!(render("not json {toolsName}", "ham", "u").is_err());
}

#[test]
fn a_child_of_a_tool_gets_the_engine_user_the_work_tree_and_almost_no_environment() {
    use agentd::direct::hooks::{CLEAN_ENV_KEEP, CleanLaunch};
    use std::collections::BTreeMap;
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in [
        ("HOME", "/home/agent"),
        ("TMPDIR", "/tmp/agent"),
        ("USER", "agent"),
        ("LANG", "C.UTF-8"),
        ("AWS_ACCESS_KEY_ID", "not-a-key"),
        ("AWS_CONFIG_FILE", "/x/aws/config"),
        ("AWS_PROFILE", "harness"),
        ("AWS_EC2_METADATA_DISABLED", "true"),
        ("CLINE_MODEL", "m"),
        ("HAMH_RESEARCH_URL", "http://127.0.0.1:3129"),
        ("HAMH_TOOLS_URL", "http://127.0.0.1:3130/mcp"),
        ("HTTPS_PROXY", "http://proxy"),
        ("PIP_INDEX_URL", "http://127.0.0.1:3128/simple"),
        ("SOME_SECRET_TOKEN", "x"),
    ] {
        env.insert(k.to_string(), v.to_string());
    }
    let launch = CleanLaunch::new(
        &env,
        PathBuf::from("/work/tree"),
        vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")],
        PathBuf::from("/agent/data"),
        Some((901, 901)),
    );
    let spec = launch.spec(vec!["rg".to_string(), "x".to_string()], "tool-stderr.log");
    let keys: Vec<&str> = spec.env.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["HOME", "LANG", "TMPDIR", "USER"],
        "only the allow-list, and only what the engine had"
    );
    assert!(keys.iter().all(|k| CLEAN_ENV_KEEP.contains(k)));
    assert!(!spec.env.keys().any(|k| k.starts_with("AWS_")));
    assert_eq!(spec.cwd, PathBuf::from("/work/tree"));
    assert_eq!(spec.run_as, Some((901, 901)));
    assert_eq!(
        spec.path,
        [PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")]
    );
    assert_eq!(
        spec.stderr_log,
        PathBuf::from("/agent/data/tool-stderr.log")
    );
    assert_eq!(spec.argv, ["rg", "x"]);
}
