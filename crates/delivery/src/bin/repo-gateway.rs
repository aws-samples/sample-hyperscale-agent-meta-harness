//! `repo-gateway` Lambda (container image, arm64): the only holder of git write access.
//! Environment: `BUCKET_NAME`, `TABLE_NAME`, `HAMH_STAGE`, `HAMH_OWNER`, optional `GIT_BIN`,
//! `WORK_ROOT`. The Step Functions task input names the operation (`stage`, `new-repo`, `push-pr`).

use delivery::lambda::{Config, gateway, gateway_event, load_aws, serve};
use lambda_runtime::Error;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cfg = Config::from_env().map_err(Error::from)?;
    let handler = gateway(&cfg, &load_aws().await).map_err(|e| Error::from(e.to_string()))?;
    let handler = &handler;
    serve("repo-gateway", |input| async move {
        gateway_event(handler, input).await
    })
    .await
}
