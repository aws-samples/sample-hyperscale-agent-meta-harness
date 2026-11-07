//! The verified bundle: the full-history bundle of `refs/heads/harness/<runId>` at the head that
//! passed FinalGates, at `store::s3::verified_bundle_key`; `bundle-validator`,
//! `repo-gateway` `push-pr` and `packager` each load it into a scratch bare repository and refuse
//! a bundle that does not verify or does not hold the expected head on the harness branch.

use crate::checks::harness_ref;
use crate::error::DeliveryError;
use crate::git::{Git, Remote};
use crate::ports::Blobs;
use harness_protocol::store::s3::verified_bundle_key;
use std::fmt;
use std::path::{Path, PathBuf};

/// Why a verified bundle cannot be delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleProblem {
    Missing {
        uri: String,
    },
    Verify {
        reason: String,
    },
    NoBranch {
        branch: String,
    },
    HeadMismatch {
        branch: String,
        found: String,
        expected: String,
    },
}

impl fmt::Display for BundleProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BundleProblem::Missing { uri } => write!(f, "no verified bundle at {uri}"),
            BundleProblem::Verify { reason } => {
                write!(f, "the verified bundle does not verify: {reason}")
            }
            BundleProblem::NoBranch { branch } => {
                write!(f, "the verified bundle has no {branch}")
            }
            BundleProblem::HeadMismatch {
                branch,
                found,
                expected,
            } => write!(
                f,
                "the verified bundle holds {found} at {branch}, not {expected}"
            ),
        }
    }
}

/// A verified bundle fetched into `repo`, with the harness branch at the expected head.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub repo: PathBuf,
    pub bundle: PathBuf,
    pub size_bytes: u64,
}

fn reason(e: DeliveryError) -> String {
    match e {
        DeliveryError::Unavailable(m)
        | DeliveryError::Invalid(m)
        | DeliveryError::Refused(m)
        | DeliveryError::AccessDenied(m) => m,
    }
}

/// Downloads, verifies and fetches the run's verified bundle into `<work>/repo.git`. The outer
/// error is transient (S3, git missing); the inner one is a property of the bundle.
pub async fn load_verified<B: Blobs>(
    git: &Git,
    blobs: &B,
    bucket: &str,
    work: &Path,
    run_id: &str,
    head_sha: &str,
) -> Result<Result<Loaded, BundleProblem>, DeliveryError> {
    let hr = harness_ref(run_id)?;
    let repo = work.join("repo.git");
    git.init_bare(&repo).await?;
    let bundle = work.join("verified.bundle");
    let key = verified_bundle_key(run_id);
    let Some(size_bytes) = blobs.get_to_file(&key, &bundle).await? else {
        return Ok(Err(BundleProblem::Missing {
            uri: format!("s3://{bucket}/{key}"),
        }));
    };
    if let Err(e) = git.bundle_verify(&repo, &bundle).await {
        return Ok(Err(BundleProblem::Verify { reason: reason(e) }));
    }
    let heads = git.bundle_heads(&repo, &bundle).await?;
    let branch = hr.as_str().to_string();
    let Some((found, _)) = heads.iter().find(|(_, r)| *r == branch) else {
        return Ok(Err(BundleProblem::NoBranch { branch }));
    };
    if found != head_sha {
        return Ok(Err(BundleProblem::HeadMismatch {
            branch,
            found: found.clone(),
            expected: head_sha.to_string(),
        }));
    }
    let refspec = format!("+{branch}:{branch}");
    git.fetch(&repo, &Remote::local(&bundle), &[refspec.as_str()])
        .await?;
    Ok(Ok(Loaded {
        repo,
        bundle,
        size_bytes,
    }))
}
