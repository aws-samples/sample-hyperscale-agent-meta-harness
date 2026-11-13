//! The backend's version. `build.rs` embeds
//! the root `VERSION` file (`N.NN`) as `HAMH_VERSION`; `lambda::to_response` returns it in `VERSION_HEADER` on every answer, so `ham` shows
//! `backend vN.NN` next to its own version.

/// The backend's version: the content of the root `VERSION` file when this crate was built.
pub const VERSION: &str = env!("HAMH_VERSION");

/// The response header that carries `VERSION` on every answer of the `/v1` API, whatever the status.
pub const VERSION_HEADER: &str = "x-hamh-backend-version";

/// The earlier name of [`VERSION_HEADER`].
pub const LEGACY_VERSION_HEADER: &str = "x-cah-backend-version"; // COMPAT-CAH
