//! The research route: Cline searches the web and downloads public repositories for context
//! through five private routes under `/internal/research/`, served by the `research-api` Lambda in the stage account.
//!
//! The VM never holds the Tavily key. agentd (or Cline itself, with the same session credentials) signs a request with SigV4 and the
//! route does everything that needs trust: it reads the run's policy row, applies the per-run limits, scans every outbound string, calls
//! Tavily or GitHub, writes the result and one audit object under `runs/<run>/agents/direct/research/` (a prefix a bucket policy
//! closes to the worker role) and answers with references, never with page or file content.
//!
//! This module holds the pieces that more than one component reads: the request and response bodies of the five routes (agentd's
//! loopback server and the `ham-research` command speak the same shapes), the stored objects ([`ItemManifest`], [`AuditObject`],
//! [`PolicyItem`]), the error words with their HTTP status and exit code, the default [`ResearchLimits`], the S3 layout ([`s3`]) and the
//! switch a run request carries ([`ResearchOptions`]). The scanner, the Tavily client, the counters and the archive handling are
//! `research-api`'s and are not wire types.
//!
//! Reading rule: request bodies are strict (`deny_unknown_fields`), stored objects and answers are read as written by their writer's
//! version; a member is added only with a protocol minor bump and is left out when empty.

use crate::common::{is_lower_hex, is_ulid, is_utc_timestamp};
use crate::direct::DIRECT_AGENT_ID;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

// ---- constants --------------------------------------------------------------------------------------------------------------

/// `schema` of a stored item manifest.
pub const ITEM_SCHEMA: &str = "hamh-research-item/1";
/// `schema` of a stored audit object.
pub const AUDIT_SCHEMA: &str = "hamh-research-audit/1";
/// `schema` of a stored search record (the snippets shown to Cline).
pub const SEARCH_SCHEMA: &str = "hamh-research-search/1";

/// An idempotency key: 1 to 128 printable ASCII characters without a space (the `/v1` rule).
pub const MAX_IDEMPOTENCY_KEY_CHARS: usize = 128;
/// Results a search may ask for: the most Tavily returns for one search. The AgentCore provider takes up to 25.
pub const MAX_SEARCH_RESULTS: u8 = 20;
/// Domains a search may restrict itself to: the most Tavily takes in `include_domains`. A provider that takes fewer refuses the request
/// and the limits answer reports that provider's own caps.
pub const MAX_INCLUDE_DOMAINS: usize = 300;
/// URLs one fetch may name: the most Tavily takes in one extract request.
pub const MAX_FETCH_URLS: usize = 20;
/// Path prefixes one repository request may name.
pub const MAX_REPO_PATHS: usize = 20;
/// The longest path prefix, ref or job id.
pub const MAX_PATH_CHARS: usize = 256;
/// The longest ref (branch, tag or commit) of a repository request.
pub const MAX_REF_CHARS: usize = 255;
/// The longest job id.
pub const MAX_JOB_ID_CHARS: usize = 64;

/// The paths of the five routes (the API Gateway resources `/internal/research/{search,fetch,repo,job,limits}`).
pub mod route {
    pub const SEARCH: &str = "/internal/research/search";
    pub const FETCH: &str = "/internal/research/fetch";
    pub const REPO: &str = "/internal/research/repo";
    pub const JOB: &str = "/internal/research/job";
    pub const LIMITS: &str = "/internal/research/limits";
    /// The five paths in route-table order.
    pub const ALL: [&str; 5] = [SEARCH, FETCH, REPO, JOB, LIMITS];
}

// ---- words --------------------------------------------------------------------------------------------------------------------

/// Where an answer came from. The stage configuration chooses the search provider; a caller cannot. Every response, audit object and
/// manifest records it, so each record can be checked against the provider the stage is configured for (`tavily`
/// unless the stage names another).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ResearchProvider {
    // Tavily with the stage's key.
    Tavily,
    // Tavily keyless mode (an explicit first wiring check, free and rate-limited).
    TavilyKeyless,
    // AWS's own web index through an AgentCore Gateway with the Web Search Tool connector. Search only: the
    // connector returns excerpts and cannot fetch a page. No third-party key; the research-api role signs the call.
    Agentcore,
    // Canned answers, test stages only.
    Fixture,
    // A GitHub archive (repositories).
    GithubArchive,
    // Material the user attached to the run (written by agentd).
    User,
}

impl ResearchProvider {
    /// The providers a stage may name for search.
    pub fn is_search_provider(self) -> bool {
        matches!(
            self,
            Self::Tavily | Self::TavilyKeyless | Self::Agentcore | Self::Fixture
        )
    }

    /// The search providers that can also fetch a page. `agentcore` cannot: its connector returns excerpts only, so a stage that
    /// names it refuses `fetch`, and no page item carries its word.
    pub fn supports_fetch(self) -> bool {
        matches!(self, Self::Tavily | Self::TavilyKeyless | Self::Fixture)
    }
}

/// What an audit object records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResearchOp {
    Search,
    Fetch,
    Repo,
    Job,
    Limits,
}

/// Tavily's search depth: basic costs 1 credit, advanced 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchDepth {
    Basic,
    Advanced,
}

/// A search restricted to recent pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimeRange {
    Day,
    Week,
    Month,
    Year,
}

/// What a stored item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    // A fetched web page (text).
    Page,
    // A downloaded repository (a filtered tar.gz).
    Repo,
    // A file the user attached to the run.
    Attachment,
}

/// The state of an asynchronous job (a repository download).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

/// How a call ended, in an audit object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditStatus {
    Ok,
    Refused,
    Error,
}

/// The closed set of words a research route answers with when it does not do what was asked. Each has one HTTP status (the route's
/// answer), and one `ham-research` exit code (2 usage, 3 policy, 4 limit, 5 upstream). The words are the product's: a client shows
/// them and a test pins them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResearchErrorCode {
    // The request is malformed or breaks a size rule.
    Usage,
    // An idempotency key was reused with different arguments.
    Conflict,
    // Research is off for the run (no policy row, switched off, or expired).
    Disabled,
    // The stage has no Tavily key stored.
    NoKey,
    // An outbound string looks like a credential.
    RefusedSecret,
    // The URL breaks the URL rules (user information, IP address, port, presigned parameters, scheme).
    RefusedUrl,
    // The URL was not seen in this run's search results, prompt or fetched pages.
    RefusedUnseenUrl,
    // The host is not served (only github.com repositories in this release).
    RefusedHost,
    // A per-run count is used up.
    LimitCalls,
    // A per-run byte budget is used up.
    LimitBytes,
    // Too many calls this minute.
    LimitRate,
    // The page or archive is over its size cap.
    TooLarge,
    // The upstream failed or answered something unusable.
    UpstreamError,
    // The upstream said slow down.
    UpstreamRateLimited,
    // The upstream has nothing at that address.
    NotFound,
    // A job did not start within 2 minutes.
    NotStarted,
    // A job did not finish within 16 minutes.
    Timeout,
    // The route failed.
    Internal,
}

/// The word is not one of [`ResearchErrorCode::ALL`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownErrorCode(pub String);

impl fmt::Display for UnknownErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown research error code {:?}", self.0)
    }
}

impl std::error::Error for UnknownErrorCode {}

impl ResearchErrorCode {
    /// Every code, in declaration order.
    pub const ALL: [Self; 18] = [
        Self::Usage,
        Self::Conflict,
        Self::Disabled,
        Self::NoKey,
        Self::RefusedSecret,
        Self::RefusedUrl,
        Self::RefusedUnseenUrl,
        Self::RefusedHost,
        Self::LimitCalls,
        Self::LimitBytes,
        Self::LimitRate,
        Self::TooLarge,
        Self::UpstreamError,
        Self::UpstreamRateLimited,
        Self::NotFound,
        Self::NotStarted,
        Self::Timeout,
        Self::Internal,
    ];

    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Conflict => "conflict",
            Self::Disabled => "disabled",
            Self::NoKey => "no_key",
            Self::RefusedSecret => "refused_secret",
            Self::RefusedUrl => "refused_url",
            Self::RefusedUnseenUrl => "refused_unseen_url",
            Self::RefusedHost => "refused_host",
            Self::LimitCalls => "limit_calls",
            Self::LimitBytes => "limit_bytes",
            Self::LimitRate => "limit_rate",
            Self::TooLarge => "too_large",
            Self::UpstreamError => "upstream_error",
            Self::UpstreamRateLimited => "upstream_rate_limited",
            Self::NotFound => "not_found",
            Self::NotStarted => "not_started",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
        }
    }

    /// The HTTP status the route answers with.
    pub fn http_status(self) -> u16 {
        match self {
            Self::Usage => 400,
            Self::Conflict => 409,
            Self::Disabled | Self::NoKey => 503,
            Self::RefusedSecret | Self::RefusedUrl | Self::RefusedUnseenUrl | Self::RefusedHost => {
                403
            }
            Self::LimitCalls | Self::LimitBytes | Self::LimitRate | Self::UpstreamRateLimited => {
                429
            }
            Self::TooLarge => 413,
            Self::UpstreamError | Self::NotStarted | Self::Timeout => 502,
            Self::NotFound => 404,
            Self::Internal => 500,
        }
    }

    /// The exit code of `ham-research`: 2 usage, 3 refused by policy, 4 limit, 5 upstream or internal. (6 pending and 7 integrity are
    /// the command's own.)
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Usage | Self::Conflict => 2,
            Self::Disabled
            | Self::NoKey
            | Self::RefusedSecret
            | Self::RefusedUrl
            | Self::RefusedUnseenUrl
            | Self::RefusedHost => 3,
            Self::LimitCalls | Self::LimitBytes | Self::LimitRate | Self::TooLarge => 4,
            Self::UpstreamError
            | Self::UpstreamRateLimited
            | Self::NotFound
            | Self::NotStarted
            | Self::Timeout
            | Self::Internal => 5,
        }
    }
}

impl fmt::Display for ResearchErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ResearchErrorCode {
    type Err = UnknownErrorCode;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownErrorCode(s.to_string()))
    }
}

/// The body of a non-2xx answer: the shape of [`crate::api::ApiError`] plus a retry hint, so a client that reads only `code` and
/// `message` works. Also the `error` of a failed job and the reason of a failed URL in a fetch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResearchError {
    pub code: ResearchErrorCode,
    /// One plain sentence; never a key, a header or fetched text beyond a short excerpt.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl ResearchError {
    pub fn new(code: ResearchErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_s: None,
            request_id: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ResearchErrorCode::Usage, message)
    }
}

impl fmt::Display for ResearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ResearchError {}

// ---- limits and usage -----------------------------------------------------------------------------------------------------------

/// The per-run limits. They are abuse and exfiltration bounds, not budgets (spend is approved), settable in the stage configuration and
/// copied into the run's policy row when the run starts. `Default` is the numbers
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResearchLimits {
    /// Search calls per run.
    pub searches: u32,
    /// URLs fetched per run.
    pub fetched_urls: u32,
    /// Repository jobs per run.
    pub repos: u32,
    /// Calls of any kind per minute and run.
    pub calls_per_minute: u32,
    /// Characters of a query.
    pub query_chars: u32,
    /// Bytes of a URL.
    pub url_bytes: u32,
    /// Bytes of model-chosen outbound text (queries, URLs, repository paths) per run.
    pub outbound_bytes: u64,
    /// Bytes of one stored page.
    pub page_bytes: u64,
    /// Bytes downloaded for one repository archive.
    pub repo_download_bytes: u64,
    /// Bytes read from one archive after decompression, kept or not.
    pub repo_read_bytes: u64,
    /// Entries read from one archive.
    pub repo_read_entries: u32,
    /// Bytes of the filtered archive that is stored.
    pub repo_compressed_bytes: u64,
    /// Bytes of the files kept from one archive.
    pub repo_unpacked_bytes: u64,
    /// Files kept from one archive.
    pub repo_files: u32,
    /// Bytes of all research material of one run.
    pub context_bytes: u64,
}

impl Default for ResearchLimits {
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            searches: 200,
            fetched_urls: 200,
            repos: 25,
            calls_per_minute: 30,
            query_chars: 400,
            url_bytes: 2048,
            outbound_bytes: 64 * 1024,
            page_bytes: 512 * 1024,
            repo_download_bytes: 128 * MIB,
            repo_read_bytes: 1024 * MIB,
            repo_read_entries: 100_000,
            repo_compressed_bytes: 64 * MIB,
            repo_unpacked_bytes: 200 * MIB,
            repo_files: 20_000,
            context_bytes: 300 * MIB,
        }
    }
}

impl ResearchLimits {
    /// A limit of zero is not a limit, and one repository cannot be allowed more than the run as a whole.
    pub fn validate(&self) -> Result<(), ResearchError> {
        let zero = [
            ("searches", u64::from(self.searches)),
            ("fetchedUrls", u64::from(self.fetched_urls)),
            ("repos", u64::from(self.repos)),
            ("callsPerMinute", u64::from(self.calls_per_minute)),
            ("queryChars", u64::from(self.query_chars)),
            ("urlBytes", u64::from(self.url_bytes)),
            ("outboundBytes", self.outbound_bytes),
            ("pageBytes", self.page_bytes),
            ("repoDownloadBytes", self.repo_download_bytes),
            ("repoReadBytes", self.repo_read_bytes),
            ("repoReadEntries", u64::from(self.repo_read_entries)),
            ("repoCompressedBytes", self.repo_compressed_bytes),
            ("repoUnpackedBytes", self.repo_unpacked_bytes),
            ("repoFiles", u64::from(self.repo_files)),
            ("contextBytes", self.context_bytes),
        ];
        if let Some((name, _)) = zero.iter().find(|(_, v)| *v == 0) {
            return Err(ResearchError::usage(format!("limit {name} is zero")));
        }
        if self.repo_unpacked_bytes > self.context_bytes || self.page_bytes > self.context_bytes {
            return Err(ResearchError::usage(
                "one item may not be allowed more than contextBytes",
            ));
        }
        if self.repo_compressed_bytes > self.repo_download_bytes {
            return Err(ResearchError::usage(
                "repoCompressedBytes exceeds repoDownloadBytes",
            ));
        }
        Ok(())
    }
}

/// What a run has used so far. The route's counters are conditional updates, so concurrent calls cannot pass a cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResearchUsage {
    pub searches: u32,
    pub fetched_urls: u32,
    pub repos: u32,
    pub outbound_bytes: u64,
    pub context_bytes: u64,
}

// ---- request validation helpers -------------------------------------------------------------------------------------------------

fn is_key_char(c: char) -> bool {
    ('!'..='~').contains(&c)
}

fn check_idempotency_key(key: &str) -> Result<(), ResearchError> {
    if key.is_empty()
        || key.chars().count() > MAX_IDEMPOTENCY_KEY_CHARS
        || !key.chars().all(is_key_char)
    {
        return Err(ResearchError::usage(
            "idempotencyKey is 1 to 128 printable characters without a space",
        ));
    }
    Ok(())
}

fn is_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

fn is_token(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn check_url_text(url: &str, limits: &ResearchLimits) -> Result<(), ResearchError> {
    if url.is_empty() {
        return Err(ResearchError::usage("a URL is empty"));
    }
    if url.len() > limits.url_bytes as usize {
        return Err(ResearchError::usage(format!(
            "a URL is longer than {} bytes",
            limits.url_bytes
        )));
    }
    Ok(())
}

fn safe_relative_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH_CHARS
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.chars().any(|c| c == '\0' || c.is_control())
        && p.split('/').all(|seg| seg != "..")
}

// ---- search -------------------------------------------------------------------------------------------------------------------------

/// `POST /internal/research/search`
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    /// 1 to [`MAX_SEARCH_RESULTS`]; absent means 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u8>,
    /// Absent means `basic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<SearchDepth>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_range: Option<TimeRange>,
    /// A repeat with the same key and the same body returns the first answer; the same key with another body is `conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl SearchRequest {
    pub fn validate(&self, limits: &ResearchLimits) -> Result<(), ResearchError> {
        let chars = self.query.chars().count();
        if self.query.trim().is_empty() {
            return Err(ResearchError::usage("query is empty"));
        }
        if chars > limits.query_chars as usize {
            return Err(ResearchError::usage(format!(
                "query is longer than {} characters",
                limits.query_chars
            )));
        }
        if let Some(n) = self.max_results
            && !(1..=MAX_SEARCH_RESULTS).contains(&n)
        {
            return Err(ResearchError::usage(format!(
                "maxResults is 1 to {MAX_SEARCH_RESULTS}"
            )));
        }
        if self.include_domains.len() > MAX_INCLUDE_DOMAINS
            || !self.include_domains.iter().all(|d| is_hostname(d))
        {
            return Err(ResearchError::usage(format!(
                "includeDomains is at most {MAX_INCLUDE_DOMAINS} host names"
            )));
        }
        if let Some(k) = &self.idempotency_key {
            check_idempotency_key(k)?;
        }
        Ok(())
    }
}

/// One search hit. `snippet` is cut to 1 KiB by the route and is third-party text: a client marks it untrusted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// `s-0001`: the search record under `searches/` holds what was shown.
    pub id: String,
    pub provider: ResearchProvider,
    pub results: Vec<SearchResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<u32>,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResearchUsage>,
}

// ---- fetch --------------------------------------------------------------------------------------------------------------------------

/// `POST /internal/research/fetch`: each URL must already appear in this run's search results, in the prompt or as a link of a page the
/// run fetched (the provenance gate).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FetchRequest {
    /// 1 to [`MAX_FETCH_URLS`].
    pub urls: Vec<String>,
    /// What the caller wants from the pages; Tavily returns focused chunks (smaller files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl FetchRequest {
    pub fn validate(&self, limits: &ResearchLimits) -> Result<(), ResearchError> {
        if self.urls.is_empty() || self.urls.len() > MAX_FETCH_URLS {
            return Err(ResearchError::usage(format!(
                "urls is 1 to {MAX_FETCH_URLS} addresses"
            )));
        }
        for u in &self.urls {
            check_url_text(u, limits)?;
        }
        if let Some(q) = &self.query
            && q.chars().count() > limits.query_chars as usize
        {
            return Err(ResearchError::usage(format!(
                "query is longer than {} characters",
                limits.query_chars
            )));
        }
        if let Some(k) = &self.idempotency_key {
            check_idempotency_key(k)?;
        }
        Ok(())
    }
}

/// A stored item as an answer names it: where its manifest and body are, and the hashes the reader checks. Content is never in an
/// answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    /// `i-0002`
    pub id: String,
    pub kind: ItemKind,
    pub origin: String,
    pub manifest_key: String,
    /// SHA-256 of the manifest's canonical bytes ([`ItemManifest::sha256`]): the item's index hash.
    pub manifest_sha256: String,
    pub body_key: String,
    pub body_sha256: String,
    pub bytes: u64,
    /// The SPDX id of the licence, or `NOASSERTION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    pub truncated: bool,
    /// Files kept and dropped (a repository).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u32>,
}

/// One URL of a fetch that produced no item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetchFailure {
    pub url: String,
    pub code: ResearchErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetchResponse {
    pub items: Vec<ItemRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<FetchFailure>,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResearchUsage>,
}

// ---- repo and job -------------------------------------------------------------------------------------------------------------------

/// `POST /internal/research/repo`: download one GitHub repository at a ref as a job. The idempotency key is required: the route starts
/// the job once per key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepoRequest {
    /// `https://github.com/<owner>/<repo>`
    pub url: String,
    /// A branch, a tag or a commit; absent means the default branch. The manifest records the commit id the archive carries.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Keep only these path prefixes (at most [`MAX_REPO_PATHS`]); absent keeps the whole filtered tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub idempotency_key: String,
}

impl RepoRequest {
    pub fn validate(&self, limits: &ResearchLimits) -> Result<(), ResearchError> {
        check_url_text(&self.url, limits)?;
        check_idempotency_key(&self.idempotency_key)?;
        if let Some(r) = &self.git_ref {
            let ok = !r.is_empty()
                && r.len() <= MAX_REF_CHARS
                && !r.starts_with('-')
                && !r.contains("..")
                && r.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
            if !ok {
                return Err(ResearchError::usage(
                    "ref is a branch, tag or commit name of letters, digits and . _ - /",
                ));
            }
        }
        if self.paths.len() > MAX_REPO_PATHS || !self.paths.iter().all(|p| safe_relative_path(p)) {
            return Err(ResearchError::usage(format!(
                "paths is at most {MAX_REPO_PATHS} relative paths without .. or a leading /"
            )));
        }
        Ok(())
    }
}

/// `POST /internal/research/job`: ask after a job. Another run's or agent's job answers `not_found`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRequest {
    pub job_id: String,
}

impl JobRequest {
    pub fn validate(&self) -> Result<(), ResearchError> {
        if !is_token(&self.job_id, MAX_JOB_ID_CHARS) {
            return Err(ResearchError::usage(
                "jobId is a token of letters, digits, - and _",
            ));
        }
        Ok(())
    }
}

/// The answer to `repo` and to `job`: HTTP 202 while the job is queued or running, 200 once it has ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobResponse {
    pub job_id: String,
    pub state: JobState,
    /// Present when `state` is `succeeded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemRef>,
    /// Present when `state` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResearchError>,
    /// While pending: when to ask again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResearchUsage>,
}

// ---- limits -------------------------------------------------------------------------------------------------------------------------

/// `POST /internal/research/limits`: read-only; it touches no counter and calls no upstream. A run with research off gets the
/// `disabled` error, which is how agentd learns not to start its loopback server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitsRequest {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LimitsResponse {
    /// Always true in an answer: a run with research off gets the `disabled` error instead.
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ResearchProvider>,
    pub limits: ResearchLimits,
    pub usage: ResearchUsage,
    /// Epoch seconds after which the policy row refuses every call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

// ---- stored objects -------------------------------------------------------------------------------------------------------------------

/// How the licence of an item was found. Evidence, not clearance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LicenceEvidence {
    /// An SPDX id or expression, or `NOASSERTION` when unsure.
    pub spdx: String,
    /// `file:<path>`, `declared:<file>` or `none`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_sha256: Option<String>,
}

/// The stored body of an item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BodyRef {
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
}

/// One file kept from a repository archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

/// One entry of a repository archive that was not kept, with the rule that dropped it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DroppedEntry {
    pub path: String,
    pub reason: String,
}

/// The caps a repository was filtered under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemCaps {
    pub max_files: u32,
    pub max_unpacked_bytes: u64,
}

/// The immutable manifest the route writes last for each stored item (`items/<id>/manifest.json`); its presence is the commit. The
/// SHA-256 of its canonical bytes is the item's index hash, so an index row can be checked against the route's own record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemManifest {
    /// [`ITEM_SCHEMA`]
    pub schema: String,
    /// `i-0002`
    pub item_id: String,
    pub kind: ItemKind,
    pub run: String,
    /// `direct`
    pub agent: String,
    pub provider: ResearchProvider,
    pub request_id: String,
    /// The page address or the repository URL.
    pub origin: String,
    /// The 40-hex commit id a repository archive carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// RFC 3339 UTC.
    pub retrieved_at: String,
    pub licence: LicenceEvidence,
    pub body: BodyRef,
    /// A page: the hash of the text as the provider returned it, before the one cleaning pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ManifestFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<DroppedEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<ItemCaps>,
    /// True when the page text was cut at the page cap.
    #[serde(default)]
    pub truncated: bool,
}

impl ItemManifest {
    /// Compact JSON with every object's keys sorted and no trailing newline: one byte form, so the hash is stable.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        serde_json::to_vec(&crate::export::canonical(&value)).unwrap_or_default()
    }

    /// Lower-case hex SHA-256 of [`Self::canonical_bytes`].
    pub fn sha256(&self) -> String {
        Sha256::digest(self.canonical_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The first rule the manifest breaks.
    pub fn validate(&self) -> Result<(), ResearchError> {
        let bad = |m: &str| Err(ResearchError::usage(format!("manifest: {m}")));
        if self.schema != ITEM_SCHEMA {
            return bad("schema is not hamh-research-item/1");
        }
        if !s3::is_item_id(&self.item_id) {
            return bad("itemId is not i-NNNN");
        }
        if self.agent != DIRECT_AGENT_ID {
            return bad("agent is not direct");
        }
        if !is_ulid(&self.run) {
            return bad("run is not a ULID");
        }
        if !is_utc_timestamp(&self.retrieved_at) {
            return bad("retrievedAt is not an RFC 3339 UTC time");
        }
        let kind_ok = match self.kind {
            ItemKind::Page => self.provider.supports_fetch(),
            ItemKind::Repo => self.provider == ResearchProvider::GithubArchive,
            ItemKind::Attachment => self.provider == ResearchProvider::User,
        };
        if !kind_ok {
            return bad("provider does not fit the kind");
        }
        if self.kind == ItemKind::Repo && self.commit.is_none() {
            return bad("a repository records its commit");
        }
        if let Some(c) = &self.commit
            && !is_lower_hex(c, 40)
        {
            return bad("commit is not 40 lower-case hex characters");
        }
        if self.licence.spdx.is_empty()
            || self.licence.spdx.len() > 128
            || self.licence.source.is_empty()
        {
            return bad("licence needs an spdx value and a source");
        }
        if let Some(h) = &self.licence.evidence_sha256
            && !is_lower_hex(h, 64)
        {
            return bad("licence evidenceSha256 is not 64 lower-case hex characters");
        }
        if !is_lower_hex(&self.body.sha256, 64) {
            return bad("body sha256 is not 64 lower-case hex characters");
        }
        let item_prefix = format!("{}items/{}/", s3::prefix(&self.run), self.item_id);
        if !self.body.key.starts_with(&item_prefix) {
            return bad("body key is not under this item's prefix");
        }
        if let Some(h) = &self.source_sha256
            && !is_lower_hex(h, 64)
        {
            return bad("sourceSha256 is not 64 lower-case hex characters");
        }
        for f in &self.files {
            if !safe_relative_path(&f.path) {
                return bad("a file path is not a safe relative path");
            }
            if !is_lower_hex(&f.sha256, 64) {
                return bad("a file sha256 is not 64 lower-case hex characters");
            }
        }
        if self
            .dropped
            .iter()
            .any(|d| d.path.is_empty() || d.reason.is_empty())
        {
            return bad("a dropped entry needs a path and a reason");
        }
        Ok(())
    }
}

/// What a call asked for, as the audit object records it (the exact query and URLs; never a header or a key).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditInput {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Bytes of model-chosen outbound text this call counted against the run's budget.
    pub bytes_out: u64,
}

/// One immutable object per call (`audit/<time>-<request-id>.json`). S3 cannot append, so each call writes its own. The LC-1 and LC-2
/// checks of the lifecycle scorer read these, so Cline's own tool log is not the only record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuditObject {
    /// [`AUDIT_SCHEMA`]
    pub schema: String,
    pub run: String,
    /// `direct`
    pub agent: String,
    pub op: ResearchOp,
    pub provider: ResearchProvider,
    /// RFC 3339 UTC.
    pub at: String,
    pub request_id: String,
    pub status: AuditStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ResearchErrorCode>,
    pub ms: u64,
    pub input: AuditInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<u32>,
    /// The ids of the items this call stored.
    pub items: Vec<String>,
    /// A fetch: whether every URL had appeared before (the provenance gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_before: Option<bool>,
    /// The upstream's own request id (Tavily's), when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

/// The per-run policy row in the research table (`run#<run>` / `cfg`), written by `run-session` when the run starts and read by the
/// route on every call: research on or off, the limits, and when the row stops being honoured. A missing, switched-off or expired row
/// refuses every call with `disabled`, so switching research off stops a running run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyItem {
    #[serde(rename = "PK")]
    pub pk: String,
    #[serde(rename = "SK")]
    pub sk: String,
    pub enabled: bool,
    /// The stage's search provider, copied from the stage configuration.
    pub provider: ResearchProvider,
    pub limits: ResearchLimits,
    /// Epoch seconds: the end of the run's longest lifetime.
    pub expires_at: u64,
    /// DynamoDB time-to-live: epoch seconds, a week after `expiresAt`.
    pub ttl: u64,
    /// URLs the user's prompt named, as `run-session` found them (at most [`MAX_PROMPT_URLS`]): they count as seen for the provenance gate, so
    /// a fetch of a page the user pointed at needs no search first. The route canonicalises them when it compares.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_urls: Vec<String>,
}

/// URLs of the prompt a policy row may carry: 150 URLs of the longest allowed length, 2,048 bytes, are about 300 KB, which one DynamoDB item
/// (400 KB) holds with room for the rest of the row.
pub const MAX_PROMPT_URLS: usize = 150;

/// The sort key of the policy row.
pub const POLICY_SK: &str = "cfg";
/// Seconds the row outlives `expiresAt` before DynamoDB may delete it.
pub const POLICY_TTL_GRACE_S: u64 = 7 * 24 * 3600;

impl PolicyItem {
    pub fn new(
        run_id: &str,
        enabled: bool,
        provider: ResearchProvider,
        limits: ResearchLimits,
        expires_at: u64,
    ) -> Self {
        Self {
            pk: Self::pk_for(run_id),
            sk: POLICY_SK.to_string(),
            enabled,
            provider,
            limits,
            expires_at,
            ttl: expires_at.saturating_add(POLICY_TTL_GRACE_S),
            prompt_urls: Vec::new(),
        }
    }

    /// The URLs the prompt named ([`PolicyItem::prompt_urls`]).
    pub fn with_prompt_urls(mut self, urls: Vec<String>) -> Self {
        self.prompt_urls = urls;
        self
    }

    /// The partition key of every research item of a run.
    pub fn pk_for(run_id: &str) -> String {
        format!("run#{run_id}")
    }

    pub fn validate(&self) -> Result<(), ResearchError> {
        let bad = |m: &str| Err(ResearchError::usage(format!("policy: {m}")));
        match self.pk.strip_prefix("run#") {
            Some(run) if is_ulid(run) => {}
            _ => return bad("PK is not run#<ULID>"),
        }
        if self.sk != POLICY_SK {
            return bad("SK is not cfg");
        }
        if !self.provider.is_search_provider() {
            return bad("provider is not a search provider");
        }
        if self.ttl < self.expires_at {
            return bad("ttl is before expiresAt");
        }
        if self.prompt_urls.len() > MAX_PROMPT_URLS
            || self
                .prompt_urls
                .iter()
                .any(|u| u.is_empty() || u.len() > self.limits.url_bytes as usize)
        {
            return bad("promptUrls is at most 50 URLs within urlBytes");
        }
        self.limits.validate()
    }
}

// ---- the run request and the summary ----------------------------------------------------------------------------------------------------

/// `StartDirectRunRequest.research` (protocol 1.38.0): the user's switch. Absent means the stage default, which is on (the card shows
/// it); `ham run --no-research` sends `{"enabled": false}`. `run-session` copies the answer into the run's policy row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResearchOptions {
    pub enabled: bool,
}

/// What a delivered file that equals a file the run stored through the research route does (`StartDirectRunRequest.third_party`, and
/// `ValidateRequest.third_party` of the bundle validator). Protocol 1.38.0: the default is `block`, which ends the run
/// without a zip; `ham ... --allow-third-party` sends `allow`, which lets the run pass and reports the copy as a finding. `ham` sends
/// only `allow`; `block` (the default, and what another client may send) is left off the bundle validator's payload, so a run that keeps the
/// default is byte for byte the run of an older client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ThirdPartyMode {
    /// The copy is a violation: the run is not delivered.
    #[default]
    Block,
    /// The copy is reported as a finding and the run passes (the owner's per-run setting).
    Allow,
}

impl ThirdPartyMode {
    pub fn is_block(&self) -> bool {
        *self == ThirdPartyMode::Block
    }
}

/// What agentd's own research API did during a run, in counts (no text, no address): the `counts` of [`DirectResearchNote`].
/// A call is a request the engine's `ham-research` command made to the loopback API. The numbers are this agentd process's; a second invocation of
/// the same run on the same VM starts again from zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectResearchCounts {
    /// Calls to `search`.
    pub searches: u64,
    /// Calls to `fetch`.
    pub fetches: u64,
    /// Calls to `repo`.
    pub repos: u64,
    /// Calls to `wait` (polls of a repository download that said pending).
    pub waits: u64,
    /// Items checked against their manifest and placed read-only in the context directory.
    pub placed: u64,
    /// Calls the policy refused (a secret-shaped string, an address the run never saw, a host that is not served, research off, no key).
    pub refused: u64,
    /// Every other call that gave nothing: a limit, the upstream, the route, a hash that differed, a placement that failed.
    pub errors: u64,
    /// Bytes placed in the context directory: the text of the pages and the unpacked files of the repositories.
    pub placed_bytes: u64,
}

/// The research part of `summary.json` for agentd to write: whether the research lane ran,
/// which provider answered, what the route counted for the run and what agentd's own API did. Counts only; it never changes the label, the
/// outcome or the delivery.
///
/// - `enabled` is true when agentd served the lane to the engine (its loopback API was up), also when the engine never called it; false, with
///   nothing else, when the user switched research off for the run or the route said it is off (`disabled`, `no_key`) or the lane could not
///   start. A stage without the research route leaves the whole member out.
/// - `usage` is the route's own count for the run, read once more when the run ends. It is left out when that read failed: a zero would claim
///   that the route counted nothing.
/// - `counts` is agentd's own count of what its API did. The two are different quantities and need not be equal (one `fetch` call can name
///   several addresses; the route counts per run, agentd per process).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectResearchNote {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ResearchProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResearchUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<DirectResearchCounts>,
}

/// The stored research objects as one document, exported in `runtime.schema.json` (`RuntimeContract.research`).
#[derive(Debug, JsonSchema)]
pub struct ResearchContract {
    pub item_manifest: ItemManifest,
    pub audit: AuditObject,
    pub policy: PolicyItem,
    pub note: DirectResearchNote,
    pub limits: ResearchLimits,
}

// ---- S3 layout ------------------------------------------------------------------------------------------------------------------------

/// The keys under `runs/<run>/agents/direct/research/`. Only `research-api` and `research-job` write here; a bucket-policy Deny stops
/// the worker session role, so Cline cannot forge a manifest, an audit object or an index.
pub mod s3 {
    use super::ItemKind;
    use crate::direct::DIRECT_AGENT_ID;

    /// `runs/<run>/agents/direct/research/`
    pub fn prefix(run_id: &str) -> String {
        format!("runs/{run_id}/agents/{DIRECT_AGENT_ID}/research/")
    }

    /// `items/<item>/manifest.json`, written last: its presence is the commit of the item.
    pub fn manifest_key(run_id: &str, item_id: &str) -> String {
        format!("{}items/{item_id}/manifest.json", prefix(run_id))
    }

    /// `items/<item>/body.txt` (a page), `body.tar.gz` (a repository) or `body.bin` (an attachment).
    pub fn body_key(run_id: &str, item_id: &str, kind: ItemKind) -> String {
        let name = match kind {
            ItemKind::Page => "body.txt",
            ItemKind::Repo => "body.tar.gz",
            ItemKind::Attachment => "body.bin",
        };
        format!("{}items/{item_id}/{name}", prefix(run_id))
    }

    /// `audit/<UTC stamp>-<request id>.json`; `stamp` is `YYYYMMDDTHHMMSSZ`.
    pub fn audit_key(run_id: &str, stamp: &str, request_id: &str) -> String {
        format!("{}audit/{stamp}-{request_id}.json", prefix(run_id))
    }

    /// `searches/<id>.json`: the snippets shown to Cline.
    pub fn search_key(run_id: &str, search_id: &str) -> String {
        format!("{}searches/{search_id}.json", prefix(run_id))
    }

    /// `i-0001`
    pub fn item_id(n: u32) -> String {
        format!("i-{n:04}")
    }

    /// `s-0001`
    pub fn search_id(n: u32) -> String {
        format!("s-{n:04}")
    }

    fn counted(id: &str, letter: u8) -> bool {
        let b = id.as_bytes();
        b.len() >= 6 && b[0] == letter && b[1] == b'-' && b[2..].iter().all(u8::is_ascii_digit)
    }

    /// `i-` and at least four digits.
    pub fn is_item_id(id: &str) -> bool {
        counted(id, b'i')
    }

    /// `s-` and at least four digits.
    pub fn is_search_id(id: &str) -> bool {
        counted(id, b's')
    }
}
