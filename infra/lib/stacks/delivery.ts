// Delivery stack: repo-gateway (the only holder of git write access), bundle-validator and packager (no secrets) as arm64
// container-image Lambdas. One image (images/delivery/Dockerfile: the three binaries, git and gitleaks) serves all three;
// each function's entry point is its binary. Phase 1 (no image yet): the ECR repository with its policy for the Lambda
// service, the roles and the log groups. Phase 2 (a digest-pinned image): the three functions.
import { CfnOutput, Stack, type StackProps } from "aws-cdk-lib";
import * as cb from "aws-cdk-lib/aws-codebuild";
import * as ecr from "aws-cdk-lib/aws-ecr";
import * as events from "aws-cdk-lib/aws-events";
import * as iam from "aws-cdk-lib/aws-iam";
import * as lambda from "aws-cdk-lib/aws-lambda";
import * as logs from "aws-cdk-lib/aws-logs";
import * as assets from "aws-cdk-lib/aws-s3-assets";
import type { Construct } from "constructs";
import { DEFAULT_REPO_NAMES } from "../repo-names.ts";
import { ACCOUNT, DELIVERY_FUNCTIONS, type DeliveryFunction, OWNER, PROJECT, REPO_NAMES, names, type StageConfig } from "../config.ts";
import type { DataStack } from "./data.ts";
import { BUILD_IMAGE } from "./image-build.ts";
import type { SecurityStack } from "./security.ts";

const KEEP_IMAGES = JSON.stringify({
  rules: [{ rulePriority: 1, description: "keep the newest 30 images", selection: { tagStatus: "any", countType: "imageCountMoreThan", countNumber: 30 }, action: { type: "expire" } }],
});

/** Lambda's maximum timeout and ephemeral storage: a bundle, its unpacked copy and the zip fit. */
const TIMEOUT_S = 900;
const EPHEMERAL_MB = 10_240;
const MEMORY_MB: Record<DeliveryFunction, number> = { "repo-gateway": 1024, "bundle-validator": 2048, packager: 2048 };

/** repo-gateway -> RepoGateway: the logical id stem of each function's resources. */
const pascal = (n: string) => n.split("-").map((s) => s[0].toUpperCase() + s.slice(1)).join("");

/** The E2E-owned oracle tests the PR check reads as a secondary source, never through a repository. */
const ORACLE_PYTHON_CALC = new URL("../../../fixtures/python-calc/oracle", import.meta.url).pathname;
const ORACLE_FANOUT_KATA = new URL("../../../fixtures/fanout-kata/oracle", import.meta.url).pathname;
const ORACLE_TS_LIB = new URL("../../../fixtures/ts-lib/oracle", import.meta.url).pathname;
const ORACLE_FANOUT_KATA_24 = new URL("../../../fixtures/fanout-kata-24/oracle", import.meta.url).pathname;
const ORACLE_LOAD_KATA = new URL("../../../fixtures/load-kata/oracle", import.meta.url).pathname;
/** The project's default source; every rule-started build overrides it with the PR's repository. */
const DEFAULT_CHECK_REPO = "hamh-e2e-python-calc";
/**
 * COMPAT-CAH-DATA: the prefix of the CodeCommit repositories that a stage created before the rename. They stay in the account when that stage is torn
 * down, and the stage accepts them again after an operator allows them (`ham-admin repos allow`). repo-gateway reads, pushes to and opens pull requests in them, the PR check builds there
 * and approves, and the rule that starts the check fires for their pull requests, when the repository-name rule lists the default entry (as `allows_earlier_name` does in the Rust copies of
 * the matcher). The repositories created now (`NEW_REPO_PREFIXES` below) take only the current prefix, in the IAM statements that create them and in the allowlist items that name them.
 */
const EARLIER_REPO_PREFIX = "cah-e2e-"; // COMPAT-CAH-DATA

/** The names `--new-repo` makes: hamh-e2e-new-<yyyymmdd> and hamh-e2e-ntier-<kind>-<yyyymmdd>; harness_protocol::target::NEW_REPO_PREFIXES. */
const NEW_REPO_PREFIXES = ["hamh-e2e-new-", "hamh-e2e-ntier-"];

/**
 * The CodeCommit repository-name patterns that the delivery grants name for a stage whose repository-name rule is `rule` (lib/repo-names.ts: a repository name, `prefix*` or `*`).
 * The functions accept the names the rule covers, and IAM must let them reach the same repositories, so each entry is a pattern of its own: an exact name stays exact, a prefix stays a
 * prefix and `*` covers the account and Region. Two more groups follow the Rust matcher (crates/delivery/src/repo_names.rs, checks.rs): the repositories of the earlier prefix, when the rule
 * lists the default entry (COMPAT-CAH-DATA), and the repositories `--new-repo` makes, which are covered whatever the rule says. Neither is added when an entry of the rule already covers it,
 * so the default rule gives the two patterns it always gave.
 */
export function grantedRepositoryNames(rule: readonly string[]): string[] {
  const covered = (pattern: string) => rule.some((entry) => entry === "*" || (entry.endsWith("*") && pattern.startsWith(entry.slice(0, -1))));
  const granted = [...rule];
  const earlier = `${EARLIER_REPO_PREFIX}*`; // COMPAT-CAH-DATA
  if (rule.includes(DEFAULT_REPO_NAMES[0]) && !covered(earlier)) granted.push(earlier);
  for (const prefix of NEW_REPO_PREFIXES) if (!covered(`${prefix}*`)) granted.push(`${prefix}*`);
  return granted;
}

type Statement = Record<string, unknown>;

export interface DeliveryStackProps extends StackProps {
  cfg: StageConfig;
  security: SecurityStack;
  data: DataStack;
  /** `<account>.dkr.ecr.<region>.amazonaws.com/hamh-<stage>-delivery@sha256:<digest>`; absent in phase 1. */
  deliveryImage?: string;
}

export class DeliveryStack extends Stack {
  readonly repositoryName: string;
  readonly functionArns: Partial<Record<DeliveryFunction, string>> = {};

  constructor(scope: Construct, id: string, props: DeliveryStackProps) {
    super(scope, id, props);
    const { cfg, security } = props;
    // The table and bucket are named by convention; the dependency orders the deploy after them.
    this.addStackDependency(props.data);
    const p = cfg.prefix;
    const region = this.region;
    this.repositoryName = `${p}-delivery`;
    const repoArn = `arn:aws:ecr:${region}:${ACCOUNT}:repository/${this.repositoryName}`;
    const bucket = `arn:aws:s3:::${names.bucket(cfg)}`;
    const table = `arn:aws:dynamodb:${region}:${ACCOUNT}:table/${names.table(cfg)}`;
    const codecommitArn = (name: string) => `arn:aws:codecommit:${region}:${ACCOUNT}:${name}`;
    // Every repository the stage reaches: the entries of the repository-name rule, the repositories of the earlier prefix and those --new-repo makes (`grantedRepositoryNames`).
    // The default rule gives hamh-e2e-* and then the earlier prefix, as before. The event pattern of the PR check is built from the same names (`prCheck`).
    const granted = grantedRepositoryNames(REPO_NAMES);
    const e2e = granted.map(codecommitArn);
    // The repositories repo-gateway may create: only the --new-repo names, whatever the rule says.
    const e2eNew = NEW_REPO_PREFIXES.map((n) => codecommitArn(`${n}*`));
    const newPrefixes = NEW_REPO_PREFIXES;

    // The physical name of each delivery function, shared by the functions below and by the repository policy.
    const functionName = (name: DeliveryFunction) => `${p}-${name}`;
    // Lambda marks an idle image function Inactive and pulls its image again as the Lambda service principal when the function is next
    // invoked. The execution role's PullDeliveryImage statement does not cover that pull: with no repository policy, three functions of a live
    // stage went Inactive about nine hours after their last run (StateReasonCode ImageAccessDenied) and the next run ended blocked at
    // StageRepo. The statement names the three functions and their qualified forms (`<arn>:*`, which never matches another function name),
    // so that a function of another account or another project cannot use this image through the service principal. Its Sid differs from
    // the one Lambda gives the statement it adds by itself (LambdaECRImageRetrievalPolicy, which is `function:*` in the whole account).
    // ArnLike, not StringLike, for the ARN key: IAM Access Analyzer reports STRING_LIKE_OPERATOR_WITH_ARN_CONDITION_KEYS, and the patterns match the same ARNs.
    const imageUsers = DELIVERY_FUNCTIONS.flatMap((name) => {
      const arn = `arn:aws:lambda:${region}:${ACCOUNT}:function:${functionName(name)}`;
      return [arn, `${arn}:*`];
    });
    const repo = new ecr.CfnRepository(this, "DeliveryRepo", {
      repositoryName: this.repositoryName, imageTagMutability: "IMMUTABLE",
      imageScanningConfiguration: { scanOnPush: true }, lifecyclePolicy: { lifecyclePolicyText: KEEP_IMAGES },
      repositoryPolicyText: {
        Version: "2012-10-17",
        Statement: [{
          Sid: "HamhLambdaServiceImagePull", Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" },
          Action: ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"],
          Condition: { ArnLike: { "aws:SourceArn": imageUsers } },
        }],
      },
    });
    new CfnOutput(this, "DeliveryRepositoryUri", { value: repo.attrRepositoryUri, description: "push target for the delivery image" });

    const dataKey = (via: string[]): Statement => ({
      Sid: "DataKeyThroughS3AndDynamoDb", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"],
      Resource: security.dataKeyArn, Condition: { StringEquals: { "kms:ViaService": via.map((s) => `${s}.${region}.amazonaws.com`) } },
    });
    const noCodeCommit: Statement = { Sid: "NoCodeCommit", Effect: "Deny", Action: "codecommit:*", Resource: "*" };
    const verifiedBundle = `${bucket}/runs/*/deliverable/verified.bundle`;

    // Grants and item 3 denies; D2 item 6 main Deny.
    const gateway: Statement[] = [
      { Sid: "ReadE2eRepositories", Effect: "Allow", Action: ["codecommit:GitPull", "codecommit:GetRepository"], Resource: e2e },
      // git-receive-pack's first request carries no reference, hence IfExists (CodeCommit user guide).
      { Sid: "PushHarnessBranches", Effect: "Allow", Action: "codecommit:GitPush", Resource: e2e,
        Condition: { StringLikeIfExists: { "codecommit:References": ["refs/heads/harness/*"] } } },
      { Sid: "DenyPushOutsideHarness", Effect: "Deny", Action: "codecommit:GitPush", Resource: "*",
        Condition: { StringNotLikeIfExists: { "codecommit:References": ["refs/heads/harness/*"] }, Null: { "codecommit:References": "false" } } },
      { Sid: "DenyMainWrites", Effect: "Deny", Resource: "*",
        Action: ["codecommit:GitPush", "codecommit:PutFile", "codecommit:CreateCommit", "codecommit:DeleteBranch", "codecommit:DeleteFile",
          "codecommit:MergeBranchesBy*", "codecommit:MergePullRequestBy*"],
        Condition: { StringEqualsIfExists: { "codecommit:References": ["refs/heads/main"] }, Null: { "codecommit:References": "false" } } },
      { Sid: "PullRequests", Effect: "Allow", Resource: e2e,
        Action: ["codecommit:CreatePullRequest", "codecommit:CreatePullRequestApprovalRule", "codecommit:PostCommentForPullRequest", "codecommit:GetPullRequest"] },
      { Sid: "NeverApproveMergeOrBuild", Effect: "Deny", Resource: "*",
        Action: ["codecommit:UpdatePullRequestApprovalState", "codecommit:OverridePullRequestApprovalRules", "codecommit:UpdatePullRequestApprovalRuleContent",
          "codecommit:DeletePullRequestApprovalRule", "codecommit:DeleteRepository", "codecommit:MergeBranchesBy*", "codecommit:MergePullRequestBy*",
          "codebuild:StartBuild"] },
      // CreateRepository with tags also authorizes TagResource; both require the four tags.
      { Sid: "CreateNewRepositories", Effect: "Allow", Action: ["codecommit:CreateRepository", "codecommit:TagResource"], Resource: e2eNew,
        Condition: {
          StringEquals: { "aws:RequestTag/project": PROJECT, "aws:RequestTag/stage": cfg.stage, "aws:RequestTag/owner": OWNER, "aws:RequestTag/managed-by": "repo-gateway" },
          "ForAllValues:StringEquals": { "aws:TagKeys": ["project", "stage", "owner", "managed-by"] },
        } },
      // The empty base: main is created on the pushed base commit; CreateBranch never moves a branch.
      { Sid: "NewRepositoryMain", Effect: "Allow", Action: "codecommit:CreateBranch", Resource: e2eNew,
        Condition: { StringEquals: { "codecommit:References": "refs/heads/main" } } },
      { Sid: "NewRepositoryDefaultBranch", Effect: "Allow", Action: "codecommit:UpdateDefaultBranch", Resource: e2eNew },
      { Sid: "AllowlistRead", Effect: "Allow", Action: "dynamodb:GetItem", Resource: table,
        Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["REPOALLOW#*"] } } },
      // new-repo allowlists the repository it created.
      { Sid: "AllowlistNewRepositories", Effect: "Allow", Action: "dynamodb:PutItem", Resource: table,
        Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": newPrefixes.map((n) => `REPOALLOW#cc:${region}:${n}*`) } } },
      // ...and adds it to the listing item ham doctor reads (listing only, never authorization).
      { Sid: "AllowlistIndex", Effect: "Allow", Action: "dynamodb:UpdateItem", Resource: table,
        Condition: { "ForAllValues:StringEquals": { "dynamodb:LeadingKeys": ["REPOALLOW#INDEX"] } } },
      { Sid: "StageInputs", Effect: "Allow", Action: "s3:PutObject",
        Resource: [`${bucket}/runs/*/input/repo.bundle`, `${bucket}/runs/*/input/tree.json`, `${bucket}/runs/*/input/repo-modes.yaml`] },
      { Sid: "ReadVerifiedBundle", Effect: "Allow", Action: "s3:GetObject", Resource: verifiedBundle },
      dataKey(["s3", "dynamodb"]),
    ];
    const validator: Statement[] = [
      // A worker or merge turn bundle becomes the verified bundle (sourceBundleUri). A run that blocked before its first merge
      // holds the base: its source is the staged input bundle, the one exact key shape per run that StageInputs lets repo-gateway
      // write.
      // A direct run whose session ended with no final bundle is delivered from agentd's last snapshot: plan-delivery names
      // turns/1/snapshot.bundle as sourceBundleUri.
      // The direct agent's key only: no other agent writes a snapshot, and nothing else under turns/ is granted by this pattern.
      { Sid: "ReadBundles", Effect: "Allow", Action: "s3:GetObject",
        Resource: [`${bucket}/runs/*/agents/*/turns/*/bundle`, `${bucket}/runs/*/agents/direct/turns/*/snapshot.bundle`,
          `${bucket}/runs/*/input/repo.bundle`, verifiedBundle] },
      // The source bundle of a direct run is read at the version agentd recorded for its own write, not at whatever is current (a GET with a
      // versionId needs s3:GetObjectVersion). The direct agent's bundle key only.
      { Sid: "ReadPinnedBundleVersions", Effect: "Allow", Action: "s3:GetObjectVersion", Resource: `${bucket}/runs/*/agents/direct/turns/*/bundle` },
      { Sid: "WriteVerifiedBundle", Effect: "Allow", Action: "s3:PutObject", Resource: verifiedBundle },
      // The record of what a passing delivery changed (crates/delivery/src/changes.rs), which control-api's head route reads for the follow-up
      // digest. One key, beside the verified bundle; control-api already reads runs/*/deliverable/*.
      { Sid: "WriteChangeRecord", Effect: "Allow", Action: "s3:PutObject", Resource: `${bucket}/runs/*/deliverable/changes.json` },
      // The third-party copy rule (crates/delivery/src/third_party.rs) lists the items a run stored through the
      // research route and reads each item's manifest and body, for every run. The two grants are the items alone: not the audit objects,
      // not the search records, not another agent's research, and no write (the worker session role is denied writes there by the bucket
      // policy of the data stack; a role that cannot list gets AccessDenied for the listing of every run and blocks every delivery).
      { Sid: "ListResearchItems", Effect: "Allow", Action: "s3:ListBucket", Resource: bucket,
        Condition: { StringLike: { "s3:prefix": ["runs/*/agents/direct/research/items/*"] } } },
      { Sid: "ReadResearchItems", Effect: "Allow", Action: "s3:GetObject", Resource: `${bucket}/runs/*/agents/direct/research/items/*` },
      dataKey(["s3"]),
      noCodeCommit,
    ];
    const packager: Statement[] = [
      { Sid: "ReadBundleAndEvidence", Effect: "Allow", Action: "s3:GetObject",
        // gate-runner and the verifier write gates-<mode>.json; collect-hidden's report stays out. A clean verification re-run
        // after a hidden-set revision writes gates-verify-r<k>.json and the goal step records it with the task's
        // other reports, so the package names it. Turn bundles become the bundle:<taskId> artifacts.
        Resource: [verifiedBundle, `${bucket}/runs/*/evidence/*`, `${bucket}/runs/*/agents/*/turns/*/bundle`,
          ...["red-check", "session", "verify"].map((m) => `${bucket}/runs/*/agents/*/turns/*/gates-${m}.json`),
          `${bucket}/runs/*/agents/*/turns/*/gates-verify-r*.json`] },
      { Sid: "ListEvidence", Effect: "Allow", Action: "s3:ListBucket", Resource: bucket, Condition: { StringLike: { "s3:prefix": ["runs/*/evidence/*"] } } },
      // The zip, the partial zip and the evidence archive; the task bundles under bundles/.
      { Sid: "WriteDeliverableZips", Effect: "Allow", Action: "s3:PutObject",
        Resource: [`${bucket}/runs/*/deliverable/*.zip`, `${bucket}/runs/*/deliverable/bundles/*`] },
      { Sid: "ArtifactItems", Effect: "Allow", Action: ["dynamodb:PutItem", "dynamodb:GetItem"], Resource: table,
        Condition: { "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["RUN#*"] } } },
      dataKey(["s3", "dynamodb"]),
      noCodeCommit,
    ];
    const grants: Record<DeliveryFunction, Statement[]> = { "repo-gateway": gateway, "bundle-validator": validator, packager };

    const roles: Partial<Record<DeliveryFunction, iam.CfnRole>> = {};
    const groups: Partial<Record<DeliveryFunction, logs.CfnLogGroup>> = {};
    for (const name of DELIVERY_FUNCTIONS) {
      const id = pascal(name);
      const group = new logs.CfnLogGroup(this, `${id}Logs`, { logGroupName: `/aws/lambda/${p}-${name}`, retentionInDays: cfg.logRetentionDays });
      groups[name] = group;
      roles[name] = new iam.CfnRole(this, `${id}Role`, {
        roleName: names.role(cfg, name),
        assumeRolePolicyDocument: {
          Version: "2012-10-17",
          // The trust policy of the control stack's Lambda roles, which run live.
          Statement: [{ Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }],
        },
        policies: [{
          policyName: name,
          policyDocument: {
            Version: "2012-10-17",
            Statement: [
              ...grants[name],
              // Same-account image pull through the execution role (Lambda container image guide).
              { Sid: "PullDeliveryImage", Effect: "Allow", Action: ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"], Resource: repoArn },
              { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: group.attrArn },
              { Sid: "Tracing", Effect: "Allow", Action: ["xray:PutTraceSegments", "xray:PutTelemetryRecords"], Resource: "*" },
            ],
          },
        }],
      });
    }

    this.prCheck(cfg, granted);

    if (props.deliveryImage === undefined) return;
    const image = props.deliveryImage;
    if (!/@sha256:[0-9a-f]{64}$/.test(image)) {
      // nosemgrep: html-in-template-string -- an exception message that shows the expected image form, no HTML is produced
      throw new Error(`deliveryImage must be pinned by digest (<repo>@sha256:<64 hex>), got ${image}`);
    }
    const repoUri = `${ACCOUNT}.dkr.ecr.${region}.amazonaws.com/${this.repositoryName}`;
    if (!image.startsWith(`${repoUri}@`)) {
      throw new Error(`deliveryImage must come from ${repoUri}, got ${image}`);
    }

    for (const name of DELIVERY_FUNCTIONS) {
      const id = pascal(name);
      const fn = new lambda.CfnFunction(this, id, {
        functionName: functionName(name), packageType: "Image", code: { imageUri: image },
        imageConfig: { entryPoint: [`/opt/delivery/bin/${name}`] }, architectures: ["arm64"],
        role: (roles[name] as iam.CfnRole).attrArn, memorySize: MEMORY_MB[name], timeout: TIMEOUT_S,
        ephemeralStorage: { size: EPHEMERAL_MB },
        tracingConfig: { mode: "Active" },
        loggingConfig: { logGroup: (groups[name] as logs.CfnLogGroup).ref, logFormat: "JSON" },
        environment: { variables: {
          BUCKET_NAME: names.bucket(cfg), TABLE_NAME: names.table(cfg), HAMH_STAGE: cfg.stage, HAMH_OWNER: OWNER, HAMH_REPO_NAMES: REPO_NAMES.join(","),
          GIT_BIN: "/usr/bin/git", GITLEAKS_BIN: "/opt/delivery/bin/gitleaks", WORK_ROOT: "/tmp/delivery",
        } },
      });
      this.functionArns[name] = fn.attrArn;
      new CfnOutput(this, `${id}Arn`, { value: fn.attrArn, description: `${name} function (Step Functions task target)` });
    }
  }

  /**
   * The CodeCommit PR check. Rule hamh-<stage>-prcheck
   * starts project hamh-<stage>-prcheck through role hamh-<stage>-prcheck-events on every open
   * PR in a repository of the stage's repository-name rule (hamh-e2e-* by default, and then also the earlier prefix of the repositories of a stage deployed before the rename)
   * from refs/heads/harness/* into main (create and source update). The build runs the
   * source commit's root buildspec.yml as role hamh-<stage>-prcheck-build, the one pool member of
   * hamh-prcheck-required, which may approve and comment but never override, edit rules, merge,
   * push or start builds.
   */
  private prCheck(cfg: StageConfig, granted: string[]): void {
    const p = cfg.prefix;
    const region = this.region;
    const e2e = granted.map((name) => `arn:aws:codecommit:${region}:${ACCOUNT}:${name}`);
    const projectName = `${p}-prcheck`;
    const projectArn = `arn:aws:codebuild:${region}:${ACCOUNT}:project/${projectName}`;
    const repoUrl = (name: string) => `https://git-codecommit.${region}.amazonaws.com/v1/repos/${name}`;
    // The ECR pull-through cache prefix of public.ecr.aws that the removed lanes stack created; no stack of this app creates it now, and the
    // grant and the IMAGE_MIRROR variable below stay so that the template is unchanged (a candidate to trim with a synth diff accepted).
    const mirrorPrefix = `${p}-ecr-public`;
    const group = new logs.CfnLogGroup(this, "PrCheckLogs", {
      logGroupName: `/aws/codebuild/${projectName}`, retentionInDays: cfg.logRetentionDays,
    });
    // One secondary source per CodeCommit fixture; each fixture buildspec reads its own identifier.
    const oracles = [
      { id: "oracle_python_calc", asset: new assets.Asset(this, "OraclePythonCalc", { path: ORACLE_PYTHON_CALC }) },
      { id: "oracle_fanout_kata", asset: new assets.Asset(this, "OracleFanoutKata", { path: ORACLE_FANOUT_KATA }) },
      { id: "oracle_ts_lib", asset: new assets.Asset(this, "OracleTsLib", { path: ORACLE_TS_LIB }) },
      { id: "oracle_fanout_kata_24", asset: new assets.Asset(this, "OracleFanoutKata24", { path: ORACLE_FANOUT_KATA_24 }) },
      { id: "oracle_load_kata", asset: new assets.Asset(this, "OracleLoadKata", { path: ORACLE_LOAD_KATA }) },
    ];
    const trust = (service: string) => ({
      Version: "2012-10-17",
      Statement: [{ Effect: "Allow", Principal: { Service: service }, Action: "sts:AssumeRole",
        Condition: { StringEquals: { "aws:SourceAccount": ACCOUNT } } }],
    });

    // No IAM path on either role: CDK's CloudFormation
    // validation (E3050) refuses a CodeBuild ServiceRole whose role has a path, and the pool member
    // ARN assumed-role/<name>/* carries no path anyway.
    const buildRole = new iam.CfnRole(this, "PrCheckBuildRole", {
      roleName: names.role(cfg, "prcheck-build"),
      assumeRolePolicyDocument: trust("codebuild.amazonaws.com"),
      policies: [{
        policyName: "prcheck-build",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            // GitPull is the source clone; the rest is what the fixture buildspec calls.
            { Sid: "ReadE2eRepositories", Effect: "Allow", Action: ["codecommit:GitPull", "codecommit:GetPullRequest", "codecommit:GetFile"], Resource: e2e },
            { Sid: "ApproveAndComment", Effect: "Allow", Action: ["codecommit:UpdatePullRequestApprovalState", "codecommit:PostCommentForPullRequest"], Resource: e2e },
            { Sid: "ReadOracleSource", Effect: "Allow", Action: ["s3:GetObject", "s3:GetObjectVersion"],
              Resource: oracles.map((o) => `arn:aws:s3:::${o.asset.s3BucketName}/${o.asset.s3ObjectKey}`) },
            // Every asset shares the CDK assets bucket.
            { Sid: "OracleSourceBucket", Effect: "Allow", Action: ["s3:GetBucketAcl", "s3:GetBucketLocation"], Resource: `arn:aws:s3:::${oracles[0].asset.s3BucketName}` },
            { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: group.attrArn },
            // Compose and Testcontainers images came only through the pull-through
            // mirror of public.ecr.aws; the first pull creates and tags the
            // mirror repository. Nothing here pushes an image.
            { Sid: "EcrToken", Effect: "Allow", Action: "ecr:GetAuthorizationToken", Resource: "*" },
            { Sid: "PullMirror", Effect: "Allow",
              Action: ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer", "ecr:BatchCheckLayerAvailability", "ecr:BatchImportUpstreamImage",
                "ecr:CreateRepository", "ecr:TagResource"],
              Resource: `arn:aws:ecr:${region}:${ACCOUNT}:repository/${mirrorPrefix}/*` },
            // Agent code runs in this build: whatever it tries, the role can only approve and comment.
            { Sid: "NeverOverrideMergeEditOrBuild", Effect: "Deny", Resource: "*",
              Action: ["codecommit:OverridePullRequestApprovalRules", "codecommit:UpdatePullRequestApprovalRuleContent",
                "codecommit:CreatePullRequestApprovalRule", "codecommit:DeletePullRequestApprovalRule",
                "codecommit:CreateApprovalRuleTemplate", "codecommit:UpdateApprovalRuleTemplate*",
                "codecommit:AssociateApprovalRuleTemplateWithRepository", "codecommit:DisassociateApprovalRuleTemplateFromRepository",
                "codecommit:MergeBranchesBy*", "codecommit:MergePullRequestBy*", "codecommit:GitPush", "codecommit:PutFile",
                "codecommit:CreateCommit", "codecommit:CreateBranch", "codecommit:DeleteBranch", "codecommit:DeleteFile",
                "codecommit:CreateRepository", "codecommit:DeleteRepository", "codecommit:UpdatePullRequestStatus",
                "codebuild:StartBuild", "codebuild:RetryBuild", "codebuild:UpdateProject"] },
          ],
        },
      }],
    });

    const project = new cb.CfnProject(this, "PrCheck", {
      name: projectName,
      description: `${p}: CodeCommit PR check; runs the PR source commit's buildspec.yml and approves the revision on success`,
      serviceRole: buildRole.attrArn,
      source: { type: "CODECOMMIT", location: repoUrl(DEFAULT_CHECK_REPO), buildSpec: "buildspec.yml", gitCloneDepth: 1 },
      secondarySources: oracles.map((o) => ({ type: "S3", location: `${o.asset.s3BucketName}/${o.asset.s3ObjectKey}`, sourceIdentifier: o.id })),
      artifacts: { type: "NO_ARTIFACTS" },
      // Privileged: the generated buildspec starts the repository's Compose data tier.
      environment: {
        type: "ARM_CONTAINER", image: BUILD_IMAGE, computeType: `BUILD_GENERAL1_${cfg.prCheck.compute.toUpperCase()}`, privilegedMode: true,
        environmentVariables: [{ name: "IMAGE_MIRROR", value: `${ACCOUNT}.dkr.ecr.${region}.amazonaws.com/${mirrorPrefix}`, type: "PLAINTEXT" }],
      },
      // 60 minutes by default, CodeBuild's own default (it was 20, and a repository whose own build took longer could never be approved); HAMH_PR_CHECK_MINUTES sets it, 5 to 480.
      timeoutInMinutes: cfg.prCheck.buildMinutes,
      queuedTimeoutInMinutes: 60,
      logsConfig: { cloudWatchLogs: { status: "ENABLED", groupName: group.ref } },
    });

    const eventsRole = new iam.CfnRole(this, "PrCheckEventsRole", {
      roleName: names.role(cfg, "prcheck-events"),
      assumeRolePolicyDocument: trust("events.amazonaws.com"),
      policies: [{
        policyName: "prcheck-events",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [{ Sid: "StartThePrCheck", Effect: "Allow", Action: "codebuild:StartBuild", Resource: project.attrArn }],
        },
      }],
    });

    const env = (name: string, value: string) => `{"name": "${name}", "value": "${value}", "type": "PLAINTEXT"}`;
    new events.CfnRule(this, "PrCheckRule", {
      name: projectName,
      // The default rule keeps its text; a configured rule is not listed here (a description holds 512 characters at most, a rule up to ten entries of 100).
      description: `${p}: start ${projectName} on ${REPO_NAMES.join() === DEFAULT_REPO_NAMES.join() ? "hamh-e2e-* harness PRs into main" : "harness PRs into main in the repositories of the stage's repository-name rule"}`,
      state: "ENABLED",
      eventPattern: {
        source: ["aws.codecommit"],
        "detail-type": ["CodeCommit Pull Request State Change"],
        // A prefix entry (and *) is a prefix of the repository ARN; an exact name is the whole ARN.
        resources: e2e.map((arn) => (arn.endsWith("*") ? { prefix: arn.slice(0, -1) } : arn)),
        detail: {
          event: ["pullRequestCreated", "pullRequestSourceBranchUpdated"],
          pullRequestStatus: ["Open"],
          destinationReference: ["refs/heads/main"],
          sourceReference: [{ prefix: "refs/heads/harness/" }],
        },
      },
      targets: [{
        id: "prcheck",
        arn: project.attrArn,
        roleArn: eventsRole.attrArn,
        inputTransformer: {
          inputPathsMap: {
            prId: "$.detail.pullRequestId", revId: "$.detail.revisionId", src: "$.detail.sourceCommit",
            dst: "$.detail.destinationCommit", repo: "$.detail.repositoryNames[0]",
          },
          // StartBuild input: the PR's repository and source commit, and what the buildspec reads.
          // nosemgrep: html-in-template-string -- the JSON input template of an EventBridge rule, its angle bracket names are input transformer placeholders and not HTML
          inputTemplate: `{"sourceVersion": "<src>", "sourceTypeOverride": "CODECOMMIT", "sourceLocationOverride": "${repoUrl("<repo>")}", `
            // nosemgrep: raw-html-join -- the join builds a JSON array of build environment overrides for an EventBridge input template from constants in this file, it is not HTML and no user data enters it
            + `"environmentVariablesOverride": [${[env("PR_ID", "<prId>"), env("PR_REVISION", "<revId>"), env("SRC_COMMIT", "<src>"),
              env("DEST_COMMIT", "<dst>"), env("REPO", "<repo>")].join(", ")}]}`,
        },
      }],
    });
    new CfnOutput(this, "PrCheckProjectName", { value: projectName, description: "CodeBuild project of the CodeCommit PR check" });
    new CfnOutput(this, "PrCheckProjectArn", { value: projectArn });
  }
}
