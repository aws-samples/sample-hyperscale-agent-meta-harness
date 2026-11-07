//! UTC time helpers without a date library: RFC 3339 parse and format, a test clock, and ULIDs
//! for event ids.

use std::io::Read;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_unix(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        now_unix()
    }
}

/// A clock tests set by hand.
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicI64,
}

impl ManualClock {
    pub fn new(t: i64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(t),
        })
    }
    pub fn set(&self, t: i64) {
        self.now.store(t, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_unix(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// Howard Hinnant's civil-date algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+00:00)` to Unix seconds; any other offset is refused.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (
        i64::from(digits(s.get(0..4)?)?),
        digits(s.get(5..7)?)?,
        digits(s.get(8..10)?)?,
    );
    let (h, mi, sec) = (
        digits(s.get(11..13)?)?,
        digits(s.get(14..16)?)?,
        digits(s.get(17..19)?)?,
    );
    if !(1..=12).contains(&mo)
        || d == 0
        || d > days_in_month(y, mo)
        || h > 23
        || mi > 59
        || sec > 59
    {
        return None;
    }
    let mut rest = s.get(19..)?;
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &frac[n..];
    }
    if !matches!(rest, "Z" | "z" | "+00:00") {
        return None;
    }
    Some(
        days_from_civil(y, mo, d) * 86_400
            + i64::from(h) * 3600
            + i64::from(mi) * 60
            + i64::from(sec),
    )
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_rfc3339(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
static FALLBACK: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// The 80 random bits of a ULID.
const RAND_MASK: u128 = (1 << 80) - 1;
/// The 48 millisecond bits of a ULID.
const MS_MASK: u64 = (1 << 48) - 1;

fn random_80() -> u128 {
    let mut buf = [0u8; 10];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        // No urandom: a counter-mixed value still keeps ids unique within the process.
        let x = FALLBACK.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::SeqCst) ^ now_millis();
        buf[..8].copy_from_slice(&x.to_be_bytes());
    }
    buf.iter().fold(0u128, |acc, b| (acc << 8) | u128::from(*b))
}

/// What a monotonic ULID generator remembers: the newest millisecond it handed out and the random part it used in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UlidState {
    ms: u64,
    rand: u128,
}

impl UlidState {
    /// The time part and the random part of the newest id.
    pub fn last(&self) -> (u64, u128) {
        (self.ms, self.rand)
    }

    /// A state that has just handed out the id made of `ms` and `rand` (the lower 80 bits).
    pub const fn at(ms: u64, rand: u128) -> Self {
        Self {
            ms: ms & MS_MASK,
            rand: rand & RAND_MASK,
        }
    }
}

fn encode_ulid(ms: u64, rand: u128) -> String {
    let v = (u128::from(ms & MS_MASK) << 80) | (rand & RAND_MASK);
    (0..26)
        .map(|i| CROCKFORD[((v >> (125 - 5 * i)) & 31) as usize] as char)
        .collect()
}

/// The next ULID of a monotonic generator (the ULID specification's monotonic mode): every id is greater than the one before it,
/// whatever the clock and the random source do.
///
/// - `now_ms` after the newest millisecond handed out: that millisecond and a fresh random part from `rng` (called only here).
/// - The same millisecond, or an earlier one (a clock that stepped back): the newest millisecond stays and the random part grows by one.
/// - The random part is full (2^80 ids in one millisecond): the next millisecond and a fresh random part. Never a panic.
///
/// Two events of one millisecond (agentd's `input-received` and the `input-closed` that follows it) therefore sort in the order they
/// were made; a random tail sorted them either way.
pub fn ulid_from(now_ms: u64, state: &mut UlidState, rng: &mut impl FnMut() -> u128) -> String {
    let now_ms = now_ms & MS_MASK;
    if now_ms > state.ms {
        state.ms = now_ms;
        state.rand = rng() & RAND_MASK;
    } else if state.rand >= RAND_MASK {
        state.ms = state.ms.wrapping_add(1) & MS_MASK;
        state.rand = rng() & RAND_MASK;
    } else {
        state.rand += 1;
    }
    encode_ulid(state.ms, state.rand)
}

static ULID_STATE: Mutex<UlidState> = Mutex::new(UlidState { ms: 0, rand: 0 });

/// A ULID that is greater than every one this process made before: 48-bit millisecond time and 80 bits in Crockford base32, the
/// random part counting up within a millisecond ([`ulid_from`]). An event id into a shard is made with this and nothing else.
pub fn new_ulid() -> String {
    let mut state = ULID_STATE.lock().unwrap_or_else(|p| p.into_inner());
    ulid_from(now_millis(), &mut state, &mut random_80)
}
