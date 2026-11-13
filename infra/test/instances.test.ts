import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { Template } from "aws-cdk-lib/assertions";
import { buildApp } from "../lib/app.ts";
import { ACCOUNT, COMPUTE_INSTANCES_MODES, DEFAULT_COMPUTE_INSTANCES, type InstancesConfig, names, OWNER, REGION, resolveComputeInstances, resolveStageSettings, type StageConfig, stageConfig } from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { phase2Images, resourcesOf, synth } from "./helpers.ts";

// 0217, 0218: the AgentCore Instances compute type is an opt-in of the deploy (HAMH_COMPUTE_INSTANCES), like the tool layer. Off (the default) the
// stage keeps the one microVM worker runtime. On, the runtimes stack adds a capacity provider and a second runtime on the same image, the security
// stack adds the provider's operator role, and run-session gets the grants to start and end sessions on it.
const FIXTURE_LAMBDA_DIR = new URL("./fixtures/lambda", import.meta.url).pathname;
const MICRO_ARN_PARAMETER = "/hamh/direct/worker-python-runtime-arn";
const INSTANCES_ARN_PARAMETER = "/hamh/direct/worker-python-instances-runtime-arn";
const PROVIDER_ID_PARAMETER = "/hamh/direct/instances-capacity-provider-id";
const TABLE = JSON.parse(readFileSync(new URL("./fixtures/compute-instances.json", import.meta.url), "utf8")) as {
  default: string; accept: { setting: string | null; mode: string }[]; reject: string[];
};
const OPERATOR_POLICY = "arn:aws:iam::aws:policy/BedrockAgentCoreRuntimeInstancesOperatorRolePolicy";

const off = synth("direct", phase2Images("direct")).templates;
const on = synth("direct", { ...phase2Images("direct"), computeInstances: "on" }).templates;
const flat = (v: unknown) => JSON.stringify(v);

const runtimesOf = (t: Template) => Object.values(resourcesOf(t, "AWS::BedrockAgentCore::Runtime")).map((r) => r.Properties as any);
const providerOf = (t: Template) => Object.values(resourcesOf(t, "AWS::BedrockAgentCore::CapacityProvider")).map((r) => r.Properties as any);

describe("HAMH_COMPUTE_INSTANCES, the stage setting", () => {
  test("two words, off by default", () => {
    expect([...COMPUTE_INSTANCES_MODES]).toEqual(["off", "on"]);
    expect(DEFAULT_COMPUTE_INSTANCES).toBe("off");
  });

  test("every accepted setting of the shared table resolves to its mode, every rejected one throws and names the setting and the words", () => {
    expect(TABLE.default).toBe(DEFAULT_COMPUTE_INSTANCES);
    for (const c of TABLE.accept) expect(resolveComputeInstances(c.setting ?? undefined), JSON.stringify(c.setting)).toBe(c.mode);
    for (const bad of TABLE.reject) {
      expect(() => resolveComputeInstances(bad), JSON.stringify(bad)).toThrow(/HAMH_COMPUTE_INSTANCES=".*" is not one of off, on/);
    }
  });

  test("the entry carries a set word to the app and rejects a bad one by name before any stack is built", () => {
    const env = { HAMH_LAMBDA_DIR: FIXTURE_LAMBDA_DIR };
    expect(entryOptions({ ...env, HAMH_COMPUTE_INSTANCES: "on" }).computeInstances).toBe("on");
    // The entry passes the trimmed word; the app resolves it again, as for the tool layer.
    expect(entryOptions({ ...env, HAMH_COMPUTE_INSTANCES: " ON " }).computeInstances).toBe("ON");
    expect(entryOptions(env).computeInstances).toBeUndefined();
    expect(entryOptions({ ...env, HAMH_COMPUTE_INSTANCES: "" }).computeInstances).toBeUndefined();
    expect(() => entryOptions({ ...env, HAMH_COMPUTE_INSTANCES: "maybe" })).toThrow(/HAMH_COMPUTE_INSTANCES/);
  });

  test("the app rejects a bad word as well", () => {
    expect(() => buildApp({ stage: "direct", computeInstances: "maybe" })).toThrow(/HAMH_COMPUTE_INSTANCES/);
  });
});

describe("the default machine list of the Instances part", () => {
  // One type in one zone ran out: EC2 had no m7g.4xlarge in the stage's zone and two runs ended blocked. The default lets AgentCore pick among types of the same
  // size and architecture, so a shortage of one type does not stop a run.
  test("b815 the default holds more than one type, all general purpose Graviton (arm64) machines of 16 vCPU, the spike's type first", () => {
    const i = stageConfig("direct").instances;
    expect(i.instanceTypes.length).toBeGreaterThan(1);
    expect(i.instanceTypes.length).toBeLessThanOrEqual(30);
    expect(i.instanceTypes[0]).toBe("m7g.4xlarge");
    for (const t of i.instanceTypes) expect(t).toMatch(/^m[0-9]+g\.4xlarge$/);
    expect(new Set(i.instanceTypes).size).toBe(i.instanceTypes.length);
    expect(i.vcpus).toBe(16);
  });

  test("b815 the provider of a stage with the Instances part allows every type of the default", () => {
    const [p] = providerOf(on.get("hamh-direct-runtimes")!);
    const types = p.ComputeConfiguration.Ec2Configuration.LaunchTemplateSource.LaunchParameters.InstanceRequirements.AllowedInstanceTypes;
    expect(types).toEqual([...stageConfig("direct").instances.instanceTypes]);
    expect(types.length).toBeGreaterThan(1);
  });
});

describe("the stage values of the Instances part", () => {
  test("the spike's values are the defaults, and the runtime lifetime does not exceed the provider's", () => {
    const i = stageConfig("direct").instances;
    expect(i.instanceTypes).toEqual(["m7g.4xlarge", "m6g.4xlarge"]);
    expect(i.rootFreeGiB).toBe(100);
    expect(i.workspaceGiB).toBe(150);
    expect(i.volumeName).toBe("workspace");
    expect(i.mountPath).toBe("/mnt/workspace");
    expect(i.maxLifetimeS).toBe(stageConfig("direct").worker.maxLifetimeS);
    expect(i.idleInstanceTimeoutS).toBeGreaterThanOrEqual(stageConfig("direct").worker.idleTimeoutS);
  });

  test("the names are the underscore form AgentCore takes and carry the stage prefix", () => {
    const cfg = stageConfig("direct");
    expect(names.capacityProvider(cfg)).toMatch(/^hamh_direct_instances_[0-9a-f]{8}$/);
    expect(names.instancesRuntime(cfg)).toBe("hamh_direct_harness_worker_python_instances");
    expect(names.capacityProviderIdParameter(cfg)).toBe(PROVIDER_ID_PARAMETER);
    expect(names.runtimeArnParameter(cfg, "worker-python-instances")).toBe(INSTANCES_ARN_PARAMETER);
    expect(names.role(cfg, "instances-operator")).toBe("hamh-direct-instances-operator");
    for (const n of [names.capacityProvider(cfg), names.instancesRuntime(cfg)]) expect(n).toMatch(/^[a-zA-Z][a-zA-Z0-9_]{0,47}$/);
  });
});

// The stage prefix is `hamh-direct` (11 characters; `hamh_direct` in underscore form). Each name below is the longest of its kind, built from the app's own helpers with a 12 digit
// account, and fits the limit of its service. The AgentCore runtime of the Instances part is the tightest: 43 of 48 characters, so the underscore form of the prefix may grow by
// five characters at most before `hamh_direct_harness_worker_python_instances` stops fitting. The lengths are those of the rename design (section 4).
describe("the longest physical names fit their service limits", () => {
  const cfg = stageConfig("direct");
  const longest: { what: string; name: string; length: number; limit: number }[] = [
    { what: "stack (observability)", name: `${cfg.prefix}-observability`, length: 25, limit: 128 },
    { what: "harness bucket", name: names.bucket(cfg), length: 32, limit: 63 },
    { what: "IAM role (harness-worker-session)", name: names.role(cfg, "harness-worker-session"), length: 34, limit: 64 },
    { what: "Lambda function (bundle-validator)", name: `${cfg.prefix}-bundle-validator`, length: 28, limit: 64 },
    { what: "state machine (run-ended)", name: names.stateMachine(cfg, "run-ended"), length: 21, limit: 80 },
    { what: "AgentCore runtime (Instances)", name: names.instancesRuntime(cfg), length: 43, limit: 48 },
    { what: "AgentCore capacity provider", name: names.capacityProvider(cfg), length: 30, limit: 48 },
    { what: "research secret", name: names.researchSecret(cfg), length: 35, limit: 512 },
  ];

  test.each(longest)("$what is $length characters of $limit", ({ name, length, limit }) => {
    expect(name.length).toBe(length);
    expect(name.length).toBeLessThanOrEqual(limit);
  });

  test("the AgentCore runtime names, with the worker runtime, match AgentCore's pattern", () => {
    for (const n of [names.instancesRuntime(cfg), names.capacityProvider(cfg), "hamh_direct_harness_worker_python"]) expect(n).toMatch(/^[a-zA-Z][a-zA-Z0-9_]{0,47}$/);
  });
});

describe("off (the default): the stage is the one microVM runtime it was", () => {
  test("no capacity provider, no second runtime, no operator role, no instance profile, no extra parameter", () => {
    const runtimes = off.get("hamh-direct-runtimes")!;
    expect(providerOf(runtimes)).toHaveLength(0);
    expect(runtimesOf(runtimes)).toHaveLength(1);
    expect(runtimesOf(runtimes)[0].AgentRuntimeName).toBe("hamh_direct_harness_worker_python");
    runtimes.resourceCountIs("AWS::SSM::Parameter", 1);
    for (const [name, t] of off) {
      expect(flat(t.toJSON()), name).not.toContain("instances-operator");
      expect(flat(t.toJSON()), name).not.toContain("BedrockAgentCoreRuntimeInstancesOperatorRolePolicy");
      t.resourceCountIs("AWS::IAM::InstanceProfile", 0);
    }
  });

  test("the microVM runtime says which isolation it uses: AGENTD_ENGINE_SANDBOX=uid, in the runtime and in its custom resource", () => {
    const runtimes = off.get("hamh-direct-runtimes")!;
    expect(runtimesOf(runtimes)[0].EnvironmentVariables.AGENTD_ENGINE_SANDBOX).toBe("uid");
    const cr = flat(Object.values(runtimes.findResources("Custom::AWS"))[0]);
    expect(cr).toContain('\\"AGENTD_ENGINE_SANDBOX\\":\\"uid\\"');
  });

  test("run-session keeps its 300 s timeout and learns nothing of Instances", () => {
    const orch = off.get("hamh-direct-orchestration")!;
    const fn = Object.values(resourcesOf(orch, "AWS::Lambda::Function"))[0].Properties as any;
    expect(fn.Timeout).toBe(300);
    expect(Object.keys(fn.Environment.Variables)).not.toContain("INSTANCES_RUNTIME_ARN_PARAMETER");
    expect(Object.keys(fn.Environment.Variables)).not.toContain("CAPACITY_PROVIDER_ID_PARAMETER");
    expect(flat(orch.toJSON())).not.toContain("DeleteCapacityProviderSession");
  });
});

describe("on: the capacity provider", () => {
  const runtimes = on.get("hamh-direct-runtimes")!;
  const [provider] = providerOf(runtimes);

  test("exactly one, named for the stage, with the operator role of the security stack", () => {
    expect(providerOf(runtimes)).toHaveLength(1);
    expect(provider.Name).toBe(names.capacityProvider(stageConfig("direct")));
    expect(provider.Name).toMatch(/^hamh_direct_instances_[0-9a-f]{8}$/);
    expect(flat(provider.PermissionsConfiguration.CapacityProviderOperatorRoleArn)).toContain("hamh-direct-instances-operator");
  });

  test("Graviton Linux, the spike's machine, and no instance profile at all", () => {
    const ec2 = provider.ComputeConfiguration.Ec2Configuration;
    const lp = ec2.LaunchTemplateSource.LaunchParameters;
    expect(lp.OperatingSystem).toBe("LINUX_ARM64");
    expect(lp.InstanceRequirements.AllowedInstanceTypes).toEqual(["m7g.4xlarge", "m6g.4xlarge"]);
    expect(lp.InstanceProfileArn).toBeUndefined();
    expect(lp.SshKeyName).toBeUndefined();
    expect(lp.Monitoring).toBe("BASIC");
  });

  test("the machine sits in the stage's private subnets behind the runtime security group, so the firewall, endpoints and S3 conditions keep applying", () => {
    const vpc = provider.ComputeConfiguration.Ec2Configuration.VpcConfiguration;
    expect(vpc.Subnets.length).toBeGreaterThanOrEqual(1);
    expect(vpc.SecurityGroups).toHaveLength(1);
    expect(flat(vpc.SecurityGroups[0])).toContain("hamh-direct-network");
    expect(flat(vpc.Subnets)).toContain("hamh-direct-network");
  });

  test("encrypted gp3 root volume with 100 GiB free, one encrypted 150 GiB gp3 workspace volume at the default 3000 IOPS and 125 MiB/s", () => {
    const ec2 = provider.ComputeConfiguration.Ec2Configuration;
    expect(ec2.RootVolume).toEqual({ VolumeType: "gp3", Encrypted: true, FreeSpaceGiB: 100 });
    expect(ec2.Volumes).toEqual([{ EbsConfiguration: { Name: "workspace", SizeGiB: 150, VolumeType: "gp3", Iops: 3000, Throughput: 125, Encrypted: true } }]);
  });

  test("the instance lives as long as a run (28,800 s) and idles out after 1,800 s", () => {
    expect(provider.ComputeConfiguration.Ec2Configuration.LifecycleConfiguration).toEqual({ MaxLifetime: 28800, IdleInstanceTimeout: 1800 });
  });

  test("the project tags are on the provider and propagate to what AgentCore launches", () => {
    const tags = Object.fromEntries((provider.Tags as { Key: string; Value: string }[]).map((t) => [t.Key, t.Value]));
    expect(tags).toMatchObject({ project: "hyperscale-agent-meta-harness", stage: "direct", owner: OWNER, "managed-by": "cdk" });
    const propagated = provider.ComputeConfiguration.Ec2Configuration.LaunchTemplateSource.LaunchParameters.PropagatedTags;
    expect(propagated).toMatchObject({ project: "hyperscale-agent-meta-harness", stage: "direct", owner: OWNER, "managed-by": "cdk" });
  });
});

describe("on: the second runtime", () => {
  const runtimes = on.get("hamh-direct-runtimes")!;
  const all = runtimesOf(runtimes);
  const micro = all.find((r) => r.AgentRuntimeName === "hamh_direct_harness_worker_python")!;
  const inst = all.find((r) => r.AgentRuntimeName === "hamh_direct_harness_worker_python_instances")!;

  test("two runtimes: the microVM one is the one it was, the new one runs on the capacity provider", () => {
    expect(all).toHaveLength(2);
    expect(micro.NetworkConfiguration.NetworkMode).toBe("VPC");
    expect(micro.FilesystemConfigurations).toEqual([{ SessionStorage: { MountPath: "/mnt/workspace" } }]);
    expect(micro.CapacityProviderConfiguration).toBeUndefined();
    expect(inst.CapacityProviderConfiguration).toEqual({ CapacityProviderArn: expect.anything() });
    expect(flat(inst.CapacityProviderConfiguration.CapacityProviderArn)).toContain("Arn");
  });

  test("no network configuration and no session storage (Instances takes its network from the provider), the workspace volume mounted at /mnt/workspace", () => {
    expect(inst.NetworkConfiguration).toBeUndefined();
    expect(flat(inst.FilesystemConfigurations)).not.toContain("SessionStorage");
    expect(inst.FilesystemConfigurations).toEqual([{ CapacityProviderVolume: { VolumeName: "workspace", MountPath: "/mnt/workspace" } }]);
  });

  test("the same image, the same execution role (the eight-action runtime base role), HTTP protocol", () => {
    expect(inst.AgentRuntimeArtifact).toEqual(micro.AgentRuntimeArtifact);
    expect(inst.RoleArn).toEqual(micro.RoleArn);
    expect(flat(inst.RoleArn)).toContain("hamh-direct-runtime-base");
    expect(inst.ProtocolConfiguration).toBe("HTTP");
  });

  test("the same environment as the microVM runtime, except the words below: AGENTD_ENGINE_SANDBOX=landlock here, uid on the microVM", () => {
    expect(micro.EnvironmentVariables.AGENTD_ENGINE_SANDBOX).toBe("uid");
    expect(inst.EnvironmentVariables.AGENTD_ENGINE_SANDBOX).toBe("landlock");
    const differ = Object.keys({ ...micro.EnvironmentVariables, ...inst.EnvironmentVariables })
      .filter((k) => micro.EnvironmentVariables[k] !== inst.EnvironmentVariables[k]).sort();
    expect(differ).toEqual(["AGENTD_DIRECT_CARGO_JOBS", "AGENTD_ENGINE_SANDBOX", "AGENTD_ENGINE_USER", "AGENTD_WORKSPACE_LIMIT_MB"]);
  });

  // In `landlock` mode agentd runs the engine as its own user and refuses to start it while AGENTD_ENGINE_USER names another one.
  // The worker image sets AGENTD_ENGINE_USER=agent (the microVM runtime keeps that), so the Instances runtime must override it with the empty string.
  test("the Instances runtime sets AGENTD_ENGINE_USER to the empty string, because landlock runs the engine as agentd's own user; the microVM runtime leaves the image's user alone", () => {
    expect(inst.EnvironmentVariables).toHaveProperty("AGENTD_ENGINE_USER", "");
    expect(micro.EnvironmentVariables).not.toHaveProperty("AGENTD_ENGINE_USER");
  });

  // Agentd calls the checkout mount full at 960 MiB (the managed session storage of a microVM holds 1 GiB). On Instances the mount is a 150 GiB
  // volume, so the same limit would stop a run with 140 GiB free; 0 switches that check off. Cargo gets one job per vCPU of the machine instead of the microVM's 2.
  test("the Instances runtime switches off the 960 MiB checkout limit of a microVM and gives cargo one job per vCPU of its machine", () => {
    expect(inst.EnvironmentVariables).toHaveProperty("AGENTD_WORKSPACE_LIMIT_MB", "0");
    expect(inst.EnvironmentVariables).toHaveProperty("AGENTD_DIRECT_CARGO_JOBS", String(stageConfig("direct").instances.vcpus));
    expect(stageConfig("direct").instances.vcpus).toBe(16);
    for (const k of ["AGENTD_WORKSPACE_LIMIT_MB", "AGENTD_DIRECT_CARGO_JOBS"]) expect(micro.EnvironmentVariables, k).not.toHaveProperty(k);
  });

  test("the runtime lifetime is the run's 28,800 s and does not exceed the provider's", () => {
    expect(inst.LifecycleConfiguration).toEqual({ IdleRuntimeSessionTimeout: 1800, MaxLifetime: 28800 });
    const p = providerOf(runtimes)[0].ComputeConfiguration.Ec2Configuration.LifecycleConfiguration;
    expect(inst.LifecycleConfiguration.MaxLifetime).toBeLessThanOrEqual(p.MaxLifetime);
    expect(inst.LifecycleConfiguration.IdleRuntimeSessionTimeout).toBeLessThanOrEqual(p.IdleInstanceTimeout);
  });

  test("the runtime waits for the provider (it is created after it)", () => {
    const ids = Object.entries(runtimes.findResources("AWS::BedrockAgentCore::Runtime")) as [string, any][];
    const [, rt] = ids.find(([, r]) => r.Properties.AgentRuntimeName.endsWith("_instances"))!;
    expect(flat(rt.Properties.CapacityProviderConfiguration)).toContain("Fn::GetAtt");
  });

  test("a live endpoint serves the Instances runtime's own version (its metadata flag is true by default there, so no custom resource re-applies it)", () => {
    const eps = Object.values(resourcesOf(runtimes, "AWS::BedrockAgentCore::RuntimeEndpoint")).map((r) => r.Properties as any);
    expect(eps).toHaveLength(2);
    const instEp = eps.find((e) => flat(e.AgentRuntimeId).includes("Instances"))!;
    expect(instEp.Name).toBe("live");
    expect(instEp.AgentRuntimeVersion).toEqual({ "Fn::GetAtt": [expect.stringContaining("Instances"), "AgentRuntimeVersion"] });
    expect(runtimes.findResources("Custom::AWS")).toBeDefined();
    expect(Object.keys(runtimes.findResources("Custom::AWS"))).toHaveLength(1);
  });

  test("the two runtime ARN parameters and the provider id parameter run-session reads", () => {
    const params = Object.values(resourcesOf(runtimes, "AWS::SSM::Parameter")).map((r) => r.Properties as any);
    expect(params.map((p) => p.Name).sort()).toEqual([INSTANCES_ARN_PARAMETER, MICRO_ARN_PARAMETER, PROVIDER_ID_PARAMETER].sort());
    expect(flat(params.find((p) => p.Name === INSTANCES_ARN_PARAMETER)!.Value)).toContain("AgentRuntimeArn");
    expect(flat(params.find((p) => p.Name === PROVIDER_ID_PARAMETER)!.Value)).toContain("CapacityProviderId");
  });

  test("neither runtime carries a description: it is part of the runtime's configuration, so a changed sentence would make AgentCore create a runtime version", () => {
    const rts = runtimesOf(runtimes);
    expect(rts).toHaveLength(2);
    for (const r of rts) expect(r, r.AgentRuntimeName).not.toHaveProperty("Description");
  });

  test("outputs name the new runtime, its id and the provider", () => {
    const outs = Object.keys(runtimes.toJSON().Outputs ?? {});
    expect(outs).toEqual(expect.arrayContaining(["WorkerPythonInstancesRuntimeArn", "WorkerPythonInstancesRuntimeId", "InstancesCapacityProviderArn", "InstancesCapacityProviderId"]));
  });

  test("the phase 1 stage (no worker image) gets the provider and no runtime", () => {
    const t = synth("direct", { computeInstances: "on" }).templates.get("hamh-direct-runtimes")!;
    expect(providerOf(t)).toHaveLength(1);
    expect(runtimesOf(t)).toHaveLength(0);
  });
});

describe("on: the operator role of the security stack", () => {
  const security = on.get("hamh-direct-security")!;
  const role = Object.values(resourcesOf(security, "AWS::IAM::Role")).map((r) => r.Properties as any).find((r) => r.RoleName === "hamh-direct-instances-operator");

  test("AgentCore assumes it, only for this account, and nothing else may", () => {
    expect(role).toBeDefined();
    const st = role.AssumeRolePolicyDocument.Statement;
    expect(st).toHaveLength(1);
    expect(st[0]).toMatchObject({ Effect: "Allow", Action: "sts:AssumeRole", Principal: { Service: "bedrock-agentcore.amazonaws.com" } });
    expect(st[0].Condition).toEqual({ StringEquals: { "aws:SourceAccount": ACCOUNT } });
  });

  test("it carries the AWS-managed operator policy and no inline policy of ours", () => {
    expect(role.ManagedPolicyArns).toEqual([OPERATOR_POLICY]);
    expect(role.Policies).toBeUndefined();
    expect(role.MaxSessionDuration).toBeUndefined();
  });

  test("the runtime base role keeps exactly its eight actions and there is no instance profile or EC2 role anywhere", () => {
    const base = Object.values(resourcesOf(security, "AWS::IAM::Role")).map((r) => r.Properties as any).find((r) => r.RoleName === "hamh-direct-runtime-base");
    const actions = base.Policies.flatMap((p: any) => p.PolicyDocument.Statement).filter((s: any) => s.Effect === "Allow")
      .flatMap((s: any) => (Array.isArray(s.Action) ? s.Action : [s.Action]));
    expect(new Set(actions)).toEqual(new Set(["cloudwatch:PutMetricData", "ecr:BatchGetImage", "ecr:GetAuthorizationToken", "ecr:GetDownloadUrlForLayer",
      "logs:CreateLogGroup", "logs:CreateLogStream", "logs:DescribeLogStreams", "logs:PutLogEvents"]));
    for (const [, t] of on) {
      t.resourceCountIs("AWS::IAM::InstanceProfile", 0);
      expect(flat(t.toJSON())).not.toContain("ec2.amazonaws.com");
    }
  });

  test("no template of ours passes a capacity provider: CloudFormation creates the Instances runtime with the deploying principal's own rights", () => {
    for (const [, t] of on) expect(flat(t.toJSON())).not.toContain("PassCapacityProvider");
    expect(Object.keys(on.get("hamh-direct-runtimes")!.findResources("Custom::AWS"))).toHaveLength(1);
  });
});

describe("on: run-session", () => {
  const orch = on.get("hamh-direct-orchestration")!;
  const fn = Object.values(resourcesOf(orch, "AWS::Lambda::Function"))[0].Properties as any;
  const role = Object.values(resourcesOf(orch, "AWS::IAM::Role")).map((r) => r.Properties as any).find((r) => r.RoleName === "hamh-direct-run-session");
  const stmts: Record<string, any>[] = role.Policies.flatMap((p: any) => p.PolicyDocument.Statement);
  const sid = (s: string) => stmts.find((x) => x.Sid === s);

  test("a cold start takes minutes, so the function may run 900 s (the Lambda maximum); it keeps its 300 s when Instances are off", () => {
    expect(fn.Timeout).toBe(900);
  });

  test("it reads the two new parameters (the runtime ARN and the capacity provider id) from the environment's names", () => {
    expect(fn.Environment.Variables.RUNTIME_ARN_PARAMETER).toBe(MICRO_ARN_PARAMETER);
    expect(fn.Environment.Variables.INSTANCES_RUNTIME_ARN_PARAMETER).toBe(INSTANCES_ARN_PARAMETER);
    expect(fn.Environment.Variables.CAPACITY_PROVIDER_ID_PARAMETER).toBe(PROVIDER_ID_PARAMETER);
    const read = sid("InstancesParameters")!;
    expect(read.Effect).toBe("Allow");
    expect(read.Action).toEqual(["ssm:GetParameter"]);
    expect(read.Resource).toEqual([
      `arn:aws:ssm:${REGION}:${ACCOUNT}:parameter${INSTANCES_ARN_PARAMETER}`,
      `arn:aws:ssm:${REGION}:${ACCOUNT}:parameter${PROVIDER_ID_PARAMETER}`,
    ]);
  });

  test("it may invoke and stop sessions on the Instances runtime and its endpoints, as on the microVM runtime", () => {
    const w = sid("InvokeInstancesWorkers")!;
    expect(w.Effect).toBe("Allow");
    expect(w.Action).toEqual(["bedrock-agentcore:InvokeAgentRuntime", "bedrock-agentcore:StopRuntimeSession", "bedrock-agentcore:GetAgentRuntimeEndpoint"]);
    expect(w.Resource).toHaveLength(3);
    expect(flat(w.Resource[0])).toContain("WorkerPythonInstances");
    expect(flat(w.Resource[1])).toContain("/runtime-endpoint/live");
    expect(flat(w.Resource[2])).toContain("/runtime-endpoint/v*");
  });

  test("it may read and delete sessions of the one capacity provider (DeleteCapacityProviderSession ends the billing), and nothing else of it", () => {
    const m = sid("EndInstancesSessions")!;
    expect(m.Effect).toBe("Allow");
    expect(m.Action).toEqual(["bedrock-agentcore:GetCapacityProvider", "bedrock-agentcore:DeleteCapacityProviderSession"]);
    expect(m.Resource).toHaveLength(1);
    // The provider's ARN is built from its name, as the worker session role's is, and not imported from the runtimes stack: a stack that imports an export of another
    // stack blocks the update that removes it, so a setting that goes from on to off could not complete (infra-iam-2). AgentCore's ids are `<name>-<id>` and a name has no
    // hyphen, so the hyphen and the star leave no other provider in reach.
    expect(m.Resource).toEqual([`arn:aws:bedrock-agentcore:${REGION}:${ACCOUNT}:capacity-provider/${names.capacityProvider(stageConfig("direct"))}-*`]);
    expect(flat(m.Resource)).not.toContain("Fn::ImportValue");
    const allActions = stmts.flatMap((s) => (Array.isArray(s.Action) ? s.Action : [s.Action]));
    for (const forbidden of ["bedrock-agentcore:CreateCapacityProvider", "bedrock-agentcore:UpdateCapacityProvider", "bedrock-agentcore:DeleteCapacityProvider", "bedrock-agentcore:PassCapacityProvider", "ec2:*", "iam:PassRole"]) {
      expect(allActions, forbidden).not.toContain(forbidden);
    }
  });

  test("the microVM statements are unchanged", () => {
    const w = sid("InvokeWorkers")!;
    expect(w.Action).toEqual(["bedrock-agentcore:InvokeAgentRuntime", "bedrock-agentcore:StopRuntimeSession", "bedrock-agentcore:GetAgentRuntimeEndpoint"]);
    expect(w.Resource).toHaveLength(3);
    expect(flat(w.Resource[0])).not.toContain("Instances");
  });
});

// C1 (fix-lane contract): run-session learns of the Instances part through exactly two environment names, each the NAME of an SSM parameter that its role may read.
// crates/run-session reads those two names (compute.rs), so a rename on either side must fail a test here and not a deploy.
describe("on: the environment names run-session reads", () => {
  const envOf = (t: Template) => (Object.values(resourcesOf(t, "AWS::Lambda::Function"))[0].Properties as any).Environment.Variables as Record<string, string>;
  const policy = (t: Template) => (Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any)
    .find((r) => r.RoleName === "hamh-direct-run-session")).Policies.flatMap((p: any) => p.PolicyDocument.Statement) as Record<string, any>[];

  test("on adds exactly INSTANCES_RUNTIME_ARN_PARAMETER and CAPACITY_PROVIDER_ID_PARAMETER and changes no other name or value", () => {
    const o = envOf(off.get("hamh-direct-orchestration")!);
    const n = envOf(on.get("hamh-direct-orchestration")!);
    expect(Object.keys(n).filter((k) => !(k in o)).sort()).toEqual(["CAPACITY_PROVIDER_ID_PARAMETER", "INSTANCES_RUNTIME_ARN_PARAMETER"]);
    expect(Object.keys(o).filter((k) => !(k in n))).toEqual([]);
    for (const k of Object.keys(o)) expect(n[k], k).toEqual(o[k]);
    // A name that run-session would read as the value itself (CAPACITY_PROVIDER_ID) is not set: the value is read from the parameter at run time.
    expect(n).not.toHaveProperty("CAPACITY_PROVIDER_ID");
  });

  test("each of the two values is a parameter name the role may read with GetParameter, and the role may read no other parameter through the Instances statement", () => {
    const n = envOf(on.get("hamh-direct-orchestration")!);
    const read = policy(on.get("hamh-direct-orchestration")!).find((x) => x.Sid === "InstancesParameters")!;
    expect(read.Action).toEqual(["ssm:GetParameter"]);
    const readable = (read.Resource as string[]).map((arn) => arn.replace(`arn:aws:ssm:${REGION}:${ACCOUNT}:parameter`, ""));
    expect(readable.sort()).toEqual([n.CAPACITY_PROVIDER_ID_PARAMETER, n.INSTANCES_RUNTIME_ARN_PARAMETER].sort());
    // They are the names the runtimes stack writes.
    const written = Object.values(resourcesOf(on.get("hamh-direct-runtimes")!, "AWS::SSM::Parameter")).map((r) => (r.Properties as any).Name);
    expect(written).toEqual(expect.arrayContaining(readable));
  });
});

// infra-iam-3: AWS::BedrockAgentCore::CapacityProvider replaces on a change of its Name, ComputeConfiguration or PermissionsConfiguration (CloudFormation resource reference),
// and a capacity provider's name is unique in the account. A fixed name cannot survive a replacement (CloudFormation cannot create the new provider beside the old one),
// so the name carries a short hash of what forces the replacement: a changed configuration is then a new provider next to the old one, and the runtime moves over.
describe("the capacity provider's name follows what forces its replacement", () => {
  const base = stageConfig("direct");
  const withInstances = (patch: Partial<InstancesConfig>): StageConfig => ({ ...base, instances: { ...base.instances, ...patch } });

  test("underscore form, a stage prefix, an eight digit suffix, and within AgentCore's 48 characters", () => {
    const n = names.capacityProvider(base);
    expect(n).toMatch(/^hamh_direct_instances_[0-9a-f]{8}$/);
    expect(n).toMatch(/^[a-zA-Z][a-zA-Z0-9_]{0,47}$/);
  });

  test("the same configuration gives the same name, every part of the compute configuration gives another", () => {
    const n = names.capacityProvider(base);
    expect(names.capacityProvider({ ...base })).toBe(n);
    expect(names.capacityProvider(withInstances({}))).toBe(n);
    const patches: Partial<InstancesConfig>[] = [
      { workspaceGiB: 300 }, { rootFreeGiB: 120 }, { instanceTypes: ["m7g.4xlarge", "m7g.8xlarge"] }, { instanceTypes: ["m7g.8xlarge"] },
      { maxLifetimeS: 14_400 }, { idleInstanceTimeoutS: 900 }, { volumeName: "scratch" },
    ];
    const seen = new Set([n]);
    for (const patch of patches) {
      const other = names.capacityProvider(withInstances(patch));
      expect(other, JSON.stringify(patch)).not.toBe(n);
      expect(seen.has(other), `${JSON.stringify(patch)} repeats an earlier name`).toBe(false);
      seen.add(other);
    }
    // The stage's subnets are part of the compute configuration too (a new Availability Zone is a new subnet list).
    expect(names.capacityProvider({ ...base, azs: [...base.azs, "us-east-1c"] })).not.toBe(n);
  });

  test("what the provider does not carry leaves the name alone: the mount path and the cargo job count belong to the runtime", () => {
    expect(names.capacityProvider(withInstances({ mountPath: "/mnt/other", vcpus: 64 }))).toBe(names.capacityProvider(base));
  });

  // A plain update of a stage that release 5.14 deployed on its default machine keeps HAMH_INSTANCES_TYPES=m7g.4xlarge (scripts/lib/deploy.py hands the kept
  // value to every cdk call). Release 5.15 changed nothing else of the provider's shape, so the provider keeps the name it had and CloudFormation replaces nothing.
  // PROVIDER_514 is the name the 5.14 tree (11cfcaea0) gives stageConfig("direct") with no setting, in this test environment.
  test("b833 the machine a 5.14 stage keeps gives its provider the name 5.14 gave it", () => {
    const PROVIDER_514 = "hamh_direct_instances_3e977628";
    const kept = stageConfig("direct", resolveStageSettings({ instancesTypes: "m7g.4xlarge" }));
    expect(kept.instances.instanceTypes).toEqual(["m7g.4xlarge"]);
    expect(names.capacityProvider(kept)).toBe(PROVIDER_514);
    expect(names.capacityProvider(base), "taking the new default is a new provider").not.toBe(PROVIDER_514);
  });

  test("the template's provider has that name, so a changed configuration is a new resource name and not a replacement under the old one", () => {
    const [provider] = providerOf(on.get("hamh-direct-runtimes")!);
    expect(provider.Name).toBe(names.capacityProvider(base));
    // Every value the hash covers is the value the template sends.
    const ec2 = provider.ComputeConfiguration.Ec2Configuration;
    expect(ec2.LaunchTemplateSource.LaunchParameters.InstanceRequirements.AllowedInstanceTypes).toEqual([...base.instances.instanceTypes]);
    expect(ec2.RootVolume.FreeSpaceGiB).toBe(base.instances.rootFreeGiB);
    expect(ec2.Volumes[0].EbsConfiguration.SizeGiB).toBe(base.instances.workspaceGiB);
    expect(ec2.Volumes[0].EbsConfiguration.Name).toBe(base.instances.volumeName);
    expect(ec2.LifecycleConfiguration).toEqual({ MaxLifetime: base.instances.maxLifetimeS, IdleInstanceTimeout: base.instances.idleInstanceTimeoutS });
  });
});

// infra-iam-2, infra-lifecycle-1, infra-defaults-3: a stack that imports another stack's export cannot be updated into a state where the export is gone. If run-session's grant
// imported the provider's ARN, a stage could not go from on to off at all. The provider's ARN is named by convention instead.
describe("on: the orchestration stack imports nothing of the capacity provider", () => {
  const imports = (t: Template) => [...JSON.stringify(t.toJSON()).matchAll(/"Fn::ImportValue":"([^"]+)"/g)].map((m) => m[1]);

  test("no import names the provider's ARN or id", () => {
    const names_ = imports(on.get("hamh-direct-orchestration")!);
    expect(names_.filter((n) => /CapacityProvider/.test(n))).toEqual([]);
  });

  test("the only export of the runtimes stack that on adds to the orchestration stack's imports is the Instances runtime's ARN (the exact-ARN grant of InvokeInstancesWorkers)", () => {
    const o = new Set(imports(off.get("hamh-direct-orchestration")!));
    const added = imports(on.get("hamh-direct-orchestration")!).filter((n) => !o.has(n));
    // CDK ends an export name with eight hex digits, a hash of the exported token's path.
    expect([...new Set(added.map((n) => n.replace(/[0-9A-F]{8}$/, "")))].sort()).toEqual(["hamh-direct-runtimes:ExportsOutputFnGetAttWorkerPythonInstancesAgentRuntimeArn"]);
  });
});

// infra-defaults-5: the claim "off equals on except where the Instances part says so" is pinned here, as test/tools-mode.test.ts pins the tool layer's setting.
describe("off and on differ only where the Instances part says so", () => {
  const json = (t: Template) => {
    const j = JSON.parse(JSON.stringify(t.toJSON()));
    delete j.Resources.CDKMetadata; // its Analytics string encodes the construct types of the stack, nothing deployed
    return j;
  };

  test("the six stacks that know nothing of Instances are byte for byte the same", () => {
    for (const n of ["network", "data", "packages", "control", "delivery", "observability"]) {
      expect(json(on.get(`hamh-direct-${n}`)!), n).toEqual(json(off.get(`hamh-direct-${n}`)!));
    }
  });

  test("security differs by the operator role alone", () => {
    const o = json(on.get("hamh-direct-security")!);
    expect(Object.keys(o.Resources).filter((id) => !(id in json(off.get("hamh-direct-security")!).Resources))).toEqual(["InstancesOperatorRole"]);
    delete o.Resources.InstancesOperatorRole;
    expect(o).toEqual(json(off.get("hamh-direct-security")!));
  });

  test("orchestration differs by run-session's two environment names, its 900 s timeout and three statements of its role", () => {
    const o = json(on.get("hamh-direct-orchestration")!);
    const fn = Object.values(o.Resources).find((r: any) => r.Type === "AWS::Lambda::Function") as any;
    expect(fn.Properties.Timeout).toBe(900);
    fn.Properties.Timeout = 300;
    delete fn.Properties.Environment.Variables.INSTANCES_RUNTIME_ARN_PARAMETER;
    delete fn.Properties.Environment.Variables.CAPACITY_PROVIDER_ID_PARAMETER;
    const role = Object.values(o.Resources).find((r: any) => r.Type === "AWS::IAM::Role" && r.Properties.RoleName === "hamh-direct-run-session") as any;
    const doc = role.Properties.Policies[0].PolicyDocument;
    const instancesSids = ["InstancesParameters", "EndInstancesSessions", "InvokeInstancesWorkers"];
    expect(doc.Statement.filter((x: any) => instancesSids.includes(x.Sid)).map((x: any) => x.Sid)).toEqual(instancesSids);
    doc.Statement = doc.Statement.filter((x: any) => !instancesSids.includes(x.Sid));
    expect(o).toEqual(json(off.get("hamh-direct-orchestration")!));
  });

  test("runtimes keeps every resource and output of off unchanged and adds only the Instances part", () => {
    const o = json(off.get("hamh-direct-runtimes")!);
    const n = json(on.get("hamh-direct-runtimes")!);
    for (const section of ["Resources", "Outputs"]) {
      for (const [id, v] of Object.entries(o[section] ?? {})) expect(n[section][id], `${section}/${id}`).toEqual(v);
      const added = Object.keys(n[section] ?? {}).filter((id) => !(id in (o[section] ?? {})));
      expect(added.length, section).toBeGreaterThan(0);
      for (const id of added) expect(id, `${section}/${id}`).toMatch(/Instances/);
    }
    for (const k of Object.keys(o)) if (k !== "Resources" && k !== "Outputs") expect(n[k], k).toEqual(o[k]);
  });
});

describe("on: the state machines are unchanged", () => {
  test("the run machine's definition is the same with and without Instances (the cold start fits its heartbeat and timeouts)", () => {
    const a = flat(off.get("hamh-direct-orchestration")!.findResources("AWS::StepFunctions::StateMachine"));
    const b = flat(on.get("hamh-direct-orchestration")!.findResources("AWS::StepFunctions::StateMachine"));
    expect(b).toEqual(a);
  });
});

describe("the whole app", () => {
  test("nine stacks either way, and off equals on only where this file says so (the on stage has strictly more resources)", () => {
    expect(off.size).toBe(9);
    expect(on.size).toBe(9);
    const count = (m: Map<string, Template>) => [...m.values()].reduce((n, t) => n + Object.keys(t.toJSON().Resources ?? {}).length, 0);
    expect(count(on)).toBeGreaterThan(count(off));
  });

  test("building twice is stable", () => {
    const a = buildApp({ stage: "direct", ...phase2Images("direct"), computeInstances: "on" }).synth().stacks.map((s) => s.stackName);
    const b = buildApp({ stage: "direct", ...phase2Images("direct"), computeInstances: "on" }).synth().stacks.map((s) => s.stackName);
    expect(a).toEqual(b);
  });
});
