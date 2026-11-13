//! `packager` Lambda (container image, arm64; no secrets): the zip, its SHA-256
//! metadata and the `ART#<artifactId>` item; returns the `delivery.ready` payload. Environment:
//! `BUCKET_NAME`, `TABLE_NAME`, `HAMH_STAGE`, `HAMH_OWNER`, optional `GIT_BIN`, `WORK_ROOT`.

use delivery::lambda::{Config, load_aws, packager, packager_event, serve};
use lambda_runtime::Error;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cfg = Config::from_env().map_err(Error::from)?;
    let handler = packager(&cfg, &load_aws().await).map_err(|e| Error::from(e.to_string()))?;
    let handler = &handler;
    serve("packager", |input| async move {
        packager_event(handler, input).await
    })
    .await
}
