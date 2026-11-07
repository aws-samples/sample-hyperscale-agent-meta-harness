//! `bundle-validator` Lambda (container image, arm64; no secrets): branch, history, path,
//! protected-path, size and gitleaks checks on the verified bundle. Environment:
//! `BUCKET_NAME`, `TABLE_NAME`, `HAMH_STAGE`, `HAMH_OWNER`, optional `GIT_BIN`, `GITLEAKS_BIN`, `WORK_ROOT`.

use delivery::lambda::{Config, load_aws, serve, validator, validator_event};
use lambda_runtime::Error;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cfg = Config::from_env().map_err(Error::from)?;
    let handler = validator(&cfg, &load_aws().await).map_err(|e| Error::from(e.to_string()))?;
    let handler = &handler;
    serve("bundle-validator", |input| async move {
        validator_event(handler, input).await
    })
    .await
}
