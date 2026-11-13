//! Lambda entry point for the private `/internal` API (API Gateway REST, AWS_IAM auth).
//! Environment: `TABLE_NAME`, `WORKER_SESSION_ROLE_ARN`, `WORKER_SESSION_ROLE_NAME`, `HARNESS_BUCKET`.

use lambda_http::{Error, Request, service_fn};
use worker_api::aws::{DynamoStore, SfnCallbacks, StsMinter};
use worker_api::handler::{Deps, handle_logged};
use worker_api::lambda::{log_lines, to_internal, to_response};

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
        minter: StsMinter {
            client: aws_sdk_sts::Client::new(&config),
            role_arn: env("WORKER_SESSION_ROLE_ARN")?,
        },
        callbacks: SfnCallbacks {
            kms: aws_sdk_kms::Client::new(&config),
            sfn: aws_sdk_sfn::Client::new(&config),
        },
        role_name: env("WORKER_SESSION_ROLE_NAME")?,
        bucket: env("HARNESS_BUCKET").and_then(|b| {
            if b.is_empty() {
                Err(Error::from("HARNESS_BUCKET is empty"))
            } else {
                Ok(b)
            }
        })?,
    };
    let deps = &deps;
    lambda_http::run(service_fn(move |req: Request| async move {
        let internal = to_internal(&req);
        let line_req = internal.clone();
        let (out, diagnostics) = handle_logged(internal, deps).await;
        for line in log_lines(&line_req, out.status, &diagnostics) {
            println!("{line}");
        }
        to_response(out).map_err(Error::from)
    }))
    .await
}
