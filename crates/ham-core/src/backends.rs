//! The backends the client talks to: the list a config file names, the rules for an alias, and the pure parts of reading from
//! several of them at once (the run id map, the merge of list pages, the version rule, the title tag and the customer-content check).
//! No terminal, network or AWS code and nothing on disk: the `ham` crate holds the transport that uses these.
//!
//! A config with no `[[backend]]` table has exactly one backend, `default`, made of the three top-level keys, and every function here
//! gives the answer the client gave before backends existed.

use crate::config::Config;

/// The alias of the stage the three top-level keys (`apiEndpoint`, `region`, `awsProfile`) name.
pub const DEFAULT_ALIAS: &str = "default";

/// The longest alias.
pub const ALIAS_MAX: usize = 32;

/// The word the title shows for the backend's version while the backends of a client report different versions.
pub const MIXED_WORD: &str = "mixed";

/// The oldest backend version that a client of this release line may change things on when it holds several backends: the
/// first of release line 5, which opened with the rename of the names a stage carries. A backend of an earlier line, or below this
/// counter, is read-only; a newer counter is allowed.
pub const MIN_BACKEND: &str = "5.00";

/// True for an alias a `[[backend]]` table may take: 1 to [`ALIAS_MAX`] of lower-case ASCII letters, digits, `-` and `_`, starting with a
/// letter or a digit. [`DEFAULT_ALIAS`] has this form too and is reserved for the top-level keys.
pub fn is_alias(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    text.len() <= ALIAS_MAX
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// What the config says about one backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendDef {
    pub alias: String,
    /// The `/v1` address of the stage. `None` only for `default` in a config that sets no `apiEndpoint`: the client then says what it always said.
    pub api_endpoint: Option<String>,
    pub region: Option<String>,
    pub aws_profile: Option<String>,
    /// Whether a run marked as customer content may start here. `default` and an absent key are no.
    pub customer_content: bool,
}

/// The backends of a config, `default` first. With no `[[backend]]` table that is `default` alone, with or without an endpoint; with tables,
/// `default` is listed only when `apiEndpoint` is set, and the tables follow in file order.
pub fn backend_defs(cfg: &Config) -> Vec<BackendDef> {
    let tables = cfg.backend.as_deref().unwrap_or_default();
    let top_level = cfg
        .api_endpoint
        .as_deref()
        .filter(|e| !e.is_empty())
        .is_some();
    let mut out = Vec::new();
    if top_level || tables.is_empty() {
        out.push(BackendDef {
            alias: DEFAULT_ALIAS.to_string(),
            api_endpoint: cfg.api_endpoint.clone(),
            region: cfg.region.clone(),
            aws_profile: cfg.aws_profile.clone(),
            customer_content: cfg.customer_content.unwrap_or(false),
        });
    }
    for t in tables {
        out.push(BackendDef {
            alias: t.alias.clone().unwrap_or_default(),
            api_endpoint: t.api_endpoint.clone(),
            region: t.region.clone(),
            aws_profile: t.aws_profile.clone(),
            customer_content: t.customer_content.unwrap_or(false),
        });
    }
    out
}

/// The aliases a command may name: [`DEFAULT_ALIAS`] first, then each table's alias.
pub fn aliases_of(cfg: &Config) -> Vec<String> {
    let mut out = vec![DEFAULT_ALIAS.to_string()];
    out.extend(cfg.backend.iter().flatten().filter_map(|t| t.alias.clone()));
    out
}

/// An endpoint written one way: no surrounding blanks, no trailing slash, lower case.
fn same_address(a: &str) -> String {
    a.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Problems of the whole backend list that no single key shows, one plain sentence each: a `default_backend` that names no backend,
/// `default` named while the top-level keys set no stage next to `[[backend]]` tables, and two backends with one address (each backend is a
/// different stage; one stage listed twice would show every run twice and make every run id ambiguous).
pub fn check_backends(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let all = backend_defs(cfg);
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            if let (Some(x), Some(y)) = (&a.api_endpoint, &b.api_endpoint)
                && !x.trim().is_empty()
                && same_address(x) == same_address(y)
            {
                out.push(format!(
                    "the backends {} and {} have the same apiEndpoint; each backend is a different stage",
                    a.alias, b.alias
                ));
            }
        }
    }
    if let Some(wanted) = cfg.default_backend.as_deref() {
        let defs = backend_defs(cfg);
        if !defs.iter().any(|d| d.alias == wanted) {
            let have: Vec<&str> = defs.iter().map(|d| d.alias.as_str()).collect();
            out.push(if wanted == DEFAULT_ALIAS {
                format!(
                    "default_backend is `{DEFAULT_ALIAS}`, but apiEndpoint is not set; the backends are {}",
                    have.join(", ")
                )
            } else {
                format!(
                    "default_backend names `{wanted}`, which is no backend; the backends are {}",
                    have.join(", ")
                )
            });
        }
    }
    out
}

// ---- routes -------------------------------------------------------------------------------------------------------------------------

/// What a `/v1` call is about; the transport that holds several backends decides the target from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route<'a> {
    /// `GET /v1/runs`: the caller's runs, which every backend answers for itself.
    List,
    /// A call about one run (`/v1/runs/<id>` and everything below it): the backend that owns the run answers.
    Run(&'a str),
    /// `POST /v1/direct/runs`: a new run, which needs a backend chosen by the person.
    Start,
    /// `POST /v1/tags/edit`: edits that name run ids in the body.
    Tags,
    /// Everything else (`whoami`, `models`, `repos`, `search`, repository admin): one backend answers.
    Other,
}

/// The segments right below `/v1/runs/` that are routes of their own and no run id (run ids are ULIDs, so none can collide with them).
const NOT_A_RUN: [&str; 1] = ["index"];

/// The route of a call. `path` starts with `/v1/` and may carry a query.
pub fn route_of<'a>(method: &str, path: &'a str) -> Route<'a> {
    let route = path.split('?').next().unwrap_or_default();
    match (method, route) {
        ("GET", "/v1/runs") => Route::List,
        ("POST", "/v1/direct/runs") => Route::Start,
        ("POST", "/v1/tags/edit") => Route::Tags,
        (_, r) => match r.strip_prefix("/v1/runs/") {
            Some(rest) => match rest
                .split('/')
                .next()
                .filter(|id| !id.is_empty() && !NOT_A_RUN.contains(id))
            {
                Some(id) => Route::Run(id),
                None => Route::Other,
            },
            None => Route::Other,
        },
    }
}

/// True when the answer to a call can teach the run map which backend holds a run: the list, and the `POST` of a start or of a call about
/// one run (they answer with a run id). Events, transcript pages and every other read are polled often and teach nothing, so the client does
/// not read their bodies for it.
pub fn learns_from(route: Route<'_>, method: &str) -> bool {
    matches!(route, Route::List)
        || (matches!(route, Route::Start | Route::Run(_)) && method.eq_ignore_ascii_case("POST"))
}

/// True for a call that changes something at a backend: every method but `GET`.
pub fn is_write(method: &str) -> bool {
    !method.eq_ignore_ascii_case("GET")
}

/// True when a call changes something at a backend: every method but `GET` and `HEAD`, except the two `POST` routes that only return an address
/// to download from (`/v1/runs/<id>/artifacts/<artifact>/download-url` and `/v1/runs/<id>/snapshot/download-url`). Any other route, a longer
/// path and an unknown method count as a change, so a new route is gated until it is listed here. The version rule reads this.
pub fn changes_something(method: &str, path: &str) -> bool {
    if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
        return false;
    }
    if !method.eq_ignore_ascii_case("POST") {
        return true;
    }
    let route = path.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = route.split('/').collect();
    let fetches_an_address = match segments.as_slice() {
        ["", "v1", "runs", id, "artifacts", artifact, "download-url"] => {
            !id.is_empty() && !artifact.is_empty()
        }
        ["", "v1", "runs", id, "snapshot", "download-url"] => !id.is_empty(),
        _ => false,
    };
    !fetches_an_address
}

// ---- the run id map -----------------------------------------------------------------------------------------------------------------

/// What the map knows about a run id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Unknown,
    One(String),
    /// The id was seen in more than one backend, in the order they were seen: two aliases may name the same stage.
    Ambiguous(Vec<String>),
}

/// The backend of each run the client has seen, learnt from list reads and from the answers to starts. It lives in memory only (invariant 5)
/// and is never written anywhere. A run never moves between backends, so an entry never goes stale.
#[derive(Clone, Debug, Default)]
pub struct RunMap {
    owners: std::collections::HashMap<String, Vec<String>>,
}

impl RunMap {
    /// Records that `alias` holds `run_id`.
    pub fn learn(&mut self, alias: &str, run_id: &str) {
        let owners = self.owners.entry(run_id.to_string()).or_default();
        if !owners.iter().any(|a| a == alias) {
            owners.push(alias.to_string());
        }
    }

    pub fn lookup(&self, run_id: &str) -> Lookup {
        match self.owners.get(run_id).map(Vec::as_slice) {
            None | Some([]) => Lookup::Unknown,
            Some([one]) => Lookup::One(one.clone()),
            Some(many) => Lookup::Ambiguous(many.to_vec()),
        }
    }

    /// The one backend of a run; `None` while the id is unknown or in two backends.
    pub fn backend_of(&self, run_id: &str) -> Option<&str> {
        match self.owners.get(run_id).map(Vec::as_slice) {
            Some([one]) => Some(one.as_str()),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

/// One backend's answer to "do you have this run?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Has,
    /// An authoritative 404 from that backend.
    NotHere,
    /// No answer that says anything about the run: a lapsed login, a backend that is down, a timeout, a 5xx, a 403. The reason is for the person.
    CannotTell(String),
}

/// What asking every backend about an unknown run id decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Owner(String),
    Ambiguous(Vec<String>),
    NotFound,
    CannotTell {
        checked: Vec<String>,
        unresolved: Vec<(String, String)>,
    },
}

/// The decision from the answers. One `Has` is the owner even when another backend could not answer (run ids are unique and runs do not
/// move); two are refused; "not found" needs every backend to have answered.
pub fn decide(outcomes: &[(String, Probe)]) -> Decision {
    let has: Vec<String> = outcomes
        .iter()
        .filter(|(_, p)| *p == Probe::Has)
        .map(|(a, _)| a.clone())
        .collect();
    match has.len() {
        1 => return Decision::Owner(has[0].clone()),
        n if n > 1 => return Decision::Ambiguous(has),
        _ => {}
    }
    let unresolved: Vec<(String, String)> = outcomes
        .iter()
        .filter_map(|(a, p)| match p {
            Probe::CannotTell(why) => Some((a.clone(), why.clone())),
            _ => None,
        })
        .collect();
    if unresolved.is_empty() {
        return Decision::NotFound;
    }
    let checked = outcomes
        .iter()
        .filter(|(_, p)| *p == Probe::NotHere)
        .map(|(a, _)| a.clone())
        .collect();
    Decision::CannotTell {
        checked,
        unresolved,
    }
}

// ---- the merge of list pages -----------------------------------------------------------------------------------------------------

/// One backend's page of its run list: `(createdAt, run id, payload)` for each row, newest first as the backend sent them, and whether a
/// next page follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub alias: String,
    pub rows: Vec<(String, String, T)>,
    pub more: bool,
}

/// A row of the merged list with the backend it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedRow<T> {
    pub created_at: String,
    pub run_id: String,
    pub backend: String,
    pub payload: T,
}

/// The merged first page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merged<T> {
    /// The rows that are final: newest first, by second and then by run id.
    pub rows: Vec<MergedRow<T>>,
    /// How many fetched rows were left out because a backend's next page may hold rows from the same time or later.
    pub held: usize,
    /// Whether more runs exist than the rows show.
    pub has_more: bool,
    /// The second (`2026-11-04T10:00:05`) up to which rows are held; `None` when nothing is held back on account of a known frontier.
    pub watermark: Option<String>,
}

/// The time of a row to the second: the first 19 characters of an RFC 3339 UTC time. A fraction is ignored, as the Runs list ignores it.
pub fn second_of(created_at: &str) -> &str {
    created_at.get(..19).unwrap_or(created_at)
}

fn merged_rows<T>(pages: Vec<Page<T>>) -> Vec<MergedRow<T>> {
    let mut rows: Vec<MergedRow<T>> = pages
        .into_iter()
        .flat_map(|p| {
            let alias = p.alias;
            p.rows
                .into_iter()
                .map(move |(created_at, run_id, payload)| MergedRow {
                    created_at,
                    run_id,
                    backend: alias.clone(),
                    payload,
                })
        })
        .collect();
    rows.sort_by(|a, b| {
        second_of(&b.created_at)
            .cmp(second_of(&a.created_at))
            .then_with(|| b.run_id.cmp(&a.run_id))
            .then_with(|| a.backend.cmp(&b.backend))
    });
    rows
}

/// Merges the newest pages of several backends so that a short list never looks complete. For each backend with a next page the oldest second
/// it fetched is its frontier: it may hold more rows at or after that second on its next page. The watermark is the newest frontier; only rows
/// newer than it are final, because every backend has shown all of its rows that are newer. Rows at or before the watermark are held back
/// (they are fetched again on the next read), and a backend whose page is empty but has more certifies nothing. A single page is passed
/// through as the backend sent it.
pub fn merge_first<T>(mut pages: Vec<Page<T>>) -> Merged<T> {
    if pages.len() == 1 {
        let p = pages.remove(0);
        let has_more = p.more;
        let alias = p.alias;
        let rows = p
            .rows
            .into_iter()
            .map(|(created_at, run_id, payload)| MergedRow {
                created_at,
                run_id,
                backend: alias.clone(),
                payload,
            })
            .collect();
        return Merged {
            rows,
            held: 0,
            has_more,
            watermark: None,
        };
    }
    let any_more = pages.iter().any(|p| p.more);
    let mut watermark: Option<String> = None;
    let mut unknown = false;
    for p in pages.iter().filter(|p| p.more) {
        match p.rows.iter().map(|(t, _, _)| second_of(t)).min() {
            None => unknown = true,
            Some(frontier) => {
                if watermark.as_deref().is_none_or(|w| frontier > w) {
                    watermark = Some(frontier.to_string());
                }
            }
        }
    }
    let all = merged_rows(pages);
    if !any_more {
        return Merged {
            rows: all,
            held: 0,
            has_more: false,
            watermark: None,
        };
    }
    if unknown {
        let held = all.len();
        return Merged {
            rows: Vec::new(),
            held,
            has_more: true,
            watermark: None,
        };
    }
    let w = watermark.clone().unwrap_or_default();
    let (rows, held): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|r| second_of(&r.created_at) > w.as_str());
    Merged {
        rows,
        held: held.len(),
        has_more: true,
        watermark,
    }
}

/// Merges whole lists (every page of every backend was read): all rows, newest first.
pub fn merge_all<T>(pages: Vec<Page<T>>) -> Vec<MergedRow<T>> {
    merged_rows(pages)
}

// ---- health, banners and the title tag -------------------------------------------------------------------------------------------

/// How a backend answered the last time the client asked it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Up,
    /// It did not answer inside the deadline.
    Slow,
    /// It could not be built or reached: the reason is the transport's.
    Down(String),
    /// The login behind its profile has lapsed or is missing; `fix` is the command that signs in again.
    SignedOut {
        fix: String,
    },
    /// The gateway of the stage answered and refused the caller (HTTP 401 or 403 that the service did not send): a wrong profile or a role
    /// without access. The text is the answer's own, with what to check; signing in again does not help, so it is no `SignedOut`.
    Refused(String),
}

/// What the screens show about one backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendInfo {
    pub alias: String,
    pub health: Health,
    /// The backend's version as its last answer said it (`5.05`); `None` until one valid header arrived.
    pub version: Option<String>,
    /// True while the rows shown for it are the ones of an earlier read, kept in memory because the last read failed.
    pub stale: bool,
    pub customer_content: bool,
    pub profile: Option<String>,
}

/// The backends in config order with their health.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendView {
    pub backends: Vec<BackendInfo>,
}

impl BackendView {
    /// One plain line for each backend that did not answer, in config order, each with the way forward.
    pub fn banners(&self) -> Vec<String> {
        self.backends
            .iter()
            .filter_map(|b| match &b.health {
                Health::Up => None,
                Health::Slow => Some(format!(
                    "{}: did not answer in time; its runs may be out of date",
                    b.alias
                )),
                Health::Down(why) => Some(format!("{}: not reachable: {why}", b.alias)),
                Health::SignedOut { fix } => Some(format!("{}: sign in needed: {fix}", b.alias)),
                Health::Refused(why) => Some(format!("{}: access refused: {why}", b.alias)),
            })
            .collect()
    }

    /// The Runs list shows a BACKEND column only when there are two backends or more.
    pub fn shows_backend_column(&self) -> bool {
        self.backends.len() >= 2
    }

    pub fn info(&self, alias: &str) -> Option<&BackendInfo> {
        self.backends.iter().find(|b| b.alias == alias)
    }
}

/// What an HTTP answer says about the backend that sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The backend is working: a result, a not-found, a conflict, a refusal of a request by the service itself.
    Fine,
    /// The gateway says the sign-in has expired.
    SignInLapsed,
    /// A throttle or a timeout (HTTP 408, 429, 504): the backend did not give its answer in time.
    Slow,
    /// The gateway refused the caller (HTTP 401 or 403 without the service's version header). The text says what to check.
    Refused(String),
    /// A server failure (HTTP 500 or above, other than 504). The text is the answer's own.
    Failed(String),
}

/// The longest message of an answer that a banner carries.
const MESSAGE_MAX: usize = 160;

/// The message of an error body for one banner line: the `message` member (the gateway writes `message` or `Message`), else the body; control
/// characters and line breaks made safe, cut to [`MESSAGE_MAX`] characters.
fn message_of(body: &[u8]) -> String {
    let member = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("Message"))
                .and_then(|m| m.as_str().map(str::to_string))
        });
    let text = member.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    let line = crate::sanitize::sanitize_line(text.trim());
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > MESSAGE_MAX {
        let cut: String = line.chars().take(MESSAGE_MAX).collect();
        format!("{cut}...")
    } else {
        line
    }
}

/// What one HTTP answer says about its backend. `from_service` is true when the answer carries a valid version header: only the service
/// (control-api) writes it, so an answer without it, such as a 403, a 429 or a 502, comes from the API gateway in front of the service. The
/// service's own 4xx answers are part of normal work and leave the backend `Fine`; its server failures count. The last answer is the health.
pub fn judge_answer(status: u16, from_service: bool, body: &[u8]) -> Answer {
    let with_message = |head: String| {
        let message = message_of(body);
        if message.is_empty() {
            head
        } else {
            format!("{head}: {message}")
        }
    };
    match status {
        401 | 403 if !from_service => {
            if String::from_utf8_lossy(body)
                .to_ascii_lowercase()
                .contains("expired")
            {
                return Answer::SignInLapsed;
            }
            let text = with_message(format!("HTTP {status}"));
            Answer::Refused(format!(
                "{text}; check the profile and Region of this backend"
            ))
        }
        408 | 429 | 504 => Answer::Slow,
        500.. => Answer::Failed(with_message(format!("HTTP {status}"))),
        _ => Answer::Fine,
    }
}

/// What the title line says about the backends' versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleVersion {
    One(String),
    /// Two backends report different versions.
    Mixed,
    Unknown,
}

impl TitleVersion {
    /// The word the title shows for the case that has no single version.
    pub fn word(&self) -> &str {
        match self {
            TitleVersion::One(v) => v,
            TitleVersion::Mixed => MIXED_WORD,
            TitleVersion::Unknown => "?.??",
        }
    }
}

/// The title's version: the one version every backend that reported one agrees on, `mixed` when they differ.
pub fn title_version(infos: &[BackendInfo]) -> TitleVersion {
    let mut known: Vec<&str> = infos.iter().filter_map(|i| i.version.as_deref()).collect();
    known.sort_unstable();
    known.dedup();
    match known.as_slice() {
        [] => TitleVersion::Unknown,
        [one] => TitleVersion::One((*one).to_string()),
        _ => TitleVersion::Mixed,
    }
}

// ---- the version rule ----------------------------------------------------------------------------------------------------------------

/// What a client may do against a backend of a given version (with two backends or more).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    Full,
    /// The backend sent no version yet: nothing is refused until it does.
    Unknown,
    /// The version is older than the client's minimum: reads work, changes are refused.
    ReadOnly {
        version: String,
        min: String,
    },
    /// A major version this client does not know, or a header that is no version: changes are refused.
    UnknownMajor {
        version: String,
    },
}

impl Support {
    /// True when a call that changes something is refused.
    pub fn refuses_writes(&self) -> bool {
        matches!(
            self,
            Support::ReadOnly { .. } | Support::UnknownMajor { .. }
        )
    }
}

/// `5.05` as (5, 5); `None` for anything that is not `digits.dd`.
fn parts(version: &str) -> Option<(u64, u64)> {
    let (major, counter) = version.split_once('.')?;
    if major.is_empty()
        || counter.len() != 2
        || !major.bytes().all(|b| b.is_ascii_digit())
        || !counter.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((major.parse().ok()?, counter.parse().ok()?))
}

/// The rule: the same major with a counter at or above `min` (newer counters included) is full; the same major below `min`, or an older major,
/// is read-only; a newer major or a header that is no version refuses changes; no header is unknown.
pub fn support(client: &str, min: &str, backend: Option<&str>) -> Support {
    let Some(version) = backend else {
        return Support::Unknown;
    };
    let (Some(client), Some(min), Some(seen)) = (parts(client), parts(min), parts(version)) else {
        return Support::UnknownMajor {
            version: version.to_string(),
        };
    };
    if seen.0 > client.0 {
        return Support::UnknownMajor {
            version: version.to_string(),
        };
    }
    if seen.0 < client.0 || seen < min {
        return Support::ReadOnly {
            version: version.to_string(),
            min: format!("{}.{:02}", min.0, min.1),
        };
    }
    Support::Full
}

// ---- refusals made on this machine ---------------------------------------------------------------------------------------------------

/// A refusal the client makes itself, before or instead of a call to a backend. Each says what to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Local {
    NoBackendChosen {
        aliases: Vec<String>,
    },
    UnknownAlias {
        alias: String,
        aliases: Vec<String>,
    },
    Ambiguous {
        run_id: String,
        aliases: Vec<String>,
    },
    CannotTell {
        run_id: String,
        checked: Vec<String>,
        unresolved: Vec<(String, String)>,
    },
    ReadOnly {
        alias: String,
        version: String,
        min: String,
    },
    UnknownMajor {
        alias: String,
        version: String,
    },
    CustomerContent {
        alias: String,
    },
    MixedBackends {
        aliases: Vec<String>,
    },
    /// A backend answered the version probe without any version: a stage that predates the tag, older than `min`.
    Untagged {
        alias: String,
        min: String,
    },
    /// A follow-up was aimed at a backend other than the one that holds the run it continues.
    ContinueElsewhere {
        run_id: String,
        owner: String,
        named: String,
    },
    /// The backend could not be built or reached and the client is not trying it again yet.
    BackendDown {
        alias: String,
        reason: String,
    },
    /// A read of every backend got nothing from any of them; the reasons are the backends' own, `alias: why` joined by `; `.
    AllFailed {
        reasons: String,
    },
}

impl std::fmt::Display for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Local::NoBackendChosen { aliases } => write!(
                f,
                "there are {} backends ({}) and none is chosen: name one with --backend <alias>, or set default_backend in config.toml",
                aliases.len(),
                aliases.join(", ")
            ),
            Local::UnknownAlias { alias, aliases } => write!(
                f,
                "no backend is named {alias:?}; the backends are {}",
                aliases.join(", ")
            ),
            Local::Ambiguous { run_id, aliases } => write!(
                f,
                "run {run_id} is found in more than one backend ({}); two aliases may name the same stage. Name the one you mean with --backend <alias>",
                aliases.join(", ")
            ),
            Local::CannotTell {
                run_id,
                checked,
                unresolved,
            } => {
                let mut parts = Vec::new();
                if !checked.is_empty() {
                    parts.push(format!("not found on {}", checked.join(", ")));
                }
                for (alias, why) in unresolved {
                    parts.push(format!("could not check {alias}: {why}"));
                }
                write!(f, "run {run_id}: {}", parts.join("; "))
            }
            Local::ReadOnly {
                alias,
                version,
                min,
            } => write!(
                f,
                "backend {alias} is v{version}; this ham needs v{min} or newer to change anything there, so it is read-only"
            ),
            Local::UnknownMajor { alias, version } => write!(
                f,
                "backend {alias} reports v{version}, a version this ham does not know; update ham before you change anything there"
            ),
            Local::Untagged { alias, min } => write!(
                f,
                "backend {alias} reports no version, so it is older than v{min}; this ham needs v{min} or newer to change anything there, so it is read-only"
            ),
            Local::ContinueElsewhere {
                run_id,
                owner,
                named,
            } => write!(
                f,
                "run {run_id} is in backend {owner}; a follow-up starts where its run is, not on {named}. Leave out --backend, or name {owner}"
            ),
            Local::CustomerContent { alias } => write!(
                f,
                "backend {alias} is not declared safe for customer content; set customerContent = true in its [[backend]] table, or start on a backend that is"
            ),
            Local::MixedBackends { aliases } => write!(
                f,
                "these runs are in different backends ({}); change them one backend at a time",
                aliases.join(", ")
            ),
            Local::BackendDown { alias, reason } => {
                write!(f, "backend {alias} is not available: {reason}")
            }
            Local::AllFailed { reasons } => write!(f, "no backend answered: {reasons}"),
        }
    }
}

impl std::error::Error for Local {}

/// The refusal of a start that is marked as customer content on a backend not declared safe for it (OD-2): the declaration is the user's
/// word; nothing here can read the backend's data-retention mode.
pub fn customer_content_refusal(def: &BackendDef, marked: bool) -> Option<Local> {
    (marked && !def.customer_content).then(|| Local::CustomerContent {
        alias: def.alias.clone(),
    })
}

// ---- the options outside the command grammar -----------------------------------------------------------------------------------------

/// The options of a client with several backends that stand outside the command grammar (`--json` is another one): which backend a command
/// is aimed at, and whether a start is marked as customer content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub backend: Option<String>,
    pub customer_content: bool,
}

/// Takes `--backend <alias>` (or `--backend=<alias>`) and `--customer-content` out of `args`, wherever they stand except as the value of the
/// option before them, and returns what is left for the command parser.
pub fn take_flags(args: &[String]) -> Result<(Flags, Vec<String>), String> {
    let is_value = crate::command::value_positions(args);
    let mut flags = Flags::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut i = 0;
    let set = |flags: &mut Flags, value: &str| -> Result<(), String> {
        if value.is_empty() || value.starts_with('-') {
            return Err("--backend takes the alias of a backend".to_string());
        }
        if flags.backend.is_some() {
            return Err("--backend is given once".to_string());
        }
        flags.backend = Some(value.to_string());
        Ok(())
    };
    while i < args.len() {
        let a = args[i].as_str();
        if is_value.get(i).copied().unwrap_or(false) {
            rest.push(args[i].clone());
            i += 1;
        } else if a == "--customer-content" {
            flags.customer_content = true;
            i += 1;
        } else if a == "--backend" {
            let value = args.get(i + 1).map_or("", String::as_str);
            set(&mut flags, value)?;
            i += 2;
        } else if let Some(value) = a.strip_prefix("--backend=") {
            set(&mut flags, value)?;
            i += 1;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    Ok((flags, rest))
}
