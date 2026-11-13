//! Delivery: the
//! three Lambdas that turn a verified head into a pull request and a zip.
//!
//! - `repo-gateway`, the only holder of git write access: `stage`, `new-repo`, `push-pr`.
//! - `bundle-validator` (no secrets): branch, path, workflow, buildspec, engine-config, size,
//!   gitleaks and third-party-copy checks over `base..head`, and the record of what a passing delivery changed (`changes`).
//! - `packager` (no secrets): the zip with `MANIFEST.sha256` and SHA-256 metadata.
//!
//! Handlers are independent of the Lambda event types; CodeCommit, S3 and DynamoDB sit behind
//! traits, and `git` and `gitleaks` run as child processes.

pub mod aws;
pub mod bundle;
pub mod changes;
pub mod checks;
pub mod credential;
pub mod error;
pub mod files;
pub mod follow_up;
pub mod gateway;
pub mod git;
pub mod glob;
pub mod lambda;
pub mod packager;
pub mod ports;
pub mod repo_names;
pub mod third_party;
pub mod validator;
