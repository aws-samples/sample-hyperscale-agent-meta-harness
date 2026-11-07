//! The `research-job` Lambda: one GitHub archive download per invocation, started asynchronously by `research-api`. It holds no key:
//! its role has no secret grant and it builds no secret source. Environment: see `research_api::lambda::job_env`.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use research_api::archive::filter_archive;
use research_api::aws::{DynamoTable, S3Blobs};
use research_api::github_source::HttpArchiveSource;
use research_api::http::{HttpClient, HttpOptions};
use research_api::jobs::{JobDeps, run_job};
use research_api::lambda::{job_env, job_log_line};
use research_api::ports::{JobEvent, SystemClock};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let env = job_env(|k| std::env::var(k).ok()).map_err(Error::from)?;
    let sdk = aws_config::load_from_env().await;
    let http = HttpClient::new(HttpOptions::default()).map_err(|e| Error::from(e.to_string()))?;
    let deps = JobDeps {
        table: DynamoTable {
            client: aws_sdk_dynamodb::Client::new(&sdk),
            table: env.table,
        },
        blobs: S3Blobs {
            client: aws_sdk_s3::Client::new(&sdk),
            bucket: env.bucket,
        },
        source: HttpArchiveSource::new(http),
        clock: SystemClock,
        filter: filter_archive,
    };
    let deps = &deps;
    lambda_runtime::run(service_fn(move |ev: LambdaEvent<JobEvent>| async move {
        let end = run_job(deps, &ev.payload).await;
        println!("{}", job_log_line(&ev.payload, &end));
        Ok::<Value, Error>(json!({"ok": true}))
    }))
    .await
}
