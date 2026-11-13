//! The CodeCommit git-over-HTTPS credential: git signs
//! with the Lambda role's credentials in the scheme of the AWS CLI credential helper and
//! `git-remote-codecommit`, so no password, token or key is ever stored.
//!
//! Scheme (git-remote-codecommit 1.17 `sign()`): canonical request
//! `GIT\n<path>\n\nhost:<host>\n\nhost\n`, SigV4 string to sign and signature for service
//! `codecommit`, password `<yyyymmddThhmmss>Z<hex signature>`, username the access key id plus
//! `%<session token>` for temporary credentials. `tests/credential.rs` holds the botocore vector.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::fmt;

const SERVICE: &str = "codecommit";

/// AWS credentials as the SDK provides them; `Debug` shows the key id only.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl fmt::Debug for AwsKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsKeys")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// What the git credential helper answers; `Debug` shows neither field (the username carries
/// the session token).
#[derive(Clone, PartialEq, Eq)]
pub struct GitCredential {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for GitCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitCredential { <redacted> }")
    }
}

/// Civil date and time of Unix seconds (Howard Hinnant's civil-from-days).
pub(crate) fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d, rem / 3_600, rem % 3_600 / 60, rem % 60)
}

/// SigV4 basic format: `yyyymmddThhmmss` in UTC.
pub fn timestamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}")
}

/// RFC 3339 in UTC: `yyyy-mm-ddThh:mm:ssZ`.
pub fn rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub fn yyyymmdd(secs: u64) -> String {
    timestamp(secs)[..8].to_string()
}

pub fn codecommit_host(region: &str) -> String {
    format!("git-codecommit.{region}.amazonaws.com")
}

pub fn repo_path(repository: &str) -> String {
    format!("/v1/repos/{repository}")
}

/// The HTTPS remote; it never carries the credential (git asks the helper).
pub fn repo_url(region: &str, repository: &str) -> String {
    format!(
        "https://{}{}",
        codecommit_host(region),
        repo_path(repository)
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub fn canonical_request(host: &str, path: &str) -> String {
    format!("GIT\n{path}\n\nhost:{host}\n\nhost\n")
}

pub fn string_to_sign(timestamp: &str, region: &str, canonical_request: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{}/{region}/{SERVICE}/aws4_request\n{}",
        &timestamp[..8],
        hex(&Sha256::digest(canonical_request.as_bytes()))
    )
}

/// The git credential for one repository at one instant; CodeCommit accepts it for a limited
/// time, so the gateway signs right before each git call.
pub fn git_credential(keys: &AwsKeys, region: &str, repository: &str, secs: u64) -> GitCredential {
    let ts = timestamp(secs);
    let canonical = canonical_request(&codecommit_host(region), &repo_path(repository));
    let k_date = hmac(
        format!("AWS4{}", keys.secret_access_key).as_bytes(),
        &ts[..8],
    );
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, SERVICE);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex(&hmac(&k_signing, &string_to_sign(&ts, region, &canonical)));
    let username = match &keys.session_token {
        Some(token) => format!("{}%{token}", keys.access_key_id),
        None => keys.access_key_id.clone(),
    };
    GitCredential {
        username,
        password: format!("{ts}Z{signature}"),
    }
}
