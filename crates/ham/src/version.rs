//! The two versions the TUI's tag shows on every screen, `ham vN.NN | backend vN.NN`.
//!
//! - [`CLIENT`] is this binary's own version: the root `VERSION` file, embedded at build time (`build.rs`).
//! - The backend's version is whatever the control plane said in the [`BACKEND_HEADER`] header of its last answer, or, while a stage of the
//!   earlier names may still answer, in [`EARLIER_BACKEND_HEADER`] (the first valid of the two; [`generation`] says which one). The live transport
//!   ([`crate::https::HttpsPlane::call`]) hands every signed answer's headers to [`note_backend`]; the TUI and `ham doctor` read the cell with
//!   [`backend`]. A backend that sends no header (one that predates the tag) or an invalid value reads as unknown, which the tag writes as
//!   `v?.??`; the API gateway's own answers (403, 429, 5xx) carry no header and leave the cell as it was. Until the first answer arrives the cell is unknown too.
//!
//! The cell is process-wide and holds one value, the newest valid one: every answer replaces it. Presigned object fetches are S3's
//! answers and never touch it.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The word the tag shows for the backend's version while the backends of a client with several of them report different versions.
pub const MIXED: &str = ham_core::backends::MIXED_WORD;

/// What the tag reads instead of the cell while a client holds several backends: their versions as one value, or `mixed`.
type Override = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>;
static OVERRIDE: Mutex<Option<Override>> = Mutex::new(None);

/// Installs (or, with `None`, removes) the reader of the tag's backend version for a client with several backends.
pub fn set_backend_override(reader: Option<Override>) {
    *OVERRIDE.lock().unwrap_or_else(PoisonError::into_inner) = reader;
}

/// This client's version, `N.NN`: the root `VERSION` file as `build.rs` read it.
pub const CLIENT: &str = env!("HAMH_VERSION");

/// The header the control plane puts on every answer, errors included. Names are lowercase in [`crate::http::Response::headers`].
pub const BACKEND_HEADER: &str = "x-hamh-backend-version";

/// COMPAT-CAH: the header a backend of the earlier names sends instead. It is read after [`BACKEND_HEADER`] until VERSION 5.50.
pub const EARLIER_BACKEND_HEADER: &str = "x-cah-backend-version"; // COMPAT-CAH

/// Which header the last valid answer of the control plane carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    /// [`BACKEND_HEADER`]: a stage deployed under the current names. Also what the client assumes before the first answer.
    Current,
    /// Only [`EARLIER_BACKEND_HEADER`]: a stage deployed under the earlier names, which still takes the earlier `--new-repo` names.
    Earlier,
}

/// The prefix of the `--new-repo` names a stage of this generation takes: `hamh-e2e-new-` for a stage of the current names, the earlier prefix for
/// one that answers with the earlier header only.
pub fn new_repo_prefix(generation: Generation) -> &'static str {
    match generation {
        Generation::Current => harness_protocol::target::NEW_REPO_PREFIXES[0],
        Generation::Earlier => "cah-e2e-new-", // COMPAT-CAH
    }
}

/// `^[0-9]+\.[0-9]{2}$`: the release candidate number, a dot and a two-digit counter. Anything else (a leading `v`, white space, a third
/// digit after the dot, non-ASCII digits) is not a version.
pub fn is_valid(text: &str) -> bool {
    let Some((major, counter)) = text.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && counter.len() == 2
        && counter.bytes().all(|b| b.is_ascii_digit())
}

/// Orders two valid versions by number, not by text (`9.99` is older than `10.01`). `None` when either is not a version.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    if !is_valid(a) || !is_valid(b) {
        return None;
    }
    let (ma, ca) = a.split_once('.')?;
    let (mb, cb) = b.split_once('.')?;
    let (ma, mb) = (ma.trim_start_matches('0'), mb.trim_start_matches('0'));
    Some(
        ma.len()
            .cmp(&mb.len())
            .then_with(|| ma.cmp(mb))
            .then_with(|| ca.cmp(cb)),
    )
}

/// What a cell holds: the backend's version and the generation of the header it came in.
#[derive(Debug)]
struct Seen {
    version: Option<String>,
    generation: Generation,
}

/// The backend's version as the answers of the control plane reported it. The client has one, process-wide (the free functions below); a test that
/// checks how answers are read makes its own with [`BackendCell::new`], so tests that run as threads of one process cannot overwrite each other.
#[derive(Debug)]
pub struct BackendCell {
    seen: Mutex<Seen>,
}

impl BackendCell {
    /// A cell that knows no version and is of the current generation.
    pub const fn new() -> BackendCell {
        BackendCell {
            seen: Mutex::new(Seen {
                version: None,
                generation: Generation::Current,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records what one answer of the control plane said about the backend. `headers` has lowercase names, as the transport holds them. A valid
    /// value is kept; an answer with no valid header sets the version to unknown. The generation is the one of the header present, and an answer
    /// with neither header leaves it as the last answer set it.
    pub fn note_backend(&self, headers: &BTreeMap<String, String>) {
        let read = read_headers(headers);
        let mut seen = self.lock();
        seen.version = read.version;
        if let Some(generation) = read.generation {
            seen.generation = generation;
        }
    }

    /// [`BackendCell::note_backend`] for a whole answer: the live transport's entry point. A gateway status without the header leaves the cell as it
    /// was, so one throttled poll does not turn the tag to `backend v?.??`. Any other answer without a valid header (a 2xx or another 4xx) comes
    /// from a backend that predates the tag and sets the cell to unknown. A valid header replaces the cell whatever the status.
    pub fn note_answer(&self, status: u16, headers: &BTreeMap<String, String>) {
        let has_valid_header = read_headers(headers).version.is_some();
        if !has_valid_header && is_gateway_status(status) {
            return;
        }
        self.note_backend(headers);
    }

    /// The backend's version as its last answer reported it, `None` while unknown.
    pub fn backend(&self) -> Option<String> {
        self.lock().version.clone()
    }

    /// The generation of the header the last valid answer carried; [`Generation::Current`] before any answer.
    pub fn generation(&self) -> Generation {
        self.lock().generation
    }

    /// Sets the cell directly, for tests that draw a screen or run `ham doctor` without a backend behind them. The generation goes back to the
    /// current one.
    #[doc(hidden)]
    pub fn set_for_test(&self, version: Option<&str>) {
        let mut seen = self.lock();
        seen.version = version.map(str::to_string);
        seen.generation = Generation::Current;
    }
}

impl Default for BackendCell {
    fn default() -> Self {
        BackendCell::new()
    }
}

/// The client's one cell: the transport writes it and the tag, `ham doctor` and the screens read it.
static GLOBAL: BackendCell = BackendCell::new();

/// What the two headers of one answer say: the first valid version (the current header first) and the generation of the backend, which is the
/// header present, valid or not: a backend that sends the current header is of the current generation, whatever that header holds.
struct Read {
    version: Option<String>,
    generation: Option<Generation>,
}

fn read_headers(headers: &BTreeMap<String, String>) -> Read {
    let valid = |name: &str| headers.get(name).filter(|v| is_valid(v)).cloned();
    let generation = if headers.contains_key(BACKEND_HEADER) {
        Some(Generation::Current)
    } else if headers.contains_key(EARLIER_BACKEND_HEADER) {
        Some(Generation::Earlier)
    } else {
        None
    };
    Read {
        version: valid(BACKEND_HEADER).or_else(|| valid(EARLIER_BACKEND_HEADER)),
        generation,
    }
}

/// The statuses the API gateway answers with itself: an IAM refusal (403), a throttle (429) and a gateway or Lambda failure (5xx). They never
/// reach the control-api handler, so they carry no [`BACKEND_HEADER`] whatever backend sits behind them.
fn is_gateway_status(status: u16) -> bool {
    matches!(status, 403 | 429) || status >= 500
}

/// True when an answer carries one of the two version headers, valid or not. Only the service (control-api) writes them, so their presence
/// says the answer is the service's and not the API gateway's in front of it. `headers` has lowercase names, as the transport holds them.
pub fn carries_tag(headers: &BTreeMap<String, String>) -> bool {
    headers.contains_key(BACKEND_HEADER) || headers.contains_key(EARLIER_BACKEND_HEADER)
}

/// [`BackendCell::note_backend`] on the client's cell.
pub fn note_backend(headers: &BTreeMap<String, String>) {
    GLOBAL.note_backend(headers);
}

/// [`BackendCell::note_answer`] on the client's cell: the live transport's entry point.
pub fn note_answer(status: u16, headers: &BTreeMap<String, String>) {
    GLOBAL.note_answer(status, headers);
}

/// The backend's version as its last answer reported it, `None` while unknown.
pub fn backend() -> Option<String> {
    let reader = OVERRIDE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match reader {
        Some(read) => read(),
        None => GLOBAL.backend(),
    }
}

/// The generation of the header the last valid answer carried; [`Generation::Current`] before any answer.
pub fn generation() -> Generation {
    GLOBAL.generation()
}

/// Sets the client's cell directly, for tests that draw a screen or run `ham doctor` without a backend behind them. The generation goes back to the
/// current one. A test that checks how answers are read uses its own [`BackendCell`] instead.
#[doc(hidden)]
pub fn set_backend_for_test(version: Option<&str>) {
    GLOBAL.set_for_test(version);
}
