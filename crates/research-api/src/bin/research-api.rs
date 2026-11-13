//! The `research-api` Lambda: search, page fetch, repository job start, job status and limits behind `/internal/research/*`.
//! The wiring of the ports lives here; the behaviour is `research_api::handler`. Environment: see `research_api::lambda::api_env`.

use lambda_http::{Error, Request, service_fn};
use research_api::agentcore::GatewayTarget;
use research_api::aws::{DynamoTable, LambdaInvoker, S3Blobs};
use research_api::handler::{Config, Deps, handle};
use research_api::http::{HttpClient, HttpOptions};
use research_api::lambda::{api_env, log_line, to_request, to_response};
use research_api::metrics::EmfMetrics;
use research_api::ports::SystemClock;
use research_api::providers::{AgentcoreDeps, ProviderDeps, TAVILY_BASE_URL, build_stage};
use research_api::secret::{AwsSecretSource, CachedSecret};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let env = api_env(|k| std::env::var(k).ok()).map_err(Error::from)?;
    let sdk = aws_config::load_from_env().await;
    // The function times out at 28 s (API Gateway allows 29): the upstream gives up first and the caller gets an answer.
    let http = HttpClient::new(HttpOptions {
        total_timeout: Duration::from_secs(22),
        ..HttpOptions::default()
    })
    .map_err(|e| Error::from(e.to_string()))?;
    let secret = CachedSecret::new(AwsSecretSource::new(
        aws_sdk_secretsmanager::Client::new(&sdk),
        env.secret_id.clone(),
    ));
    // The gateway of the `agentcore` provider: the address was checked by `api_env`; the credentials are the function's own role.
    let agentcore = match &env.agentcore {
        Some(gateway) => Some(AgentcoreDeps {
            target: GatewayTarget::parse(&gateway.gateway_url)
                .map_err(|e| Error::from(e.to_string()))?,
            tool: gateway.tool.clone(),
            credentials: sdk.credentials_provider().ok_or_else(|| {
                Error::from("the function has no AWS credentials provider".to_string())
            })?,
        }),
        None => None,
    };
    // The Tavily key's health goes to the Lambda's log as Embedded Metric Format lines (crates/research-api/src/metrics.rs); only the keyed Tavily
    // provider reports, also as the keyed half of an `auto` stage (nothing is written while AgentCore serves).
    let metrics =
        EmfMetrics::new(&env.stage, SystemClock, std::io::stdout()).map_err(Error::from)?;
    let provider = build_stage(
        env.choice(),
        env.stage_kind,
        ProviderDeps {
            http: Some(http),
            secret: Some(secret),
            tavily_base_url: TAVILY_BASE_URL.to_string(),
            agentcore,
        },
    )
    .map_err(Error::from)?
    .with_metrics(Arc::new(metrics));
    let deps = Deps {
        table: DynamoTable {
            client: aws_sdk_dynamodb::Client::new(&sdk),
            table: env.table,
        },
        blobs: S3Blobs {
            client: aws_sdk_s3::Client::new(&sdk),
            bucket: env.bucket,
        },
        provider,
        invoker: LambdaInvoker {
            client: aws_sdk_lambda::Client::new(&sdk),
            function: env.job_function,
        },
        clock: SystemClock,
        cfg: Config {
            worker_role_name: env.worker_role_name,
            stage_rate_per_min: env.stage_rate_per_min,
            stage_kind: env.stage_kind,
        },
    };
    let deps = &deps;
    lambda_http::run(service_fn(move |req: Request| async move {
        let resp = handle(deps, &to_request(&req)).await;
        println!("{}", log_line(&resp.log));
        to_response(resp).map_err(Error::from)
    }))
    .await
}
