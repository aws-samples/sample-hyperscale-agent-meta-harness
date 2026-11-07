import { describe, expect, test } from "vitest";
import { Match } from "aws-cdk-lib/assertions";
import { ACCOUNT, REGION } from "../lib/config.ts";
import { resourcesOf, template } from "./helpers.ts";

// The data stack: single table, GSI1, Streams.
describe("data stack", () => {
  const t = template("direct", "data");

  test("table harness: PK/SK strings, on demand, streams, GSI1 on owner and createdAt, PITR, KMS", () => {
    t.hasResourceProperties("AWS::DynamoDB::Table", {
      TableName: "hamh-direct-harness",
      BillingMode: "PAY_PER_REQUEST",
      KeySchema: [{ AttributeName: "PK", KeyType: "HASH" }, { AttributeName: "SK", KeyType: "RANGE" }],
      StreamSpecification: { StreamViewType: "NEW_AND_OLD_IMAGES" },
      PointInTimeRecoverySpecification: { PointInTimeRecoveryEnabled: true },
      SSESpecification: Match.objectLike({ SSEEnabled: true, SSEType: "KMS", KMSMasterKeyId: Match.anyValue() }),
      GlobalSecondaryIndexes: [Match.objectLike({
        IndexName: "GSI1",
        KeySchema: [{ AttributeName: "ownerPrincipalArn", KeyType: "HASH" }, { AttributeName: "createdAt", KeyType: "RANGE" }],
        Projection: { ProjectionType: "ALL" },
      })],
    });
    const attrs = (Object.values(resourcesOf(t, "AWS::DynamoDB::Table"))[0].Properties as any).AttributeDefinitions;
    expect(attrs.every((a: any) => a.AttributeType === "S")).toBe(true);
  });

  test("bucket: SSE-KMS with bucket key, versioning, no public access, owner-enforced, lifecycle", () => {
    t.hasResourceProperties("AWS::S3::Bucket", {
      BucketName: `hamh-direct-harness-${ACCOUNT}`,
      BucketEncryption: { ServerSideEncryptionConfiguration: [Match.objectLike({
        BucketKeyEnabled: true, ServerSideEncryptionByDefault: Match.objectLike({ SSEAlgorithm: "aws:kms" }) })] },
      VersioningConfiguration: { Status: "Enabled" },
      PublicAccessBlockConfiguration: { BlockPublicAcls: true, BlockPublicPolicy: true, IgnorePublicAcls: true, RestrictPublicBuckets: true },
      OwnershipControls: { Rules: [{ ObjectOwnership: "BucketOwnerEnforced" }] },
      LifecycleConfiguration: { Rules: Match.arrayWith([
        Match.objectLike({ AbortIncompleteMultipartUpload: { DaysAfterInitiation: 7 }, Status: "Enabled" }),
        Match.objectLike({ NoncurrentVersionExpiration: Match.objectLike({ NoncurrentDays: 30 }), Status: "Enabled" }),
      ]) },
    });
  });

  test("bucket policy denies non-TLS access", () => {
    const pol = Object.values(resourcesOf(t, "AWS::S3::BucketPolicy"));
    expect(pol).toHaveLength(1);
    const stmts = (pol[0].Properties as any).PolicyDocument.Statement;
    const tls = stmts.find((s: any) => s.Effect === "Deny" && JSON.stringify(s.Condition).includes("aws:SecureTransport"));
    expect(tls).toBeDefined();
    expect(JSON.stringify(tls.Condition)).toContain("false");
  });

  test("the invocation log group has its own rotating key: CloudWatch Logs for that group and the logging role only", () => {
    const keys = Object.entries(resourcesOf(t, "AWS::KMS::Key"));
    expect(keys).toHaveLength(1);
    const [keyId, key] = keys[0];
    const k = key.Properties as any;
    expect(k.EnableKeyRotation).toBe(true);
    expect(k.KeySpec ?? "SYMMETRIC_DEFAULT").toBe("SYMMETRIC_DEFAULT");
    const group = `arn:aws:logs:${REGION}:${ACCOUNT}:log-group:/hamh/direct/bedrock-invocations`;
    const stmts = k.KeyPolicy.Statement as any[];
    expect(stmts).toHaveLength(5);
    const root = stmts.find((s) => JSON.stringify(s.Principal).includes(":root"));
    expect(root.Principal).toEqual({ AWS: `arn:aws:iam::${ACCOUNT}:root` });
    const bySid = Object.fromEntries(stmts.map((s) => [s.Sid, s]));
    // Access Analyzer: kms:DescribeKey carries no encryption context
    // (UNSUPPORTED_ACTION_FOR_CONDITION_KEY), so it has its own statement per principal without that
    // condition, and the context key is a string key matched with StringEquals (TYPE_MISMATCH on ArnEquals).
    const logs = { Service: `logs.${REGION}.amazonaws.com` };
    expect(bySid.CloudWatchLogsForTheGroup).toEqual({
      Sid: "CloudWatchLogsForTheGroup", Effect: "Allow", Principal: logs, Resource: "*",
      Action: ["kms:Encrypt", "kms:Decrypt", "kms:ReEncrypt*", "kms:GenerateDataKey*"],
      Condition: { StringEquals: { "kms:EncryptionContext:aws:logs:arn": group } },
    });
    expect(bySid.CloudWatchLogsDescribesTheKey).toEqual({
      Sid: "CloudWatchLogsDescribesTheKey", Effect: "Allow", Principal: logs, Action: "kms:DescribeKey", Resource: "*",
    });
    // Bedrock writes as hamh-direct-bedrock-logging (security stack); CloudWatch Logs asks KMS for it.
    const writer = { AWS: `arn:aws:iam::${ACCOUNT}:role/hamh-direct-bedrock-logging` };
    expect(bySid.BedrockLoggingWritesThroughLogs).toEqual({
      Sid: "BedrockLoggingWritesThroughLogs", Effect: "Allow", Principal: writer, Resource: "*",
      Action: ["kms:Encrypt", "kms:Decrypt", "kms:GenerateDataKey*"],
      Condition: { StringEquals: { "kms:ViaService": `logs.${REGION}.amazonaws.com`, "kms:EncryptionContext:aws:logs:arn": group } },
    });
    expect(bySid.BedrockLoggingDescribesTheKeyThroughLogs).toEqual({
      Sid: "BedrockLoggingDescribesTheKeyThroughLogs", Effect: "Allow", Principal: writer, Action: "kms:DescribeKey", Resource: "*",
      Condition: { StringEquals: { "kms:ViaService": `logs.${REGION}.amazonaws.com` } },
    });
    for (const st of stmts.filter((x) => JSON.stringify(x.Condition ?? {}).includes("kms:EncryptionContext"))) {
      expect([st.Action].flat(), st.Sid).not.toContain("kms:DescribeKey");
      expect(Object.keys(st.Condition), st.Sid).toEqual(["StringEquals"]);
    }
    t.hasResourceProperties("AWS::KMS::Alias", { AliasName: "alias/hamh-direct-invocation-logs", TargetKeyId: { Ref: keyId } });
    t.hasResourceProperties("AWS::Logs::LogGroup", {
      LogGroupName: "/hamh/direct/bedrock-invocations", KmsKeyId: { "Fn::GetAtt": [keyId, "Arn"] },
    });
  });

  test("the Bedrock invocation log group exists with the stage retention", () => {
    t.hasResourceProperties("AWS::Logs::LogGroup", { LogGroupName: "/hamh/direct/bedrock-invocations", RetentionInDays: 30 });
  });
});
