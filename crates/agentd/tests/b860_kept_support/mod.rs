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
/// What stands between [`KEPT_LINE`] and the engine's head in the first line of the marker of a pack that was made
/// from a look's bundle: `agentd kept bundle <head>`.
pub const KEPT_BUNDLE_WORD: &str = "bundle ";

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
/// first line names. The head is read from both first lines of a marker, `agentd kept <head>`
/// of release 5.28's keep and `agentd kept bundle <head>` of a pack that was made from a look's bundle ([`made_from_a_bundle`]).
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
                let head = head.strip_prefix(KEPT_BUNDLE_WORD).unwrap_or(head);
                found.push((path, head.trim().to_string()));
            }
        }
    }
    found
}

/// Whether the kept pack beside the marker `marker` was made from a look's bundle (the form): its marker's first line
/// reads `agentd kept bundle <head>`. Such a pack also holds the copy's own commit, its trees and the files no commit holds.
pub fn made_from_a_bundle(marker: &Path) -> bool {
    std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .next()
        .and_then(|l| l.strip_prefix(KEPT_LINE))
        .is_some_and(|rest| rest.starts_with(KEPT_BUNDLE_WORD))
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
/// `git bundle create` starts by itself), which files in `dir` steer. A keep has two such calls, and the five files
/// below mean the older keep's call only (`pack-objects --revs`, [`is_the_older_keep`]), never the filter call of a keep by
/// the form (`pack-objects --stdin-packs`, [`is_the_filter`]):
/// - `keep-fail`: the call fails at once;
/// - `keep-disturb`: `dir/disturb.sh` runs first, once, with the real git (the engine rewrites its history under the keep);
/// - `keep-hold`: the call writes its process id into `keep-running` and waits for as long as `keep-hold` is there;
/// - `keep-short-disk`: no file the call writes may grow beyond a few dozen KiB, as on a disk that fills in the middle of the write;
/// - `keep-hold-after`: the real `pack-objects` runs to its end and writes its pack, then the call writes its process id into
///   `keep-running` and waits for as long as `keep-hold-after` is there: a keep that is cut then is cut after its files exist.
///
/// While one of those five files is there, the stand-in ends the filter call with a non-zero exit and git's words
/// `could not find pack` (what git says when a pack the call names is gone). The form then gives way for that keep without a
/// mark, and the older keep's call, which the file fails or holds, runs in the same wait: a test that sets such a file holds
/// what it held before the form, for the older keep behind a give-way. The form's own git has files of its own:
/// - `filter-gone`: the filter call ends with a non-zero exit and git's words `could not find pack`, and nothing else is steered;
/// - `filter-fail`: the filter call fails at once with other words: a refusal of the form's own;
/// - `filter-hold`: the filter call writes its process id into `filter-running` and waits for as long as `filter-hold` is there;
/// - `index-fail`: an `index-pack --stdin` in a private repository of the copies (the form's indexing, never the start check's
///   `index-pack --verify`) fails at once;
/// - `index-no-name`: such an `index-pack --stdin` ends 0 at once and names no pack (git is not run);
/// - `index-name-no-pack`: such an `index-pack --stdin` ends 0 at once and prints, as git does, the word `pack` and a name
///   of 40 hexadecimal characters, and writes no pack (git is not run);
/// - `filter-bad-index`: the filter call runs to its end, then the index it wrote into `kept/incoming` is overwritten with
///   bytes that are no index, and the call ends as git ended;
/// - `check-hold` (below) holds any `index-pack` of a private repository, the form's indexing too.
///
/// For every `index-pack --stdin` in a private repository of the copies the stand-in writes one line into `dir/index-stdin.log`
/// before git runs ([`index_stdin`]): whether its standard input is a regular file, the position that file is read from, and
/// the four bytes at that position. The position is read from a duplicate of the standard input (the system answers for a
/// descriptor as it is when it is asked, and while a shell reads a file that file is its standard input), and the bytes
/// through a second opening of the file, so git reads from the same place.
///
/// While `fixed-dates` is there, every commit made through the stand-in carries [`FIXTURE_DATE`] as both of its dates
/// ([`fix_the_dates`]): the worker's git calls take no date from the worker's environment, so the search path is the one place
/// where a test can name them.
///
/// For every call in a private repository of the copies it also writes one line into `dir/kept-seen.log` before it does anything
/// else: what the folder of the kept pack and the folder a keep writes into held when the call started ([`kept_seen`]). More
/// files in `dir` steer calls in such a repository:
/// - `add-fail`: every `add` fails at once, so a look fails in the step that reads the tree;
/// - `add-hold`: an `add` writes its process id into `add-running` and waits for as long as `add-hold` is there: the git work of
///   a look takes as long as a test holds it;
/// - `check-hold`: an `index-pack` (git's check of a kept pack when a worker starts) writes its process id into `check-running`
///   and waits for as long as `check-hold` is there: a check that does not end;
/// - `bundle-hold`: a `bundle` (the bundle step of a look) writes its process id into `bundle-running` and waits for as
///   long as `bundle-hold` is there, before git writes the bundle: the engine's head and the engine's store have been read by
///   then, so a test can change them while the look runs, and the git work of the look takes as long as the test holds it.
/// - `count-fail`: a `rev-list --count` (the look's count of what is above the floors) fails at once: the look then
///   writes its bundle whole;
/// - `count-zero`: a `rev-list --count` answers 0 and git is not run: the look then writes its bundle above the
///   head's own parents, as the copy of the end does for a head the base holds.
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
      if [ "$a" = bundle ] && [ -e '{dir}/bundle-hold' ]; then
        echo $$ > '{dir}/bundle-running'
        while [ -e '{dir}/bundle-hold' ]; do sleep 0.05; done
      fi
    done
    case "$*" in
      *'rev-list --count '*)
        if [ -e '{dir}/count-fail' ]; then echo 'the test fails this count' >&2; exit 1; fi
        if [ -e '{dir}/count-zero' ]; then echo 0; exit 0; fi ;;
    esac ;;
esac
if [ -e '{dir}/fixed-dates' ]; then
  GIT_AUTHOR_DATE='{date}'; GIT_COMMITTER_DATE='{date}'; export GIT_AUTHOR_DATE GIT_COMMITTER_DATE
fi
keep=0
filter=0
index=0
fed=0
case "$GIT_DIR" in
  */snapshot.git)
    for a in "$@"; do
      if [ "$a" = pack-objects ]; then keep=1; fi
      if [ "$a" = --stdin-packs ]; then filter=1; fi
      if [ "$a" = index-pack ]; then index=1; fi
      if [ "$a" = --stdin ]; then fed=1; fi
    done ;;
esac
if [ "$index" = 1 ] && [ "$fed" = 1 ]; then
  if [ -f /proc/$$/fd/0 ]; then
    pos=unknown
    exec 4<&0
    while read -r k v; do if [ "$k" = pos: ]; then pos="$v"; fi; done < /proc/$$/fdinfo/4
    exec 4<&-
    (
      printf 'regular|%s|' "$pos"
      dd if=/proc/$$/fd/0 bs=1 skip="$pos" count=4 2>/dev/null
      printf '\n'
    ) >> '{dir}/index-stdin.log'
  else
    printf 'other||\n' >> '{dir}/index-stdin.log'
  fi
  if [ -e '{dir}/index-fail' ]; then echo 'the test fails this index-pack' >&2; exit 1; fi
  if [ -e '{dir}/index-no-name' ]; then exit 0; fi
  if [ -e '{dir}/index-name-no-pack' ]; then
    printf 'pack\t%s\n' 0123456789abcdef0123456789abcdef01234567
    exit 0
  fi
fi
if [ "$keep" = 1 ] && [ "$filter" = 1 ]; then
  keep=0
  for f in keep-fail keep-disturb keep-hold keep-short-disk keep-hold-after filter-gone; do
    if [ -e '{dir}/'"$f" ]; then
      echo "fatal: could not find pack 'the-test-steers-this-keep'" >&2
      exit 128
    fi
  done
  if [ -e '{dir}/filter-fail' ]; then echo 'the test fails this filter' >&2; exit 1; fi
  if [ -e '{dir}/filter-hold' ]; then
    echo $$ > '{dir}/filter-running'
    while [ -e '{dir}/filter-hold' ]; do sleep 0.05; done
  fi
  if [ -e '{dir}/filter-bad-index' ]; then
    '{real}' "$@"
    status=$?
    for f in "$GIT_DIR"/kept/incoming/*.idx; do
      if [ -e "$f" ]; then chmod u+w "$f"; printf 'no index' > "$f"; fi
    done
    exit $status
  fi
fi
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
/// write to, which no other `pack-objects` of a session does. A keep call of either role, the filter call of a keep by
/// the form ([`is_the_filter`]) or the older keep's call ([`is_the_older_keep`]).
pub fn is_the_keep(call: &str) -> bool {
    call.split_whitespace().any(|w| w == "pack-objects") && call.contains("snapshot.git/")
}

/// Whether the logged call `call` is a `pack-objects` with the argument `option` whose last argument names a file of a
/// private repository of the copies: read from the words of the call, the git command first ([`command_of`]).
fn is_a_keep_call_with(call: &str, option: &str) -> bool {
    command_of(call) == "pack-objects"
        && call.split_whitespace().any(|w| w == option)
        && call
            .split_whitespace()
            .last()
            .is_some_and(|w| w.contains("snapshot.git/"))
}

/// Whether the logged call `call` is the filter call of a keep by the form: `pack-objects --stdin-packs` that writes into
/// the private repository of the copies.
pub fn is_the_filter(call: &str) -> bool {
    is_a_keep_call_with(call, "--stdin-packs")
}

/// Whether the logged call `call` is the older keep's call (release 5.28's keep): `pack-objects --revs` that writes into
/// the private repository of the copies.
pub fn is_the_older_keep(call: &str) -> bool {
    is_a_keep_call_with(call, "--revs")
}

/// Whether the logged call `call` is the bundle step of a look: a `bundle` that names `snapshot.bundle` of a private
/// repository of the copies. A keep is due only after a look, and a look that a keep follows wrote its bundle.
pub fn is_the_bundle_step(call: &str) -> bool {
    command_of(call) == "bundle" && call.contains("snapshot.git/snapshot.bundle")
}

/// How many filter calls the logged calls `calls` hold.
pub fn filters_in(calls: &[String]) -> usize {
    calls.iter().filter(|c| is_the_filter(c)).count()
}

/// How many calls of the older keep the logged calls `calls` hold.
pub fn older_keeps_in(calls: &[String]) -> usize {
    calls.iter().filter(|c| is_the_older_keep(c)).count()
}

/// How many filter calls were started so far.
pub fn filters_started(dir: &Path) -> usize {
    filters_in(&git_calls(dir))
}

/// How many calls of the older keep were started so far.
pub fn older_keeps_started(dir: &Path) -> usize {
    older_keeps_in(&git_calls(dir))
}

/// The number of keeps the logged calls `calls` hold, from the two counters by role: every filter call, plus every
/// older keep's call that is not the next keep call after a filter call. A keep by the form starts the filter call; a keep at
/// which the form is off starts the older keep's call; a form that gave way behind its filter starts both, in that order, and
/// is one keep. The older keep's call belongs to the filter call before it only while no bundle step of a look stands between
/// the two ([`is_the_bundle_step`]): a keep that the form made and a later keep at which the form is off are two keeps.
pub fn keeps_in(calls: &[String]) -> usize {
    let (mut keeps, mut behind_a_filter) = (0, false);
    for call in calls {
        if is_the_filter(call) {
            keeps += 1;
            behind_a_filter = true;
        } else if is_the_older_keep(call) {
            if !behind_a_filter {
                keeps += 1;
            }
            behind_a_filter = false;
        } else if is_the_bundle_step(call) {
            behind_a_filter = false;
        }
    }
    keeps
}

/// How many keeps were started so far ([`keeps_in`]). Before a keep started one `pack-objects`; it now starts the filter
/// call, the older keep's call, or the filter call and then the older keep's call, and each of these is one keep.
pub fn keeps_started(dir: &Path) -> usize {
    keeps_in(&git_calls(dir))
}

/// What one git call of a keep is, for a test that asserts the roles and their order in a branch it arranged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepCall {
    /// `index-pack --stdin` in the private repository: the form indexes the look's bundle.
    Index,
    /// The filter call of the form ([`is_the_filter`]).
    Filter,
    /// The older keep's call ([`is_the_older_keep`]).
    Older,
}

/// The git calls of keeps that agentd made so far in the private repository `git_dir`, oldest first, each by its role,
/// from the `skip`-th call of that repository on ([`kept_seen`]): the form's indexing is read from the log where it ran, by the
/// repository it ran in, because its arguments name no file.
pub fn keep_calls_from(dir: &Path, git_dir: &Path, skip: usize) -> Vec<KeepCall> {
    kept_seen(dir, git_dir)
        .iter()
        .skip(skip)
        .filter_map(|seen| {
            let has = |word: &str| seen.call.split_whitespace().any(|w| w == word);
            if has("index-pack") && has("--stdin") {
                Some(KeepCall::Index)
            } else if is_the_filter(&seen.call) {
                Some(KeepCall::Filter)
            } else if is_the_older_keep(&seen.call) {
                Some(KeepCall::Older)
            } else {
                None
            }
        })
        .collect()
}

/// Every git call of keeps so far in the private repository `git_dir`, by role ([`keep_calls_from`]).
pub fn keep_calls(dir: &Path, git_dir: &Path) -> Vec<KeepCall> {
    keep_calls_from(dir, git_dir, 0)
}

/// What the stand-in wrote down of the standard input of each `index-pack --stdin` in a private repository so far,
/// oldest first: whether it is a regular file, the position it is read from, and the four bytes at that position.
pub fn index_stdin(dir: &Path) -> Vec<(bool, Option<u64>, Vec<u8>)> {
    std::fs::read(dir.join("index-stdin.log"))
        .unwrap_or_default()
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.splitn(3, |b| *b == b'|');
            let kind = parts.next().unwrap_or_default();
            let pos = parts
                .next()
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|p| p.trim().parse().ok());
            (
                kind == b"regular",
                pos,
                parts.next().unwrap_or_default().to_vec(),
            )
        })
        .collect()
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
