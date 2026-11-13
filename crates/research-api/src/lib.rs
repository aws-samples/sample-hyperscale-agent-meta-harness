//! The research route of a direct run.
//!
//! Cline researches with Tavily and downloads public repositories for context. The VM's firewall stays as it is: the VM calls the private
//! API under `/internal/research/*` with its session credentials and this crate answers, as two Lambda functions in the stage account:
//!
//! - `research-api` (28 s) serves search, page fetch, repository job start, job status and limits. It alone can read the Tavily key. It
//!   reads the run's policy row on every call, applies the per-run limits with conditional counters, scans every outbound string, calls
//!   Tavily and writes the results and one audit object under `runs/<run>/agents/direct/research/`.
//! - `research-job` (15 min, no key) downloads one GitHub archive, filters it while streaming, hashes every file, writes the filtered
//!   archive and, last, the manifest.
//!
//! A bucket-policy Deny stops the worker session role from writing under that prefix, so Cline cannot forge a manifest, an audit object or
//! an index. The wire types are `harness_protocol::research`; this crate holds the behaviour and its ports.
//!
//! Modules by owner (the lanes of the first build):
//! - the route: [`identity`], [`policy`], [`counters`], [`audit`], [`ops`], [`jobs`], [`handler`], [`aws`], [`lambda`];
//! - outbound text, URLs and the HTTP/search-provider side: [`scan`], [`http`], [`secret`], [`tavily`], [`agentcore`], [`providers`], [`github_source`],
//!   and the metrics port the Tavily client reports to: [`metrics`];
//! - the repository side: [`archive`], [`licence`];
//! - the shared interfaces: [`ports`].

pub mod agentcore;
pub mod archive;
pub mod audit;
pub mod aws;
pub mod counters;
#[cfg(feature = "testing")]
pub mod fakes;
pub mod github_source;
pub mod handler;
pub mod http;
pub mod identity;
pub mod jobs;
pub mod lambda;
pub mod licence;
pub mod metrics;
pub mod ops;
pub mod policy;
pub mod ports;
pub mod providers;
pub mod scan;
pub mod secret;
pub mod tavily;
