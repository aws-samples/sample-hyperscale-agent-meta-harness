//! The pure checks of `ham push <runId> <git-url> [--branch <name>]`.
//! The command builds the run's repository from the delivered zip and pushes its branch to a remote with the user's own
//! git, so it takes nothing secret: a URL that carries a credential is refused before anything is downloaded, and no message here
//! repeats the URL. The work is `crates/ham/src/push.rs`.
//!
//! The URL forms are the ones git users write: `https://` and `http://` (the credential helper answers), `ssh://[user@]host/path`,
//! `git@host:path` and `host:path`, `git://`, `file:///path` and a path on the machine. Everything else is refused: a leading `-` (an
//! option), a transport helper (`ext::...` runs a command), an unknown scheme, white space or control characters.

use std::fmt;

/// Why a git URL is not accepted. [`fmt::Display`] says what to do and never repeats the URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlProblem {
    Empty,
    /// Starts with `-`: git would read it as an option.
    Option,
    /// White space or a control character.
    Spaces,
    /// `ext::...`, `fd::...` and other transport helpers.
    Helper,
    /// A scheme git can speak but `ham push` does not offer (`ftp://`, `git+ssh://`).
    Scheme,
    /// No host after the scheme.
    Host,
    /// A password, or a token in front of the `@`.
    Credentials,
}

impl fmt::Display for UrlProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UrlProblem::Empty => "the git URL is empty",
            UrlProblem::Option => {
                "the git URL starts with a dash, which git would read as an option; give a URL such as https://host/group/repo.git or git@host:group/repo.git"
            }
            UrlProblem::Spaces => "the git URL has a space or a control character in it",
            UrlProblem::Helper => {
                "the git URL names a transport helper (like ext::), which ham push does not use; give an https://, ssh:// or git@host:path URL"
            }
            UrlProblem::Scheme => {
                "the git URL has a scheme ham push does not offer; use https://, http://, ssh://, git://, file:// or git@host:path"
            }
            UrlProblem::Host => "the git URL has no host",
            UrlProblem::Credentials => {
                "the git URL carries a credential (a password, or a token in front of the @). A URL like that ends up in your shell history and in .git/config. Give the plain URL; git then uses your SSH key or your credential helper"
            }
        })
    }
}

impl std::error::Error for UrlProblem {}

/// The schemes of a URL with `://`.
const SCHEMES: [&str; 5] = ["http", "https", "ssh", "git", "file"];

/// Whether `url` is a path on this machine (relative paths included) and not a URL or the scp-like `host:path`. Git's own rule: a
/// colon makes it scp-like unless a slash comes first.
pub fn is_local_path(url: &str) -> bool {
    if url.contains("://") {
        return false;
    }
    match (url.find(':'), url.find('/')) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(colon), Some(slash)) => slash < colon,
    }
}

/// `<transport>::<address>` at the start: git's remote helpers (`ext::sh -c ...` runs a command).
fn is_helper(url: &str) -> bool {
    url.split_once("::").is_some_and(|(transport, _)| {
        !transport.is_empty()
            && transport
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
    })
}

/// Checks the URL a person gives `ham push` (see the module doc). `Ok` means git can be handed it as one argument after `--`.
pub fn check_git_url(url: &str) -> Result<(), UrlProblem> {
    if url.is_empty() {
        return Err(UrlProblem::Empty);
    }
    if url.starts_with('-') {
        return Err(UrlProblem::Option);
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(UrlProblem::Spaces);
    }
    if is_helper(url) {
        return Err(UrlProblem::Helper);
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !SCHEMES.contains(&scheme.as_str()) {
            return Err(UrlProblem::Scheme);
        }
        if scheme == "file" {
            return if rest.starts_with('/') {
                Ok(())
            } else {
                Err(UrlProblem::Host)
            };
        }
        let authority = rest.split('/').next().unwrap_or_default();
        let (userinfo, host) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        if host.is_empty() {
            return Err(UrlProblem::Host);
        }
        return match userinfo {
            // ssh://git@host is the account name every ssh remote has; a password has no place in any URL, and over http, https
            // and git a bare user name is a token (https://<token>@github.com/...).
            Some(u) if scheme != "ssh" || u.contains(':') => Err(UrlProblem::Credentials),
            _ => Ok(()),
        };
    }
    if is_local_path(url) {
        return Ok(());
    }
    // scp-like: [user@]host:path. `user:secret@host:path` has its @ in the first path segment.
    let (host, path) = url.split_once(':').unwrap_or((url, ""));
    if host.is_empty() || host == "@" {
        return Err(UrlProblem::Host);
    }
    if path
        .split('/')
        .next()
        .is_some_and(|first| first.contains('@'))
    {
        return Err(UrlProblem::Credentials);
    }
    Ok(())
}

/// The remote branch a push uses when `--branch` is not given: the run's own branch.
pub fn default_branch(run_id: &str) -> String {
    format!("harness/{run_id}")
}

/// A branch name by git's ref rules (`git check-ref-format --branch`): the rules that matter for a name a person types. Git checks the
/// name again when it pushes; this stops a bad one before the zip is downloaded. The error says why, and starts with the name.
pub fn check_branch(name: &str) -> Result<(), String> {
    let why = |r: &str| Err(format!("{name:?} is not a branch name ({r})"));
    if name.is_empty() {
        return Err("is empty: give a branch name such as main".to_string());
    }
    if name.starts_with('-') {
        return why("it starts with a dash");
    }
    if name == "@" {
        return why("@ alone is not allowed");
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_control() || " ~^:?*[\\".contains(*c))
    {
        return why(&format!("{c:?} is not allowed"));
    }
    for (bad, r) in [
        ("..", "two dots in a row"),
        ("@{", "@{ is not allowed"),
        ("//", "two slashes in a row"),
    ] {
        if name.contains(bad) {
            return why(r);
        }
    }
    if name.starts_with('/') || name.ends_with('/') {
        return why("it starts or ends with a slash");
    }
    if name.ends_with('.') {
        return why("it ends with a dot");
    }
    if name
        .split('/')
        .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        return why("a part of it starts with a dot or ends with .lock");
    }
    Ok(())
}

/// Masks what stands in front of the `@` of every `scheme://user:<secret>@host` and every `user:secret@host:path` word in `text` with
/// `***`, for a message that repeats what a person typed. A plain `git@host:path` has nothing to hide and is left as it is.
pub fn mask_userinfo(text: &str) -> String {
    text.split(' ').map(mask_word).collect::<Vec<_>>().join(" ")
}

fn mask_word(word: &str) -> String {
    let start = word.find("://").map_or(0, |i| i + 3);
    let rest = &word[start..];
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    let Some(at) = authority.rfind('@') else {
        return word.to_string();
    };
    // A scp-like word needs a colon before the @ to hold a secret; a URL's user name is masked whatever it is.
    if start == 0 && !authority[..at].contains(':') {
        return word.to_string();
    }
    format!("{}***{}", &word[..start], &word[start + at..])
}
