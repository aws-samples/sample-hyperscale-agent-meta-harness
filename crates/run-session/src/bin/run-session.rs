//! The `run-session` Lambda (stage `direct`): the Step Functions ops of `hamh-direct-run` and the `run-ended`
//! EventBridge target. Environment: `TABLE_NAME`, `BUCKET_NAME`, `DATA_KEY_ARN`, `WORKER_SESSION_ROLE_ARN`,
//! `RUNTIME_ARN_PARAMETER`, `INTERNAL_API_URL`, `TENANT`, `PACKAGES_DOMAIN`, `PACKAGES_OWNER`, `PACKAGES_REPOSITORY`,
//! `WORKER_MAX_LIFETIME_S`, `HAMH_STAGE` (`direct`), `RUN_STATE_MACHINE_ARN`, `AWS_REGION`. The wiring is
//! `run_session::wiring::wire` and the invocation `run_session::lambda::handle_payload`, both tested over a replay client.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let config = aws_config::load_from_env().await;
    let deps = run_session::wiring::wire(&config, |k| std::env::var(k).ok())
        .await
        .map_err(Error::from)?;
    let deps = &deps;
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| async move {
        run_session::lambda::handle_payload(deps, event.payload).await
    }))
    .await
}
