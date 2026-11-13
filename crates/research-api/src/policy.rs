//! The run's policy row, read on every call: research on or off, the limits and when the row stops being honoured. A missing, switched-off,
//! expired or malformed row refuses the call with `disabled`, so switching research off stops a running run.

use crate::ports::{Fail, Table};
use harness_protocol::research::{PolicyItem, ResearchErrorCode};

pub async fn active<T: Table>(table: &T, run: &str, now_s: u64) -> Result<PolicyItem, Fail> {
    let disabled = |why: &str| {
        Fail::new(
            ResearchErrorCode::Disabled,
            format!("research is off for this run ({why})"),
        )
    };
    let item = table
        .get_policy(run)
        .await?
        .ok_or_else(|| disabled("no policy"))?;
    if !item.enabled {
        return Err(disabled("switched off"));
    }
    if now_s >= item.expires_at {
        return Err(disabled("expired"));
    }
    item.validate().map_err(|_| disabled("invalid policy"))?;
    Ok(item)
}
