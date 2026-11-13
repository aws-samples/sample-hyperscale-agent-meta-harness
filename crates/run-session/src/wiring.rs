//! The wiring of the Lambda (`src/bin/run-session.rs` is only the Lambda runtime around it): the environment the CDK
//! stack sets, the AWS clients built from one `SdkConfig`, and the one place that decides whether the stage mints a
//! package token. Kept in the library so the same code runs under test with a replay HTTP client.

use crate::aws::{
    AgentCoreSessions, CodeArtifactTokens, DynamoStore, KmsTokens, RuntimeParameter, S3Blobs,
};
use crate::lambda::config_from_lookup;
use crate::name_run::Namer;
use crate::ops::Deps;
use crate::ports::PackageTokens;
use crate::utility::BedrockConverse;
use std::sync::Arc;
use worker_api::aws::StsMinter;

/// The package index of the stage: a CodeArtifact token minted with the Lambda's own role, or none (the stage has no
/// credentials provider to mint with; a stage without `PACKAGES_DOMAIN` is a [`CodeArtifactTokens`] that mints nothing).
#[derive(Clone, Debug)]
pub enum Packages {
    CodeArtifact(CodeArtifactTokens),
    Off,
}

impl PackageTokens for Packages {
    async fn token(&self, duration_s: u32) -> Result<Option<String>, String> {
        match self {
            Packages::CodeArtifact(t) => t.token(duration_s).await,
            Packages::Off => Ok(None),
        }
    }
}

/// What the Lambda runs on.
pub type LambdaDeps = Deps<DynamoStore, KmsTokens, StsMinter, AgentCoreSessions, S3Blobs, Packages>;

fn need(get: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    get(name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("missing environment variable {name}"))
}

/// Reads the function environment through `get` and builds every client from `config`. It makes no AWS call: every variable is
/// checked first, so a misconfigured function fails at start with the variable's name, and nothing else can fail it. The worker
/// runtime's ARN is not read here; `start-session` reads it through `Sessions::runtime_arn` when a session starts. Reading it at
/// start would make a stage whose runtimes stack had not written the SSM parameter yet kill the function at init (`Runtime.ExitError`),
/// and with it every op, the Blocked state and the reaper, so a failed run would stay `active`.
pub async fn wire(
    config: &aws_config::SdkConfig,
    get: impl Fn(&str) -> Option<String>,
) -> Result<LambdaDeps, String> {
    let parameter = need(&get, "RUNTIME_ARN_PARAMETER")?;
    let table = need(&get, "TABLE_NAME")?;
    let key_id = need(&get, "DATA_KEY_ARN")?;
    let role_arn = need(&get, "WORKER_SESSION_ROLE_ARN")?;
    let cfg = config_from_lookup(&get, String::new())?;
    let (_, instances) = crate::compute::from_lookup(&get)?;
    Ok(Deps {
        store: DynamoStore {
            client: aws_sdk_dynamodb::Client::new(config),
            table,
            research_table: get("RESEARCH_TABLE_NAME").filter(|v| !v.is_empty()),
        },
        tokens: KmsTokens {
            client: aws_sdk_kms::Client::new(config),
            key_id,
        },
        minter: StsMinter {
            client: aws_sdk_sts::Client::new(config),
            role_arn,
        },
        sessions: AgentCoreSessions {
            client: aws_sdk_bedrockagentcore::Client::new(config),
            control: aws_sdk_bedrockagentcorecontrol::Client::new(config),
            conflict_base_ms: dispatcher::aws::CONFLICT_BASE_MS,
            runtime_parameter: Some(RuntimeParameter {
                client: aws_sdk_ssm::Client::new(config),
                name: parameter,
            }),
            instances_parameter: instances.runtime_arn.map(|name| RuntimeParameter {
                client: aws_sdk_ssm::Client::new(config),
                name,
            }),
            capacity_provider_parameter: instances.capacity_provider.map(|name| RuntimeParameter {
                client: aws_sdk_ssm::Client::new(config),
                name,
            }),
        },
        blobs: S3Blobs {
            client: aws_sdk_s3::Client::new(config),
            bucket: cfg.bucket.clone(),
        },
        packages: CodeArtifactTokens::from_lookup(config, &get)
            .map_or(Packages::Off, Packages::CodeArtifact),
        // The helper calls use the function's own credentials; building the client makes no call.
        namer: config
            .credentials_provider()
            .map_or_else(Namer::off, |credentials| {
                Namer::new(Arc::new(BedrockConverse::new(credentials)))
            }),
        cfg,
    })
}
