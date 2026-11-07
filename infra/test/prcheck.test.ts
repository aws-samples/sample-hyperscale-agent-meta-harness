import { describe, expect, test } from "vitest";
import { resourcesOf, synth, template } from "./helpers.ts";
import { ACCOUNT, REGION } from "../lib/config.ts";
import { repositoryArns } from "./earlier-names.ts";

// The CodeCommit PR check, in the delivery stack so the
// app keeps its nine stacks: CodeBuild ARM project hamh-<stage>-prcheck, started only by
// EventBridge rule hamh-<stage>-prcheck through role hamh-<stage>-prcheck-events, running the PR
// source commit's root buildspec.yml with the oracle tests as a secondary source, as build role
// hamh-<stage>-prcheck-build, the one pool member of hamh-prcheck-required.
// hamh-e2e-* and the earlier prefix of the repositories a stage created before the rename (test/earlier-names.ts).
const E2E = repositoryArns(REGION, ACCOUNT);
const PROJECT_ARN = `arn:aws:codebuild:${REGION}:${ACCOUNT}:project/hamh-direct-prcheck`;

type Stmt = { Sid?: string; Effect: string; Action: string | string[]; Resource: unknown; Condition?: Record<string, Record<string, unknown>> };
const list = <T>(v: T | T[] | undefined): T[] => (v === undefined ? [] : Array.isArray(v) ? v : [v]);

function role(name: string): any {
  const t = template("direct", "delivery");
  const roles = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any).filter((p) => p.RoleName === name);
  expect(roles, name).toHaveLength(1);
  return roles[0];
}

const stmts = (r: any): Stmt[] => r.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
const sid = (st: Stmt[], s: string): Stmt => {
  const found = st.find((x) => x.Sid === s);
  if (!found) throw new Error(`no statement ${s}: ${st.map((x) => x.Sid).join(", ")}`);
  return found;
};

/** IAM wildcard match (`*`, `?`) of a policy pattern against a concrete action or ARN. */
function wild(pattern: string, value: string): boolean {
  const re = new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".")}$`, "i");
  return re.test(value);
}

describe("PR check", () => {
  const t = template("direct", "delivery");
  const project = () => {
    const ps = Object.values(resourcesOf(t, "AWS::CodeBuild::Project")).map((r) => r.Properties as any);
    expect(ps).toHaveLength(1);
    return ps[0];
  };

  test("an ARM project runs the source commit's root buildspec.yml with the oracle tests as a secondary source", () => {
    const p = project();
    expect(p.Name).toBe("hamh-direct-prcheck");
    expect(p.Source.Type).toBe("CODECOMMIT");
    expect(p.Source.BuildSpec).toBe("buildspec.yml");
    expect(p.Source.Location).toBe(`https://git-codecommit.${REGION}.amazonaws.com/v1/repos/hamh-e2e-python-calc`);
    // One S3 secondary source per CodeCommit fixture's oracle tests: python-calc, fanout-kata (W1),
    // ts-lib, fanout-kata-24 and load-kata.
    expect(p.SecondarySources.map((s: any) => s.SourceIdentifier)).toEqual(["oracle_python_calc", "oracle_fanout_kata",
      "oracle_ts_lib", "oracle_fanout_kata_24", "oracle_load_kata"]);
    for (const src of p.SecondarySources) {
      expect(src.Type).toBe("S3");
      expect(JSON.stringify(src.Location)).toMatch(/\.zip/);
    }
    expect(p.Artifacts).toEqual({ Type: "NO_ARTIFACTS" });
    expect(p.Environment.Type).toBe("ARM_CONTAINER");
    expect(p.Environment.Image).toBe("aws/codebuild/amazonlinux-aarch64-standard:4.0");
    // The n-tier suites start their Compose data tier (and Testcontainers), so the build
    // runs a Docker daemon; images come through the lanes stack's pull-through mirror.
    expect(p.Environment.PrivilegedMode).toBe(true);
    expect(p.Environment.EnvironmentVariables).toEqual([
      { Name: "IMAGE_MIRROR", Value: `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-ecr-public`, Type: "PLAINTEXT" },
    ]);
    expect(p.ServiceRole).toEqual({ "Fn::GetAtt": ["PrCheckBuildRole", "Arn"] });
    expect(p.LogsConfig.CloudWatchLogs.Status).toBe("ENABLED");
    const groups = Object.values(resourcesOf(t, "AWS::Logs::LogGroup")).map((r) => (r.Properties as any).LogGroupName);
    expect(groups).toContain("/aws/codebuild/hamh-direct-prcheck");
  });

  test("the rule starts it on PR create and source update for harness PRs into main, in hamh-e2e-* repositories and those of the earlier prefix", () => {
    const rules = Object.values(resourcesOf(t, "AWS::Events::Rule")).map((r) => r.Properties as any);
    expect(rules).toHaveLength(1);
    const r = rules[0];
    expect(r.Name).toBe("hamh-direct-prcheck");
    expect(r.State).toBe("ENABLED");
    expect(r.EventPattern).toEqual({
      source: ["aws.codecommit"],
      "detail-type": ["CodeCommit Pull Request State Change"],
      resources: E2E.map((arn) => ({ prefix: arn.slice(0, -1) })),
      detail: {
        event: ["pullRequestCreated", "pullRequestSourceBranchUpdated"],
        pullRequestStatus: ["Open"],
        destinationReference: ["refs/heads/main"],
        sourceReference: [{ prefix: "refs/heads/harness/" }],
      },
    });
    expect(r.Targets).toHaveLength(1);
    const target = r.Targets[0];
    expect(target.Arn).toEqual({ "Fn::GetAtt": ["PrCheck", "Arn"] });
    expect(target.RoleArn).toEqual({ "Fn::GetAtt": ["PrCheckEventsRole", "Arn"] });
    expect(target.InputTransformer.InputPathsMap).toEqual({
      prId: "$.detail.pullRequestId", revId: "$.detail.revisionId", src: "$.detail.sourceCommit",
      dst: "$.detail.destinationCommit", repo: "$.detail.repositoryNames[0]",
    });
    // The template is StartBuild input once EventBridge fills the placeholders.
    const filled = String(target.InputTransformer.InputTemplate)
      .replace("<prId>", "7").replace("<revId>", "rev-7").replace(/<src>/g, "s".repeat(40))
      .replace("<dst>", "d".repeat(40)).replace(/<repo>/g, "hamh-e2e-python-calc");
    expect(JSON.parse(filled)).toEqual({
      sourceVersion: "s".repeat(40),
      sourceTypeOverride: "CODECOMMIT",
      sourceLocationOverride: `https://git-codecommit.${REGION}.amazonaws.com/v1/repos/hamh-e2e-python-calc`,
      environmentVariablesOverride: [
        { name: "PR_ID", value: "7", type: "PLAINTEXT" },
        { name: "PR_REVISION", value: "rev-7", type: "PLAINTEXT" },
        { name: "SRC_COMMIT", value: "s".repeat(40), type: "PLAINTEXT" },
        { name: "DEST_COMMIT", value: "d".repeat(40), type: "PLAINTEXT" },
        { name: "REPO", value: "hamh-e2e-python-calc", type: "PLAINTEXT" },
      ],
    });
  });

  test("the events role may start only this project", () => {
    const r = role("hamh-direct-prcheck-events");
    expect(r.Path).toBeUndefined();
    expect(r.AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "events.amazonaws.com" }, Action: "sts:AssumeRole",
        Condition: { StringEquals: { "aws:SourceAccount": ACCOUNT } } },
    ]);
    expect(stmts(r)).toEqual([{ Sid: "StartThePrCheck", Effect: "Allow", Action: "codebuild:StartBuild", Resource: { "Fn::GetAtt": ["PrCheck", "Arn"] } }]);
  });

  test("the build role reads hamh-e2e-* repositories and those of the earlier prefix, approves and comments, and nothing else on CodeCommit", () => {
    const r = role("hamh-direct-prcheck-build");
    expect(r.Path).toBeUndefined();
    expect(r.AssumeRolePolicyDocument.Statement).toEqual([
      { Effect: "Allow", Principal: { Service: "codebuild.amazonaws.com" }, Action: "sts:AssumeRole",
        Condition: { StringEquals: { "aws:SourceAccount": ACCOUNT } } },
    ]);
    const st = stmts(r);
    const read = sid(st, "ReadE2eRepositories");
    expect(list(read.Action).sort()).toEqual(["codecommit:GetFile", "codecommit:GetPullRequest", "codecommit:GitPull"]);
    expect(read.Resource).toEqual(E2E);
    const approve = sid(st, "ApproveAndComment");
    expect(list(approve.Action).sort()).toEqual(["codecommit:PostCommentForPullRequest", "codecommit:UpdatePullRequestApprovalState"]);
    expect(approve.Resource).toEqual(E2E);
    const cc = st.filter((s) => s.Effect === "Allow").flatMap((s) => list(s.Action)).filter((a) => a.startsWith("codecommit:"));
    expect(cc.sort()).toEqual(["codecommit:GetFile", "codecommit:GetPullRequest", "codecommit:GitPull",
      "codecommit:PostCommentForPullRequest", "codecommit:UpdatePullRequestApprovalState"]);
    const oracle = sid(st, "ReadOracleSource");
    expect(list(oracle.Action).sort()).toEqual(["s3:GetObject", "s3:GetObjectVersion"]);
    // Exactly the five oracle zips (python-calc, fanout-kata, ts-lib, fanout-kata-24, load-kata), each
    // an object ARN (no bucket-wide read).
    expect(list(oracle.Resource)).toHaveLength(5);
    for (const r of list(oracle.Resource)) expect(JSON.stringify(r)).toMatch(/\.zip/);
    const deny = sid(st, "NeverOverrideMergeEditOrBuild");
    expect(deny.Effect).toBe("Deny");
    expect(deny.Resource).toBe("*");
    for (const a of ["codecommit:OverridePullRequestApprovalRules", "codecommit:UpdatePullRequestApprovalRuleContent",
      "codecommit:CreatePullRequestApprovalRule", "codecommit:DeletePullRequestApprovalRule", "codecommit:MergePullRequestByFastForward",
      "codecommit:MergeBranchesBySquash", "codecommit:GitPush", "codecommit:PutFile", "codecommit:CreateCommit",
      "codecommit:DeleteBranch", "codecommit:DeleteRepository", "codebuild:StartBuild"]) {
      expect(list(deny.Action).some((p) => wild(p, a)), a).toBe(true);
    }
  });

  test("the build role pulls Compose images only through the ECR pull-through mirror", () => {
    const st = stmts(role("hamh-direct-prcheck-build"));
    const token = sid(st, "EcrToken");
    expect(token.Action).toBe("ecr:GetAuthorizationToken");
    expect(token.Resource).toBe("*");
    const pull = sid(st, "PullMirror");
    expect(list(pull.Action).sort()).toEqual(["ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:BatchImportUpstreamImage",
      "ecr:CreateRepository", "ecr:GetDownloadUrlForLayer", "ecr:TagResource"]);
    expect(pull.Resource).toBe(`arn:aws:ecr:${REGION}:${ACCOUNT}:repository/hamh-direct-ecr-public/*`);
    const ecr = st.filter((s) => s.Effect === "Allow").flatMap((s) => list(s.Action)).filter((a) => a.startsWith("ecr:"));
    expect(ecr.filter((a) => /Put|Upload|Delete|Complete|Initiate/.test(a))).toEqual([]);
  });

  test("every PR-check resource carries the project tag", () => {
    for (const type of ["AWS::CodeBuild::Project", "AWS::Events::Rule", "AWS::IAM::Role"]) {
      for (const r of Object.values(resourcesOf(t, type)).map((x) => x.Properties as any)) {
        const tags = Object.fromEntries(list(r.Tags).map((x: any) => [x.Key, x.Value]));
        expect(tags.project, `${type} ${r.Name ?? r.RoleName}`).toBe("hyperscale-agent-meta-harness");
      }
    }
  });
});

describe("nothing else may approve or start the check", () => {
  /** (owner, statement) for every Allow statement of every role and policy in the app. */
  function allows(): Array<[string, Stmt]> {
    const out: Array<[string, Stmt]> = [];
    for (const [stack, t] of synth("direct").templates) {
      for (const [id, r] of Object.entries(t.toJSON().Resources ?? {}) as [string, any][]) {
        const docs: any[] = [];
        if (r.Type === "AWS::IAM::Role") docs.push(...list(r.Properties?.Policies).map((p: any) => p.PolicyDocument));
        if (r.Type === "AWS::IAM::Policy" || r.Type === "AWS::IAM::ManagedPolicy") docs.push(r.Properties?.PolicyDocument);
        const owner = r.Properties?.RoleName ?? `${stack}/${id}`;
        for (const d of docs) for (const s of list(d?.Statement) as Stmt[]) if (s.Effect === "Allow") out.push([owner, s]);
      }
    }
    return out;
  }

  const resources = (s: Stmt): string[] => list(s.Resource).map((x) => (typeof x === "string" ? x : JSON.stringify(x)));

  test("only the PR-check build role may approve a pull request", () => {
    const approvers = allows()
      .filter(([, s]) => list(s.Action).some((a) => wild(a, "codecommit:UpdatePullRequestApprovalState")))
      .map(([o]) => o);
    expect([...new Set(approvers)]).toEqual(["hamh-direct-prcheck-build"]);
  });

  test("only the events role may start the PR-check project", () => {
    const starters = allows()
      .filter(([, s]) => list(s.Action).some((a) => wild(a, "codebuild:StartBuild")))
      .filter(([, s]) => resources(s).some((r) => r.includes("PrCheck") || wild(r, PROJECT_ARN)))
      .map(([o]) => o);
    expect([...new Set(starters)]).toEqual(["hamh-direct-prcheck-events"]);
  });

  test("no role may override, edit or delete approval rules or merge", () => {
    for (const a of ["codecommit:OverridePullRequestApprovalRules", "codecommit:UpdatePullRequestApprovalRuleContent",
      "codecommit:DeletePullRequestApprovalRule", "codecommit:MergePullRequestByThreeWay", "codecommit:MergeBranchesByFastForward"]) {
      const holders = allows().filter(([, s]) => list(s.Action).some((p) => wild(p, a))).map(([o]) => o);
      expect(holders, a).toEqual([]);
    }
  });
});
