//! The rules of `ham update` that need no process, file or network: where the newest version comes from (`update_source`, with the
//! word `off` as the off switch), versions compared as numbers, the plan (up to date, which parts change, what is refused) and the changelog
//! sections between two versions, and how the plan of `setup.sh --update --dry-run` is read for an update that deploys nothing on the stage.
//! The command itself lives in `ham::update`. It also holds the contract of an answer that asks for an update: the ending of
//! its code and the form of its sentence.

use crate::push::check_git_url;

/// The repository `ham update` reads when `update_source` is not set: the public repository, whose main branch carries one commit per release.
pub const DEFAULT_SOURCE: &str =
    "https://github.com/aws-samples/sample-hyperscale-agent-meta-harness.git";

/// The value of `update_source` that turns `ham update` off.
pub const OFF: &str = "off";

/// The `code` of the answer a stage gives a request that would start a run while the stage is being updated: HTTP 409 with the service's own
/// sentence and a `Retry-After` header. The API service sends it and the client shows the sentence alone.
pub const STAGE_UPDATING: &str = "stage-updating";

/// The ending of the `code` of an answer that asks for an update: `<feature>-needs-update`. A feature that an older `ham` cannot handle
/// answers that client with HTTP 409, one sentence that names `ham update` and such a code. The ending is reserved for this meaning: "this
/// client is older than the feature needs, and `ham update` is the remedy". No other code may end in it.
///
/// The service recognises the older client by what its request lacks (a member or a query word that every client from the feature's release
/// on sends), never by a version. It sends the answer only when what the request asks for cannot be given to a client that lacks the member,
/// and it writes nothing and hands nothing out before it answers, so the same request can be repeated after the update. Every released
/// client shows the answer as `HTTP 409: <sentence> (<code>)`.
pub const NEEDS_UPDATE_SUFFIX: &str = "-needs-update";

/// Whether a `code` of the service asks for an update: it ends in [`NEEDS_UPDATE_SUFFIX`] and names a feature before it.
pub fn asks_for_update(code: &str) -> bool {
    code.strip_suffix(NEEDS_UPDATE_SUFFIX)
        .is_some_and(|feature| !feature.is_empty())
}

/// Whether a sentence fits an answer that asks for an update: it holds the words `run ham update`, is one line without a control character,
/// and has at most 240 characters. Its form is `<what cannot be done> needs a newer ham[ to <verb>]; [the work is saved; ]run ham update and
/// ask again`, where "the work is saved" stands only where that is true. The test of a feature that sends such an answer calls this on its
/// sentence and [`asks_for_update`] on its code.
pub fn needs_update_sentence_ok(sentence: &str) -> bool {
    sentence.contains("run ham update")
        && !sentence
            .chars()
            .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
        && sentence.chars().count() <= 240
}

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

/// How the `update:` line of setup's plan begins when the update deploys nothing on the stage (`scripts/setup/full.sh`, `full_plan`).
pub const PLAN_UPDATE_NOTHING: [&str; 2] =
    ["nothing is deployed;", "nothing on the stage changes;"];

/// How the `guard:` line of setup's plan begins when the update deploys nothing: the first with `--wait-for-runs`, the second without it.
pub const PLAN_GUARD_NOTHING: [&str; 2] = [
    "this update changes nothing on the stage,",
    "this update deploys nothing,",
];

/// The texts of the plan lines named `field` (`update`, `guard`): a line that begins with the field name and a colon after its indent.
fn plan_lines<'a>(plan: &'a str, field: &str) -> Vec<&'a str> {
    plan.lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix(field)?
                .strip_prefix(':')
                .map(str::trim)
        })
        .collect()
}

/// Whether the plan that `setup.sh --update --dry-run` printed says that the update deploys nothing on the stage: it has exactly one
/// `update:` line, which begins with one of [`PLAN_UPDATE_NOTHING`], and exactly one `guard:` line, which begins with one of
/// [`PLAN_GUARD_NOTHING`]. Setup decides whether an update deploys (the stage's record, the account file, the settings, the stacks); this
/// reads its decision and makes none. Any other text is `false`: a plan that deploys, a plan of a later release in other words, two plans,
/// one of the lines without the other. `false` only keeps the wait `ham update` always had, so a plan this client does not know costs time
/// and never skips a wait.
pub fn plan_deploys_nothing(plan: &str) -> bool {
    let begins = |lines: Vec<&str>, with: &[&str]| match lines.as_slice() {
        [one] => with.iter().any(|w| one.starts_with(w)),
        _ => false,
    };
    begins(plan_lines(plan, "update"), &PLAN_UPDATE_NOTHING)
        && begins(plan_lines(plan, "guard"), &PLAN_GUARD_NOTHING)
}

// ---- the update notice of the terminal app ---------------------------------------------------------------------------------------
//
// The rules of the notice, with no process, file or network either: which version a listing of the update source names (`newest_of`), when
// a notice is due and its words (`notice_for`, `notice_words`), the web page of an update source (`web_address`) and the sentences after a
// click on the notice, and the schedule of the look (`LookSchedule`). The look itself (one git call) lives in `ham::update`.

/// What the listing of the update source says about its newest release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heads {
    /// The version `ham update` would install, as far as the version tags tell.
    Newest(String),
    /// The source has no main branch or no version tag: nothing to name.
    NoRelease,
    /// The listing is not what git prints for a listing: a look that failed.
    Garbled,
}

/// The id of a line of the listing: 40 lower-case hex digits.
fn is_listing_id(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The version a tag name stands for and whether the name is the peeled line of an annotated tag (`refs/tags/v5.25^{}`); `None` for every
/// name that is no version tag. A version has the form of the root `VERSION` file, with no leading zero in its major number unless that is `0`.
fn version_tag(name: &str) -> Option<(&str, bool)> {
    let rest = name.strip_prefix("refs/tags/v")?;
    let (version, peeled) = match rest.strip_suffix("^{}") {
        Some(v) => (v, true),
        None => (rest, false),
    };
    parse_version(version)?;
    let major = version.split_once('.').map_or(version, |(m, _)| m);
    (major == "0" || !major.starts_with('0')).then_some((version, peeled))
}

/// The newest release a listing names. `listing` is the standard output of `git ls-remote` for `refs/heads/main` and `refs/tags/v*`: one
/// line per name, `<40 lower-case hex><tab><name>`.
///
/// - Only exact names count: `refs/heads/main` and the version tags (`refs/tags/v<version>`, and `refs/tags/v<version>^{}` for the commit of
///   an annotated tag). Every other line of that shape is passed over.
/// - A tag's commit is the id of its `^{}` line when there is one, else the id of its own line.
/// - Newest is the highest version tag whose commit is the commit of main, because `ham update` installs the head of main. When the head of
///   main carries no version tag (someone committed after the release), it is the highest version tag of the listing.
/// - No main branch or no version tag is [`Heads::NoRelease`], and so is an empty listing. A line of another shape, and two lines for the
///   main branch, are [`Heads::Garbled`].
pub fn newest_of(listing: &str) -> Heads {
    let mut main: Option<&str> = None;
    // Per version tag, in the order of the listing: the version, the id of its own line and the id of its `^{}` line.
    let mut tags: Vec<(&str, Option<&str>, Option<&str>)> = Vec::new();
    for line in listing.lines() {
        let Some((id, name)) = line.split_once('\t') else {
            return Heads::Garbled;
        };
        if !is_listing_id(id) || name.is_empty() {
            return Heads::Garbled;
        }
        if name == "refs/heads/main" {
            if main.replace(id).is_some() {
                return Heads::Garbled;
            }
            continue;
        }
        let Some((version, peeled)) = version_tag(name) else {
            continue;
        };
        let at = match tags.iter().position(|(v, _, _)| *v == version) {
            Some(i) => i,
            None => {
                tags.push((version, None, None));
                tags.len() - 1
            }
        };
        if peeled {
            tags[at].2 = Some(id);
        } else {
            tags[at].1 = Some(id);
        }
    }
    let Some(main) = main else {
        return Heads::NoRelease;
    };
    let highest = |at_main: bool| {
        tags.iter()
            .filter(|(_, own, peeled)| !at_main || peeled.or(*own) == Some(main))
            .filter_map(|(version, _, _)| parse_version(version).map(|n| (n, *version)))
            .max()
            .map(|(_, version)| version.to_string())
    };
    match highest(true).or_else(|| highest(false)) {
        Some(version) => Heads::Newest(version),
        None => Heads::NoRelease,
    }
}

/// The update notice of the terminal app: `version` is the newer release that `ham update` can install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateNotice {
    pub version: String,
}

/// The notice that is due, from the client's version, the backend's (`None`: not known) and the newest version the last look named.
/// It has one state: the newest version is known and [`plan`] for it is an update. A backend that differs from the client raises no
/// notice by itself, because only the source tells whether `ham update` has a version that closes the gap.
pub fn notice_for(
    client: &str,
    backend: Option<&str>,
    newest: Option<&str>,
) -> Option<UpdateNotice> {
    let z = newest?;
    matches!(plan(client, backend, z), Plan::Update(_)).then(|| UpdateNotice {
        version: z.to_string(),
    })
}

/// The long and the short text of a notice: `v<version> available` and `update`. The notice only indicates the newer release; it names
/// no key and no command.
pub fn notice_words(notice: &UpdateNotice) -> (String, String) {
    (
        format!("v{} available", notice.version),
        "update".to_string(),
    )
}

/// The web page of an update source, for a click on the notice: the value of `update_source` when it is an `https://` address, without a
/// trailing `.git` and without a trailing slash. The default source gives the page of the public repository. An address with a query
/// or a fragment (`?`, `#`) is the page as it stands: nothing of it is rewritten, so a `.git` in front of the query stays and a query
/// that ends in `.git` or a slash keeps its end. Every other source has no page: a local path, any other scheme, and an address that
/// names a login, holds white space or a control character, or names no host.
///
/// The page goes to the link opener as one argument and no shell reads it, so `&`, `;` and `$` in it are characters of the address.
pub fn web_address(source: &str) -> Option<String> {
    const SCHEME: &str = "https://";
    let source = source.trim();
    if !source
        .get(..SCHEME.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(SCHEME))
        || source.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let page = if source.contains(['?', '#']) {
        source
    } else {
        let page = source.trim_end_matches('/');
        let page = page.strip_suffix(".git").unwrap_or(page);
        page.trim_end_matches('/')
    };
    let rest = page.get(SCHEME.len()..)?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    (!host.is_empty() && !host.contains('@')).then(|| page.to_string())
}

/// How every sentence after a click on the notice ends: the update itself runs from a shell.
pub const TO_UPDATE: &str = "To update, quit ham and run ham update.";

/// What the app says after a click on the notice opened the page of the update source at `address`.
pub fn page_opened_words(address: &str) -> String {
    format!("Opened {address}. {TO_UPDATE}")
}

/// What the app says after a click on the notice when the update source has no web page.
pub fn no_page_words(version: &str) -> String {
    format!("v{version} is available. {TO_UPDATE}")
}

/// What the app says after a click on the notice when the page could not be opened: `failure` is the app's sentence for a link that
/// could not be opened, and how to update follows it.
pub fn page_failed_words(failure: &str) -> String {
    format!("{}. {TO_UPDATE}", failure.trim_end_matches('.'))
}

/// Seconds from a look that ended with a release or with none to the next look.
pub const LOOK_EVERY_S: i64 = 86_400;

/// Seconds from a look that failed to the next look.
pub const LOOK_RETRY_S: i64 = 3_600;

/// When the terminal app looks for a newer release: at its start, a day after a look that ended, an hour after a look that failed, and
/// after a Settings write of one of the two keys. It has no clock of its own: every method that needs the time takes the wall clock in
/// Unix seconds, and "once a day" lives in memory only. At most one look is in flight.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LookSchedule {
    /// Rises with every [`LookSchedule::reset`]; a look carries the generation it started in.
    generation: u64,
    in_flight: bool,
    /// A reset asked for a look that has not started yet.
    asked: bool,
    /// When the last look that applied ended, and the seconds to the next one.
    last: Option<(i64, i64)>,
}

impl LookSchedule {
    /// The first look is due at once.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a look should start at `now`. Never while one is in flight. A `now` below the end of the last look means the wall clock went
    /// backwards: the interval starts again at `now`.
    pub fn due(&mut self, now: i64) -> bool {
        if self.in_flight {
            return false;
        }
        if self.asked {
            return true;
        }
        let Some((end, every)) = self.last.as_mut() else {
            return true;
        };
        if now < *end {
            *end = now;
            return false;
        }
        now >= end.saturating_add(*every)
    }

    /// A look starts: the generation it carries.
    pub fn started(&mut self) -> u64 {
        self.in_flight = true;
        self.asked = false;
        self.generation
    }

    /// A look of `generation` ended at `now`. Whether its result applies: only a look of the current generation does, and only then is
    /// the end recorded with its interval ([`LOOK_RETRY_S`] when it `failed`, else [`LOOK_EVERY_S`]).
    pub fn ended(&mut self, generation: u64, failed: bool, now: i64) -> bool {
        self.in_flight = false;
        if generation != self.generation {
            return false;
        }
        self.last = Some((now, if failed { LOOK_RETRY_S } else { LOOK_EVERY_S }));
        true
    }

    /// A Settings write of `update_source` or `ui.update_notice`: the result of a look in flight no longer applies, and a look is due as
    /// soon as none is in flight.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.asked = true;
    }
}
