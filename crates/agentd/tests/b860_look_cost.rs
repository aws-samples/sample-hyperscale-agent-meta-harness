//! What a look of the copy task costs the work in front of it.
//!
//! A long command that uses both processors of a session is slowed by the copies' git work. The measurement of the save's cost named
//! where that work goes; these tests hold the rules that make a look cheaper without making a copy later, rarer or less complete: the
//! settings of agentd's private repositories, the hash of a copy's bundle, which is made once and counted as the look's work, and the
//! git processes a look starts: how many, and which.
//!
//! `b860_hashing_counts_as_the_looks_work` reads a counter of the whole process (`agentd::s3::digests_made`): it needs a process of
//! its own, which `cargo nextest` gives every test and plain `cargo test` does not.
#![allow(clippy::unwrap_used)]

mod b860_support;
mod common;

use agentd::direct::driver::GitProbe;
use agentd::direct::goal::tree::TreeFp;
use agentd::direct::snapshot::{BundleForm, GitSource, GiveUp, Snapshotter, hash_bundle};
use agentd::s3::S3Objects;
use b860_support::bucket::{Bucket, Plan};
use b860_support::{MIB, hash_of, noise};
use common::{creds, git, python_calc, scratch};
use harness_protocol::bundle_header::{HEADER_BYTES_MAX, heads, needs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The settings a private repository of a working session carries, and their values.
const SET: [(&str, &str); 4] = [
    ("pack.threads", "1"),
    ("pack.window", "0"),
    ("gc.auto", "0"),
    ("core.untrackedCache", "false"),
];
/// The settings that stay git's own: no private repository names them.
const NOT_SET: [&str; 4] = [
    "core.preloadIndex",
    "index.threads",
    "pack.compression",
    "core.looseCompression",
];

/// The value of the setting `key` in the own configuration of the repository at `git_dir`; `None` when that file does not set it.
fn setting(git_dir: &Path, key: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(git_dir.join("config"))
        .args(["--get", key])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_string())
}

/// The four private repositories of a working session pack with one thread and without a delta search, run no automatic `gc` and keep
/// no untracked cache. Before, a copy's pack ran on both processors of the session with a delta search, for a pack that is uploaded at
/// once. The settings are written once, and the repository of a read of a stopped session's storage has none of them.
#[tokio::test]
async fn b860_private_repositories_carry_their_settings() {
    let dir = scratch("b860-settings");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let _copies = Snapshotter::new(&data, &repo, &base).await.unwrap();
    let _goal = TreeFp::open(&data, &repo, &[]).await.unwrap();
    let _probe = GitProbe::open(&data, &repo).await;
    let _facts = GitSource::open(&data, &repo, &base).await.unwrap();
    for name in ["snapshot.git", "goal.git", "probe.git", "facts.git"] {
        let git_dir = data.join(name);
        assert!(git_dir.join("config").is_file(), "{name} was made");
        for (key, value) in SET {
            assert_eq!(
                setting(&git_dir, key).as_deref(),
                Some(value),
                "TRIGGER PRESENT: {name} does not set {key}"
            );
        }
        for key in NOT_SET {
            assert_eq!(setting(&git_dir, key), None, "{name} sets {key}");
        }
    }

    // A later session on the same machine finds the repository: its settings are written once.
    let _again = Snapshotter::new(&data, &repo, &base).await.unwrap();
    let config = std::fs::read_to_string(data.join("snapshot.git/config")).unwrap();
    assert_eq!(
        config.matches("threads = 1").count(),
        1,
        "the settings are written once:\n{config}"
    );
    assert_eq!(
        setting(&data.join("snapshot.git"), "core.logAllRefUpdates").as_deref(),
        Some("false"),
        "the setting the repository had before stays"
    );

    // The repository of a read of a stopped session's storage stays as release 5.23 has it.
    let read = dir.join("read-back");
    std::fs::create_dir_all(&read).unwrap();
    let _back = Snapshotter::for_read_back(&read, &repo, &base)
        .await
        .unwrap();
    for (key, _) in SET {
        assert_eq!(
            setting(&read.join("snapshot.git"), key),
            None,
            "the repository of a read sets {key}"
        );
    }
}

/// The hash of a copy's bundle is processor time of the copy, so the look makes it as the last part of its own work, under the look's
/// limit, where the spacing of the looks counts it; the upload then sends the bundle under that hash and does not hash it again. Before,
/// the upload hashed the bundle after the look's work had ended, so the spacing did not count that time. The copy of the end has no
/// hash from a look: its store makes it.
#[tokio::test]
async fn b860_hashing_counts_as_the_looks_work() {
    let dir = scratch("b860-hash");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut copies = Snapshotter::new(&data, &repo, &base).await.unwrap();
    // A file that does not compress: the bundle is several megabytes, so its hash takes a moment.
    std::fs::write(repo.join("data.bin"), noise(1, 4 * MIB as usize)).unwrap();
    let limit = GiveUp::at(Some(tokio::time::Instant::now() + Duration::from_secs(120)));
    let mut made = copies.snapshot_until(&limit).await.unwrap().unwrap();
    assert_eq!(
        made.digest, None,
        "git has made the copy; it has no hash yet"
    );

    // The hash is made within the look's limit, once.
    let hashed_before = agentd::s3::digests_made();
    hash_bundle(&mut made, &limit).await.unwrap();
    let digest = made.digest.clone().expect("the look hashed its bundle");
    let (sha, bytes) = hash_of(&made.bundle);
    assert_eq!(
        (digest.sha256_hex.as_str(), digest.bytes),
        (sha.as_str(), bytes)
    );
    assert_eq!(bytes, made.bundle_bytes);
    assert_eq!(agentd::s3::digests_made() - hashed_before, 1);

    // The upload sends the bundle under that hash and does not hash it again, whatever the number of its tries.
    let bucket = Bucket::start(Plan {
        answer_500: 1,
        ..Plan::default()
    });
    let objects = S3Objects::new("us-east-1".into(), Some(bucket.endpoint.clone()));
    let hashed_before = agentd::s3::digests_made();
    objects
        .put_file_retrying_with(
            &bucket.uri("runs/R/snapshot.bundle"),
            &creds("2099-01-01T00:00:00Z"),
            &made.bundle,
            &digest,
            3,
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(
        agentd::s3::digests_made(),
        hashed_before,
        "the upload does not hash the bundle again"
    );
    let puts = bucket.puts();
    assert_eq!(
        puts.iter().map(|p| p.status).collect::<Vec<_>>(),
        vec![500, 200]
    );
    for put in &puts {
        assert_eq!((put.bytes, put.sha256_hex.as_str()), (bytes, sha.as_str()));
        assert_eq!(
            put.signed_sha256, sha,
            "the request is signed with the look's hash"
        );
    }

    // A hash that the look's limit gives up is a failed look, like a git call that it gives up.
    std::fs::write(repo.join("data.bin"), noise(2, 4 * MIB as usize)).unwrap();
    let mut second = copies.snapshot_until(&limit).await.unwrap().unwrap();
    let over = GiveUp::at(Some(tokio::time::Instant::now()));
    let e = hash_bundle(&mut second, &over).await.unwrap_err();
    assert!(e.contains("given up"), "{e}");
    assert_eq!(second.digest, None);

    // The copy of the end comes without a hash: its store makes it.
    let end = copies.final_copy(None).await.unwrap();
    assert!(end.final_copy);
    assert_eq!(end.digest, None);
}

// ---- the git processes of a look ------------------------------------------------------------------------------------------------

/// The variable that makes [`b860_a_look_stays_inside_its_budget`] the process that looks: it names the test's scratch folder.
const BUDGET_DIR: &str = "B860_BUDGET_DIR";
/// The commit the generated repository of that test starts at, for the process that looks.
const BUDGET_BASE: &str = "B860_BUDGET_BASE";
/// The form of the copies the process that looks writes: `base` for copies that leave out what the bundle the run started from
/// holds, which is the form of the release ([`b860_a_look_above_the_base_stays_inside_its_budget`]); not set for copies that hold
/// the whole history.
const BUDGET_FORM: &str = "B860_BUDGET_FORM";
/// The bundle the run of [`b860_a_look_above_the_base_stays_inside_its_budget`] starts from, in the test's scratch folder.
const BUDGET_BASE_BUNDLE: &str = "repo.bundle";
/// The most git processes a look at an unchanged tree may start: `add -A`, `write-tree`, the read of the engine's head
/// (`rev-parse HEAD`), and the count of the clean-up when the look before it hashed a changed tree. The read of the head is
/// added here on purpose, and it is one call: a commit that changes no file is work of the run, and a look that never read the
/// head left it in no copy until a file changed again or the run ended.
const UNCHANGED_LOOK_GITS: usize = 4;
/// The most git processes a look that makes a copy may start: the eight of a copy (`add -A`, `write-tree`, `rev-parse HEAD`,
/// `cat-file -e` twice, `commit-tree`, `update-ref`, `bundle create`) and the count of the clean-up when the look before it hashed a
/// changed tree.
const CHANGED_LOOK_GITS: usize = 9;
/// The git commands a look at an unchanged tree starts. A look that starts another one fails the test until the command is added
/// here on purpose. `rev-parse` is the read of the engine's head, once a look ([`Looks::look`]).
const UNCHANGED_LOOK_COMMANDS: [&str; 4] = ["add", "write-tree", "rev-parse", "count-objects"];
/// The git commands a look that makes a copy starts.
const CHANGED_LOOK_COMMANDS: [&str; 8] = [
    "add",
    "write-tree",
    "count-objects",
    "rev-parse",
    "cat-file",
    "commit-tree",
    "update-ref",
    "bundle",
];
/// The most git processes a look that makes a copy above the run's base may start, the form of the release: the nine of a copy that
/// holds the whole history and one `rev-list --count`, by which the writer of such a copy learns whether the copy's commit is above
/// the base. It is added here on purpose: a copy above the base is a fraction of the size of a whole one, and the count walks only
/// what the run added to the base.
const CHANGED_LOOK_GITS_ABOVE: usize = 10;
/// The git commands a look that makes a copy above the run's base starts: those of a whole copy and `rev-list`.
const CHANGED_LOOK_COMMANDS_ABOVE: [&str; 9] = [
    "add",
    "write-tree",
    "count-objects",
    "rev-parse",
    "cat-file",
    "commit-tree",
    "update-ref",
    "rev-list",
    "bundle",
];
/// The most git processes the copy of the end above the run's base may start when the session's final head is given: the count of
/// the clean-up when the step before it wrote a copy, `rev-parse` for the head's tree, `update-ref`, `rev-list --count`,
/// `rev-parse` for the head's parents when the head is a commit the base holds, and `bundle create`.
const END_COPY_GITS_ABOVE: usize = 6;
/// The git commands the copy of the end above the run's base starts.
const END_COPY_COMMANDS_ABOVE: [&str; 5] = [
    "count-objects",
    "rev-parse",
    "update-ref",
    "rev-list",
    "bundle",
];
/// The git commands of the clean-up of the private repository. No look of this test may start one: its repository stays far below
/// the marks of the clean-up (64 MiB, 6,700 objects), and a clean-up below them is the walk over the history that every look after a
/// changed one made (0.32 s of processor time on 20,000 commits, where the rest of the look cost 0.03 s).
const CLEAN_UP_COMMANDS: [&str; 3] = ["prune", "repack", "gc"];
/// The most processor time the git processes of one look that copies 200 changed small files may take. It is far above what such a
/// look takes (a few hundredths of a second): the bound catches a look that reads and hashes the whole tree again and again, or packs
/// what it need not pack, never a slow host.
const CHANGED_LOOK_CPU: Duration = Duration::from_secs(5);

/// A repository of `files` small files in folders of a hundred, committed once.
fn generated(dir: &Path, files: usize) -> PathBuf {
    let repo = dir.join("generated");
    for n in 0..files {
        let folder = repo.join(format!("pkg{:02}", n / 100));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(format!("file{n:04}.txt")),
            format!("line one of file {n}\nline two\n"),
        )
        .unwrap();
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

/// A `git` for the front of the search path that writes one line for each time it is started, with what it was started with, into
/// `dir/git-calls.log` and then is the real git. The answer is the folder that holds it.
fn counting_git(dir: &Path) -> PathBuf {
    let found = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(found.stdout).unwrap().trim().to_string();
    assert!(real.starts_with('/'), "git on the search path: {real:?}");
    let bin = dir.join("counting-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\nexec '{real}' \"$@\"\n",
        log = dir.join("git-calls.log").display(),
    );
    let path = bin.join("git");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The git processes that were started so far, one line each ([`counting_git`]).
fn git_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("git-calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The git command of one logged call: the first word after the options agentd gives every git (`-C <folder>`, `-c <setting>`).
fn command_of(call: &str) -> &str {
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

/// The processor time of the processes this process started and has waited for, and of theirs (`cutime` and `cstime` of
/// `/proc/self/stat`, which Linux counts in hundredths of a second).
fn children_cpu() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // `<pid> (<name>) <state> ...`: the fields after the name, the state first; the two times are the 14th and the 15th of them.
    let rest = stat.rsplit_once(')').unwrap().1;
    let ticks: u64 = rest
        .split_whitespace()
        .skip(13)
        .take(2)
        .map(|n| n.parse::<u64>().unwrap())
        .sum();
    Duration::from_millis(ticks * 10)
}

/// Rewrites every tenth file of the generated repository: 200 of 2,000.
fn change_two_hundred(repo: &Path, round: u32) {
    for n in (0..2_000).step_by(10) {
        std::fs::write(
            repo.join(format!("pkg{:02}/file{n:04}.txt", n / 100)),
            format!("line one of file {n}\nchanged in round {round}\n"),
        )
        .unwrap();
    }
}

/// The first bytes of a bundle file: what a reader takes to find its header.
fn header_of(bundle: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(bundle).unwrap();
    bytes.truncate(usize::try_from(HEADER_BYTES_MAX).unwrap());
    bytes
}

/// The looks of one process that counts ([`budget_looks`]), and what they started.
struct Looks<'a> {
    dir: &'a Path,
    limit: GiveUp,
    report: String,
    /// For copies above the run's base: the commit every such copy of a look leaves out, the head of the bundle the run started
    /// from. `None` for copies that hold the whole history.
    above: Option<String>,
}

impl Looks<'_> {
    /// One look, held to its bounds: whether it makes a copy (`copy`), which git commands it starts and how many, and for a look that
    /// copies the processor time they take.
    async fn look(&mut self, copies: &mut Snapshotter, what: &str, copy: bool) {
        let (most, known) = match (copy, &self.above) {
            (true, Some(_)) => (CHANGED_LOOK_GITS_ABOVE, &CHANGED_LOOK_COMMANDS_ABOVE[..]),
            (true, None) => (CHANGED_LOOK_GITS, &CHANGED_LOOK_COMMANDS[..]),
            (false, _) => (UNCHANGED_LOOK_GITS, &UNCHANGED_LOOK_COMMANDS[..]),
        };
        let before = git_calls(self.dir).len();
        let cpu_before = children_cpu();
        let made = copies.snapshot_until(&self.limit).await.unwrap();
        let cpu = children_cpu().saturating_sub(cpu_before);
        let calls = git_calls(self.dir).split_off(before);
        let commands: Vec<&str> = calls.iter().map(|call| command_of(call)).collect();
        let listed = calls.join("\n");
        assert_eq!(made.is_some(), copy, "{what}: a copy was made");
        for command in &commands {
            assert!(
                !CLEAN_UP_COMMANDS.contains(command),
                "TRIGGER PRESENT: {what} started git {command}: the clean-up ran below its marks, and walks the history at this \
                 look:\n{listed}"
            );
            assert!(
                known.contains(command),
                "TRIGGER PRESENT: {what} started git {command}, which no look of its kind starts:\n{listed}"
            );
        }
        // One pass over the tree, one tree written, one read of the engine's head,
        // and for a copy one bundle.
        let once: &[&str] = if copy {
            &["add", "write-tree", "rev-parse", "bundle"]
        } else {
            &["add", "write-tree", "rev-parse"]
        };
        for command in once {
            let times = commands.iter().filter(|c| *c == command).count();
            assert_eq!(
                times, 1,
                "TRIGGER PRESENT: {what} started git {command} {times} times, not once:\n{listed}"
            );
        }
        assert!(
            calls.len() <= most,
            "TRIGGER PRESENT: {what} started {} git processes, more than {most}:\n{listed}",
            calls.len()
        );
        assert!(
            !copy || cpu <= CHANGED_LOOK_CPU,
            "TRIGGER PRESENT: the git processes of {what} took {cpu:?} of processor time"
        );
        // The writer of a copy above the base ran: it counted once, and git named the base as what the copy needs.
        if let (Some(base), Some(made)) = (&self.above, &made) {
            let times = commands.iter().filter(|c| **c == "rev-list").count();
            assert_eq!(
                times, 1,
                "TRIGGER PRESENT: {what} started git rev-list {times} times, not once:\n{listed}"
            );
            assert_eq!(
                needs(&header_of(&made.bundle)).unwrap(),
                vec![base.clone()],
                "TRIGGER ABSENT: the copy of {what} was not written above the run's base"
            );
        }
        self.report.push_str(&format!(
            "{what}: {} git processes ({}), {} ms of processor time\n",
            calls.len(),
            commands.join(" "),
            cpu.as_millis()
        ));
    }

    /// The copy of the end at the session's final head `head`, above the run's base, held to its bounds: which git commands it
    /// starts and how many, that it writes one bundle, and that the bundle names `head` and needs exactly `left_out`.
    async fn end_copy(
        &mut self,
        copies: &mut Snapshotter,
        what: &str,
        head: &str,
        left_out: &[String],
    ) {
        let before = git_calls(self.dir).len();
        let made = copies.final_copy(Some(head)).await.unwrap();
        let calls = git_calls(self.dir).split_off(before);
        let commands: Vec<&str> = calls.iter().map(|call| command_of(call)).collect();
        let listed = calls.join("\n");
        for command in &commands {
            assert!(
                END_COPY_COMMANDS_ABOVE.contains(command),
                "TRIGGER PRESENT: {what} started git {command}, which no copy of the end above the base starts:\n{listed}"
            );
        }
        for command in ["bundle", "rev-list"] {
            let times = commands.iter().filter(|c| **c == command).count();
            assert_eq!(
                times, 1,
                "TRIGGER PRESENT: {what} started git {command} {times} times, not once:\n{listed}"
            );
        }
        assert!(
            calls.len() <= END_COPY_GITS_ABOVE,
            "TRIGGER PRESENT: {what} started {} git processes, more than {END_COPY_GITS_ABOVE}:\n{listed}",
            calls.len()
        );
        assert!(made.final_copy, "{what}");
        assert_eq!(made.commit, head, "{what} names the session's final head");
        assert_eq!(
            needs(&header_of(&made.bundle)).unwrap(),
            left_out,
            "TRIGGER ABSENT: {what} was not written above the run's base"
        );
        self.report.push_str(&format!(
            "{what}: {} git processes ({})\n",
            calls.len(),
            commands.join(" ")
        ));
    }
}

/// The looks of [`b860_a_look_stays_inside_its_budget`], in the process whose search path leads with the counting git.
async fn budget_looks(dir: &Path) {
    let repo = dir.join("generated");
    let base = std::env::var(BUDGET_BASE).unwrap();
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut copies = Snapshotter::new(&data, &repo, &base).await.unwrap();
    copies
        .take_prime_note()
        .expect("the first reading was made")
        .expect("the first reading went through");
    assert!(
        !git_calls(dir).is_empty(),
        "the git of the private repository goes through the counting git"
    );
    // The form of the release: the copies leave out what the bundle the run started from holds, as the worker sets it up from
    // that bundle's own header.
    let above = match std::env::var(BUDGET_FORM).ok().as_deref() {
        Some("base") => {
            let floors = heads(&header_of(&dir.join(BUDGET_BASE_BUNDLE))).unwrap();
            assert!(
                floors.contains(&base),
                "the bundle the run starts from names the base: {floors:?}"
            );
            copies.set_form(BundleForm::AboveFloor);
            copies.set_floors(floors);
            Some(base.clone())
        }
        Some(other) => panic!("{BUDGET_FORM} names no form of a copy: {other}"),
        None => None,
    };
    let limit = GiveUp::at(Some(tokio::time::Instant::now() + Duration::from_secs(300)));
    let mut looks = Looks {
        dir,
        limit,
        report: String::new(),
        above: above.clone(),
    };
    looks
        .look(&mut copies, "the first look, nothing changed", false)
        .await;
    change_two_hundred(&repo, 1);
    looks
        .look(&mut copies, "a look at 200 changed files", true)
        .await;
    looks
        .look(&mut copies, "the look after it, nothing changed", false)
        .await;
    change_two_hundred(&repo, 2);
    looks
        .look(&mut copies, "a second look at 200 changed files", true)
        .await;
    // A look that copies right after a look that copied: the count of the clean-up comes with it.
    change_two_hundred(&repo, 3);
    looks
        .look(
            &mut copies,
            "a look at 200 changed files right after it",
            true,
        )
        .await;
    looks
        .look(&mut copies, "the look after that, nothing changed", false)
        .await;
    if above.is_some() {
        // The session ends: its final head is the commit that holds its work, which is above the base.
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "the work of the session"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        looks
            .end_copy(
                &mut copies,
                "the copy of the end at the session's final head",
                &head,
                std::slice::from_ref(&base),
            )
            .await;
        // A final head that the base holds (a session that made no commit, a reset to an earlier commit): git writes no bundle
        // with nothing in it, so the copy holds that one commit and needs its parent.
        let parent = git(&repo, &["rev-parse", &format!("{base}^")]);
        looks
            .end_copy(
                &mut copies,
                "the copy of the end at a commit the base holds",
                &base,
                &[parent],
            )
            .await;
    }
    let report = looks.report;
    std::fs::write(dir.join("budget-report.txt"), report).unwrap();
}

/// Starts the test `test` of this file once more as the process that looks, with the counting git first on its search path, and
/// answers what that process counted ([`budget_looks`]).
fn looks_in_their_own_process(dir: &Path, test: &str, base: &str, form: Option<&str>) -> String {
    let bin = counting_git(dir);
    let search = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut looking = std::process::Command::new(std::env::current_exe().unwrap());
    looking
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("PATH", search)
        .env(BUDGET_DIR, dir)
        .env(BUDGET_BASE, base);
    if let Some(form) = form {
        looking.env(BUDGET_FORM, form);
    }
    let out = looking.output().unwrap();
    assert!(
        out.status.success(),
        "the looks did not stay inside their budget:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(dir.join("budget-report.txt"))
        .expect("the process that looks wrote what it counted")
}

/// A look stays inside its budget of git processes: on a generated repository of 2,000 files, a look at an unchanged tree starts at most
/// four (`add -A`, `write-tree`, the read of the engine's head, and the count of the clean-up after a look that hashed a changed
/// tree), and a look that copies 200 changed files at most nine, whose processor time stays far below a fixed bound. The test reads
/// what each git was started with: a look reads the tree once (one `add -A`, one `write-tree`) and the engine's head once (one
/// `rev-parse`), a copy writes one bundle, a look starts only the git commands listed above for its kind, and no look starts
/// `prune`, `repack` or `gc`, because the repository of this test stays far below the marks of the clean-up. So a change that puts
/// the clean-up back in front of every look, reads the tree or the head a second time, or adds a git call of any other kind to a
/// look fails here. A look at an unchanged tree whose head stands where the last copy found it makes no copy, so it stores nothing.
///
/// What it does not hold: the time a look takes. The processor time of a look on a repository with a long history, and what the
/// looks cost a long command, are measured on the bench and in the live proof of a release.
///
/// agentd starts `git` by name, so the test counts with a `git` that leads the search path. A process cannot change its own search
/// path here, so the test starts itself once more with that path and the scratch folder named in its environment; that second
/// process makes the looks and holds the bounds, and writes what it counted.
#[tokio::test]
async fn b860_a_look_stays_inside_its_budget() {
    if let Some(dir) = std::env::var_os(BUDGET_DIR) {
        return budget_looks(Path::new(&dir)).await;
    }
    let dir = scratch("b860-budget");
    let repo = generated(&dir, 2_000);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let report =
        looks_in_their_own_process(&dir, "b860_a_look_stays_inside_its_budget", &base, None);
    assert_eq!(report.lines().count(), 6, "six looks were made:\n{report}");
    println!("{report}");
}

/// The budget of [`b860_a_look_stays_inside_its_budget`] for the copies a release writes: copies that leave out what the bundle the
/// run started from holds (`BundleForm::AboveFloor`, with the floors read from that bundle's header, as the worker sets them up).
/// The writer of such a copy asks git once more than the writer of a whole copy: `rev-list --count`, by which it learns whether the
/// copy's commit is above the base. So a look that makes a copy starts at most ten git processes here, one of them `rev-list`, and
/// each copy is read back: its header must name the base as the commit it needs, which shows that the writer of the form ran. A look
/// at an unchanged tree is the same in both forms.
///
/// The same process then makes the copy of the end twice, with the session's final head given. At a head above the base it starts
/// `rev-parse` for the head's tree, `update-ref`, `rev-list --count` and `bundle create`, and the copy needs the base. At a head that
/// the base holds git would write a bundle with nothing in it, so the writer asks for the head's parents (a second `rev-parse`), and
/// the copy holds that one commit and needs its parent; with the count of the clean-up that makes six. The generated repository has a
/// second commit here, so that the base has a parent.
///
/// A change that adds a git call of another kind to the writer of a copy above the base, counts a second time or writes a second
/// bundle fails here. What the test does not hold is the same as for the whole form: the time a look takes.
#[tokio::test]
async fn b860_a_look_above_the_base_stays_inside_its_budget() {
    if let Some(dir) = std::env::var_os(BUDGET_DIR) {
        return budget_looks(Path::new(&dir)).await;
    }
    let dir = scratch("b860-budget-above");
    let repo = generated(&dir, 2_000);
    std::fs::write(
        repo.join("pkg00/file0000.txt"),
        "line one of file 0\nthe second commit of the base\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base, second commit"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    // The bundle a run starts from, as the service stages it: `HEAD` and the branch, one commit under two names.
    git(
        &repo,
        &[
            "bundle",
            "create",
            "-q",
            dir.join(BUDGET_BASE_BUNDLE).to_str().unwrap(),
            "HEAD",
            "refs/heads/main",
        ],
    );
    let report = looks_in_their_own_process(
        &dir,
        "b860_a_look_above_the_base_stays_inside_its_budget",
        &base,
        Some("base"),
    );
    assert_eq!(
        report.lines().count(),
        8,
        "six looks and two copies of the end were made:\n{report}"
    );
    println!("{report}");
}
