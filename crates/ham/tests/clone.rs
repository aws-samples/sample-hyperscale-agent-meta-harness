//! `ham clone`: the delivered zip's `repo.bundle` becomes a local git repository
//! with no git host and no git credential. These tests run the library functions against a real tiny repository, its real
//! bundle and a zip laid out like the packager's: complete and partial zips, a corrupt bundle, a destination that is not
//! empty, git missing, a download that resumes, and a tree that does not equal the zip's `src/`.
#![allow(clippy::unwrap_used)]

mod clonefix;
mod support;

use clonefix::{
    Entry, Opts, RUN, assert_tree, build, build_custom, build_opts, build_with, git, git_os,
    staging_left,
};
use ham::api::{Api, Retry};
use ham::clone::{
    Blob, CloneError, Git, Request, RunRequest, blob_id, check_clean, check_worktree, clone_run,
    clone_zip, default_dir_name, git_mode,
};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::{Fake, REGION, creds};

fn req<'a>(zip: &'a Path, dest: Option<&'a Path>, cwd: &'a Path) -> Request<'a> {
    Request {
        run_id: RUN,
        zip,
        dest,
        cwd,
        partial: false,
    }
}

#[test]
fn a_complete_zip_becomes_a_repository_whose_tree_equals_src() {
    let fx = build("clone-ok", RUN);
    let cwd = support::scratch("clone-ok-cwd");
    let out = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("work/calc")), &cwd),
    )
    .unwrap();
    assert_eq!(out.dir, cwd.join("work/calc"));
    assert_eq!(out.branch, format!("harness/{RUN}"));
    assert_eq!(out.head, fx.head);
    assert_eq!(out.commits, 2);
    assert_eq!(out.files, fx.items.len());
    assert_eq!(out.top, fx.top);
    assert!(
        !out.partial && out.warnings.is_empty(),
        "{:?}",
        out.warnings
    );

    let d = &out.dir;
    assert_eq!(
        git(d, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/harness/{RUN}")
    );
    assert_eq!(git(d, &["rev-parse", "HEAD"]), fx.head);
    assert_eq!(git(d, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(git(d, &["remote"]), "", "no remote");
    assert_eq!(git(d, &["status", "--porcelain"]), "", "clean checkout");
    assert!(
        clonefix::git_in(d, &["config", "--get-regexp", "^(remote|branch)\\."]).is_err(),
        "no remote or upstream configuration is left"
    );
    assert_eq!(git(d, &["for-each-ref", "refs/remotes"]), "");
    assert_tree(d, &fx.items);
    assert!(!d.join(".git/FETCH_HEAD").exists());
    let reflog = git(d, &["reflog", "--all"]);
    assert!(
        !reflog.contains(".ham-clone"),
        "the reflog names the staging directory: {reflog}"
    );
    // The staging directory is gone, and the destination's parent holds only the clone.
    assert_eq!(staging_left(&cwd.join("work")), Vec::<String>::new());
}

#[test]
fn the_default_directory_is_the_repo_name_the_zip_was_named_for() {
    assert_eq!(
        default_dir_name(&format!("python-calc-{RUN}-abc1234"), RUN),
        "python-calc"
    );
    assert_eq!(
        default_dir_name(&format!("my-app-v2-{RUN}-0123abc"), RUN),
        "my-app-v2"
    );
    assert_eq!(
        default_dir_name(&format!("a.b_c-{RUN}-abc1234"), RUN),
        "a.b_c"
    );
    // Anything that is not a plain single path component falls back to the run id.
    for top in [
        "weird".to_string(),
        format!("-{RUN}-abc1234"),
        format!("a b-{RUN}-abc1234"),
        format!("..-{RUN}-abc1234"),
        format!(".-{RUN}-abc1234"),
        format!("a/b-{RUN}-abc1234"),
        format!("caf\u{e9}-{RUN}-abc1234"),
    ] {
        assert_eq!(default_dir_name(&top, RUN), RUN, "{top}");
    }
    // Without a destination the clone lands in ./<repo>.
    let fx = build("clone-default", RUN);
    let cwd = support::scratch("clone-default-cwd");
    let out = clone_zip(&Git::system(), &req(&fx.zip, None, &cwd)).unwrap();
    assert_eq!(out.dir, cwd.join("python-calc"));
}

#[test]
fn a_partial_zip_clones_the_same_way_and_says_it_is_partial() {
    let fx = build("clone-partial", RUN);
    let cwd = support::scratch("clone-partial-cwd");
    let mut r = req(&fx.zip, Some(Path::new("p")), &cwd);
    r.partial = true;
    let out = clone_zip(&Git::system(), &r).unwrap();
    assert!(out.partial);
    assert_tree(&out.dir, &fx.items);
}

#[test]
fn a_bundle_that_does_not_verify_is_refused_and_nothing_is_published() {
    let fx = build("clone-corrupt", RUN);
    for (label, edit) in [
        (
            "garbage",
            Box::new(|b: &mut Vec<u8>| *b = b"this is not a bundle\n".to_vec())
                as Box<dyn Fn(&mut Vec<u8>)>,
        ),
        (
            "truncated",
            Box::new(|b: &mut Vec<u8>| b.truncate(b.len() - 40)),
        ),
    ] {
        let zip = fx.zip_edited(&format!("{label}.zip"), |e| {
            for x in e.iter_mut() {
                if let Entry::File { name, data, .. } = x
                    && name.ends_with("/repo.bundle")
                {
                    edit(data);
                }
            }
        });
        let cwd = support::scratch(&format!("clone-corrupt-{label}"));
        let err = clone_zip(&Git::system(), &req(&zip, Some(Path::new("out")), &cwd)).unwrap_err();
        assert!(matches!(err, CloneError::Bundle(_)), "{label}: {err:?}");
        assert!(err.to_string().contains("repo.bundle"), "{label}: {err}");
        assert!(!cwd.join("out").exists(), "{label}: nothing published");
        assert_eq!(staging_left(&cwd), Vec::<String>::new(), "{label}");
    }
}

#[test]
fn a_bundle_without_the_harness_branch_is_refused() {
    let fx = build_with("clone-nobranch", RUN, 0, true);
    let cwd = support::scratch("clone-nobranch-cwd");
    let err = clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new("out")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::Bundle(_)), "{err:?}");
    assert!(err.to_string().contains(&format!("harness/{RUN}")), "{err}");
    assert!(!cwd.join("out").exists());
    assert_eq!(staging_left(&cwd), Vec::<String>::new());
}

#[test]
fn a_destination_that_is_not_an_empty_directory_is_refused_untouched() {
    let fx = build("clone-dest", RUN);
    let cwd = support::scratch("clone-dest-cwd");

    let full = cwd.join("full");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(full.join("mine.txt"), "keep me").unwrap();
    let err = clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new("full")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::Dest(_)), "{err:?}");
    assert!(err.to_string().contains("not empty"), "{err}");
    assert_eq!(
        std::fs::read_to_string(full.join("mine.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(std::fs::read_dir(&full).unwrap().count(), 1);

    std::fs::write(cwd.join("afile"), "x").unwrap();
    let err = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("afile")), &cwd),
    )
    .unwrap_err();
    assert!(matches!(err, CloneError::Dest(_)), "{err:?}");

    let empty = cwd.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    std::os::unix::fs::symlink(&empty, cwd.join("alink")).unwrap();
    let err = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("alink")), &cwd),
    )
    .unwrap_err();
    assert!(matches!(err, CloneError::Dest(_)), "{err:?}");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    assert_eq!(staging_left(&cwd), Vec::<String>::new());

    // An existing empty directory is replaced by the clone in one rename.
    let out = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("empty")), &cwd),
    )
    .unwrap();
    assert_eq!(out.dir, empty);
    assert_tree(&empty, &fx.items);
    assert!(empty.join(".git").is_dir());
}

#[test]
fn git_missing_or_unusable_is_said_in_plain_words_and_nothing_is_made() {
    let fx = build("clone-nogit", RUN);
    let cwd = support::scratch("clone-nogit-cwd");
    // Not on the machine, and the macOS case: a git that exits non-zero (the stub without the command line tools).
    for program in ["/nonexistent/git-for-ham-tests", "false"] {
        let err = clone_zip(
            &Git::with_program(program),
            &req(&fx.zip, Some(Path::new("out")), &cwd),
        )
        .unwrap_err();
        assert!(matches!(err, CloneError::NoGit(_)), "{program}: {err:?}");
        let text = err.to_string();
        assert!(text.contains("git is not available"), "{text}");
        assert!(text.contains("xcode-select --install"), "{text}");
        assert!(!cwd.join("out").exists());
        assert_eq!(staging_left(&cwd), Vec::<String>::new());
    }
    assert!(
        Git::with_program("/nonexistent/git-for-ham-tests")
            .check()
            .is_err()
    );
    assert!(Git::system().check().unwrap().starts_with("git version"));
}

fn altered(fx: &clonefix::Fixture, label: &str, edit: impl FnOnce(&mut Vec<Entry>)) -> String {
    let zip = fx.zip_edited(&format!("{label}.zip"), edit);
    let cwd = support::scratch(&format!("clone-altered-{label}"));
    let err = clone_zip(&Git::system(), &req(&zip, Some(Path::new("out")), &cwd)).unwrap_err();
    assert!(
        matches!(err, CloneError::TreeMismatch(_)),
        "{label}: {err:?}"
    );
    assert!(!cwd.join("out").exists(), "{label}: nothing published");
    assert_eq!(staging_left(&cwd), Vec::<String>::new(), "{label}");
    err.to_string()
}

#[test]
fn a_zip_whose_src_differs_from_the_bundle_head_is_refused_with_the_path() {
    let fx = build("clone-altered", RUN);
    let top = fx.top.clone();
    let named = |n: &str| format!("{top}/src/{n}");

    let msg = altered(&fx, "content", |e| {
        for x in e.iter_mut() {
            if let Entry::File { name, data, .. } = x
                && *name == named("calc.py")
            {
                data.extend_from_slice(b"# altered\n");
            }
        }
    });
    assert!(
        msg.contains("calc.py") && msg.contains("content differs"),
        "{msg}"
    );

    let msg = altered(&fx, "mode", |e| {
        for x in e.iter_mut() {
            if let Entry::File { name, perm, .. } = x
                && *name == named("run.sh")
            {
                *perm = 0o644;
            }
        }
    });
    assert!(
        msg.contains("run.sh") && msg.contains("100644") && msg.contains("100755"),
        "{msg}"
    );

    let msg = altered(&fx, "extra", |e| {
        e.push(Entry::File {
            name: named("extra.txt"),
            data: b"x".to_vec(),
            perm: 0o644,
        });
    });
    assert!(
        msg.contains("only in the zip") && msg.contains("extra.txt"),
        "{msg}"
    );

    let msg = altered(&fx, "missing", |e| {
        e.retain(|x| x.name() != named("calc.py"))
    });
    assert!(
        msg.contains("only in the repository") && msg.contains("calc.py"),
        "{msg}"
    );

    let msg = altered(&fx, "symlink", |e| {
        for x in e.iter_mut() {
            if let Entry::Symlink { name, target } = x
                && *name == named("link.py")
            {
                *target = "tests/test_calc.py".into();
            }
        }
    });
    assert!(msg.contains("link.py"), "{msg}");

    // A file standing where git has a symlink (and the reverse) is a mode difference too.
    let msg = altered(&fx, "kind", |e| {
        for x in e.iter_mut() {
            if x.name() == named("link.py") {
                *x = Entry::File {
                    name: named("link.py"),
                    data: b"calc.py".to_vec(),
                    perm: 0o644,
                };
            }
        }
    });
    assert!(msg.contains("link.py") && msg.contains("120000"), "{msg}");
}

/// A zip made by an earlier release of the packager (a symbolic link flattened to a regular file, line endings converted by
/// `git archive`) cannot equal the bundle's tree. It is refused as before, and the message says what kind of zip that can be and how
/// to read it, so the person is not left to suspect the bundle.
#[test]
fn a_zip_in_the_shape_an_earlier_packager_made_is_refused_with_a_hint() {
    let fx = build_opts(
        "clone-old",
        RUN,
        Opts {
            crlf: true,
            ..Opts::default()
        },
    );
    let zip = fx.zip_edited_consistent("old.zip", |e| {
        for x in e.iter_mut() {
            let flattened = match &*x {
                Entry::Symlink { name, target } if name.ends_with("/link.py") => {
                    Some(Entry::File {
                        name: name.clone(),
                        data: target.clone().into_bytes(),
                        perm: 0o777,
                    })
                }
                _ => None,
            };
            if let Some(f) = flattened {
                *x = f;
            } else if let Entry::File { name, data, .. } = x
                && name.ends_with("/notes.txt")
            {
                *data = b"line one\r\nline two\r\n".to_vec();
            }
        }
    });
    let cwd = support::scratch("clone-old-cwd");
    let err = clone_zip(&Git::system(), &req(&zip, Some(Path::new("out")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::TreeMismatch(_)), "{err:?}");
    let text = err.to_string();
    for want in ["link.py", "notes.txt", "earlier release", "unzip"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert!(!cwd.join("out").exists(), "nothing is published");
}

#[test]
fn a_zip_with_no_bundle_or_two_is_refused_and_a_non_zip_is_not_read() {
    let fx = build("clone-shape", RUN);
    let cwd = support::scratch("clone-shape-cwd");
    let none = fx.zip_edited("nobundle.zip", |e| {
        e.retain(|x| !x.name().ends_with("/repo.bundle"))
    });
    let err = clone_zip(&Git::system(), &req(&none, Some(Path::new("o1")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
    let text = err.to_string();
    assert!(
        text.contains("repo.bundle") && text.contains("unzip"),
        "{text}"
    );

    let two = fx.zip_edited("two.zip", |e| {
        e.push(Entry::File {
            name: "other-top/repo.bundle".into(),
            data: b"x".to_vec(),
            perm: 0o644,
        });
    });
    let err = clone_zip(&Git::system(), &req(&two, Some(Path::new("o2")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
    assert!(err.to_string().contains("more than one"), "{err}");

    let junk = fx.dir.join("junk.zip");
    std::fs::write(&junk, b"definitely not a zip file").unwrap();
    let err = clone_zip(&Git::system(), &req(&junk, Some(Path::new("o3")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
    assert!(err.to_string().contains("not a readable zip"), "{err}");

    let missing = fx.dir.join("absent.zip");
    let err = clone_zip(&Git::system(), &req(&missing, Some(Path::new("o4")), &cwd)).unwrap_err();
    assert!(
        matches!(err, CloneError::Zip(_) | CloneError::Io(_)),
        "{err:?}"
    );
    for o in ["o1", "o2", "o3", "o4"] {
        assert!(!cwd.join(o).exists());
    }
    assert_eq!(staging_left(&cwd), Vec::<String>::new());
}

#[test]
fn blob_ids_and_modes_follow_git() {
    // `printf 'hello\n' | git hash-object --stdin` and the empty blob.
    assert_eq!(
        blob_id(b"hello\n"),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
    assert_eq!(blob_id(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_eq!(git_mode(Some(0o100_644)).unwrap(), "100644");
    assert_eq!(git_mode(Some(0o100_755)).unwrap(), "100755");
    assert_eq!(
        git_mode(Some(0o100_700)).unwrap(),
        "100755",
        "any execute bit"
    );
    assert_eq!(
        git_mode(Some(0o104_755)).unwrap(),
        "100755",
        "setuid is not a git mode"
    );
    assert_eq!(git_mode(Some(0o120_777)).unwrap(), "120000");
    assert_eq!(
        git_mode(Some(0o000_644)).unwrap(),
        "100644",
        "no file type bits: a regular file"
    );
    assert_eq!(git_mode(None).unwrap(), "100644");
    assert!(
        git_mode(Some(0o010_644)).is_err(),
        "a fifo is not a source file"
    );
    assert!(
        git_mode(Some(0o140_755)).is_err(),
        "a socket is not a source file"
    );
}

#[test]
fn a_checkout_that_git_calls_dirty_is_refused_with_a_reason() {
    assert!(check_clean(b"").is_ok());
    let err = check_clean(b" M calc.py\0?? stray.txt\0").unwrap_err();
    assert!(matches!(err, CloneError::Dirty(_)), "{err:?}");
    let text = err.to_string();
    for want in ["calc.py", "stray.txt", "case-insensitive"] {
        assert!(text.contains(want), "{want}: {text}");
    }
}

/// A `.gitattributes` rule that converts line endings changes the bytes of a checkout on any system. The committed blobs are the ones
/// proven equal to the zip's `src/`, so the clone is published, and the person is told which files differ in bytes.
#[test]
fn a_gitattributes_conversion_is_a_warning_and_the_clone_still_succeeds() {
    let fx = build_opts(
        "clone-crlf",
        RUN,
        Opts {
            crlf: true,
            ..Opts::default()
        },
    );
    let cwd = support::scratch("clone-crlf-cwd");
    let out = clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new("out")), &cwd)).unwrap();
    assert_eq!(
        std::fs::read(out.dir.join("notes.txt")).unwrap(),
        b"line one\r\nline two\r\n"
    );
    assert_eq!(
        git(&out.dir, &["status", "--porcelain"]),
        "",
        "git sees no change"
    );
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(
        out.warnings[0].contains(".gitattributes") && out.warnings[0].contains("notes.txt"),
        "{:?}",
        out.warnings
    );
}

/// Git calls a checkout clean when two index entries fold into one file (`A` and `a` on a case-insensitive file system, or two Unicode
/// forms of one name); only a walk of the disk sees that a path of the zip is missing.
#[test]
fn the_checkout_on_disk_must_hold_every_path_of_the_zip_once_with_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = support::scratch("clone-disk");
    std::fs::write(dir.join("a"), "x").unwrap();
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("a", dir.join("link")).unwrap();
    std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
    std::fs::write(dir.join(".git/config"), "ignored").unwrap();
    std::fs::create_dir_all(dir.join("submodule")).unwrap();
    let blob = |mode, bytes: &[u8]| Blob {
        mode,
        id: blob_id(bytes),
    };
    let mut want = std::collections::BTreeMap::new();
    want.insert(b"a".to_vec(), blob("100644", b"x"));
    want.insert(b"run.sh".to_vec(), blob("100755", b"#!/bin/sh\n"));
    want.insert(b"link".to_vec(), blob("120000", b"a"));
    assert_eq!(check_worktree(&dir, &want).unwrap(), Vec::<String>::new());

    // The zip also has `A`, which the disk folded into `a`.
    let mut folded = want.clone();
    folded.insert(b"A".to_vec(), blob("100644", b"x"));
    let err = check_worktree(&dir, &folded).unwrap_err();
    assert!(matches!(err, CloneError::TreeMismatch(_)), "{err:?}");
    assert!(
        err.to_string().contains("only in the zip, not on disk: A"),
        "{err}"
    );

    // A file the zip does not have, a wrong mode and a link where the zip has a file.
    let mut other = want.clone();
    other.remove(b"link".as_slice());
    other.insert(b"run.sh".to_vec(), blob("100644", b"#!/bin/sh\n"));
    other.insert(b"a".to_vec(), blob("120000", b"x"));
    let text = check_worktree(&dir, &other).unwrap_err().to_string();
    for want in [
        "only on disk, not in the zip: link",
        "run.sh: mode 100644 in the zip, 100755 on disk",
        "a: mode 120000 in the zip, 100644 on disk",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }

    // Different bytes at the right path and mode are reported, not refused.
    let mut changed = want.clone();
    changed.insert(b"a".to_vec(), blob("100644", b"y"));
    assert_eq!(check_worktree(&dir, &changed).unwrap(), ["a"]);
}

#[test]
fn a_relative_working_directory_and_a_destination_ending_in_a_dot_are_resolved_before_git_runs() {
    let fx = build("clone-rel", RUN);
    let abs = support::scratch("clone-rel-cwd");
    // The same directory, written relative to wherever the test runs.
    let here = std::env::current_dir().unwrap();
    let mut rel = std::path::PathBuf::new();
    for _ in here.components().skip(1) {
        rel.push("..");
    }
    rel.push(abs.strip_prefix("/").unwrap());
    assert!(rel.is_relative());
    let out = clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new("out")), &rel)).unwrap();
    assert!(out.dir.is_absolute(), "{:?}", out.dir);
    // Absolute, and the same directory (`..` components are kept, not folded: a link could make that wrong).
    assert_eq!(
        std::fs::canonicalize(&out.dir).unwrap(),
        std::fs::canonicalize(abs.join("out")).unwrap()
    );
    assert_tree(&abs.join("out"), &fx.items);

    // `emp/.` is `emp`: the staging directory is made beside it, not inside it.
    std::fs::create_dir_all(abs.join("emp")).unwrap();
    let out = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("emp/.")), &abs),
    )
    .unwrap();
    assert_eq!(out.dir, abs.join("emp"));
    assert_tree(&abs.join("emp"), &fx.items);
    assert_eq!(staging_left(&abs), Vec::<String>::new());
}

/// `ham clone <run> .` in an empty directory renamed the work tree over the directory the caller's shell sits in and left the shell in
/// a deleted directory (the review's finding F2). The directory the caller is in is refused with the command to run instead, and
/// nothing is made.
#[test]
fn the_directory_the_caller_is_in_is_refused_with_a_plain_message_and_left_alone() {
    let fx = build("clone-here", RUN);
    let parent = support::scratch("clone-here-parent");
    let here = parent.join("here");
    std::fs::create_dir_all(&here).unwrap();
    for dest in [Path::new("."), Path::new("../here"), here.as_path()] {
        let err = clone_zip(&Git::system(), &req(&fx.zip, Some(dest), &here)).unwrap_err();
        assert!(matches!(err, CloneError::Dest(_)), "{dest:?}: {err:?}");
        let text = err.to_string();
        assert!(
            text.contains("the directory you are in")
                && text.contains(&format!("ham clone {RUN} <")),
            "{dest:?}: {text}"
        );
        assert!(
            here.is_dir() && std::fs::read_dir(&here).unwrap().next().is_none(),
            "{dest:?}: the directory is untouched"
        );
        assert_eq!(staging_left(&parent), Vec::<String>::new(), "{dest:?}");
    }
    // Another empty directory is still filled.
    let elsewhere = parent.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("../elsewhere")), &here),
    )
    .unwrap();
    assert_tree(&out.dir, &fx.items);
}

/// An existing empty destination is a directory the person made, so the clone keeps its permission bits (a `chmod 700` directory
/// does not become 755).
#[test]
fn an_existing_empty_destination_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let fx = build("clone-mode", RUN);
    let cwd = support::scratch("clone-mode-cwd");
    for (name, mode) in [("private", 0o700), ("shared", 0o750)] {
        std::fs::create_dir_all(cwd.join(name)).unwrap();
        std::fs::set_permissions(cwd.join(name), std::fs::Permissions::from_mode(mode)).unwrap();
        let out = clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new(name)), &cwd)).unwrap();
        let got = std::fs::metadata(&out.dir).unwrap().permissions().mode() & 0o7777;
        assert_eq!(got, mode, "{name}: {got:o}");
        assert_tree(&out.dir, &fx.items);
    }
}

/// An existing empty destination that the person made unwritable (`chmod 500` or `555`) still gets the clone, and the clone keeps that
/// mode. The staged directory is moved first and takes the mode after: a directory cannot be moved into another parent while it has no
/// write permission, which the old order (mode first, move second) set up for it.
#[test]
fn an_existing_empty_destination_without_write_permission_gets_the_clone_and_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let fx = build("clone-mode-ro", RUN);
    let cwd = support::scratch("clone-mode-ro-cwd");
    for (name, mode) in [("owner-reads", 0o500), ("everyone-reads", 0o555)] {
        std::fs::create_dir_all(cwd.join(name)).unwrap();
        std::fs::set_permissions(cwd.join(name), std::fs::Permissions::from_mode(mode)).unwrap();
        let out = clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new(name)), &cwd));
        // The scratch directory stays removable whatever the result.
        let dir = cwd.join(name);
        let got = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = out.unwrap_or_else(|e| panic!("{name} ({mode:o}): {e}"));
        assert_eq!(got, mode, "{name}: {got:o}");
        assert_tree(&out.dir, &fx.items);
        assert_eq!(staging_left(&cwd), Vec::<String>::new(), "{name}");
        assert!(out.warnings.is_empty(), "{name}: {:?}", out.warnings);
    }
}

/// A delivery of a repository that holds a file named in bytes that are not UTF-8 (`caf\xe9.txt`, a Latin-1 name from a Linux file system),
/// with the zip the packager's copy writes for it. `git archive` stores such a name without the zip's UTF-8 flag, and the copy reads it back
/// as CP437 text (`0xe9` is `\u{398}`) and writes that, so `src/` has the file under another name than git has.
fn non_utf8_name_delivery(label: &str) -> (clonefix::Fixture, std::path::PathBuf) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let fx = build_custom(
        label,
        RUN,
        &format!("python-calc-{RUN}-abc1234"),
        vec![
            clonefix::item("calc.py", "x = 1\n", 0o100_644),
            clonefix::item("cafX.txt", "latte\n", 0o100_644),
        ],
        vec![],
        false,
    );
    let repo = fx.dir.join("repo");
    git_os(
        &repo,
        &[
            OsStr::new("mv"),
            OsStr::new("cafX.txt"),
            OsStr::from_bytes(b"caf\xe9.txt"),
        ],
    );
    git(&repo, &["commit", "-q", "-m", "a name in Latin-1"]);
    let bundle = fx.dir.join("renamed.bundle");
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            bundle.to_str().unwrap(),
            "HEAD",
            &format!("refs/heads/harness/{RUN}"),
        ],
    );
    let bundle = std::fs::read(bundle).unwrap();
    let zip = fx.zip_edited("non-utf8-name.zip", |entries| {
        for e in entries.iter_mut() {
            if let Entry::File { name, data, .. } = e {
                if name.ends_with("/repo.bundle") {
                    *data = bundle.clone();
                } else if name.ends_with("/src/cafX.txt") {
                    *name = name.replace("cafX.txt", "caf\u{398}.txt");
                }
            }
        }
    });
    (fx, zip)
}

/// A repository file name that is not UTF-8 cannot match a zip entry name, which is text. The refusal says that, shows the name with its bytes
/// escaped, and does not blame an earlier release of the packager.
#[test]
fn a_file_name_that_is_not_utf_8_is_named_as_the_cause_of_the_tree_mismatch() {
    let (_fx, zip) = non_utf8_name_delivery("clone-non-utf8-name");
    let cwd = support::scratch("clone-non-utf8-name-cwd");
    let err = clone_zip(&Git::system(), &req(&zip, Some(Path::new("out")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::TreeMismatch(_)), "{err:?}");
    let text = err.to_string();
    for want in [
        "caf\\xe9.txt",
        "not valid UTF-8",
        "Nothing was published",
        "unzip",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert!(
        !text.contains("earlier release"),
        "the hint is for a zip made by an older packager: {text}"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "a replacement character stands for the name: {text}"
    );
    assert!(!cwd.join("out").exists(), "nothing published");
    assert_eq!(staging_left(&cwd), Vec::<String>::new());
}

#[test]
fn a_run_id_that_is_not_one_plain_component_is_refused_before_anything_is_made() {
    let fx = build("clone-runid", RUN);
    let cwd = support::scratch("clone-runid-cwd");
    let long = "A".repeat(65);
    for bad in ["", "../x", "a/b", "x y", ".hidden", long.as_str()] {
        let r = Request {
            run_id: bad,
            zip: &fx.zip,
            dest: Some(Path::new("out")),
            cwd: &cwd,
            partial: false,
        };
        let err = clone_zip(&Git::system(), &r).unwrap_err();
        assert!(matches!(err, CloneError::RunId(_)), "{bad:?}: {err:?}");
        assert!(err.to_string().contains("not a run id"), "{err}");
    }
    assert_eq!(
        std::fs::read_dir(&cwd).unwrap().count(),
        0,
        "nothing was made"
    );
}

#[test]
fn a_zip_whose_top_directory_is_not_utf_8_is_refused() {
    let fx = build_custom(
        "clone-utf8",
        RUN,
        "toXp",
        clonefix::first_commit(),
        vec![],
        false,
    );
    let mut bytes = fx.zip_bytes();
    let (from, to) = (b"toXp/".as_slice(), b"to\xffp/".as_slice());
    let mut at = 0;
    let mut n = 0;
    while let Some(i) = bytes[at..].windows(from.len()).position(|w| w == from) {
        bytes[at + i..at + i + from.len()].copy_from_slice(to);
        at += i + from.len();
        n += 1;
    }
    assert!(n >= 2, "the name sits in a local and a central header: {n}");
    let zip = fx.dir.join("utf8.zip");
    std::fs::write(&zip, bytes).unwrap();
    let cwd = support::scratch("clone-utf8-cwd");
    let err = clone_zip(&Git::system(), &req(&zip, Some(Path::new("out")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::Zip(_)), "{err:?}");
    assert!(err.to_string().contains("not UTF-8"), "{err}");
    assert!(!cwd.join("out").exists());
}

/// A program that exits 0 but is not git (a stub that prints something else) is as good as no git.
#[test]
fn a_program_that_is_not_git_is_not_git() {
    let err = Git::with_program("echo").check().unwrap_err();
    assert!(matches!(err, CloneError::NoGit(_)), "{err:?}");
    assert!(err.to_string().contains("it printed"), "{err}");
}

#[test]
fn more_than_eight_dirty_paths_are_counted() {
    let status: Vec<u8> = (0..11)
        .flat_map(|i| format!(" M f{i:02}\0").into_bytes())
        .collect();
    let text = check_clean(&status).unwrap_err().to_string();
    assert!(
        text.contains("f00") && text.contains("and 3 more"),
        "{text}"
    );
}

#[test]
fn a_destination_that_cannot_be_looked_at_or_made_is_refused_with_the_path() {
    use std::os::unix::fs::PermissionsExt;
    let fx = build("clone-dest-err", RUN);
    let cwd = support::scratch("clone-dest-err-cwd");
    // A file where a directory should be: the path below it cannot be inspected.
    std::fs::write(cwd.join("afile"), "x").unwrap();
    let err = clone_zip(
        &Git::system(),
        &req(&fx.zip, Some(Path::new("afile/sub")), &cwd),
    )
    .unwrap_err();
    assert!(matches!(err, CloneError::Dest(_)), "{err:?}");
    assert!(err.to_string().contains("afile/sub"), "{err}");

    // A parent the person cannot write to: neither the missing parents nor the staging directory can be made.
    let ro = cwd.join("ro");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let root = std::fs::write(ro.join("probe"), "x").is_ok();
    if !root {
        for (dest, want) in [
            ("ro/missing/out", "cannot create"),
            ("ro/out", "cannot create a staging directory"),
        ] {
            let err =
                clone_zip(&Git::system(), &req(&fx.zip, Some(Path::new(dest)), &cwd)).unwrap_err();
            assert!(matches!(err, CloneError::Dest(_)), "{dest}: {err:?}");
            assert!(err.to_string().contains(want), "{dest}: {err}");
        }
        assert_eq!(std::fs::read_dir(&ro).unwrap().count(), 0);
    }
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn entries_without_a_directory_are_ignored_and_a_duplicate_name_cannot_hide_a_file() {
    let fx = clonefix::build_custom(
        "clone-dup",
        RUN,
        &format!("python-calc-{RUN}-abc1234"),
        vec![
            clonefix::item("dup_a.txt", "x", 0o100_644),
            clonefix::item("dup_b.txt", "x", 0o100_644),
        ],
        vec![],
        false,
    );
    // A file at the top of the zip is none of ham clone's business.
    let loose = fx.zip_edited("loose.zip", |e| {
        e.push(Entry::File {
            name: "loose.txt".into(),
            data: b"x".to_vec(),
            perm: 0o644,
        });
    });
    let cwd = support::scratch("clone-dup-cwd");
    clone_zip(&Git::system(), &req(&loose, Some(Path::new("loose")), &cwd)).unwrap();

    // Two entries with one name (the second name patched over the first, same length, same bytes, same CRC): the zip crate keeps
    // the later one, so the earlier file vanishes from `src/` and the repository has a file the zip lacks.
    let mut bytes = fx.zip_bytes();
    let (from, to) = (b"dup_b.txt".as_slice(), b"dup_a.txt".as_slice());
    let mut at = 0;
    while let Some(i) = bytes[at..].windows(from.len()).position(|w| w == from) {
        bytes[at + i..at + i + from.len()].copy_from_slice(to);
        at += i + from.len();
    }
    let dup = fx.dir.join("dup.zip");
    std::fs::write(&dup, bytes).unwrap();
    let err = clone_zip(&Git::system(), &req(&dup, Some(Path::new("dup")), &cwd)).unwrap_err();
    assert!(matches!(err, CloneError::TreeMismatch(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("only in the repository: dup_b.txt"),
        "{err}"
    );
    assert!(!cwd.join("dup").exists());
    assert_eq!(staging_left(&cwd), Vec::<String>::new());
}

fn api(fake: &Fake) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(&fake.endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    })
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    api(fake)
        .start_direct_run(&req, &format!("k-{prompt}"))
        .await
        .unwrap()
        .run_id
}

const MIB: u64 = 1024 * 1024;

#[tokio::test]
async fn a_killed_download_resumes_and_then_clones() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "clone-resume").await;
    let fx = build_with("clone-resume", &run, 3 * MIB as usize, false);
    let (_, sha) = fake.mem.deliverable(&run, fx.zip_bytes());
    let downloads = support::scratch("clone-resume-dl");
    let cwd = support::scratch("clone-resume-cwd");
    let request = RunRequest {
        run_id: &run,
        dest: Some(Path::new("out")),
        downloads: &downloads,
        cwd: &cwd,
        chunk: MIB,
    };
    let git = Git::system();

    fake.faults.fail_range_at.store(2 * MIB, Ordering::SeqCst);
    let mut notes = Vec::new();
    let err = clone_run(&api(&fake), &git, &request, &mut |n| {
        notes.push(n.to_string())
    })
    .await
    .unwrap_err();
    assert!(matches!(err, CloneError::Pull(_)), "{err:?}");
    assert!(
        !cwd.join("out").exists(),
        "nothing is cloned from a broken download"
    );
    let part = downloads.join(format!("python-calc-{run}-abc1234.zip.part"));
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 2 * MIB);

    fake.mem.inner.lock().unwrap().ranges.clear();
    let (cloned, pulled) = clone_run(&api(&fake), &git, &request, &mut |n| {
        notes.push(n.to_string())
    })
    .await
    .unwrap();
    assert_eq!(
        pulled.resumed_from,
        2 * MIB,
        "only the missing range is fetched"
    );
    assert_eq!(cloned.zip, pulled.path);
    assert_eq!(cloned.zip_sha256, sha);
    assert!(!cloned.partial);
    assert_eq!(cloned.head, fx.head);
    assert_tree(&cloned.dir, &fx.items);
    assert!(
        notes.is_empty(),
        "a complete delivery needs no notice: {notes:?}"
    );
    assert!(
        pulled.path.is_file(),
        "the zip stays in the download directory"
    );
}

#[tokio::test]
async fn a_blocked_run_clones_its_partial_zip_with_a_notice_and_a_run_with_no_zip_says_so() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "clone-partial").await;
    let downloads = support::scratch("clone-partial-dl");
    let cwd = support::scratch("clone-partial-cwd");
    let request = RunRequest {
        run_id: &run,
        dest: Some(Path::new("out")),
        downloads: &downloads,
        cwd: &cwd,
        chunk: MIB,
    };
    let git = Git::system();
    let mut notes = Vec::new();

    let err = clone_run(&api(&fake), &git, &request, &mut |n| {
        notes.push(n.to_string())
    })
    .await
    .unwrap_err();
    assert!(matches!(err, CloneError::NoDelivery(_)), "{err:?}");
    assert!(
        err.to_string().contains("no zip or partial zip yet"),
        "{err}"
    );

    let fx = build("clone-partial", &run);
    fake.mem.partial(&run, fx.zip_bytes());
    let (cloned, pulled) = clone_run(&api(&fake), &git, &request, &mut |n| {
        notes.push(n.to_string())
    })
    .await
    .unwrap();
    assert!(cloned.partial);
    assert_eq!(
        pulled.artifact.kind,
        harness_protocol::store::ArtifactKind::Partial
    );
    assert!(
        notes.iter().any(|n| n.contains("partial")),
        "the partial delivery is announced before the clone: {notes:?}"
    );
    assert_tree(&cloned.dir, &fx.items);
}

#[tokio::test]
async fn git_missing_stops_before_anything_is_downloaded() {
    let fake = Fake::start().await;
    let run = new_run(&fake, "clone-nogit-dl").await;
    let fx = build("clone-nogit-dl", &run);
    fake.mem.deliverable(&run, fx.zip_bytes());
    let downloads = support::scratch("clone-nogit-dl-dl");
    let cwd = support::scratch("clone-nogit-dl-cwd");
    let request = RunRequest {
        run_id: &run,
        dest: None,
        downloads: &downloads,
        cwd: &cwd,
        chunk: MIB,
    };
    let err = clone_run(
        &api(&fake),
        &Git::with_program("/nonexistent/git-for-ham-tests"),
        &request,
        &mut |_| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CloneError::NoGit(_)), "{err:?}");
    assert_eq!(
        std::fs::read_dir(&downloads).unwrap().count(),
        0,
        "nothing downloaded"
    );
    assert!(
        !fake
            .mem
            .seen()
            .iter()
            .any(|s| s.path.starts_with("/objects/")),
        "no object was requested"
    );
}
