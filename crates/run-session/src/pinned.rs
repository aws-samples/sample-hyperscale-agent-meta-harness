//! Reading the end-of-run objects of a run by the version records agentd sent.
//!
//! agentd writes `bundle`, `summary.json` and `test-report.json` under `runs/<run>/agents/direct/turns/<n>/` with the run's session credentials,
//! and the engine holds the same credentials: a process it leaves behind can write a newer version of any of these keys. The service therefore
//! does not read "the object at the key" for a run that requires version records. It reads the version agentd recorded for its own write and
//! checks the digest, and it reads no other version. The records arrive in `turn-complete`, a call that needs the run's completion key, which
//! the engine does not hold; worker-api keeps them only from such a call, and only for a run whose Agent item says it requires them.
//!
//! What this covers: the three objects above. What it leaves as it was: the periodic snapshot (`snapshot.json`, `snapshot.bundle`), which the
//! service delivers only when a session ended with no final bundle and which its label says is a snapshot.

use crate::ports::{Blobs, Res, Store};
use harness_protocol::api::internal::TurnCompleteRequest;
use harness_protocol::direct::DIRECT_AGENT_ID;
use harness_protocol::output_pins::{OutputPin, OutputPins};

/// How far the end-of-run objects of one slice can be trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The run's Agent item does not require version records (it was written before they existed): the objects are read at their keys, as
    /// they always were.
    Legacy,
    /// The run requires them and the turn carried a well formed set.
    Pinned(OutputPins),
    /// The run requires them and none came (the session ended without a `turn-complete`, or its set was not well formed). Nothing at the keys
    /// is the harness's, so none of the three objects is read.
    Unattested,
}

/// The trust for the slice whose `turn-complete` is `turn`. An Agent item that is missing reads as a run that does not require records, as the
/// completion-key check reads it.
pub async fn trust_of<S: Store>(
    store: &S,
    run_id: &str,
    turn: Option<&TurnCompleteRequest>,
) -> Res<Trust> {
    let required = store
        .get_agent(run_id, DIRECT_AGENT_ID)
        .await?
        .and_then(|a| a.creds)
        .is_some_and(|c| c.output_pins_required());
    if !required {
        return Ok(Trust::Legacy);
    }
    Ok(match turn.and_then(|t| t.output_pins.as_ref()) {
        Some(p) if p.validate().is_ok() => Trust::Pinned(p.clone()),
        _ => Trust::Unattested,
    })
}

/// What reading one object by its record found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read<T> {
    /// agentd recorded no such object: it stored none.
    Absent,
    /// The object is the one agentd wrote.
    Found(T),
    /// agentd recorded the object and what is there is not it: the version is gone, or the bytes or the stored digest differ. Names the object.
    Refused(&'static str),
}

/// The bytes of the version `pin` records, at most `max_bytes`, when their digest is the recorded one. A record with no version id is read at
/// the current version and held to the digest. A version that is missing, too large or different is [`Read::Refused`]: never a retry on the
/// current version.
pub async fn read<B: Blobs>(
    blobs: &B,
    key: &str,
    pin: Option<&OutputPin>,
    name: &'static str,
    max_bytes: usize,
) -> Res<Read<Vec<u8>>> {
    let Some(pin) = pin else {
        return Ok(Read::Absent);
    };
    Ok(
        match blobs
            .get_version_bounded(key, pin.version_id.as_deref(), max_bytes)
            .await?
        {
            Some(bytes) if pin.matches_body(&bytes) => Read::Found(bytes),
            _ => Read::Refused(name),
        },
    )
}

/// Whether the version `pin` records exists and S3 stored it with the recorded SHA-256. It reads no body, so a bundle of any size is checked
/// without a download.
pub async fn check<B: Blobs>(
    blobs: &B,
    key: &str,
    pin: Option<&OutputPin>,
    name: &'static str,
) -> Res<Read<()>> {
    let Some(pin) = pin else {
        return Ok(Read::Absent);
    };
    let want = pin.sha256_base64();
    Ok(
        match blobs.head_version(key, pin.version_id.as_deref()).await? {
            Some(f) if !want.is_empty() && f.sha256_base64.as_deref() == Some(want.as_str()) => {
                Read::Found(())
            }
            _ => Read::Refused(name),
        },
    )
}
