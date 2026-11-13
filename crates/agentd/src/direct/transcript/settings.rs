//! The transcript module's settings.
//! Every value has a default; a value that does not parse keeps the default and one out of range is held to
//! its bound.

use std::time::Duration;

/// How the writer behaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `AGENTD_TRANSCRIPT=off` switches the module off: it registers nothing and the run is as it was.
    pub enabled: bool,
    /// An object is written at the latest this long after its first record (`AGENTD_TRANSCRIPT_FLUSH_MS`, 100 ms to 30 s).
    pub flush_every: Duration,
    /// An object is written when its records pass this many bytes before compression (`AGENTD_TRANSCRIPT_FLUSH_BYTES`, 1 KiB to 512 KiB).
    pub flush_bytes: usize,
    /// Streamed message text is written when this much has gathered (`AGENTD_TRANSCRIPT_COALESCE_BYTES`) or has waited this long
    /// (`AGENTD_TRANSCRIPT_COALESCE_MS`), and at once when another message follows.
    pub coalesce_bytes: usize,
    pub coalesce_every: Duration,
    /// The payload of a record is cut to this many bytes (`AGENTD_TRANSCRIPT_RECORD_CAP_BYTES`, 1 KiB to 440 KiB); a record is cut to
    /// `pre_cap` first when it is larger, so the redaction rules see a bounded text.
    pub record_cap: usize,
    pub pre_cap: usize,
    /// The bytes of streamed text held back at each write so a secret split across two writes is seen whole
    /// (`AGENTD_TRANSCRIPT_WINDOW_BYTES`, 256 B to 64 KiB), and the most one open shape is held (`AGENTD_TRANSCRIPT_MAX_HOLD_BYTES`).
    pub window: usize,
    pub max_hold: usize,
    /// The unwritten bytes the module may hold in all: the capture queue and the objects waiting for upload
    /// (`AGENTD_TRANSCRIPT_QUEUE_BYTES`, 1 MiB to 256 MiB).
    pub queue_bytes: usize,
    /// The final flush before `turn-complete` is posted gives up after this long (`AGENTD_TRANSCRIPT_FINAL_MS`, at most 12 s: the driver
    /// gives the finalize hook 15 s).
    pub final_deadline: Duration,
    /// An upload is tried this many times (1 to 20) with a doubling pause from `retry_base` (`AGENTD_TRANSCRIPT_UPLOAD_TRIES`,
    /// `AGENTD_TRANSCRIPT_RETRY_MS`).
    pub upload_tries: u32,
    pub retry_base: Duration,
    /// The payload of an opaque record (a message the module does not know) is cut to this many bytes.
    pub opaque_cut: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            flush_every: Duration::from_secs(30),
            flush_bytes: 256 * 1024,
            coalesce_bytes: 8 * 1024,
            coalesce_every: Duration::from_secs(2),
            record_cap: 256 * 1024,
            pre_cap: 1024 * 1024,
            window: 4 * 1024,
            max_hold: 64 * 1024,
            queue_bytes: 32 * 1024 * 1024,
            final_deadline: Duration::from_secs(10),
            upload_tries: 6,
            retry_base: Duration::from_millis(200),
            opaque_cut: 4096,
        }
    }
}

impl Settings {
    /// The defaults with every `AGENTD_TRANSCRIPT_*` variable of `lookup` applied.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut s = Self::default();
        if let Some(v) = lookup("AGENTD_TRANSCRIPT")
            && matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no" | "disabled"
            )
        {
            s.enabled = false;
        }
        let num = |name: &str, lo: u64, hi: u64| -> Option<u64> {
            lookup(name)?
                .trim()
                .parse::<u64>()
                .ok()
                .map(|n| n.clamp(lo, hi))
        };
        let bytes = |name: &str, lo: usize, hi: usize, to: &mut usize| {
            if let Some(n) = num(name, lo as u64, hi as u64) {
                *to = n as usize;
            }
        };
        if let Some(n) = num("AGENTD_TRANSCRIPT_FLUSH_MS", 100, 30_000) {
            s.flush_every = Duration::from_millis(n);
        }
        bytes(
            "AGENTD_TRANSCRIPT_FLUSH_BYTES",
            1024,
            512 * 1024,
            &mut s.flush_bytes,
        );
        bytes(
            "AGENTD_TRANSCRIPT_COALESCE_BYTES",
            256,
            64 * 1024,
            &mut s.coalesce_bytes,
        );
        if let Some(n) = num("AGENTD_TRANSCRIPT_COALESCE_MS", 50, 10_000) {
            s.coalesce_every = Duration::from_millis(n);
        }
        bytes(
            "AGENTD_TRANSCRIPT_RECORD_CAP_BYTES",
            1024,
            440 * 1024,
            &mut s.record_cap,
        );
        bytes(
            "AGENTD_TRANSCRIPT_WINDOW_BYTES",
            256,
            64 * 1024,
            &mut s.window,
        );
        bytes(
            "AGENTD_TRANSCRIPT_MAX_HOLD_BYTES",
            0,
            1024 * 1024,
            &mut s.max_hold,
        );
        s.max_hold = s.max_hold.max(2 * s.window);
        bytes(
            "AGENTD_TRANSCRIPT_QUEUE_BYTES",
            1024 * 1024,
            256 * 1024 * 1024,
            &mut s.queue_bytes,
        );
        if let Some(n) = num("AGENTD_TRANSCRIPT_FINAL_MS", 100, 12_000) {
            s.final_deadline = Duration::from_millis(n);
        }
        if let Some(n) = num("AGENTD_TRANSCRIPT_UPLOAD_TRIES", 1, 20) {
            s.upload_tries = n as u32;
        }
        if let Some(n) = num("AGENTD_TRANSCRIPT_RETRY_MS", 1, 10_000) {
            s.retry_base = Duration::from_millis(n);
        }
        s
    }

    /// The settings of this process.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}
