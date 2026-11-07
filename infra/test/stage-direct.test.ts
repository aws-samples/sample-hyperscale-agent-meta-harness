import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "vitest";
import { ACCOUNT, LAMBDAS, OWNER, REGION, STACK_NAMES, TARGET, stageConfig } from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { phase2Images, synth } from "./helpers.ts";

// Direct run: stage `direct` is the one stage of the app. It builds nine stacks of `hamh-direct-*`
// names in its own network and tags everything, so a stage-filtered teardown finds every resource and nothing of another project.
const REQUIRED_TAGS = { project: "hyperscale-agent-meta-harness", owner: OWNER, "managed-by": "cdk" };
const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;

const direct = synth("direct", phase2Images("direct"));

type Resource = { Type: string; Properties?: Record<string, any> };

/** Every physical name the templates of a stage give a resource: any `...Name` property whose value is a plain string. */
function physicalNames(templates: Map<string, { toJSON(): any }>): Map<string, string> {
  const out = new Map<string, string>();
  for (const [stack, t] of templates) {
    out.set(`stack ${stack}`, stack);
    for (const [id, r] of Object.entries(t.toJSON().Resources ?? {}) as [string, Resource][]) {
      // An AgentCore endpoint is named inside its runtime (`live`), and a Lambda environment variable is not a name.
      // The default research provider (auto) builds the AgentCore Gateway, whose one target is named inside its gateway (`web-search-tool`, which also fixes
      // the tool name `web-search-tool___WebSearch`); the gateway itself is named hamh-direct-websearch and is checked.
      if (r.Type === "AWS::BedrockAgentCore::RuntimeEndpoint" || r.Type === "AWS::BedrockAgentCore::GatewayTarget") continue;
      for (const [k, v] of Object.entries(r.Properties ?? {})) {
        if (!/Name$/.test(k) || typeof v !== "string") continue;
        // Names that are not the resource's own account-wide name: a VPC endpoint's service, a policy's label inside its role, an API stage.
        // A CloudWatch alarm names the metric it reads (`TavilyKeyAgeDays`); that is the metric's name, not the alarm's, and the alarm's own `AlarmName` is checked.
        if (["ServiceName", "PolicyName", "StageName", "MetricName"].includes(k)) continue;
        // A CodeArtifact repository is named inside its domain, which the same stack creates: its name is the domain's (the stack's) and its own.
        const domain = r.Type === "AWS::CodeArtifact::Repository" ? `${stack}/` : "";
        out.set(`${stack}/${id}.${k}`, domain + v);
      }
      // An SSM parameter, an events rule and a log group name themselves with `Name`: covered by the suffix test above.
    }
  }
  return out;
}

describe("stage config", () => {
  test("direct: its own CIDR, one AZ, the full microVM lifetime, one engine route and no ladder", () => {
    const c = stageConfig("direct");
    expect(c.prefix).toBe("hamh-direct");
    expect(c.vpcCidr).toBe("10.50.0.0/16");
    expect(c.azs).toEqual(TARGET.azs.direct);
    expect(c.azs).toHaveLength(1);
    expect(c.worker).toEqual({ maxLifetimeS: 28_800, idleTimeoutS: 1_800 });
    expect(c.logRetentionDays).toBe(30);
    expect(c.engineRouting).toBe("run");
    expect(c.routeLadder).toBe("off");
  });

  test("the VPC the network stack creates is the stage's CIDR", () => {
    const vpcs = Object.values(direct.templates.get("hamh-direct-network")!.toJSON().Resources as Record<string, Resource>).filter(
      (r) => r.Type === "AWS::EC2::VPC");
    expect(vpcs.map((v) => v.Properties!.CidrBlock)).toEqual(["10.50.0.0/16"]);
  });

  test("an unknown stage is refused with direct named, live and drill included", () => {
    for (const stage of ["prod", "live", "drill"]) expect(() => stageConfig(stage as never)).toThrow(/expected direct/);
  });
});

describe("the stacks", () => {
  test("nine stacks, no gateway and no lanes, and the multi-agent orchestration is not among them", () => {
    expect([...direct.templates.keys()].sort()).toEqual(STACK_NAMES.map((s) => `hamh-direct-${s}`).sort());
    expect(STACK_NAMES).toHaveLength(9);
    expect(STACK_NAMES).not.toContain("gateway");
    expect(STACK_NAMES).not.toContain("lanes");
  });

  test("none of the multi-agent line's functions, machines or projects exists in the stage", () => {
    const all = JSON.stringify([...direct.templates.values()].map((t) => t.toJSON()));
    for (const gone of ["dispatcher", "goal-step", "gate-runner", "-evaluate", "run-tasks", "admission", "sweeper", "dag-scheduler", "repo-read",
      "worker-goal-loop", "hidden-admission", "laneb", "-run-ended\"", "harness-verifier-session", "harness-judge-session"]) {
      if (gone === "-run-ended\"") continue; // direct has its own run-ended rule
      expect(all.includes(gone), gone).toBe(false);
    }
  });

  test("only the python worker runtime exists", () => {
    const rt = direct.templates.get("hamh-direct-runtimes")!.toJSON();
    const runtimes = Object.values(rt.Resources as Record<string, Resource>).filter((r) => r.Type === "AWS::BedrockAgentCore::Runtime");
    expect(runtimes.map((r) => r.Properties!.AgentRuntimeName)).toEqual(["hamh_direct_harness_worker_python"]);
    const repos = Object.values(rt.Resources as Record<string, Resource>).filter((r) => r.Type === "AWS::ECR::Repository");
    expect(repos.map((r) => r.Properties!.RepositoryName)).toEqual(["hamh-direct-worker-python"]);
  });

  test("the python worker's runtime lives eight hours with a 30 minute idle timeout", () => {
    const rt = direct.templates.get("hamh-direct-runtimes")!.toJSON();
    const runtime = Object.values(rt.Resources as Record<string, Resource>).find((r) => r.Type === "AWS::BedrockAgentCore::Runtime")!;
    expect(runtime.Properties!.LifecycleConfiguration).toEqual({ IdleRuntimeSessionTimeout: 1800, MaxLifetime: 28800 });
    expect(runtime.Properties!.EnvironmentVariables.HAMH_STAGE).toBe("direct");
    expect(Object.keys(runtime.Properties!.EnvironmentVariables).sort()).toEqual([
      "AGENTD_ENGINE_SANDBOX", "AGENTD_GOPROXY", "AGENTD_PACKAGES_DOMAIN", "AGENTD_PACKAGES_OWNER", "AGENTD_PACKAGES_REPOSITORY", "AGENTD_TOOLS_MODE", "HAMH_STAGE", "HAMH_WORKER_REVISION"]);
  });

  // First deploy, 2026-10-14: hamh-direct-observability rolled back with "User: 700000000011 already has 5 trails in us-east-1". Stage
  // direct therefore builds no trail of its own.
  test("observability builds no trail, no trail bucket, no trail log group, no trail role, no alarm and no dashboard", () => {
    const obs = direct.templates.get("hamh-direct-observability")!.toJSON().Resources as Record<string, Resource>;
    const types = Object.values(obs).map((r) => r.Type);
    expect(types).not.toContain("AWS::CloudTrail::Trail");
    expect(types).not.toContain("AWS::S3::Bucket");
    expect(types).not.toContain("AWS::S3::BucketPolicy");
    expect(types).not.toContain("AWS::Logs::LogGroup");
    expect(types).not.toContain("AWS::IAM::Role");
    expect(types).toContain("AWS::Logs::QueryDefinition");
    expect(types).not.toContain("AWS::CloudWatch::Alarm");
    expect(types).not.toContain("AWS::CloudWatch::CompositeAlarm");
    expect(types).not.toContain("AWS::CloudWatch::Dashboard");
  });
});

describe("the image-build project (review of the run machine, S3)", () => {
  // scripts/codebuild-run.sh --stage direct starts `hamh-direct-image-build`; stage direct has no lanes stack, so the project is in its
  // runtimes stack, the stack that owns the worker repository it pushes to. A first deploy creates it in the image-less first pass.
  const runtimes = direct.templates.get("hamh-direct-runtimes")!.toJSON().Resources as Record<string, Resource>;
  const ofType = (type: string) => Object.entries(runtimes).filter(([, r]) => r.Type === type);

  test("the runtimes stack holds one ARM, Docker-enabled CodeBuild project named hamh-direct-image-build", () => {
    const projects = ofType("AWS::CodeBuild::Project").map(([, r]) => r.Properties!);
    expect(projects.map((p) => p.Name)).toEqual(["hamh-direct-image-build"]);
    const [p] = projects;
    expect(p.Environment).toMatchObject({ Type: "ARM_CONTAINER", Image: "aws/codebuild/amazonlinux-aarch64-standard:4.0", PrivilegedMode: true });
    expect(p.Source.Location).toBe(`hamh-direct-harness-${ACCOUNT}/builds/source/latest.zip`);
    expect(p.LogsConfig.CloudWatchLogs.Status).toBe("ENABLED");
    const logs = ofType("AWS::Logs::LogGroup").map(([, r]) => r.Properties!.LogGroupName);
    expect(logs).toEqual(["/hamh/direct/codebuild/image-build"]);
    expect(direct.templates.has("hamh-direct-lanes")).toBe(false);
  });

  test("its role pushes to the python worker and delivery repositories alone, reads sources and writes outputs of the stage bucket", () => {
    const roles = ofType("AWS::IAM::Role").map(([, r]) => r.Properties!).filter((r) => r.RoleName === "hamh-direct-image-build");
    expect(roles).toHaveLength(1);
    const st = roles[0].Policies.flatMap((x: any) => x.PolicyDocument.Statement) as Array<Record<string, any>>;
    const push = st.find((x) => x.Sid === "PushHarnessImages")!;
    expect(push.Resource).toEqual([
      `arn:aws:ecr:${REGION}:${ACCOUNT}:repository/hamh-direct-worker-python`,
      `arn:aws:ecr:${REGION}:${ACCOUNT}:repository/hamh-direct-delivery`,
    ]);
    const bucket = `arn:aws:s3:::hamh-direct-harness-${ACCOUNT}`;
    expect(st.find((x) => x.Sid === "ReadBuildSources")!.Resource).toBe(`${bucket}/builds/source/*`);
    expect(st.find((x) => x.Sid === "WriteBuildOutputs")!.Resource).toBe(`${bucket}/builds/out/*`);
    for (const x of st) {
      expect(JSON.stringify(x.Resource ?? ""), x.Sid).not.toMatch(/hamh-(live|drill)/);
      expect([x.Action].flat().join(), x.Sid).not.toMatch(/bedrock|codecommit|sts:AssumeRole|iam:/);
    }
  });

  test("it carries the four tags and names nothing of another stage", () => {
    const [[, project]] = ofType("AWS::CodeBuild::Project");
    const tags = Object.fromEntries((project.Properties!.Tags as Array<{ Key: string; Value: string }>).map((t) => [t.Key, t.Value]));
    expect(tags).toEqual({ ...REQUIRED_TAGS, stage: "direct" });
    expect(JSON.stringify(runtimes)).not.toMatch(/hamh-live|hamh-drill|\/hamh\/(live|drill)/);
  });
});

describe("physical names (section 9.5)", () => {
  const dn = physicalNames(direct.templates);

  test("the direct stage names something", () => {
    expect(dn.size).toBeGreaterThan(40);
  });

  test("every direct name carries hamh-direct, hamh_direct or /hamh/direct", () => {
    const bad = [...dn].filter(([, v]) => !/hamh-direct|hamh_direct|\/hamh\/direct/.test(v));
    expect(bad.map(([k, v]) => `${k}=${v}`)).toEqual([]);
  });

  test("the stack names all start hamh-direct-", () => {
    for (const s of direct.templates.keys()) expect(s.startsWith("hamh-direct-"), s).toBe(true);
  });

  test("no template names the old stages or their network", () => {
    const all = JSON.stringify([...direct.templates.values()].map((t) => t.toJSON()));
    expect(all).not.toMatch(/hamh-live|hamh_live|\/hamh\/live|hamh-drill|hamh_drill|\/hamh\/drill|10\.40\.|10\.41\./);
  });

  test("the harness bucket, table, role and state machine names are the section 9.1 table's", () => {
    const all = new Set(dn.values());
    for (const want of [
      `hamh-direct-harness-${ACCOUNT}`, "hamh-direct-harness", "hamh-direct-harness-worker-session", "hamh-direct-run-session", "hamh-direct-run",
      "hamh-direct-control-api", "hamh-direct-worker-api", "hamh-direct-run-ended", "/aws/lambda/hamh-direct-run-session",
      "/hamh/direct/worker-python-runtime-arn", "hamh_direct_harness_worker_python", `hamh-direct-gomod-${ACCOUNT}`, "hamh-direct-repo-gateway",
      "hamh-direct-bundle-validator", "hamh-direct-packager", "hamh-direct",
    ]) {
      expect(all.has(want), want).toBe(true);
    }
  });
});

describe("tags and teardown", () => {
  const tagMap = (tags: unknown): Record<string, string> => {
    if (Array.isArray(tags)) return Object.fromEntries(tags.map((t: { Key: string; Value: string }) => [t.Key, t.Value]));
    if (tags && typeof tags === "object") return tags as Record<string, string>;
    return {};
  };
  // Resource types of these stacks whose CloudFormation schema takes tags: each must carry the four.
  const TAGGABLE = [
    "AWS::S3::Bucket", "AWS::DynamoDB::Table", "AWS::Lambda::Function", "AWS::IAM::Role", "AWS::KMS::Key", "AWS::Logs::LogGroup",
    "AWS::StepFunctions::StateMachine", "AWS::ECR::Repository", "AWS::EC2::VPC", "AWS::EC2::Subnet", "AWS::EC2::SecurityGroup",
    "AWS::ApiGateway::RestApi", "AWS::SSM::Parameter", "AWS::CloudTrail::Trail", "AWS::BedrockAgentCore::Runtime",
  ];

  test("every taggable resource carries project, stage=direct, owner and managed-by", () => {
    let tagged = 0;
    const types = new Set<string>();
    for (const [name, t] of direct.templates) {
      for (const [id, r] of Object.entries(t.toJSON().Resources ?? {}) as [string, Resource][]) {
        types.add(r.Type);
        const taggable = TAGGABLE.includes(r.Type) || (r.Properties && "Tags" in r.Properties);
        if (!taggable) continue;
        tagged += 1;
        const tags = tagMap(r.Properties?.Tags);
        for (const [k, v] of Object.entries({ ...REQUIRED_TAGS, stage: "direct" })) {
          expect(tags[k], `${name}/${id} (${r.Type}) tag ${k}`).toBe(v);
        }
      }
    }
    expect(tagged).toBeGreaterThan(30);
    expect([...types]).toContain("AWS::StepFunctions::StateMachine");
  });

  test("every stack carries the four tags itself, as CloudFormation stack tags, so a stage-filtered teardown finds it", () => {
    const { app } = direct;
    const stacks = app.synth().stacks;
    expect(stacks.map((s) => s.stackName).sort()).toEqual(STACK_NAMES.map((s) => `hamh-direct-${s}`).sort());
    for (const s of stacks) expect(s.tags, s.stackName).toEqual({ ...REQUIRED_TAGS, stage: "direct" });
  });

  test("the stage tag separates the stages: no direct resource carries stage=live or stage=drill", () => {
    const all = JSON.stringify([...direct.templates.values()].map((t) => t.toJSON()));
    expect(all).not.toMatch(/"Key":"stage","Value":"(live|drill)"/);
    expect(all).not.toMatch(/"stage":"(live|drill)"/);
  });
});

describe("the CDK entry for stage direct", () => {
  const context = JSON.stringify({ stage: "direct" });
  test("the stage needs worker-api, control-api, run-session and the two research functions", () => {
    expect(LAMBDAS).toEqual(["worker-api", "control-api", "run-session", "research-api", "research-job"]);
  });

  test("a directory with the five binaries is enough, and a directory without run-session or a research function is refused", () => {
    const dir = mkdtempSync(join(tmpdir(), "ham-entry-direct-"));
    try {
      for (const name of ["worker-api", "control-api"]) {
        mkdirSync(join(dir, name));
        writeFileSync(join(dir, name, "bootstrap"), "fixture");
      }
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: dir, CDK_CONTEXT_JSON: context })).toThrow(/run-session\/bootstrap/);
      mkdirSync(join(dir, "run-session"));
      writeFileSync(join(dir, "run-session", "bootstrap"), "fixture");
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: dir, CDK_CONTEXT_JSON: context })).toThrow(/research-api\/bootstrap/);
      mkdirSync(join(dir, "research-api"));
      writeFileSync(join(dir, "research-api", "bootstrap"), "fixture");
      expect(() => entryOptions({ HAMH_LAMBDA_DIR: dir, CDK_CONTEXT_JSON: context })).toThrow(/research-job\/bootstrap/);
      mkdirSync(join(dir, "research-job"));
      writeFileSync(join(dir, "research-job", "bootstrap"), "fixture");
      const o = entryOptions({ HAMH_LAMBDA_DIR: dir, CDK_CONTEXT_JSON: context });
      expect(o.stage).toBe("direct");
      // without the context the stage is direct as well
      expect(entryOptions({ HAMH_LAMBDA_DIR: dir }).stage).toBe("direct");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("the test fixture directory holds every binary of the stage", () => {
    expect(entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, CDK_CONTEXT_JSON: context }).stage).toBe("direct");
  });

  test("the account and Region are the selected accounts.json entry's", () => {
    for (const t of direct.templates.values()) expect(JSON.stringify(t.toJSON())).not.toContain("us-gov");
    expect(TARGET.region).toBe(REGION);
  });
});
