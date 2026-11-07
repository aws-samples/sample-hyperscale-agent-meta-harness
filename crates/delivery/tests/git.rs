//! The git layer: the credential helper hands git exactly the signed CodeCommit credential (a
//! local HTTP server that demands Basic auth records what git sends), git never prompts, pushes
//! go only to the harness ref, and bundle reads report what a bundle holds.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::*;
use delivery::checks::harness_ref;
use delivery::credential::GitCredential;
use delivery::git::{EMPTY_TREE, Git, Remote};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Answers 401 with a Basic challenge until a request carries Authorization, records it, then 404.
fn basic_auth_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(6) {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut auth = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line
                    .strip_prefix("Authorization: ")
                    .or_else(|| line.strip_prefix("authorization: "))
                {
                    auth = Some(v.trim().to_string());
                }
            }
            let resp = match auth {
                Some(a) => {
                    seen2.lock().unwrap().push(a);
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                }
                None => {
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"codecommit\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                }
            };
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    (port, seen)
}

fn setup() -> (tempfile::TempDir, Git, std::path::PathBuf) {
    let t = tmp();
    let home = t.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let repo = t.path().join("repo.git");
    (t, Git::new("git", &home), repo)
}

#[tokio::test]
async fn git_sends_the_helper_credential_as_basic_auth() {
    let (_t, git, repo) = setup();
    git.init_bare(&repo).await.unwrap();
    let (port, seen) = basic_auth_server();
    // A CodeCommit git password: the signing time and a 64-hex-digit signature, built at run time so no
    // credential-shaped literal is committed.
    let signature: String = (0..32u32)
        .map(|i| format!("{:02x}", (i * 97 + 81) % 256))
        .collect();
    let cred = GitCredential {
        username: "AKIDEXAMPLE%test-session-token".into(),
        password: format!("20260925T120000Z{signature}"),
    };
    let remote = Remote {
        url: format!("http://127.0.0.1:{port}/v1/repos/hamh-e2e-x"),
        credential: Some(cred.clone()),
    };
    let err = git
        .ls_remote(&repo, &remote, "refs/heads/main")
        .await
        .unwrap_err();
    let shown = err.to_string();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.first().map(String::as_str),
        Some(
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{}", cred.username, cred.password))
            )
            .as_str()
        ),
        "{seen:?}"
    );
    // The error names the remote but never the credential.
    assert!(!shown.contains("test-session-token"), "{shown}");
    assert!(!shown.contains(&signature[..8]), "{shown}");
}

#[tokio::test]
async fn without_a_credential_git_fails_at_once_and_never_prompts() {
    let (_t, git, repo) = setup();
    git.init_bare(&repo).await.unwrap();
    let (port, seen) = basic_auth_server();
    let remote = Remote {
        url: format!("http://127.0.0.1:{port}/v1/repos/hamh-e2e-x"),
        credential: None,
    };
    let start = Instant::now();
    assert!(
        git.ls_remote(&repo, &remote, "refs/heads/main")
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn push_sends_one_commit_to_the_harness_ref_and_ls_remote_reads_it_back() {
    let (t, git, repo) = setup();
    let (work, shas) = work_repo(t.path(), "work", &[&[("a.txt", "a\n")]]);
    let remote_dir = t.path().join("remotes");
    std::fs::create_dir_all(&remote_dir).unwrap();
    let bare = publish(&work, &remote_dir, "hamh-e2e-x");
    let remote = Remote {
        url: bare.display().to_string(),
        credential: None,
    };
    git.init_bare(&repo).await.unwrap();
    git.fetch(&repo, &remote, &["+refs/heads/main:refs/heads/main"])
        .await
        .unwrap();
    let hr = harness_ref(RUN).unwrap();
    git.push(&repo, &remote, &shas[0], &hr).await.unwrap();
    assert_eq!(
        git.ls_remote(&repo, &remote, hr.as_str()).await.unwrap(),
        Some(shas[0].clone())
    );
    assert_eq!(
        git.ls_remote(&repo, &remote, "refs/heads/nope")
            .await
            .unwrap(),
        None
    );
    let r = refs(&bare);
    assert_eq!(r.len(), 2, "{r:?}");
}

#[tokio::test]
async fn an_empty_commit_has_the_empty_tree_the_harness_identity_and_the_given_time() {
    let (_t, git, repo) = setup();
    git.init_bare(&repo).await.unwrap();
    let sha = git
        .empty_commit(&repo, "ham: empty base", 1_790_337_600)
        .await
        .unwrap();
    assert_eq!(
        common::git(&repo, &["rev-parse", &format!("{sha}^{{tree}}")]),
        EMPTY_TREE
    );
    assert_eq!(
        common::git(&repo, &["show", "-s", "--format=%an <%ae> %at %s", &sha]),
        "ham-harness <ham-harness@hyperscale-agent-meta-harness.invalid> 1790337600 ham: empty base"
    );
    // Deterministic: the same time gives the same commit.
    assert_eq!(
        git.empty_commit(&repo, "ham: empty base", 1_790_337_600)
            .await
            .unwrap(),
        sha
    );
}

#[tokio::test]
async fn bundle_heads_and_verify_report_what_a_bundle_holds() {
    let (t, git, repo) = setup();
    let (work, _) = work_repo(t.path(), "work", &[&[("a.txt", "a\n")]]);
    let f = t.path().join("v.bundle");
    let head = verified_bundle(&work, RUN, &f);
    git.init_bare(&repo).await.unwrap();
    git.bundle_verify(&repo, &f).await.unwrap();
    assert_eq!(
        git.bundle_heads(&repo, &f).await.unwrap(),
        vec![(head.clone(), format!("refs/heads/harness/{RUN}"))]
    );
    let junk = t.path().join("junk.bundle");
    std::fs::write(&junk, b"not a bundle").unwrap();
    assert!(git.bundle_verify(&repo, &junk).await.is_err());
}

#[tokio::test]
async fn root_entry_reads_one_entry_of_the_committed_root_tree_by_exact_name() {
    // The pull-request check is attached from the root entry `buildspec.yml` of the delivered head.
    let (t, git, _) = setup();
    let (work, _) = work_repo(
        t.path(),
        "work",
        &[&[
            ("a.txt", "a\n"),
            ("buildspec.yml", "version: 0.2\n"),
            ("ci/buildspec.yml", "nested\n"),
            ("Buildspec.yml.txt", "other\n"),
            ("dir/b.txt", "b\n"),
        ]],
    );
    let head = common::git(&work, &["rev-parse", "HEAD"]);
    let e = git
        .root_entry(&work, &head, "buildspec.yml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (e.mode.as_str(), e.kind.as_str(), e.path.as_str()),
        ("100644", "blob", "buildspec.yml")
    );
    assert_eq!(
        e.sha,
        common::git(&work, &["rev-parse", &format!("{head}:buildspec.yml")])
    );
    // A folder is a tree entry; a nested file and a name that only starts the same are not the root entry.
    let d = git.root_entry(&work, &head, "dir").await.unwrap().unwrap();
    assert_eq!((d.mode.as_str(), d.kind.as_str()), ("040000", "tree"));
    for name in [
        "Buildspec.yml",
        "buildspec.yaml",
        "b.txt",
        "ci/buildspec.yml",
        "build",
    ] {
        assert_eq!(
            git.root_entry(&work, &head, name).await.unwrap(),
            None,
            "{name}"
        );
    }
}

#[tokio::test]
async fn root_entry_reports_links_modes_and_submodules_and_fails_on_a_commit_that_is_not_there() {
    let (t, git, _) = setup();
    let (work, _) = work_repo(t.path(), "work", &[&[("a.txt", "a\n")]]);
    std::os::unix::fs::symlink("a.txt", work.join("buildspec.yml")).unwrap();
    common::git(&work, &["add", "-A"]);
    common::git(&work, &["commit", "-q", "-m", "link"]);
    let link = common::git(&work, &["rev-parse", "HEAD"]);
    // Asked from a sub-folder: the lookup is against the root tree of the commit, never against the working directory.
    std::fs::create_dir_all(work.join("sub")).unwrap();
    let l = git
        .root_entry(&work.join("sub"), &link, "buildspec.yml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((l.mode.as_str(), l.kind.as_str()), ("120000", "blob"));
    // A submodule entry: mode 160000, kind commit.
    common::git(&work, &["rm", "-q", "--cached", "buildspec.yml"]);
    common::git(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},buildspec.yml", "b".repeat(40)),
        ],
    );
    common::git(&work, &["commit", "-q", "-m", "submodule"]);
    let sub = common::git(&work, &["rev-parse", "HEAD"]);
    let s = git
        .root_entry(&work, &sub, "buildspec.yml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((s.mode.as_str(), s.kind.as_str()), ("160000", "commit"));
    // A commit that is not in the repository is an error, never "no such entry".
    assert!(
        git.root_entry(&work, &"c".repeat(40), "buildspec.yml")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn root_entry_of_a_commit_with_an_empty_root_tree_is_none_and_not_an_error() {
    // The empty-base commit of a `--new-repo` run that committed nothing has no entry at all. That is "no build file", and the
    // machine must not retry it as a failed lookup.
    let (t, git, _) = setup();
    let (work, _) = work_repo(t.path(), "work", &[&[("a.txt", "a\n")]]);
    let full = common::git(&work, &["rev-parse", "HEAD"]);
    let empty = common::git(&work, &["commit-tree", EMPTY_TREE, "-m", "empty base"]);
    // The trigger: the commit's root tree is the empty tree.
    assert_eq!(
        common::git(&work, &["rev-parse", &format!("{empty}^{{tree}}")]),
        EMPTY_TREE
    );
    assert_eq!(
        git.root_entry(&work, &empty, "buildspec.yml")
            .await
            .unwrap(),
        None
    );
    // The answer belongs to the commit: the same repository still answers for a commit whose root tree holds the name.
    assert!(
        git.root_entry(&work, &full, "a.txt")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        git.root_entry(&work, &full, "buildspec.yml").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn root_entry_reads_names_with_a_tab_a_newline_or_non_ascii_letters_byte_for_byte() {
    // `ls-tree -z` lists raw names. The first tab of a record ends its mode, type and hash part, so a name that holds a tab, a newline or an
    // umlaut must neither hide the real build file nor stand in for it. The names sort before and after `buildspec.yml`.
    let (t, git, _) = setup();
    let tab = "a\tbuildspec.yml";
    let newline = "x\nbuildspec.yml";
    let (work, _) = work_repo(
        t.path(),
        "work",
        &[&[
            ("buildspec.yml", "version: 0.2\n"),
            (tab, "a tab in the name\n"),
            (newline, "a newline in the name\n"),
            ("ünï.txt", "umlauts\n"),
        ]],
    );
    let head = common::git(&work, &["rev-parse", "HEAD"]);
    // The trigger: the tree really holds four entries, so the lookups below choose among them.
    assert_eq!(
        common::git(&work, &["ls-tree", "-z", "--name-only", &head])
            .split('\0')
            .filter(|n| !n.is_empty())
            .count(),
        4
    );
    let real = git
        .root_entry(&work, &head, "buildspec.yml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(real.path, "buildspec.yml");
    assert_eq!(
        real.sha,
        common::git(&work, &["rev-parse", &format!("{head}:buildspec.yml")])
    );
    for name in [tab, newline, "ünï.txt"] {
        let e = git
            .root_entry(&work, &head, name)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no entry for {name:?}"));
        assert_eq!(e.path, name);
        assert_ne!(e.sha, real.sha, "{name:?} is not the build file");
        assert_eq!(
            e.sha,
            common::git(&work, &["rev-parse", &format!("{head}:{name}")]),
            "{name:?}"
        );
    }
    // A part of a name is not the name.
    for name in ["a", "x", "uni.txt", "ÜNÏ.txt", "yml", "buildspec.yml\t"] {
        assert_eq!(
            git.root_entry(&work, &head, name).await.unwrap(),
            None,
            "{name:?}"
        );
    }
}
