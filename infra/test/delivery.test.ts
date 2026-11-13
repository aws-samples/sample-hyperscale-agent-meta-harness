import { readFileSync } from "node:fs";
import { Template } from "aws-cdk-lib/assertions";
import { describe, expect, test } from "vitest";
import { buildApp } from "../lib/app.ts";
import { resourcesOf, template } from "./helpers.ts";
import { ACCOUNT, OWNER, REGION } from "../lib/config.ts";
import { repositoryArns } from "./earlier-names.ts";

// Delivery stack:
// repo-gateway (the only git writer), bundle-validator and packager as arm64 container-image
// Lambdas from one delivery image (git + gitleaks), with least-privilege roles.
const REPO_URI = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-delivery`;
const IMAGE = `${REPO_URI}@sha256:${"a".repeat(64)}`;
// The repositories the gateway reaches: hamh-e2e-* and, for those a stage created before the rename, the earlier prefix (test/earlier-names.ts).
const E2E = repositoryArns(REGION, ACCOUNT);
const E2E_NEW = `arn:aws:codecommit:${REGION}:${ACCOUNT}:hamh-e2e-new-*`;
// The n-tier acceptance repositories hamh-e2e-ntier-<kind>-<yyyymmdd>.
const E2E_NTIER = `arn:aws:codecommit:${REGION}:${ACCOUNT}:hamh-e2e-ntier-*`;
const BUCKET = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
const FUNCTIONS = ["repo-gateway", "bundle-validator", "packager"];
/** repo-gateway -> RepoGateway: the logical id stem of each function, role, log group and output. */
const pascal = (n: string) => n.split("-").map((s) => s[0].toUpperCase() + s.slice(1)).join("");

function withImage(stage: "direct", image: string): Template {
  const app = buildApp({ stage, deliveryImage: image });
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const stack = app.node.children.find((c) => (c as any).stackName === `hamh-${stage}-delivery`);
  if (!stack) throw new Error("no delivery stack");
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  return Template.fromStack(stack as any);
}

type Stmt = { Sid?: string; Effect: string; Action: string | string[]; Resource: string | string[]; Condition?: Record<string, Record<string, unknown>> };

const list = (v: string | string[] | undefined): string[] => (v === undefined ? [] : Array.isArray(v) ? v : [v]);

function statements(t: Template, roleName: string): Stmt[] {
  const roles = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any).filter((p) => p.RoleName === roleName);
  expect(roles, roleName).toHaveLength(1);
  return roles[0].Policies.flatMap((p: any) => p.PolicyDocument.Statement);
}

const sid = (stmts: Stmt[], s: string): Stmt => {
  const found = stmts.find((x) => x.Sid === s);
  if (!found) throw new Error(`no statement ${s}: ${stmts.map((x) => x.Sid).join(", ")}`);
  return found;
};

const allowed = (stmts: Stmt[]) => stmts.filter((s) => s.Effect === "Allow").flatMap((s) => list(s.Action));

/** IAM's pattern match for a Resource or an Action: `*` is any run of characters (a `/` included), `?` is one character. */
const iamMatch = (pattern: string, value: string): boolean =>
  new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".")}$`).test(value);

/** Whether the policy lets the action reach the resource: an unconditional Allow that matches and no matching Deny. */
function iamAllows(stmts: Stmt[], action: string, arn: string): boolean {
  const hits = (s: Stmt) => list(s.Action).some((a) => iamMatch(a, action)) && list(s.Resource).some((r) => typeof r === "string" && iamMatch(r, arn));
  return stmts.some((s) => s.Effect === "Allow" && s.Condition === undefined && hits(s)) && !stmts.some((s) => s.Effect === "Deny" && hits(s));
}

const bucketArn = (stage: "direct") => `arn:aws:s3:::hamh-${stage}-harness-${ACCOUNT}`;

/** The S3 keys a delivery names and the delivery roles that must or must not reach each (see the fixture's comment). */
interface SourceKey { id: string; key: string; roles: string[]; producer: string }
const SOURCE_KEYS = JSON.parse(readFileSync(new URL("./fixtures/delivery-source-keys.json", import.meta.url), "utf8")) as {
  run: string; reads: SourceKey[]; writes: SourceKey[]; never: SourceKey[];
};

describe("delivery stack, phase 1 (no delivery image)", () => {
  const t = template("direct", "delivery");

  test("one immutable, scanned ECR repository for the delivery image and no function yet", () => {
    const repos = Object.values(resourcesOf(t, "AWS::ECR::Repository")).map((r) => r.Properties as any);
    expect(repos.map((r) => r.RepositoryName)).toEqual(["hamh-direct-delivery"]);
    expect(repos[0].ImageTagMutability).toBe("IMMUTABLE");
    expect(repos[0].ImageScanningConfiguration).toEqual({ ScanOnPush: true });
    t.resourceCountIs("AWS::Lambda::Function", 0);
    expect(Object.keys(t.findOutputs("DeliveryRepositoryUri"))).toHaveLength(1);
  });

  test("the three function roles exist before the functions, so their policies can be validated", () => {
    const names = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => (r.Properties as any).RoleName).sort();
    // The two PR-check roles (test/prcheck.test.ts) live in the same stack.
    expect(names).toEqual(["hamh-direct-bundle-validator", "hamh-direct-packager", "hamh-direct-prcheck-build", "hamh-direct-prcheck-events",
      "hamh-direct-repo-gateway"]);
    for (const n of FUNCTIONS.map((f) => `hamh-direct-${f}`)) {
      const role = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any).find((p) => p.RoleName === n);
      expect(role.AssumeRolePolicyDocument.Statement).toEqual([
        { Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" },
      ]);
    }
  });
});

describe("delivery stack, phase 2 (a digest-pinned delivery image)", () => {
  const t = withImage("direct", IMAGE);
  const fns = () => Object.values(resourcesOf(t, "AWS::Lambda::Function")).map((r) => r.Properties as any);

  test("three arm64 container-image functions, one binary each, from the one image", () => {
    expect(fns().map((f) => f.FunctionName).sort()).toEqual(FUNCTIONS.map((n) => `hamh-direct-${n}`).sort());
    for (const name of FUNCTIONS) {
      const f = fns().find((x) => x.FunctionName === `hamh-direct-${name}`);
      expect(f.PackageType).toBe("Image");
      expect(f.Code).toEqual({ ImageUri: IMAGE });
      expect(f.ImageConfig).toEqual({ EntryPoint: [`/opt/delivery/bin/${name}`] });
      expect(f.Architectures).toEqual(["arm64"]);
      expect(f.Runtime).toBeUndefined();
      expect(f.Timeout).toBe(900);
      expect(f.EphemeralStorage).toEqual({ Size: 10240 });
      expect(f.MemorySize).toBeGreaterThanOrEqual(1024);
      expect(f.TracingConfig).toEqual({ Mode: "Active" });
      expect(f.LoggingConfig.LogFormat).toBe("JSON");
      expect(f.Environment.Variables).toEqual({
        BUCKET_NAME: `hamh-direct-harness-${ACCOUNT}`, TABLE_NAME: "hamh-direct-harness", HAMH_STAGE: "direct", HAMH_OWNER: OWNER,
        HAMH_REPO_NAMES: "hamh-e2e-*", GIT_BIN: "/usr/bin/git", GITLEAKS_BIN: "/opt/delivery/bin/gitleaks", WORK_ROOT: "/tmp/delivery",
      });
      expect(f.Role).toEqual({ "Fn::GetAtt": [`${pascal(name)}Role`, "Arn"] });
    }
    const groups = Object.values(resourcesOf(t, "AWS::Logs::LogGroup")).map((r) => (r.Properties as any).LogGroupName)
      .filter((g: string) => g.startsWith("/aws/lambda/")).sort();
    expect(groups).toEqual(FUNCTIONS.map((n) => `/aws/lambda/hamh-direct-${n}`).sort());
    for (const name of FUNCTIONS) expect(Object.keys(t.findOutputs(`${pascal(name)}Arn`)), name).toHaveLength(1);
  });

  test("an image outside the delivery repository or not pinned by digest is refused", () => {
    expect(() => withImage("direct", `${REPO_URI}:latest`)).toThrow(/digest/);
    expect(() => withImage("direct", `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-worker-python@sha256:${"b".repeat(64)}`)).toThrow(/hamh-direct-delivery/);
  });

  test("each role pulls only the delivery image and writes only its own log group", () => {
    for (const name of FUNCTIONS) {
      const st = statements(t, `hamh-direct-${name}`);
      const pull = sid(st, "PullDeliveryImage");
      expect(list(pull.Action).sort()).toEqual(["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"]);
      expect(pull.Resource).toBe(`arn:aws:ecr:${REGION}:${ACCOUNT}:repository/hamh-direct-delivery`);
      const logs = sid(st, "OwnLogs");
      expect(list(logs.Action).sort()).toEqual(["logs:CreateLogStream", "logs:PutLogEvents"]);
      expect(JSON.stringify(logs.Resource)).toContain(`${pascal(name)}Logs`);
      // No Allow statement is unscoped except X-Ray (PutTraceSegments takes no resource).
      for (const s of st.filter((x) => x.Effect === "Allow" && x.Sid !== "Tracing")) {
        expect(list(s.Resource).every((r) => r !== "*"), `${name} ${s.Sid}`).toBe(true);
      }
      for (const a of allowed(st)) expect(a.endsWith(":*") || a === "*", `${name} ${a}`).toBe(false);
    }
  });
});

describe("the delivery repository lets the Lambda service re-activate an idle image function", () => {
  // Three container-image functions of a live stage went Inactive about nine hours after their last run.
  // Their roles allowed the pull, but the repository had no policy for the
  // Lambda service principal, which is the caller when Lambda pulls the image again after idle time. The policy names the three functions
  // and nothing else: without the condition, a function of any account could use the image through the service principal.
  const arn = (fn: string) => `arn:aws:lambda:${REGION}:${ACCOUNT}:function:${fn}`;
  const WANTED = FUNCTIONS.flatMap((n) => [arn(`hamh-direct-${n}`), `${arn(`hamh-direct-${n}`)}:*`]).sort();
  type RepoPolicy = { Version: string; Statement: Array<Record<string, unknown>> };
  const policyOf = (t: Template): RepoPolicy => {
    const repos = Object.values(resourcesOf(t, "AWS::ECR::Repository")).map((r) => r.Properties as Record<string, unknown>);
    expect(repos.map((r) => r.RepositoryName)).toEqual(["hamh-direct-delivery"]);
    expect(repos[0].RepositoryPolicyText, "the delivery repository has no policy for the Lambda service").toBeDefined();
    return repos[0].RepositoryPolicyText as RepoPolicy;
  };
  const sourceArns = (p: RepoPolicy): string[] =>
    list((p.Statement[0].Condition as { ArnLike: { "aws:SourceArn": string | string[] } }).ArnLike["aws:SourceArn"]);
  const phases: Array<[string, () => Template]> = [["phase 1", () => template("direct", "delivery")], ["phase 2", () => withImage("direct", IMAGE)]];

  for (const [phase, build] of phases) {
    test(`${phase}: one statement lets the Lambda service read the image of the three functions and nothing else`, () => {
      const p = policyOf(build());
      expect(p.Version).toBe("2012-10-17");
      expect(p.Statement).toHaveLength(1);
      const s = p.Statement[0];
      expect(Object.keys(s).sort()).toEqual(["Action", "Condition", "Effect", "Principal", "Sid"]);
      // Not the Sid Lambda gives its own automatic statement (LambdaECRImageRetrievalPolicy): a repository that holds both would carry the
      // same Sid twice, and an automatic statement is the broad `function:*` form this policy exists to avoid.
      expect(s.Sid).toBe("HamhLambdaServiceImagePull");
      expect(s.Effect).toBe("Allow");
      expect(s.Principal).toEqual({ Service: "lambda.amazonaws.com" });
      expect(list(s.Action as string[]).sort()).toEqual(["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"]);
      // ArnLike on aws:SourceArn, the operator IAM Access Analyzer asks for with an ARN key;
      // it matches the same ARNs.
      expect(Object.keys(s.Condition as object)).toEqual(["ArnLike"]);
      expect(Object.keys((s.Condition as { ArnLike: object }).ArnLike)).toEqual(["aws:SourceArn"]);
      expect([...sourceArns(p)].sort()).toEqual(WANTED);
    });
  }

  test("the image arrives in phase 2 with no change to the policy", () => {
    expect(policyOf(withImage("direct", IMAGE))).toEqual(policyOf(template("direct", "delivery")));
  });

  test("the patterns match the deployed functions (bare or with a qualifier) and no other function", () => {
    const p = policyOf(withImage("direct", IMAGE));
    const matches = (source: string) => sourceArns(p).some((pat) => iamMatch(pat, source));
    const deployed = Object.values(resourcesOf(withImage("direct", IMAGE), "AWS::Lambda::Function")).map((r) => (r.Properties as any).FunctionName as string);
    expect(deployed.sort()).toEqual(FUNCTIONS.map((n) => `hamh-direct-${n}`).sort());
    for (const fn of deployed) {
      for (const source of [arn(fn), `${arn(fn)}:$LATEST`, `${arn(fn)}:7`]) expect(matches(source), source).toBe(true);
    }
    for (const other of [arn("hamh-direct-run-session"), arn("hamh-direct-control-api"), arn("hamh-direct-packager-extra"), arn("hamh-direct-repo-gateway2"),
      arn("other-project-fn"), arn("*"),
      `arn:aws:lambda:${REGION}:${"1".repeat(12)}:function:hamh-direct-packager`, `arn:aws:lambda:us-west-1:${ACCOUNT}:function:hamh-direct-packager`]) {
      expect(matches(other), other).toBe(false);
    }
  });
});

describe("repo-gateway role", () => {
  const st = statements(template("direct", "delivery"), "hamh-direct-repo-gateway");

  test("reads hamh-e2e-* repositories and those of the earlier prefix only", () => {
    const s = sid(st, "ReadE2eRepositories");
    expect(list(s.Action).sort()).toEqual(["codecommit:GetRepository", "codecommit:GitPull"]);
    expect(s.Resource).toEqual(E2E);
    expect(s.Condition).toBeUndefined();
  });

  test("pushes only refs/heads/harness/* and is denied every other ref", () => {
    const push = sid(st, "PushHarnessBranches");
    expect(push.Action).toBe("codecommit:GitPush");
    expect(push.Resource).toEqual(E2E);
    expect(push.Condition).toEqual({ StringLikeIfExists: { "codecommit:References": ["refs/heads/harness/*"] } });
    const deny = sid(st, "DenyPushOutsideHarness");
    expect(deny.Effect).toBe("Deny");
    expect(deny.Action).toBe("codecommit:GitPush");
    expect(deny.Resource).toBe("*");
    expect(deny.Condition).toEqual({
      StringNotLikeIfExists: { "codecommit:References": ["refs/heads/harness/*"] },
      Null: { "codecommit:References": "false" },
    });
    expect(st.filter((s) => s.Effect === "Allow" && list(s.Action).includes("codecommit:GitPush"))).toHaveLength(1);
  });

  test("carries the explicit Deny on refs/heads/main writes and merges", () => {
    const deny = sid(st, "DenyMainWrites");
    expect(deny.Effect).toBe("Deny");
    expect(list(deny.Action).sort()).toEqual([
      "codecommit:CreateCommit", "codecommit:DeleteBranch", "codecommit:DeleteFile", "codecommit:GitPush",
      "codecommit:MergeBranchesBy*", "codecommit:MergePullRequestBy*", "codecommit:PutFile",
    ]);
    expect(deny.Resource).toBe("*");
    expect(deny.Condition).toEqual({
      StringEqualsIfExists: { "codecommit:References": ["refs/heads/main"] },
      Null: { "codecommit:References": "false" },
    });
  });

  test("opens pull requests with rules and comments and can never approve, override, edit rules, merge, delete or start a build", () => {
    const pr = sid(st, "PullRequests");
    expect(list(pr.Action).sort()).toEqual([
      "codecommit:CreatePullRequest", "codecommit:CreatePullRequestApprovalRule", "codecommit:GetPullRequest",
      "codecommit:PostCommentForPullRequest",
    ]);
    expect(pr.Resource).toEqual(E2E);
    const deny = sid(st, "NeverApproveMergeOrBuild");
    expect(deny.Effect).toBe("Deny");
    expect(list(deny.Action).sort()).toEqual([
      "codebuild:StartBuild", "codecommit:DeletePullRequestApprovalRule", "codecommit:DeleteRepository",
      "codecommit:MergeBranchesBy*", "codecommit:MergePullRequestBy*", "codecommit:OverridePullRequestApprovalRules",
      "codecommit:UpdatePullRequestApprovalRuleContent", "codecommit:UpdatePullRequestApprovalState",
    ]);
    expect(deny.Resource).toBe("*");
    for (const a of ["codecommit:UpdatePullRequestApprovalState", "codecommit:OverridePullRequestApprovalRules",
      "codecommit:DeleteRepository", "codebuild:StartBuild", "codecommit:MergePullRequestByFastForward"]) {
      expect(allowed(st), a).not.toContain(a);
    }
  });

  test("creates only hamh-e2e-new-* and hamh-e2e-ntier-* repositories, only with the four project tags in the request", () => {
    const s = sid(st, "CreateNewRepositories");
    expect(list(s.Action).sort()).toEqual(["codecommit:CreateRepository", "codecommit:TagResource"]);
    expect(list(s.Resource)).toEqual([E2E_NEW, E2E_NTIER]);
    expect(s.Condition).toEqual({
      StringEquals: {
        "aws:RequestTag/project": "hyperscale-agent-meta-harness", "aws:RequestTag/stage": "direct",
        "aws:RequestTag/owner": OWNER, "aws:RequestTag/managed-by": "repo-gateway",
      },
      "ForAllValues:StringEquals": { "aws:TagKeys": ["project", "stage", "owner", "managed-by"] },
    });
  });

  test("creates main only in a new repository, on the empty base (CreateBranch cannot move a branch)", () => {
    const s = sid(st, "NewRepositoryMain");
    expect(s.Action).toBe("codecommit:CreateBranch");
    expect(list(s.Resource)).toEqual([E2E_NEW, E2E_NTIER]);
    expect(s.Condition).toEqual({ StringEquals: { "codecommit:References": "refs/heads/main" } });
    const d = sid(st, "NewRepositoryDefaultBranch");
    expect(d.Action).toBe("codecommit:UpdateDefaultBranch");
    expect(list(d.Resource)).toEqual([E2E_NEW, E2E_NTIER]);
  });

  test("reads only allowlist items and touches only the run input bundle, the tree listing and the verified bundle", () => {
    const a = sid(st, "AllowlistRead");
    expect(a.Action).toBe("dynamodb:GetItem");
    expect(a.Resource).toBe(`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`);
    expect(a.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["REPOALLOW#*"] } });
    const put = sid(st, "StageInputs");
    expect(put.Action).toBe("s3:PutObject");
    // The base's.harness/modes.yaml text.
    expect(list(put.Resource).sort()).toEqual([`${BUCKET}/runs/*/input/repo-modes.yaml`, `${BUCKET}/runs/*/input/repo.bundle`,
      `${BUCKET}/runs/*/input/tree.json`]);
    const get = sid(st, "ReadVerifiedBundle");
    expect(get.Action).toBe("s3:GetObject");
    expect(get.Resource).toBe(`${BUCKET}/runs/*/deliverable/verified.bundle`);
    expect(allowed(st).filter((x) => x.startsWith("s3:")).sort()).toEqual(["s3:GetObject", "s3:PutObject"]);
    expect(allowed(st).filter((x) => x.startsWith("dynamodb:")).sort()).toEqual(["dynamodb:GetItem", "dynamodb:PutItem", "dynamodb:UpdateItem"]);
  });

  test("writes allowlist items only for the new-repo names it creates (the harness's own entries)", () => {
    const w = sid(st, "AllowlistNewRepositories");
    expect(w.Action).toBe("dynamodb:PutItem");
    expect(w.Resource).toBe(`arn:aws:dynamodb:${REGION}:${ACCOUNT}:table/hamh-direct-harness`);
    expect(w.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": [
      `REPOALLOW#cc:${REGION}:hamh-e2e-new-*`, `REPOALLOW#cc:${REGION}:hamh-e2e-ntier-*`,
    ] } });
    const i = sid(st, "AllowlistIndex");
    expect(i.Action).toBe("dynamodb:UpdateItem");
    expect(i.Condition).toEqual({ "ForAllValues:StringEquals": { "dynamodb:LeadingKeys": ["REPOALLOW#INDEX"] } });
  });
});

describe("bundle-validator and packager roles (no secrets, no CodeCommit)", () => {
  const t = template("direct", "delivery");

  test("neither role holds any CodeCommit or Secrets Manager action and both deny codecommit:*", () => {
    for (const name of ["bundle-validator", "packager"]) {
      const st = statements(t, `hamh-direct-${name}`);
      expect(allowed(st).filter((a) => a.startsWith("codecommit:") || a.startsWith("secretsmanager:")), name).toEqual([]);
      const deny = sid(st, "NoCodeCommit");
      expect(deny.Effect).toBe("Deny");
      expect(deny.Action).toBe("codecommit:*");
      expect(deny.Resource).toBe("*");
    }
  });

  test("bundle-validator reads turn bundles, the direct agent's snapshot bundle, the staged input bundle and the verified bundle and writes only the verified bundle", () => {
    const st = statements(t, "hamh-direct-bundle-validator");
    const get = sid(st, "ReadBundles");
    expect(get.Action).toBe("s3:GetObject");
    // The staged input bundle is the held head of a run that blocked before its first merge:
    // one exact key shape per run, the key repo-gateway's StageInputs writes.
    // The snapshot bundle is the held head of a direct run whose session ended with no final bundle:
    // the direct agent's key alone, not every agent's.
    expect(list(get.Resource).sort()).toEqual([
      `${BUCKET}/runs/*/agents/*/turns/*/bundle`, `${BUCKET}/runs/*/agents/direct/turns/*/snapshot.bundle`,
      `${BUCKET}/runs/*/deliverable/verified.bundle`, `${BUCKET}/runs/*/input/repo.bundle`,
    ]);
    const put = sid(st, "WriteVerifiedBundle");
    expect(put.Action).toBe("s3:PutObject");
    expect(put.Resource).toBe(`${BUCKET}/runs/*/deliverable/verified.bundle`);
    // s3:ListBucket is the third-party copy rule's listing of the run's stored research items, limited by prefix below.
    expect([...new Set(allowed(st).filter((a) => a.startsWith("s3:") || a.startsWith("dynamodb:")))].sort()).toEqual([
      "s3:GetObject", "s3:GetObjectVersion", "s3:ListBucket", "s3:PutObject"]);
  });

  // The validator reads the bundle version agentd recorded. GetObjectVersion is its own action on the direct agent's bundle key only.
  test("b759: bundle-validator reads one version of the direct agent's bundle and no other key by version", () => {
    const st = statements(t, "hamh-direct-bundle-validator");
    const v = sid(st, "ReadPinnedBundleVersions");
    expect(v.Effect).toBe("Allow");
    expect(v.Action).toBe("s3:GetObjectVersion");
    expect(v.Resource).toBe(`${BUCKET}/runs/*/agents/direct/turns/*/bundle`);
    expect(v.Condition).toBeUndefined();
    expect(st.filter((x) => [x.Action].flat().includes("s3:GetObjectVersion")).map((x) => x.Sid)).toEqual(["ReadPinnedBundleVersions"]);
    // The packager reads the verified copy only: no version read there.
    const pk = statements(t, "hamh-direct-packager");
    expect(pk.filter((x) => [x.Action].flat().includes("s3:GetObjectVersion"))).toEqual([]);
  });

  test("bundle-validator also writes the record of what a passing delivery changed, and only that key", () => {
    const st = statements(t, "hamh-direct-bundle-validator");
    const put = sid(st, "WriteChangeRecord");
    expect(put.Effect).toBe("Allow");
    expect(put.Action).toBe("s3:PutObject");
    expect(put.Resource).toBe(`${BUCKET}/runs/*/deliverable/changes.json`);
    // Its writes are the verified bundle and the record, nothing beside the zips.
    const writers = st.filter((x) => list(x.Action).includes("s3:PutObject"));
    expect(writers.map((x) => x.Sid).sort()).toEqual(["WriteChangeRecord", "WriteVerifiedBundle"]);
    // The packager's zip rule does not reach it, and the packager cannot overwrite it.
    const key = `${bucketArn("direct")}/runs/01M4HA6ZFWNP2PFSF6CBJ2MYC9/deliverable/changes.json`;
    expect(iamAllows(statements(t, "hamh-direct-packager"), "s3:PutObject", key)).toBe(false);
    expect(iamAllows(statements(t, "hamh-direct-repo-gateway"), "s3:PutObject", key)).toBe(false);
    expect(iamAllows(st, "s3:PutObject", key)).toBe(true);
  });

  test("packager reads the bundle, turn gate reports and evidence, writes only zips and ART items", () => {
    const st = statements(t, "hamh-direct-packager");
    const read = sid(st, "ReadBundleAndEvidence");
    expect(read.Action).toBe("s3:GetObject");
    // The keys gate-runner and the verifier write: gates-<mode>.json.
    // Never
    // gates-collect-hidden.json, whose report names the hidden tests. Turn bundles become the
    // bundle:<taskId> artifacts.
    // A clean verification re-run after a hidden-set revision writes gates-verify-r<k>.json; the goal step records
    // it with the task's other reports, so the package names it.
    expect(list(read.Resource).sort()).toEqual([
      `${BUCKET}/runs/*/agents/*/turns/*/bundle`,
      `${BUCKET}/runs/*/agents/*/turns/*/gates-red-check.json`, `${BUCKET}/runs/*/agents/*/turns/*/gates-session.json`,
      `${BUCKET}/runs/*/agents/*/turns/*/gates-verify-r*.json`,
      `${BUCKET}/runs/*/agents/*/turns/*/gates-verify.json`, `${BUCKET}/runs/*/deliverable/verified.bundle`, `${BUCKET}/runs/*/evidence/*`,
    ]);
    const ls = sid(st, "ListEvidence");
    expect(ls.Action).toBe("s3:ListBucket");
    expect(ls.Resource).toBe(BUCKET);
    expect(ls.Condition).toEqual({ StringLike: { "s3:prefix": ["runs/*/evidence/*"] } });
    const put = sid(st, "WriteDeliverableZips");
    expect(put.Action).toBe("s3:PutObject");
    // The zip, the partial zip and the evidence archive; the task bundles under bundles/.
    expect(list(put.Resource)).toEqual([`${BUCKET}/runs/*/deliverable/*.zip`, `${BUCKET}/runs/*/deliverable/bundles/*`]);
    const item = sid(st, "ArtifactItems");
    expect(list(item.Action).sort()).toEqual(["dynamodb:GetItem", "dynamodb:PutItem"]);
    expect(item.Condition).toEqual({ "ForAllValues:StringLike": { "dynamodb:LeadingKeys": ["RUN#*"] } });
  });

  test("the data key is usable only through S3 and DynamoDB", () => {
    for (const name of FUNCTIONS) {
      const st = statements(t, `hamh-direct-${name}`);
      const k = sid(st, "DataKeyThroughS3AndDynamoDb");
      expect(JSON.stringify(k.Condition)).toContain("kms:ViaService");
      expect(allowed(st).filter((a) => a.startsWith("kms:")).every((a) => ["kms:Decrypt", "kms:GenerateDataKey", "kms:DescribeKey"].includes(a))).toBe(true);
    }
  });
});

describe("the policy matcher", () => {
  test("a star spans slashes, a question mark is one character, every other character is literal", () => {
    expect(iamMatch("a/*/c", "a/b/c")).toBe(true);
    expect(iamMatch("a/*/c", "a/b/b2/c")).toBe(true);
    expect(iamMatch("a/*/c", "a/c")).toBe(false);
    expect(iamMatch("x/?.json", "x/1.json")).toBe(true);
    expect(iamMatch("x/?.json", "x/12.json")).toBe(false);
    expect(iamMatch("repo.bundle", "repo_bundle")).toBe(false);
    expect(iamMatch("runs/*/input/repo.bundle", "runs/R/input/repo.bundle.bak")).toBe(false);
    expect(iamMatch("a(b)+[c]", "a(b)+[c]")).toBe(true);
  });

  test("an Allow with a condition does not count and a matching Deny wins", () => {
    const s3 = (e: string, r: string, c?: Record<string, Record<string, unknown>>): Stmt => ({ Effect: e, Action: "s3:GetObject", Resource: r, Condition: c });
    expect(iamAllows([s3("Allow", "arn:x/*")], "s3:GetObject", "arn:x/k")).toBe(true);
    expect(iamAllows([s3("Allow", "arn:x/*", { StringEquals: { k: "v" } })], "s3:GetObject", "arn:x/k")).toBe(false);
    expect(iamAllows([s3("Allow", "arn:x/*"), s3("Deny", "arn:x/k")], "s3:GetObject", "arn:x/k")).toBe(false);
    expect(iamAllows([s3("Allow", "arn:x/*")], "s3:PutObject", "arn:x/k")).toBe(false);
  });
});

describe("the bundles a held delivery validates", () => {
  // The run blocked with two tasks and nothing merged: partial_plan held the base and named the staged input bundle as the
  // validator's sourceBundleUri; the validator role could not read it and ValidatePartial retried AccessDenied for 2 min 36 s.
  for (const stage of ["direct"] as const) {
    test(`${stage}: bundle-validator reads the staged input bundle of a base-held delivery and the turn bundle of a one-task head`, () => {
      const patterns = list(sid(statements(template(stage, "delivery"), `hamh-${stage}-bundle-validator`), "ReadBundles").Resource);
      for (const key of ["runs/01M4HA6ZFWNP2PFSF6CBJ2MYC9/input/repo.bundle", "runs/01M4HA6ZFWNP2PFSF6CBJ2MYC9/agents/core-a1/turns/4/bundle"]) {
        expect(patterns.some((p) => iamMatch(p, `${bucketArn(stage)}/${key}`)), `${stage}: ${key} against ${patterns.join(", ")}`).toBe(true);
      }
    });
  }
});

describe("the snapshot a direct run is delivered from", () => {
  // plan-delivery names turns/1/snapshot.bundle as the validator's sourceBundleUri when the session ended without a final bundle;
  // the validator was refused it (s3:GetObject), so the run ended blocked with the snapshot undelivered.
  const key = (agent: string, name: string) => `runs/01M4ZX2QEMB4JFCA411Q5169AY/agents/${agent}/turns/1/${name}`;
  for (const stage of ["direct"] as const) {
    const arn = (k: string) => `${bucketArn(stage)}/${k}`;
    const policy = (role: string) => statements(template(stage, "delivery"), `hamh-${stage}-${role}`);

    test(`${stage}: bundle-validator can read the direct agent's snapshot bundle and no other agent's`, () => {
      expect(iamAllows(policy("bundle-validator"), "s3:GetObject", arn(key("direct", "snapshot.bundle")))).toBe(true);
      expect(iamAllows(policy("bundle-validator"), "s3:GetObject", arn(key("core-a1", "snapshot.bundle")))).toBe(false);
    });

    test(`${stage}: bundle-validator cannot read the snapshot's other files and cannot write the snapshot`, () => {
      for (const other of ["snapshot.json", "summary.json", "stop-digest-1.json", "fresh-session-2.md"]) {
        expect(iamAllows(policy("bundle-validator"), "s3:GetObject", arn(key("direct", other))), other).toBe(false);
      }
      expect(iamAllows(policy("bundle-validator"), "s3:PutObject", arn(key("direct", "snapshot.bundle")))).toBe(false);
    });

    test(`${stage}: packager and repo-gateway never read the snapshot bundle`, () => {
      for (const role of ["packager", "repo-gateway"]) {
        expect(iamAllows(policy(role), "s3:GetObject", arn(key("direct", "snapshot.bundle"))), role).toBe(false);
      }
    });
  }
});

describe("every S3 key a delivery names is reachable by the role that uses it and by no other (fixtures/delivery-source-keys.json)", () => {
  for (const stage of ["direct"] as const) {
    const t = template(stage, "delivery");
    const policy = (role: string) => statements(t, `hamh-${stage}-${role}`);
    const arn = (key: string) => `${bucketArn(stage)}/${key}`;

    // Every miss is listed at once: a plan can name several keys and one failing run should show them all.
    const misses = (rows: SourceKey[], action: string, wanted: boolean): string[] =>
      rows.flatMap((k) => k.roles
        .filter((role) => iamAllows(policy(role), action, arn(k.key)) !== wanted)
        .map((role) => `${role} ${wanted ? "cannot" : "can"} ${action.slice(3)} ${k.id}: ${k.key} (${k.producer})`));

    test(`${stage}: each role reads every key its plan can name`, () => {
      expect(misses(SOURCE_KEYS.reads, "s3:GetObject", true)).toEqual([]);
    });

    test(`${stage}: each role writes the keys it publishes`, () => {
      expect(misses(SOURCE_KEYS.writes, "s3:PutObject", true)).toEqual([]);
    });

    test(`${stage}: hidden tests, their reports and the run request stay out of every delivery role`, () => {
      expect(misses(SOURCE_KEYS.never, "s3:GetObject", false)).toEqual([]);
    });
  }
});

describe("the research material the bundle validator checks a delivery against", () => {
  // The third-party copy rule (crates/delivery/src/third_party.rs) lists the items a run stored under
  // runs/<run>/agents/direct/research/items/ and reads each item's manifest and body. Without those two grants S3 answers AccessDenied to the
  // listing of every run, research or not, and the validation step blocks every delivery.
  const stage = "direct" as const;
  const RUN = "01M4XAB0CDEFGHJKMNPQRSTVWX";
  const research = (rest: string) => `runs/${RUN}/agents/direct/research/${rest}`;
  const arn = (k: string) => `${bucketArn(stage)}/${k}`;
  const policy = (role: string) => statements(template(stage, "delivery"), `hamh-${stage}-${role}`);
  /** Whether the policy lets s3:ListBucket on the bucket with this `prefix` parameter: an Allow whose s3:prefix StringLike values match it. */
  const listAllows = (stmts: Stmt[], prefix: string): boolean => stmts.some((x) => x.Effect === "Allow"
    && list(x.Action).some((a) => iamMatch(a, "s3:ListBucket")) && list(x.Resource).some((r) => iamMatch(r, bucketArn(stage)))
    && (x.Condition === undefined || list(x.Condition.StringLike?.["s3:prefix"] as string[] | undefined).some((pat) => iamMatch(pat, prefix))));

  test("bundle-validator lists the stored items of a run: one s3:ListBucket on the bucket, limited by s3:prefix to the research items", () => {
    const st = policy("bundle-validator");
    const listing = st.filter((x) => x.Effect === "Allow" && list(x.Action).includes("s3:ListBucket"));
    expect(listing).toHaveLength(1);
    expect(listing[0].Resource).toBe(BUCKET);
    expect(listing[0].Condition).toEqual({ StringLike: { "s3:prefix": ["runs/*/agents/direct/research/items/*"] } });
    expect(listAllows(st, research("items/"))).toBe(true);
  });

  test("bundle-validator lists nothing wider: not the bucket, a run, the direct agent, the research root, the audit or search objects, another agent's research", () => {
    const st = policy("bundle-validator");
    for (const prefix of ["", "runs/", `runs/${RUN}/`, `runs/${RUN}/agents/direct/`, research(""), research("audit/"), research("searches/"),
      `runs/${RUN}/agents/core-a1/research/items/`, `runs/${RUN}/evidence/`]) {
      expect(listAllows(st, prefix), `list ${prefix}`).toBe(false);
    }
  });

  test("bundle-validator reads the manifest and body of a stored item and no other object under the research prefix", () => {
    const st = policy("bundle-validator");
    for (const k of [research("items/i-0001/manifest.json"), research("items/i-0001/body.tar.gz"), research("items/i-0002/body.txt")]) {
      expect(iamAllows(st, "s3:GetObject", arn(k)), k).toBe(true);
    }
    for (const k of [research("audit/20261016T150000Z-req-1.json"), research("searches/s-0001.json"),
      `runs/${RUN}/agents/core-a1/research/items/i-0001/manifest.json`, `runs/${RUN}/agents/direct/turns/1/summary.json`]) {
      expect(iamAllows(st, "s3:GetObject", arn(k)), k).toBe(false);
    }
  });

  test("bundle-validator cannot write or delete under the research prefix, and no other delivery role lists or reads it", () => {
    const manifest = arn(research("items/i-0001/manifest.json"));
    for (const a of ["s3:PutObject", "s3:PutObjectTagging", "s3:DeleteObject"]) expect(iamAllows(policy("bundle-validator"), a, manifest), a).toBe(false);
    for (const role of ["packager", "repo-gateway"]) {
      expect(iamAllows(policy(role), "s3:GetObject", manifest), role).toBe(false);
      expect(listAllows(policy(role), research("items/")), role).toBe(false);
    }
  });

  test("the worker session role still cannot write there, and the Deny names no read or list action, so it cannot block the validator", () => {
    const policies = Object.values(resourcesOf(template(stage, "data"), "AWS::S3::BucketPolicy"));
    expect(policies).toHaveLength(1);
    const deny = (policies[0].Properties as { PolicyDocument: { Statement: Array<Stmt & { Principal?: unknown }> } }).PolicyDocument.Statement
      .find((x) => x.Sid === "WorkerSessionsCannotWriteResearchMaterial");
    expect(deny).toBeDefined();
    expect(deny!.Effect).toBe("Deny");
    expect(JSON.stringify(deny!.Resource)).toContain("/runs/*/agents/direct/research/*");
    expect(deny!.Condition).toEqual({ ArnEquals: { "aws:PrincipalArn": `arn:aws:iam::${ACCOUNT}:role/hamh-direct-harness-worker-session` } });
    expect(list(deny!.Action)).toContain("s3:PutObject");
    for (const a of list(deny!.Action)) expect(a).not.toMatch(/^s3:(Get|List)/);
  });
});
