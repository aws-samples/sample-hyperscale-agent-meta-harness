//! The stage starts no new runs while a deploy holds its deploy lock.
//!
//! Setup and deploy create the SSM parameter `<deployment prefix>/deploy-lock` before they change the stage, write it again at least every ten
//! minutes while they work and delete it at the end (the deploy lock of `scripts/setup/runner.sh` and `scripts/lib/deploy.py`). The lock is the one
//! mark of an update: there is no second flag. A request that would launch a run reads it right before the write that creates the run
//! ([`admit`], called by `direct::start_prepared`, where `POST /v1/direct/runs`, the retry and the resume all end), and the stage is closed to it
//! while the lock
//!
//! 1. exists,
//! 2. has no line that is exactly `deploys=no` (setup writes it for a hold that changes nothing on the stage: a client-only update, the removal of
//!    a runner that was left, and the time after its build has succeeded), and
//! 3. was written by SSM at most [`STALE_AFTER_S`] seconds ago: the deploy's own rule for a lock nobody renews. The time is the parameter's
//!    `LastModifiedDate`, which SSM sets, and not a time inside the value, which comes from the clock of the holder's machine.
//!
//! The answer is then 409 [`STAGE_UPDATING`] with [`MESSAGE`] and `Retry-After: `[`RETRY_AFTER_S`], and one line in the log. Every other route
//! stays open, so running work can end by itself.
//!
//! What never closes the stage: no parameter (the normal case, no log line); a lock nobody renews (one log line with its age); and a read the
//! stage's own setup refuses, for example a role without the grant on a stage whose function is newer than its role (one log line with the
//! reason): no retry would help, and an unreadable parameter must not stop every user. A read that may pass on a retry (a throttle, a fault of
//! SSM, no answer) is the 503 of the deployment read beside it: every start already depends on SSM, and letting the run start there would open
//! the stage at the moment a deploy puts load on it.

use crate::handler::{DeployLock, Execution, Reply, StoreError, V1Response, unavailable};
use harness_protocol::api::ApiError;

/// The `code` of the refusal.
pub use ham_core::update::STAGE_UPDATING;

/// The sentence of the refusal, shown by `ham` as it is.
pub const MESSAGE: &str =
    "This stage is being updated, so it starts no new runs. Try again in a few minutes.";

/// The seconds of the refusal's `Retry-After` header: a polite pause before the next try, not the length of the update.
pub const RETRY_AFTER_S: u32 = 120;

/// How long after its last write a lock still closes the stage: the one integer line of `scripts/lib/deploy-lock-stale-seconds.txt`, the file
/// setup reads for its own rule (`RN_LOCK_STALE_SECONDS`), so the two cannot differ. A text that is no positive integer fails the build.
pub const STALE_AFTER_S: u64 = seconds(include_str!(
    "../../../scripts/lib/deploy-lock-stale-seconds.txt"
));

/// The line of a lock whose holder changes nothing on the stage.
const DEPLOYS_NOTHING: &str = "deploys=no";

const fn seconds(text: &str) -> u64 {
    let b = text.as_bytes();
    let mut i = 0;
    let mut n: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        assert!(
            i < 9,
            "deploy-lock-stale-seconds.txt: the number is too long"
        );
        n = n * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    assert!(
        n > 0,
        "deploy-lock-stale-seconds.txt must start with a positive integer"
    );
    while i < b.len() {
        assert!(
            b[i] == b'\n' || b[i] == b'\r',
            "deploy-lock-stale-seconds.txt must hold one integer line and nothing else"
        );
        i += 1;
    }
    n
}

/// What a lock that exists means for a request that would launch a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Its holder deploys nothing: the stage is open.
    DeploysNothing,
    /// Nobody wrote it for more than [`STALE_AFTER_S`] seconds (`None`: SSM named no time): its holder is taken for gone and the stage is open.
    NotRenewed { age_s: Option<u64> },
    /// A deploy holds the stage: it was written `age_s` seconds ago.
    Closed { age_s: u64 },
}

/// Judges a lock by its text and by the time SSM wrote it, against the clock `now_epoch_s` (Unix seconds). A write time ahead of the clock
/// is a write of this moment.
pub fn judge(lock: &DeployLock, now_epoch_s: i64) -> Verdict {
    if lock.value.lines().any(|line| line == DEPLOYS_NOTHING) {
        return Verdict::DeploysNothing;
    }
    let Some(written) = lock.written_epoch_s else {
        return Verdict::NotRenewed { age_s: None };
    };
    let age_s = u64::try_from(now_epoch_s.saturating_sub(written)).unwrap_or(0);
    if age_s > STALE_AFTER_S {
        Verdict::NotRenewed { age_s: Some(age_s) }
    } else {
        Verdict::Closed { age_s }
    }
}

fn now_epoch_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// 409 [`STAGE_UPDATING`] with the `Retry-After` header; the line for the log names the age of the lock and nothing of its text.
fn refusal(age_s: u64) -> Reply {
    let body = serde_json::to_string(&ApiError {
        code: STAGE_UPDATING.into(),
        message: MESSAGE.into(),
        request_id: None,
    })
    .unwrap_or_else(|_| "{}".into());
    (
        V1Response {
            status: 409,
            body,
            retry_after_s: Some(RETRY_AFTER_S),
        },
        vec![format!(
            "the start is refused: the deploy lock of the stage was written {age_s} s ago, so a deploy holds the stage"
        )],
    )
}

/// Asks the stage's deploy lock about a request that would launch a run. `Ok` lets it go on, with the lines for the log (none in the
/// normal case); `Err` is the answer to send in its place: the 409 of a stage that is being updated, or the 503 of a read that may pass on a
/// retry (`retry` are its retry words).
pub(crate) async fn admit<E: Execution>(execution: &E, retry: &str) -> Result<Vec<String>, Reply> {
    match execution.deploy_lock().await {
        Ok(None) => Ok(Vec::new()),
        Ok(Some(lock)) => match judge(&lock, now_epoch_s()) {
            Verdict::DeploysNothing => Ok(Vec::new()),
            Verdict::NotRenewed { age_s } => {
                let age = age_s.map_or_else(
                    || "at a time SSM did not name".to_string(),
                    |s| format!("{s} s ago"),
                );
                Ok(vec![format!(
                    "the deploy lock of the stage was last written {age} (the limit is {STALE_AFTER_S} s): nobody renews it, so the run starts"
                )])
            }
            Verdict::Closed { age_s } => Err(refusal(age_s)),
        },
        Err(e @ StoreError::NotSetUp(_)) => Ok(vec![format!(
            "the deploy lock of the stage could not be read, so the run starts: {e}"
        )]),
        Err(e) => Err(unavailable(&e, retry)),
    }
}

#[cfg(test)]
mod tests {
    use super::seconds;

    #[test]
    fn b804_admit_the_number_file_is_read_as_one_integer_line() {
        assert_eq!(seconds("1800\n"), 1800);
        assert_eq!(seconds("1800"), 1800);
        assert_eq!(seconds("900\r\n"), 900);
    }
}
