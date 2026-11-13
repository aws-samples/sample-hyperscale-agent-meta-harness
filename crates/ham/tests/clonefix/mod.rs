//! A delivered zip for the `ham clone` tests: a tiny real git repository, its `repo.bundle` as the packager makes it
//! (`HEAD` set to `refs/heads/harness/<runId>`, `git bundle create repo.bundle HEAD refs/heads/harness/<runId>`) and a zip
//! laid out like the packager's (`<repo>-<runId>-<sha7>/src/...`, `repo.bundle`, `EVIDENCE.md`, `MANIFEST.sha256`), written
//! with the `zip` crate. Git runs with its configuration isolated, so the test hosts' own settings never leak in.
#![allow(dead_code, clippy::unwrap_used)]

use super::support;
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub const RUN: &str = "01K6A000000000000000000CL1";

/// One path of the repository: its content and git mode (`100644`, `100755` or `120000` with the link target as content).
#[derive(Clone, Debug)]
pub struct Item {
    pub path: String,
    pub content: Vec<u8>,
    pub mode: u32,
}

/// One entry of the zip.
#[derive(Clone, Debug)]
pub enum Entry {
    Dir(String),
    File {
        name: String,
        data: Vec<u8>,
        perm: u32,
    },
    Symlink {
        name: String,
        target: String,
    },
}

impl Entry {
    pub fn name(&self) -> &str {
        match self {
            Entry::Dir(n) | Entry::File { name: n, .. } | Entry::Symlink { name: n, .. } => n,
        }
    }
}

pub struct Fixture {
    pub run: String,
    /// `python-calc-<run>-abc1234`, the zip's one top directory.
    pub top: String,
    pub dir: PathBuf,
    pub zip: PathBuf,
    pub bundle: Vec<u8>,
    pub items: Vec<Item>,
    pub head: String,
    pub commits: usize,
}

/// Runs `git` with an isolated environment and fixed author, committer and dates.
pub fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-10-08T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-08T12:00:00Z")
        .output()
        .unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    git_in(dir, args).unwrap()
}

/// [`git`] with arguments that are not text: a path in bytes that are not UTF-8.
pub fn git_os(dir: &Path, args: &[&std::ffi::OsStr]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-10-08T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-08T12:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

pub fn write_item(root: &Path, it: &Item) {
    let p = root.join(&it.path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    if it.mode == 0o120_000 {
        std::os::unix::fs::symlink(String::from_utf8(it.content.clone()).unwrap(), &p).unwrap();
    } else {
        std::fs::write(&p, &it.content).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let perm = if it.mode == 0o100_755 { 0o755 } else { 0o644 };
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(perm)).unwrap();
    }
}

/// The paths of the repository in its two commits.
pub fn first_commit() -> Vec<Item> {
    vec![
        item("calc.py", "def add(a, b):\n    return a + b\n", 0o100_644),
        item(
            "tests/test_calc.py",
            "from calc import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n",
            0o100_644,
        ),
    ]
}

pub fn second_commit() -> Vec<Item> {
    vec![
        item("run.sh", "#!/bin/sh\npython3 -m pytest -q\n", 0o100_755),
        item("docs/caf\u{e9}.md", "# Caf\u{e9}\n", 0o100_644),
        item("link.py", "calc.py", 0o120_000),
        item("README.md", "# python-calc\n", 0o100_644),
    ]
}

pub fn item(path: &str, content: &str, mode: u32) -> Item {
    Item {
        path: path.into(),
        content: content.as_bytes().to_vec(),
        mode,
    }
}

/// Builds the repository, its bundle and the zip under a fresh scratch directory. `big` adds a file of that many bytes
/// (a zip big enough for ranged downloads).
/// How the fixture differs from the plain one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    /// A file of this many bytes (a zip big enough for ranged downloads).
    pub big: usize,
    /// The bundle holds only branch `other`.
    pub without_harness_branch: bool,
    /// A tracked `.gitattributes` sends `*.py` through the `lfs` filter, which the user's global git configuration may define.
    pub attributes: bool,
    /// A tracked `.gitattributes` has `*.txt text eol=crlf`, and `notes.txt` holds LF line endings: a checkout writes CRLF.
    pub crlf: bool,
}

pub fn build(label: &str, run: &str) -> Fixture {
    build_opts(label, run, Opts::default())
}

pub fn build_with(label: &str, run: &str, big: usize, without_harness_branch: bool) -> Fixture {
    build_opts(
        label,
        run,
        Opts {
            big,
            without_harness_branch,
            ..Opts::default()
        },
    )
}

pub fn build_opts(label: &str, run: &str, o: Opts) -> Fixture {
    let mut first = first_commit();
    if o.attributes {
        first.push(item(".gitattributes", "*.py filter=lfs\n", 0o100_644));
    }
    if o.crlf {
        first.push(item(".gitattributes", "*.txt text eol=crlf\n", 0o100_644));
        first.push(item("notes.txt", "line one\nline two\n", 0o100_644));
    }
    if o.big > 0 {
        first.push(Item {
            path: "data/big.bin".into(),
            content: (0..o.big).map(|i| (i % 251) as u8).collect(),
            mode: 0o100_644,
        });
    }
    build_custom(
        label,
        run,
        &format!("python-calc-{run}-abc1234"),
        first,
        second_commit(),
        o.without_harness_branch,
    )
}

/// A repository of `first` then (when it is not empty) `second` as a second commit, its bundle and its zip with top directory `top`.
pub fn build_custom(
    label: &str,
    run: &str,
    top: &str,
    first: Vec<Item>,
    second: Vec<Item>,
    without_harness_branch: bool,
) -> Fixture {
    let dir = support::scratch(label);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let branch = format!("harness/{run}");
    git(&repo, &["init", "-q", "--initial-branch", &branch]);
    let mut items = first;
    for it in &items {
        write_item(&repo, it);
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let mut commits = 1;
    if !second.is_empty() {
        for it in &second {
            write_item(&repo, it);
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "second"]);
        items.extend(second);
        commits = 2;
    }
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let bundle_path = dir.join("repo.bundle");
    let refname = format!("refs/heads/{branch}");
    if without_harness_branch {
        git(&repo, &["branch", "-q", "other"]);
        git(
            &repo,
            &[
                "bundle",
                "create",
                "-q",
                bundle_path.to_str().unwrap(),
                "other",
            ],
        );
    } else {
        git(
            &repo,
            &[
                "bundle",
                "create",
                "-q",
                bundle_path.to_str().unwrap(),
                "HEAD",
                &refname,
            ],
        );
    }
    let bundle = std::fs::read(&bundle_path).unwrap();
    let fx = Fixture {
        run: run.into(),
        top: top.into(),
        zip: dir.join("delivered.zip"),
        dir,
        bundle,
        items,
        head,
        commits,
    };
    write_zip(&fx.zip, &fx.entries());
    fx
}

impl Fixture {
    /// The packager's layout: directory entries, `src/` verbatim, the bundle, evidence and the manifest.
    pub fn entries(&self) -> Vec<Entry> {
        let t = &self.top;
        let mut v = vec![Entry::Dir(format!("{t}/")), Entry::Dir(format!("{t}/src/"))];
        let mut dirs = std::collections::BTreeSet::new();
        for it in &self.items {
            if let Some((d, _)) = it.path.rsplit_once('/') {
                dirs.insert(d.to_string());
            }
        }
        for d in dirs {
            v.push(Entry::Dir(format!("{t}/src/{d}/")));
        }
        for it in &self.items {
            let name = format!("{t}/src/{}", it.path);
            v.push(if it.mode == 0o120_000 {
                Entry::Symlink {
                    name,
                    target: String::from_utf8(it.content.clone()).unwrap(),
                }
            } else {
                Entry::File {
                    name,
                    data: it.content.clone(),
                    perm: if it.mode == 0o100_755 { 0o755 } else { 0o644 },
                }
            });
        }
        v.push(Entry::File {
            name: format!("{t}/repo.bundle"),
            data: self.bundle.clone(),
            perm: 0o644,
        });
        v.push(Entry::File {
            name: format!("{t}/EVIDENCE.md"),
            data: b"# Evidence\n".to_vec(),
            perm: 0o644,
        });
        with_manifest(&mut v, t);
        v
    }

    /// A copy of the standard entries after `edit`, written to `<dir>/<file>`.
    pub fn zip_edited(&self, file: &str, edit: impl FnOnce(&mut Vec<Entry>)) -> PathBuf {
        let mut e = self.entries();
        edit(&mut e);
        let p = self.dir.join(file);
        write_zip(&p, &e);
        p
    }

    /// Like [`Fixture::zip_edited`], then MANIFEST.sha256 is written again for the edited entries, so the zip is consistent
    /// (a delivery that merely lacks something, not one that was tampered with).
    pub fn zip_edited_consistent(&self, file: &str, edit: impl FnOnce(&mut Vec<Entry>)) -> PathBuf {
        let mut e = self.entries();
        edit(&mut e);
        e.retain(|x| !x.name().ends_with("/MANIFEST.sha256"));
        with_manifest(&mut e, &self.top);
        let p = self.dir.join(file);
        write_zip(&p, &e);
        p
    }

    pub fn zip_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.zip).unwrap()
    }
}

pub fn write_zip(path: &Path, entries: &[Entry]) {
    let f = std::fs::File::create(path).unwrap();
    let mut z = ZipWriter::new(f);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for e in entries {
        match e {
            Entry::Dir(n) => z.add_directory(n, opts.unix_permissions(0o755)).unwrap(),
            Entry::File { name, data, perm } => {
                z.start_file(name, opts.unix_permissions(*perm)).unwrap();
                std::io::Write::write_all(&mut z, data).unwrap();
            }
            Entry::Symlink { name, target } => {
                z.add_symlink(name, target, opts).unwrap();
            }
        }
    }
    z.finish().unwrap();
}

/// Names under `dir` that start with `.ham-clone` (a staging directory left behind).
pub fn staging_left(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".ham-clone"))
                .collect()
        })
        .unwrap_or_default()
}

/// Checks that `dir` holds every item as a file with its bytes and mode (a symlink with its target).
pub fn assert_tree(dir: &Path, items: &[Item]) {
    use std::os::unix::fs::PermissionsExt;
    for it in items {
        let p = dir.join(&it.path);
        if it.mode == 0o120_000 {
            let target = std::fs::read_link(&p).unwrap();
            assert_eq!(
                target.to_string_lossy().as_bytes(),
                it.content.as_slice(),
                "{}",
                it.path
            );
        } else {
            assert_eq!(std::fs::read(&p).unwrap(), it.content, "{}", it.path);
            let exec = std::fs::metadata(&p).unwrap().permissions().mode() & 0o111 != 0;
            assert_eq!(exec, it.mode == 0o100_755, "{} exec bit", it.path);
        }
    }
}

/// Appends MANIFEST.sha256 as the packager writes it: `<sha256>  <path>` for every other file under `top`, sorted by path.
pub fn with_manifest(v: &mut Vec<Entry>, top: &str) {
    let mut lines = std::collections::BTreeMap::new();
    for e in v.iter() {
        let (name, bytes) = match e {
            Entry::Dir(_) => continue,
            Entry::File { name, data, .. } => (name, data.clone()),
            Entry::Symlink { name, target } => (name, target.clone().into_bytes()),
        };
        let rel = name.strip_prefix(&format!("{top}/")).unwrap().to_string();
        lines.insert(rel, support::sha256_hex(&bytes));
    }
    let manifest: String = lines.iter().map(|(p, h)| format!("{h}  {p}\n")).collect();
    v.push(Entry::File {
        name: format!("{top}/MANIFEST.sha256"),
        data: manifest.into_bytes(),
        perm: 0o644,
    });
}
