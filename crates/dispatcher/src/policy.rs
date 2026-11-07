//! The inline STS session policy of a worker session.
//!
//! The effective permissions are the intersection of `harness-worker-session`'s identity policy
//! and this policy. `RoleGrants` lets the role's non-Bedrock grants through unchanged (their
//! resources stay scoped by the role); `LeasedModel` narrows Bedrock to the leased model's
//! inference profile and foundation models; `LeasedMantle` appears only for `aws-mantle/` models.

use crate::stage::{ACCOUNT, INFERENCE_REGIONS, REGION};
use serde_json::json;

/// STS rejects an inline session policy longer than 2,048 characters.
pub const MAX_SESSION_POLICY_CHARS: usize = 2048;

const MANTLE_PREFIX: &str = "aws-mantle/";

/// The Region segment of the foundation-model ARNs of a run with failover routes: every Region whose name starts with `us-`. It does not
/// match the empty Region of the Region-less ARN (see [`bedrock_resources_for_routes`]).
pub const WILDCARD_REGION: &str = "us-*";

/// Prefixes of cross-Region inference profile ids.
const GEO_PREFIXES: [&str; 8] = ["us", "us-gov", "eu", "apac", "jp", "au", "ca", "global"];

/// The role's grants besides Bedrock, passed through on `*`. No CodeArtifact or STS action: the
/// engine uses these credentials, so the dispatcher mints the package token with its own role.
const ROLE_GRANTS: [&str; 7] = [
    "s3:GetObject",
    "s3:PutObject",
    "s3:AbortMultipartUpload",
    "s3:ListBucket",
    "kms:Decrypt",
    "kms:GenerateDataKey",
    "execute-api:Invoke",
];

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "model id {0:?} is not a catalog model id ([A-Za-z0-9._:-], optional aws-mantle/) or a home inference-profile or foundation-model ARN"
    )]
    BadModel(String),
    #[error("model Region {0:?} is not an inference Region")]
    BadRegion(String),
    #[error("session policy is {0} characters; STS allows {MAX_SESSION_POLICY_CHARS}")]
    TooLong(usize),
}

fn model_chars_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// The model an ARN names when it names only what its id already leases: a
/// system inference profile in the home account and Region, or a foundation model in an inference
/// Region. Every other ARN (another account or Region, application profiles, provisioned models)
/// is `None`, so the policy never widens.
fn model_of_arn(arn: &str) -> Option<&str> {
    let rest = arn.strip_prefix("arn:aws:bedrock:")?;
    let (region, rest) = rest.split_once(':')?;
    let (account, resource) = rest.split_once(':')?;
    if let Some(id) = resource.strip_prefix("inference-profile/") {
        let geo = id
            .split_once('.')
            .is_some_and(|(p, _)| GEO_PREFIXES.contains(&p));
        return (region == REGION && account == ACCOUNT && geo).then_some(id);
    }
    let id = resource.strip_prefix("foundation-model/")?;
    (INFERENCE_REGIONS.contains(&region) && account.is_empty()).then_some(id)
}

/// The base model id without `aws-mantle/` (or the id an allowed ARN names), refusing anything
/// that could widen the policy.
fn base_model(model_id: &str) -> Result<&str, PolicyError> {
    let bad = || PolicyError::BadModel(model_id.to_string());
    let id = if model_id.starts_with("arn:") {
        model_of_arn(model_id).ok_or_else(bad)?
    } else {
        model_id.strip_prefix(MANTLE_PREFIX).unwrap_or(model_id)
    };
    if id.starts_with("arn:") || !model_chars_ok(id) {
        return Err(bad());
    }
    Ok(id)
}

/// Bedrock resources the leased model is invoked through: a geo profile's ARN in the home Region
/// plus its foundation model in every inference Region (and Region-less for `global.`).
pub fn bedrock_resources(model_id: &str) -> Result<Vec<String>, PolicyError> {
    bedrock_resources_in(model_id, REGION)
}

/// [`bedrock_resources`] for a model called in `region` (one of the inference Regions): the geo
/// profile ARN is that Region's.
pub fn bedrock_resources_in(model_id: &str, region: &str) -> Result<Vec<String>, PolicyError> {
    if !INFERENCE_REGIONS.contains(&region) {
        return Err(PolicyError::BadRegion(region.to_string()));
    }
    let id = base_model(model_id)?;
    let (geo, base) = match id.split_once('.') {
        Some((p, rest)) if GEO_PREFIXES.contains(&p) => (Some(p), rest),
        _ => (None, id),
    };
    if base.is_empty() {
        return Err(PolicyError::BadModel(model_id.to_string()));
    }
    let mut out = Vec::with_capacity(5);
    if geo.is_some() {
        out.push(format!(
            "arn:aws:bedrock:{region}:{ACCOUNT}:inference-profile/{id}"
        ));
    }
    for r in INFERENCE_REGIONS {
        out.push(format!("arn:aws:bedrock:{r}::foundation-model/{base}"));
    }
    if geo == Some("global") {
        out.push(format!("arn:aws:bedrock:::foundation-model/{base}"));
    }
    Ok(out)
}

/// Bedrock resources a run with failover routes is invoked through: for each route (a model id and the Region it is called in) the
/// profile's ARN in the home account, and for each distinct base model one foundation-model ARN whose Region segment is the wildcard
/// [`WILDCARD_REGION`] (`us-*`), plus the Region-less ARN for a base that a `global.` profile in the lease serves. The wildcard is what
/// makes nine routes fit the 2,048-character limit (all-literal Regions need about 2,370 characters; this form 1,675 with the real session
/// tags packing to 36 percent, measured against STS).
///
/// It adds no model, action, account or resource type: effective permissions are the intersection of this policy and
/// `harness-worker-session`'s identity policy, whose Bedrock invoke ceiling (`infra/lib/stacks/security.ts` `bedrockResources`, pinned
/// by `infra/test/security.test.ts`) names the foundation models, inference profiles and application inference profiles of
/// [`INFERENCE_REGIONS`] and the Region-less `arn:aws:bedrock:::foundation-model/*`, which a `global.` profile call is authorised on.
/// The ceiling still bounds the wildcard to the three inference Regions, and the safety of the form depends on the ceiling staying
/// literal; `us-*` adds a second bound of its own, the US Regions, should the ceiling ever widen.
///
/// Why `us-*` and not `*`: a bare `*` also matched the Region-less ARN `arn:aws:bedrock:::foundation-model/<base>` of a base with
/// no `global.` profile (today `zai.glm-5.3`), which the single-route builder ([`bedrock_resources_in`]) never granted. `us-*` needs `us-`
/// after `bedrock:`, so it matches no Region-less ARN; the Region-less ARN is written out only for the bases a `global.` route serves.
/// An id must be a catalog id: an ARN, a wildcard or an odd character is refused, like in [`bedrock_resources_in`].
pub fn bedrock_resources_for_routes(routes: &[(&str, &str)]) -> Result<Vec<String>, PolicyError> {
    if routes.is_empty() {
        return Err(PolicyError::BadModel(String::new()));
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |r: String| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    let mut foundation: Vec<String> = Vec::new();
    for (model_id, region) in routes {
        if !INFERENCE_REGIONS.contains(region) {
            return Err(PolicyError::BadRegion((*region).to_string()));
        }
        if model_id.starts_with("arn:") {
            return Err(PolicyError::BadModel((*model_id).to_string()));
        }
        let id = base_model(model_id)?;
        let (geo, base) = match id.split_once('.') {
            Some((p, rest)) if GEO_PREFIXES.contains(&p) => (Some(p), rest),
            _ => (None, id),
        };
        if base.is_empty() {
            return Err(PolicyError::BadModel((*model_id).to_string()));
        }
        if geo.is_some() {
            push(format!(
                "arn:aws:bedrock:{region}:{ACCOUNT}:inference-profile/{id}"
            ));
        }
        let wildcard = format!("arn:aws:bedrock:{WILDCARD_REGION}::foundation-model/{base}");
        if !foundation.contains(&wildcard) {
            foundation.push(wildcard);
        }
        if geo == Some("global") {
            let global = format!("arn:aws:bedrock:::foundation-model/{base}");
            if !foundation.contains(&global) {
                foundation.push(global);
            }
        }
    }
    for f in foundation {
        push(f);
    }
    Ok(out)
}

/// The inline session policy of a run that may switch routes: [`session_policy_in`] over every route. One route gives the same
/// text as [`session_policy_in`] (the run has no failover); several give the compressed form of [`bedrock_resources_for_routes`].
pub fn session_policy_for_routes(routes: &[(&str, &str)]) -> Result<String, PolicyError> {
    if let [(model_id, region)] = routes {
        if model_id.starts_with("arn:") {
            return Err(PolicyError::BadModel((*model_id).to_string()));
        }
        return session_policy_in(model_id, region);
    }
    let resources = bedrock_resources_for_routes(routes)?;
    let mut statements = vec![
        json!({ "Sid": "RoleGrants", "Effect": "Allow", "Action": ROLE_GRANTS, "Resource": "*" }),
        json!({
            "Sid": "LeasedModel", "Effect": "Allow",
            "Action": ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream"],
            "Resource": resources,
        }),
    ];
    if routes.iter().any(|(id, _)| id.starts_with(MANTLE_PREFIX)) {
        statements.push(json!({
            "Sid": "LeasedMantle", "Effect": "Allow", "Action": "bedrock-mantle:CreateInference", "Resource": "*",
        }));
    }
    let text = json!({ "Version": "2012-10-17", "Statement": statements }).to_string();
    let n = text.chars().count();
    if n > MAX_SESSION_POLICY_CHARS {
        return Err(PolicyError::TooLong(n));
    }
    Ok(text)
}

/// Minified policy JSON for the leased model, at most [`MAX_SESSION_POLICY_CHARS`] characters.
pub fn session_policy(model_id: &str) -> Result<String, PolicyError> {
    session_policy_in(model_id, REGION)
}

/// [`session_policy`] for a model called in `region` (one of the inference Regions).
pub fn session_policy_in(model_id: &str, region: &str) -> Result<String, PolicyError> {
    let mut statements = vec![
        json!({ "Sid": "RoleGrants", "Effect": "Allow", "Action": ROLE_GRANTS, "Resource": "*" }),
        json!({
            "Sid": "LeasedModel", "Effect": "Allow",
            "Action": ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream"],
            "Resource": bedrock_resources_in(model_id, region)?,
        }),
    ];
    if model_id.starts_with(MANTLE_PREFIX) {
        statements.push(json!({
            "Sid": "LeasedMantle", "Effect": "Allow", "Action": "bedrock-mantle:CreateInference", "Resource": "*",
        }));
    }
    let text = json!({ "Version": "2012-10-17", "Statement": statements }).to_string();
    let n = text.chars().count();
    if n > MAX_SESSION_POLICY_CHARS {
        return Err(PolicyError::TooLong(n));
    }
    Ok(text)
}
