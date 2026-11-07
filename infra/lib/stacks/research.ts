// The research route: Cline researches with Tavily and downloads public repositories for context while the VM
// firewall stays as it is. The VM calls the private /internal API with its session credentials; five routes under /internal/research/ go
// to the `research-api` Lambda, which alone can read the Tavily key. It starts `research-job` (the repository download, no key)
// asynchronously. Results land under runs/<run>/agents/direct/research/ in the harness bucket, and a bucket-policy Deny (data.ts) stops
// the worker session role writing there, so Cline cannot forge a manifest, an audit object or an index.
//
// This is a construct of the control stack, not a tenth stack (a decision of the build: the nine-stack contract, the deploy order, the
// teardown stack list and the docs stay as they are). The functions run outside any VPC and reach the internet themselves; the worker
// session policy, the VPC endpoints and the Network Firewall allowlist do not change.
//
// What is here: the table (per-run policy row, counters, jobs, seen URLs), the secret (created EMPTY; scripts/research-key-put.py fills it),
// the secret's resource policy (only the research-api role reads it), two roles, two functions with their log groups, the asynchronous
// invoke settings of research-job, and the routes. Nothing here holds a key, and neither function has X-Ray tracing.
//
// A stage whose provider can call Tavily (`tavily` and `auto`; `TAVILY_CAPABLE_PROVIDERS`) also gets four CloudWatch alarms on the stored key (research-alarms.ts): its age, a refused key, a limited rate and a
// plan limit. They read metrics that research-api writes as Embedded Metric Format lines, and need no IAM statement of their own.
//
// A stage that selects the `agentcore` provider also gets an AgentCore Gateway with the Web Search Tool connector (research-gateway.ts).
// research-api then takes the gateway's URL and the search tool's name from its environment and may invoke that one gateway; it still runs outside
// the VPC and reaches the gateway's public host itself, so the endpoints, the firewall allowlist and the worker session policy stay as they are.
//
// The `auto` provider (the default of a stage) builds both backends: the gateway above and the secret below, which every provider gets. research-api
// then holds the settings and the grants of both, the Tavily key's read and the gateway's invoke and nothing wider, and chooses for every request by whether
// a key is stored.
import { Stack } from "aws-cdk-lib";
import * as apigw from "aws-cdk-lib/aws-apigateway";
import * as ddb from "aws-cdk-lib/aws-dynamodb";
import * as iam from "aws-cdk-lib/aws-iam";
import * as lambda from "aws-cdk-lib/aws-lambda";
import * as logs from "aws-cdk-lib/aws-logs";
import * as assets from "aws-cdk-lib/aws-s3-assets";
import * as secretsmanager from "aws-cdk-lib/aws-secretsmanager";
import { Construct } from "constructs";
import {
  ACCOUNT, AGENTCORE_SEARCH_TOOL, carriesGateway, carriesTavily, names, RESEARCH_STAGE_RATE_PER_MIN, type ResearchAlarmSettings, type ResearchProvider, STAGE_KIND, type StageConfig,
} from "../config.ts";
import type { DataStack } from "./data.ts";
import { ResearchAlarms } from "./research-alarms.ts";
import { ResearchGateway } from "./research-gateway.ts";
import type { SecurityStack } from "./security.ts";

/** The five routes below /internal/research/ (harness-protocol `research::route`), all POST. */
export const RESEARCH_ROUTES = ["search", "fetch", "repo", "job", "limits"] as const;

/** research-api answers within API Gateway's 29 s integration limit. */
const API_TIMEOUT_S = 28;
/** research-job: the longest a repository download may run (the Lambda maximum). */
const JOB_TIMEOUT_S = 900;
/** A job that Lambda could not start within this many seconds is dropped; research-api reads it as `not_started` after two minutes. */
const JOB_EVENT_AGE_S = 120;

export interface ResearchRouteProps {
  cfg: StageConfig;
  security: SecurityStack;
  data: DataStack;
  /** Directory holding `research-api/bootstrap` and `research-job/bootstrap` (cargo lambda build output of the one package). */
  lambdaDir: string;
  provider: ResearchProvider;
  /** The settings of the Tavily key alarms; the alarms exist only for the `tavily` provider and only when enabled. */
  alarms: ResearchAlarmSettings;
  /** The role name research-api checks the caller against (the same one worker-api checks). */
  workerSessionRoleName: string;
  api: apigw.CfnRestApi;
  /** The `/internal` resource; `research` is created below it. */
  internal: apigw.CfnResource;
}

export class ResearchRoute extends Construct {
  /** The five POST methods: the internal API's deployment depends on them. */
  readonly methods: apigw.CfnMethod[];

  constructor(scope: Construct, id: string, props: ResearchRouteProps) {
    super(scope, id);
    const { cfg, security, data, api } = props;
    const stack = Stack.of(this);
    const region = stack.region;
    const p = cfg.prefix;
    const tableName = names.researchTable(cfg);
    const tableArn = `arn:aws:dynamodb:${region}:${ACCOUNT}:table/${tableName}`;
    const researchObjects = `arn:aws:s3:::${data.bucketName}/runs/*/agents/direct/research/*`;
    const apiName = `${p}-research-api`;
    const jobName = `${p}-research-job`;
    const lambdaTrust = {
      Version: "2012-10-17",
      Statement: [{ Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }],
    };
    // The same data-key grant as worker-api and control-api: the table and the bucket are encrypted under it.
    const dataKey = {
      Sid: "DataKeyThroughDynamoDbAndS3", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"],
      Resource: security.dataKeyArn,
      Condition: { StringEquals: { "kms:ViaService": [`dynamodb.${region}.amazonaws.com`, `s3.${region}.amazonaws.com`] } },
    };

    // ---- the table: one item family per run, all with a `ttl` -----------------------------------------------------------------
    const table = new ddb.CfnTable(this, "Table", {
      tableName,
      billingMode: "PAY_PER_REQUEST",
      attributeDefinitions: [{ attributeName: "PK", attributeType: "S" }, { attributeName: "SK", attributeType: "S" }],
      keySchema: [{ attributeName: "PK", keyType: "HASH" }, { attributeName: "SK", keyType: "RANGE" }],
      timeToLiveSpecification: { attributeName: "ttl", enabled: true },
      sseSpecification: { sseEnabled: true, sseType: "KMS", kmsMasterKeyId: security.dataKeyArn },
      deletionProtectionEnabled: false,
    });

    // ---- the secret: created empty, under the default key; the owner stores the key with scripts/research-key-put.py ---------------
    const secret = new secretsmanager.CfnSecret(this, "TavilyApiKey", {
      name: names.researchSecret(cfg),
      description: `${p} Tavily API key of the research route: created empty, stored with scripts/research-key-put.py, read only by ${apiName}`,
    });

    // ----: the AgentCore Gateway of the `agentcore` provider (its service role, the gateway and the Web Search Tool target). An `auto` stage has one
    // too. The other providers synthesize exactly what they did before.
    const searchGateway = carriesGateway(props.provider) ? new ResearchGateway(this, "WebSearchGateway", { cfg }) : undefined;

    // ---- research-job: the repository download. It holds no key. -------------------------------------------------------------------
    const jobLogs = new logs.CfnLogGroup(this, "JobLogs", { logGroupName: `/aws/lambda/${jobName}`, retentionInDays: cfg.logRetentionDays });
    const jobRole = new iam.CfnRole(this, "JobRole", {
      roleName: names.role(cfg, "research-job"),
      assumeRolePolicyDocument: lambdaTrust,
      policies: [{
        policyName: "research-job",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            // The job record it was started for and the run's counters; research-api created them, so no PutItem.
            { Sid: "ResearchTable", Effect: "Allow", Action: ["dynamodb:GetItem", "dynamodb:UpdateItem"], Resource: tableArn },
            // The filtered archive, then the manifest (written last, its presence is the commit of the item). GetObject lets a retry after a
            // lost answer compare the bytes already stored with its own instead of failing (every write is create-only).
            { Sid: "ResearchObjects", Effect: "Allow", Action: ["s3:GetObject", "s3:PutObject"], Resource: researchObjects },
            dataKey,
            { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${jobLogs.attrArn}` },
          ],
        },
      }],
    });
    const jobCode = new assets.Asset(this, "JobCode", { path: `${props.lambdaDir}/research-job` });
    const job = new lambda.CfnFunction(this, "Job", {
      functionName: jobName, runtime: "provided.al2023", architectures: ["arm64"], handler: "bootstrap",
      role: jobRole.attrArn, memorySize: 2048, timeout: JOB_TIMEOUT_S,
      code: { s3Bucket: jobCode.s3BucketName, s3Key: jobCode.s3ObjectKey },
      loggingConfig: { logGroup: jobLogs.ref, logFormat: "JSON" },
      environment: { variables: { RESEARCH_TABLE_NAME: tableName, HARNESS_BUCKET: data.bucketName, HAMH_STAGE_KIND: STAGE_KIND } },
    });
    // Started asynchronously by research-api: no retry (a repeat is the job record's idempotency key, not Lambda's), no destination.
    new lambda.CfnEventInvokeConfig(this, "JobAsync", {
      functionName: job.ref, qualifier: "$LATEST", maximumRetryAttempts: 0, maximumEventAgeInSeconds: JOB_EVENT_AGE_S,
    });

    // ---- research-api: search, page fetch, job start and status, limits. The only reader of the key. -------------------------------
    const apiLogs = new logs.CfnLogGroup(this, "ApiLogs", { logGroupName: `/aws/lambda/${apiName}`, retentionInDays: cfg.logRetentionDays });
    const apiRole = new iam.CfnRole(this, "ApiRole", {
      roleName: names.role(cfg, "research-api"),
      assumeRolePolicyDocument: lambdaTrust,
      policies: [{
        policyName: "research-api",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            // Policy row, counters, idempotency records, seen URLs and job records.
            { Sid: "ResearchTable", Effect: "Allow",
              Action: ["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem", "dynamodb:BatchWriteItem", "dynamodb:BatchGetItem"],
              Resource: tableArn },
            // Items, search records and audit objects: written once each (create-only) and never deleted. GetObject is for the retry that
            // finds its own object already stored after a lost answer: it compares the bytes instead of failing.
            { Sid: "ResearchObjects", Effect: "Allow", Action: ["s3:GetObject", "s3:PutObject"], Resource: researchObjects },
            dataKey,
            // The Tavily key, by this secret's own ARN. The secret's resource policy denies every other principal.
            { Sid: "ReadTavilyKey", Effect: "Allow", Action: ["secretsmanager:GetSecretValue"], Resource: secret.ref },
            { Sid: "StartResearchJob", Effect: "Allow", Action: ["lambda:InvokeFunction"], Resource: job.attrArn },
            // The signed MCP call to this stage's web search gateway (AWS_IAM authorization), on that one gateway's ARN. The gateway's own service
            // role holds the search permission; research-api needs nothing else of bedrock-agentcore.
            ...(searchGateway ? [{ Sid: "InvokeSearchGateway", Effect: "Allow", Action: ["bedrock-agentcore:InvokeGateway"], Resource: searchGateway.gatewayArn }] : []),
            { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${apiLogs.attrArn}` },
          ],
        },
      }],
    });
    const apiCode = new assets.Asset(this, "ApiCode", { path: `${props.lambdaDir}/research-api` });
    const fn = new lambda.CfnFunction(this, "Api", {
      functionName: apiName, runtime: "provided.al2023", architectures: ["arm64"], handler: "bootstrap",
      role: apiRole.attrArn, memorySize: 512, timeout: API_TIMEOUT_S,
      code: { s3Bucket: apiCode.s3BucketName, s3Key: apiCode.s3ObjectKey },
      loggingConfig: { logGroup: apiLogs.ref, logFormat: "JSON" },
      environment: { variables: {
        RESEARCH_TABLE_NAME: tableName, HARNESS_BUCKET: data.bucketName, TAVILY_SECRET_ID: names.researchSecret(cfg),
        RESEARCH_JOB_FUNCTION: jobName, RESEARCH_PROVIDER: props.provider, HAMH_STAGE_KIND: STAGE_KIND,
        // The stage name is the value of the Stage dimension of the Tavily key metrics (crates/research-api/src/metrics.rs).
        HAMH_STAGE: cfg.stage,
        RESEARCH_STAGE_RATE_PER_MIN: String(RESEARCH_STAGE_RATE_PER_MIN),
        // The handler parses the caller from the assumed-role ARN and checks this role name, as worker-api does.
        WORKER_SESSION_ROLE_NAME: props.workerSessionRoleName,
        // Where the `agentcore` provider posts its signed MCP calls, and the name of the search tool the gateway publishes for its target.
        ...(searchGateway ? { AGENTCORE_GATEWAY_URL: searchGateway.gatewayUrl, AGENTCORE_SEARCH_TOOL: AGENTCORE_SEARCH_TOOL } : {}),
      } },
    });

    // The alarms on the stored Tavily key, for the providers that can call Tavily (`TAVILY_CAPABLE_PROVIDERS`) while they are not switched off.
    if (carriesTavily(props.provider) && props.alarms.enabled) new ResearchAlarms(this, "TavilyAlarms", { cfg, settings: props.alarms });

    // The functions name the table and the secret by name and by ARN in their environment and roles: they are created after both exist.
    job.addResourceDependency(table);
    fn.addResourceDependency(table);
    fn.addResourceDependency(secret);

    // Only the research-api role reads the key: every other principal, the account's administrators included, is denied the value.
    // DescribeSecret, PutSecretValue and TagResource stay open: the owner stores the key and checks that it is there.
    new secretsmanager.CfnResourcePolicy(this, "TavilyApiKeyPolicy", {
      secretId: secret.ref,
      blockPublicPolicy: true,
      resourcePolicy: {
        Version: "2012-10-17",
        Statement: [{
          Sid: "OnlyResearchApiReadsTheKey", Effect: "Deny", Principal: "*",
          Action: ["secretsmanager:GetSecretValue", "secretsmanager:BatchGetSecretValue"], Resource: "*",
          Condition: { StringNotEquals: { "aws:PrincipalArn": apiRole.attrArn } },
        }],
      },
    });

    // ---- the routes: /internal/research/{search,fetch,repo,job,limits} ---------------------------------------------------------------
    const research = new apigw.CfnResource(this, "Route", { restApiId: api.ref, parentId: props.internal.ref, pathPart: "research" });
    this.methods = RESEARCH_ROUTES.map((route) => {
      const res = new apigw.CfnResource(this, `Route-${route}`, { restApiId: api.ref, parentId: research.ref, pathPart: route });
      return new apigw.CfnMethod(this, `Post-${route}`, {
        restApiId: api.ref, resourceId: res.ref, httpMethod: "POST", authorizationType: "AWS_IAM",
        integration: {
          type: "AWS_PROXY", integrationHttpMethod: "POST",
          uri: `arn:aws:apigateway:${region}:lambda:path/2015-03-31/functions/${fn.attrArn}/invocations`,
        },
      });
    });
    new lambda.CfnPermission(this, "ApiInvokesResearchApi", {
      action: "lambda:InvokeFunction", functionName: fn.ref, principal: "apigateway.amazonaws.com",
      sourceArn: `arn:aws:execute-api:${region}:${ACCOUNT}:${api.ref}/*/POST/internal/research/*`,
    });
  }
}
