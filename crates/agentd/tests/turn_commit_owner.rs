//! The alignment of staged exec bits (`workspace::apply_staged_exec`) under the uid split: agentd runs as root
//! and the engine user owns the worktree for the turn, so agentd changes the mode only of a file that user
//! owns. The engine could change such a file's mode itself, so whatever name or link leads agentd to an inode,
//! it never gives the engine user a mode change it could not make.
//! Without a split (`set_engine_uid` never called) every file of the worktree is agentd's own.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::workspace::{apply_staged_exec, commit_turn, prepare_local, set_engine_uid};
use common::{git, scratch};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

fn set_mode(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mode_bits(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

async fn staged_wrapper(label: &str) -> agentd::workspace::Worktree {
    let d = scratch(label);
    let src = d.join("app");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("mvnw"), "#!/bin/sh\n").unwrap();
    set_mode(&src.join("mvnw"), 0o644);
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "."]);
    git(&src, &["commit", "-q", "-m", "base"]);
    let wt = prepare_local(&d.join("ws"), "run", "foundation-a1", &src)
        .await
        .unwrap();
    git(&wt.repo, &["update-index", "--chmod=+x", "mvnw"]);
    wt
}

#[tokio::test]
async fn a_file_the_engine_user_owns_gets_its_staged_exec_bit() {
    let wt = staged_wrapper("owner-engine").await;
    let mine = std::fs::metadata(wt.repo.join("mvnw")).unwrap().uid();
    set_engine_uid(&wt, mine);

    assert_eq!(
        apply_staged_exec(&wt).await.unwrap(),
        vec!["mvnw".to_string()]
    );
    assert_eq!(mode_bits(&wt.repo.join("mvnw")), 0o755);
    let head = commit_turn(&wt, 1).await.unwrap();
    assert!(git(&wt.repo, &["ls-tree", &head, "mvnw"]).starts_with("100755 "));
}

#[tokio::test]
async fn a_file_another_user_owns_keeps_its_mode_under_the_uid_split() {
    let wt = staged_wrapper("owner-other").await;
    let mine = std::fs::metadata(wt.repo.join("mvnw")).unwrap().uid();
    set_engine_uid(&wt, mine.wrapping_add(1));

    assert!(apply_staged_exec(&wt).await.unwrap().is_empty());
    assert_eq!(mode_bits(&wt.repo.join("mvnw")), 0o644, "left alone");
    let head = commit_turn(&wt, 1).await.unwrap();
    assert!(
        git(&wt.repo, &["ls-tree", &head, "mvnw"]).starts_with("100644 "),
        "git add -A records the disk mode of a file agentd did not change"
    );
}
