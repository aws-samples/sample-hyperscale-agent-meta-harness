// Control stack: the private /internal
// REST API on the worker-VPC execute-api endpoint, backed by worker-api, and the public /v1 REST
// API (AWS_IAM, this account only, never a worker session), backed by control-api.
import { createHash } from "node:crypto";
import { CfnOutput, Stack, type StackProps } from "aws-cdk-lib";
import * as apigw from "aws-cdk-lib/aws-apigateway";
import * as iam from "aws-cdk-lib/aws-iam";
import * as lambda from "aws-cdk-lib/aws-lambda";
import * as logs from "aws-cdk-lib/aws-logs";
import * as assets from "aws-cdk-lib/aws-s3-assets";
import type { Construct } from "constructs";
import {
  ACCOUNT, ENGINE_POLICY_VERSION, INFERENCE_REGIONS, names, OPERATOR_PRINCIPALS, REPO_NAMES, type ResearchAlarmSettings, type ResearchProvider, RUN_ENGINE, RUN_ENGINE_SET, type StageConfig,
} from "../config.ts";
import type { DataStack } from "./data.ts";
import type { NetworkStack } from "./network.ts";
import { RESEARCH_ROUTES, ResearchRoute } from "./research.ts";
import type { SecurityStack } from "./security.ts";

/**
 * The routes of the private /internal API. The last four are message control: agentd reports its session, claims a
 * message before it sends it, seals the run and reports the outcome. Flat names, one resource each, like the first four.
 */
export const INTERNAL_ROUTES = [
  "creds", "heartbeat", "turn-complete", "events",
  "message-session", "message-claim", "message-seal", "message-outcome",
] as const;

/**
 * A deployment's logical ID, suffixed with a hash of what it snapshots (routes and resource policy):
 * API Gateway applies a changed resource policy only when the API is deployed again, so an edit must
 * make CloudFormation create a new deployment and repoint the stage.
 */
function deploymentId(stack: Stack, base: string, snapshot: { routes: string[]; policy: unknown }): string {
  return base + createHash("sha256").update(JSON.stringify(stack.resolve(snapshot))).digest("hex").slice(0, 8);
}

/** The resource of both API policies: a resource policy governs only its own API, so the API ID
 * wildcard names every stage, method and path of this API, as the `execute-api:/*` shorthand did
 * (Access Analyzer rejects the shorthand as MISSING_ARN_FIELD). */
const thisApi = (region: string) => `arn:aws:execute-api:${region}:${ACCOUNT}:*/*`;

export interface ControlStackProps extends StackProps {
  cfg: StageConfig;
  network: NetworkStack;
  security: SecurityStack;
  data: DataStack;
  /** Directory holding `worker-api/bootstrap` (cargo lambda build output). */
  lambdaDir: string;
  /** The research route's search provider; already judged against the stage kind by `buildApp`. */
  researchProvider: ResearchProvider;
  /** The settings of the Tavily key alarms, resolved by `buildApp`. */
  researchAlarms: ResearchAlarmSettings;
}

export class ControlStack extends Stack {
  readonly internalApiId: string;
  /** Base URL agentd calls: https://<api>.execute-api.<region>.amazonaws.com/<stage> */
  readonly internalApiUrl: string;
  /** Base URL ham calls: https://<api>.execute-api.<region>.amazonaws.com/<stage> (routes under /v1). */
  readonly v1ApiUrl: string;

  constructor(scope: Construct, id: string, props: ControlStackProps) {
    super(scope, id, props);
    const { cfg, network, security, data } = props;
    const p = cfg.prefix;
    const region = this.region;
    const workerSessionRoleName = names.role(cfg, "harness-worker-session");
    // By naming convention: no cross-stack reference, and the template names the role it trusts.
    const workerSessionRoleArn = `arn:aws:iam::${ACCOUNT}:role/${workerSessionRoleName}`;

    const fnName = `${p}-worker-api`;
    const fnLogs = new logs.CfnLogGroup(this, "WorkerApiLogs", { logGroupName: `/aws/lambda/${fnName}`, retentionInDays: cfg.logRetentionDays });
    const role = new iam.CfnRole(this, "WorkerApiRole", {
      roleName: names.role(cfg, "worker-api"),
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }],
      },
      policies: [{
        policyName: "worker-api",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "HarnessTable", Effect: "Allow", Action: ["dynamodb:GetItem", "dynamodb:UpdateItem", "dynamodb:BatchWriteItem", "dynamodb:Query"],
              Resource: `arn:aws:dynamodb:${region}:${ACCOUNT}:table/${names.table(cfg)}` },
            { Sid: "DataKeyThroughDynamoDb", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"],
              Resource: security.dataKeyArn, Condition: { StringEquals: { "kms:ViaService": `dynamodb.${region}.amazonaws.com` } } },
            { Sid: "MintWorkerSessions", Effect: "Allow", Action: ["sts:AssumeRole", "sts:TagSession", "sts:SetSourceIdentity"],
              Resource: workerSessionRoleArn },
            { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${fnLogs.attrArn}` },
            { Sid: "Tracing", Effect: "Allow", Action: ["xray:PutTraceSegments", "xray:PutTelemetryRecords"], Resource: "*" },
            // Dispatched turns: run-session encrypts each task token under {run, agent, turn}.
            { Sid: "TurnTokens", Effect: "Allow", Action: ["kms:Decrypt"], Resource: security.dataKeyArn,
              Condition: { "ForAllValues:StringEquals": { "kms:EncryptionContextKeys": ["run", "agent", "turn"] } } },
            // The turn is dispatched by the one machine `run`.
            { Sid: "TurnCallbacks", Effect: "Allow", Action: ["states:SendTaskSuccess", "states:SendTaskHeartbeat"],
              Resource: ["run"].map((m) => `arn:aws:states:${region}:${ACCOUNT}:stateMachine:${names.stateMachine(cfg, m)}`) },
          ],
        },
      }],
    });

    const code = new assets.Asset(this, "WorkerApiCode", { path: `${props.lambdaDir}/worker-api` });
    const fn = new lambda.CfnFunction(this, "WorkerApi", {
      functionName: fnName, runtime: "provided.al2023", architectures: ["arm64"], handler: "bootstrap",
      role: role.attrArn, memorySize: 256, timeout: 28,
      code: { s3Bucket: code.s3BucketName, s3Key: code.s3ObjectKey },
      tracingConfig: { mode: "Active" },
      loggingConfig: { logGroup: fnLogs.ref, logFormat: "JSON" },
      environment: { variables: {
        TABLE_NAME: data.tableName, WORKER_SESSION_ROLE_ARN: workerSessionRoleArn, WORKER_SESSION_ROLE_NAME: workerSessionRoleName,
        // A turn-complete may name only the caller's own turn bundle in this bucket (sweep SW-67).
        HARNESS_BUCKET: data.bucketName,
      } },
    });

    const internalPolicy = {
      Version: "2012-10-17",
      Statement: [
        { Sid: "WorkerSessionsThroughTheWorkerEndpoint", Effect: "Allow", Principal: "*", Action: "execute-api:Invoke",
          Resource: thisApi(region),
          Condition: {
            StringEquals: { "aws:SourceVpce": network.executeApiEndpointId },
            ArnEquals: { "aws:PrincipalArn": workerSessionRoleArn },
          } },
        { Sid: "NothingFromOtherEndpoints", Effect: "Deny", Principal: "*", Action: "execute-api:Invoke", Resource: thisApi(region),
          Condition: { StringNotEquals: { "aws:SourceVpce": network.executeApiEndpointId } } },
      ],
    };
    const api = new apigw.CfnRestApi(this, "InternalApi", {
      name: `${p}-internal`,
      description: `${p} private /internal API for agentd (creds, heartbeat, turn-complete, events, message-session, message-claim, message-seal, message-outcome, research)`,
      endpointConfiguration: { types: ["PRIVATE"], vpcEndpointIds: [network.executeApiEndpointId] },
      policy: internalPolicy,
    });
    this.internalApiId = api.ref;

    const internal = new apigw.CfnResource(this, "Internal", { restApiId: api.ref, parentId: api.attrRootResourceId, pathPart: "internal" });
    const methods = INTERNAL_ROUTES.map((route) => {
      const res = new apigw.CfnResource(this, `Route-${route}`, { restApiId: api.ref, parentId: internal.ref, pathPart: route });
      return new apigw.CfnMethod(this, `Post-${route}`, {
        restApiId: api.ref, resourceId: res.ref, httpMethod: "POST", authorizationType: "AWS_IAM",
        integration: {
          type: "AWS_PROXY", integrationHttpMethod: "POST",
          uri: `arn:aws:apigateway:${region}:lambda:path/2015-03-31/functions/${fn.attrArn}/invocations`,
        },
      });
    });

    new lambda.CfnPermission(this, "ApiInvokesWorkerApi", {
      action: "lambda:InvokeFunction", functionName: fn.ref, principal: "apigateway.amazonaws.com",
      sourceArn: `arn:aws:execute-api:${region}:${ACCOUNT}:${api.ref}/*/POST/internal/*`,
    });

    // Five more routes below /internal/research/, served by research-api (not worker-api); their resources, functions, table
    // and secret are a construct of this stack (infra/lib/stacks/research.ts).
    const research = new ResearchRoute(this, "Research", {
      cfg, security, data, lambdaDir: props.lambdaDir, provider: props.researchProvider, alarms: props.researchAlarms, workerSessionRoleName, api, internal,
    });
    const deployment = new apigw.CfnDeployment(this, deploymentId(this, "InternalDeployment", {
      routes: [...INTERNAL_ROUTES.map((r) => `POST /internal/${r}`), ...RESEARCH_ROUTES.map((r) => `POST /internal/research/${r}`)],
      policy: internalPolicy,
    }), { restApiId: api.ref, description: "internal routes" });
    for (const m of [...methods, ...research.methods]) deployment.addResourceDependency(m);
    // No access or execution logs here: both need the account-wide API Gateway CloudWatch role, an
    // account setting forbids changing. worker-api writes one access line per request.
    const stage = new apigw.CfnStage(this, "InternalStage", {
      restApiId: api.ref, deploymentId: deployment.ref, stageName: cfg.stage, tracingEnabled: true,
    });
    this.internalApiUrl = `https://${api.ref}.execute-api.${region}.amazonaws.com/${stage.stageName}`;
    new CfnOutput(this, "InternalApiId", { value: api.ref, description: "private /internal REST API id" });
    new CfnOutput(this, "InternalApiUrl", { value: this.internalApiUrl, description: "base URL agentd posts /internal/* to" });

    this.v1ApiUrl = this.publicApi(props, workerSessionRoleArn);
    new CfnOutput(this, "V1ApiUrl", { value: this.v1ApiUrl, description: "base URL ham signs /v1/* requests to" });
    new CfnOutput(this, "RepoNames", { value: REPO_NAMES.join(","), description: "the repository names a run may open a pull request in (setup.sh reads this rule to preserve it during an update)" });
  }

  /** The /v1 API and control-api; returns the stage base URL. */
  private publicApi(props: ControlStackProps, workerSessionRoleArn: string): string {
    const { cfg, security } = props;
    const p = cfg.prefix;
    const region = this.region;
    const table = `arn:aws:dynamodb:${region}:${ACCOUNT}:table/${names.table(cfg)}`;
    const bucket = names.bucket(cfg);
    // By naming convention, like the worker-session role: the orchestration stack creates it.
    const runStateMachineArn = `arn:aws:states:${region}:${ACCOUNT}:stateMachine:${names.stateMachine(cfg, "run")}`;
    const healthPartitions = {
      "ForAllValues:StringLike": { "dynamodb:LeadingKeys": INFERENCE_REGIONS.map((r) => `HEALTH#${r}#*`) },
    };
    const fnName = `${p}-control-api`;
    const fnLogs = new logs.CfnLogGroup(this, "ControlApiLogs", { logGroupName: `/aws/lambda/${fnName}`, retentionInDays: cfg.logRetentionDays });
    const role = new iam.CfnRole(this, "ControlApiRole", {
      roleName: names.role(cfg, "control-api"),
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }],
      },
      policies: [{
        policyName: "control-api",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            // PutItem covers the idempotency claim and each item of the run-start transaction.
            { Sid: "HarnessTable", Effect: "Allow", Action: ["dynamodb:PutItem", "dynamodb:GetItem", "dynamodb:Query"], Resource: table },
            { Sid: "RunsByOwner", Effect: "Allow", Action: ["dynamodb:Query"], Resource: `${table}/index/GSI1` },
            // ham-admin repos allow|deny and ham doctor: the entries and their listing.
            { Sid: "RepositoryAllowlist", Effect: "Allow", Action: ["dynamodb:UpdateItem", "dynamodb:DeleteItem", "dynamodb:BatchGetItem"],
              Resource: table, Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["REPOALLOW#*"] } } },
            { Sid: "RunRequests", Effect: "Allow", Action: ["s3:PutObject"], Resource: `arn:aws:s3:::${bucket}/runs/*/input/request.json` },
            // `POST /v1/runs/{runId}/retry` starts a blocked run again with the request it was started with. That object is the one full
            // copy of the request, and the role wrote it but could not read it. The route checks that the caller owns the run before it reads
            // (IAM cannot name the caller's runs). The same key as RunRequests, a read only: no other object of the input prefix.
            { Sid: "ReadRunRequests", Effect: "Allow", Action: ["s3:GetObject"], Resource: `arn:aws:s3:::${bucket}/runs/*/input/request.json` },
            // Whether the source run delivered a bundle is asked by a listing whose prefix is the key itself. A GET of an
            // absent key is 403 for a role that may not list the bucket (404 only for one that may), so a run that delivered nothing would look
            // like an outage. The condition lets the role list the prefixes of that one key and nothing else; the read of the key stays
            // under PresignArtifacts.
            { Sid: "ListDeliveredHead", Effect: "Allow", Action: ["s3:ListBucket"], Resource: `arn:aws:s3:::${bucket}`,
              Condition: { StringLike: { "s3:prefix": ["runs/*/deliverable/verified.bundle"] } } },
            // A presigned download URL carries this role's permissions; never runs/*/hidden/*.
            { Sid: "PresignArtifacts", Effect: "Allow", Action: ["s3:GetObject"],
              Resource: [`arn:aws:s3:::${bucket}/runs/*/deliverable/*`, `arn:aws:s3:::${bucket}/runs/*/evidence/*`] },
            // The transcript route reads the stored conversation of a run its caller owns. The chunks sit in a prefix the
            // worker's engine can also write, so the grant is exactly the direct agent's first-turn transcript prefix: GetObject on its chunk
            // objects, and ListBucket only for a prefix inside it (the route lists with StartAfter; any other prefix is refused). No write,
            // no delete, no version read. The route checks ownership before it reads (IAM cannot name the caller's runs).
            { Sid: "ReadTranscriptChunks", Effect: "Allow", Action: ["s3:GetObject"],
              Resource: `arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/1/transcript/*` },
            { Sid: "ListTranscriptPrefix", Effect: "Allow", Action: ["s3:ListBucket"], Resource: `arn:aws:s3:::${bucket}`,
              Condition: { StringLike: { "s3:prefix": ["runs/*/agents/direct/turns/1/transcript/*"] } } },
            // The work in progress. POST /v1/runs/{runId}/snapshot/download-url reads the snapshot record and the first bytes
            // of the bundle and presigns the bundle, so the role may get these two objects of the direct agent and nothing else under
            // agents/ (the summary, the stop digests, the nonce, the final bundle and a future transcript stay closed to it).
            // s3:GetObjectVersion on the same two keys: the URL is pinned to the version whose header the route checked, so a bundle
            // agentd overwrites meanwhile (every ten minutes) does not break a slow download; the bucket keeps noncurrent versions 30 days.
            { Sid: "SnapshotDownload", Effect: "Allow", Action: ["s3:GetObject", "s3:GetObjectVersion"],
              // Each session of a run keeps its own snapshot under turns/n/, and the route reads the newest one. `?` is one
              // character: the turns 1 to 9, 10 to 99 and 100 to 256 need one pattern each, with no wildcard that spans a path (the test
              // `b776w_the_snapshot_download_grant_names_every_turn_number_up_to_the_session_count` reads the digit count of MAX_SLICES).
              Resource: [`arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/?/snapshot.bundle`,
                `arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/?/snapshot.json`,
                `arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/??/snapshot.bundle`,
                `arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/??/snapshot.json`,
                `arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/???/snapshot.bundle`,
                `arn:aws:s3:::${bucket}/runs/*/agents/direct/turns/???/snapshot.json`] },
            { Sid: "DataKeyThroughDynamoDbAndS3", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"],
              Resource: security.dataKeyArn,
              Condition: { StringEquals: { "kms:ViaService": [`dynamodb.${region}.amazonaws.com`, `s3.${region}.amazonaws.com`] } } },
            { Sid: "StartRun", Effect: "Allow", Action: ["states:StartExecution"], Resource: runStateMachineArn },
            // A cancel stops the run's execution (named after the run) only for `?discard=true`; there is no child execution to list or
            // stop (SfnExecution::stop_children answers an empty list).
            { Sid: "CancelRun", Effect: "Allow", Action: ["states:StopExecution"],
              Resource: ["run"].map((m) => `arn:aws:states:${region}:${ACCOUNT}:execution:${names.stateMachine(cfg, m)}:*`) },
            // The run index. The start and the backfill write the start fields of a run's search item with an UpdateItem that
            // keeps every field already set (the outcome and the end time are run-session's). The item sits under OWNER#<hash of the
            // caller>, outside the run partitions and outside GSI1 (it has no ownerPrincipalArn); reads use the table-wide GetItem and Query.
            { Sid: "RunIndex", Effect: "Allow", Action: ["dynamodb:UpdateItem"],
              Resource: table, Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["OWNER#*"] } } },
            // Run control updates only the fields it owns on run items (status, version, budget),
            // never a whole-item Put.
            { Sid: "RunControl", Effect: "Allow", Action: ["dynamodb:UpdateItem"],
              Resource: table, Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["RUN#*"] } } },
            // StartRun pins the newest ladder that verifies, and GET /v1/routes shows what it
            // would pin. The ROUTELADDER items are read with the table-wide GetItem and Query above; the
            // signature is checked with the engine-policy key.
            { Sid: "VerifyRouteLadder", Effect: "Allow", Action: ["kms:Verify"], Resource: security.signingKeyArn },
            // And D8: GET /v1/routes reads the ladder's route health with one BatchGetItem, and
            // `ham-admin route-health reset` closes a route with one conditional UpdateItem. Both carry the
            // LeadingKeys condition of the goal-step role's health grants, so neither reaches another partition.
            { Sid: "RouteHealthRead", Effect: "Allow", Action: ["dynamodb:BatchGetItem"], Resource: table, Condition: healthPartitions },
            { Sid: "RouteHealthReset", Effect: "Allow", Action: ["dynamodb:UpdateItem"], Resource: table, Condition: healthPartitions },
            // Each new Run item names the deployment it runs on.
            { Sid: "DeploymentParameters", Effect: "Allow", Action: ["ssm:GetParameters"],
              Resource: ["deployment-digest", "deployed-source-tree"].map((n) => `arn:aws:ssm:${region}:${ACCOUNT}:parameter/hamh/${cfg.stage}/${n}`) },
            // A request that names `--compute instances` is refused up front when the stage has no Instances worker runtime. control-api asks by reading that runtime's
            // ARN parameter with GetParameter (the one the runtimes stack writes with `HAMH_COMPUTE_INSTANCES=on`). The grant does not depend on the setting: an absent parameter
            // answers ParameterNotFound, a 422 the user can act on, whereas a role without the grant would answer AccessDenied for a stage that merely leaves Instances off.
            { Sid: "InstancesRuntimeArnParameter", Effect: "Allow", Action: ["ssm:GetParameter"],
              Resource: `arn:aws:ssm:${region}:${ACCOUNT}:parameter${names.runtimeArnParameter(cfg, "worker-python-instances")}` },
            { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${fnLogs.attrArn}` },
            { Sid: "Tracing", Effect: "Allow", Action: ["xray:PutTraceSegments", "xray:PutTelemetryRecords"], Resource: "*" },
          ],
        },
      }],
    });
    const code = new assets.Asset(this, "ControlApiCode", { path: `${props.lambdaDir}/control-api` });
    const fn = new lambda.CfnFunction(this, "ControlApi", {
      functionName: fnName, runtime: "provided.al2023", architectures: ["arm64"], handler: "bootstrap",
      role: role.attrArn, memorySize: 256, timeout: 28,
      code: { s3Bucket: code.s3BucketName, s3Key: code.s3ObjectKey },
      tracingConfig: { mode: "Active" },
      loggingConfig: { logGroup: fnLogs.ref, logFormat: "JSON" },
      environment: { variables: {
        TABLE_NAME: names.table(cfg), BUCKET_NAME: bucket, RUN_STATE_MACHINE_ARN: runStateMachineArn,
        // New Run items record the newest signed engine-policy version under policy routing,
        // the bootstrap version when the table holds none.
        ACCOUNT_ID: ACCOUNT, ENGINE_POLICY_VERSION: String(ENGINE_POLICY_VERSION), ENGINE_ROUTING: cfg.engineRouting,
        DEPLOYMENT_PARAMETER_PREFIX: `/hamh/${cfg.stage}`,
        // The fallback engine where no policy route covers a role and model; GET /v1/models and /v1/pools
        // list a model's rows on the engines its roles are routed to, this one only for those roles.
        // Cline is the default engine and OpenCode the second; ENGINE_SET is the engines StartRun admits, named here so no
        // default decides it: a request whose explicit models would start a session elsewhere is refused.
        ENGINE: RUN_ENGINE, ENGINE_SET: RUN_ENGINE_SET.join(","),
        // /v1/admin/* and TDD-qualification runs: the operator principals.
        OPERATOR_PRINCIPALS,
        // The CodeCommit repository names an operator may allow (the stage's rule from the account file; lib/repo-names.ts). The delivery functions get the same list.
        HAMH_REPO_NAMES: REPO_NAMES.join(","),
        // The key that verifies a route ladder (set: the stage reads and verifies ladders, so
        // GET /v1/routes works), the switch that makes StartRun pin one (off until an operator has published
        // and activated a ladder), and the namespace that counts the versions a walk skipped.
        ROUTE_LADDER_KEY: names.enginePolicyKeyAlias(cfg), ROUTE_LADDER: cfg.routeLadder,
        METRICS_NAMESPACE: names.metricsNamespace(cfg),
      } },
    });

    const v1Policy = {
      Version: "2012-10-17",
      Statement: [
        { Sid: "CallersFromThisAccount", Effect: "Allow", Principal: "*", Action: "execute-api:Invoke", Resource: thisApi(region),
          Condition: { StringEquals: { "aws:PrincipalAccount": ACCOUNT } } },
        { Sid: "NeverAWorkerSession", Effect: "Deny", Principal: "*", Action: "execute-api:Invoke", Resource: thisApi(region),
          Condition: { ArnEquals: { "aws:PrincipalArn": workerSessionRoleArn } } },
      ],
    };
    const api = new apigw.CfnRestApi(this, "V1Api", {
      name: `${p}-v1`,
      description: `${p} public /v1 API for ham (AWS_IAM)`,
      endpointConfiguration: { types: ["REGIONAL"] },
      policy: v1Policy,
    });
    const v1 = new apigw.CfnResource(this, "V1", { restApiId: api.ref, parentId: api.attrRootResourceId, pathPart: "v1" });
    const proxy = new apigw.CfnResource(this, "V1Proxy", { restApiId: api.ref, parentId: v1.ref, pathPart: "{proxy+}" });
    // One greedy route: control-api owns the route table, 404 and 405.
    const method = new apigw.CfnMethod(this, "V1Any", {
      restApiId: api.ref, resourceId: proxy.ref, httpMethod: "ANY", authorizationType: "AWS_IAM",
      integration: {
        type: "AWS_PROXY", integrationHttpMethod: "POST",
        uri: `arn:aws:apigateway:${region}:lambda:path/2015-03-31/functions/${fn.attrArn}/invocations`,
      },
    });
    new lambda.CfnPermission(this, "ApiInvokesControlApi", {
      action: "lambda:InvokeFunction", functionName: fn.ref, principal: "apigateway.amazonaws.com",
      sourceArn: `arn:aws:execute-api:${region}:${ACCOUNT}:${api.ref}/*/*/v1/*`,
    });
    const deployment = new apigw.CfnDeployment(this, deploymentId(this, "V1Deployment", { routes: ["ANY /v1/{proxy+}"], policy: v1Policy }),
      { restApiId: api.ref, description: "v1 proxy route" });
    deployment.addResourceDependency(method);
    // No access or execution logs; control-api logs each request.
    const stage = new apigw.CfnStage(this, "V1Stage", {
      restApiId: api.ref, deploymentId: deployment.ref, stageName: cfg.stage, tracingEnabled: true,
    });
    return `https://${api.ref}.execute-api.${region}.amazonaws.com/${stage.stageName}`;
  }
}
