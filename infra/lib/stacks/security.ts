// Policies name the bucket and roles of later stacks by convention (config `names`), so this
// stack has no reference back to them; trust uses aws:PrincipalArn because IAM rejects role
// principals that do not exist yet.
import { Fn, Stack, type StackProps } from "aws-cdk-lib";
import * as iam from "aws-cdk-lib/aws-iam";
import * as kms from "aws-cdk-lib/aws-kms";
import type { Construct } from "constructs";
import { ACCOUNT, INFERENCE_REGIONS, names, OPERATOR, type StageConfig } from "../config.ts";
import { principalArnPatterns, runnerRoleArn } from "../operator.ts";
import type { NetworkStack } from "./network.ts";

/** Session tags the dispatcher sets on every worker session. */
export const SESSION_TAG_KEYS = ["run", "agent", "tenant", "mode", "engine"] as const;

/** What the base execution role (the one MMDS exposes) may never do. */
export const BASE_ROLE_DENIES = [
  "sts:AssumeRole", "sts:TagSession", "sts:SetSourceIdentity", "sts:GetFederationToken",
  "bedrock:*", "bedrock-mantle:*", "s3:*", "dynamodb:*", "appsync:*", "secretsmanager:*", "ssm:GetParameter*",
  "kms:Decrypt", "lambda:*", "iam:*",
  "bedrock-agentcore:GetWorkloadAccessToken", "bedrock-agentcore:GetWorkloadAccessTokenForJWT",
  "bedrock-agentcore:GetWorkloadAccessTokenForUserId", "bedrock-agentcore:GetResourceOauth2Token",
  "bedrock-agentcore:GetResourceApiKey", "bedrock-agentcore:InvokeAgentRuntime",
  "bedrock-agentcore:InvokeAgentRuntimeCommand", "bedrock-agentcore:StopRuntimeSession",
  "bedrock-agentcore:InvokeGateway",
  // C17: no microVM identity reaches the E2E git host or starts its PR check.
  "codecommit:*", "codebuild:StartBuild",
];

/** Every action the runtime base role allows (infra/test/security.test.ts pins it). */
export const RUNTIME_BASE_ALLOWS = [
  "cloudwatch:PutMetricData", "ecr:BatchGetImage", "ecr:GetAuthorizationToken", "ecr:GetDownloadUrlForLayer",
  "logs:CreateLogGroup", "logs:CreateLogStream", "logs:DescribeLogStreams", "logs:PutLogEvents",
] as const;

export interface SecurityStackProps extends StackProps {
  cfg: StageConfig;
  network: NetworkStack;
  /** Create the operator role of the Instances capacity provider (`HAMH_COMPUTE_INSTANCES=on`). Default false: nothing is added to the stack. */
  computeInstances?: boolean;
}

/**
 * The AWS-managed policy of the Instances capacity provider's operator role. Its instance-role sibling is not used: the provider has no instance profile.
 */
export const INSTANCES_OPERATOR_POLICY_ARN = "arn:aws:iam::aws:policy/BedrockAgentCoreRuntimeInstancesOperatorRolePolicy";

/**
 * The principal `scripts/deploy.sh` runs as when the accounts.json entry names no `operator`: the IAM role `Admin` (the Testacct Admin role); the E2E second principal and the
 * Go module store's filler role trust only it. An entry that names an `operator` replaces it by that identity in `deployingTrust`,
 * `deployingPrincipalArns` and the minters below.
 */
export const DEPLOYING_PRINCIPAL = `arn:aws:iam::${ACCOUNT}:role/Admin`;

/**
 * The `Principal` (and `Condition`) of a trust statement that lets the deploying principal assume a role. Without an `operator` it is the role `Admin` as the principal,
 * as it always was. With one it is the account root narrowed by `aws:PrincipalArn` to that identity (the shape of the minters of the worker-session role): IAM rejects a
 * role principal that does not exist, and the role of a stranger's account is whatever it is, so the trust names the identity by pattern and not by a principal that must
 * exist; the identity still needs `sts:AssumeRole` in its own policy, which an administrator has. A role has a path (an IAM Identity Center role lives under
 * /aws-reserved/sso.amazonaws.com/) that the session ARN does not carry, so the patterns cover the role without and with any path. `runner` adds the setup runner's role
 * (the Go module store: the runner fills it and removes it), which an `operator` stage names there and in no other place.
 */
export function deployingTrust(opts: { runner?: boolean } = {}): { Principal: { AWS: string }; Condition?: { ArnLike: { "aws:PrincipalArn": string[] } } } {
  if (!OPERATOR) return { Principal: { AWS: DEPLOYING_PRINCIPAL } };
  return { Principal: { AWS: `arn:aws:iam::${ACCOUNT}:root` }, Condition: { ArnLike: { "aws:PrincipalArn": deployingPrincipalArns(opts) } } };
}

/** The `aws:PrincipalArn` values of the deploying principal: the role `Admin`, or the entry's `operator` (and the setup runner's role with `runner`). */
export function deployingPrincipalArns(opts: { runner?: boolean } = {}): string[] {
  if (!OPERATOR) return [DEPLOYING_PRINCIPAL];
  return [...principalArnPatterns(ACCOUNT, OPERATOR), ...(opts.runner ? [runnerRoleArn(ACCOUNT)] : [])];
}

// Session trust conditions without wildcards: Access Analyzer rejects `?*` on these keys
// (WILDCARD_USAGE_TOO_PERMISSIVE). Null false plus StringNotEquals "" requires the same thing, the key
// present and non-empty (a session tag value may be empty; STS refuses a source identity under 2 characters).
const RUN_AND_AGENT_TAGS = {
  Null: { "aws:RequestTag/run": "false", "aws:RequestTag/agent": "false" },
  StringNotEquals: { "aws:RequestTag/run": "", "aws:RequestTag/agent": "" },
};
const A_SOURCE_IDENTITY = { Null: { "sts:SourceIdentity": "false" }, StringNotEquals: { "sts:SourceIdentity": "" } };

const rootKeyPolicy = {
  Version: "2012-10-17",
  Statement: [{
    Sid: "AccountAdministersThroughIam", Effect: "Allow",
    Principal: { AWS: `arn:aws:iam::${ACCOUNT}:root` }, Action: "kms:*", Resource: "*",
  }],
};

export class SecurityStack extends Stack {
  readonly dataKeyArn: string;
  readonly signingKeyArn: string;
  readonly runtimeBaseRoleArn: string;
  readonly workerSessionRoleArn: string;

  constructor(scope: Construct, id: string, props: SecurityStackProps) {
    super(scope, id, props);
    const { cfg, network } = props;
    const p = cfg.prefix;
    const region = this.region;

    const dataKey = new kms.CfnKey(this, "DataKey", {
      description: `${p} data at rest: harness table, bucket, logs`, enableKeyRotation: true, keyPolicy: rootKeyPolicy,
    });
    new kms.CfnAlias(this, "DataKeyAlias", { aliasName: `alias/${p}-data`, targetKeyId: dataKey.ref });
    this.dataKeyArn = dataKey.attrArn;

    const signingKey = new kms.CfnKey(this, "EnginePolicyKey", {
      // nosemgrep: html-in-template-string -- the template literal is the description text of a KMS key, with a placeholder for the policy version, it is not HTML
      description: `${p} engine policy signing (ENGINEPOLICY#v<n>)`, keySpec: "ECC_NIST_P256", keyUsage: "SIGN_VERIFY",
      keyPolicy: rootKeyPolicy,
    });
    // cfn_nag F19 (rotation) cannot apply: the key is asymmetric (ECC_NIST_P256, SIGN_VERIFY) and AWS KMS rotates only symmetric encryption keys.
    signingKey.addMetadata("cfn_nag", { rules_to_suppress: [{
      id: "F19",
      reason: "Asymmetric signing key (ECC_NIST_P256, SIGN_VERIFY): AWS KMS supports automatic rotation only for symmetric encryption keys, so EnableKeyRotation does not apply to this key. The symmetric DataKey of this stack has EnableKeyRotation true.",
    }] });
    new kms.CfnAlias(this, "EnginePolicyKeyAlias", { aliasName: `alias/${p}-engine-policy`, targetKeyId: signingKey.ref });
    this.signingKeyArn = signingKey.attrArn;

    // Base execution role for AgentCore runtimes: exposed through MMDS, so it holds nothing an agent can abuse. The microVM grants its
    // processes no CAP_NET_ADMIN, so the metadata service cannot be blocked by uid: the engine can read
    // this role's credentials, and the role is the control. It allows exactly the actions of RUNTIME_BASE_ALLOWS, on the scopes below:
    // log writes to this stage's own runtime log groups (runtime names start with the stage prefix in underscore form, and their log
    // groups are `/aws/bedrock-agentcore/runtimes/<name>-<id>-<endpoint>`), the pull of this stage's runtime repositories, the registry
    // token, and metrics in the bedrock-agentcore namespace. No `logs:DescribeLogGroups` (it takes no resource scope) and no X-Ray.
    const logGroupArn = `arn:aws:logs:${region}:${ACCOUNT}:log-group:/aws/bedrock-agentcore/runtimes/${p.replace(/-/g, "_")}_*`;
    const pulledRepositories = [`${p}-worker-python`]
      .map((r) => `arn:aws:ecr:${region}:${ACCOUNT}:repository/${r}`);
    const runtimeBase = new iam.CfnRole(this, "RuntimeBaseRole", {
      roleName: names.role(cfg, "runtime-base"), maxSessionDuration: 3600,
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{
          Sid: "AgentCoreRuntimes", Effect: "Allow", Principal: { Service: "bedrock-agentcore.amazonaws.com" },
          Action: "sts:AssumeRole",
          Condition: {
            StringEquals: { "aws:SourceAccount": ACCOUNT },
            ArnLike: { "aws:SourceArn": `arn:aws:bedrock-agentcore:${region}:${ACCOUNT}:*` },
          },
        }],
      },
      policies: [{
        policyName: "runtime-base",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "PullOwnImages", Effect: "Allow", Action: ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"],
              Resource: pulledRepositories },
            { Sid: "EcrToken", Effect: "Allow", Action: "ecr:GetAuthorizationToken", Resource: "*" },
            { Sid: "OwnLogGroups", Effect: "Allow", Action: ["logs:CreateLogGroup", "logs:DescribeLogStreams"], Resource: logGroupArn },
            { Sid: "OwnLogStreams", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${logGroupArn}:log-stream:*` },
            { Sid: "RuntimeMetrics", Effect: "Allow", Action: "cloudwatch:PutMetricData", Resource: "*",
              Condition: { StringEquals: { "cloudwatch:namespace": "bedrock-agentcore" } } },
            { Sid: "DenyEverythingAnAgentCouldAbuse", Effect: "Deny", Action: [...BASE_ROLE_DENIES], Resource: "*" },
          ],
        },
      }],
    });
    this.runtimeBaseRoleArn = runtimeBase.attrArn;

    // The role AgentCore assumes to launch and clean up the machines of the Instances capacity provider. Opt-in, and the only thing the option adds to this stack. Trust is the
    // service principal for this account; the permissions are the AWS-managed operator policy and nothing of ours, so no inline statement can widen it.
    // There is no instance profile and no EC2 role: the machines carry no instance credentials, and the execution role stays the eight-action runtime base role above.
    if (props.computeInstances === true) {
      new iam.CfnRole(this, "InstancesOperatorRole", {
        roleName: names.role(cfg, "instances-operator"),
        assumeRolePolicyDocument: {
          Version: "2012-10-17",
          Statement: [{
            Sid: "AgentCoreCapacityProviders", Effect: "Allow", Principal: { Service: "bedrock-agentcore.amazonaws.com" },
            Action: "sts:AssumeRole", Condition: { StringEquals: { "aws:SourceAccount": ACCOUNT } },
          }],
        },
        managedPolicyArns: [INSTANCES_OPERATOR_POLICY_ARN],
      });
    }

    // harness-worker-session: the permission ceiling of every agent session; the dispatcher narrows
    // it further with an inline session policy.
    const bucket = names.bucket(cfg);
    const bucketArn = `arn:aws:s3:::${bucket}`;
    // run-session mints the sessions (it also holds the Lambda that invokes the microVM); worker-api mints the ones /internal/creds refreshes.
    // The operator of the stage mints as well (the E2E tools and the probes assume the worker-session role with their own session tags): the IAM role `Admin`, or the
    // identity the entry names (written as patterns, so the condition is ArnLike).
    const serviceMinters = [names.role(cfg, "worker-api"), names.role(cfg, "run-session")].map((r) => `arn:aws:iam::${ACCOUNT}:role/${r}`);
    const minters = [...(OPERATOR ? principalArnPatterns(ACCOUNT, OPERATOR) : [`arn:aws:iam::${ACCOUNT}:role/Admin`]), ...serviceMinters];
    const mintersMatch = OPERATOR ? "ArnLike" : "ArnEquals";
    const bedrockResources = INFERENCE_REGIONS.flatMap((r) => [
      `arn:aws:bedrock:${r}::foundation-model/*`,
      `arn:aws:bedrock:${r}:${ACCOUNT}:inference-profile/*`,
      `arn:aws:bedrock:${r}:${ACCOUNT}:application-inference-profile/*`,
    ]).concat(["arn:aws:bedrock:::foundation-model/*"]);
    const workerSession = new iam.CfnRole(this, "WorkerSessionRole", {
      roleName: names.role(cfg, "harness-worker-session"), maxSessionDuration: 3600,
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{
          Sid: "MintersWithSessionTags", Effect: "Allow", Principal: { AWS: `arn:aws:iam::${ACCOUNT}:root` },
          Action: ["sts:AssumeRole", "sts:TagSession"],
          Condition: {
            [mintersMatch]: { "aws:PrincipalArn": minters },
            ...RUN_AND_AGENT_TAGS,
            "ForAllValues:StringEquals": { "aws:TagKeys": [...SESSION_TAG_KEYS] },
          },
        }, {
          // SetSourceIdentity is authorized with no request tags in its context, so it has its own
          // statement: minters only, and a source identity must be set.
          Sid: "MintersSetSourceIdentity", Effect: "Allow", Principal: { AWS: `arn:aws:iam::${ACCOUNT}:root` },
          Action: ["sts:SetSourceIdentity"],
          Condition: {
            [mintersMatch]: { "aws:PrincipalArn": minters },
            ...A_SOURCE_IDENTITY,
          },
        }],
      },
      policies: [{
        policyName: "harness-worker-session",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "DenyWithoutRunTag", Effect: "Deny", Action: "*", Resource: "*", Condition: { Null: { "aws:PrincipalTag/run": "true" } } },
            { Sid: "DenyWithoutAgentTag", Effect: "Deny", Action: "*", Resource: "*", Condition: { Null: { "aws:PrincipalTag/agent": "true" } } },
            // C17: agents never reach the E2E git host or start its PR check.
            { Sid: "DenyGitHostAndPrCheck", Effect: "Deny", Action: ["codecommit:*", "codebuild:StartBuild"], Resource: "*" },
            // Root invariant 2: the engine uses these credentials, so they never mint a
            // package-registry token; run-session mints it with its own role.
            { Sid: "NoPackageTokens", Effect: "Deny", Action: ["codeartifact:GetAuthorizationToken", "sts:GetServiceBearerToken"], Resource: "*" },
            // Direct calls only: a call an AWS service makes for the worker (S3 -> KMS Decrypt for an
            // SSE-KMS object) carries neither key, and follows a direct call that passed this check.
            { Sid: "DenyOutsideWorkerNetwork", Effect: "Deny", Action: "*", Resource: "*",
              Condition: {
                StringNotEqualsIfExists: { "aws:SourceVpce": network.endpointIds },
                NotIpAddressIfExists: { "aws:SourceIp": network.natPublicIps.map((ip) => Fn.join("", [ip, "/32"])) },
                BoolIfExists: { "aws:ViaAWSService": "false" },
              } },
            { Sid: "ReadRunInputs", Effect: "Allow", Action: "s3:GetObject", Resource: `${bucketArn}/runs/\${aws:PrincipalTag/run}/input/*` },
            { Sid: "ReadWriteOwnAgentPrefix", Effect: "Allow", Action: ["s3:GetObject", "s3:PutObject", "s3:AbortMultipartUpload"],
              Resource: `${bucketArn}/runs/\${aws:PrincipalTag/run}/agents/\${aws:PrincipalTag/agent}/*` },
            { Sid: "ListOwnPrefixesOnly", Effect: "Allow", Action: "s3:ListBucket", Resource: bucketArn,
              Condition: { StringLike: { "s3:prefix": [
                "runs/${aws:PrincipalTag/run}/input/*", "runs/${aws:PrincipalTag/run}/agents/${aws:PrincipalTag/agent}/*"] } } },
            { Sid: "DataKeyThroughS3Only", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey"], Resource: dataKey.attrArn,
              Condition: { StringEquals: { "kms:ViaService": `s3.${region}.amazonaws.com` } } },
            { Sid: "InvokeModelsCeiling", Effect: "Allow", Action: ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream"],
              Resource: bedrockResources },
            { Sid: "InvokeMantleCeiling", Effect: "Allow", Action: "bedrock-mantle:CreateInference", Resource: "*",
              Condition: { StringEquals: { "aws:RequestedRegion": [...INFERENCE_REGIONS] } } },
            { Sid: "InternalApi", Effect: "Allow", Action: "execute-api:Invoke",
              Resource: `arn:aws:execute-api:${region}:${ACCOUNT}:*/*/POST/internal/*` },
          ],
        },
      }],
    });
    this.workerSessionRoleArn = workerSession.attrArn;

    // Bedrock model invocation logging writes as this role.
    const invocationLogs = `arn:aws:logs:${region}:${ACCOUNT}:log-group:${names.bedrockInvocationLogGroup(cfg)}:log-stream:*`;
    new iam.CfnRole(this, "BedrockLoggingRole", {
      roleName: names.role(cfg, "bedrock-logging"), maxSessionDuration: 3600,
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{
          Effect: "Allow", Principal: { Service: "bedrock.amazonaws.com" }, Action: "sts:AssumeRole",
          Condition: {
            StringEquals: { "aws:SourceAccount": ACCOUNT },
            ArnLike: { "aws:SourceArn": `arn:aws:bedrock:${region}:${ACCOUNT}:*` },
          },
        }],
      },
      policies: [{
        policyName: "invocation-logs",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [{ Sid: "WriteInvocationLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: invocationLogs }],
        },
      }],
    });

    // The second IAM principal, a stranger to every run. A role, never
    // an IAM user or key: the deploying principal assumes it (its sessions carry a source identity,
    // so the trust allows sts:SetSourceIdentity) and it may only say who it is and try the /v1 reads,
    // which control-api must refuse with 403 for another owner's run.
    const v1 = `arn:aws:execute-api:${region}:${ACCOUNT}:*/${cfg.stage}`;
    new iam.CfnRole(this, "E2eOtherPrincipalRole", {
      roleName: names.role(cfg, "e2e-other-principal"), maxSessionDuration: 3600,
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{
          Sid: "DeployingPrincipalOnly", Effect: "Allow", ...deployingTrust(),
          Action: ["sts:AssumeRole", "sts:SetSourceIdentity"],
        }],
      },
      policies: [{
        policyName: "e2e-other-principal",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "WhoAmI", Effect: "Allow", Action: "sts:GetCallerIdentity", Resource: "*" },
            { Sid: "ReadV1AsAStranger", Effect: "Allow", Action: "execute-api:Invoke",
              Resource: [`${v1}/GET/v1/*`, `${v1}/POST/v1/runs/*/artifacts/*/download-url`] },
          ],
        },
      }],
    });
  }
}
