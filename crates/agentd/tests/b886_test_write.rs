//! The test write of the disk rule itself (`disk::test_write_with`, `disk::test_write_in`, `disk::probe_name`) and the
//! folders of a run it is made in (`disk::run_write_dirs`).
//!
//! The folder is opened once, a link in its place is not followed, and it must be the worker's own (its user, no write for group or
//! others). The file is made new through that handle, under a name no other write gets, without following a link, for the worker's
//! user alone, and it is removed through the handle. `WriteStep` is how a test looks in: `Checked` when the folder has passed the
//! rule and before the file is touched, `Made` when the file stands and before it is removed. The product gives nothing to run there.
//!
//! Every name in these tests is an ordinary one, and every link is one the test makes in its own scratch folder.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;
mod direct_support;

use agentd::direct::disk::{self, DiskProbe, HostDisk, TestWrite, WriteStep};
use direct_support::{Direct, Opts, RUN};
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A folder of the test's own that neither group nor others may write, whatever the umask of the test run.
fn own(label: &str) -> PathBuf {
    let d = common::scratch(label);
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// What a look at the file found while it stood.
#[derive(Debug, PartialEq, Eq)]
struct Stood {
    name: String,
    folder: Vec<String>,
    bytes: u64,
    mode: u32,
    plain_file: bool,
}

#[test]
fn b886_the_test_write_makes_one_file_of_the_workers_own_in_the_folder_and_leaves_none_behind() {
    let dir = own("b886-land");
    let name = disk::probe_name().unwrap();
    let seen = RefCell::new(Vec::new());
    let r = disk::test_write_with(&dir, &name, &|step| {
        if let WriteStep::Made(n) = step {
            let meta = std::fs::symlink_metadata(dir.join(n)).unwrap();
            seen.borrow_mut().push(Stood {
                name: n.to_string(),
                folder: names_in(&dir),
                bytes: meta.len(),
                mode: meta.permissions().mode() & 0o7777,
                plain_file: meta.file_type().is_file(),
            });
        }
    });
    assert!(matches!(r, TestWrite::Took), "{r:?}");
    assert_eq!(
        seen.into_inner(),
        [Stood {
            name: name.clone(),
            folder: vec![name],
            bytes: 4096,
            mode: 0o600,
            plain_file: true,
        }]
    );
    assert!(names_in(&dir).is_empty(), "the test write left a file");
}

#[test]
fn b886_every_test_write_has_a_name_no_other_write_gets() {
    let mut names: Vec<String> = (0..64).map(|_| disk::probe_name().unwrap()).collect();
    for n in &names {
        let tail = n
            .strip_prefix(".agentd-write-probe-")
            .unwrap_or_else(|| panic!("{n}"));
        assert_eq!(tail.len(), 32, "{n}");
        assert!(tail.bytes().all(|c| c.is_ascii_hexdigit()), "{n}");
    }
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 64, "two test writes got one name");
}

/// The folder is checked once and the write stays in it, whatever its path comes to name afterwards.
#[test]
fn b886_the_write_stays_in_the_folder_that_was_checked() {
    let base = own("b886-stay");
    let (first, moved, other) = (base.join("run"), base.join("moved"), base.join("other"));
    for d in [&first, &other] {
        std::fs::create_dir(d).unwrap();
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let seen = RefCell::new(None);
    let r = disk::test_write_with(&first, "probe-of-the-test", &|step| match step {
        // The path is given to another folder between the check and the write.
        WriteStep::Checked => {
            std::fs::rename(&first, &moved).unwrap();
            std::os::unix::fs::symlink(&other, &first).unwrap();
        }
        WriteStep::Made(n) => {
            *seen.borrow_mut() = Some((moved.join(n).is_file(), names_in(&other)));
        }
    });
    assert!(matches!(r, TestWrite::Took), "{r:?}");
    assert_eq!(
        seen.into_inner(),
        Some((true, Vec::new())),
        "the file was not made in the folder that was checked"
    );
    assert!(names_in(&moved).is_empty());
    assert!(names_in(&other).is_empty());
}

/// A name that is taken is not written: the file is made new or not at all, and a link under the name is not followed.
#[test]
fn b886_a_name_that_is_taken_is_not_written_and_a_link_there_is_not_followed() {
    let dir = own("b886-taken");
    let kept = own("b886-taken-kept").join("kept.txt");
    std::fs::write(&kept, b"kept").unwrap();
    std::os::unix::fs::symlink(&kept, dir.join("probe-of-the-test")).unwrap();
    let made = RefCell::new(0u32);
    let r = disk::test_write_with(&dir, "probe-of-the-test", &|step| {
        if matches!(step, WriteStep::Made(_)) {
            *made.borrow_mut() += 1;
        }
    });
    assert!(matches!(r, TestWrite::NotTried(_)), "{r:?}");
    assert_eq!(made.into_inner(), 0);
    assert_eq!(std::fs::read(&kept).unwrap(), b"kept");
    assert!(dir.join("probe-of-the-test").is_symlink());
    // A file under the name is left as it is too.
    std::fs::write(dir.join("probe-of-the-test-2"), b"mine").unwrap();
    let r = disk::test_write_with(&dir, "probe-of-the-test-2", &|_| {});
    assert!(matches!(r, TestWrite::NotTried(_)), "{r:?}");
    assert_eq!(
        std::fs::read(dir.join("probe-of-the-test-2")).unwrap(),
        b"mine"
    );
}

/// The three results: a folder of the worker's own takes the write; a folder that passes the rule and takes no new file refuses it,
/// which is what a read-only file system does; a folder that is not the worker's own is not tried.
#[test]
fn b886_a_test_write_took_was_refused_or_was_not_tried() {
    let ok = own("b886-three");
    assert!(matches!(disk::test_write_in(&ok), TestWrite::Took));
    // The folder of this process in /proc: a real folder of this user that nobody else may write, and it takes no new file.
    let no_new_file = PathBuf::from(format!("/proc/{}", std::process::id()));
    let r = disk::test_write_in(&no_new_file);
    assert!(matches!(r, TestWrite::Refused(_)), "{r:?}");
    assert!(disk::test_write(&no_new_file).is_err());
    // A link in a folder's place, a folder that others may write, a plain file and a path that names nothing.
    let link = own("b886-three-links").join("to-ok");
    std::os::unix::fs::symlink(&ok, &link).unwrap();
    let open = common::scratch("b886-three-open");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let file = ok.join("a-file");
    std::fs::write(&file, b"x").unwrap();
    for dir in [&link, &open, &file, &ok.join("not-there")] {
        let r = disk::test_write_in(dir);
        assert!(
            matches!(r, TestWrite::NotTried(_)),
            "{}: {r:?}",
            dir.display()
        );
    }
    assert_eq!(names_in(&ok), ["a-file"]);
    assert!(names_in(&open).is_empty());
}

/// The machine's report says for each folder it was given what its test write came to.
#[tokio::test]
async fn b886_the_machines_report_says_what_each_test_write_came_to() {
    let ok = own("b886-report");
    let link = own("b886-report-links").join("to-ok");
    std::os::unix::fs::symlink(&ok, &link).unwrap();
    let no_new_file = PathBuf::from(format!("/proc/{}", std::process::id()));
    let host = HostDisk::new(&ok, &ok).with_write_dirs([link.clone(), no_new_file.clone()]);
    let text = host.diagnose().await;
    assert!(
        text.contains(&format!("write test {}: ok\n", ok.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "write test {}: not tried (the folder is not a folder)\n",
            link.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("write test {}: failed: ", no_new_file.display())),
        "{text}"
    );
    assert!(names_in(&ok).is_empty());
}

/// The folders of a run that get the test write besides the two roots: the run's own folder on the root volume and the run's own
/// folder on the session's storage. The worker makes both and hands neither to the engine. The engine's state folder and the
/// parent of the checkout, which the engine writes, are on the same two file systems and are not in the list.
#[tokio::test(flavor = "multi_thread")]
async fn b886_the_folders_of_a_runs_test_write_are_the_runs_own_on_both_disks() {
    let d = Direct::start(Opts::default()).await;
    d.invoke().await;
    let agent_data = d.agent_data();
    let workspace_root = d.app.cfg.workspace_root.clone();
    let dirs = disk::run_write_dirs(&agent_data, &workspace_root, RUN);
    assert_eq!(dirs, [agent_data.clone(), workspace_root.join(RUN)]);
    // Both exist in a real run and take the test write.
    for dir in &dirs {
        assert!(dir.is_dir(), "{}", dir.display());
        let r = disk::test_write_in(dir);
        assert!(matches!(r, TestWrite::Took), "{}: {r:?}", dir.display());
    }
    // The folders the engine is given are inside them and are not in the list.
    let engine_state = agent_data.join("data");
    let checkout_parent = workspace_root.join(RUN).join("direct");
    assert!(engine_state.is_dir() && checkout_parent.is_dir());
    assert!(!dirs.contains(&engine_state) && !dirs.contains(&checkout_parent));
    d.wait_healthy().await;
}

/// Which folders a session gives the test write, read from the session's own words. After an engine error that names no cause the
/// worker writes what the machine reports to its log, with one `write test <folder>:` line for each folder of the session's disk
/// check. They are the two roots and the run's own folder on each disk, and no folder the engine writes.
#[tokio::test(flavor = "multi_thread")]
async fn b886_a_session_gives_the_test_write_the_two_roots_and_the_runs_own_folders() {
    let d = Direct::start(Opts {
        scenario: "internal_error_then_done",
        ..Opts::default()
    })
    .await;
    d.invoke().await;
    let workspace_root = d.app.cfg.workspace_root.clone();
    let log = d.log.lines().join("\n");
    let report = log
        .split("names no cause; the machine reports:")
        .nth(1)
        .unwrap_or_else(|| panic!("the session wrote no report of the machine: {log}"));
    let mut folders: Vec<PathBuf> = report
        .lines()
        .filter_map(|l| l.strip_prefix("write test "))
        .filter_map(|l| l.split_once(": "))
        .map(|(folder, _)| PathBuf::from(folder))
        .collect();
    folders.sort();
    let mut want = vec![
        d.app.cfg.data_root.clone(),
        workspace_root.clone(),
        d.agent_data(),
        workspace_root.join(RUN),
    ];
    want.sort();
    assert_eq!(folders, want, "{report}");
    d.wait_healthy().await;
}

/// The run's own two folders get the test write whatever umask the worker was started with. A folder that carries a write bit for its
/// group, which is what a umask of 0002 leaves, loses the bit when the session starts.
#[tokio::test(flavor = "multi_thread")]
async fn b886_the_runs_two_folders_made_under_a_umask_of_0002_get_the_test_write() {
    let d = Direct::start(Opts::default()).await;
    let dirs = disk::run_write_dirs(&d.agent_data(), &d.app.cfg.workspace_root, RUN);
    for dir in &dirs {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            matches!(disk::test_write_in(dir), TestWrite::NotTried(_)),
            "the test proves nothing: {} takes the test write as it is",
            dir.display()
        );
    }
    d.invoke().await;
    for dir in &dirs {
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755, "{}: {mode:o}", dir.display());
        let r = disk::test_write_in(dir);
        assert!(matches!(r, TestWrite::Took), "{}: {r:?}", dir.display());
    }
    d.wait_healthy().await;
}
