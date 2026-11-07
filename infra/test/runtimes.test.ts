import { describe, expect, test } from "vitest";
import { Template } from "aws-cdk-lib/assertions";
import { buildApp } from "../lib/app.ts";
import { ACCOUNT, REGION } from "../lib/config.ts";
import { resourcesOf, template } from "./helpers.ts";

const IMAGE = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-worker-python@sha256:` + "a".repeat(64);
// Agentd points pip and npm at the packages stack's CodeArtifact repository.
const PACKAGES_ENV = { AGENTD_PACKAGES_DOMAIN: "hamh-direct", AGENTD_PACKAGES_OWNER: `${ACCOUNT}`, AGENTD_PACKAGES_REPOSITORY: "packages" };
// The stage's Go module store (test/gomod.test.ts pins the bucket and its policy), the module source every worker and verifier names.
const GOMOD_URL = `https://hamh-direct-gomod-${ACCOUNT}.s3.${REGION}.amazonaws.com`;

function withImage(image = IMAGE, workerRevision?: number): Template {
  const app = buildApp({ stage: "direct", workerImage: image, workerRevision });
  const stack = app.node.children.find((c: any) => c.stackName === "hamh-direct-runtimes") as any;
  return Template.fromStack(stack);
}

describe("runtimes stack", () => {
  test("phase 1 without an image: the ECR repository and the image-build project, no runtime", () => {
    const t = template("direct", "runtimes");
    const repos = Object.values(resourcesOf(t, "AWS::ECR::Repository")).map((r) => r.Properties as any);
    expect(repos.map((r) => r.RepositoryName)).toEqual(["hamh-direct-worker-python"]);
    for (const r of repos) {
      expect(r.ImageTagMutability).toBe("IMMUTABLE");
      expect(r.ImageScanningConfiguration).toEqual({ ScanOnPush: true });
      expect(r.LifecyclePolicy.LifecyclePolicyText).toBeDefined();
    }
    t.resourceCountIs("AWS::BedrockAgentCore::Runtime", 0);
  });

  test("a digest-pinned image is required for a runtime", () => {
    expect(() => buildApp({ stage: "direct", workerImage: `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-worker-python:latest` }))
      .toThrow(/digest/);
  });

  test("the worker runtime runs the image in VPC mode with the base role and session storage", () => {
    const t = withImage();
    const rts = Object.values(resourcesOf(t, "AWS::BedrockAgentCore::Runtime")).map((r) => r.Properties as any);
    expect(rts).toHaveLength(1);
    const r = rts[0];
    expect(r.AgentRuntimeName).toBe("hamh_direct_harness_worker_python");
    expect(r.AgentRuntimeArtifact.ContainerConfiguration.ContainerUri).toBe(IMAGE);
    expect(JSON.stringify(r.RoleArn)).toContain("hamh-direct-runtime-base");
    expect(r.NetworkConfiguration.NetworkMode).toBe("VPC");
    expect(r.NetworkConfiguration.NetworkModeConfig.Subnets).toHaveLength(1);
    expect(r.NetworkConfiguration.NetworkModeConfig.SecurityGroups).toHaveLength(1);
    expect(r.ProtocolConfiguration).toBe("HTTP");
    expect(r.FilesystemConfigurations).toEqual([{ SessionStorage: { MountPath: "/mnt/workspace" } }]);
    // 8 hours, and 1,800 s idle to cover a gate run plus an evaluator pass between turns.
    expect(r.LifecycleConfiguration).toEqual({ IdleRuntimeSessionTimeout: 1800, MaxLifetime: 28800 });
  });

  test("a worker runtime carries no description, so editing a sentence never makes AgentCore create a runtime version", () => {
    // The description is part of the runtime's configuration: AgentCore makes a new runtime version for every change of it. CloudFormation's own version lacks the
    // requireMMDSV2 flag and the custom resource below re-applies the flag only when the image changes, so a description-only change left `live` on the old version
    // and the newest version (the DEFAULT endpoint) without the flag. No runtime resource sets one.
    const rts = Object.values(resourcesOf(withImage(), "AWS::BedrockAgentCore::Runtime")).map((r) => r.Properties as any);
    expect(rts).toHaveLength(1);
    for (const r of rts) expect(r, "Description").not.toHaveProperty("Description");
    const cr = JSON.stringify(Object.values(withImage().findResources("Custom::AWS"))[0]);
    expect(cr).not.toContain("description");
  });

  test("the runtime uses the network stack's runtime security group and creates none", () => {
    const t = withImage();
    t.resourceCountIs("AWS::EC2::SecurityGroup", 0);
    const r = Object.values(resourcesOf(t, "AWS::BedrockAgentCore::Runtime"))[0].Properties as any;
    const groups = r.NetworkConfiguration.NetworkModeConfig.SecurityGroups;
    expect(groups).toHaveLength(1);
    expect(JSON.stringify(groups[0])).toContain("hamh-direct-network");
  });

  test("a custom resource re-applies requireMMDSV2 and platform V1 with the full configuration", () => {
    const t = withImage();
    const crs = Object.values(t.findResources("Custom::AWS")).map((r: any) => r.Properties);
    expect(crs).toHaveLength(1);
    const text = JSON.stringify(crs[0]);
    for (const needle of ["updateAgentRuntime", "requireMMDSV2", "\\\"platformVersion\\\":\\\"V1\\\"", "/mnt/workspace",
      "\\\"serverProtocol\\\":\\\"HTTP\\\"", "\\\"networkMode\\\":\\\"VPC\\\"", "sha256:" + "a".repeat(64)]) {
      expect(text, needle).toContain(needle);
    }
  });

  test("the custom resource loads the v3 client by package name and installs the current SDK", () => {
    // CDK lowercases unknown v2 service names to @aws-sdk/client-bedrockagentcorecontrol, which does
    // not exist; the Lambda-bundled SDK may predate metadataConfiguration and platformVersion, which
    // SDK v3 drops silently. deploy.sh checks the flag on the live version either way.
    const cr = Object.values(withImage().findResources("Custom::AWS"))[0] as any;
    // Create is an Fn::Join of the call JSON (it embeds tokens), so match the serialized text.
    expect(JSON.stringify(cr.Properties.Create)).toContain('\\"service\\":\\"@aws-sdk/client-bedrock-agentcore-control\\"');
    expect(cr.Properties.InstallLatestAwsSdk).toBe(true);
  });

  test("the worker revision is a runtime environment variable in the runtime and the custom resource", () => {
    const t = withImage(IMAGE, 3);
    const r = Object.values(resourcesOf(t, "AWS::BedrockAgentCore::Runtime"))[0].Properties as any;
    // The microVM runtime says which isolation agentd uses for the engine; agentd refuses to start the engine without the word.
    expect(r.EnvironmentVariables).toEqual({ HAMH_STAGE: "direct", HAMH_WORKER_REVISION: "3", ...PACKAGES_ENV, AGENTD_GOPROXY: GOMOD_URL, AGENTD_TOOLS_MODE: "off", AGENTD_ENGINE_SANDBOX: "uid" });
    const cr = JSON.stringify(Object.values(t.findResources("Custom::AWS"))[0]);
    expect(cr).toContain('\\"environmentVariables\\":{\\"HAMH_STAGE\\":\\"direct\\",\\"HAMH_WORKER_REVISION\\":\\"3\\",'
      + `\\"AGENTD_PACKAGES_DOMAIN\\":\\"hamh-direct\\",\\"AGENTD_PACKAGES_OWNER\\":\\"${ACCOUNT}\\",`
      + `\\"AGENTD_PACKAGES_REPOSITORY\\":\\"packages\\",\\"AGENTD_GOPROXY\\":\\"${GOMOD_URL}\\",\\"AGENTD_TOOLS_MODE\\":\\"off\\",\\"AGENTD_ENGINE_SANDBOX\\":\\"uid\\"}`);
    const r0 = Object.values(resourcesOf(withImage(), "AWS::BedrockAgentCore::Runtime"))[0].Properties as any;
    expect(r0.EnvironmentVariables.HAMH_WORKER_REVISION).toBe("0");
  });

  test("a revision bump is a CloudFormation environment change that reruns the custom resource (S2)", () => {
    const create = (rev: number) => JSON.stringify((Object.values(withImage(IMAGE, rev).findResources("Custom::AWS"))[0] as any).Properties.Create);
    expect(create(1)).not.toEqual(create(0));
  });

  test("the custom resource reruns when the image changes", () => {
    const a = JSON.stringify(Object.values(withImage().findResources("Custom::AWS"))[0]);
    const b = JSON.stringify(Object.values(withImage(IMAGE.replace(/a{64}$/, "b".repeat(64))).findResources("Custom::AWS"))[0]);
    const idOf = (s: string) => JSON.parse(s).Properties.Create;
    expect(idOf(a)).not.toEqual(idOf(b));
  });

  test("a live endpoint serves the version the requireMMDSV2 update created", () => {
    // deploymentDigest covers the runtime versions the live endpoints reference. The CFN
    // runtime's own version lacks the flag, so live must pin the custom resource's version.
    const t = withImage();
    const eps = Object.entries(resourcesOf(t, "AWS::BedrockAgentCore::RuntimeEndpoint"));
    expect(eps).toHaveLength(1);
    const [, ep] = eps[0] as [string, any];
    expect(ep.Properties.Name).toBe("live");
    expect(ep.Properties.AgentRuntimeId).toEqual({ "Fn::GetAtt": ["WorkerPython", "AgentRuntimeId"] });
    const crId = Object.keys(t.findResources("Custom::AWS"))[0];
    expect(ep.Properties.AgentRuntimeVersion).toEqual({ "Fn::GetAtt": [crId, "agentRuntimeVersion"] });
    const create = JSON.parse(JSON.stringify((Object.values(t.findResources("Custom::AWS"))[0] as any).Properties.Create));
    expect(JSON.stringify(create)).toContain("outputPaths");
    expect(JSON.stringify(create)).toContain("agentRuntimeVersion");
  });

  test("phase 1 has no endpoint", () => {
    template("direct", "runtimes").resourceCountIs("AWS::BedrockAgentCore::RuntimeEndpoint", 0);
  });

  test("the custom resource role may update only this runtime and pass only the base role to AgentCore", () => {
    const t = withImage();
    const policies = Object.values(resourcesOf(t, "AWS::IAM::Policy")).map((r) => (r.Properties as any).PolicyDocument.Statement).flat();
    const update = policies.find((s: any) => JSON.stringify(s.Action).includes("bedrock-agentcore:UpdateAgentRuntime"));
    expect(update).toBeDefined();
    // Exactly this runtime: its own ARN attribute, not a name pattern.
    expect(update.Resource).toEqual({ "Fn::GetAtt": ["WorkerPython", "AgentRuntimeArn"] });
    const pass = policies.find((s: any) => JSON.stringify(s.Action).includes("iam:PassRole"));
    expect(pass).toBeDefined();
    expect(JSON.stringify(pass.Resource)).toContain("hamh-direct-runtime-base");
    expect(JSON.stringify(pass.Condition)).toContain("bedrock-agentcore.amazonaws.com");
  });
});

describe("runtimes stack outputs", () => {
  test("the repository URI is an output in both phases, the runtime ARN and id once a runtime exists", () => {
    const phase1 = template("direct", "runtimes").toJSON().Outputs ?? {};
    expect(Object.keys(phase1)).toContain("WorkerPythonRepositoryUri");
    expect(Object.keys(phase1)).not.toContain("WorkerPythonRuntimeArn");
    const phase2 = withImage().toJSON().Outputs ?? {};
    expect(Object.keys(phase2)).toEqual(expect.arrayContaining(["WorkerPythonRepositoryUri", "WorkerPythonRuntimeArn", "WorkerPythonRuntimeId"]));
  });

  test("the worker runtime ARN is published for run-session (SSM), only when it exists", () => {
    const params = Object.values(resourcesOf(withImage(), "AWS::SSM::Parameter")).map((r) => r.Properties as any);
    const p = params.find((x) => x.Name === "/hamh/direct/worker-python-runtime-arn");
    expect(p).toBeDefined();
    expect(p.Type).toBe("String");
    expect(JSON.stringify(p.Value)).toContain("AgentRuntimeArn");
    template("direct", "runtimes").resourceCountIs("AWS::SSM::Parameter", 0);
  });
});
