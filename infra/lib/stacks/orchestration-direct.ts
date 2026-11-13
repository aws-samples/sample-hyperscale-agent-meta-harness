// Orchestration stack of stage `direct`: the `run-session` Lambda, the Standard state
// machine `hamh-direct-run` (statemachines/direct.asl.json, 20 states), and what clears a run whose execution ended ABORTED, FAILED or
// TIMED_OUT: the EventBridge rule `hamh-direct-run-ended` starts the small Standard machine `hamh-direct-run-ended`
// (statemachines/direct-run-ended.asl.json), which calls run-session every five minutes until it answers. An asynchronous Lambda target
// gives up after two retries in three minutes, so a run whose execution failed while the function could not start would stay `active`.
//
// run-session is the only Lambda that invokes the worker runtime and mints `harness-worker-session` credentials. It holds no CodeBuild, CodeCommit or
// Step Functions grant, and one Bedrock grant: `InvokeUtilityModel`, `bedrock:InvokeModel` on the ARNs of the utility model that
// `HAMH_UTILITY_MODEL` selects (config.ts `resolveUtilityModel`), for the run names; absent when the setting is `off`. The machine's first state
// asks run-session to name the run without waiting, so the function has an event config (one retry, ten minutes). The machine's role may invoke
// run-session and the three delivery Lambdas, which are
// addressed by name so this stack has no reference to the delivery stack. When the utility model also writes run tags,
// run-session holds one more statement, `TagVocabulary`: a Query of the owner partitions that can read the tag attributes only. The runtime ARN comes from the SSM parameter the runtimes
// stack writes, read by `start-session` when a session starts (never at function start); the policy names the exact runtime ARN and its
// endpoints (weak cross-stack reads).
import { Annotations, CfnOutput, Stack, type StackProps } from "aws-cdk-lib";
import * as events from "aws-cdk-lib/aws-events";
import * as iam from "aws-cdk-lib/aws-iam";
import * as lambda from "aws-cdk-lib/aws-lambda";
import * as logs from "aws-cdk-lib/aws-logs";
import * as assets from "aws-cdk-lib/aws-s3-assets";
import * as sfn from "aws-cdk-lib/aws-stepfunctions";
import { readFileSync } from "node:fs";
import type { Construct } from "constructs";
import { ACCOUNT, names, OWNER, type ResearchProvider, type ResolvedUtilityModel, type ResolvedUtilityPurposes, sliceDeadlineS, type StageConfig } from "../config.ts";
import type { DataStack } from "./data.ts";
import type { PackagesStack } from "./packages.ts";
import type { SecurityStack } from "./security.ts";

const DIRECT_ASL = new URL("../../../statemachines/direct.asl.json", import.meta.url);
const DIRECT_RUN_ENDED_ASL = new URL("../../../statemachines/direct-run-ended.asl.json", import.meta.url);

/** The delivery Lambdas the machine invokes, by function name (the delivery stack creates them). */
export const DELIVERY_STEPS = ["repo-gateway", "bundle-validator", "packager"] as const;

/**
 * The longest the machine runs (`TimeoutSeconds` of direct.asl.json): the longest total a run may ask for and ten
 * hours for the stage preparation and the delivery. Step Functions ends an execution after a year, and keeps 25,000 history events of it: 256 sessions at the most (`MAX_SLICES`)
 * stay under four fifths of that even when every step retries as often as it may (the test `b776w_the_history_of_the_longest_run_fits_the_machine_limit_with_room`).
 */
export const RUN_TIMEOUT_S = 2_628_000;

export interface DirectOrchestrationStackProps extends StackProps {
  cfg: StageConfig;
  security: SecurityStack;
  data: DataStack;
  /** The CodeArtifact repository whose token run-session mints for the session. */
  packages: PackagesStack;
  /** The control stack's private /internal API base URL (agentd's refresh and events target). */
  internalApiUrl: string;
  /** Directory holding `run-session/bootstrap` (cargo lambda build output). */
  lambdaDir: string;
  /**
   * The runtimes stack's exact python worker runtime ARN; absent until its image deploys (phase 1). With `HAMH_COMPUTE_INSTANCES=on` `computeInstances` is true and the
   * Instances runtime's ARN is there once its image deploys (run-session may invoke and stop sessions on the runtime and read and delete sessions of the capacity provider).
   * The provider is named by convention, with no reference to the runtimes stack: a stack that imports an export of another stack blocks every update that removes the export.
   */
  runtimes: { workers: { python?: string; instances?: string }; computeInstances?: boolean };
  /** The research route's search provider: run-session copies it into each run's policy row. */
  researchProvider: ResearchProvider;
  /** The utility model the deploy setting selected: its row id (or `off`) for the Lambda and its ARNs for the one Bedrock statement. */
  utilityModel: ResolvedUtilityModel;
  /** The purposes of the utility model the deploy setting selected: the value for `UTILITY_PURPOSES` and whether the tag branch's vocabulary read is granted. */
  utilityPurposes: ResolvedUtilityPurposes;
}

type Statement = Record<string, unknown>;

export class DirectOrchestrationStack extends Stack {
  readonly runStateMachineArn: string;
  readonly runSessionArn: string;

  constructor(scope: Construct, id: string, props: DirectOrchestrationStackProps) {
    super(scope, id, props);
    const { cfg, security } = props;
    const p = cfg.prefix;
    const region = this.region;
    const table = `arn:aws:dynamodb:${region}:${ACCOUNT}:table/${names.table(cfg)}`;
    const bucket = names.bucket(cfg);
    const bucketArn = `arn:aws:s3:::${bucket}`;
    const workerSessionRoleArn = `arn:aws:iam::${ACCOUNT}:role/${names.role(cfg, "harness-worker-session")}`;
    const workerRuntimeParameter = names.runtimeArnParameter(cfg, "worker-python");
    const parameterArn = `arn:aws:ssm:${region}:${ACCOUNT}:parameter${workerRuntimeParameter}`;
    const functionArn = (name: string) => `arn:aws:lambda:${region}:${ACCOUNT}:function:${p}-${name}`;
    // The exact worker runtime ARN and its endpoints: `live` and the `v<N>` a run pins. A `runtime/<name>-*` pattern would also match
    // every `runtime/<name>-<id>/runtime-endpoint/<name>` ARN, so the ARN is exact. Without the image there is no runtime and no grant.
    const runtime = props.runtimes.workers.python;
    const runtimeArns = runtime === undefined ? [] : [runtime, `${runtime}/runtime-endpoint/live`, `${runtime}/runtime-endpoint/v*`];
    const runtimeStatements: Statement[] = runtimeArns.length === 0 ? [] : [{
      Sid: "InvokeWorkers", Effect: "Allow",
      Action: ["bedrock-agentcore:InvokeAgentRuntime", "bedrock-agentcore:StopRuntimeSession", "bedrock-agentcore:GetAgentRuntimeEndpoint"],
      Resource: runtimeArns,
    }];

    // The Instances part, present only with `HAMH_COMPUTE_INSTANCES=on`. The runtime's statement is the microVM one on its own ARN and endpoints. A session on an Instances
    // runtime is a machine of the stage's account: StopRuntimeSession stops the agent in it and the machine only idles out later, so a run's end calls
    // DeleteCapacityProviderSession, which removes the machine, its network interface and its disks and ends the billing. Both calls name the one capacity provider exactly; nothing here
    // creates, updates, deletes or passes a provider, and no EC2 or IAM action is granted.
    const instancesRuntime = props.runtimes.workers.instances;
    const instancesParameter = names.runtimeArnParameter(cfg, "worker-python-instances");
    const providerIdParameter = names.capacityProviderIdParameter(cfg);
    const withInstances = props.runtimes.computeInstances === true;
    // The provider's ARN is `capacity-provider/<name>-<id>` and a name has no hyphen, so `<name>-*` reaches that provider and no other. It is built from the name, as the worker
    // session role's ARN is, and not taken from the runtimes stack: an import would keep that stack from dropping the export, so the setting could not go from on to off.
    const providerArn = `arn:aws:bedrock-agentcore:${region}:${ACCOUNT}:capacity-provider/${names.capacityProvider(cfg)}-*`;
    const instancesStatements: Statement[] = !withInstances ? [] : [
      { Sid: "InstancesParameters", Effect: "Allow", Action: ["ssm:GetParameter"],
        Resource: [`arn:aws:ssm:${region}:${ACCOUNT}:parameter${instancesParameter}`, `arn:aws:ssm:${region}:${ACCOUNT}:parameter${providerIdParameter}`] },
      { Sid: "EndInstancesSessions", Effect: "Allow", Action: ["bedrock-agentcore:GetCapacityProvider", "bedrock-agentcore:DeleteCapacityProviderSession"],
        Resource: [providerArn] },
      ...(instancesRuntime === undefined ? [] : [{
        Sid: "InvokeInstancesWorkers", Effect: "Allow",
        Action: ["bedrock-agentcore:InvokeAgentRuntime", "bedrock-agentcore:StopRuntimeSession", "bedrock-agentcore:GetAgentRuntimeEndpoint"],
        Resource: [instancesRuntime, `${instancesRuntime}/runtime-endpoint/live`, `${instancesRuntime}/runtime-endpoint/v*`],
      }]),
    ];

    const statements: Statement[] = [
      // The Run, Goal, Agent and Turn items and the events: Get, Put and Update (the status transactions are authorized item by item).
      { Sid: "HarnessTable", Effect: "Allow", Action: ["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem"], Resource: table },
      // `prepare` checks the stored request against the run.created event, the oldest harness event of the run (partition EVT#<run>#00); `plan-delivery` of a follow-up run
      // reads the delivery.pr event of the run it continues.
      { Sid: "RunCreatedEvent", Effect: "Allow", Action: ["dynamodb:Query"], Resource: table,
        Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["EVT#*"] } } },
      // finalize settles a run's messages from the message items, which it finds with one query of the run's own partition.
      { Sid: "RunMessages", Effect: "Allow", Action: ["dynamodb:Query"], Resource: table,
        Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["RUN#*"] } } },
      // The run's stored request (the prompt and the rules live there only).
      { Sid: "RunInputs", Effect: "Allow", Action: ["s3:GetObject"], Resource: `${bucketArn}/runs/*/input/request.json` },
      // A key that does not exist must read as absent, not as access denied: S3 answers a missing key with 404 only to a caller that
      // may list the bucket (the agent's optional summary, snapshot and bundle, and the evidence file before it is written).
      { Sid: "ListForAbsentKeys", Effect: "Allow", Action: ["s3:ListBucket"], Resource: bucketArn },
      // What agentd wrote: summary.json, the final bundle, snapshot.json and snapshot.bundle (no write: the session writes them).
      { Sid: "AgentOutputs", Effect: "Allow", Action: ["s3:GetObject"], Resource: `${bucketArn}/runs/*/agents/direct/turns/*` },
      // plan-delivery reads the version agentd recorded for each of its three end-of-run objects, not whatever is at the key (a GET or a
      // HEAD with a versionId needs s3:GetObjectVersion). These three keys only: no snapshot, no stop digest, no other version read.
      { Sid: "PinnedAgentOutputs", Effect: "Allow", Action: ["s3:GetObjectVersion"],
        Resource: ["bundle", "summary.json", "test-report.json"].map((k) => `${bucketArn}/runs/*/agents/direct/turns/*/${k}`) },
      // The delivery decision, written once; the packager zips it.
      { Sid: "Evidence", Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: `${bucketArn}/runs/*/evidence/direct.json` },
      // The service's own records of a run's slices (what each restored and how it ended), written once each. Outside agents/direct/, which the microVM writes.
      { Sid: "SliceRecords", Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: `${bucketArn}/runs/*/slices/*` },
      // The checked copy of the clean-checkout test report: plan-delivery writes it create-only and the packager zips it into evidence/.
      // One key, as narrow as the decision file's. Without it the write is refused in a deployed stage.
      { Sid: "TestReportEvidence", Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: `${bucketArn}/runs/*/evidence/test-report.json` },
      { Sid: "DataKeyThroughServices", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"],
        Resource: security.dataKeyArn,
        Condition: { StringEquals: { "kms:ViaService": [`dynamodb.${region}.amazonaws.com`, `s3.${region}.amazonaws.com`] } } },
      // The session nonce (review S1): the refresh nonce of the run's one session, sealed under the turn-shaped context and written
      // once, so a retried start-session sends the nonce the running agentd already holds (the Agent item keeps only its hash).
      // It sits outside agents/direct/turns/, which the microVM writes; the microVM's role holds no direct kms:Decrypt.
      { Sid: "SessionNonce", Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: `${bucketArn}/runs/*/agents/direct/nonce.enc` },
      // The task token and the session nonce are encrypted to their turn; worker-api decrypts the token under the same context.
      { Sid: "TurnTokens", Effect: "Allow", Action: ["kms:Encrypt"], Resource: security.dataKeyArn,
        Condition: { "ForAllValues:StringEquals": { "kms:EncryptionContextKeys": ["run", "agent", "turn"] } } },
      // This role reads its own session nonce back and nothing else: the context must carry the nonce's agent word (a StringEquals on
      // a key the request lacks is false, so a decrypt with no context is refused, which `ForAllValues` alone would let through).
      { Sid: "SessionNonceKey", Effect: "Allow", Action: ["kms:Decrypt"], Resource: security.dataKeyArn,
        Condition: { StringEquals: { "kms:EncryptionContext:agent": "direct-nonce" },
          "ForAllValues:StringEquals": { "kms:EncryptionContextKeys": ["run", "agent", "turn"] } } },
      { Sid: "MintWorkerSessions", Effect: "Allow", Action: ["sts:AssumeRole", "sts:TagSession", "sts:SetSourceIdentity"],
        Resource: workerSessionRoleArn },
      ...runtimeStatements,
      ...instancesStatements,
      { Sid: "RuntimeArn", Effect: "Allow", Action: ["ssm:GetParameter"], Resource: parameterArn },
      // `prepare` writes the run's research policy row (research on or off, the limits, the expiry) into the research table,
      // which the control stack creates (named by convention, no reference). One PutItem into the run# partition: no read, update or delete.
      // The table is under the data key, which DataKeyThroughServices already covers.
      { Sid: "ResearchPolicyRow", Effect: "Allow", Action: ["dynamodb:PutItem"],
        Resource: `arn:aws:dynamodb:${region}:${ACCOUNT}:table/${names.researchTable(cfg)}`,
        Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["run#*"] } } },
      // The run name. `name-run` calls the utility model (Converse over SigV4, signed with this role's own credentials; a minted worker session
      // cannot be used, because `harness-worker-session` denies every call from outside the worker network). One action on the ARNs of one model, from
      // the catalog row: the inference profile in this account and Region and the foundation model in each inference Region. Absent when the
      // setting is `off`. Converse needs `bedrock:InvokeModel` and no other action.
      ...(props.utilityModel.resources.length === 0 ? [] : [{
        Sid: "InvokeUtilityModel", Effect: "Allow", Action: ["bedrock:InvokeModel"], Resource: props.utilityModel.resources,
      }]),
      // The tag branch of `name-run` builds its vocabulary from the maintainers' newest search items with one Query of the maintainers' partition, so the model
      // can reuse the person's own tags. The statement lets this role Query the owner partitions and read nothing but the tag attributes (and the two key
      // attributes DynamoDB requires to be listed) of what it finds: `dynamodb:Attributes` is evaluated on the attributes the request names, and a read with no
      // projection returns every attribute, so `dynamodb:Select` must be SPECIFIC_ATTRIBUTES as well. The table ARN only, no index. The prompt head, the
      // outcome, the run name and the conversation bodies cannot be projected or filtered on through this grant. Absent when the model is `off` or the purposes
      // lack `run-tags`.
      ...(props.utilityPurposes.purposes.includes("run-tags") ? [{
        Sid: "TagVocabulary", Effect: "Allow", Action: ["dynamodb:Query"], Resource: table,
        Condition: {
          "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["OWNER#*"] },
          "ForAllValues:StringEquals": { "dynamodb:Attributes": ["PK", "SK", "runTags", "runTagsUser", "runTagsOff"] },
          StringEquals: { "dynamodb:Select": "SPECIFIC_ATTRIBUTES" },
        },
      }] : []),
      // Root invariant 2: the package token is minted here, outside the microVM, with this role; the worker session's
      // credentials (the engine's) carry no CodeArtifact grant. agentd's loopback proxy adds the token to the engine's requests.
      { Sid: "PackageTokenBearer", Effect: "Allow", Action: "sts:GetServiceBearerToken", Resource: "*",
        Condition: { StringEquals: { "sts:AWSServiceName": "codeartifact.amazonaws.com" } } },
      { Sid: "PackageToken", Effect: "Allow", Action: "codeartifact:GetAuthorizationToken",
        Resource: `arn:aws:codeartifact:${region}:${ACCOUNT}:domain/${props.packages.domainName}` },
      { Sid: "PackageRead", Effect: "Allow", Action: "codeartifact:ReadFromRepository",
        Resource: `arn:aws:codeartifact:${region}:${ACCOUNT}:repository/${props.packages.domainName}/${props.packages.repositoryName}` },
    ];

    const fnName = `${p}-run-session`;
    const fnLogs = new logs.CfnLogGroup(this, "RunSessionLogs", { logGroupName: `/aws/lambda/${fnName}`, retentionInDays: cfg.logRetentionDays });
    const role = new iam.CfnRole(this, "RunSessionRole", {
      roleName: names.role(cfg, "run-session"),
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }],
      },
      policies: [{
        policyName: "run-session",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            ...statements,
            { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${fnLogs.attrArn}` },
            { Sid: "Tracing", Effect: "Allow", Action: ["xray:PutTraceSegments", "xray:PutTelemetryRecords"], Resource: "*" },
          ],
        },
      }],
    });
    const code = new assets.Asset(this, "RunSessionCode", { path: `${props.lambdaDir}/run-session` });
    const runStateMachineName = names.stateMachine(cfg, "run");
    const runMachineArn = `arn:aws:states:${region}:${ACCOUNT}:stateMachine:${runStateMachineName}`;
    const fn = new lambda.CfnFunction(this, "RunSessionFunction", {
      functionName: fnName, runtime: "provided.al2023", architectures: ["arm64"], handler: "bootstrap",
      // A cold Instances session launches a machine first, inside the start-session call; with the option the
      // function may run the Lambda maximum, 900 s. The run machine's own states and heartbeat are unchanged.
      role: role.attrArn, memorySize: 256, timeout: withInstances ? 900 : 300,
      code: { s3Bucket: code.s3BucketName, s3Key: code.s3ObjectKey },
      tracingConfig: { mode: "Active" },
      loggingConfig: { logGroup: fnLogs.ref, logFormat: "JSON" },
      environment: { variables: {
        TABLE_NAME: names.table(cfg), BUCKET_NAME: bucket, DATA_KEY_ARN: security.dataKeyArn,
        WORKER_SESSION_ROLE_ARN: workerSessionRoleArn, RUNTIME_ARN_PARAMETER: workerRuntimeParameter,
        // Absent unless the stage has the Instances part; run-session reads these names to find the Instances runtime and the provider to end a session on.
        ...(!withInstances ? {} : {
          INSTANCES_RUNTIME_ARN_PARAMETER: instancesParameter, CAPACITY_PROVIDER_ID_PARAMETER: providerIdParameter,
        }),
        INTERNAL_API_URL: props.internalApiUrl, TENANT: OWNER,
        PACKAGES_DOMAIN: props.packages.domainName, PACKAGES_OWNER: ACCOUNT, PACKAGES_REPOSITORY: props.packages.repositoryName,
        WORKER_MAX_LIFETIME_S: String(cfg.worker.maxLifetimeS), HAMH_STAGE: cfg.stage,
        // A test knob: a shorter prompt deadline for every slice (seconds, 60 to 28,800) so a live proof crosses the session limit in minutes. Absent in production.
        ...(sliceDeadlineS() === undefined ? {} : { HAMH_SLICE_DEADLINE_S: String(sliceDeadlineS()) }),
        // How long Cline may go without output before the harness cancels the prompt (seconds, 60 to 3,600); absent leaves agentd's own 900. `HAMH_PROVIDER_SILENCE_S` of the deploy.
        ...(cfg.providerSilenceS === undefined ? {} : { HAMH_PROVIDER_SILENCE_S: String(cfg.providerSilenceS) }),
        RESEARCH_TABLE_NAME: names.researchTable(cfg), RESEARCH_PROVIDER: props.researchProvider,
        // The utility model's row id, or `off`; absent, blank or unknown reads as off in the Lambda.
        UTILITY_MODEL: props.utilityModel.value,
        // The purposes the stage runs, a comma list in catalog order (`run-name,run-tags` by default), or `off` with the model.
        UTILITY_PURPOSES: props.utilityPurposes.value,
        // Built from the names, not referenced: the machine's definition names this function.
        RUN_STATE_MACHINE_ARN: runMachineArn,
      } },
    });
    this.runSessionArn = fn.attrArn;
    if (props.utilityModel.fallback !== undefined) Annotations.of(this).addInfo(props.utilityModel.fallback);
    // The run machine's first state invokes this function asynchronously (`name-run`, InvocationType Event) and never waits. Lambda queues the
    // event and, on an error, retries it; one retry and ten minutes bound a wish whose loss leaves the name built from the prompt. The setting does not
    // change this: the machine calls `name-run` whether or not a model is set. No other caller invokes the function asynchronously.
    new lambda.CfnEventInvokeConfig(this, "RunSessionAsync", {
      functionName: fn.ref, qualifier: "$LATEST", maximumRetryAttempts: 1, maximumEventAgeInSeconds: 600,
    });

    // hamh-direct-run: control-api starts one execution per run, named after the run.
    const delivery = DELIVERY_STEPS.map(functionArn);
    const invoked = [fn.attrArn, ...delivery];
    const runRole = new iam.CfnRole(this, "RunRole", {
      roleName: names.role(cfg, "run"),
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Effect: "Allow", Principal: { Service: "states.amazonaws.com" }, Action: "sts:AssumeRole" }],
      },
      policies: [{
        policyName: "run",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "RunSteps", Effect: "Allow", Action: ["lambda:InvokeFunction"], Resource: invoked.flatMap((f) => [f, `${f}:*`]) },
            { Sid: "Tracing", Effect: "Allow", Action: ["xray:PutTraceSegments", "xray:PutTelemetryRecords", "xray:GetSamplingRules", "xray:GetSamplingTargets"], Resource: "*" },
          ],
        },
      }],
    });
    const run = new sfn.CfnStateMachine(this, "Run", {
      stateMachineName: runStateMachineName,
      stateMachineType: "STANDARD",
      roleArn: runRole.attrArn,
      definitionString: readFileSync(DIRECT_ASL, "utf8"),
      definitionSubstitutions: {
        RunSessionArn: fn.attrArn, RepoGatewayArn: delivery[0], BundleValidatorArn: delivery[1], PackagerArn: delivery[2],
      },
      tracingConfiguration: { enabled: true },
    });
    this.runStateMachineArn = run.attrArn;
    new CfnOutput(this, "RunArn", { value: run.attrArn, description: "control-api's StartExecution target, one execution per run" });
    new CfnOutput(this, "RunSessionArn", { value: fn.attrArn, description: "the run-session function (Step Functions task target and run-ended target)" });

    // run-ended: an execution that ends ABORTED (a discard), FAILED or TIMED_OUT clears the run and stops its session. The rule starts a
    // Standard machine that calls run-session until it answers (statemachines/direct-run-ended.asl.json), because the function can be
    // down when the event arrives: the rule used to invoke it asynchronously, and Lambda drops an event after two retries.
    const endedName = names.stateMachine(cfg, "run-ended");
    const endedRole = new iam.CfnRole(this, "RunEndedRole", {
      roleName: names.role(cfg, "run-ended"),
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Effect: "Allow", Principal: { Service: "states.amazonaws.com" }, Action: "sts:AssumeRole" }],
      },
      policies: [{
        policyName: "run-ended",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "RunSessionInvoke", Effect: "Allow", Action: ["lambda:InvokeFunction"], Resource: [fn.attrArn, `${fn.attrArn}:*`] },
            { Sid: "Tracing", Effect: "Allow", Action: ["xray:PutTraceSegments", "xray:PutTelemetryRecords", "xray:GetSamplingRules", "xray:GetSamplingTargets"], Resource: "*" },
          ],
        },
      }],
    });
    const endedMachine = new sfn.CfnStateMachine(this, "RunEndedMachine", {
      stateMachineName: endedName,
      stateMachineType: "STANDARD",
      roleArn: endedRole.attrArn,
      definitionString: readFileSync(DIRECT_RUN_ENDED_ASL, "utf8"),
      definitionSubstitutions: { RunSessionArn: fn.attrArn },
      tracingConfiguration: { enabled: true },
    });
    const endedEventsRole = new iam.CfnRole(this, "RunEndedEventsRole", {
      roleName: names.role(cfg, "run-ended-events"),
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Effect: "Allow", Principal: { Service: "events.amazonaws.com" }, Action: "sts:AssumeRole" }],
      },
      policies: [{
        policyName: "run-ended-events",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [{ Sid: "StartReaper", Effect: "Allow", Action: ["states:StartExecution"], Resource: endedMachine.attrArn }],
        },
      }],
    });
    new events.CfnRule(this, "RunEnded", {
      name: `${p}-run-ended`, state: "ENABLED",
      eventPattern: {
        source: ["aws.states"], "detail-type": ["Step Functions Execution Status Change"],
        detail: { stateMachineArn: [run.attrArn], status: ["ABORTED", "FAILED", "TIMED_OUT"] },
      },
      targets: [{ id: "run-ended-machine", arn: endedMachine.attrArn, roleArn: endedEventsRole.attrArn }],
    });
    new CfnOutput(this, "RunEndedArn", { value: endedMachine.attrArn, description: "the machine the run-ended rule starts: it clears a run whose execution ended early, retrying until run-session answers" });
  }
}
