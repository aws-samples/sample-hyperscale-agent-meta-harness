//! Run and event identifiers, timestamps and digests.

use sha2::{Digest, Sha256};

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A ULID (48-bit millisecond time, 80 random bits, Crockford base32): run ids and event ids, so
/// both sort by creation.
pub fn new_ulid() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut rnd = [0u8; 10];
    // getrandom fails only when the OS has no entropy source.
    getrandom::fill(&mut rnd).unwrap_or_else(|e| panic!("no OS entropy: {e}"));
    let v = ((ms & ((1 << 48) - 1)) << 80)
        | rnd.iter().fold(0u128, |acc, b| (acc << 8) | u128::from(*b));
    (0..26)
        .map(|k| CROCKFORD[((v >> (125 - 5 * k)) & 31) as usize] as char)
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds (civil-from-days, Howard Hinnant).
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn now_rfc3339() -> String {
    rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
