import { describe, expect, test } from "vitest";
import { Match } from "aws-cdk-lib/assertions";
import { AGENTCORE_SERVICE_PRINCIPAL, ALLOWED_EGRESS_DOMAINS, DEPLOYABLE_REGIONS, PLATFORM_DNS_NAMES, STORAGE_ACCOUNTS_VARIABLE,
  agentCoreStorageStatement, storageAccountOverrides } from "../lib/stacks/network.ts";
import { ACCOUNT, REGION, REMOTE_INFERENCE_REGIONS } from "../lib/config.ts";
import { resourcesOf, template } from "./helpers.ts";

// Egress table (bedrock-mantle.us-west-2.api.aws) and;
// everything else drops. The cross-Region Bedrock names are
// the inference Regions other than the selected home Region.
const REQUIRED_DOMAINS = [
  ...REMOTE_INFERENCE_REGIONS.map((r) => `bedrock-runtime.${r}.amazonaws.com`),
  "bedrock-mantle.us-west-2.api.aws",
  "bedrock-mantle.us-east-1.api.aws",
  "bedrock-mantle.us-east-2.api.aws",
  "api.osv.dev",
];
const FORBIDDEN = ["cline.bot", "registry.npmjs.org", "models.dev", "opencode.ai", "github.com"];
const INTERFACE_SERVICES = ["bedrock-runtime", "execute-api", "logs", "xray", "codeartifact.api",
  "codeartifact.repositories", "bedrock-agentcore", "ecr.api", "ecr.dkr"];

/**
 * Runs `fn` with HAMH_AGENTCORE_STORAGE_ACCOUNTS set to `value` (unset for undefined) and puts the variable back. The default-mode
 * tests pass undefined, so a variable in the environment of the test run cannot change what they read.
 */
function withStorageVariable<T>(value: string | undefined, fn: () => T): T {
  const name = "HAMH_AGENTCORE_STORAGE_ACCOUNTS";
  const before = process.env[name];
  if (value === undefined) delete process.env[name];
  else process.env[name] = value;
  try {
    return fn();
  } finally {
    if (before === undefined) delete process.env[name];
    else process.env[name] = before;
  }
}

describe("network stack", () => {
  const net = withStorageVariable(undefined, () => template("direct", "network"));

  test("private subnets span the stage's one AZ", () => {
    const azs = (t: typeof net, tier: string) => Object.values(resourcesOf(t, "AWS::EC2::Subnet"))
      .filter((r) => JSON.stringify(r.Properties?.Tags).includes(`"${tier}"`))
      .map((r) => r.Properties?.AvailabilityZone);
    expect(new Set(azs(net, "private")).size).toBe(1);
  });

  test("one NAT gateway with an Elastic IP per AZ", () => {
    net.resourceCountIs("AWS::EC2::NatGateway", 1);
    net.resourceCountIs("AWS::EC2::EIP", 1);
  });

  test("the firewall allowlist holds exactly the documented names", () => {
    expect([...ALLOWED_EGRESS_DOMAINS].sort()).toEqual([...REQUIRED_DOMAINS].sort());
    const groups = Object.values(resourcesOf(net, "AWS::NetworkFirewall::RuleGroup"));
    expect(groups).toHaveLength(1);
    const src = (groups[0].Properties as any).RuleGroup.RulesSource.RulesSourceList;
    expect(src.GeneratedRulesType).toBe("ALLOWLIST");
    expect([...src.TargetTypes].sort()).toEqual(["HTTP_HOST", "TLS_SNI"]);
    expect([...src.Targets].sort()).toEqual([...REQUIRED_DOMAINS].sort());
    for (const bad of FORBIDDEN) {
      expect(src.Targets.some((d: string) => d.includes(bad)), bad).toBe(false);
    }
  });

  test("platform DNS names are resolved by the microVM platform only and get no firewall pass rule", () => {
    // Amendment 2026-09-25: the AgentCore microVM resolves time.aws.com itself.
    expect([...PLATFORM_DNS_NAMES]).toEqual(["time.aws.com"]);
    const groups = Object.values(resourcesOf(net, "AWS::NetworkFirewall::RuleGroup"));
    const targets: string[] = (groups[0].Properties as any).RuleGroup.RulesSource.RulesSourceList.Targets;
    for (const n of PLATFORM_DNS_NAMES) {
      expect(targets.includes(n), n).toBe(false);
    }
  });

  test("the firewall drops and alerts on everything the allowlist does not match", () => {
    net.hasResourceProperties("AWS::NetworkFirewall::FirewallPolicy", {
      FirewallPolicy: Match.objectLike({
        StatelessDefaultActions: ["aws:forward_to_sfe"],
        StatelessFragmentDefaultActions: ["aws:forward_to_sfe"],
        StatefulEngineOptions: { RuleOrder: "STRICT_ORDER" },
        StatefulDefaultActions: Match.arrayWith(["aws:drop_established", "aws:alert_established"]),
      }),
    });
    net.hasResourceProperties("AWS::NetworkFirewall::LoggingConfiguration", {
      LoggingConfiguration: {
        LogDestinationConfigs: Match.arrayWith([
          Match.objectLike({ LogType: "ALERT", LogDestinationType: "CloudWatchLogs" }),
          Match.objectLike({ LogType: "FLOW", LogDestinationType: "CloudWatchLogs" }),
        ]),
      },
    });
  });

  test("egress routes run private -> NAT -> firewall -> internet gateway", () => {
    const routes = Object.values(resourcesOf(net, "AWS::EC2::Route")).map((r) => r.Properties ?? {});
    const toNat = routes.filter((r) => r.DestinationCidrBlock === "0.0.0.0/0" && "NatGatewayId" in r);
    const toFirewall = routes.filter((r) => r.DestinationCidrBlock === "0.0.0.0/0" && "VpcEndpointId" in r);
    const toIgw = routes.filter((r) => r.DestinationCidrBlock === "0.0.0.0/0" && "GatewayId" in r);
    expect(toNat).toHaveLength(1);
    expect(toFirewall).toHaveLength(1);
    expect(toIgw).toHaveLength(1);
    // Return traffic from the internet re-enters through the firewall (edge association).
    net.resourceCountIs("AWS::EC2::GatewayRouteTableAssociation", 1);
    const edge = routes.filter((r) => "VpcEndpointId" in r && r.DestinationCidrBlock !== "0.0.0.0/0");
    expect(edge).toHaveLength(1);
  });

  test("workers reach the AgentCore Gateway through its interface endpoint", () => {
    const eps = Object.values(resourcesOf(net, "AWS::EC2::VPCEndpoint")).map((r) => r.Properties as any);
    const gw = eps.filter((e) => JSON.stringify(e.ServiceName).includes("bedrock-agentcore.gateway"));
    expect(gw).toHaveLength(1);
    expect(gw[0].VpcEndpointType).toBe("Interface");
    expect(gw[0].PrivateDnsEnabled).toBe(true);
    expect(JSON.stringify(gw[0].ServiceName)).toContain("com.amazonaws.");
  });

  test("interface endpoints and the S3 gateway endpoint exist", () => {
    const eps = Object.values(resourcesOf(net, "AWS::EC2::VPCEndpoint")).map((r) => r.Properties ?? {});
    const names = eps.map((e) => JSON.stringify(e.ServiceName));
    for (const svc of INTERFACE_SERVICES) {
      expect(names.some((n) => n.includes(`.${svc}"`) || n.endsWith(`.${svc}`)), svc).toBe(true);
    }
    const s3 = eps.filter((e) => e.VpcEndpointType === "Gateway");
    expect(s3).toHaveLength(1);
    const policy = JSON.stringify(s3[0].PolicyDocument);
    expect(policy).toContain("acr-storage-");
    expect(policy).toContain("hamh-direct-");
    // Access Analyzer REDUNDANT_RESOURCE: `*` in a resource ARN also matches `/`, so each bucket
    // pattern already covers its objects; listing `<pattern>/*` too was a finding.
    // One statement per owner: the harness buckets (ours) and AgentCore's session storage (the service's), so each can
    // carry its own condition (the owner and service-principal tests below).
    const stmts = (s3[0].PolicyDocument as any).Statement;
    const buckets = stmts.find((st: any) => st.Sid === "HarnessBuckets");
    expect(buckets.Action).toBe("s3:*");
    expect(buckets.Resource).toEqual(["arn:aws:s3:::hamh-direct-*"]);
    const session = stmts.find((st: any) => st.Sid === "AgentCoreSessionStorage");
    expect(session.Action).toBe("s3:*");
    expect(session.Resource).toEqual([`arn:aws:s3:::acr-storage-*-${REGION}-an`, `arn:aws:s3:::acr-storage-*-${REGION}-an/*`]);
    // AgentCore pulls VPC-mode container images through the VPC: ECR layers live in this bucket,
    // read-only (AgentCore VPC guide). Without it the first microVM start failed: TLS handshake timeout.
    const layer = (s3[0].PolicyDocument as any).Statement.find((st: any) => JSON.stringify(st.Resource).includes("starport-layer-bucket"));
    expect(layer).toBeDefined();
    expect(layer.Action).toEqual(["s3:GetObject"]);
    expect(layer.Resource).toEqual([`arn:aws:s3:::prod-${REGION}-starport-layer-bucket/*`]);
    for (const e of eps.filter((x) => x.VpcEndpointType === "Interface")) {
      expect(e.PrivateDnsEnabled).toBe(true);
    }
  });

  test("Route 53 Resolver query logging covers the VPC (K10)", () => {
    net.resourceCountIs("AWS::Route53Resolver::ResolverQueryLoggingConfig", 1);
    net.resourceCountIs("AWS::Route53Resolver::ResolverQueryLoggingConfigAssociation", 1);
  });

  test("the worker runtime security group lives here, HTTPS egress only and no ingress", () => {
    // Its lifetime is the VPC's: AgentCore keeps agentic_ai ENIs on it after a runtime is deleted,
    // so a group owned by the runtimes stack blocked a rollback until AgentCore released them.
    const sgs = Object.values(resourcesOf(template("direct", "network"), "AWS::EC2::SecurityGroup")).map((r) => r.Properties as any);
    const rt = sgs.filter((s) => String(s.GroupDescription).includes("AgentCore worker runtimes"));
    expect(rt).toHaveLength(1);
    expect(rt[0].SecurityGroupIngress ?? []).toEqual([]);
    expect(rt[0].SecurityGroupEgress).toEqual([{ IpProtocol: "tcp", FromPort: 443, ToPort: 443, CidrIp: "0.0.0.0/0", Description: "HTTPS to endpoints and the NAT allowlist" }]);
  });

  test("every log group has a retention", () => {
    for (const lg of Object.values(resourcesOf(net, "AWS::Logs::LogGroup"))) {
      expect(lg.Properties?.RetentionInDays).toBeGreaterThan(0);
    }
  });

  // Amendment A16 (ported from the retired routeladder.test.ts): a streamed model call outlives the firewall's default
  // idle clock, so the stateful engine holds a flow for 900 s, and the egress allowlist did not widen.
  test("the stateful engine sets tcpIdleTimeoutSeconds to 900 and keeps STRICT_ORDER", () => {
    const policies = Object.values(resourcesOf(net, "AWS::NetworkFirewall::FirewallPolicy")).map((r) => r.Properties as any);
    expect(policies).toHaveLength(1);
    expect(policies[0].FirewallPolicy.StatefulEngineOptions).toEqual({ RuleOrder: "STRICT_ORDER", FlowTimeouts: { TcpIdleTimeoutSeconds: 900 } });
    expect(policies[0].FirewallPolicy.StatefulDefaultActions).toEqual(["aws:drop_established", "aws:alert_established"]);
  });

  test("the egress allowlist is the Bedrock names of the other inference Regions, the Mantle names and the advisory service", () => {
    const groups = Object.values(resourcesOf(net, "AWS::NetworkFirewall::RuleGroup")).map((r) => r.Properties as any);
    expect(groups).toHaveLength(1);
    const src = groups[0].RuleGroup.RulesSource.RulesSourceList;
    expect(src.Targets).toHaveLength(REMOTE_INFERENCE_REGIONS.length + 4);
    for (const t of src.Targets) expect(t, t).toMatch(/^(bedrock-runtime\.[a-z0-9-]+\.amazonaws\.com|bedrock-mantle\.[a-z0-9-]+\.api\.aws|api\.osv\.dev)$/);
    expect(groups[0].RuleGroup.StatefulRuleOptions).toEqual({ RuleOrder: "STRICT_ORDER" });
  });
});

// The S3 gateway endpoint is the one path from the worker VPC to S3 that the Network Firewall never sees, and S3 bucket
// names are global: a policy that names buckets by pattern alone (`hamh-direct-*`, `acr-storage-*`) also admits a bucket of
// ANOTHER account with such a name, a credential-free exfiltration channel (root AGENTS.md invariant 3). Every Allow
// must therefore be bound to something an attacker's bucket cannot supply. Two bindings exist:
//  - the owner of the bucket (aws:ResourceAccount: the account that owns the bucket, documented in the IAM condition-key
//    reference), for the stage's own buckets and, when an operator names the owner, for AgentCore's session storage;
//  - the requester, for AgentCore's session storage: aws:PrincipalServiceName is set only when an AWS service principal makes
//    the request (never for a role session, an access key or an anonymous call), and the bucket pattern is the one AWS
//    documents for the endpoint policy (`acr-storage-*-<region>-an`, the account regional namespace of this Region).
// The ECR layer bucket is AWS-owned and read-only.
type Stmt = { Sid?: string; Effect: string; Action: string | string[]; Resource: string | string[]; Condition?: Record<string, Record<string, unknown>> };
const list = (x: unknown): string[] => (Array.isArray(x) ? x : [x]) as string[];
const ecrLayer = (region: string) => `prod-${region}-starport-layer-bucket`;
const storageBucket = (region: string) => `arn:aws:s3:::acr-storage-*-${region}-an`;

/** The accounts a statement pins through a plain StringEquals on the bucket owner; undefined when it pins none. */
function pinnedOwners(st: Stmt): string[] | undefined {
  const eq = st.Condition?.StringEquals ?? {};
  const v = eq["aws:ResourceAccount"] ?? eq["s3:ResourceAccount"];
  const owners = v === undefined ? [] : list(v);
  return owners.length > 0 && owners.every((o) => /^\d{12}$/.test(o)) ? owners : undefined;
}

/**
 * True when a statement is bound to the AgentCore service principal alone (a plain StringEquals on one key, one value) and
 * names nothing but this Region's session-storage buckets and their objects, with the condition and the resources AWS documents.
 */
function boundToAgentCore(st: Stmt, region: string): boolean {
  const cond = st.Condition ?? {};
  if (Object.keys(cond).join() !== "StringEquals") return false;
  const eq = cond.StringEquals;
  if (Object.keys(eq).join() !== "aws:PrincipalServiceName" || eq["aws:PrincipalServiceName"] !== "bedrock-agentcore.amazonaws.com") return false;
  const bucket = storageBucket(region);
  const resources = list(st.Resource);
  return resources.length > 0 && resources.every((r) => r === bucket || r === `${bucket}/*`);
}

/** The Allow statements that admit a bucket whatever account owns it: not bound, and not the AWS-owned ECR layer read. */
function unguardedAllows(policy: { Statement: Stmt[] }, region: string): string[] {
  return policy.Statement.filter((st) => st.Effect === "Allow").filter((st) => {
    if (pinnedOwners(st) || boundToAgentCore(st, region)) return false;
    const layerRead = list(st.Resource).every((r) => r === `arn:aws:s3:::${ecrLayer(region)}/*`) && list(st.Action).every((a) => a === "s3:GetObject");
    return !layerRead;
  }).map((st) => st.Sid ?? JSON.stringify(st.Resource));
}

describe("S3 gateway endpoint policy: no other account's bucket is reachable", () => {
  // The policy of the stack as the default deploy builds it; `variable` sets HAMH_AGENTCORE_STORAGE_ACCOUNTS for the synth.
  const policyOf = (stage: "direct", variable?: string) => {
    const eps = Object.values(resourcesOf(withStorageVariable(variable, () => template(stage, "network")), "AWS::EC2::VPCEndpoint")).map((r) => r.Properties as any);
    const gw = eps.filter((e) => e.VpcEndpointType === "Gateway");
    expect(gw).toHaveLength(1);
    return gw[0].PolicyDocument as { Statement: Stmt[] };
  };
  const named = (p: { Statement: Stmt[] }, pattern: string) => p.Statement.filter((st) => list(st.Resource).includes(pattern));

  test("the guard flags an Allow on a bucket pattern that does not pin the owner (so the tests below can fail)", () => {
    const bare = { Statement: [{ Sid: "Old", Effect: "Allow", Action: "s3:*", Resource: ["arn:aws:s3:::hamh-direct-*"] }] };
    expect(unguardedAllows(bare, REGION)).toEqual(["Old"]);
    const pinned = { Statement: [{ ...bare.Statement[0], Condition: { StringEquals: { "aws:ResourceAccount": ACCOUNT } } }] };
    expect(unguardedAllows(pinned, REGION)).toEqual([]);
    // An absent key must deny, a pattern can match anything, a list may not be empty, and only an account id pins.
    const notPins: Array<Record<string, Record<string, unknown>>> = [
      { StringEqualsIfExists: { "aws:ResourceAccount": ACCOUNT } }, { StringLike: { "aws:ResourceAccount": "*" } },
      { StringEquals: { "aws:ResourceAccount": [] } }, { StringEquals: { "aws:ResourceAccount": "*" } },
      { StringEquals: { "aws:SourceVpce": "vpce-1" } }, { StringNotEquals: { "aws:ResourceAccount": ACCOUNT } },
    ];
    for (const Condition of notPins) {
      expect(unguardedAllows({ Statement: [{ ...bare.Statement[0], Condition }] }, REGION), JSON.stringify(Condition)).toEqual(["Old"]);
    }
    const layer = { Effect: "Allow", Sid: "Layer", Action: ["s3:GetObject"], Resource: [`arn:aws:s3:::${ecrLayer(REGION)}/*`] };
    expect(unguardedAllows({ Statement: [layer] }, REGION)).toEqual([]);
    // The exemption is the one layer bucket read with GetObject only: a wider action or another bucket is flagged.
    expect(unguardedAllows({ Statement: [{ ...layer, Action: ["s3:*"] }] }, REGION)).toEqual(["Layer"]);
    expect(unguardedAllows({ Statement: [{ ...layer, Resource: ["arn:aws:s3:::prod-*"] }] }, REGION)).toEqual(["Layer"]);
    expect(unguardedAllows({ Statement: [{ ...layer, Resource: [`arn:aws:s3:::${ecrLayer(REGION)}/*`, "arn:aws:s3:::hamh-direct-*"] }] }, REGION)).toEqual(["Layer"]);
  });

  test("the guard accepts the AgentCore service principal only in the documented shape, on this Region's session-storage buckets", () => {
    const bucket = storageBucket(REGION);
    const good: Stmt = { Sid: "Storage", Effect: "Allow", Action: "s3:*", Resource: [bucket, `${bucket}/*`],
      Condition: { StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } } };
    expect(unguardedAllows({ Statement: [good] }, REGION)).toEqual([]);
    // The key is absent for a role session, an access key and an anonymous call, so a form that lets an absent key through
    // (IfExists, a negation) or a pattern or a list value (any service) does not bind the requester.
    const notBound: Array<Record<string, Record<string, unknown>>> = [
      { StringEqualsIfExists: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } },
      { StringNotEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } },
      { StringLike: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } },
      { StringEquals: { "aws:PrincipalServiceName": "*" } },
      { StringEquals: { "aws:PrincipalServiceName": "bedrock.amazonaws.com" } },
      { StringEquals: { "aws:PrincipalServiceName": ["bedrock-agentcore.amazonaws.com", "s3.amazonaws.com"] } },
      { StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com", "aws:SourceVpce": "vpce-1" } },
      { StringEquals: { "aws:PrincipalServiceNames": "bedrock-agentcore.amazonaws.com" } },
      { StringEquals: { "aws:PrincipalType": "Service" } },
    ];
    for (const Condition of notBound) {
      expect(unguardedAllows({ Statement: [{ ...good, Condition }] }, REGION), JSON.stringify(Condition)).toEqual(["Storage"]);
    }
    // The binding does not carry to a wider or another bucket pattern: the owner position is a wildcard only inside the
    // account regional namespace of the stage Region.
    for (const Resource of [["arn:aws:s3:::acr-storage-*"], ["arn:aws:s3:::*"], [`${bucket}/*`, "arn:aws:s3:::hamh-direct-*"],
      [`arn:aws:s3:::acr-storage-*-${REGION}-an*`], ["arn:aws:s3:::acr-storage-*-eu-west-1-an"], [`arn:aws:s3:::acr-*-${REGION}-an`],
      [`arn:aws:s3:::acr-storage-*-${REGION}`], [`arn:aws:s3:::acr-storage-*-${REGION}-an/*/*`]]) {
      expect(unguardedAllows({ Statement: [{ ...good, Resource }] }, REGION), JSON.stringify(Resource)).toEqual(["Storage"]);
    }
    // No condition at all admits every account's bucket of that name.
    expect(unguardedAllows({ Statement: [{ ...good, Condition: undefined }] }, REGION)).toEqual(["Storage"]);
  });

  test.each(["direct"] as const)("every Allow of the %s endpoint pins the bucket owner or the AgentCore service principal, except the ECR layer read", (stage) => {
    expect(unguardedAllows(policyOf(stage), REGION)).toEqual([]);
    // The same holds when an operator names the owner of the session storage.
    expect(unguardedAllows(policyOf(stage, `${REGION}=111122223333`), REGION)).toEqual([]);
  });

  test.each(["direct"] as const)("the %s harness buckets are admitted only when this account owns them", (stage) => {
    const own = named(policyOf(stage), `arn:aws:s3:::hamh-${stage}-*`);
    expect(own).toHaveLength(1);
    expect(own[0].Effect).toBe("Allow");
    expect(own[0].Action).toBe("s3:*");
    // The account comes from the stack environment (accounts.json through config.ts), as a plain StringEquals: an
    // IfExists form would let a request without the key through.
    expect(own[0].Condition).toEqual({ StringEquals: { "aws:ResourceAccount": ACCOUNT } });
    // No other hamh-* pattern (a retired stage's buckets included) is named anywhere in the policy.
    expect(JSON.stringify(policyOf(stage))).not.toMatch(/hamh-(live|drill)-/);
  });

  test.each(["direct"] as const)("AgentCore's session storage on the %s endpoint is admitted only to the AgentCore service principal, on this Region's account regional buckets", (stage) => {
    const policy = policyOf(stage);
    const bucket = storageBucket(REGION);
    const sess = named(policy, bucket);
    expect(sess).toHaveLength(1);
    expect(sess[0].Effect).toBe("Allow");
    // The platform mounts /mnt/workspace through this endpoint and writes to it: s3:* stays (the ECR layer statement is the GetObject one).
    expect(sess[0].Action).toBe("s3:*");
    // The bucket pattern ends in `-an`, so the objects need their own entry (AWS documents both).
    expect(list(sess[0].Resource)).toEqual([bucket, `${bucket}/*`]);
    // A plain StringEquals on the one key AWS documents: the key is absent for a role session, an access key and an anonymous
    // call, so none of them, whatever bucket they name, is admitted. No account number appears anywhere in the statement.
    expect(sess[0].Condition).toEqual({ StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } });
    expect(AGENTCORE_SERVICE_PRINCIPAL).toBe("bedrock-agentcore.amazonaws.com");
    expect(JSON.stringify(sess[0])).not.toMatch(/\d{12}/);
    // No other statement reaches the acr-storage family, and the whole policy stays within the 20,480 characters of an endpoint policy.
    expect(policy.Statement.filter((st) => JSON.stringify(st.Resource).includes("acr-storage"))).toHaveLength(1);
    expect(JSON.stringify(policy, null, 1).length).toBeLessThan(20_480);
  });

  test("a stage in each deployable Region gets the same default statement, shaped for its Region, without any account number", () => {
    expect([...DEPLOYABLE_REGIONS]).toEqual(["us-east-1", "us-west-2"]);
    for (const region of DEPLOYABLE_REGIONS) {
      const st = agentCoreStorageStatement(region, undefined);
      expect(st, region).toMatchObject({ Sid: "AgentCoreSessionStorage", Effect: "Allow", Principal: "*", Action: "s3:*" });
      expect(st.Resource, region).toEqual([storageBucket(region), `${storageBucket(region)}/*`]);
      expect(st.Condition, region).toEqual({ StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } });
      expect(JSON.stringify(st), region).not.toMatch(/\d{12}/);
    }
  });

  test("a Region the app does not deploy in fails synth, with a message that names the Region, the two it accepts and where to change it", () => {
    for (const variable of [undefined, "eu-west-1=111122223333", "us-east-1=111122223333"]) {
      let message = "";
      try {
        agentCoreStorageStatement("eu-west-1", variable);
      } catch (e) {
        message = String((e as Error).message);
      }
      expect(message, String(variable)).toMatch(/eu-west-1/);
      expect(message, String(variable)).toMatch(/us-east-1.*us-west-2/);
      expect(message, String(variable)).toMatch(/accounts\.json|--region/);
      // The variable changes who may use the storage bucket; it does not make a Region deployable, and the text must not suggest it does.
      expect(message, String(variable)).not.toContain(STORAGE_ACCOUNTS_VARIABLE);
    }
    expect(() => agentCoreStorageStatement("", undefined)).toThrow(/Region/);
    expect(() => agentCoreStorageStatement("US-EAST-1", undefined)).toThrow(/US-EAST-1/);
    expect(() => agentCoreStorageStatement("us-east-1; us-west-2", undefined)).toThrow(/Region/);
  });

  test("an operator who names the owner of the session storage gets the owner pin for that Region only", () => {
    expect(STORAGE_ACCOUNTS_VARIABLE).toBe("HAMH_AGENTCORE_STORAGE_ACCOUNTS");
    const raw = "us-east-1=111122223333+444455556666, us-west-2=777788889999";
    const east = agentCoreStorageStatement("us-east-1", raw);
    expect(east.Resource).toEqual([storageBucket("us-east-1"), `${storageBucket("us-east-1")}/*`]);
    expect(east.Action).toBe("s3:*");
    expect(east.Condition).toEqual({ StringEquals: { "aws:ResourceAccount": ["111122223333", "444455556666"] } });
    expect(agentCoreStorageStatement("us-west-2", raw).Condition).toEqual({ StringEquals: { "aws:ResourceAccount": ["777788889999"] } });
    // A Region the variable does not name keeps the default; the entry for the other Region is checked but not used.
    expect(agentCoreStorageStatement("us-west-2", "us-east-1=111122223333").Condition)
      .toEqual({ StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } });
    // Empty and blank mean "not set", as for the other HAMH_ variables.
    for (const blank of [undefined, "", "   ", " , "]) {
      expect(agentCoreStorageStatement("us-east-1", blank).Condition, JSON.stringify(blank)).toEqual({ StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } });
    }
  });

  test("the variable is read when the stack is built, replaces the default for its Region and leaves nothing of the default behind", () => {
    const policy = policyOf("direct", `${REGION}=111122223333`);
    const sess = named(policy, storageBucket(REGION));
    expect(sess).toHaveLength(1);
    expect(sess[0].Condition).toEqual({ StringEquals: { "aws:ResourceAccount": ["111122223333"] } });
    expect(JSON.stringify(policy)).not.toContain("aws:PrincipalServiceName");
    // The environment of the run is put back.
    expect(named(policyOf("direct"), storageBucket(REGION))[0].Condition).toEqual({ StringEquals: { "aws:PrincipalServiceName": "bedrock-agentcore.amazonaws.com" } });
  });

  test("the variable is checked strictly: a malformed, repeated, empty or foreign entry fails synth and names the variable", () => {
    expect(storageAccountOverrides(undefined)).toEqual({});
    expect(storageAccountOverrides("")).toEqual({});
    expect(storageAccountOverrides("us-east-1=111122223333")).toEqual({ "us-east-1": ["111122223333"] });
    expect(storageAccountOverrides(" us-east-1=111122223333+444455556666 , us-west-2=777788889999 "))
      .toEqual({ "us-east-1": ["111122223333", "444455556666"], "us-west-2": ["777788889999"] });
    const bad = ["us-east-1:oops", "us-east-1", "us-east-1=", "=111122223333", "us-east-1=1111", "us-east-1=1111222233334",
      "us-east-1=111122223333+", "us-east-1=+111122223333", "us-east-1=*", "us-east-1=1111222233*", "us-east-1=111122223333 444455556666",
      "us-east-1=111122223333+111122223333", "us-east-1=111122223333,us-east-1=444455556666", "us-east-1=111122223333,,oops",
      "eu-west-1=111122223333", "US-EAST-1=111122223333", "us-east-1 = 111122223333", "us-east-1=1111-2222-3333"];
    for (const raw of bad) {
      expect(() => storageAccountOverrides(raw), raw).toThrow(/HAMH_AGENTCORE_STORAGE_ACCOUNTS/);
      expect(() => agentCoreStorageStatement("us-east-1", raw), raw).toThrow(/HAMH_AGENTCORE_STORAGE_ACCOUNTS/);
    }
    // A foreign Region says which Regions are valid.
    expect(() => storageAccountOverrides("eu-west-1=111122223333")).toThrow(/us-east-1.*us-west-2/);
  });

  test("the ECR layer read is untouched: GetObject on the one AWS-owned bucket of the Region, no account condition", () => {
    const layer = policyOf("direct").Statement.filter((st) => JSON.stringify(st.Resource).includes("starport-layer-bucket"));
    expect(layer).toHaveLength(1);
    expect(layer[0]).toMatchObject({ Sid: "EcrImageLayers", Effect: "Allow" });
    expect(list(layer[0].Action)).toEqual(["s3:GetObject"]);
    expect(list(layer[0].Resource)).toEqual([`arn:aws:s3:::${ecrLayer(REGION)}/*`]);
    expect(layer[0].Condition).toBeUndefined();
  });

  test("the endpoint keeps its logical id and immutable properties, so the policy change updates it in place", () => {
    // AWS::EC2::VPCEndpoint PolicyDocument: "Update requires: No interruption"; ServiceName, VpcEndpointType and VpcId
    // replace the endpoint (CloudFormation template reference). A new logical id would replace it too.
    const eps = resourcesOf(template("direct", "network"), "AWS::EC2::VPCEndpoint");
    const gw = Object.entries(eps).filter(([, e]) => (e.Properties as any).VpcEndpointType === "Gateway");
    expect(gw.map(([id]) => id)).toEqual(["S3Endpoint"]);
    expect(JSON.stringify((gw[0][1].Properties as any).ServiceName)).toContain(".s3");
  });
});
