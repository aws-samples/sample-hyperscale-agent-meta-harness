//! The endpoints a run's sessions are called at.
//!
//! AgentCore's `qualifier` is an endpoint name. A bare runtime version is refused with
//! `ResourceNotFoundException: No endpoint or agent found with qualifier '50'` (the 4f77949
//! outage). deploy.sh keeps an endpoint `v<N>` on each recent worker version beside `live`. A
//! run's first dispatch pins the `v<N>` of the version `live` serves, so a later repoint of `live`
//! leaves the run on its version; without a READY `v<N>` it pins `live` and says why. Every session
//! call checks its qualifier with [`is_endpoint_name`] before a request leaves.

/// The endpoint deploy.sh verifies and repoints; the qualifier of unpinned calls.
pub const LIVE: &str = "live";
/// AgentCore endpoint names are 1 to 48 characters: `[a-zA-Z][a-zA-Z0-9_]{0,47}`.
pub const MAX_ENDPOINT_NAME: usize = 48;
/// A runtime version is a positive integer; this caps its digits.
const MAX_VERSION_DIGITS: usize = 19;

/// Whether `s` is an AgentCore endpoint name (`[a-zA-Z][a-zA-Z0-9_]{0,47}`). A bare version such
/// as `50` is not.
pub fn is_endpoint_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_ENDPOINT_NAME
        && b[0].is_ascii_alphabetic()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn is_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= MAX_VERSION_DIGITS
        && !v.starts_with('0')
        && v.bytes().all(|b| b.is_ascii_digit())
}

/// The endpoint deploy.sh keeps on runtime version `version`: `v<version>`; `None` for anything
/// that is not a version number.
pub fn version_endpoint(version: &str) -> Option<String> {
    is_version(version).then(|| format!("v{version}"))
}

/// The version a `v<N>` endpoint name stands for; `None` for any other name.
pub fn endpoint_version(name: &str) -> Option<u64> {
    let v = name.strip_prefix('v')?;
    if is_version(v) { v.parse().ok() } else { None }
}

/// An endpoint as GetAgentRuntimeEndpoint describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointState {
    pub name: String,
    /// `READY`, `CREATING`, `UPDATING`, `CREATE_FAILED`, ...
    pub status: String,
    /// The version the endpoint serves; absent while it is being created.
    pub live_version: Option<String>,
}

/// What a run is pinned to: the worker version its first dispatch found behind `live` (evidence)
/// and the endpoint every call on its sessions uses (the qualifier).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub version: String,
    pub endpoint: String,
}

/// A pin, and why it is `live` when it is not the version's own endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinChoice {
    pub pin: Pin,
    /// `v<N>-absent`, `v<N>-<status>` or `v<N>-serves-<M>`; `None` on the version endpoint.
    pub fallback: Option<String>,
}

/// The pin of a run's first dispatch: `v<N>` when that endpoint is READY and serves N, the version
/// `live` serves; otherwise `live` itself, with the reason. An error when `live` names no version.
pub fn choose_pin(
    live: &EndpointState,
    versioned: Option<&EndpointState>,
) -> Result<PinChoice, String> {
    let version = live
        .live_version
        .as_deref()
        .filter(|v| is_version(v))
        .ok_or_else(|| {
            format!(
                "the {} endpoint serves no runtime version ({:?})",
                live.name, live.live_version
            )
        })?;
    let name = format!("v{version}");
    let fallback = match versioned {
        None => Some(format!("{name}-absent")),
        Some(e) if e.status != "READY" => Some(format!("{name}-{}", e.status)),
        Some(e) if e.live_version.as_deref() != Some(version) => Some(format!(
            "{name}-serves-{}",
            e.live_version.as_deref().unwrap_or("none")
        )),
        Some(_) => None,
    };
    let endpoint = if fallback.is_some() {
        LIVE.to_string()
    } else {
        name
    };
    Ok(PinChoice {
        pin: Pin {
            version: version.to_string(),
            endpoint,
        },
        fallback,
    })
}

/// The qualifier of calls on a session whose run recorded `endpoint`; `live` when it recorded none
/// (a run from before pinning, or one the 4f77949 build pinned by a bare version).
pub fn session_qualifier(endpoint: Option<&str>) -> &str {
    endpoint.unwrap_or(LIVE)
}
