//! Lambda entry point for the public `/v1` API (API Gateway REST, AWS_IAM auth).
//! Environment: `TABLE_NAME`, `BUCKET_NAME`, `RUN_STATE_MACHINE_ARN`, `ACCOUNT_ID`, `DEPLOYMENT_PARAMETER_PREFIX`
//! (`/hamh/<stage>`) and `OPERATOR_PRINCIPALS` (comma-separated ARN patterns allowed on `/v1/admin/*`). `HAMH_REPO_NAMES` (optional, comma-separated) is the stage's rule for the
//! CodeCommit repository names an operator may allow; without it the rule is the default `hamh-e2e-*`.
//! The version every answer reports is the release label of the stage when it holds one for the source tree it runs, else the
//! version compiled in; `lambda::serve` is the whole path of a request, and this file only builds what it needs.

use control_api::aws::SsmReleaseLabel;
use control_api::aws::{DynamoStore, S3Blobs, SfnExecution};
use control_api::handler::Deps;
use control_api::lambda::{operators_setting, serve};
use control_api::repo_names::RepoNames;
use control_api::version::ReleaseLabel;
use lambda_http::{Error, Request, service_fn};

fn env(name: &str) -> Result<String, Error> {
    std::env::var(name).map_err(|_| Error::from(format!("missing environment variable {name}")))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let config = aws_config::load_from_env().await;
    let deps = Deps {
        store: DynamoStore {
            client: aws_sdk_dynamodb::Client::new(&config),
            table: env("TABLE_NAME")?,
        },
        blobs: S3Blobs {
            client: aws_sdk_s3::Client::new(&config),
            bucket: env("BUCKET_NAME")?,
            credentials: config.credentials_provider(),
        },
        execution: SfnExecution {
            client: aws_sdk_sfn::Client::new(&config),
            state_machine_arn: env("RUN_STATE_MACHINE_ARN")?,
            ssm: aws_sdk_ssm::Client::new(&config),
            deployment_prefix: env("DEPLOYMENT_PARAMETER_PREFIX")?,
        },
        account: env("ACCOUNT_ID")?,
        operators: operators_setting(&env("OPERATOR_PRINCIPALS")?).map_err(Error::from)?,
        repo_names: RepoNames::from_lookup(|k| std::env::var(k).ok()).map_err(Error::from)?,
    };
    let deps = &deps;
    let release = ReleaseLabel::new(SsmReleaseLabel {
        client: aws_sdk_ssm::Client::new(&config),
        prefix: env("DEPLOYMENT_PARAMETER_PREFIX")?,
    });
    let release = &release;
    lambda_http::run(service_fn(move |req: Request| async move {
        serve(&req, deps, release).await.map_err(Error::from)
    }))
    .await
}
