// Network stack.
// Egress path: private subnet -> NAT gateway (public subnet) -> Network Firewall endpoint
// (firewall subnet) -> internet gateway. Return traffic re-enters through the firewall via the
// internet gateway edge route table. AWS service traffic in the home Region uses VPC endpoints.
import { Stack, type StackProps } from "aws-cdk-lib";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import * as iam from "aws-cdk-lib/aws-iam";
import * as logs from "aws-cdk-lib/aws-logs";
import * as nfw from "aws-cdk-lib/aws-networkfirewall";
import * as r53r from "aws-cdk-lib/aws-route53resolver";
import * as cr from "aws-cdk-lib/custom-resources";
import type { Construct } from "constructs";
import { REGION, remoteInferenceRegions, type StageConfig } from "../config.ts";
import { lookupEnv } from "../env.ts";

/**
 * The only names reachable through the NAT path from a stage whose home Region is `home`; everything
 * else is dropped and logged (`*.cline.bot`, `registry.npmjs.org`, `models.dev`, `opencode.ai`,
 * `github.com` included).
 */
export function egressDomains(home: string): string[] {
  return [
    ...remoteInferenceRegions(home).map((r) => `bedrock-runtime.${r}.amazonaws.com`),
    "bedrock-mantle.us-west-2.api.aws",
    "bedrock-mantle.us-east-1.api.aws",
    "bedrock-mantle.us-east-2.api.aws",
    "api.osv.dev",
  ];
}

/** The allowlist of the selected account's home Region (config.ts `REGION`). */
export const ALLOWED_EGRESS_DOMAINS: readonly string[] = egressDomains(REGION);

/** Names the AgentCore microVM platform resolves on its own (its clock
 *  sync queried time.aws.com 25 times in a 24 minute window). DNS only: no firewall pass rule, so
 *  they appear in no rule group; their Resolver queries are accepted. */
export const PLATFORM_DNS_NAMES: readonly string[] = ["time.aws.com"];

/** Home-Region AWS services reached through interface endpoints. AgentCore
 *  pulls VPC-mode container images through the VPC, so ECR needs both of its endpoints; workers
 *  reach the harness gateway (`*.gateway.bedrock-agentcore.<region>.amazonaws.com`) through its own. */
export const INTERFACE_ENDPOINT_SERVICES: readonly string[] = [
  "bedrock-runtime", "execute-api", "logs", "xray", "codeartifact.api", "codeartifact.repositories",
  "bedrock-agentcore", "ecr.api", "ecr.dkr", "bedrock-agentcore.gateway",
];

/**
 * The home Regions a stage deploys in. The egress allowlist, the model catalog and the AgentCore session-storage policy are set
 * up for these Regions only; any other Region fails synth.
 */
export const DEPLOYABLE_REGIONS: readonly string[] = ["us-east-1", "us-west-2"];

/**
 * The service principal that AgentCore Runtime's managed session storage acts as when it reads and writes its bucket,
 * `acr-storage-<account>-<region>-an`. The worker runtime mounts /mnt/workspace on that bucket through the S3 gateway
 * endpoint, so the endpoint policy must admit it. The bucket pattern of the Region and the condition `aws:PrincipalServiceName`
 * set to this value are the ones AWS documents for this endpoint (File system configurations for AgentCore Runtime, "Managed
 * session storage networking"). The statement allows `s3:*`, which is wider than the three actions of AWS's example
 * (`s3:GetObject`, `s3:PutObject`, `s3:ListBucket`): AWS does not document the calls the service's storage sync makes, so the
 * actions are not narrowed, and the bucket pattern and the service principal scope the statement. The key is present only when an AWS service principal makes the request, and absent for
 * a role session, an access key and an anonymous request, so code in a worker cannot use the endpoint to reach a bucket
 * of another account, whatever that bucket is called. The statement needs no account number.
 */
export const AGENTCORE_SERVICE_PRINCIPAL = "bedrock-agentcore.amazonaws.com";

/**
 * Optional: `<region>=<account>[+<account>]`, comma separated. For each Region it names, the session-storage statement admits
 * the buckets that those accounts own (`aws:ResourceAccount`) instead of the AgentCore service principal. The account is the
 * one that the Route 53 Resolver query log of the worker VPC shows in `acr-storage-<account>-<region>-an`. Use it to pin the
 * owner, or when the default does not mount.
 */
export const STORAGE_ACCOUNTS_VARIABLE = "HAMH_AGENTCORE_STORAGE_ACCOUNTS";

/** The entries of `STORAGE_ACCOUNTS_VARIABLE` by Region. Unset, empty or blank is no entry; any other text that is not of that shape throws. */
export function storageAccountOverrides(raw: string | undefined): Record<string, readonly string[]> {
  const out: Record<string, readonly string[]> = {};
  for (const part of (raw ?? "").split(",").map((s) => s.trim()).filter((s) => s.length > 0)) {
    const m = /^([a-z]{2}(?:-[a-z]+)+-[0-9])=([0-9]{12}(?:\+[0-9]{12})*)$/.exec(part);
    if (m === null) {
      throw new Error(`network: ${STORAGE_ACCOUNTS_VARIABLE} entry ${JSON.stringify(part)} does not match region=account or region=account+account.`);
    }
    const [, region, accountList] = m;
    if (!DEPLOYABLE_REGIONS.includes(region)) {
      throw new Error(`network: ${STORAGE_ACCOUNTS_VARIABLE} names Region ${region}, but a stage deploys only in ${DEPLOYABLE_REGIONS.join(" and ")}`);
    }
    if (Object.hasOwn(out, region)) {
      throw new Error(`network: ${STORAGE_ACCOUNTS_VARIABLE} names Region ${region} twice`);
    }
    const accounts = accountList.split("+");
    if (new Set(accounts).size !== accounts.length) {
      throw new Error(`network: ${STORAGE_ACCOUNTS_VARIABLE} names an account twice for ${region}`);
    }
    out[region] = accounts;
  }
  return out;
}

/** The endpoint-policy statement for AgentCore's session storage in `region`; `raw` is the value of `STORAGE_ACCOUNTS_VARIABLE`. */
export function agentCoreStorageStatement(region: string, raw: string | undefined): {
  Sid: string; Effect: "Allow"; Principal: "*"; Action: "s3:*"; Resource: string[]; Condition: { StringEquals: Record<string, string | string[]> };
} {
  if (!DEPLOYABLE_REGIONS.includes(region)) {
    throw new Error(`network: the stage Region ${JSON.stringify(region)} is outside the deployable Regions ${DEPLOYABLE_REGIONS.join(" and ")}. Choose one of them as "region" in your accounts.json entry or with setup.sh --region.`);
  }
  // The variable is read in full first, so a malformed entry fails whatever the Region of the stack is.
  const owners = storageAccountOverrides(raw);
  // The bucket name ends in `-an`, so the objects need an entry of their own (the pattern AWS documents lists both).
  const bucket = `arn:aws:s3:::acr-storage-*-${region}-an`;
  return {
    Sid: "AgentCoreSessionStorage", Effect: "Allow", Principal: "*", Action: "s3:*", Resource: [bucket, `${bucket}/*`],
    Condition: { StringEquals: Object.hasOwn(owners, region)
      ? { "aws:ResourceAccount": [...owners[region]] }
      : { "aws:PrincipalServiceName": AGENTCORE_SERVICE_PRINCIPAL } },
  };
}

export interface NetworkStackProps extends StackProps {
  cfg: StageConfig;
}

export class NetworkStack extends Stack {
  readonly vpcId: string;
  readonly vpcCidr: string;
  readonly privateSubnetIds: string[] = [];
  readonly endpointSecurityGroupId: string;
  /** Every worker runtime's security group; lives with the VPC (AgentCore ENIs outlive a runtime). */
  readonly runtimeSecurityGroupId: string;
  /** NAT gateway Elastic IPs: the worker role's aws:SourceIp condition. */
  readonly natPublicIps: string[] = [];
  /** Every worker-VPC endpoint (interface and S3 gateway): the worker role's aws:SourceVpce condition. */
  readonly endpointIds: string[] = [];
  /** The execute-api interface endpoint: the private /internal API binds to it. */
  executeApiEndpointId = "";
  /** The S3 gateway endpoint: the Go module store's bucket policy admits anonymous reads that come through it. */
  s3EndpointId = "";

  constructor(scope: Construct, id: string, props: NetworkStackProps) {
    super(scope, id, props);
    const { cfg } = props;
    const p = cfg.prefix;
    const name = (n: string) => [{ key: "Name", value: `${p}-${n}` }];

    const vpc = new ec2.CfnVPC(this, "Vpc", {
      cidrBlock: cfg.vpcCidr, enableDnsHostnames: true, enableDnsSupport: true, tags: name("vpc"),
    });
    this.vpcId = vpc.ref;
    this.vpcCidr = cfg.vpcCidr;

    const igw = new ec2.CfnInternetGateway(this, "Igw", { tags: name("igw") });
    const igwAttach = new ec2.CfnVPCGatewayAttachment(this, "IgwAttach", { vpcId: vpc.ref, internetGatewayId: igw.ref });

    // Octets of the /16: private /20 at 16*i, public /24 at 128+i, firewall /28 at 140+i.
    const [a, b] = cfg.vpcCidr.split(".");
    const subnet = (tier: string, i: number, az: string, cidr: string) => new ec2.CfnSubnet(this, `${tier}${i}`, {
      vpcId: vpc.ref, availabilityZone: az, cidrBlock: cidr, mapPublicIpOnLaunch: false,
      tags: [...name(`${tier}-${az}`), { key: "tier", value: tier }],
    });

    const logGroup = (id: string, logName: string) => new logs.CfnLogGroup(this, id, {
      logGroupName: `/hamh/${cfg.stage}/network/${logName}`, retentionInDays: cfg.logRetentionDays,
    });

    // Network Firewall: stateful domain allowlist, strict order, drop and alert everything else.
    const ruleGroup = new nfw.CfnRuleGroup(this, "EgressAllowlist", {
      ruleGroupName: `${p}-egress-allowlist`, type: "STATEFUL", capacity: 100,
      ruleGroup: {
        rulesSource: {
          rulesSourceList: {
            generatedRulesType: "ALLOWLIST", targetTypes: ["TLS_SNI", "HTTP_HOST"], targets: [...ALLOWED_EGRESS_DOMAINS],
          },
        },
        statefulRuleOptions: { ruleOrder: "STRICT_ORDER" },
      },
    });
    const policy = new nfw.CfnFirewallPolicy(this, "FirewallPolicy", {
      firewallPolicyName: `${p}-egress`,
      firewallPolicy: {
        statelessDefaultActions: ["aws:forward_to_sfe"],
        statelessFragmentDefaultActions: ["aws:forward_to_sfe"],
        // The firewall drops a TCP flow with no bytes for 350 s by default. A long
        // silent model call crosses NAT and this firewall, so the idle clock is 900 s here (defence in depth; the NAT
        // gateway's 350 s is fixed and agentd's 30 s TCP keepalive covers it). Changing it can restart the stateful
        // engine, so it deploys before the drill.
        statefulEngineOptions: { ruleOrder: "STRICT_ORDER", flowTimeouts: { tcpIdleTimeoutSeconds: 900 } },
        statefulDefaultActions: ["aws:drop_established", "aws:alert_established"],
        statefulRuleGroupReferences: [{ resourceArn: ruleGroup.attrRuleGroupArn, priority: 100 }],
      },
    });

    const firewallSubnets: ec2.CfnSubnet[] = [];
    const publicSubnets: ec2.CfnSubnet[] = [];
    cfg.azs.forEach((az, i) => {
      firewallSubnets.push(subnet("firewall", i, az, `${a}.${b}.${140 + i}.0/28`));
      publicSubnets.push(subnet("public", i, az, `${a}.${b}.${128 + i}.0/24`));
    });

    const firewall = new nfw.CfnFirewall(this, "Firewall", {
      firewallName: `${p}-egress`, firewallPolicyArn: policy.attrFirewallPolicyArn, vpcId: vpc.ref,
      subnetMappings: firewallSubnets.map((s) => ({ subnetId: s.ref })), deleteProtection: false,
      subnetChangeProtection: false, firewallPolicyChangeProtection: false,
    });
    const alertLogs = logGroup("FirewallAlertLogs", "firewall-alert");
    const flowLogs = logGroup("FirewallFlowLogs", "firewall-flow");
    new nfw.CfnLoggingConfiguration(this, "FirewallLogging", {
      firewallArn: firewall.attrFirewallArn,
      loggingConfiguration: {
        logDestinationConfigs: [
          { logType: "ALERT", logDestinationType: "CloudWatchLogs", logDestination: { logGroup: alertLogs.ref } },
          { logType: "FLOW", logDestinationType: "CloudWatchLogs", logDestination: { logGroup: flowLogs.ref } },
        ],
      },
    });

    // CloudFormation lists firewall endpoints unordered; read the endpoint of each AZ explicitly.
    const fwEndpoints = new cr.AwsCustomResource(this, "FirewallEndpoints", {
      onUpdate: {
        service: "NetworkFirewall", action: "describeFirewall",
        parameters: { FirewallArn: firewall.attrFirewallArn },
        physicalResourceId: cr.PhysicalResourceId.of(`${p}-firewall-endpoints`),
        outputPaths: cfg.azs.map((az) => `FirewallStatus.SyncStates.${az}.Attachment.EndpointId`),
      },
      policy: cr.AwsCustomResourcePolicy.fromStatements([
        new iam.PolicyStatement({ actions: ["network-firewall:DescribeFirewall"], resources: [firewall.attrFirewallArn] }),
      ]),
      installLatestAwsSdk: false,
    });
    const fwEndpoint = (az: string) => fwEndpoints.getResponseField(`FirewallStatus.SyncStates.${az}.Attachment.EndpointId`);

    const edgeRt = new ec2.CfnRouteTable(this, "EdgeRt", { vpcId: vpc.ref, tags: name("rt-igw-edge") });
    new ec2.CfnGatewayRouteTableAssociation(this, "EdgeAssoc", { gatewayId: igw.ref, routeTableId: edgeRt.ref })
      .addResourceDependency(igwAttach);

    const privateRts: ec2.CfnRouteTable[] = [];
    cfg.azs.forEach((az, i) => {
      // One firewall route table per AZ keeps each AZ's egress path independent.
      const firewallRt = new ec2.CfnRouteTable(this, `FirewallRt${i}`, { vpcId: vpc.ref, tags: name(`rt-firewall-${az}`) });
      new ec2.CfnSubnetRouteTableAssociation(this, `FirewallAssoc${i}`, { subnetId: firewallSubnets[i].ref, routeTableId: firewallRt.ref });
      new ec2.CfnRoute(this, `FirewallDefault${i}`, { routeTableId: firewallRt.ref, destinationCidrBlock: "0.0.0.0/0", gatewayId: igw.ref })
        .addResourceDependency(igwAttach);

      const pub = publicSubnets[i];
      const publicRt = new ec2.CfnRouteTable(this, `PublicRt${i}`, { vpcId: vpc.ref, tags: name(`rt-public-${az}`) });
      new ec2.CfnSubnetRouteTableAssociation(this, `PublicAssoc${i}`, { subnetId: pub.ref, routeTableId: publicRt.ref });
      new ec2.CfnRoute(this, `PublicDefault${i}`, { routeTableId: publicRt.ref, destinationCidrBlock: "0.0.0.0/0", vpcEndpointId: fwEndpoint(az) });
      new ec2.CfnRoute(this, `EdgeToPublic${i}`, { routeTableId: edgeRt.ref, destinationCidrBlock: pub.cidrBlock, vpcEndpointId: fwEndpoint(az) });

      const eip = new ec2.CfnEIP(this, `NatEip${i}`, { domain: "vpc", tags: name(`nat-${az}`) });
      eip.addResourceDependency(igwAttach);
      this.natPublicIps.push(eip.attrPublicIp);
      const nat = new ec2.CfnNatGateway(this, `Nat${i}`, { subnetId: pub.ref, allocationId: eip.attrAllocationId, tags: name(`nat-${az}`) });

      const priv = subnet("private", i, az, `${a}.${b}.${16 * i}.0/20`);
      this.privateSubnetIds.push(priv.ref);
      const privateRt = new ec2.CfnRouteTable(this, `PrivateRt${i}`, { vpcId: vpc.ref, tags: name(`rt-private-${az}`) });
      privateRts.push(privateRt);
      new ec2.CfnSubnetRouteTableAssociation(this, `PrivateAssoc${i}`, { subnetId: priv.ref, routeTableId: privateRt.ref });
      new ec2.CfnRoute(this, `PrivateDefault${i}`, { routeTableId: privateRt.ref, destinationCidrBlock: "0.0.0.0/0", natGatewayId: nat.ref });
    });

    // Interface endpoints for home-Region services; HTTPS from inside the VPC only.
    const endpointSg = new ec2.CfnSecurityGroup(this, "EndpointSg", {
      vpcId: vpc.ref, groupDescription: `${p} interface endpoints: HTTPS from the VPC`,
      securityGroupIngress: [{ ipProtocol: "tcp", fromPort: 443, toPort: 443, cidrIp: cfg.vpcCidr }],
      // An endpoint network interface only receives connections. The group says so (cfn_nag F1000): the one rule below stands in for the
      // implicit allow-all egress rule and matches no traffic a network interface sends (the loopback address). The ICMP idiom of the CDK
      // L2 construct would trade F1000 for W29 (a port range on an egress rule); a single TCP port trades it for nothing.
      securityGroupEgress: [{ ipProtocol: "tcp", fromPort: 1, toPort: 1, cidrIp: "127.0.0.1/32", description: "No outbound traffic: interface endpoints only receive" }],
      tags: name("sg-endpoints"),
    });
    this.endpointSecurityGroupId = endpointSg.attrGroupId;

    // Worker runtimes: HTTPS egress only. It lives here because AgentCore keeps its agentic_ai
    // ENIs on the group for a while after a runtime is deleted; teardown waits on them.
    const runtimeSg = new ec2.CfnSecurityGroup(this, "RuntimeSg", {
      vpcId: vpc.ref, groupDescription: `${p} AgentCore worker runtimes: HTTPS egress only`,
      securityGroupEgress: [{ ipProtocol: "tcp", fromPort: 443, toPort: 443, cidrIp: "0.0.0.0/0", description: "HTTPS to endpoints and the NAT allowlist" }],
      tags: name("sg-runtimes"),
    });
    this.runtimeSecurityGroupId = runtimeSg.attrGroupId;
    for (const svc of INTERFACE_ENDPOINT_SERVICES) {
      const ep = new ec2.CfnVPCEndpoint(this, `Endpoint-${svc.replace(/\./g, "-")}`, {
        vpcId: vpc.ref, serviceName: `com.amazonaws.${this.region}.${svc}`, vpcEndpointType: "Interface",
        privateDnsEnabled: true, subnetIds: this.privateSubnetIds, securityGroupIds: [endpointSg.attrGroupId],
      });
      this.endpointIds.push(ep.ref);
      if (svc === "execute-api") this.executeApiEndpointId = ep.ref;
    }

    // S3 gateway endpoint scoped to the harness buckets and AgentCore session storage buckets. Bucket names are global, so
    // a pattern alone also admits a bucket of ANOTHER account with such a name, through the one path the Network Firewall
    // never sees (invariant 3). Each pattern therefore carries a binding that a foreign bucket cannot supply: the owner of the
    // buckets it admits (`aws:ResourceAccount`, the account that owns the bucket) for the stage's own buckets, and for AgentCore's
    // session storage the AgentCore service principal (or, when an operator names the owner, that owner). Plain StringEquals, so
    // a request without the key is denied. The ECR layer bucket is AWS-owned and read-only. Changing only PolicyDocument updates
    // the endpoint in place; keep its logical id "S3Endpoint".
    const s3Endpoint = new ec2.CfnVPCEndpoint(this, "S3Endpoint", {
      vpcId: vpc.ref, serviceName: `com.amazonaws.${this.region}.s3`, vpcEndpointType: "Gateway",
      routeTableIds: privateRts.map((rt) => rt.ref),
      policyDocument: {
        Version: "2012-10-17",
        Statement: [
          {
            // Each bucket pattern also matches its objects (`*` spans `/`; Access Analyzer REDUNDANT_RESOURCE).
            Sid: "HarnessBuckets", Effect: "Allow", Principal: "*", Action: "s3:*",
            Resource: [`arn:aws:s3:::${p}-*`],
            Condition: { StringEquals: { "aws:ResourceAccount": this.account } },
          },
          // The platform mounts session storage (/mnt/workspace) through this endpoint and writes to it.
          agentCoreStorageStatement(this.region, lookupEnv(process.env, "AGENTCORE_STORAGE_ACCOUNTS")),
          {
            // ECR image layers for VPC-mode container pulls (AgentCore VPC guide), read-only.
            Sid: "EcrImageLayers", Effect: "Allow", Principal: "*", Action: ["s3:GetObject"],
            Resource: [`arn:aws:s3:::prod-${this.region}-starport-layer-bucket/*`],
          },
        ],
      },
    });
    this.endpointIds.push(s3Endpoint.ref);
    this.s3EndpointId = s3Endpoint.ref;

    // Resolver query logging: every DNS name a worker resolves.
    const dnsLogs = logGroup("ResolverQueryLogs", "resolver-queries");
    const qlc = new r53r.CfnResolverQueryLoggingConfig(this, "ResolverQueryLogging", {
      name: `${p}-worker-dns`, destinationArn: dnsLogs.attrArn,
    });
    new r53r.CfnResolverQueryLoggingConfigAssociation(this, "ResolverQueryLoggingAssoc", {
      resolverQueryLogConfigId: qlc.attrId, resourceId: vpc.ref,
    });
  }
}
