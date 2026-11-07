// Runtimes stack, stage `direct`.
// Phase 1 (no image yet): the python worker's ECR repository and the image-build project (scripts/codebuild-run.sh builds the stage's
// images there). Phase 2 (a digest-pinned image): the runtime, a custom resource that re-applies requireMMDSV2 and platform V1 through
// UpdateAgentRuntime on every configuration change (CloudFormation has neither property and UpdateAgentRuntime replaces the whole
// configuration), and the `live` endpoint on that version. The stage deploys the python worker alone, unless `HAMH_COMPUTE_INSTANCES=on`:
// then the stack also holds an Instances capacity provider (EC2 machines in the stage's VPC) in both phases and, in phase 2, a second worker runtime on the same image that
// runs on it, with its own `live` endpoint and runtime ARN parameter.
import { CfnOutput, Stack, type StackProps } from "aws-cdk-lib";
import * as agentcore from "aws-cdk-lib/aws-bedrockagentcore";
import * as ecr from "aws-cdk-lib/aws-ecr";
import * as iam from "aws-cdk-lib/aws-iam";
import * as ssm from "aws-cdk-lib/aws-ssm";
import * as cr from "aws-cdk-lib/custom-resources";
import { createHash } from "node:crypto";
import type { Construct } from "constructs";
import { ACCOUNT, instancesProviderShape, names, type StageConfig, type ToolsMode } from "../config.ts";
import { DIRECT_IMAGE_REPOSITORIES, addImageBuild } from "./image-build.ts";
import type { NetworkStack } from "./network.ts";
import type { SecurityStack } from "./security.ts";

const IDLE_TIMEOUT_S = 900;
const MAX_LIFETIME_S = 28_800; // AgentCore's 8-hour session limit
const SESSION_STORAGE = "/mnt/workspace";

const KEEP_IMAGES = JSON.stringify({
  rules: [{ rulePriority: 1, description: "keep the newest 30 images", selection: { tagStatus: "any", countType: "imageCountMoreThan", countNumber: 30 }, action: { type: "expire" } }],
});

export interface RuntimesStackProps extends StackProps {
  cfg: StageConfig;
  network: NetworkStack;
  /** `<repo>@sha256:<digest>` of the python worker image; absent in phase 1. */
  workerImage?: string;
  /** HAMH_WORKER_REVISION: a bump is an environment change, so a new runtime version. */
  workerRevision: number;
  /** The data key's stack: the stage holds its image-build project here and the project encrypts with the key. */
  security: SecurityStack;
  /** The CodeArtifact repository agentd writes the package index for. */
  packages: { domainName: string; repositoryName: string };
  /** The tool layer's stage setting, resolved: the worker runtime's `AGENTD_TOOLS_MODE`. Always a concrete word, so the runtime never leaves the mode to agentd's default. */
  toolsMode: ToolsMode;
  /** Create the Instances capacity provider and the second runtime (`HAMH_COMPUTE_INSTANCES=on`). Default false: the stack is the one it was. */
  computeInstances?: boolean;
}

type Compute = { kind: "microvm" } | { kind: "instances"; capacityProviderArn: string; volumeName: string; mountPath: string };

interface RuntimeSpec {
  /** Construct id prefix, `WorkerPython`. */
  id: string;
  /** The custom resource's physical id prefix and the SSM parameter's runtime name, `worker-python`. */
  slug: string;
  name: string;
  image: string;
  roleArn: string;
  compute: Compute;
  environmentVariables: Record<string, string>;
  endpointDescription: string;
  parameterDescription: string;
  lifecycle?: { idleRuntimeSessionTimeout: number; maxLifetime: number };
}

function pinned(image: string, what: string): void {
  if (!/@sha256:[0-9a-f]{64}$/.test(image)) {
    throw new Error(`${what} must be pinned by digest (<repo>@sha256:<64 hex>), got ${image}`);
  }
}

export class RuntimesStack extends Stack {
  readonly workerRepositoryName: string;
  readonly workerRuntimeArn?: string;
  /** The python worker runtime's ARN, when its image exists (the run-session grants). */
  readonly workerRuntimeArns: { python?: string; instances?: string } = {};

  constructor(scope: Construct, id: string, props: RuntimesStackProps) {
    super(scope, id, props);
    const { cfg } = props;
    const p = cfg.prefix;
    this.workerRepositoryName = `${p}-worker-python`;

    const repo = new ecr.CfnRepository(this, "WorkerPythonRepo", {
      repositoryName: this.workerRepositoryName, imageTagMutability: "IMMUTABLE",
      imageScanningConfiguration: { scanOnPush: true }, lifecyclePolicy: { lifecyclePolicyText: KEEP_IMAGES },
    });
    new CfnOutput(this, "WorkerPythonRepositoryUri", { value: repo.attrRepositoryUri, description: "push target for the python worker image" });
    // The project scripts/codebuild-run.sh builds the stage's images in (hamh-direct-image-build) lives here, beside the repository it
    // pushes to; a fresh stage's first (image-less) pass creates it before the first image build.
    addImageBuild(this, cfg, props.security, { roleName: "image-build", repositories: DIRECT_IMAGE_REPOSITORIES });

    // The Instances capacity provider, in both phases (it needs no image). Opt-in.
    const provider = props.computeInstances === true ? this.capacityProvider(props) : undefined;

    // The worker runtime: agentd, the Cline fork and the language toolchain.
    if (props.workerImage !== undefined) {
      pinned(props.workerImage, "workerImage");
      const environmentVariables: Record<string, string> = {
        HAMH_STAGE: cfg.stage, HAMH_WORKER_REVISION: String(props.workerRevision),
        AGENTD_PACKAGES_DOMAIN: props.packages.domainName, AGENTD_PACKAGES_OWNER: ACCOUNT,
        AGENTD_PACKAGES_REPOSITORY: props.packages.repositoryName,
        // The Go module store. agentd hands the URL to the lock step and leaves it for the gate commands;
        // the URL names a read-only bucket the worker VPC's S3 gateway endpoint reaches and carries no credential.
        AGENTD_GOPROXY: names.gomodProxyUrl(cfg),
        // The kill switch of agentd's tool server. A change is a change of the runtime's environment, so a new runtime version (the custom
        // resource below re-sends the whole configuration); scripts/lib/deploy.py guards it like a bump. `off` unless the operator set HAMH_TOOLS_MODE=on.
        AGENTD_TOOLS_MODE: props.toolsMode,
        // How agentd keeps the engine away from its own secrets. The microVM has a second user for it (`uid`: agentd runs as root and starts the
        // engine as uid 901); an Instances container has no capabilities, so there the engine runs in a Landlock sandbox (`landlock`). The mode is the deployment's, never
        // a user setting (root invariant 2 is locked), and agentd refuses to start the engine when the word is missing or not one of the two.
        AGENTD_ENGINE_SANDBOX: "uid",
      };
      this.workerRuntimeArn = this.runtime(props, {
        id: "WorkerPython", slug: "worker-python",
        name: `${p.replace(/-/g, "_")}_harness_worker_python`,
        image: props.workerImage,
        roleArn: `arn:aws:iam::${ACCOUNT}:role/${names.role(cfg, "runtime-base")}`,
        compute: { kind: "microvm" },
        environmentVariables,
        endpointDescription: `${p} python worker: the requireMMDSV2 version`,
        parameterDescription: `${p} python worker runtime ARN (InvokeAgentRuntime target)`,
        lifecycle: { idleRuntimeSessionTimeout: cfg.worker.idleTimeoutS, maxLifetime: cfg.worker.maxLifetimeS },
      }).attrAgentRuntimeArn;
      this.workerRuntimeArns.python = this.workerRuntimeArn;

      // The same image on the capacity provider. The same environment and execution role (the eight-action runtime base role) as the microVM runtime, with the words
      // below. The runtime's lifetime may not exceed the provider's and its idle timeout stays within the provider's idle limit.
      if (provider !== undefined) {
        const i = cfg.instances;
        this.workerRuntimeArns.instances = this.runtime(props, {
          id: "WorkerPythonInstances", slug: "worker-python-instances",
          name: names.instancesRuntime(cfg),
          image: props.workerImage,
          roleArn: `arn:aws:iam::${ACCOUNT}:role/${names.role(cfg, "runtime-base")}`,
          compute: { kind: "instances", capacityProviderArn: provider.attrArn, volumeName: i.volumeName, mountPath: i.mountPath },
          environmentVariables: {
            ...environmentVariables,
            // An Instances container has no capabilities, so the engine runs in a Landlock sandbox as agentd's own user. agentd refuses `landlock` while
            // AGENTD_ENGINE_USER names another user, and the worker image sets it to `agent` (the microVM runtime keeps that), so this runtime overrides it with the empty string.
            AGENTD_ENGINE_SANDBOX: "landlock",
            AGENTD_ENGINE_USER: "",
            // Agentd calls the checkout mount full at 960 MiB (the managed session storage of a microVM holds 1 GiB). Here the mount is a volume of `workspaceGiB`,
            // and 0 switches that check off (a limit of 960 MiB would stop a run with 140 GiB free). Cargo gets one job per vCPU of the machine, not the microVM's 2.
            AGENTD_WORKSPACE_LIMIT_MB: "0",
            AGENTD_DIRECT_CARGO_JOBS: String(i.vcpus),
          },
          endpointDescription: `${p} python worker on Instances: the version of the runtime`,
          parameterDescription: `${p} python worker runtime ARN on Instances (InvokeAgentRuntime target)`,
          lifecycle: {
            idleRuntimeSessionTimeout: Math.min(cfg.worker.idleTimeoutS, i.idleInstanceTimeoutS),
            maxLifetime: Math.min(cfg.worker.maxLifetimeS, i.maxLifetimeS),
          },
        }).attrAgentRuntimeArn;
      }
    }
  }

  /**
   * The Instances capacity provider: EC2 machines that AgentCore launches in the stage's own private subnets behind the runtime security group, so the NAT, the Network
   * Firewall allowlist, the endpoints and the S3 `aws:SourceVpce`/`aws:SourceIp` conditions apply to them as they do to a microVM. AgentCore picks the machine from the allow-list;
   * each machine has an encrypted gp3 root disk and one encrypted gp3 workspace volume that the Instances runtime mounts. It has NO instance profile,
   * so the only credentials a process on the machine can reach are the execution role's, as on a microVM.
   * AgentCore assumes the operator role of the security stack to launch and clean up the machines. The id goes to an SSM parameter for run-session, which ends a session with
   * `DeleteCapacityProviderSession` (the call that ends the billing and removes the machine and its disks).
   */
  private capacityProvider(props: RuntimesStackProps): agentcore.CfnCapacityProvider {
    const { cfg, network } = props;
    // Everything that replaces the provider comes from one object, and the name carries its hash (config.ts `instancesProviderShape`), so none of it can change without a new name.
    const shape = instancesProviderShape(cfg);
    const provider = new agentcore.CfnCapacityProvider(this, "InstancesCapacityProvider", {
      name: names.capacityProvider(cfg),
      description: `${cfg.prefix} Instances capacity provider: the worker runtime on EC2 machines in the stage VPC`,
      permissionsConfiguration: { capacityProviderOperatorRoleArn: `arn:aws:iam::${ACCOUNT}:role/${shape.operatorRole}` },
      computeConfiguration: {
        ec2Configuration: {
          launchTemplateSource: {
            launchParameters: {
              operatingSystem: shape.operatingSystem,
              instanceRequirements: { allowedInstanceTypes: shape.allowedInstanceTypes },
              monitoring: shape.monitoring,
              propagatedTags: shape.propagatedTags,
            },
          },
          vpcConfiguration: { subnets: network.privateSubnetIds, securityGroups: [network.runtimeSecurityGroupId] },
          rootVolume: shape.rootVolume,
          volumes: shape.volumes.map((ebsConfiguration) => ({ ebsConfiguration })),
          lifecycleConfiguration: shape.lifecycle,
        },
      },
    });
    new CfnOutput(this, "InstancesCapacityProviderArn", { value: provider.attrArn, description: "the Instances capacity provider" });
    new CfnOutput(this, "InstancesCapacityProviderId", { value: provider.attrCapacityProviderId });
    // run-session reads it when it ends a session (DeleteCapacityProviderSession takes the id, not the ARN).
    new ssm.CfnParameter(this, "InstancesCapacityProviderIdParameter", {
      name: names.capacityProviderIdParameter(cfg), type: "String", value: provider.attrCapacityProviderId,
      description: `${cfg.prefix} Instances capacity provider id (DeleteCapacityProviderSession target)`,
    });
    return provider;
  }

  /**
   * A runtime on its image, its `live` endpoint, outputs and the runtime ARN parameter. A microVM runtime also gets the requireMMDSV2 custom resource (CloudFormation has neither
   * the flag nor the platform version, and UpdateAgentRuntime replaces the whole configuration), and its `live` endpoint serves the version that resource creates. An Instances
   * runtime has no network configuration and no session storage (the machine's network comes from the capacity provider and its workspace volume is mounted by the runtime),
   * names the provider, and needs no custom resource: AgentCore sets `requireMMDSV2` to true and the platform version to V1 on it by default,
   * so its `live` endpoint serves the runtime's own version.
   */
  private runtime(props: RuntimesStackProps, s: RuntimeSpec): agentcore.CfnRuntime {
    const { network } = props;
    const p = props.cfg.prefix;
    const lifecycleConfiguration = s.lifecycle ?? { idleRuntimeSessionTimeout: IDLE_TIMEOUT_S, maxLifetime: MAX_LIFETIME_S };
    const micro = s.compute.kind === "microvm";
    const networkConfiguration = micro
      ? { networkMode: "VPC", networkModeConfig: { subnets: network.privateSubnetIds, securityGroups: [network.runtimeSecurityGroupId] } }
      : undefined;
    const filesystemConfigurations = s.compute.kind === "instances"
      ? [{ capacityProviderVolume: { volumeName: s.compute.volumeName, mountPath: s.compute.mountPath } }]
      : [{ sessionStorage: { mountPath: SESSION_STORAGE } }];
    const capacityProviderConfiguration = s.compute.kind === "instances" ? { capacityProviderArn: s.compute.capacityProviderArn } : undefined;
    // No `description`: it is part of the runtime's configuration, and AgentCore makes a new runtime version for every change of that configuration. CloudFormation's own
    // version lacks the requireMMDSV2 flag and the custom resource below applies it only when the image changes, so a sentence edited after a deploy gave the next update
    // a version without the flag as the newest one (the DEFAULT endpoint) while `live` stayed behind. The runtime name says what the runtime is.
    const runtime = new agentcore.CfnRuntime(this, s.id, {
      agentRuntimeName: s.name,
      agentRuntimeArtifact: { containerConfiguration: { containerUri: s.image } },
      roleArn: s.roleArn,
      ...(networkConfiguration ? { networkConfiguration } : {}),
      protocolConfiguration: "HTTP",
      filesystemConfigurations,
      ...(capacityProviderConfiguration ? { capacityProviderConfiguration } : {}),
      lifecycleConfiguration,
      environmentVariables: s.environmentVariables,
    });
    new CfnOutput(this, `${s.id}RuntimeArn`, { value: runtime.attrAgentRuntimeArn, description: "InvokeAgentRuntime target" });
    new CfnOutput(this, `${s.id}RuntimeId`, { value: runtime.attrAgentRuntimeId });
    // run-session reads it when a session starts: no cross-stack reference to a phase-2 resource.
    new ssm.CfnParameter(this, `${s.id}RuntimeArnParameter`, {
      name: names.runtimeArnParameter(props.cfg, s.slug), type: "String", value: runtime.attrAgentRuntimeArn,
      description: s.parameterDescription,
    });

    let liveVersion: string = runtime.attrAgentRuntimeVersion;
    if (micro) {
      // UpdateAgentRuntime replaces the whole configuration, so send all of it with the flag.
      const update = {
        // The v3 package name: CDK would lowercase "BedrockAgentCoreControl" to a package that does not exist.
        service: "@aws-sdk/client-bedrock-agentcore-control",
        action: "updateAgentRuntime",
        parameters: {
          agentRuntimeId: runtime.attrAgentRuntimeId,
          agentRuntimeArtifact: { containerConfiguration: { containerUri: s.image } },
          roleArn: s.roleArn,
          networkConfiguration,
          protocolConfiguration: { serverProtocol: "HTTP" },
          filesystemConfigurations,
          lifecycleConfiguration,
          environmentVariables: s.environmentVariables,
          metadataConfiguration: { requireMMDSV2: true },
          platformVersion: "V1",
        },
        // The endpoint below pins the version this call creates; nothing else is read back.
        outputPaths: ["agentRuntimeVersion"],
        // Keyed on the image, so every new image (every new runtime version) re-applies the flag.
        physicalResourceId: cr.PhysicalResourceId.of(
          `${p}-${s.slug}-mmdsv2-${createHash("sha256").update(s.image).digest("hex").slice(0, 16)}`),
      };
      const mmds = new cr.AwsCustomResource(this, `${s.id}MmdsV2`, {
        onCreate: update,
        onUpdate: update,
        // The Lambda-bundled SDK may predate metadataConfiguration and platformVersion, which
        // SDK v3 drops silently. deploy.sh verifies the flag on the live version either way.
        installLatestAwsSdk: true,
        policy: cr.AwsCustomResourcePolicy.fromStatements([
          new iam.PolicyStatement({ actions: ["bedrock-agentcore:UpdateAgentRuntime", "bedrock-agentcore:GetAgentRuntime"], resources: [runtime.attrAgentRuntimeArn] }),
          new iam.PolicyStatement({ actions: ["iam:PassRole"], resources: [s.roleArn],
            conditions: { StringEquals: { "iam:PassedToService": "bedrock-agentcore.amazonaws.com" } } }),
          new iam.PolicyStatement({ actions: ["ec2:DescribeSubnets", "ec2:DescribeSecurityGroups", "ec2:DescribeVpcs"], resources: ["*"] }),
        ]),
      });
      mmds.node.addDependency(runtime);
      liveVersion = mmds.getResponseField("agentRuntimeVersion");
    }

    // CloudFormation's own runtime version of a microVM runtime lacks the flag, so `live` serves the custom resource's
    // version; deploy.sh checks it and hashes its image into the deployment digest. An Instances runtime serves its own.
    new agentcore.CfnRuntimeEndpoint(this, `${s.id}Live`, {
      agentRuntimeId: runtime.attrAgentRuntimeId,
      name: "live",
      agentRuntimeVersion: liveVersion,
      description: s.endpointDescription,
    });
    return runtime;
  }
}
