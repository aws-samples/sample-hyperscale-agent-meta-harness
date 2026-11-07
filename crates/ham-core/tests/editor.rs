//! Ctrl-G, the draft in the user's editor. The editor is `$VISUAL`, else `$EDITOR`, split like a shell line into an argument
//! vector and started with no shell (nothing is expanded or run through a shell); the draft round-trips through a file created
//! exclusively under the user's `TMPDIR` with mode 0600, and the file is deleted afterwards on every path. A text that comes back is
//! cleaned like a paste and tainted. The child runs through a closure, so the TUI hands it the terminal
//! (`suspend_for`) and these tests hand it a stand-in or a real script.

#![allow(clippy::unwrap_used)]

use ham_core::editor::{
    EditError, Editor, MAX_DRAFT_BYTES, Outcome, default_dir, edit, edit_with, edit_with_report,
};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh directory under the workspace temp dir (`TMPDIR`).
fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ham-edit-test-{label}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn entries(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

fn ok() -> ExitStatus {
    ExitStatus::from_raw(0)
}

fn failed() -> ExitStatus {
    ExitStatus::from_raw(1 << 8)
}

/// A shell script, run as `sh <script> ...` (never executed directly, so a thread that forks while it is written cannot make the run
/// fail with "text file busy").
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn argv(line: &str) -> Vec<String> {
    Editor::from_command(line).unwrap().argv().to_vec()
}

fn last_arg(cmd: &std::process::Command) -> PathBuf {
    PathBuf::from(cmd.get_args().last().unwrap())
}

// ---- the command line ---------------------------------------------------------------------------------------------------------------

#[test]
fn an_editor_command_splits_like_a_shell_line_and_expands_nothing() {
    assert_eq!(argv("vim"), ["vim"]);
    assert_eq!(argv("  vim  "), ["vim"]);
    assert_eq!(argv("code --wait"), ["code", "--wait"]);
    assert_eq!(
        argv("emacsclient -t -a ''"),
        ["emacsclient", "-t", "-a", ""]
    );
    assert_eq!(
        argv("\"/Applications/My Editor/bin/ed\" --wait"),
        ["/Applications/My Editor/bin/ed", "--wait"]
    );
    assert_eq!(argv("'/opt/x y/ed' -w"), ["/opt/x y/ed", "-w"]);
    assert_eq!(argv("vim\\ x"), ["vim x"]);
    assert_eq!(argv("ed -c \"set ft=md\""), ["ed", "-c", "set ft=md"]);
    assert_eq!(
        argv("ed $HOME ~ * ; z && w | v > f"),
        [
            "ed", "$HOME", "~", "*", ";", "z", "&&", "w", "|", "v", ">", "f"
        ],
        "no variable, tilde, glob or operator means anything"
    );
}

#[test]
fn a_bad_command_line_is_refused_with_a_reason() {
    for blank in ["", "   ", "\t"] {
        assert_eq!(
            Editor::from_command(blank),
            Err(EditError::NoEditor),
            "{blank:?}"
        );
    }
    for bad in ["vim 'unclosed", "vim \"unclosed"] {
        assert!(
            matches!(Editor::from_command(bad), Err(EditError::BadCommand(_))),
            "{bad:?}"
        );
    }
    for bad in ["vim\u{1b}[2J", "vim\u{0}", "vim\u{7}"] {
        assert!(
            matches!(Editor::from_command(bad), Err(EditError::BadCommand(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn visual_wins_then_editor_and_a_blank_one_does_not_count() {
    let name = |v: Option<&str>, e: Option<&str>| {
        Editor::from_env_values(v, e).map(|x| x.argv()[0].clone())
    };
    assert_eq!(name(Some("code -w"), Some("vim")).unwrap(), "code");
    assert_eq!(name(Some("  "), Some("vim")).unwrap(), "vim");
    assert_eq!(name(Some(""), Some("vim")).unwrap(), "vim");
    assert_eq!(name(None, Some("vim")).unwrap(), "vim");
    assert_eq!(name(Some("nano"), None).unwrap(), "nano");
    assert_eq!(name(None, None), Err(EditError::NoEditor));
    assert_eq!(name(Some(" "), Some("")), Err(EditError::NoEditor));
    // A damaged $VISUAL is reported, not skipped.
    assert!(matches!(
        name(Some("vim 'x"), Some("vim")),
        Err(EditError::BadCommand(_))
    ));
    assert_eq!(
        EditError::NoEditor.to_string(),
        "no editor: set $VISUAL or $EDITOR"
    );
}

#[test]
fn the_environment_is_read_without_a_panic() {
    let _ = Editor::from_env();
}

#[test]
fn the_command_is_the_program_its_arguments_and_the_file_last() {
    let e = Editor::from_command("code --wait --new-window").unwrap();
    let c = e.command_for(Path::new("/tmp/x/draft.md"));
    assert_eq!(c.get_program(), "code");
    let args: Vec<_> = c
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, ["--wait", "--new-window", "/tmp/x/draft.md"]);
}

// ---- the round trip -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_default_directory_is_the_users_temp_dir() {
    assert_eq!(default_dir(), std::env::temp_dir());
}

#[test]
fn the_draft_is_written_to_a_private_file_the_editor_edits_and_the_file_is_deleted() {
    let dir = scratch("roundtrip");
    let e = Editor::from_command("stand-in").unwrap();
    let mut seen = None;
    let out = edit_with(&e, "hello", &dir, |cmd| {
        let p = last_arg(cmd);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        seen = Some((p.clone(), mode, std::fs::read_to_string(&p).unwrap()));
        std::fs::write(&p, "hello\nadded by the editor\n").unwrap();
        Ok(ok())
    })
    .unwrap();
    let (path, mode, before) = seen.unwrap();
    let private = path.parent().unwrap().to_path_buf();
    assert_eq!(
        out,
        Outcome::Edited {
            text: "hello\nadded by the editor".into(),
            tainted: true
        }
    );
    assert_eq!(mode, 0o600, "only the user can read the draft");
    assert_eq!(
        before, "hello\n",
        "the editor gets the draft with a final line break"
    );
    // The file sits in a directory of its own that only the user can enter, so a swap or backup file the editor writes
    // beside it is not left in the shared temp directory.
    assert_eq!(private.parent().unwrap(), dir);
    assert!(
        private
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ham-draft-")
    );
    assert_eq!(path.file_name().unwrap(), "draft.md");
    assert!(!path.exists());
    assert!(!private.exists());
    assert_eq!(entries(&dir), 0);
}

#[test]
fn the_private_directory_is_closed_to_everyone_else() {
    let dir = scratch("mode");
    let e = Editor::from_command("stand-in").unwrap();
    let mut seen = None;
    edit_with(&e, "x", &dir, |cmd| {
        let private = last_arg(cmd).parent().unwrap().to_path_buf();
        seen = Some(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777);
        Ok(ok())
    })
    .unwrap();
    assert_eq!(seen, Some(0o700));
}

#[test]
fn files_an_editor_writes_beside_the_draft_are_removed_with_it() {
    let dir = scratch("sidecars");
    let e = Editor::from_command("stand-in").unwrap();
    let out = edit_with(&e, "secret draft", &dir, |cmd| {
        let p = last_arg(cmd);
        let beside = p.parent().unwrap();
        std::fs::write(beside.join(".draft.md.swp"), "swap with the draft in it").unwrap();
        std::fs::write(beside.join("draft.md~"), "backup").unwrap();
        std::fs::write(beside.join("#draft.md#"), "autosave").unwrap();
        std::fs::write(&p, "secret draft\nmore\n").unwrap();
        Ok(ok())
    })
    .unwrap();
    assert!(matches!(out, Outcome::Edited { .. }));
    assert_eq!(
        entries(&dir),
        0,
        "the editor's own files went with the directory"
    );
}

#[test]
fn a_draft_file_that_is_not_a_regular_file_is_never_read() {
    let dir = scratch("swap");
    let other = scratch("decoy");
    let decoy = other.join("secret.txt");
    std::fs::write(&decoy, "do not read me").unwrap();
    let e = Editor::from_command("stand-in").unwrap();
    let out = edit_with(&e, "x", &dir, |cmd| {
        let p = last_arg(cmd);
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(&decoy, &p).unwrap();
        Ok(ok())
    });
    match out {
        Err(EditError::Io(why)) => assert!(why.contains("regular file"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(entries(&dir), 0);
    assert!(
        decoy.exists(),
        "the link was removed, not what it points at"
    );
}

#[test]
fn the_draft_path_is_absolute_even_for_a_relative_directory() {
    let dir = scratch("relative");
    let cwd = std::env::current_dir().unwrap();
    let up = cwd.components().count() - 1;
    let mut rel = PathBuf::new();
    for _ in 0..up {
        rel.push("..");
    }
    rel.push(dir.strip_prefix("/").unwrap());
    assert!(rel.is_relative());
    let e = Editor::from_command("stand-in").unwrap();
    let mut abs = None;
    edit_with(&e, "x", &rel, |cmd| {
        abs = Some(last_arg(cmd).is_absolute());
        Ok(ok())
    })
    .unwrap();
    assert_eq!(
        abs,
        Some(true),
        "a path that starts with a dash or a dot is never taken for an option or the wrong place"
    );
    assert_eq!(entries(&dir), 0);
}

#[test]
fn the_report_says_how_long_the_editor_ran_and_when_it_returned_at_once() {
    let dir = scratch("report");
    let e = Editor::from_command("stand-in").unwrap();
    let quick = edit_with_report(&e, "x", &dir, |_| Ok(ok())).unwrap();
    assert_eq!(quick.outcome, Outcome::Unchanged);
    assert!(quick.returned_at_once());
    assert!(
        quick.hint().unwrap().contains("--wait"),
        "{:?}",
        quick.hint()
    );
    // An editor that ran a while is not suspected, whatever it returned.
    let slow = edit_with_report(&e, "x", &dir, |_| {
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        Ok(ok())
    })
    .unwrap();
    assert!(slow.elapsed >= std::time::Duration::from_millis(1_000));
    assert!(!slow.returned_at_once());
    assert_eq!(slow.hint(), None);
    // A changed draft needs no hint, however fast.
    let edited = edit_with_report(&e, "x", &dir, |cmd| {
        std::fs::write(last_arg(cmd), "changed\n").unwrap();
        Ok(ok())
    })
    .unwrap();
    assert!(matches!(edited.outcome, Outcome::Edited { .. }));
    assert_eq!(edited.hint(), None);
    assert_eq!(entries(&dir), 0);
}

#[test]
fn a_file_that_is_not_utf8_says_so_and_that_the_draft_in_the_box_is_unchanged() {
    let dir = scratch("utf8msg");
    let e = Editor::from_command("stand-in").unwrap();
    let err = edit_with(&e, "x", &dir, |cmd| {
        std::fs::write(last_arg(cmd), [0xffu8, 0xfe]).unwrap();
        Ok(ok())
    })
    .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("UTF-8") && text.contains("unchanged"),
        "{text}"
    );
}

#[test]
fn a_draft_the_editor_does_not_change_is_unchanged_whatever_line_breaks_it_adds() {
    let dir = scratch("same");
    let e = Editor::from_command("stand-in").unwrap();
    for draft in ["hello", "hello\n", "hello\n\n\n", "two\nlines", ""] {
        let out = edit_with(&e, draft, &dir, |_| Ok(ok())).unwrap();
        assert_eq!(out, Outcome::Unchanged, "{draft:?}");
    }
    let out = edit_with(&e, "keep", &dir, |cmd| {
        std::fs::write(last_arg(cmd), "keep\n\n").unwrap();
        Ok(ok())
    })
    .unwrap();
    assert_eq!(out, Outcome::Unchanged);
    assert_eq!(entries(&dir), 0);
}

#[test]
fn an_editor_that_exits_with_an_error_cancels_and_its_changes_are_ignored() {
    let dir = scratch("cancel");
    let e = Editor::from_command("stand-in").unwrap();
    let out = edit_with(&e, "hello", &dir, |cmd| {
        std::fs::write(last_arg(cmd), "changed then quit with :cq").unwrap();
        Ok(failed())
    })
    .unwrap();
    assert_eq!(out, Outcome::Cancelled);
    assert_eq!(entries(&dir), 0);
}

#[test]
fn an_editor_that_removes_the_file_cancels() {
    let dir = scratch("gone");
    let e = Editor::from_command("stand-in").unwrap();
    let out = edit_with(&e, "hello", &dir, |cmd| {
        std::fs::remove_file(last_arg(cmd)).unwrap();
        Ok(ok())
    })
    .unwrap();
    assert_eq!(out, Outcome::Cancelled);
    assert_eq!(entries(&dir), 0);
}

#[test]
fn the_text_that_comes_back_is_cleaned_like_a_paste() {
    let dir = scratch("clean");
    let e = Editor::from_command("stand-in").unwrap();
    let out = edit_with(&e, "x", &dir, |cmd| {
        std::fs::write(last_arg(cmd), "a\u{1b}[2Jb\r\nc\u{7}\r").unwrap();
        Ok(ok())
    })
    .unwrap();
    assert_eq!(
        out,
        Outcome::Edited {
            text: "ab\nc".into(),
            tainted: true
        }
    );
}

#[test]
fn a_file_that_is_not_text_or_is_too_large_is_an_error_and_is_still_deleted() {
    let dir = scratch("bad");
    let e = Editor::from_command("stand-in").unwrap();
    let out = edit_with(&e, "x", &dir, |cmd| {
        std::fs::write(last_arg(cmd), [0xff, 0xfe, 0x41]).unwrap();
        Ok(ok())
    });
    assert_eq!(out, Err(EditError::NotUtf8));
    assert_eq!(entries(&dir), 0);

    let out = edit_with(&e, "x", &dir, |cmd| {
        std::fs::write(last_arg(cmd), vec![b'a'; MAX_DRAFT_BYTES + 1]).unwrap();
        Ok(ok())
    });
    assert_eq!(out, Err(EditError::TooLarge));
    assert_eq!(entries(&dir), 0);

    // Exactly the limit is fine.
    let out = edit_with(&e, "x", &dir, |cmd| {
        std::fs::write(last_arg(cmd), vec![b'a'; MAX_DRAFT_BYTES]).unwrap();
        Ok(ok())
    })
    .unwrap();
    assert!(matches!(out, Outcome::Edited { .. }));
    assert_eq!(entries(&dir), 0);
}

#[test]
fn an_editor_that_cannot_start_is_an_error_that_names_it_and_leaves_no_file() {
    let dir = scratch("nostart");
    let e = Editor::from_command("/nonexistent-dir-xyz/editor-xyz --flag").unwrap();
    let err = edit(&e, "hello", &dir).unwrap_err();
    match &err {
        EditError::Spawn { program, .. } => assert_eq!(program, "/nonexistent-dir-xyz/editor-xyz"),
        other => panic!("{other:?}"),
    }
    assert!(
        err.to_string().contains("/nonexistent-dir-xyz/editor-xyz"),
        "{err}"
    );
    assert_eq!(entries(&dir), 0);
}

#[test]
fn a_missing_directory_is_an_error() {
    let e = Editor::from_command("stand-in").unwrap();
    let err = edit_with(&e, "x", Path::new("/nonexistent-dir-xyz"), |_| Ok(ok())).unwrap_err();
    assert!(matches!(err, EditError::Io(_)), "{err:?}");
}

#[test]
fn every_edit_gets_its_own_file_even_at_the_same_time() {
    let dir = scratch("unique");
    let e = Editor::from_command("stand-in").unwrap();
    let paths: Vec<PathBuf> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..16)
            .map(|_| {
                s.spawn(|| {
                    let mut p = None;
                    edit_with(&e, "x", &dir, |cmd| {
                        p = Some(last_arg(cmd));
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok(ok())
                    })
                    .unwrap();
                    p.unwrap()
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let set: std::collections::HashSet<_> = paths.iter().collect();
    assert_eq!(set.len(), 16);
    assert_eq!(entries(&dir), 0);
}

// ---- real processes -----------------------------------------------------------------------------------------------------------------

#[test]
fn a_real_editor_script_edits_the_file_and_gets_the_arguments_without_a_shell() {
    let scripts = scratch("scripts");
    let tmp = scratch("tmp");
    let rec = scripts.join("args.txt");
    let ed = script(
        &scripts,
        "ed.sh",
        &format!(
            "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\nfor last; do :; done\nprintf 'added by editor\\n' >> \"$last\"\n",
            rec.display()
        ),
    );
    let line = format!("/bin/sh '{}' 'a;b' '$HOME' '&&' c", ed.display());
    let e = Editor::from_command(&line).unwrap();
    let out = edit(&e, "hello", &tmp).unwrap();
    assert_eq!(
        out,
        Outcome::Edited {
            text: "hello\nadded by editor".into(),
            tainted: true
        }
    );
    let got = std::fs::read_to_string(&rec).unwrap();
    let lines: Vec<&str> = got.lines().collect();
    // A shell would have split at the semicolon and ampersand and expanded $HOME.
    assert_eq!(&lines[..4], ["a;b", "$HOME", "&&", "c"]);
    assert!(
        lines[4].starts_with(tmp.to_str().unwrap()),
        "the draft file is the last argument: {}",
        lines[4]
    );
    assert_eq!(entries(&tmp), 0);
}

#[test]
fn a_real_editor_that_exits_with_status_one_cancels() {
    let scripts = scratch("scripts-fail");
    let tmp = scratch("tmp-fail");
    let ed = script(&scripts, "ed.sh", "printf 'changed\\n' >> \"$1\"\nexit 1\n");
    let e = Editor::from_command(&format!("/bin/sh '{}'", ed.display())).unwrap();
    assert_eq!(edit(&e, "hello", &tmp).unwrap(), Outcome::Cancelled);
    assert_eq!(entries(&tmp), 0);
}

#[test]
fn a_program_with_spaces_in_its_path_starts_when_quoted() {
    let base = scratch("spaces");
    let dir = base.join("my editors");
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = scratch("tmp-spaces");
    // `sh` is found by name; the script path has a space and is one argument.
    let ed = script(&dir, "ed one.sh", "printf 'x\\n' >> \"$1\"\n");
    let e = Editor::from_command(&format!("/bin/sh \"{}\"", ed.display())).unwrap();
    assert_eq!(
        edit(&e, "a", &tmp).unwrap(),
        Outcome::Edited {
            text: "a\nx".into(),
            tainted: true
        }
    );
}
