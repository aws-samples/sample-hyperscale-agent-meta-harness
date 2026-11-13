//! The rules of `ham update` that need no process, file or network: where the newest version comes from (`update_source`, with the
//! word `off` as the off switch), versions compared as numbers, the plan (up to date, which parts change, what is refused) and the changelog
//! sections between two versions. The command itself lives in `ham::update`.

use crate::push::check_git_url;

/// The repository `ham update` reads when `update_source` is not set: the public repository, whose main branch carries one commit per release.
pub const DEFAULT_SOURCE: &str =
    "https://github.com/aws-samples/sample-hyperscale-agent-meta-harness.git";

/// The value of `update_source` that turns `ham update` off.
pub const OFF: &str = "off";

/// Where the newest version comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// `update_source = "off"`: `ham update` makes no network call.
    Off,
    /// A git URL: `https://`, or a `file:///` path written out on purpose (a local mirror).
    Url(String),
}

/// The source a value of `update_source` names: unset or empty is [`DEFAULT_SOURCE`], `off` is [`Source::Off`], anything else must be an
/// `https://` URL or a `file:///` path that passes the checks of a `ham push` URL.
pub fn source(setting: Option<&str>) -> Result<Source, String> {
    let value = setting.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(Source::Url(DEFAULT_SOURCE.to_string()));
    }
    if value == OFF {
        return Ok(Source::Off);
    }
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("file:///")) {
        return Err(format!(
            "update_source must be an https:// URL, a file:/// path or the word off, not {value:?}"
        ));
    }
    check_git_url(value).map_err(|p| format!("update_source: {p}"))?;
    Ok(Source::Url(value.to_string()))
}

/// A version `N.NN` (the root `VERSION` file) as numbers: the major number and the two-digit counter. Anything else is `None`.
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || minor.len() != 2 || !digits(minor) {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// What an update changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parts {
    BackendAndClient,
    /// The backend runs the newest version already; setup installs the client of that commit.
    ClientOnly,
    /// The client runs the newest version already; setup deploys the backend (and fetches the same client again).
    BackendOnly,
}

impl Parts {
    pub fn words(self) -> &'static str {
        match self {
            Parts::BackendAndClient => "backend and client",
            Parts::ClientOnly => "client only",
            Parts::BackendOnly => "backend only",
        }
    }
}

/// Why an update does not go ahead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The backend sent no version, so ham cannot tell whether main is newer.
    BackendUnknown,
    /// The client or the backend runs a newer version than main: an update never moves a version back.
    Downgrade { part: &'static str, runs: String },
    /// A version that is not `N.NN`.
    BadVersion(String),
}

/// The plan of an update from the client's version, the backend's and the newest one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    UpToDate,
    Update(Parts),
    Refuse(Refusal),
}

/// The plan for `client`, `backend` (`None`: the backend sent no version) and `newest`, the version at the head of main.
pub fn plan(client: &str, backend: Option<&str>, newest: &str) -> Plan {
    let Some(z) = parse_version(newest) else {
        return Plan::Refuse(Refusal::BadVersion(newest.to_string()));
    };
    let Some(x) = parse_version(client) else {
        return Plan::Refuse(Refusal::BadVersion(client.to_string()));
    };
    let Some(backend) = backend else {
        return Plan::Refuse(Refusal::BackendUnknown);
    };
    let Some(y) = parse_version(backend) else {
        return Plan::Refuse(Refusal::BadVersion(backend.to_string()));
    };
    if x > z {
        return Plan::Refuse(Refusal::Downgrade {
            part: "ham",
            runs: client.to_string(),
        });
    }
    if y > z {
        return Plan::Refuse(Refusal::Downgrade {
            part: "the backend",
            runs: backend.to_string(),
        });
    }
    match (x < z, y < z) {
        (false, false) => Plan::UpToDate,
        (true, true) => Plan::Update(Parts::BackendAndClient),
        (true, false) => Plan::Update(Parts::ClientOnly),
        (false, true) => Plan::Update(Parts::BackendOnly),
    }
}

/// One version's section of `CHANGELOG.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub version: String,
    /// The heading line, `## [N.NN] - YYYY-MM-DD`.
    pub heading: String,
    /// The lines under the heading, up to the next version heading, trimmed.
    pub body: String,
}

/// The version of a heading line `## [N.NN] ...`; `None` for `## [Unreleased]` and any other line.
fn heading_version(line: &str) -> Option<String> {
    let rest = line.strip_prefix("## [")?;
    let (v, _) = rest.split_once(']')?;
    parse_version(v).map(|_| v.to_string())
}

/// Every version section of a changelog, in file order (newest first).
fn sections(changelog: &str) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    let mut open = false;
    for line in changelog.lines() {
        if line.starts_with("## ") {
            open = false;
            if let Some(version) = heading_version(line) {
                out.push(Change {
                    version,
                    heading: line.to_string(),
                    body: String::new(),
                });
                open = true;
            }
            continue;
        }
        if open && let Some(last) = out.last_mut() {
            last.body.push_str(line);
            last.body.push('\n');
        }
    }
    for c in &mut out {
        c.body = c.body.trim().to_string();
    }
    out
}

/// The sections whose version is above `above` and at most `upto`, newest first. `## [Unreleased]` is never one of them.
pub fn changes_between(changelog: &str, above: &str, upto: &str) -> Vec<Change> {
    let (Some(low), Some(high)) = (parse_version(above), parse_version(upto)) else {
        return Vec::new();
    };
    sections(changelog)
        .into_iter()
        .filter(|c| parse_version(&c.version).is_some_and(|v| v > low && v <= high))
        .collect()
}

/// The version of the newest dated heading, which should equal the `VERSION` file of the same commit.
pub fn newest_heading(changelog: &str) -> Option<String> {
    sections(changelog).into_iter().next().map(|c| c.version)
}
