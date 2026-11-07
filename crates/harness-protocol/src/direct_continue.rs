//! A run that starts from the head an earlier run delivered.
//!
//! `ham continue RUN` sends the normal start request with one more member, `continueFrom`. control-api checks that the source run exists, belongs
//! to the caller and delivered a head, and stores the request with the head it resolved (`headSha` is then always present, so agentd can check the
//! seed). run-session passes `fromRun` to the repo-gateway, which stages the source run's verified bundle as the new run's `input/repo.bundle`, with
//! the delivered head as the checked-out commit. The member is accepted only with the `s3-only` target and no `baseRef`; a repository run keeps
//! `--base harness/<runId>`. The prompt Cline receives is the user's own text; nothing about the earlier run is injected.
//!
//! Everything here is re-exported from [`crate::direct`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The source of a follow-up run. `run_id` is the finished run; `head_sha` is the commit it delivered (40 lowercase hex).
///
/// On the wire from `ham`, `headSha` may be left out (the user asked for "the head this run delivered"); when it is present it must equal the
/// source run's delivered head, or the start is refused (409): the client continues from the commit it showed, or not at all. In the stored
/// request, and everywhere after control-api, `headSha` is present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinueFrom {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

/// The rule a [`ContinueFrom`] breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinueFromError {
    /// `runId` is not 26 letters and digits.
    BadRunId,
    /// `headSha` is not 40 lowercase hexadecimal digits.
    BadHeadSha,
}

impl std::fmt::Display for ContinueFromError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRunId => write!(
                f,
                "continueFrom.runId must be a run id (26 letters and digits)"
            ),
            Self::BadHeadSha => write!(
                f,
                "continueFrom.headSha must be a full commit id (40 lowercase hexadecimal digits)"
            ),
        }
    }
}

impl std::error::Error for ContinueFromError {}

/// Checks the shape of a [`ContinueFrom`]. Whether the run exists, is the caller's and delivered that head is control-api's check.
pub fn check_continue_from(c: &ContinueFrom) -> Result<(), ContinueFromError> {
    if c.run_id.len() != RUN_ID_LEN || !c.run_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ContinueFromError::BadRunId);
    }
    if let Some(h) = &c.head_sha
        && !is_commit_id(h)
    {
        return Err(ContinueFromError::BadHeadSha);
    }
    Ok(())
}

/// The length of a run id (a ULID).
pub const RUN_ID_LEN: usize = 26;

/// Whether `s` is a full commit id: 40 lowercase hexadecimal digits.
pub fn is_commit_id(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
