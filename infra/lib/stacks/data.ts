// Data stack: the single harness table, the harness bucket and the
// Bedrock model invocation log group with its own KMS key.
import { Stack, type StackProps } from "aws-cdk-lib";
import * as ddb from "aws-cdk-lib/aws-dynamodb";
import * as kms from "aws-cdk-lib/aws-kms";
import * as logs from "aws-cdk-lib/aws-logs";
import * as s3 from "aws-cdk-lib/aws-s3";
import type { Construct } from "constructs";
import { ACCOUNT, names, type StageConfig } from "../config.ts";
import type { SecurityStack } from "./security.ts";

export interface DataStackProps extends StackProps {
  cfg: StageConfig;
  security: SecurityStack;
}

export class DataStack extends Stack {
  readonly tableName: string;
  readonly tableArn: string;
  readonly tableStreamArn: string;
  readonly bucketName: string;
  readonly bucketArn: string;

  constructor(scope: Construct, id: string, props: DataStackProps) {
    super(scope, id, props);
    const { cfg, security } = props;

    const table = new ddb.CfnTable(this, "Harness", {
      tableName: names.table(cfg),
      billingMode: "PAY_PER_REQUEST",
      attributeDefinitions: [
        { attributeName: "PK", attributeType: "S" }, { attributeName: "SK", attributeType: "S" },
        { attributeName: "ownerPrincipalArn", attributeType: "S" }, { attributeName: "createdAt", attributeType: "S" },
      ],
      keySchema: [{ attributeName: "PK", keyType: "HASH" }, { attributeName: "SK", keyType: "RANGE" }],
      // GSI1 serves GET /v1/runs: only Run items carry ownerPrincipalArn, so the index is sparse.
      globalSecondaryIndexes: [{
        indexName: "GSI1",
        keySchema: [{ attributeName: "ownerPrincipalArn", keyType: "HASH" }, { attributeName: "createdAt", keyType: "RANGE" }],
        projection: { projectionType: "ALL" },
      }],
      streamSpecification: { streamViewType: "NEW_AND_OLD_IMAGES" },
      // route-health items carry `ttl` (last write + 24 h, epoch seconds) so DynamoDB
      // deletes the ones a route stopped writing. Housekeeping only: every deadline is an epoch field the reader
      // checks against its own clock, and items with no `ttl` attribute (ROUTELADDER versions, runs) are never expired.
      timeToLiveSpecification: { attributeName: "ttl", enabled: true },
      pointInTimeRecoverySpecification: { pointInTimeRecoveryEnabled: true },
      sseSpecification: { sseEnabled: true, sseType: "KMS", kmsMasterKeyId: security.dataKeyArn },
      deletionProtectionEnabled: false,
    });
    this.tableName = names.table(cfg);
    this.tableArn = table.attrArn;
    this.tableStreamArn = table.attrStreamArn;

    const bucketName = names.bucket(cfg);
    const bucket = new s3.CfnBucket(this, "HarnessBucket", {
      bucketName,
      bucketEncryption: { serverSideEncryptionConfiguration: [{
        bucketKeyEnabled: true, serverSideEncryptionByDefault: { sseAlgorithm: "aws:kms", kmsMasterKeyId: security.dataKeyArn } }] },
      versioningConfiguration: { status: "Enabled" },
      publicAccessBlockConfiguration: { blockPublicAcls: true, blockPublicPolicy: true, ignorePublicAcls: true, restrictPublicBuckets: true },
      ownershipControls: { rules: [{ objectOwnership: "BucketOwnerEnforced" }] },
      lifecycleConfiguration: { rules: [
        { id: "abort-incomplete-uploads", status: "Enabled", abortIncompleteMultipartUpload: { daysAfterInitiation: 7 } },
        { id: "expire-noncurrent-versions", status: "Enabled", noncurrentVersionExpiration: { noncurrentDays: 30 } },
      ] },
    });
    // The research route's results (item manifests, bodies, audit objects, search records) sit under
    // runs/<run>/agents/direct/research/ and are written only by research-api and research-job. The worker session role (the engine holds
    // its credentials) can write elsewhere under agents/direct/, but a Deny here wins over its session policy, so Cline cannot forge a
    // manifest, an audit object or an index. The role is named by convention, as security.ts and control.ts name it.
    const workerSessionRoleArn = `arn:aws:iam::${ACCOUNT}:role/${names.role(cfg, "harness-worker-session")}`;
    new s3.CfnBucketPolicy(this, "HarnessBucketPolicy", {
      bucket: bucket.ref,
      policyDocument: {
        Version: "2012-10-17",
        Statement: [{
          Sid: "DenyInsecureTransport", Effect: "Deny", Principal: "*", Action: "s3:*",
          Resource: [bucket.attrArn, `${bucket.attrArn}/*`], Condition: { Bool: { "aws:SecureTransport": "false" } },
        }, {
          Sid: "WorkerSessionsCannotWriteResearchMaterial", Effect: "Deny", Principal: "*",
          Action: ["s3:PutObject", "s3:PutObjectTagging", "s3:DeleteObject", "s3:DeleteObjectVersion", "s3:DeleteObjectTagging", "s3:AbortMultipartUpload"],
          Resource: `${bucket.attrArn}/runs/*/agents/direct/research/*`,
          Condition: { ArnEquals: { "aws:PrincipalArn": workerSessionRoleArn } },
        }],
      },
    });
    this.bucketName = bucketName;
    this.bucketArn = bucket.attrArn;

    // scripts/deploy.sh points the home Region's model invocation logging here only when it is unset
    // and the accounts.json entry allows it. The group has its own
    // key: CloudWatch Logs uses it for this group only, and Bedrock's
    // writes as hamh-<stage>-bedrock-logging reach it through logs.
    const group = names.bedrockInvocationLogGroup(cfg);
    const groupArn = `arn:aws:logs:${this.region}:${ACCOUNT}:log-group:${group}`;
    const context = { "kms:EncryptionContext:aws:logs:arn": groupArn };
    const viaLogs = `logs.${this.region}.amazonaws.com`;
    const logsService = { Service: viaLogs };
    const bedrockLogging = { AWS: `arn:aws:iam::${ACCOUNT}:role/${names.role(cfg, "bedrock-logging")}` };
    const invocationKey = new kms.CfnKey(this, "InvocationLogsKey", {
      description: `${cfg.prefix} Bedrock invocation log group ${group}`, enableKeyRotation: true,
      keyPolicy: {
        Version: "2012-10-17",
        Statement: [
          { Sid: "AccountAdministersThroughIam", Effect: "Allow", Principal: { AWS: `arn:aws:iam::${ACCOUNT}:root` },
            Action: "kms:*", Resource: "*" },
          // The encryption-context key is a string key (StringEquals; Access Analyzer TYPE_MISMATCH on
          // ArnEquals), and kms:DescribeKey carries no encryption context, so each principal gets it in
          // its own statement without that condition (UNSUPPORTED_ACTION_FOR_CONDITION_KEY).
          { Sid: "CloudWatchLogsForTheGroup", Effect: "Allow", Principal: logsService, Resource: "*",
            Action: ["kms:Encrypt", "kms:Decrypt", "kms:ReEncrypt*", "kms:GenerateDataKey*"],
            Condition: { StringEquals: context } },
          { Sid: "CloudWatchLogsDescribesTheKey", Effect: "Allow", Principal: logsService, Action: "kms:DescribeKey", Resource: "*" },
          { Sid: "BedrockLoggingWritesThroughLogs", Effect: "Allow", Principal: bedrockLogging, Resource: "*",
            Action: ["kms:Encrypt", "kms:Decrypt", "kms:GenerateDataKey*"],
            Condition: { StringEquals: { "kms:ViaService": viaLogs, ...context } } },
          { Sid: "BedrockLoggingDescribesTheKeyThroughLogs", Effect: "Allow", Principal: bedrockLogging, Action: "kms:DescribeKey",
            Resource: "*", Condition: { StringEquals: { "kms:ViaService": viaLogs } } },
        ],
      },
    });
    new kms.CfnAlias(this, "InvocationLogsKeyAlias", { aliasName: `alias/${cfg.prefix}-invocation-logs`, targetKeyId: invocationKey.ref });
    new logs.CfnLogGroup(this, "BedrockInvocations", {
      logGroupName: group, retentionInDays: cfg.logRetentionDays, kmsKeyId: invocationKey.attrArn,
    });
  }
}
