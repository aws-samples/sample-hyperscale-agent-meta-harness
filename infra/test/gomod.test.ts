import { describe, expect, test } from "vitest";
import { ACCOUNT, OWNER, REGION } from "../lib/config.ts";
import { resourcesOf, synth, template } from "./helpers.ts";

// The Go module store. A Go stack needs a module source (GOPROXY protocol) that no gate has: the
// worker VPC reaches no public proxy and CodeArtifact has no Go format. The source is an approved, create-only, static
// GOPROXY tree in an S3 bucket per stage. An operator fills it; workers only read it, anonymously
// and only through the stage's own S3 gateway endpoint, so no credential enters an agent microVM and the egress
// allowlist does not change.
const STAGES = ["direct"] as const;
type Any = any; // eslint-disable-line @typescript-eslint/no-explicit-any
const bucket = (s: string) => `hamh-${s}-gomod-${ACCOUNT}`;
const proxyUrl = (s: string) => `https://${bucket(s)}.s3.${REGION}.amazonaws.com`;
const ADMIN = `arn:aws:iam::${ACCOUNT}:role/Admin`;
const filler = (s: string) => `arn:aws:iam::${ACCOUNT}:role/hamh-${s}-gomod-filler`;
const arr = (x: unknown): string[] => (x === undefined ? [] : [x].flat() as string[]);

function storeBucket(stage: string) {
  const t = template(stage as "direct", "packages");
  const found = Object.entries(resourcesOf(t, "AWS::S3::Bucket")).filter(([, r]) => (r.Properties as Any).BucketName === bucket(stage));
  expect(found.length, `the ${bucket(stage)} bucket in the ${stage} packages stack`).toBe(1);
  const [id, res] = found[0];
  const policies = Object.values(resourcesOf(t, "AWS::S3::BucketPolicy")).map((p) => p.Properties as Any)
    .filter((p) => JSON.stringify(p.Bucket).includes(id));
  return { t, id, props: res.Properties as Any, policy: policies[0] as Any, policies };
}

const statements = (policy: Any): Any[] => policy?.PolicyDocument?.Statement ?? [];
const sid = (policy: Any, name: string): Any => {
  const s = statements(policy).find((x) => x.Sid === name);
  expect(s, `statement ${name}`).toBeDefined();
  return s;
};

describe.each(STAGES)("Go module store bucket, %s stage", (stage) => {
  test("the bucket is named hamh-<stage>-gomod-<account>, private, versioned and encrypted with SSE-S3", () => {
    const { props } = storeBucket(stage);
    expect(props.VersioningConfiguration).toEqual({ Status: "Enabled" });
    expect(props.PublicAccessBlockConfiguration).toEqual({
      BlockPublicAcls: true, BlockPublicPolicy: true, IgnorePublicAcls: true, RestrictPublicBuckets: true });
    expect(props.OwnershipControls.Rules).toEqual([{ ObjectOwnership: "BucketOwnerEnforced" }]);
    // An anonymous reader cannot decrypt an SSE-KMS object: the store is SSE-S3 (public module bytes, no secret).
    const sse = props.BucketEncryption.ServerSideEncryptionConfiguration;
    expect(sse).toHaveLength(1);
    expect(sse[0].ServerSideEncryptionByDefault).toEqual({ SSEAlgorithm: "AES256" });
    expect(JSON.stringify(props.BucketEncryption)).not.toContain("kms");
  });

  test("Object Lock in governance mode keeps every stored version (teardown can bypass it, invariant 7)", () => {
    const { props } = storeBucket(stage);
    expect(props.ObjectLockEnabled).toBe(true);
    expect(props.ObjectLockConfiguration).toEqual({
      ObjectLockEnabled: "Enabled", Rule: { DefaultRetention: { Mode: "GOVERNANCE", Days: 365 } } });
  });

  test("nothing in the store ever expires: the only lifecycle rule aborts incomplete multipart uploads", () => {
    const { props } = storeBucket(stage);
    const rules: Any[] = props.LifecycleConfiguration?.Rules ?? [];
    expect(rules.length).toBeGreaterThan(0);
    for (const r of rules) {
      expect(r.ExpirationInDays, r.Id).toBeUndefined();
      expect(r.NoncurrentVersionExpiration, r.Id).toBeUndefined();
      expect(r.ExpiredObjectDeleteMarker, r.Id).toBeUndefined();
      expect(r.AbortIncompleteMultipartUpload, r.Id).toBeDefined();
    }
  });

  test("the bucket carries the four project tags", () => {
    const { props } = storeBucket(stage);
    const tags = Object.fromEntries((props.Tags as Any[]).map((x) => [x.Key, x.Value]));
    expect(tags).toMatchObject({ project: "hyperscale-agent-meta-harness", stage, owner: OWNER, "managed-by": "cdk" });
  });

  test("reads: anonymous, GetObject and ListBucket only, and only through this stage's S3 gateway endpoint", () => {
    const { policy } = storeBucket(stage);
    const allows = statements(policy).filter((s) => s.Effect === "Allow");
    expect(allows.length).toBeGreaterThan(0);
    const net = synth(stage).templates.get(`hamh-${stage}-network`)!;
    const gateway = Object.entries(resourcesOf(net, "AWS::EC2::VPCEndpoint")).filter(([, e]) => (e.Properties as Any).VpcEndpointType === "Gateway");
    expect(gateway).toHaveLength(1);
    const actions = new Set<string>();
    for (const s of allows) {
      expect(s.Principal, s.Sid).toBe("*");
      for (const a of arr(s.Action)) actions.add(a);
      // The one condition that keeps a Principal "*" statement private: the request came through the endpoint.
      expect(Object.keys(s.Condition ?? {}), s.Sid).toEqual(["StringEquals"]);
      expect(Object.keys(s.Condition.StringEquals), s.Sid).toEqual(["aws:SourceVpce"]);
      expect(JSON.stringify(s.Condition.StringEquals["aws:SourceVpce"]), s.Sid).toContain(gateway[0][0]);
    }
    expect([...actions].sort()).toEqual(["s3:GetObject", "s3:ListBucket"]);
    // GetObject names the objects and ListBucket the bucket (ListBucket makes a missing key a 404 that go treats
    // as "not in the store", instead of a 403 it reports as an infrastructure fault).
    const get = allows.find((s) => arr(s.Action).includes("s3:GetObject"))!;
    const list = allows.find((s) => arr(s.Action).includes("s3:ListBucket"))!;
    expect(JSON.stringify(get.Resource)).toContain("/*");
    expect(JSON.stringify(list.Resource)).not.toContain("/*");
  });

  test("every object is create-only: a put without If-None-Match is denied, except the two index files", () => {
    const { policy } = storeBucket(stage);
    const s = sid(policy, "DenyPutWithoutIfNoneMatch");
    expect(s.Effect).toBe("Deny");
    expect(s.Principal).toBe("*");
    expect(arr(s.Action)).toEqual(["s3:PutObject"]);
    expect(s.Condition).toEqual({ Null: { "s3:if-none-match": "true" } });
    // `list` and `@latest` are derived indexes that must change when a version is added; every version stays.
    expect(s.Resource).toBeUndefined();
    const notResource = arr(s.NotResource).map((r) => JSON.stringify(r)).join(" ");
    expect(notResource).toContain("/*/@v/list");
    expect(notResource).toContain("/*/@latest");
    expect(arr(s.NotResource)).toHaveLength(2);
  });

  test("the index files change only by compare-and-swap: a put needs If-Match or If-None-Match (no unconditional write exists)", () => {
    // Measured live (acct-a01, 2026-10-03): the s3:if-match condition key is accepted; a put with the right ETag writes,
    // a stale ETag answers 412, a plain put is denied. Two fills that run at once cannot lose an update.
    const { policy } = storeBucket(stage);
    const s = sid(policy, "DenyIndexPutWithoutCondition");
    expect(s.Effect).toBe("Deny");
    expect(s.Principal).toBe("*");
    expect(arr(s.Action)).toEqual(["s3:PutObject"]);
    expect(s.Condition).toEqual({ Null: { "s3:if-none-match": "true", "s3:if-match": "true" } });
    expect(s.NotResource).toBeUndefined();
    const same = (x: unknown) => arr(x).map((r) => JSON.stringify(r).replace(/^.*(\/\*\/@(?:v\/list|latest)).*$/, "$1")).sort();
    expect(same(s.Resource)).toEqual(["/*/@latest", "/*/@v/list"]);
    expect(same(sid(policy, "DenyPutWithoutIfNoneMatch").NotResource)).toEqual(same(s.Resource));
  });

  test("only SSE-S3 is accepted on a put (no KMS key, no customer key): an anonymous reader must be able to read it", () => {
    const { policy } = storeBucket(stage);
    const kms = sid(policy, "DenyEncryptionOtherThanSseS3");
    expect(kms.Effect).toBe("Deny");
    expect(arr(kms.Action)).toEqual(["s3:PutObject"]);
    expect(kms.Condition).toEqual({
      StringNotEquals: { "s3:x-amz-server-side-encryption": "AES256" },
      Null: { "s3:x-amz-server-side-encryption": "false" },
    });
    const c = sid(policy, "DenyCustomerKeyEncryption");
    expect(c.Condition).toEqual({ Null: { "s3:x-amz-server-side-encryption-customer-algorithm": "false" } });
  });

  test("writes belong to the filler role alone; deletes, retention and the governance bypass to the deploying Admin role alone", () => {
    const { policy } = storeBucket(stage);
    const w = sid(policy, "DenyWritesFromOtherPrincipals");
    expect(w.Effect).toBe("Deny");
    expect(w.Principal).toBe("*");
    expect(arr(w.Action).sort()).toEqual(["s3:PutObject", "s3:PutObjectTagging", "s3:PutObjectVersionTagging"]);
    expect(w.Condition).toEqual({ ArnNotLike: { "aws:PrincipalArn": [filler(stage)] } });
    // The filler writes content; it never changes a lock. Only Admin (teardown) touches retention, legal hold and the bypass.
    const d = sid(policy, "DenyDeletesFromOtherPrincipals");
    expect(d.Effect).toBe("Deny");
    for (const a of ["s3:DeleteObject", "s3:DeleteObjectVersion", "s3:BypassGovernanceRetention", "s3:PutObjectRetention", "s3:PutObjectLegalHold"]) {
      expect(arr(d.Action), a).toContain(a);
    }
    expect(d.Condition).toEqual({ ArnNotLike: { "aws:PrincipalArn": [ADMIN] } });
  });

  test("a put cannot escalate the lock: no COMPLIANCE mode and no retention past the default, for any principal", () => {
    // Opus 5.5 (2026-10-03): a writer could send x-amz-object-lock-mode COMPLIANCE with a far-future date and make teardown
    // impossible; the filler role also lacks s3:PutObjectRetention, this is the second lock.
    const { policy } = storeBucket(stage);
    const c = sid(policy, "DenyComplianceLock");
    expect(c.Effect).toBe("Deny");
    expect(c.Principal).toBe("*");
    expect(arr(c.Action).sort()).toEqual(["s3:PutObject", "s3:PutObjectRetention"]);
    expect(c.Condition).toEqual({ StringEquals: { "s3:object-lock-mode": "COMPLIANCE" } });
    const r = sid(policy, "DenyRetentionBeyondTheDefault");
    expect(r.Effect).toBe("Deny");
    expect(arr(r.Action).sort()).toEqual(["s3:PutObject", "s3:PutObjectRetention"]);
    expect(r.Condition).toEqual({ NumericGreaterThan: { "s3:object-lock-remaining-retention-days": 365 } });
  });

  test("a plain-HTTP request is denied", () => {
    const { policy } = storeBucket(stage);
    const s = sid(policy, "DenyInsecureTransport");
    expect(s.Effect).toBe("Deny");
    expect(arr(s.Action)).toEqual(["s3:*"]);
    expect(s.Condition).toEqual({ Bool: { "aws:SecureTransport": "false" } });
  });

  test("no statement allows a write or a delete, and none allows anyone but the endpoint's readers", () => {
    const { policy } = storeBucket(stage);
    for (const s of statements(policy).filter((x) => x.Effect === "Allow")) {
      for (const a of arr(s.Action)) expect(a, s.Sid).toMatch(/^s3:(GetObject|ListBucket)$/);
    }
  });

  test("the stage outputs name the bucket and the GOPROXY URL", () => {
    const t = template(stage, "packages");
    const out = t.toJSON().Outputs ?? {};
    expect(out.GomodBucketName?.Value).toBe(bucket(stage));
    expect(out.GomodProxyUrl?.Value).toBe(proxyUrl(stage));
  });

  test("the stage's S3 gateway endpoint policy already admits the bucket and its objects (no network change)", () => {
    const net = synth(stage).templates.get(`hamh-${stage}-network`)!;
    const ep = Object.values(resourcesOf(net, "AWS::EC2::VPCEndpoint")).map((e) => e.Properties as Any)
      .find((e) => e.VpcEndpointType === "Gateway");
    const glob = (pattern: string) => new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*")}$`);
    const admits = (arn: string, action: string) => ep.PolicyDocument.Statement.some((st: Any) => st.Effect === "Allow"
      && arr(st.Action).some((a) => a === "s3:*" || a === action) && arr(st.Resource).some((r) => glob(r).test(arn)));
    expect(admits(`arn:aws:s3:::${bucket(stage)}/github.com/lib/pq/@v/list`, "s3:GetObject")).toBe(true);
    expect(admits(`arn:aws:s3:::${bucket(stage)}`, "s3:ListBucket")).toBe(true);
    for (const retired of ["live", "drill"]) {
      expect(admits(`arn:aws:s3:::${bucket(retired)}/x`, "s3:GetObject"), `the ${retired} stage's store`).toBe(false);
    }
  });
});

describe.each(STAGES)("Go module store filler role, %s stage", (stage) => {
  const role = () => {
    const t = template(stage, "packages");
    const r = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((x) => x.Properties as Any)
      .find((x) => x.RoleName === `hamh-${stage}-gomod-filler`);
    expect(r, "the filler role").toBeDefined();
    return r as Any;
  };

  test("only the deploying Admin principal can assume it (it is a role, never a user or a key)", () => {
    const trust = role().AssumeRolePolicyDocument.Statement;
    expect(trust).toHaveLength(1);
    expect(trust[0].Effect).toBe("Allow");
    expect(trust[0].Principal).toEqual({ AWS: ADMIN });
    // The Admin session carries a source identity into the chain (as for the E2E second principal).
    expect(arr(trust[0].Action).sort()).toEqual(["sts:AssumeRole", "sts:SetSourceIdentity"]);
    expect(role().MaxSessionDuration).toBe(3600);
  });

  test("it may put, get and list in its own store and nothing else: no delete, no retention, no bypass", () => {
    const stmts: Any[] = role().Policies.flatMap((p: Any) => p.PolicyDocument.Statement);
    const allowed = new Set<string>();
    for (const s of stmts) {
      expect(s.Effect).toBe("Allow");
      for (const a of arr(s.Action)) allowed.add(a);
      for (const r of arr(s.Resource)) expect(r, "resource").toMatch(new RegExp(`^arn:aws:s3:::${bucket(stage)}(/\\*)?$`));
    }
    expect([...allowed].sort()).toEqual(["s3:GetObject", "s3:ListBucket", "s3:PutObject"]);
  });

  test("no other role of any stack can read or write the store", () => {
    const { templates } = synth(stage);
    for (const [name, t] of templates) {
      for (const [id, r] of Object.entries(resourcesOf(t, "AWS::IAM::Role"))) {
        const props = r.Properties as Any;
        if (props.RoleName === `hamh-${stage}-gomod-filler`) continue;
        expect(JSON.stringify(props.Policies ?? []), `${name}/${id}`).not.toContain("gomod");
      }
      for (const [id, r] of Object.entries(resourcesOf(t, "AWS::IAM::Policy"))) {
        expect(JSON.stringify((r.Properties as Any).PolicyDocument), `${name}/${id}`).not.toContain("gomod");
      }
    }
  });
});

describe("Go module source in the environments", () => {
  const img = (stage: string, repo: string) => `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-${stage}-${repo}@sha256:${"a".repeat(64)}`;
  const images = (stage: string) => ({ workerImage: img(stage, "worker-python"), deliveryImage: img(stage, "delivery") });
  const runtimes = (stage: "direct") =>
    Object.values(resourcesOf(synth(stage, images(stage)).templates.get(`hamh-${stage}-runtimes`)!, "AWS::BedrockAgentCore::Runtime")).map((r) => r.Properties as Any);

  test.each(STAGES)("%s: the worker runtime names the stage's module store as AGENTD_GOPROXY", (stage) => {
    const workers = runtimes(stage);
    expect(workers).toHaveLength(1);
    for (const w of workers) {
      expect(w.EnvironmentVariables.AGENTD_GOPROXY, w.AgentRuntimeName).toBe(proxyUrl(stage));
      expect(w.EnvironmentVariables, w.AgentRuntimeName).not.toHaveProperty("HARNESS_GATES_GOPROXY");
    }
  });

  test.each(STAGES)("%s: the requireMMDSV2 update re-sends the module source with the rest of a runtime's environment", (stage) => {
    const t = synth(stage, images(stage)).templates.get(`hamh-${stage}-runtimes`)!;
    const crs = Object.values(t.findResources("Custom::AWS")).map((r: Any) => JSON.stringify(r.Properties));
    const withWorker = crs.filter((c) => c.includes("AGENTD_PACKAGES_DOMAIN"));
    expect(withWorker.length).toBe(1);
    for (const c of withWorker) expect(c).toContain(`\\"AGENTD_GOPROXY\\":\\"${proxyUrl(stage)}\\"`);
  });

  test.each(STAGES)("%s: the runtimes stack deploys after the packages stack that holds the store it names", (stage) => {
    // The runtimes stack names the bucket by convention (no CloudFormation reference), so the cloud assembly needs the edge that
    // scripts/lib/deploy.py orders by: a worker that ran before the bucket existed would read NoSuchBucket as "not in the store".
    const asm = synth(stage, images(stage)).app.synth();
    expect(asm.getStackByName(`hamh-${stage}-runtimes`).dependencies.map((d) => d.id)).toContain(`hamh-${stage}-packages`);
  });

  test("the worker VPC egress allowlist did not change (the module source is read through the S3 gateway endpoint)", () => {
    for (const stage of STAGES) {
      const net = synth(stage).templates.get(`hamh-${stage}-network`)!;
      const groups = Object.values(resourcesOf(net, "AWS::NetworkFirewall::RuleGroup"));
      const targets: string[] = (groups[0].Properties as Any).RuleGroup.RulesSource.RulesSourceList.Targets;
      for (const bad of ["golang.org", "goproxy", "github.com", "amazonaws.com/hamh-", "s3.", "gomod"]) {
        expect(targets.some((d) => d.includes(bad)), `${stage}: ${bad}`).toBe(false);
      }
    }
  });
});
