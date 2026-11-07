//! Small pure helpers of the counters: the minute a rate window belongs to and the bytes a call counts against the outbound budget.

/// Epoch minutes of a time in milliseconds.
pub fn epoch_minute(now_ms: u64) -> u64 {
    now_ms / 60_000
}

/// The bytes of model-chosen text a call counts against `outbound_bytes`.
pub fn outbound_len(parts: &[&str]) -> u64 {
    parts.iter().map(|p| p.len() as u64).sum()
}
