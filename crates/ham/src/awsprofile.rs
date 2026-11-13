//! The kind of an AWS profile, read from the AWS files by section and key names only: no value is read into the program. ham uses the
//! kind to choose how a profile signs: IAM Identity Center (SSO) and assume-role profiles through the AWS CLI ([`crate::awscli`]),
//! everything else through the credential providers of aws-config ([`crate::sigv4`]).
//!
//! setup.sh holds the same definition in its function `profile_kind`. `crates/ham/tests/aws_profile_kinds.rs` and
//! `scripts/tests/test_setup_script.py` run the two over the same fixture directory, `fixtures/aws-profiles/`, so they cannot drift apart.
//!
//! The rules, which both follow line by line:
//! - The files are the AWS config file (`AWS_CONFIG_FILE`, else `~/.aws/config`) and the credentials file (`AWS_SHARED_CREDENTIALS_FILE`,
//!   else `~/.aws/credentials`), the config file first, each read only when it exists.
//! - A section header is a line whose first non-blank character is `[`. The name is what stands between the `[` and the first `]`, trimmed,
//!   without a leading `profile` and the blanks after it. Text after the `]` (a comment) is ignored.
//! - The keys of a profile are the lines under its headers, in either file, up to the next header. A key is the text before the first `=`,
//!   trimmed (a line without `=` is a key as a whole). Comment lines start with `#` or `;` and so name no key of ours.
//! - The kind is the first that applies: a `credential_process` key; an `aws_access_key_id` key (static keys); a key that starts with `sso_`
//!   (IAM Identity Center); a `role_arn` or `web_identity_token_file` key (assume-role); otherwise unknown. A name that no header gives is missing.

use std::path::PathBuf;

/// What a profile name is, by the keys under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    /// No file defines the profile (or there is no AWS file at all).
    Missing,
    /// A `credential_process` profile: ham reads its credentials through aws-config, which runs the command.
    CredentialProcess,
    /// A profile with `aws_access_key_id` and a secret key.
    StaticKeys,
    /// A profile with `sso_*` keys: IAM Identity Center. The AWS CLI turns the sign-in into credentials.
    Sso,
    /// A profile with `role_arn` (with `source_profile` or `credential_source`) or a web identity token file. The AWS CLI assumes the role.
    AssumeRole,
    /// The profile exists and holds none of the keys above (for example a profile that only names a Region).
    Unknown,
}

impl ProfileKind {
    /// The word setup.sh prints for the kind (`profile_kind` in setup.sh).
    pub fn word(self) -> &'static str {
        match self {
            ProfileKind::Missing => "missing",
            ProfileKind::CredentialProcess => "credential_process",
            ProfileKind::StaticKeys => "static",
            ProfileKind::Sso => "sso",
            ProfileKind::AssumeRole => "assume",
            ProfileKind::Unknown => "unknown",
        }
    }

    /// True for the kinds whose credentials come from `aws configure export-credentials`.
    pub fn uses_aws_cli(self) -> bool {
        matches!(self, ProfileKind::Sso | ProfileKind::AssumeRole)
    }
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// A line trimmed the way setup.sh's awk does: blanks and tabs off the front; blanks, tabs and a carriage return off the back.
fn trim(s: &str) -> &str {
    s.trim_start_matches(is_blank)
        .trim_end_matches(|c| is_blank(c) || c == '\r')
}

/// The profile name of a section header line, or `None` when the line is no header.
fn header_name(line: &str) -> Option<&str> {
    let line = trim(line);
    let inner = line.strip_prefix('[')?;
    let inner = inner.split(']').next().unwrap_or(inner);
    let inner = trim(inner);
    match inner.strip_prefix("profile") {
        Some(rest) if rest.starts_with(is_blank) => Some(trim(rest)),
        _ => Some(inner),
    }
}

/// The kind of profile `name` in these file texts (the config file first, then the credentials file).
pub fn kind_in(files: &[&str], name: &str) -> ProfileKind {
    let (mut found, mut process, mut keys, mut sso, mut role) = (false, false, false, false, false);
    for text in files {
        let mut current = false;
        for line in text.lines() {
            if trim_start_is_header(line) {
                current = header_name(line) == Some(name);
                found |= current;
                continue;
            }
            if !current {
                continue;
            }
            let key = trim(line.split('=').next().unwrap_or(line));
            match key {
                "credential_process" => process = true,
                "aws_access_key_id" => keys = true,
                "role_arn" | "web_identity_token_file" => role = true,
                k if k.starts_with("sso_") => sso = true,
                _ => {}
            }
        }
    }
    if !found {
        ProfileKind::Missing
    } else if process {
        ProfileKind::CredentialProcess
    } else if keys {
        ProfileKind::StaticKeys
    } else if sso {
        ProfileKind::Sso
    } else if role {
        ProfileKind::AssumeRole
    } else {
        ProfileKind::Unknown
    }
}

fn trim_start_is_header(line: &str) -> bool {
    line.trim_start_matches(is_blank).starts_with('[')
}

/// The AWS config file and the credentials file, where they exist.
fn aws_file_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let pick = |var: &str, under_home: &str| -> Option<PathBuf> {
        match std::env::var_os(var).filter(|v| !v.is_empty()) {
            Some(path) => Some(PathBuf::from(path)),
            None => home.as_ref().map(|h| h.join(under_home)),
        }
    };
    [
        pick("AWS_CONFIG_FILE", ".aws/config"),
        pick("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials"),
    ]
    .into_iter()
    .flatten()
    .filter(|p| p.is_file())
    .collect()
}

/// The kind of profile `name` in the AWS files of this user. A file that cannot be read counts as absent.
pub fn profile_kind(name: &str) -> ProfileKind {
    let texts: Vec<String> = aws_file_paths()
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    kind_in(&refs, name)
}
