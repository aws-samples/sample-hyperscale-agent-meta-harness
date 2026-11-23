//! The worker's own test write to the session's storage: the real write, in a temporary folder.
//!
//! The write that decides whether the storage refuses data is made in the run's own folder and nowhere else: a folder the worker makes
//! and the engine cannot write. The file is created new, without following a link, mode 0600, 4 KiB, synced and removed.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::storage_watch::{HostWrites, StorageWrites, TEST_FILE, test_write_with};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A layout like a session's: `<workspace root>/<run>` with the engine's folder `direct/` inside it.
fn layout() -> (PathBuf, PathBuf) {
    let root = common::scratch("b881-write");
    let run = root.join("01JRUN0000000000000000000A");
    std::fs::create_dir_all(run.join("direct/repo")).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    (root, run)
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn b881_the_test_write_is_made_in_the_runs_folder_only() {
    let (root, run) = layout();
    // What the disk rule's test write would meet in the checkout's parent, which the engine's user owns: a folder, a link and a file at
    // its predictable name. None of it is touched, and none of it changes the answer.
    let engine_dir = run.join("direct");
    let old = format!(".agentd-write-probe-{}", std::process::id());
    std::fs::create_dir(engine_dir.join(&old)).unwrap();
    let outside = root.join("outside.txt");
    std::fs::write(&outside, "untouched").unwrap();
    std::os::unix::fs::symlink(&outside, engine_dir.join(TEST_FILE)).unwrap();
    std::fs::write(
        engine_dir.join("repo").join(TEST_FILE),
        "the engine's own file",
    )
    .unwrap();
    let before_engine = names(&engine_dir);
    let before_root = names(&root);

    HostWrites.test(&run).unwrap();

    assert_eq!(names(&engine_dir), before_engine);
    assert_eq!(names(&root), before_root);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
    assert_eq!(
        std::fs::read_to_string(engine_dir.join("repo").join(TEST_FILE)).unwrap(),
        "the engine's own file"
    );
    assert_eq!(
        names(&run),
        vec!["direct".to_string()],
        "the test file is removed"
    );

    // A run's folder that group or others may write is not the worker's own: it is not tested, and nothing is concluded.
    for mode in [0o775, 0o757, 0o777] {
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(mode)).unwrap();
        let e = HostWrites.test(&run).unwrap_err();
        assert_eq!(e.step, "folder", "{mode:o}");
        assert_eq!(e.errno, None);
        assert!(!e.refused());
        assert_eq!(names(&run), vec!["direct".to_string()]);
    }
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A link in the place of the run's folder is not followed, and a folder that is missing is no refusal.
    let link = root.join("link-to-run");
    std::os::unix::fs::symlink(&run, &link).unwrap();
    assert_eq!(HostWrites.test(&link).unwrap_err().step, "folder");
    let gone = HostWrites.test(&root.join("no-such-run")).unwrap_err();
    assert_eq!(gone.step, "folder");
    assert!(!gone.refused());
}

#[test]
fn b881_the_test_file_is_created_new_without_following_a_link_and_removed() {
    let (root, run) = layout();
    // A leftover of a test that never ended is replaced by a new file.
    std::fs::write(run.join(TEST_FILE), "left over").unwrap();
    HostWrites.test(&run).unwrap();
    assert!(!run.join(TEST_FILE).exists());

    // A link at the file's name is removed as a name, never followed: what it points at is not written.
    let target = root.join("target.txt");
    std::fs::write(&target, "untouched").unwrap();
    std::os::unix::fs::symlink(&target, run.join(TEST_FILE)).unwrap();
    HostWrites.test(&run).unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    assert!(std::fs::symlink_metadata(run.join(TEST_FILE)).is_err());

    // A folder at the file's name cannot be replaced: one log line's worth of error, and no refusal.
    std::fs::create_dir(run.join(TEST_FILE)).unwrap();
    let e = HostWrites.test(&run).unwrap_err();
    assert_eq!(e.step, "leftover");
    assert!(!e.refused());
    std::fs::remove_dir(run.join(TEST_FILE)).unwrap();

    // The mode and the size of the file while it exists: a folder that cannot be written after the create shows them.
    // (The write itself is watched through a folder made read-only after a first run: the create then fails with errno 13, no refusal.)
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(run.join("probe-of-the-test"), "x").is_err() {
        let denied = HostWrites.test(&run).unwrap_err();
        assert!(matches!(denied.step, "create" | "leftover"), "{denied:?}");
        assert_eq!(denied.errno, Some(13));
        assert!(!denied.refused(), "errno 13 is no refusal");
    }
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_file(run.join("probe-of-the-test"));
    // The folder's owner is this process's user, which is what the rule asks.
    assert_eq!(std::fs::metadata(&run).unwrap().uid(), rustix_uid());
}

fn rustix_uid() -> u32 {
    // The effective user of the test process, read from a file it just made.
    let f = common::scratch("b881-uid").join("f");
    std::fs::write(&f, "").unwrap();
    std::fs::metadata(&f).unwrap().uid()
}

/// Only errno 28 from the create, the write, the sync or the rename is a refusal.
#[test]
fn b881_which_errors_are_a_refusal() {
    use agentd::direct::storage_watch::WriteError;
    let e = |step: &'static str, errno: Option<i32>| WriteError {
        step,
        errno,
        text: String::new(),
        test_switch: false,
    };
    for step in ["create", "write", "sync", "rename"] {
        assert!(e(step, Some(28)).refused(), "{step}");
        for other in [30, 13, 21, 5, 122] {
            assert!(!e(step, Some(other)).refused(), "{step} {other}");
        }
        assert!(!e(step, None).refused());
    }
    for step in ["remove", "leftover", "folder"] {
        assert!(!e(step, Some(28)).refused(), "{step}");
    }
}

// ----: the write stays in the folder that was checked --------------------------------------------------------------------------

#[test]
fn b884_the_test_write_stays_in_the_folder_that_was_checked_when_its_path_comes_to_name_another() {
    let (root, run) = layout();
    // Another folder, with a file at the test file's name: what a write by path meets once the path names that folder.
    let other = root.join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(other.join(TEST_FILE), "another folder's file").unwrap();
    let moved = root.join("the-run-moved");
    let swapped = std::cell::Cell::new(false);
    // Between the check of the folder and the write, the checked folder is moved away and a link to the other folder takes its
    // place: the run's path now names the other folder.
    let done = test_write_with(&run, &|| {
        std::fs::rename(&run, &moved).unwrap();
        std::os::unix::fs::symlink(&other, &run).unwrap();
        swapped.set(true);
    });
    assert!(
        swapped.get(),
        "TRIGGER PRESENT is not shown: the seam between the check and the write was not called"
    );
    println!(
        "B884 TRIGGER PRESENT: the run's path named another folder between the check and the write"
    );
    // The other folder was not touched: its file is there with its content, and nothing was added.
    assert_eq!(
        std::fs::read_to_string(other.join(TEST_FILE))
            .ok()
            .as_deref(),
        Some("another folder's file"),
        "the write went to the folder the path named after the check"
    );
    assert_eq!(names(&other), vec![TEST_FILE.to_string()]);
    // The write was made in the folder that was checked: it worked there and left nothing behind.
    done.unwrap();
    assert_eq!(names(&moved), vec!["direct".to_string()]);
}

#[test]
fn b884_a_folder_that_fails_the_rule_is_not_written_to_and_the_seam_is_not_reached() {
    let (root, run) = layout();
    let reached = std::cell::Cell::new(0u32);
    let count = || reached.set(reached.get() + 1);
    // A folder that group or others may write, a link in the folder's place, a file in its place and a missing folder.
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(test_write_with(&run, &count).unwrap_err().step, "folder");
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = root.join("link-to-run");
    std::os::unix::fs::symlink(&run, &link).unwrap();
    let e = test_write_with(&link, &count).unwrap_err();
    assert_eq!(
        (e.step, e.text.as_str()),
        ("folder", "the run's folder is not a folder")
    );
    let file = root.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let e = test_write_with(&file, &count).unwrap_err();
    assert_eq!(
        (e.step, e.text.as_str()),
        ("folder", "the run's folder is not a folder")
    );
    assert_eq!(
        test_write_with(&root.join("no-such-run"), &count)
            .unwrap_err()
            .step,
        "folder"
    );
    assert_eq!(reached.get(), 0);
    assert_eq!(names(&run), vec!["direct".to_string()]);
    // A folder that passes reaches the seam once, and the write works.
    test_write_with(&run, &count).unwrap();
    assert_eq!(reached.get(), 1);
    assert_eq!(names(&run), vec!["direct".to_string()]);
}
