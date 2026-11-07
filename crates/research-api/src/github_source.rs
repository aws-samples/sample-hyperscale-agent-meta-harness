//! The GitHub archive source: `https://github.com/<owner>/<repo>/archive/<ref>.tar.gz` and nothing else.
//!
//! GitHub answers that address with a redirect to `codeload.github.com`. This source follows exactly that one redirect, requires its
//! target to be `https://codeload.github.com` (the port, if any, 443; no user information) and treats any other place, a second redirect
//! or an answer without a redirect as an `upstream_error` ("unexpected redirect host"). It sends no credential and no cookie: a public
//! archive needs none, and no GitHub token is ever stored in the stage account. The body is read only up to the caller's cap.

use crate::http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use crate::ports::{ArchiveSource, Downloaded, Fail, GithubRepo};
use harness_protocol::research::ResearchErrorCode;
use std::time::Duration;
use tokio::time::Instant;

const GITHUB: &str = "https://github.com";
const CODELOAD_HOST: &str = "codeload.github.com";
/// The whole download, both requests.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// The longest `retry-after` that is passed on.
const MAX_RETRY_AFTER_S: u64 = 3600;

/// Downloads a public repository archive with an [`HttpClient`] (its own limits are overridden per call).
#[derive(Clone, Debug)]
pub struct HttpArchiveSource {
    http: HttpClient,
    timeout: Duration,
}

impl HttpArchiveSource {
    pub fn new(http: HttpClient) -> Self {
        Self {
            http,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// The time allowed for the whole download (ten minutes unless set).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn get(
        &self,
        url: &str,
        max_bytes: u64,
        deadline: Instant,
    ) -> Result<HttpResponse, Fail> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Fail::upstream("GitHub did not answer in time"));
        }
        let request = HttpRequest {
            method: http::Method::GET,
            url: url.to_string(),
            headers: vec![("accept".to_string(), "*/*".to_string())],
            body: Vec::new(),
        };
        let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        self.http
            .send_limited(request, cap, remaining)
            .await
            .map_err(|e| match e {
                HttpError::TooLarge => Fail::new(
                    ResearchErrorCode::TooLarge,
                    format!("the archive is larger than {max_bytes} bytes"),
                ),
                HttpError::Timeout => Fail::upstream("GitHub did not answer in time"),
                HttpError::InvalidRequest(_) => {
                    Fail::internal("the archive request could not be built")
                }
                other => Fail::upstream(format!("GitHub could not be reached ({})", other.class())),
            })
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn unexpected_redirect() -> Fail {
    Fail::upstream("unexpected redirect host")
}

/// The address the redirect names, when it is `https://codeload.github.com[:443]/...`.
fn codeload_target(response: &HttpResponse) -> Result<String, Fail> {
    let location = response
        .header("location")
        .ok_or_else(unexpected_redirect)?
        .trim();
    if location
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(unexpected_redirect());
    }
    let rest = match location.get(..8) {
        Some(scheme) if scheme.eq_ignore_ascii_case("https://") => &location[8..],
        _ => return Err(unexpected_redirect()),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = match authority.rsplit_once(':') {
        Some((host, "443")) => host,
        Some(_) => return Err(unexpected_redirect()),
        None => authority,
    };
    if !host.eq_ignore_ascii_case(CODELOAD_HOST) {
        return Err(unexpected_redirect());
    }
    let tail = tail.split('#').next().unwrap_or("");
    let tail = if tail.is_empty() || tail.starts_with('?') {
        format!("/{tail}")
    } else {
        tail.to_string()
    };
    Ok(format!("https://{CODELOAD_HOST}{tail}"))
}

fn retry_after(response: &HttpResponse) -> Option<u32> {
    response
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|s| u32::try_from(s.min(MAX_RETRY_AFTER_S)).unwrap_or(3600))
}

/// The word for a status that is not the archive.
fn status_failure(status: u16, response: &HttpResponse) -> Fail {
    match status {
        404 => Fail::new(
            ResearchErrorCode::NotFound,
            "the repository or the ref was not found",
        ),
        403 | 429 => {
            let fail = Fail::new(
                ResearchErrorCode::UpstreamRateLimited,
                "GitHub is limiting archive downloads",
            );
            match retry_after(response) {
                Some(seconds) => fail.with_retry_after(seconds),
                None => fail,
            }
        }
        other => Fail::upstream(format!("GitHub answered {other}")),
    }
}

fn plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl ArchiveSource for HttpArchiveSource {
    async fn download(
        &self,
        repo: &GithubRepo,
        git_ref: Option<&str>,
        max_bytes: u64,
    ) -> Result<Downloaded, Fail> {
        let git_ref = git_ref.unwrap_or("HEAD");
        let ref_ok = !git_ref.is_empty()
            && !git_ref.starts_with('-')
            && !git_ref.contains("..")
            && git_ref
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
        if !plain_name(&repo.owner) || !plain_name(&repo.repo) || !ref_ok {
            return Err(Fail::new(
                ResearchErrorCode::Usage,
                "the repository name or the ref is not valid",
            ));
        }
        let deadline = Instant::now() + self.timeout;
        let url = format!(
            "{GITHUB}/{}/{}/archive/{git_ref}.tar.gz",
            repo.owner, repo.repo
        );
        let first = self.get(&url, max_bytes, deadline).await?;
        if !is_redirect(first.status) {
            return Err(match first.status {
                200..=299 => Fail::upstream("GitHub answered without a redirect"),
                status => status_failure(status, &first),
            });
        }
        let target = codeload_target(&first)?;
        let second = self.get(&target, max_bytes, deadline).await?;
        match second.status {
            200 => Ok(Downloaded {
                bytes: second.body,
                final_host: CODELOAD_HOST.to_string(),
            }),
            status if is_redirect(status) => Err(unexpected_redirect()),
            status => Err(status_failure(status, &second)),
        }
    }
}
