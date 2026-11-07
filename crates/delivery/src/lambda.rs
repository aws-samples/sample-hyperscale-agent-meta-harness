//! Lambda glue shared by the three binaries: configuration from the function environment, the
//! AWS-backed handler objects, the error kind as `errorType`, input parsing, and one JSON log line
//! per invocation that names the function, operation, run and outcome (plus the reason of a
//! refusal or failure), never the body.

use crate::aws::{CodeCommitApi, CodeCommitRemotes, DynamoTable, S3Blobs};
use crate::error::DeliveryError;
use crate::gateway::{CodeCommit, Gateway, GatewayRequest, Remotes};
use crate::git::Git;
use crate::packager::{PackageRequest, Packager};
use crate::ports::{Allowlist, Artifacts, Blobs};
use crate::repo_names::RepoNames;
use crate::validator::{Gitleaks, SecretScanner, ValidateRequest, Validator};
use aws_config::{BehaviorVersion, Region, SdkConfig};
use harness_protocol::target::E2E_REGION;
use lambda_runtime::{Diagnostic, LambdaEvent, service_fn};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::future::Future;
use std::path::PathBuf;

/// The function environment the CDK stack sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub bucket: String,
    pub table: String,
    /// `live`, `drill` or `direct`.
    pub stage: String,
    /// The stage's owner (`HAMH_OWNER`): 1 to 64 letters, digits or `+ = . @ _ -`. It is the `owner` tag of the repositories the gateway creates.
    pub owner: String,
    /// The stage's repository-name rule (`HAMH_REPO_NAMES`, optional, comma separated; default `hamh-e2e-*`).
    pub repo_names: RepoNames,
    pub git_bin: PathBuf,
    pub gitleaks_bin: PathBuf,
    /// Parent of the per-invocation scratch directories (Lambda: `/tmp`).
    pub work_root: PathBuf,
}

/// The rule of the `owner` of an account-file entry (infra/lib/owner.ts, scripts/lib/aws_target.py): a tag value, no path, no pattern, no ARN.
fn owner_ok(owner: &str) -> bool {
    (1..=64).contains(&owner.len())
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+=.@_-".contains(&b))
}

impl Config {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let need = |k: &str| get(k).ok_or_else(|| format!("missing environment variable {k}"));
        let stage = need("HAMH_STAGE")?;
        if !matches!(stage.as_str(), "live" | "drill" | "direct") {
            return Err(format!(
                "HAMH_STAGE must be live, drill or direct, got {stage:?}"
            ));
        }
        let owner = need("HAMH_OWNER")?;
        if !owner_ok(&owner) {
            return Err(format!(
                "HAMH_OWNER must be 1 to 64 letters, digits or + = . @ _ -, got {owner:?}"
            ));
        }
        let repo_names = RepoNames::from_lookup(&get)?;
        Ok(Config {
            bucket: need("BUCKET_NAME")?,
            table: need("TABLE_NAME")?,
            stage,
            owner,
            repo_names,
            git_bin: get("GIT_BIN").map_or_else(|| PathBuf::from("git"), PathBuf::from),
            gitleaks_bin: get("GITLEAKS_BIN")
                .map_or_else(|| PathBuf::from("gitleaks"), PathBuf::from),
            work_root: get("WORK_ROOT")
                // nosemgrep: temp-dir -- default work root when the work root setting is unset, the image sets /tmp/delivery, the Lambda sandbox /tmp is the only writable path and holds only the scratch data of this function
                .map_or_else(|| std::env::temp_dir().join("delivery"), PathBuf::from),
        })
    }

    pub fn from_env() -> Result<Config, String> {
        Config::from_lookup(|k| std::env::var(k).ok())
    }

    /// `HOME` for git and gitleaks, created on first use.
    pub fn home(&self) -> Result<PathBuf, DeliveryError> {
        let h = self.work_root.join("home");
        std::fs::create_dir_all(&h)?;
        Ok(h)
    }
}

pub fn diagnostic(e: &DeliveryError) -> Diagnostic {
    Diagnostic {
        error_type: e.error_type().into(),
        error_message: e.to_string(),
    }
}

pub fn parse<T: DeserializeOwned>(input: Value) -> Result<T, DeliveryError> {
    serde_json::from_value(input).map_err(|e| DeliveryError::invalid(e.to_string()))
}

pub fn invocation_line(
    function: &str,
    input: &Value,
    result: &Result<Value, DeliveryError>,
) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let mut line = json!({
        "function": function,
        "operation": field("operation"),
        "runId": field("runId"),
        "outcome": match result {
            Ok(_) => "ok",
            Err(e) => e.error_type(),
        },
    });
    if let Err(e) = result {
        line["reason"] = Value::String(e.to_string());
    }
    line.to_string()
}

/// Runs the Lambda loop: each event's payload goes to `handler`, its error becomes a Diagnostic.
pub async fn serve<F, Fut>(function: &'static str, handler: F) -> Result<(), lambda_runtime::Error>
where
    F: Fn(Value) -> Fut,
    Fut: Future<Output = Result<Value, DeliveryError>>,
{
    let handler = &handler;
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| async move {
        let input = event.payload;
        let result = handler(input.clone()).await;
        println!("{}", invocation_line(function, &input, &result));
        result.map_err(|e| diagnostic(&e))
    }))
    .await
}

/// Unix seconds now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The SDK configuration of every delivery function: the E2E Region, whatever Region the function
/// runs in, so the repository ARN checks and the git endpoint agree.
pub async fn load_aws() -> SdkConfig {
    aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(E2E_REGION))
        .load()
        .await
}

pub type AwsGateway = Gateway<CodeCommitApi, DynamoTable, S3Blobs, CodeCommitRemotes>;
pub type AwsValidator = Validator<S3Blobs, Gitleaks>;
pub type AwsPackager = Packager<S3Blobs, DynamoTable>;

fn blobs(cfg: &Config, aws: &SdkConfig) -> S3Blobs {
    S3Blobs {
        client: aws_sdk_s3::Client::new(aws),
        bucket: cfg.bucket.clone(),
    }
}

fn table(cfg: &Config, aws: &SdkConfig) -> DynamoTable {
    DynamoTable {
        client: aws_sdk_dynamodb::Client::new(aws),
        table: cfg.table.clone(),
    }
}

pub fn gateway(cfg: &Config, aws: &SdkConfig) -> Result<AwsGateway, DeliveryError> {
    let credentials = aws
        .credentials_provider()
        .ok_or_else(|| DeliveryError::unavailable("no AWS credentials provider"))?;
    Ok(Gateway {
        codecommit: CodeCommitApi {
            client: aws_sdk_codecommit::Client::new(aws),
        },
        allowlist: table(cfg, aws),
        blobs: blobs(cfg, aws),
        remotes: CodeCommitRemotes {
            credentials,
            region: E2E_REGION.into(),
            clock: now_unix,
        },
        git: Git::new(&cfg.git_bin, cfg.home()?),
        stage: cfg.stage.clone(),
        owner: cfg.owner.clone(),
        repo_names: cfg.repo_names.clone(),
        bucket: cfg.bucket.clone(),
        work_root: cfg.work_root.clone(),
        clock: now_unix,
    })
}

pub fn validator(cfg: &Config, aws: &SdkConfig) -> Result<AwsValidator, DeliveryError> {
    let home = cfg.home()?;
    Ok(Validator {
        blobs: blobs(cfg, aws),
        scanner: Gitleaks {
            bin: cfg.gitleaks_bin.clone(),
            home: home.clone(),
        },
        git: Git::new(&cfg.git_bin, home),
        bucket: cfg.bucket.clone(),
        work_root: cfg.work_root.clone(),
    })
}

pub fn packager(cfg: &Config, aws: &SdkConfig) -> Result<AwsPackager, DeliveryError> {
    Ok(Packager {
        blobs: blobs(cfg, aws),
        artifacts: table(cfg, aws),
        git: Git::new(&cfg.git_bin, cfg.home()?),
        bucket: cfg.bucket.clone(),
        work_root: cfg.work_root.clone(),
    })
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, DeliveryError> {
    serde_json::to_value(v).map_err(|e| DeliveryError::unavailable(e.to_string()))
}

/// One `repo-gateway` event: the input names the operation.
pub async fn gateway_event<C: CodeCommit, A: Allowlist, B: Blobs, R: Remotes>(
    gw: &Gateway<C, A, B, R>,
    input: Value,
) -> Result<Value, DeliveryError> {
    let req: GatewayRequest = parse(input)?;
    gw.handle(req).await
}

/// One `bundle-validator` event: a `ValidateRequest`, answered with the `ValidateOutput`.
pub async fn validator_event<B: Blobs, S: SecretScanner>(
    v: &Validator<B, S>,
    input: Value,
) -> Result<Value, DeliveryError> {
    let req: ValidateRequest = parse(input)?;
    // Boxed, as the gateway's operations: the future nests past the layout query depth.
    to_value(&Box::pin(v.validate(&req)).await?)
}

/// One `packager` event: a `PackageRequest`, answered with the `delivery.ready` payload.
pub async fn packager_event<B: Blobs, T: Artifacts>(
    p: &Packager<B, T>,
    input: Value,
) -> Result<Value, DeliveryError> {
    let req: PackageRequest = parse(input)?;
    to_value(&Box::pin(p.package(&req)).await?)
}
