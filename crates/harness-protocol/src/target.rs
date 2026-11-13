//! Repository targets.
//!
//! - `cc:<name>`: a CodeCommit repository in [`E2E_ACCOUNT`] [`E2E_REGION`], the `accounts.json`
//!   entry the build selected; credential mode `iam`.
//! - `gh:<owner>/<name>`: a GitHub repository; needs the tenant GitHub App (`app` mode). Out of scope for now:
//!   the string parses, and repo-gateway refuses it at the run's first state.
//! - `gl:<group>[/<subgroup>...]/<project>`: a GitLab project delivered through GitLab CI. Not served yet: the
//!   string parses, and repo-gateway refuses it at the run's first state.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

// E2E_ACCOUNT, E2E_REGION, E2E_PROFILE, OPERATOR_POOL_MEMBER, SELECTED_BY and ACCOUNTS: the
// `accounts.json` entry `HAMH_AWS_PROFILE` selected at build time.
include!(concat!(env!("OUT_DIR"), "/aws_target.rs"));
/// The `--new-repo` name forms the harness creates in CodeCommit: `hamh-e2e-new-<yyyymmdd>`
/// and `hamh-e2e-ntier-<kind>-<yyyymmdd>`, `<kind>` 1 to 32 lowercase letters
/// or digits. The `repo-gateway` role may create only these (its `CreateRepository` resources).
pub const NEW_REPO_PREFIXES: [&str; 2] = ["hamh-e2e-new-", "hamh-e2e-ntier-"];
/// COMPAT-CAH-DATA: the forms of the earlier stage generation (`cah-e2e-new-<yyyymmdd>`, `cah-e2e-ntier-<kind>-<yyyymmdd>`). A backend of that
/// generation creates only these, and `ham` asks it for these when only its earlier version header answered. Ends at VERSION 6.00.
pub const NEW_REPO_PREFIXES_EARLIER: [&str; 2] = ["cah-e2e-new-", "cah-e2e-ntier-"];

const TARGET_PATTERN: &str = concat!(
    r"^(cc:[A-Za-z0-9._-]{1,100}",
    r"|gh:[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?/[A-Za-z0-9._-]{1,100}",
    r"|gl:[A-Za-z0-9_][A-Za-z0-9._-]*(?:/[A-Za-z0-9_][A-Za-z0-9._-]*)+)$"
);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RepoTarget {
    CodeCommit { name: String },
    GitHub { owner: String, name: String },
    GitLab { path: String },
}

/// `repo-gateway` credential mode for a target scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialMode {
    /// Role credentials, no secret (`cc:`).
    Iam,
    /// Tenant GitHub App (`gh:`).
    App,
    /// Pulled by a GitLab pipeline schedule; no GitLab credential in AWS (`gl:`).
    GitlabCi,
}

/// How a run starts: from an existing repository, a new repository, or an empty S3 base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum RunTarget {
    Repo { repo: RepoTarget },
    NewRepo { repo: RepoTarget },
    S3Only,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("unknown target scheme in {0:?}; use cc:<name> (the repository target served today)")]
    UnknownScheme(String),
    #[error("invalid {scheme} target {input:?}: {reason}")]
    InvalidName {
        scheme: &'static str,
        input: String,
        reason: String,
    },
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// `yyyymmdd` naming a calendar date.
fn is_calendar_date(date: &str) -> bool {
    date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && {
        let num = |r: std::ops::Range<usize>| date[r].parse::<u32>().unwrap_or(0);
        let (y, m, d) = (num(0..4), num(4..6), num(6..8));
        (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)
    }
}

/// Why `repo-gateway` will not create CodeCommit repository `name` for `--new-repo`; `None` for the
/// [`NEW_REPO_PREFIXES`] forms. The text is the exact reason `ham` shows.
pub fn new_repo_refusal(name: &str) -> Option<String> {
    new_repo_refusal_for(&NEW_REPO_PREFIXES, name)
}

/// [`new_repo_refusal`] for a backend of the earlier stage generation (COMPAT-CAH-DATA): `None` for the [`NEW_REPO_PREFIXES_EARLIER`] forms, and the
/// reason names those forms.
pub fn new_repo_refusal_earlier(name: &str) -> Option<String> {
    new_repo_refusal_for(&NEW_REPO_PREFIXES_EARLIER, name)
}

/// The name an earlier-generation backend takes for the new-repo name `name` that has a [`NEW_REPO_PREFIXES`] form: the same name with the earlier
/// prefix. `None` for any other name (COMPAT-CAH-DATA).
pub fn new_repo_name_earlier(name: &str) -> Option<String> {
    NEW_REPO_PREFIXES
        .iter()
        .zip(NEW_REPO_PREFIXES_EARLIER)
        .find_map(|(now, earlier)| {
            name.strip_prefix(now)
                .map(|rest| format!("{earlier}{rest}"))
        })
}

fn new_repo_refusal_for(prefixes: &[&str; 2], name: &str) -> Option<String> {
    let ok = if let Some(date) = name.strip_prefix(prefixes[0]) {
        is_calendar_date(date)
    } else if let Some(rest) = name.strip_prefix(prefixes[1]) {
        rest.rsplit_once('-').is_some_and(|(kind, date)| {
            (1..=32).contains(&kind.len())
                && kind
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && is_calendar_date(date)
        })
    } else {
        false
    };
    (!ok).then(|| {
        format!(
            "refused: cc:{name} is not a new-repo name; use cc:{}<yyyymmdd> or cc:{}<kind>-<yyyymmdd>",
            prefixes[0], prefixes[1]
        )
    })
}

impl RepoTarget {
    pub fn scheme(&self) -> &'static str {
        match self {
            RepoTarget::CodeCommit { .. } => "cc",
            RepoTarget::GitHub { .. } => "gh",
            RepoTarget::GitLab { .. } => "gl",
        }
    }

    pub fn credential_mode(&self) -> CredentialMode {
        match self {
            RepoTarget::CodeCommit { .. } => CredentialMode::Iam,
            RepoTarget::GitHub { .. } => CredentialMode::App,
            RepoTarget::GitLab { .. } => CredentialMode::GitlabCi,
        }
    }

    /// DynamoDB partition key of the target's allowlist entry.
    pub fn allowlist_key(&self) -> String {
        match self {
            RepoTarget::CodeCommit { name } => format!("REPOALLOW#cc:{E2E_REGION}:{name}"),
            RepoTarget::GitHub { owner, name } => format!("REPOALLOW#{owner}/{name}"),
            RepoTarget::GitLab { path } => format!("REPOALLOW#gl:{path}"),
        }
    }
}

impl fmt::Display for RepoTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoTarget::CodeCommit { name } => write!(f, "cc:{name}"),
            RepoTarget::GitHub { owner, name } => write!(f, "gh:{owner}/{name}"),
            RepoTarget::GitLab { path } => write!(f, "gl:{path}"),
        }
    }
}

fn name_chars_ok(s: &str) -> bool {
    s.bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn invalid(scheme: &'static str, input: &str, reason: &str) -> TargetError {
    TargetError::InvalidName {
        scheme,
        input: input.to_string(),
        reason: reason.to_string(),
    }
}

fn check_codecommit(input: &str, name: &str) -> Result<(), TargetError> {
    if name.is_empty() || name.len() > 100 {
        return Err(invalid(
            "cc",
            input,
            "a CodeCommit name has 1 to 100 characters",
        ));
    }
    if !name_chars_ok(name) {
        return Err(invalid(
            "cc",
            input,
            "only letters, digits, '.', '_' and '-' are allowed",
        ));
    }
    if name.ends_with(".git") || name == "." || name == ".." {
        return Err(invalid(
            "cc",
            input,
            "the name may not end in .git or be . or ..",
        ));
    }
    Ok(())
}

fn check_github(input: &str, owner: &str, name: &str) -> Result<(), TargetError> {
    let owner_ok = (1..=39).contains(&owner.len())
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !owner.starts_with('-')
        && !owner.ends_with('-');
    if !owner_ok {
        return Err(invalid(
            "gh",
            input,
            "the owner has 1 to 39 letters, digits or inner hyphens",
        ));
    }
    if name.is_empty() || name.len() > 100 || !name_chars_ok(name) || name == "." || name == ".." {
        return Err(invalid(
            "gh",
            input,
            "the repository name has 1 to 100 letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

fn check_gitlab(input: &str, path: &str) -> Result<(), TargetError> {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 2 {
        return Err(invalid(
            "gl",
            input,
            "a GitLab target is <group>[/<subgroup>...]/<project>",
        ));
    }
    for s in segments {
        let first_ok = s
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
        if !first_ok || !name_chars_ok(s) || s.ends_with(".git") || s.ends_with(".atom") {
            return Err(invalid(
                "gl",
                input,
                "each path segment starts with a letter, digit or '_' and holds only letters, digits, '.', '_' or '-'",
            ));
        }
    }
    Ok(())
}

impl FromStr for RepoTarget {
    type Err = TargetError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let Some((scheme, rest)) = input.split_once(':') else {
            return Err(TargetError::UnknownScheme(input.to_string()));
        };
        match scheme {
            "cc" => {
                check_codecommit(input, rest)?;
                Ok(RepoTarget::CodeCommit {
                    name: rest.to_string(),
                })
            }
            "gh" => {
                let Some((owner, name)) = rest.split_once('/') else {
                    return Err(invalid("gh", input, "a GitHub target is <owner>/<name>"));
                };
                check_github(input, owner, name)?;
                Ok(RepoTarget::GitHub {
                    owner: owner.to_string(),
                    name: name.to_string(),
                })
            }
            "gl" => {
                check_gitlab(input, rest)?;
                Ok(RepoTarget::GitLab {
                    path: rest.to_string(),
                })
            }
            _ => Err(TargetError::UnknownScheme(input.to_string())),
        }
    }
}

impl Serialize for RepoTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RepoTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for RepoTarget {
    fn schema_name() -> Cow<'static, str> {
        "RepoTarget".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::RepoTarget").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "Repository target: cc:<name>, a CodeCommit repository in the deployment's account and home Region. Only `cc:` targets are served: the gh:<owner>/<name> and gl:<group>/<project> forms parse and are refused, because GitHub and GitLab delivery are not supported.",
            "pattern": TARGET_PATTERN,
        })
    }
}
