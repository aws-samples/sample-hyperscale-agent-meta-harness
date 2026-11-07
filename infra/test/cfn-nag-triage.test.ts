import { describe, expect, test } from "vitest";
import { resourcesOf, synth, template } from "./helpers.ts";

// F1000 is fixed in the template. F16 and F19 are acknowledged where the resource is defined, with the scanner's own mechanism
// (Metadata.cfn_nag.rules_to_suppress) and a reason that is true of that resource. These tests pin the three outcomes so a later edit cannot
// drop the fix or the reason.
type Ack = { id: string; reason: string };
const acks = (resource: unknown): Ack[] => ((resource as { Metadata?: { cfn_nag?: { rules_to_suppress?: Ack[] } } }).Metadata?.cfn_nag?.rules_to_suppress) ?? [];

describe("cfn_nag triage of the synthesized templates", () => {
  test("F1000: the interface endpoint group states its egress and allows none", () => {
    const groups = Object.values(resourcesOf(template("direct", "network"), "AWS::EC2::SecurityGroup"));
    const endpoints = groups.filter((g) => JSON.stringify(g.Properties?.Tags).includes("sg-endpoints"));
    expect(endpoints).toHaveLength(1);
    const props = endpoints[0].Properties as { SecurityGroupIngress: unknown; SecurityGroupEgress?: unknown };
    // One rule that matches no traffic a network interface sends (TCP port 1 to the loopback address). It is not the CDK L2 idiom (an ICMP
    // rule with a type and code as its port range), which cfn_nag reports as W29.
    expect(props.SecurityGroupEgress).toEqual([
      { CidrIp: "127.0.0.1/32", IpProtocol: "tcp", FromPort: 1, ToPort: 1, Description: expect.stringMatching(/no outbound/i) },
    ]);
    expect(props.SecurityGroupIngress).toEqual([expect.objectContaining({ IpProtocol: "tcp", FromPort: 443, ToPort: 443 })]);
  });

  test("F19: the asymmetric engine policy signing key acknowledges the rotation rule with its reason", () => {
    const keys = Object.values(template("direct", "security").findResources("AWS::KMS::Key"));
    const signing = keys.filter((k) => (k.Properties as { KeyUsage?: string }).KeyUsage === "SIGN_VERIFY");
    expect(signing).toHaveLength(1);
    expect((signing[0].Properties as { KeySpec?: string }).KeySpec).toBe("ECC_NIST_P256");
    expect(acks(signing[0])).toEqual([{ id: "F19", reason: expect.stringMatching(/asymmetric.*(automatic )?rotation/i) }]);
    // The symmetric data key keeps real rotation and carries no acknowledgement.
    const data = keys.filter((k) => (k.Properties as { KeyUsage?: string }).KeyUsage === undefined);
    expect(data).toHaveLength(1);
    expect((data[0].Properties as { EnableKeyRotation?: boolean }).EnableKeyRotation).toBe(true);
    expect(acks(data[0])).toEqual([]);
  });

  test("F16: the Go module store policy acknowledges the wildcard principal and names the condition that bounds it", () => {
    const policies = Object.values(template("direct", "packages").findResources("AWS::S3::BucketPolicy"));
    expect(policies).toHaveLength(1);
    const list = acks(policies[0]);
    expect(list).toHaveLength(1);
    expect(list[0].id).toBe("F16");
    expect(list[0].reason).toMatch(/aws:SourceVpce/);
    expect(list[0].reason).toMatch(/Block Public Access/i);
    // The claim in the reason holds in the template: every Allow statement of the policy carries the gateway-endpoint condition.
    const doc = (policies[0].Properties as { PolicyDocument: { Statement: Array<{ Effect: string; Principal: unknown; Condition?: Record<string, unknown> }> } }).PolicyDocument;
    const allows = doc.Statement.filter((s) => s.Effect === "Allow");
    expect(allows.length).toBeGreaterThan(0);
    for (const s of allows) expect(JSON.stringify(s.Condition)).toContain("aws:SourceVpce");
  });

  test("every cfn_nag acknowledgement in the nine templates is one of these two, with a reason of 80 or more characters", () => {
    const found: Ack[] = [];
    for (const [, t] of synth("direct").templates) {
      const json = t.toJSON() as { Resources: Record<string, unknown> };
      for (const r of Object.values(json.Resources)) found.push(...acks(r));
    }
    expect(found.map((a) => a.id).sort()).toEqual(["F16", "F19"]);
    for (const a of found) expect(a.reason.length, a.id).toBeGreaterThanOrEqual(80);
  });
});
