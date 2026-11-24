//! What the tests of the kept pack share: the folders of the private repository, the engine's commits, a look that must make
//! a copy, a stand-in for git that can hold, fail, starve or disturb the keep's own `pack-objects`, fail or hold the reading of
//! the tree,
//! hold git's check of a kept pack and write down what the folders of the keep held at each call, and the second process whose
//! search path leads with that stand-in.
#![allow(dead_code, clippy::unwrap_used)]

use crate::b860_thin_support::Lab;
use crate::common::{FIXTURE_DATE, git, scratch};
use agentd::direct::snapshot::{GiveUp, SnapshotMade, Snapshotter, TidyMarks};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How the first line of the marker of a pack the worker keeps starts; the engine's head follows.
pub const KEPT_LINE: &str = "agentd kept ";

/// The variable that makes a test of the stand-in the process whose git calls go through it: it names the folder of the stand-in.
pub const STAND_IN_DIR: &str = "B860_KEPT_DIR";

/// The limit of a look or a keep of these tests: far away.
pub fn limit() -> GiveUp {
    GiveUp::at(Some(tokio::time::Instant::now() + Duration::from_secs(300)))
}

/// The marks of a disk that has less free than any rule of the keep asks for.
pub fn short_disk() -> TidyMarks {
    TidyMarks {
        free_bytes: u64::MAX,
        ..TidyMarks::RUN
    }
}

/// The private repository of the copies under the data folder `data` of `l`.
pub fn private(l: &Lab, data: &str) -> PathBuf {
    l.dir.join(data).join("snapshot.git")
}

/// The folder of the private repository that holds the kept pack.
pub fn kept_folder(l: &Lab, data: &str) -> PathBuf {
    private(l, data).join("kept/pack")
}

/// The folder a keep writes its pack into before the pack stands.
pub fn incoming_folder(l: &Lab, data: &str) -> PathBuf {
    private(l, data).join("kept/incoming")
}

/// The names in the folder `dir`, sorted; none when the folder is not there.
pub fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The packs the worker keeps in the private repository `git_dir`, wherever in it they lie: the marker file of each and the head its
/// first line names.
pub fn kept_markers(git_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    for folder in ["kept/pack", "objects/pack"] {
        let folder = git_dir.join(folder);
        for name in names(&folder) {
            if !name.ends_with(".keep") {
                continue;
            }
            let path = folder.join(&name);
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(head) = text.lines().next().and_then(|l| l.strip_prefix(KEPT_LINE)) {
                found.push((path, head.trim().to_string()));
            }
        }
    }
    found
}

/// The objects the pack beside the marker `marker` holds: the id and the kind of each. `repo` is any repository to run git in.
pub fn objects_of(repo: &Path, marker: &Path) -> Vec<(String, String)> {
    let idx = marker.with_extension("idx");
    git(repo, &["verify-pack", "-v", idx.to_str().unwrap()])
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let id = words.next()?;
            let kind = words.next()?;
            (id.len() == 40 && ["commit", "tree", "blob", "tag"].contains(&kind))
                .then(|| (id.to_string(), kind.to_string()))
        })
        .collect()
}

/// The engine commits `n` times, from round `from`: each commit adds a file and rewrites another, and its objects stay loose in the
/// engine's store. The answer is the engine's head.
pub fn engine_commits(repo: &Path, from: u32, n: u32) -> String {
    std::fs::create_dir_all(repo.join("run")).unwrap();
    for round in from..from + n {
        std::fs::write(
            repo.join(format!("run/file{round:04}.txt")),
            format!("the file of round {round}\n"),
        )
        .unwrap();
        std::fs::write(
            repo.join("run/log.txt"),
            format!("the log after round {round}\n"),
        )
        .unwrap();
        git(repo, &["add", "run"]);
        git(repo, &["commit", "-q", "-m", &format!("round {round}")]);
    }
    git(repo, &["rev-parse", "HEAD"])
}

/// What [`edit`] writes in round `round`.
pub fn work_of(round: u32) -> String {
    format!("work in progress, round {round}\n")
}

/// Work that is in no commit: the tree differs at the next look, and the content names the round.
pub fn edit(repo: &Path, round: u32) {
    std::fs::write(repo.join("work.txt"), work_of(round)).unwrap();
}

/// One look that must make a copy.
pub async fn look(copies: &mut Snapshotter) -> SnapshotMade {
    copies
        .snapshot_until(&limit())
        .await
        .unwrap()
        .expect("the tree changed, so the look makes a copy")
}

/// [`edit`] in round `round`, then [`look`]: every look of these tests that is meant to make a copy is given a changed tree.
pub async fn changed_look(copies: &mut Snapshotter, repo: &Path, round: u32) -> SnapshotMade {
    edit(repo, round);
    look(copies).await
}

/// A fresh repository that holds only the bundle the run started from, with the copy `copy` checked and fetched into it: `git bundle
/// verify`, a fetch of the copy's ref and a connectivity check, with neither the engine's store nor the private repository at hand.
pub fn fetched_into_the_base(l: &Lab, name: &str, copy: &Path) -> PathBuf {
    let fresh = l.dir.join(format!("fresh-{name}"));
    git(
        &l.dir,
        &[
            "clone",
            "-q",
            "--no-checkout",
            l.base_bundle.to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );
    git(&fresh, &["bundle", "verify", "-q", copy.to_str().unwrap()]);
    git(
        &fresh,
        &[
            "fetch",
            "-q",
            copy.to_str().unwrap(),
            "+refs/heads/snapshot:refs/heads/snapshot",
        ],
    );
    git(&fresh, &["fsck", "--no-progress", "--connectivity-only"]);
    fresh
}

// ---- the stand-in for git ---------------------------------------------------------------------------------------------------------

/// A `git` for the front of the search path. It writes one line for each time it is started into `dir/git-calls.log` and then is the
/// real git, but for the keep's own `pack-objects` (the one agentd starts in the private repository of the copies, not the one
/// `git bundle create` starts by itself), which files in `dir` steer:
/// - `keep-fail`: the call fails at once;
/// - `keep-disturb`: `dir/disturb.sh` runs first, once, with the real git (the engine rewrites its history under the keep);
/// - `keep-hold`: the call writes its process id into `keep-running` and waits for as long as `keep-hold` is there;
/// - `keep-short-disk`: no file the call writes may grow beyond a few dozen KiB, as on a disk that fills in the middle of the write;
/// - `keep-hold-after`: the real `pack-objects` runs to its end and writes its pack, then the call writes its process id into
///   `keep-running` and waits for as long as `keep-hold-after` is there: a keep that is cut then is cut after its files exist.
///
/// While `fixed-dates` is there, every commit made through the stand-in carries [`FIXTURE_DATE`] as both of its dates
/// ([`fix_the_dates`]): the worker's git calls take no date from the worker's environment, so the search path is the one place
/// where a test can name them.
///
/// For every call in a private repository of the copies it also writes one line into `dir/kept-seen.log` before it does anything
/// else: what the folder of the kept pack and the folder a keep writes into held when the call started ([`kept_seen`]). Two more
/// files in `dir` steer calls in such a repository:
/// - `add-fail`: every `add` fails at once, so a look fails in the step that reads the tree;
/// - `add-hold`: an `add` writes its process id into `add-running` and waits for as long as `add-hold` is there: the git work of
///   a look takes as long as a test holds it;
/// - `check-hold`: an `index-pack` (git's check of a kept pack when a worker starts) writes its process id into `check-running`
///   and waits for as long as `check-hold` is there: a check that does not end.
///
/// For every `git init` of a private repository of the copies, the first thing a worker that starts writes into it, the stand-in
/// writes one line into `dir/init-seen.log` before it does anything else: what the folder of the kept pack held then
/// ([`init_seen`]).
///
/// The answer is the folder that holds the stand-in.
pub fn stand_in_for_git(dir: &Path) -> PathBuf {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let real_dir = Path::new(&real).parent().unwrap().display().to_string();
    let bin = dir.join("stand-in-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> '{dir}/git-calls.log'
case "$*" in
  *'init -q --bare '*/snapshot.git)
    for a in "$@"; do made="$a"; done
    (
      printf '%s|' "$made"
      for f in "$made"/kept/pack/*; do if [ -e "$f" ]; then printf '%s ' "${{f##*/}}"; fi; done
      printf '\n'
    ) >> '{dir}/init-seen.log' ;;
esac
case "$GIT_DIR" in
  */snapshot.git)
    (
      printf '%s|' "$GIT_DIR"
      for f in "$GIT_DIR"/kept/pack/*; do if [ -e "$f" ]; then printf '%s ' "${{f##*/}}"; fi; done
      printf '|'
      for f in "$GIT_DIR"/kept/incoming/*; do if [ -e "$f" ]; then printf '%s ' "${{f##*/}}"; fi; done
      printf '|%s\n' "$*"
    ) >> '{dir}/kept-seen.log'
    for a in "$@"; do
      if [ "$a" = add ] && [ -e '{dir}/add-fail' ]; then echo 'the test fails this add' >&2; exit 1; fi
      if [ "$a" = add ] && [ -e '{dir}/add-hold' ]; then
        echo $$ > '{dir}/add-running'
        while [ -e '{dir}/add-hold' ]; do sleep 0.05; done
      fi
      if [ "$a" = index-pack ] && [ -e '{dir}/check-hold' ]; then
        echo $$ > '{dir}/check-running'
        while [ -e '{dir}/check-hold' ]; do sleep 0.05; done
      fi
    done ;;
esac
if [ -e '{dir}/fixed-dates' ]; then
  GIT_AUTHOR_DATE='{date}'; GIT_COMMITTER_DATE='{date}'; export GIT_AUTHOR_DATE GIT_COMMITTER_DATE
fi
keep=0
case "$GIT_DIR" in
  */snapshot.git) for a in "$@"; do if [ "$a" = pack-objects ]; then keep=1; fi; done ;;
esac
if [ "$keep" = 1 ]; then
  if [ -e '{dir}/keep-fail' ]; then echo 'the test fails this pack-objects' >&2; exit 1; fi
  if [ -e '{dir}/keep-disturb' ]; then
    mv '{dir}/keep-disturb' '{dir}/keep-disturbed'
    env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_ALTERNATE_OBJECT_DIRECTORIES PATH="{real_dir}:$PATH" \
      sh '{dir}/disturb.sh' >> '{dir}/disturb.log' 2>&1
  fi
  if [ -e '{dir}/keep-hold' ]; then
    echo $$ > '{dir}/keep-running'
    while [ -e '{dir}/keep-hold' ]; do sleep 0.05; done
  fi
  if [ -e '{dir}/keep-short-disk' ]; then ulimit -f 64; fi
  if [ -e '{dir}/keep-hold-after' ]; then
    '{real}' "$@"
    status=$?
    echo $$ > '{dir}/keep-running'
    while [ -e '{dir}/keep-hold-after' ]; do sleep 0.05; done
    exit $status
  fi
fi
exec '{real}' "$@"
"#,
        dir = dir.display(),
        date = FIXTURE_DATE,
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// From now on every commit made through the stand-in in `dir` carries [`FIXTURE_DATE`] as both of its dates.
/// A copy's own commit is otherwise stamped with the second of the
/// clock, and the size of a bundle changes with the bytes of that commit: two copies of the same state, made in two seconds, can
/// differ by a byte. A test that compares the size of two copies makes both in a process whose git calls go through the stand-in
/// ([`second_process`]) and calls this first; the two copies of one state are then one commit, whatever the clock shows.
pub fn fix_the_dates(dir: &Path) {
    std::fs::write(dir.join("fixed-dates"), "").unwrap();
}

/// The git processes agentd and the test started so far through the stand-in, one line each.
pub fn git_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("git-calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// What one git call in a private repository of the copies found when it started, as the stand-in wrote it down before it did
/// anything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The names in the folder of the kept pack.
    pub pack: Vec<String>,
    /// The names in the folder a keep writes into.
    pub incoming: Vec<String>,
    /// The arguments of the call.
    pub call: String,
}

/// The git calls agentd made so far through the stand-in in the private repository `git_dir`, oldest first, each with what the two
/// folders of the keep held when it started.
pub fn kept_seen(dir: &Path, git_dir: &Path) -> Vec<Seen> {
    let words =
        |part: &str| -> Vec<String> { part.split_whitespace().map(str::to_string).collect() };
    std::fs::read_to_string(dir.join("kept-seen.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '|');
            let (repo, pack, incoming, call) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            (Path::new(repo) == git_dir).then(|| Seen {
                pack: words(pack),
                incoming: words(incoming),
                call: call.to_string(),
            })
        })
        .collect()
}

/// What the folder of the kept pack held at each `git init` of the private repository `git_dir` so far, oldest first, as the
/// stand-in wrote it down before git ran: `git init` is the first write of a worker that starts.
pub fn init_seen(dir: &Path, git_dir: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(dir.join("init-seen.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (repo, pack) = line.split_once('|')?;
            (Path::new(repo) == git_dir)
                .then(|| pack.split_whitespace().map(str::to_string).collect())
        })
        .collect()
}

/// Whether the logged call `call` is the keep's own `pack-objects`: it names a file of the private repository of the copies to
/// write to, which no other `pack-objects` of a session does.
pub fn is_the_keep(call: &str) -> bool {
    call.split_whitespace().any(|w| w == "pack-objects") && call.contains("snapshot.git/")
}

/// How many times the keep's own `pack-objects` was started so far.
pub fn keeps_started(dir: &Path) -> usize {
    git_calls(dir).iter().filter(|c| is_the_keep(c)).count()
}

/// The git command of one logged call: the first word after the options agentd gives every git (`-C <folder>`, `-c <setting>`).
pub fn command_of(call: &str) -> &str {
    let mut words = call.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-C" || word == "-c" {
            words.next();
        } else {
            return word;
        }
    }
    ""
}

/// For a test whose git calls must go through the stand-in: agentd starts `git` by name and a process cannot change its own search
/// path here, so the test starts itself once more with the stand-in first on the path. In that second process the answer is the
/// folder of the stand-in, and the test goes on; in the first one the second process is waited for, must have passed, and the
/// answer is `None`. What the test printed in the second process is printed again by the first, so that a run that shows the
/// output of its tests shows the numbers a test prints for the record.
pub fn second_process(test: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(STAND_IN_DIR) {
        return Some(PathBuf::from(dir));
    }
    let dir = scratch("b860-kept-git");
    let bin = stand_in_for_git(&dir);
    let search = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("PATH", search)
        .env(STAND_IN_DIR, &dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{test}, in the process whose search path leads with the stand-in for git:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    print!("{}", String::from_utf8_lossy(&out.stdout));
    None
}

/// Whether the process `pid` is still there.
pub fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}/stat")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            !stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
}
