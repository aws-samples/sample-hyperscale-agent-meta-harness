// The CodeBuild ARM project that builds the stage's images (scripts/codebuild-run.sh): the Cline fork's linux-arm64 build and the
// worker and delivery images. Sources arrive as S3 zips under builds/source/; outputs go to builds/out/ and to the stage's ECR
// repositories. The runtimes stack holds it, beside the worker repository it pushes to (review S3).
import * as cb from "aws-cdk-lib/aws-codebuild";
import * as iam from "aws-cdk-lib/aws-iam";
import * as logs from "aws-cdk-lib/aws-logs";
import type { Stack } from "aws-cdk-lib";
import { ACCOUNT, names, type StageConfig } from "../config.ts";
import type { SecurityStack } from "./security.ts";

export const BUILD_IMAGE = "aws/codebuild/amazonlinux-aarch64-standard:4.0";

export interface ImageBuildOptions {
  /** The suffix of the service role's name (`names.role(cfg, roleName)`): `image-build`. */
  roleName: string;
  /** The ECR repositories (without the stage prefix) the role may push to. */
  repositories: readonly string[];
}

/** The python worker and the delivery image: the two images of stage `direct`. */
export const DIRECT_IMAGE_REPOSITORIES = ["worker-python", "delivery"] as const;

/**
 * Adds the image-build project, its service role and its log group to `stack` under the construct ids `BuildLogs`, `BuildRole` and
 * `ImageBuild`. Returns the project's name.
 */
export function addImageBuild(stack: Stack, cfg: StageConfig, security: SecurityStack, opts: ImageBuildOptions): string {
  const p = cfg.prefix;
  const region = stack.region;
  const bucketArn = `arn:aws:s3:::${names.bucket(cfg)}`;
  const projectName = `${p}-image-build`;

  const logGroup = new logs.CfnLogGroup(stack, "BuildLogs", {
    logGroupName: `/hamh/${cfg.stage}/codebuild/image-build`, retentionInDays: cfg.logRetentionDays,
  });

  const role = new iam.CfnRole(stack, "BuildRole", {
    roleName: names.role(cfg, opts.roleName),
    assumeRolePolicyDocument: {
      Version: "2012-10-17",
      Statement: [{ Effect: "Allow", Principal: { Service: "codebuild.amazonaws.com" }, Action: "sts:AssumeRole",
        Condition: { StringEquals: { "aws:SourceAccount": ACCOUNT } } }],
    },
    policies: [{
      policyName: "image-build",
      policyDocument: {
        Version: "2012-10-17",
        Statement: [
          { Sid: "EcrToken", Effect: "Allow", Action: "ecr:GetAuthorizationToken", Resource: "*" },
          // The harness image repositories only (see DIRECT_IMAGE_REPOSITORIES).
          { Sid: "PushHarnessImages", Effect: "Allow",
            Resource: opts.repositories.map((r) => `arn:aws:ecr:${region}:${ACCOUNT}:repository/${p}-${r}`),
            Action: ["ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer", "ecr:InitiateLayerUpload",
              "ecr:UploadLayerPart", "ecr:CompleteLayerUpload", "ecr:PutImage", "ecr:DescribeImages"] },
          { Sid: "ReadBuildSources", Effect: "Allow", Action: ["s3:GetObject", "s3:GetObjectVersion"], Resource: `${bucketArn}/builds/source/*` },
          { Sid: "WriteBuildOutputs", Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: `${bucketArn}/builds/out/*` },
          // Image builds COPY the promoted Cline artifact; promotion itself is a builder step.
          { Sid: "ReadPromotedCline", Effect: "Allow", Action: "s3:GetObject", Resource: `${bucketArn}/engine/cline/*` },
          { Sid: "BucketLocation", Effect: "Allow", Action: ["s3:GetBucketLocation", "s3:GetBucketVersioning"], Resource: bucketArn },
          { Sid: "DataKeyThroughS3", Effect: "Allow", Action: ["kms:Decrypt", "kms:GenerateDataKey"], Resource: security.dataKeyArn,
            Condition: { StringEquals: { "kms:ViaService": `s3.${region}.amazonaws.com` } } },
          { Sid: "OwnLogs", Effect: "Allow", Action: ["logs:CreateLogStream", "logs:PutLogEvents"], Resource: `${logGroup.attrArn.replace(/:\*$/, "")}:*` },
          { Sid: "BuildReports", Effect: "Allow", Resource: `arn:aws:codebuild:${region}:${ACCOUNT}:report-group/${p}-*`,
            Action: ["codebuild:CreateReportGroup", "codebuild:CreateReport", "codebuild:UpdateReport", "codebuild:BatchPutTestCases"] },
        ],
      },
    }],
  });

  new cb.CfnProject(stack, "ImageBuild", {
    name: projectName,
    description: `${p}: native linux-arm64 image builds and the Cline fork linux-arm64 build`,
    serviceRole: role.attrArn,
    encryptionKey: security.dataKeyArn,
    environment: { type: "ARM_CONTAINER", image: BUILD_IMAGE, computeType: "BUILD_GENERAL1_LARGE", privilegedMode: true },
    // Each build overrides the location with builds/source/<sha>.zip and the buildspec it needs.
    source: { type: "S3", location: `${names.bucket(cfg)}/builds/source/latest.zip` },
    artifacts: { type: "NO_ARTIFACTS" },
    timeoutInMinutes: 180,
    queuedTimeoutInMinutes: 60,
    logsConfig: { cloudWatchLogs: { status: "ENABLED", groupName: logGroup.ref } },
  });
  return projectName;
}
