//! `git` as a child process. Every call runs with an empty environment plus `PATH`, a scratch
//! `HOME`, no system or global config, no hooks or fsmonitor, no prompts and no redirects. A
//! remote's credential reaches git only through a one-shot credential helper that reads two
//! environment variables of that one child, so it is never in a URL, a file or an argument.

use crate::checks::{HARNESS_REF_PREFIX, HarnessRef};
use crate::credential::GitCredential;
use crate::error::DeliveryError;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// The empty tree's object id (SHA-1 repositories).
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Author and committer of harness-authored commits (the new-repo base).
pub const HARNESS_NAME: &str = "ham-harness";
pub const HARNESS_EMAIL: &str = "ham-harness@hyperscale-agent-meta-harness.invalid";

const GUARDS: &[&str] = &[
    "-c",
    "safe.directory=*",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "http.followRedirects=false",
    // Resets any helper list; the per-call helper below is the only one.
    "-c",
    "credential.helper=",
];

/// Answers `get` with the two variables and ignores `store` and `erase`.
const HELPER: &str = "credential.helper=!f() { test \"$1\" = get || exit 0; printf 'username=%s\\npassword=%s\\n' \"$HAMH_GIT_USERNAME\" \"$HAMH_GIT_PASSWORD\"; }; f";

/// A fetch or push target: an HTTPS URL, a local path or a bundle file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub url: String,
    pub credential: Option<GitCredential>,
}

impl Remote {
    pub fn local(path: &Path) -> Self {
        Remote {
            url: path.display().to_string(),
            credential: None,
        }
    }
}

/// One entry of `git diff-tree --name-status`: `A`, `M`, `D` or `T`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub status: char,
    pub path: String,
}

/// The attributes `git archive` runs under (see [`Git::verbatim_archives`]).
const VERBATIM_ATTRIBUTES: &str =
    "* -export-ignore -export-subst -text -eol -ident -filter -working-tree-encoding\n";

/// One blob or gitlink of `git ls-tree -r -l`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: String,
    pub sha: String,
    /// `None` for a gitlink.
    pub size: Option<u64>,
    pub path: String,
}

/// A blob that a range of commits adds: its object id, its size and the first path it was seen at (`git rev-list --objects` names an
/// object once).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddedBlob {
    pub sha: String,
    pub size: u64,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct Git {
    bin: PathBuf,
    home: PathBuf,
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

/// A full object id (SHA-1: 40 lowercase hexadecimal characters).
fn is_object_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reads the answer of `git cat-file --batch` for `count` blobs: `<id> blob <size>\n<size bytes>\n` per object. Every record is
/// read at a checked offset; a missing object, another type, a size that is not a number or runs past the end, a missing line break
/// and any byte left over are errors, so a cut-off answer is never taken for the content of a file.
pub fn parse_blobs(data: &[u8], count: usize) -> Result<Vec<Vec<u8>>, DeliveryError> {
    let bad = |why: &str| DeliveryError::unavailable(format!("git cat-file: {why}"));
    let mut out = Vec::with_capacity(count.min(1024));
    let mut at = 0usize;
    for _ in 0..count {
        let rest = data
            .get(at..)
            .ok_or_else(|| bad("the answer is cut short"))?;
        let nl = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| bad("a header has no line break"))?;
        let header = std::str::from_utf8(&rest[..nl]).map_err(|_| bad("a header is not text"))?;
        let mut f = header.split(' ');
        let (_id, kind, size) = (f.next(), f.next(), f.next());
        if kind != Some("blob") || f.next().is_some() {
            return Err(bad("an object is missing or is not a blob"));
        }
        let size: usize = size
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("a size is not a number"))?;
        let start = at
            .checked_add(nl + 1)
            .ok_or_else(|| bad("an offset overflows"))?;
        let end = start
            .checked_add(size)
            .ok_or_else(|| bad("a size overflows"))?;
        let content = data
            .get(start..end)
            .ok_or_else(|| bad("a size runs past the end of the answer"))?;
        if data.get(end) != Some(&b'\n') {
            return Err(bad("a blob is not followed by a line break"));
        }
        out.push(content.to_vec());
        at = end + 1;
    }
    if at != data.len() {
        return Err(bad("the answer has more bytes than the objects asked for"));
    }
    Ok(out)
}

impl Git {
    pub fn new(bin: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Git {
            bin: bin.into(),
            home: home.into(),
        }
    }

    fn command(&self, dir: &Path, remote: Option<&Remote>) -> Command {
        let mut c = Command::new(&self.bin);
        c.current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("LC_ALL", "C")
            .args(GUARDS)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(cred) = remote.and_then(|r| r.credential.as_ref()) {
            c.args(["-c", HELPER])
                .env("HAMH_GIT_USERNAME", &cred.username)
                .env("HAMH_GIT_PASSWORD", &cred.password);
        }
        c
    }

    async fn output(
        &self,
        dir: &Path,
        remote: Option<&Remote>,
        args: &[&str],
        envs: &[(&str, String)],
    ) -> Result<std::process::Output, DeliveryError> {
        let mut c = self.command(dir, remote);
        c.args(args);
        for (k, v) in envs {
            c.env(k, v);
        }
        c.output()
            .await
            .map_err(|e| DeliveryError::unavailable(format!("git {}: {e}", args[0])))
    }

    async fn run(
        &self,
        dir: &Path,
        remote: Option<&Remote>,
        args: &[&str],
    ) -> Result<String, DeliveryError> {
        let out = self.output(dir, remote, args, &[]).await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git {}: {}",
                args[0],
                tail(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `run` with extra environment variables (no remote).
    async fn run_env(
        &self,
        dir: &Path,
        args: &[&str],
        envs: &[(&str, String)],
    ) -> Result<String, DeliveryError> {
        let out = self.output(dir, None, args, envs).await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git {}: {}",
                args[0],
                tail(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub async fn init_bare(&self, repo: &Path) -> Result<(), DeliveryError> {
        std::fs::create_dir_all(repo)?;
        self.run(
            repo,
            None,
            &["init", "-q", "--bare", "--initial-branch=main", "."],
        )
        .await
        .map(|_| ())
    }

    /// `git fetch --no-tags` of the refspecs from a remote or bundle.
    pub async fn fetch(
        &self,
        repo: &Path,
        remote: &Remote,
        refspecs: &[&str],
    ) -> Result<(), DeliveryError> {
        let mut args = vec![
            "fetch",
            "-q",
            "--no-tags",
            "--no-write-fetch-head",
            remote.url.as_str(),
        ];
        args.extend_from_slice(refspecs);
        self.run(repo, Some(remote), &args).await.map(|_| ())
    }

    /// The object id a remote advertises for exactly `name`.
    pub async fn ls_remote(
        &self,
        repo: &Path,
        remote: &Remote,
        name: &str,
    ) -> Result<Option<String>, DeliveryError> {
        let out = self
            .run(
                repo,
                Some(remote),
                &["ls-remote", remote.url.as_str(), name],
            )
            .await?;
        Ok(out.lines().find_map(|l| {
            let (sha, r) = l.split_once('\t')?;
            (r == name).then(|| sha.to_string())
        }))
    }

    /// Pushes one commit to the harness ref, never forced; the ref type admits no other target.
    pub async fn push(
        &self,
        repo: &Path,
        remote: &Remote,
        sha: &str,
        dst: &HarnessRef,
    ) -> Result<(), DeliveryError> {
        if !dst.as_str().starts_with(HARNESS_REF_PREFIX) || sha.contains(':') {
            return Err(DeliveryError::Refused(format!(
                "refused: push to {}; repo-gateway pushes only {HARNESS_REF_PREFIX}<runId>",
                dst.as_str()
            )));
        }
        let refspec = format!("{sha}:{}", dst.as_str());
        self.run(
            repo,
            Some(remote),
            &[
                "push",
                "-q",
                "--porcelain",
                "--no-verify",
                remote.url.as_str(),
                refspec.as_str(),
            ],
        )
        .await
        .map(|_| ())
    }

    /// A parentless commit of the empty tree by the harness identity at `unix` seconds.
    pub async fn empty_commit(
        &self,
        repo: &Path,
        message: &str,
        unix: u64,
    ) -> Result<String, DeliveryError> {
        let tree = self.run(repo, None, &["mktree"]).await?;
        let date = format!("@{unix} +0000");
        let envs = [
            ("GIT_AUTHOR_NAME", HARNESS_NAME.to_string()),
            ("GIT_AUTHOR_EMAIL", HARNESS_EMAIL.to_string()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", HARNESS_NAME.to_string()),
            ("GIT_COMMITTER_EMAIL", HARNESS_EMAIL.to_string()),
            ("GIT_COMMITTER_DATE", date),
        ];
        let out = self
            .output(repo, None, &["commit-tree", &tree, "-m", message], &envs)
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git commit-tree: {}",
                tail(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// A commit by the harness identity at `unix` seconds whose tree is `parent`'s plus `files`
    /// (path, content; regular files), with `parent` its only parent. Returns the commit and each
    /// file's blob id, in order. A temporary index keeps the bare repository untouched otherwise.
    pub async fn commit_files(
        &self,
        repo: &Path,
        parent: &str,
        files: &[(&str, &str)],
        message: &str,
        unix: u64,
    ) -> Result<(String, Vec<String>), DeliveryError> {
        let scratch_dir = crate::files::scratch(repo.parent().unwrap_or(repo), "index-")?;
        let index = [(
            "GIT_INDEX_FILE",
            scratch_dir.path().join("index").display().to_string(),
        )];
        self.run_env(repo, &["read-tree", parent], &index).await?;
        let mut blobs = Vec::new();
        for (i, (path, body)) in files.iter().enumerate() {
            let f = scratch_dir.path().join(format!("blob-{i}"));
            std::fs::write(&f, body)?;
            let f = f.display().to_string();
            let blob = self
                .run(repo, None, &["hash-object", "-w", "--no-filters", &f])
                .await?;
            let info = format!("100644,{blob},{path}");
            self.run_env(
                repo,
                &["update-index", "--add", "--cacheinfo", &info],
                &index,
            )
            .await?;
            blobs.push(blob);
        }
        let tree = self.run_env(repo, &["write-tree"], &index).await?;
        let date = format!("@{unix} +0000");
        let envs = [
            ("GIT_AUTHOR_NAME", HARNESS_NAME.to_string()),
            ("GIT_AUTHOR_EMAIL", HARNESS_EMAIL.to_string()),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", HARNESS_NAME.to_string()),
            ("GIT_COMMITTER_EMAIL", HARNESS_EMAIL.to_string()),
            ("GIT_COMMITTER_DATE", date),
        ];
        let commit = self
            .run_env(
                repo,
                &["commit-tree", &tree, "-p", parent, "-m", message],
                &envs,
            )
            .await?;
        Ok((commit, blobs))
    }

    pub async fn update_ref(
        &self,
        repo: &Path,
        name: &str,
        sha: &str,
    ) -> Result<(), DeliveryError> {
        self.run(repo, None, &["update-ref", name, sha])
            .await
            .map(|_| ())
    }

    pub async fn symbolic_head(&self, repo: &Path, target: &str) -> Result<(), DeliveryError> {
        self.run(repo, None, &["symbolic-ref", "HEAD", target])
            .await
            .map(|_| ())
    }

    /// The commit a revision names, if it exists.
    pub async fn rev_parse(&self, repo: &Path, rev: &str) -> Result<Option<String>, DeliveryError> {
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .output(repo, None, &["rev-parse", "--verify", "-q", &spec], &[])
            .await?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }

    pub async fn bundle_create(
        &self,
        repo: &Path,
        out: &Path,
        refs: &[&str],
    ) -> Result<(), DeliveryError> {
        let out = out.display().to_string();
        let mut args = vec!["bundle", "create", "-q", out.as_str()];
        args.extend_from_slice(refs);
        self.run(repo, None, &args).await.map(|_| ())
    }

    /// `git bundle verify`; the error carries git's reason.
    pub async fn bundle_verify(&self, repo: &Path, bundle: &Path) -> Result<(), DeliveryError> {
        let b = bundle.display().to_string();
        self.run(repo, None, &["bundle", "verify", "-q", &b])
            .await
            .map(|_| ())
    }

    /// `(sha, ref)` for every ref a bundle records.
    pub async fn bundle_heads(
        &self,
        repo: &Path,
        bundle: &Path,
    ) -> Result<Vec<(String, String)>, DeliveryError> {
        let b = bundle.display().to_string();
        let out = self.run(repo, None, &["bundle", "list-heads", &b]).await?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(s, r)| (s.to_string(), r.to_string()))
            .collect())
    }

    /// Net changes from `base` (a commit or [`EMPTY_TREE`]) to `head`, renames as delete plus add.
    pub async fn diff_tree(
        &self,
        repo: &Path,
        base: &str,
        head: &str,
    ) -> Result<Vec<Change>, DeliveryError> {
        let out = self
            .output(
                repo,
                None,
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-renames",
                    "--no-commit-id",
                    "--name-status",
                    base,
                    head,
                ],
                &[],
            )
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git diff-tree: {}",
                tail(&out.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let fields: Vec<&str> = text.split('\0').filter(|f| !f.is_empty()).collect();
        Ok(fields
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| Change {
                status: c[0].chars().next().unwrap_or('?'),
                path: c[1].to_string(),
            })
            .collect())
    }

    /// Every entry of the tree at `rev`, recursively, with blob sizes.
    pub async fn ls_tree(&self, repo: &Path, rev: &str) -> Result<Vec<TreeEntry>, DeliveryError> {
        let out = self
            .output(
                repo,
                None,
                &["ls-tree", "-r", "-l", "-z", "--full-tree", rev],
                &[],
            )
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git ls-tree: {}",
                tail(&out.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .split('\0')
            .filter_map(|rec| {
                let (meta, path) = rec.split_once('\t')?;
                let mut f = meta.split_whitespace();
                let (mode, kind, sha, size) = (f.next()?, f.next()?, f.next()?, f.next()?);
                Some(TreeEntry {
                    mode: mode.into(),
                    kind: kind.into(),
                    sha: sha.into(),
                    size: size.parse().ok(),
                    path: path.into(),
                })
            })
            .collect())
    }

    /// The entry named exactly `name` in the root tree of the commit `rev`, or `None` when that tree has no such entry (a folder is one
    /// `tree` entry and is not listed into). It reads the committed tree (`ls-tree --full-tree`), never the working directory, and compares
    /// the whole name byte for byte, so a pathspec rule (case folding, a glob) cannot match another name. A commit that is not in the
    /// repository is an error, never "no entry". The listing is made without sizes, so `size` of the entry is `None`.
    pub async fn root_entry(
        &self,
        repo: &Path,
        rev: &str,
        name: &str,
    ) -> Result<Option<TreeEntry>, DeliveryError> {
        let out = self
            .output(repo, None, &["ls-tree", "-z", "--full-tree", rev], &[])
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git ls-tree: {}",
                tail(&out.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for rec in text.split('\0') {
            let Some((meta, path)) = rec.split_once('\t') else {
                continue;
            };
            if path != name {
                continue;
            }
            let mut f = meta.split_whitespace();
            let (Some(mode), Some(kind), Some(sha)) = (f.next(), f.next(), f.next()) else {
                return Err(DeliveryError::unavailable(format!(
                    "git ls-tree: unreadable entry for {name}"
                )));
            };
            return Ok(Some(TreeEntry {
                mode: mode.into(),
                kind: kind.into(),
                sha: sha.into(),
                size: None,
                path: path.into(),
            }));
        }
        Ok(None)
    }

    /// The bytes of the blob at `<rev>:<path>`.
    pub async fn blob(&self, repo: &Path, rev: &str, path: &str) -> Result<Vec<u8>, DeliveryError> {
        let spec = format!("{rev}:{path}");
        let out = self
            .output(repo, None, &["cat-file", "blob", &spec], &[])
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git cat-file: {}",
                tail(&out.stderr)
            )));
        }
        Ok(out.stdout)
    }

    /// The bytes of the blob `sha`.
    pub async fn cat_blob(&self, repo: &Path, sha: &str) -> Result<Vec<u8>, DeliveryError> {
        let out = self
            .output(repo, None, &["cat-file", "blob", sha], &[])
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git cat-file: {}",
                tail(&out.stderr)
            )));
        }
        Ok(out.stdout)
    }

    /// `git` with `input` on its standard input; the output is read while the input is written, so neither side can block the other.
    async fn output_with_input(
        &self,
        dir: &Path,
        args: &[&str],
        input: Vec<u8>,
    ) -> Result<std::process::Output, DeliveryError> {
        use tokio::io::AsyncWriteExt;
        let mut c = self.command(dir, None);
        c.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let fail = |e: std::io::Error| DeliveryError::unavailable(format!("git {}: {e}", args[0]));
        let mut child = c.spawn().map_err(fail)?;
        let mut stdin = child.stdin.take();
        let write = async move {
            if let Some(mut s) = stdin.take() {
                // A closed pipe means git stopped reading; its exit status says why.
                let _ = s.write_all(&input).await;
                let _ = s.shutdown().await;
            }
        };
        let (_, out) = tokio::join!(write, child.wait_with_output());
        out.map_err(fail)
    }

    /// The blobs that `base..head` adds, in the order `git rev-list --objects` lists them (`base` of `None` means all of `head`). A
    /// blob is listed once, at the first path git names for it. The commits' own trees are not listed.
    pub async fn added_blobs(
        &self,
        repo: &Path,
        base: Option<&str>,
        head: &str,
    ) -> Result<Vec<AddedBlob>, DeliveryError> {
        let negative = base.map(|b| format!("^{b}"));
        let mut args = vec!["rev-list", "--objects"];
        if let Some(n) = &negative {
            args.push(n.as_str());
        }
        args.push(head);
        let out = self.output(repo, None, &args, &[]).await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git rev-list: {}",
                tail(&out.stderr)
            )));
        }
        let listing = String::from_utf8_lossy(&out.stdout);
        let mut named: Vec<(&str, &str)> = Vec::new();
        for line in listing.lines() {
            let (sha, path) = line.split_once(' ').unwrap_or((line, ""));
            if is_object_id(sha) && !path.is_empty() {
                named.push((sha, path));
            }
        }
        if named.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for (sha, _) in &named {
            input.extend_from_slice(sha.as_bytes());
            input.push(b'\n');
        }
        let out = self
            .output_with_input(
                repo,
                &[
                    "cat-file",
                    "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                ],
                input,
            )
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git cat-file: {}",
                tail(&out.stderr)
            )));
        }
        let checked = String::from_utf8_lossy(&out.stdout);
        let mut blobs = Vec::new();
        let mut answers = checked.lines();
        for (sha, path) in named {
            let line = answers.next().ok_or_else(|| {
                DeliveryError::unavailable("git cat-file: fewer answers than objects")
            })?;
            let mut f = line.split(' ');
            if f.next() != Some(sha) {
                return Err(DeliveryError::unavailable(
                    "git cat-file: an answer is for another object",
                ));
            }
            if let (Some("blob"), Some(size)) = (f.next(), f.next())
                && let Ok(size) = size.parse::<u64>()
            {
                blobs.push(AddedBlob {
                    sha: sha.to_string(),
                    size,
                    path: path.to_string(),
                });
            }
        }
        Ok(blobs)
    }

    /// The bytes of each blob in `shas`, in the same order (`git cat-file --batch`). An object that is missing, is not a blob or
    /// is cut short is an error.
    pub async fn blobs(&self, repo: &Path, shas: &[&str]) -> Result<Vec<Vec<u8>>, DeliveryError> {
        if shas.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = Vec::new();
        for sha in shas {
            if !is_object_id(sha) {
                return Err(DeliveryError::unavailable("git cat-file: not an object id"));
            }
            input.extend_from_slice(sha.as_bytes());
            input.push(b'\n');
        }
        let out = self
            .output_with_input(repo, &["cat-file", "--batch"], input)
            .await?;
        if !out.status.success() {
            return Err(DeliveryError::unavailable(format!(
                "git cat-file: {}",
                tail(&out.stderr)
            )));
        }
        parse_blobs(&out.stdout, shas.len())
    }

    pub async fn is_ancestor(
        &self,
        repo: &Path,
        ancestor: &str,
        head: &str,
    ) -> Result<bool, DeliveryError> {
        let out = self
            .output(
                repo,
                None,
                &["merge-base", "--is-ancestor", ancestor, head],
                &[],
            )
            .await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(DeliveryError::unavailable(format!(
                "git merge-base: {}",
                tail(&out.stderr)
            ))),
        }
    }

    /// Makes `git archive` copy the tree verbatim: `$GIT_DIR/info/attributes` outranks every
    /// in-tree `.gitattributes`, so `export-ignore` and `export-subst` cannot hide or rewrite files,
    /// and no conversion changes a file's bytes on the way out (`text` and `eol` line endings,
    /// `ident` expansion, a `filter` driver, a `working-tree-encoding`): every entry's bytes are its
    /// committed blob, which `ham clone` and verify row R12 compare with the bundle's tree.
    pub async fn verbatim_archives(&self, repo: &Path) -> Result<(), DeliveryError> {
        let info = repo.join("info");
        std::fs::create_dir_all(&info)?;
        std::fs::write(info.join("attributes"), VERBATIM_ATTRIBUTES)?;
        Ok(())
    }

    /// `git archive --format=zip --prefix=<prefix>` of `rev` into `out`.
    pub async fn archive_zip(
        &self,
        repo: &Path,
        rev: &str,
        prefix: &str,
        out: &Path,
    ) -> Result<(), DeliveryError> {
        let prefix = format!("--prefix={prefix}");
        let out = format!("--output={}", out.display());
        self.run(repo, None, &["archive", "--format=zip", &prefix, &out, rev])
            .await
            .map(|_| ())
    }

    /// Whether `sha` is a parentless commit of the empty tree with exactly this message.
    pub async fn is_empty_root(
        &self,
        repo: &Path,
        sha: &str,
        message: &str,
    ) -> Result<bool, DeliveryError> {
        let out = self
            .output(repo, None, &["show", "-s", "--format=%T%n%P%n%B", sha], &[])
            .await?;
        if !out.status.success() {
            return Ok(false);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let (tree, parents) = (lines.next(), lines.next());
        let body: Vec<&str> = lines.collect();
        Ok(tree == Some(EMPTY_TREE) && parents == Some("") && body.join("\n").trim() == message)
    }

    /// Committer time of `rev` in Unix seconds.
    pub async fn commit_time(&self, repo: &Path, rev: &str) -> Result<u64, DeliveryError> {
        let out = self
            .run(repo, None, &["show", "-s", "--format=%ct", rev])
            .await?;
        out.parse()
            .map_err(|_| DeliveryError::unavailable(format!("git show: commit time {out:?}")))
    }
}
