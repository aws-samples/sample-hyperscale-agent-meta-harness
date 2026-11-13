import { describe, expect, test } from "vitest";
import { resourcesOf, template } from "./helpers.ts";

// (package installs resolve through CodeArtifact on a VPC endpoint) and packages row.
const STORES: Record<string, string> = {
  "npm-store": "public:npmjs",
  "pypi-store": "public:pypi",
  "maven-store": "public:maven-central",
  "crates-store": "public:crates-io",
};

describe("packages stack", () => {
  const t = template("direct", "packages");

  test("one KMS-encrypted domain per stage", () => {
    const d = Object.values(resourcesOf(t, "AWS::CodeArtifact::Domain"));
    expect(d).toHaveLength(1);
    expect((d[0].Properties as any).DomainName).toBe("hamh-direct");
    expect((d[0].Properties as any).EncryptionKey).toBeDefined();
  });

  test("a store repository per ecosystem with its public upstream", () => {
    const repos = Object.values(resourcesOf(t, "AWS::CodeArtifact::Repository")).map((r) => r.Properties as any);
    for (const [name, conn] of Object.entries(STORES)) {
      const r = repos.find((x) => x.RepositoryName === name);
      expect(r, name).toBeDefined();
      expect(r.ExternalConnections).toEqual([conn]);
    }
  });

  test("the packages repository aggregates the stores and has no external connection", () => {
    const repos = Object.values(resourcesOf(t, "AWS::CodeArtifact::Repository")).map((r) => r.Properties as any);
    const agg = repos.find((x) => x.RepositoryName === "packages");
    expect(agg).toBeDefined();
    expect(agg.ExternalConnections ?? []).toEqual([]);
    expect([...agg.Upstreams].sort()).toEqual(Object.keys(STORES).sort());
  });
});
