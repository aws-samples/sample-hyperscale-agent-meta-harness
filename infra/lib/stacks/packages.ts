// Packages stack: one CodeArtifact domain with a
// store repository per ecosystem behind a single aggregate `packages` repository. Workers reach it
// through the codeartifact.api and codeartifact.repositories interface endpoints.
//
// Go has no CodeArtifact format (defect D11), so the same stack holds the Go module store: an S3 bucket
// per stage with an approved, create-only GOPROXY tree. The operator fills it as the filler role;
// workers only read it, anonymously and only through the stage's S3 gateway endpoint. No credential enters a microVM, the
// Network Firewall allowlist does not change, and the endpoint policy (`hamh-<stage>-*`) already admits the bucket.
import { CfnOutput, Stack, type StackProps } from "aws-cdk-lib";
import * as ca from "aws-cdk-lib/aws-codeartifact";
import * as iam from "aws-cdk-lib/aws-iam";
import * as s3 from "aws-cdk-lib/aws-s3";
import type { Construct } from "constructs";
import { ACCOUNT, GOMOD_INDEX_KEYS, GOMOD_RETENTION_DAYS, names, type StageConfig } from "../config.ts";
import type { NetworkStack } from "./network.ts";
import { deployingPrincipalArns, deployingTrust, type SecurityStack } from "./security.ts";

/** Store repository name -> public upstream (CodeArtifact external connection). */
export const STORE_REPOSITORIES: Readonly<Record<string, string>> = {
  "npm-store": "public:npmjs",
  "pypi-store": "public:pypi",
  "maven-store": "public:maven-central",
  "crates-store": "public:crates-io",
};

export interface PackagesStackProps extends StackProps {
  cfg: StageConfig;
  security: SecurityStack;
  /** The stage's network: the Go module store's bucket policy admits reads that come through its S3 gateway endpoint. */
  network: NetworkStack;
}

export class PackagesStack extends Stack {
  readonly domainName: string;
  readonly repositoryName = "packages";
  readonly gomodBucketName: string;
  readonly gomodProxyUrl: string;

  constructor(scope: Construct, id: string, props: PackagesStackProps) {
    super(scope, id, props);
    const { cfg, security, network } = props;
    const domain = new ca.CfnDomain(this, "Domain", { domainName: cfg.prefix, encryptionKey: security.dataKeyArn });
    this.domainName = cfg.prefix;

    const stores = Object.entries(STORE_REPOSITORIES).map(([name, upstream]) => {
      const repo = new ca.CfnRepository(this, `Repo-${name}`, {
        domainName: domain.attrName, repositoryName: name, externalConnections: [upstream],
        description: `${cfg.prefix} ${name}: cache of ${upstream}`,
      });
      repo.addResourceDependency(domain);
      return repo;
    });
    const packages = new ca.CfnRepository(this, "Repo-packages", {
      domainName: domain.attrName, repositoryName: this.repositoryName,
      upstreams: Object.keys(STORE_REPOSITORIES),
      description: `${cfg.prefix} packages: the one repository workers resolve through`,
    });
    for (const r of stores) packages.addResourceDependency(r);

    this.gomodBucketName = names.gomodBucket(cfg);
    this.gomodProxyUrl = names.gomodProxyUrl(cfg);
    this.goModuleStore(cfg, network);
  }

  /**
   * The Go module store. A bucket policy with Principal "*" and `aws:SourceVpce` is accepted under all four Block
   * Public Access flags (IsPublic false). An anonymous read through the gateway endpoint answers 200 (404 for a missing key,
   * because ListBucket is allowed too), a read from outside the VPC answers 403, and `go mod tidy` resolves against the https
   * bucket URL. S3 also accepts `s3:if-none-match` and `s3:if-match` as policy condition keys, which is how the tree is held
   * create-only.
   */
  private goModuleStore(cfg: StageConfig, network: NetworkStack): void {
    const name = names.gomodBucket(cfg);
    const arn = `arn:aws:s3:::${name}`;
    const objects = `${arn}/*`;
    const indexes = GOMOD_INDEX_KEYS.map((k) => `${arn}/${k}`);
    const filler = `arn:aws:iam::${ACCOUNT}:role/${names.gomodFillerRole(cfg)}`;
    const viaGateway = { StringEquals: { "aws:SourceVpce": [network.s3EndpointId] } };

    const bucket = new s3.CfnBucket(this, "GoModuleStore", {
      bucketName: name,
      // SSE-S3, never KMS: an anonymous reader cannot decrypt an SSE-KMS object. The bytes are public modules.
      bucketEncryption: { serverSideEncryptionConfiguration: [{ serverSideEncryptionByDefault: { sseAlgorithm: "AES256" } }] },
      versioningConfiguration: { status: "Enabled" },
      publicAccessBlockConfiguration: { blockPublicAcls: true, blockPublicPolicy: true, ignorePublicAcls: true, restrictPublicBuckets: true },
      ownershipControls: { rules: [{ objectOwnership: "BucketOwnerEnforced" }] },
      objectLockEnabled: true,
      objectLockConfiguration: { objectLockEnabled: "Enabled", rule: { defaultRetention: { mode: "GOVERNANCE", days: GOMOD_RETENTION_DAYS } } },
      // Nothing expires: a module version stays for as long as the stage does. Only unfinished uploads are cleaned.
      lifecycleConfiguration: { rules: [{ id: "abort-incomplete-uploads", status: "Enabled", abortIncompleteMultipartUpload: { daysAfterInitiation: 1 } }] },
    });
    const policy = new s3.CfnBucketPolicy(this, "GoModuleStorePolicy", {
      bucket: bucket.ref,
      policyDocument: {
        Version: "2012-10-17",
        Statement: [
          { Sid: "DenyInsecureTransport", Effect: "Deny", Principal: "*", Action: "s3:*", Resource: [arn, objects],
            Condition: { Bool: { "aws:SecureTransport": "false" } } },
          // Anonymous reads of public module bytes, only for requests that came through this stage's S3 gateway endpoint.
          // Block Public Access treats a fixed aws:SourceVpce as non-public. ListBucket makes a missing key a 404 that go reads
          // as "not in the store" (an agent error) instead of a 403 the lock step would call an infrastructure fault.
          { Sid: "ReadFromTheWorkerVpc", Effect: "Allow", Principal: "*", Action: "s3:GetObject", Resource: objects, Condition: viaGateway },
          { Sid: "ListFromTheWorkerVpc", Effect: "Allow", Principal: "*", Action: "s3:ListBucket", Resource: arn, Condition: viaGateway },
          // Every object is create-only. The two index files per module are derived from the versions that have a zip and
          // change by compare-and-swap (If-Match with the ETag just read, or If-None-Match for a new module), so no
          // unconditional overwrite exists and two fills cannot lose each other's update.
          { Sid: "DenyPutWithoutIfNoneMatch", Effect: "Deny", Principal: "*", Action: "s3:PutObject", NotResource: indexes,
            Condition: { Null: { "s3:if-none-match": "true" } } },
          { Sid: "DenyIndexPutWithoutCondition", Effect: "Deny", Principal: "*", Action: "s3:PutObject", Resource: indexes,
            Condition: { Null: { "s3:if-none-match": "true", "s3:if-match": "true" } } },
          { Sid: "DenyEncryptionOtherThanSseS3", Effect: "Deny", Principal: "*", Action: "s3:PutObject", Resource: objects,
            Condition: { StringNotEquals: { "s3:x-amz-server-side-encryption": "AES256" }, Null: { "s3:x-amz-server-side-encryption": "false" } } },
          { Sid: "DenyCustomerKeyEncryption", Effect: "Deny", Principal: "*", Action: "s3:PutObject", Resource: objects,
            Condition: { Null: { "s3:x-amz-server-side-encryption-customer-algorithm": "false" } } },
          // Writes: the filler role only, and it changes no lock. Deletes, retention, legal hold and the governance bypass that teardown
          // needs: the deploying principal only (the IAM role Admin, or the accounts.json entry's operator and the setup runner's role).
          { Sid: "DenyWritesFromOtherPrincipals", Effect: "Deny", Principal: "*", Resource: objects,
            Action: ["s3:PutObject", "s3:PutObjectTagging", "s3:PutObjectVersionTagging"],
            Condition: { ArnNotLike: { "aws:PrincipalArn": [filler] } } },
          { Sid: "DenyDeletesFromOtherPrincipals", Effect: "Deny", Principal: "*", Resource: objects,
            Action: ["s3:DeleteObject", "s3:DeleteObjectVersion", "s3:DeleteObjectTagging", "s3:DeleteObjectVersionTagging", "s3:BypassGovernanceRetention",
              "s3:PutObjectRetention", "s3:PutObjectLegalHold"],
            Condition: { ArnNotLike: { "aws:PrincipalArn": deployingPrincipalArns({ runner: true }) } } },
          // A put cannot escalate the lock for anyone: a COMPLIANCE lock or a retention past the default would
          // make teardown impossible (invariant 7).
          { Sid: "DenyComplianceLock", Effect: "Deny", Principal: "*", Action: ["s3:PutObject", "s3:PutObjectRetention"], Resource: objects,
            Condition: { StringEquals: { "s3:object-lock-mode": "COMPLIANCE" } } },
          { Sid: "DenyRetentionBeyondTheDefault", Effect: "Deny", Principal: "*", Action: ["s3:PutObject", "s3:PutObjectRetention"], Resource: objects,
            Condition: { NumericGreaterThan: { "s3:object-lock-remaining-retention-days": GOMOD_RETENTION_DAYS } } },
        ],
      },
    });
    // cfn_nag F16 (wildcard principal): the Deny statements use it to apply to everyone; the two Allow statements are anonymous reads
    // of public module bytes that the go tool makes without credentials, and each requires aws:SourceVpce to equal this stage's gateway endpoint.
    policy.addMetadata("cfn_nag", { rules_to_suppress: [{
      id: "F16",
      reason: "Wildcard principal in two Allow statements (s3:GetObject, s3:ListBucket) that require aws:SourceVpce to equal this stage's S3 gateway endpoint, for anonymous reads of public Go module bytes by the go tool in the worker VM. Block Public Access is on for all four settings, so the bucket is not public. The other wildcard-principal statements are Deny statements.",
    }] });

    // The operator's role, never a user or a key: the deploying principal assumes it (its sessions carry a source identity,
    // so the trust allows sts:SetSourceIdentity, as for the E2E second principal). It writes create-only and reads back; it
    // cannot delete, change retention or bypass governance, and no other role of the app names the bucket.
    new iam.CfnRole(this, "GoModuleFillerRole", {
      roleName: names.gomodFillerRole(cfg), maxSessionDuration: 3600,
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{ Sid: "DeployingPrincipalOnly", Effect: "Allow", ...deployingTrust({ runner: true }),
          Action: ["sts:AssumeRole", "sts:SetSourceIdentity"] }],
      },
      policies: [{
        policyName: "gomod-filler",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            { Sid: "PutAndReadStoreObjects", Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: objects },
            { Sid: "ListTheStore", Effect: "Allow", Action: "s3:ListBucket", Resource: arn },
          ],
        },
      }],
    });

    new CfnOutput(this, "GomodBucketName", { value: name, description: "the Go module store bucket" });
    new CfnOutput(this, "GomodProxyUrl", { value: names.gomodProxyUrl(cfg), description: "GOPROXY for the workers of this stage" });
  }
}
