//! Lambda entry point for the public `/v1` API (API Gateway REST, AWS_IAM auth).
//! Environment: `TABLE_NAME`, `BUCKET_NAME`, `RUN_STATE_MACHINE_ARN`, `ACCOUNT_ID`, `DEPLOYMENT_PARAMETER_PREFIX`
//! (`/hamh/<stage>`) and `OPERATOR_PRINCIPALS` (comma-separated ARN patterns allowed on `/v1/admin/*`). `HAMH_REPO_NAMES` (optional, comma-separated) is the stage's rule for the
//! CodeCommit repository names an operator may allow; without it the rule is the default `hamh-e2e-*`.

use control_api::aws::{DynamoStore, S3Blobs, SfnExecution};
use control_api::handler::{Deps, handle_logged};
use control_api::lambda::{log_lines, operators_setting, to_response, to_v1};
use control_api::repo_names::RepoNames;
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
    lambda_http::run(service_fn(move |req: Request| async move {
        let v1 = to_v1(&req);
        let line_req = v1.clone();
        let (out, diagnostics) = handle_logged(v1, deps).await;
        for line in log_lines(&line_req, out.status, &diagnostics) {
            println!("{line}");
        }
        to_response(out).map_err(Error::from)
    }))
    .await
}
