// COMPAT-CAH COMPAT-CAH-DATA: what the CDK app does for the names of the generation before the rename.
//   - COMPAT-CAH, ends at VERSION 5.50: every variable of the app is read under its current name HAMH_X and, second, under the earlier name CAH_X (lib/env.ts), and an
//     accounts file that setup.sh generated before the rename names the selector CAH_AWS_PROFILE.
//   - COMPAT-CAH-DATA, ends at VERSION 6.00: the CodeCommit repositories that a stage created under the earlier prefix stay reachable by the repository gateway, the PR check
//     builds and the rule that starts them; the repositories a stage creates now take only the current prefix.
// lib/env.ts has its own table-driven tests. This file drives the modules that read the environment, with the earlier names.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { Template } from "aws-cdk-lib/assertions";
import { describe, expect, test } from "vitest";
import { buildApp } from "../lib/app.ts";
import * as config from "../lib/config.ts";
import { entryOptions } from "../lib/entry.ts";
import { processNotices } from "../lib/env.ts";
import * as ownerRule from "../lib/owner.ts";
import * as rule from "../lib/repo-names.ts";
import { ACCOUNT, REGION } from "../lib/config.ts";
import { EARLIER_REPOSITORY_PREFIX, EARLIER_SELECTOR, current, earlier, earlierRepositoryArn, repositoryArns } from "./earlier-names.ts";
import { resourcesOf, template } from "./helpers.ts";

const QUIET = { HAMH_NO_DEPRECATION_NOTICES: "1" };
const EXAMPLE = fileURLToPath(new URL("../../fixtures/setup/accounts-generated.json", import.meta.url));
const EXAMPLE_DOC = JSON.parse(readFileSync(EXAMPLE, "utf8"));
// An accounts file with the three roles, built from the generated example so that the test does not depend on the committed accounts.json: the active entry, a history entry and a test entry.
const DOC = {
  ...EXAMPLE_DOC,
  accounts: [
    EXAMPLE_DOC.accounts[0],
    { ...EXAMPLE_DOC.accounts[0], role: "history", profile: "history-profile", account: "444455556666" },
    { ...EXAMPLE_DOC.accounts[0], role: "test", profile: "test-profile", account: "777788889999" },
  ],
};
const REPO_TABLE = JSON.parse(readFileSync(new URL("./fixtures/repo-names.json", import.meta.url), "utf8"));
const FIXTURE = new URL("./fixtures/lambda", import.meta.url).pathname;
const IMAGE = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-worker-python@sha256:` + "c".repeat(64);
const DELIVERY = `${ACCOUNT}.dkr.ecr.${REGION}.amazonaws.com/hamh-direct-delivery@sha256:` + "d".repeat(64);
const scratch = (): string => mkdtempSync(join(tmpdir(), "earlier-names-"));
const doc = (entry: Record<string, unknown>) => ({ ...EXAMPLE_DOC, accounts: [{ ...EXAMPLE_DOC.accounts[0], ...entry }] });
const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const profileOf = (t: config.Target) => t.profile;

/** Runs `fn` with stderr captured; the lines are what the notices wrote. */
function captured<T>(fn: () => T): { result: T; stderr: string } {
  const lines: string[] = [];
  const real = process.stderr.write.bind(process.stderr);
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  (process.stderr as any).write = (chunk: string | Uint8Array) => { lines.push(String(chunk)); return true; };
  try {
    return { result: fn(), stderr: lines.join("") };
  } finally {
    process.stderr.write = real;
  }
}

describe("the entry options under the earlier variable names", () => {
  // Nine settings have an earlier name; the three inputs of one deploy are renamed with no fallback (Tier 2).
  const WITH_EARLIER: Record<string, string> = {
    WORKER_REVISION: "3", RESEARCH_PROVIDER: "tavily", UTILITY_MODEL: "off", UTILITY_PURPOSES: "run-name", TOOLS_MODE: "on", TAVILY_ALARMS: "off",
    TAVILY_KEY_MAX_AGE_DAYS: "45", RESEARCH_ALARM_TOPIC_ARN: "arn:aws:sns:us-east-1:111122223333:alarms", COMPUTE_INSTANCES: "on",
  };
  const DEPLOY_INPUTS: Record<string, string> = { LAMBDA_DIR: FIXTURE, WORKER_IMAGE: IMAGE, DELIVERY_IMAGE: DELIVERY };
  const withNames = (name: (suffix: string) => string, from: Record<string, string>, only: string[] = Object.keys(from)) =>
    Object.fromEntries(only.map((k) => [name(k), from[k]]));
  const inputs = withNames(current, DEPLOY_INPUTS);

  test("the nine settings under the earlier names give the options that the current names give", () => {
    const now = entryOptions({ ...inputs, ...withNames(current, WITH_EARLIER), ...QUIET });
    expect(now.workerRevision).toBe(3);
    expect(entryOptions({ ...inputs, ...withNames(earlier, WITH_EARLIER), ...QUIET })).toEqual(now);
  });

  test.each(Object.keys(WITH_EARLIER))("%s alone under the earlier name is read", (name) => {
    const viaCurrent = entryOptions({ ...inputs, ...withNames(current, WITH_EARLIER, [name]), ...QUIET });
    const viaEarlier = entryOptions({ ...inputs, ...withNames(earlier, WITH_EARLIER, [name]), ...QUIET });
    expect(viaEarlier).toEqual(viaCurrent);
  });

  test("the three inputs of one deploy have no earlier name: it is not read and prints nothing", () => {
    processNotices.reset();
    const { result, stderr } = captured(() => {
      expect(() => entryOptions({ CAH_LAMBDA_DIR: FIXTURE })).toThrow(/HAMH_LAMBDA_DIR is required/);
      return entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, CAH_WORKER_IMAGE: IMAGE, CAH_DELIVERY_IMAGE: DELIVERY });
    });
    expect(result.workerImage).toBeUndefined();
    expect(result.deliveryImage).toBeUndefined();
    expect(stderr).toBe("");
  });

  test("the current name wins when both are set, and one notice says which one is used", () => {
    processNotices.reset();
    const { result, stderr } = captured(() => entryOptions({ HAMH_LAMBDA_DIR: FIXTURE, HAMH_WORKER_REVISION: "7", CAH_WORKER_REVISION: "9" }));
    expect(result.workerRevision).toBe(7);
    expect(stderr).toContain("Both CAH_WORKER_REVISION and HAMH_WORKER_REVISION are set. HAMH_WORKER_REVISION is used.");
  });

  test("a refusal names the variable that answered, so the person changes the one they set", () => {
    expect(() => entryOptions({ ...inputs, CAH_WORKER_REVISION: "x", ...QUIET })).toThrow(/CAH_WORKER_REVISION must be a non-negative integer/);
    expect(() => entryOptions({ HAMH_LAMBDA_DIR: new URL("./fixtures", import.meta.url).pathname, ...QUIET })).toThrow(/HAMH_LAMBDA_DIR .*has no worker-api\/bootstrap/);
    expect(() => entryOptions({ ...inputs, CAH_RESEARCH_PROVIDER: "nope", ...QUIET })).toThrow(/CAH_RESEARCH_PROVIDER must be one of/);
    expect(() => entryOptions({ ...QUIET })).toThrow(/HAMH_LAMBDA_DIR is required/);
  });

  test("the notice comes once per name on stderr, and HAMH_NO_DEPRECATION_NOTICES=1 silences it", () => {
    processNotices.reset();
    const { stderr } = captured(() => {
      entryOptions({ ...inputs, CAH_TOOLS_MODE: "on" });
      entryOptions({ ...inputs, CAH_TOOLS_MODE: "on" });
      entryOptions({ ...inputs, ...QUIET, CAH_TAVILY_ALARMS: "off" });
    });
    expect(stderr.split("\n").filter((l) => l.includes("CAH_TOOLS_MODE is the old name of HAMH_TOOLS_MODE"))).toHaveLength(1);
    expect(stderr).not.toContain("CAH_TAVILY_ALARMS");
  });
});

describe("the account selector under the earlier name", () => {
  // The profiles of the two entries that only a selector reaches.
  const HISTORY = "history-profile";
  const TEST_ENTRY = "test-profile";

  test("CAH_AWS_PROFILE selects the entry when HAMH_AWS_PROFILE is not set, and never the active one", () => {
    expect(profileOf(config.selectTarget(DOC, { CAH_AWS_PROFILE: HISTORY, ...QUIET }))).toBe(HISTORY);
    expect(profileOf(config.selectTarget(DOC, { CAH_AWS_PROFILE: TEST_ENTRY, ...QUIET }))).toBe(TEST_ENTRY);
  });

  test("an empty HAMH_AWS_PROFILE does not hide CAH_AWS_PROFILE: a selector is never ignored", () => {
    expect(profileOf(config.selectTarget(DOC, { HAMH_AWS_PROFILE: "", CAH_AWS_PROFILE: HISTORY, ...QUIET }))).toBe(HISTORY);
  });

  test("the same profile under both names is the selection", () => {
    expect(profileOf(config.selectTarget(DOC, { HAMH_AWS_PROFILE: HISTORY, CAH_AWS_PROFILE: HISTORY, ...QUIET }))).toBe(HISTORY);
  });

  test("two different profiles are an error that names both variables; the active entry is not the fallback", () => {
    const env = { HAMH_AWS_PROFILE: HISTORY, CAH_AWS_PROFILE: TEST_ENTRY, ...QUIET };
    expect(() => config.selectTarget(DOC, env)).toThrow(/HAMH_AWS_PROFILE/);
    expect(() => config.selectTarget(DOC, env)).toThrow(/CAH_AWS_PROFILE/);
  });

  test("an unknown profile under the earlier name is refused with the variable that was set", () => {
    expect(() => config.selectTarget(DOC, { CAH_AWS_PROFILE: "default", ...QUIET })).toThrow(/CAH_AWS_PROFILE=default names no entry/);
  });

  test("a file generated before the rename names the selector CAH_AWS_PROFILE and still loads; any other word is refused", () => {
    expect(EARLIER_SELECTOR).toBe("CAH_AWS_PROFILE");
    const old = { ...DOC, selector: EARLIER_SELECTOR };
    expect(config.selectTarget(old, {}).role).toBe("active");
    expect(profileOf(config.selectTarget(old, { HAMH_AWS_PROFILE: HISTORY }))).toBe(HISTORY);
    expect(() => config.selectTarget({ ...DOC, selector: "AWS_PROFILE" }, {})).toThrow(/selector HAMH_AWS_PROFILE or CAH_AWS_PROFILE/);
    expect(() => config.selectTarget({ ...DOC, selector: "hamh_aws_profile" }, {})).toThrow(/selector/);
  });
});

describe("the account-file override under the earlier name", () => {
  test("CAH_ACCOUNTS_FILE names the file when HAMH_ACCOUNTS_FILE is not set or is empty", () => {
    expect(config.accountsFilePath({ CAH_ACCOUNTS_FILE: EXAMPLE, ...QUIET })).toBe(EXAMPLE);
    expect(config.accountsFilePath({ HAMH_ACCOUNTS_FILE: "", CAH_ACCOUNTS_FILE: EXAMPLE, ...QUIET })).toBe(EXAMPLE);
    expect(config.loadAccounts({ CAH_ACCOUNTS_FILE: EXAMPLE, ...QUIET })).toEqual(EXAMPLE_DOC);
  });

  test("the same file under both names is that file", () => {
    expect(config.accountsFilePath({ HAMH_ACCOUNTS_FILE: EXAMPLE, CAH_ACCOUNTS_FILE: EXAMPLE, ...QUIET })).toBe(EXAMPLE);
  });

  test("two different files are an error that names both variables and never falls back to the committed file", () => {
    const other = join(scratch(), "other.json");
    writeFileSync(other, JSON.stringify(EXAMPLE_DOC));
    const env = { HAMH_ACCOUNTS_FILE: EXAMPLE, CAH_ACCOUNTS_FILE: other, ...QUIET };
    expect(() => config.accountsFilePath(env)).toThrow(/HAMH_ACCOUNTS_FILE/);
    expect(() => config.accountsFilePath(env)).toThrow(/CAH_ACCOUNTS_FILE/);
    expect(() => config.loadAccounts(env)).toThrow(/CAH_ACCOUNTS_FILE/);
  });

  test("a relative path under the earlier name is refused and the refusal names that variable", () => {
    expect(() => config.accountsFilePath({ CAH_ACCOUNTS_FILE: "accounts.json", ...QUIET })).toThrow(/CAH_ACCOUNTS_FILE must be an absolute path/);
  });

  test("a file generated before the rename (selector CAH_AWS_PROFILE) loads from either variable and selects its active entry", () => {
    const p = join(scratch(), "generated-before.json");
    writeFileSync(p, JSON.stringify({ ...EXAMPLE_DOC, selector: EARLIER_SELECTOR }));
    for (const env of [{ HAMH_ACCOUNTS_FILE: p }, { CAH_ACCOUNTS_FILE: p }]) {
      const t = config.selectTarget(config.loadAccounts({ ...env, ...QUIET }), {});
      expect([t.account, t.role, t.profile]).toEqual(["111122223333", "active", "your-aws-profile"]);
    }
  });
});

describe("the owner under the earlier variable name", () => {
  const target = () => config.selectTarget(doc({}), {});

  test("CAH_OWNER sets the owner when HAMH_OWNER is not set or is empty", () => {
    expect(ownerRule.ownerOf(target(), { CAH_OWNER: "legacy-owner", ...QUIET })).toBe("legacy-owner");
    expect(ownerRule.ownerOf(target(), { HAMH_OWNER: "", CAH_OWNER: "legacy-owner", ...QUIET })).toBe("legacy-owner");
  });

  test("the same owner under both names is that owner; two different owners are an error that names both variables", () => {
    expect(ownerRule.ownerOf(target(), { HAMH_OWNER: "same", CAH_OWNER: "same", ...QUIET })).toBe("same");
    const env = { HAMH_OWNER: "new-owner", CAH_OWNER: "legacy-owner", ...QUIET };
    expect(() => ownerRule.ownerOf(target(), env)).toThrow(/HAMH_OWNER/);
    expect(() => ownerRule.ownerOf(target(), env)).toThrow(/CAH_OWNER/);
  });

  test("an owner that is not a tag value is refused and the refusal names the variable that was set", () => {
    expect(() => ownerRule.ownerOf(target(), { CAH_OWNER: "has space", ...QUIET })).toThrow(/^CAH_OWNER: owner must be 1 to 64 letters/);
  });
});

describe("the repository-name rule under the earlier variable name", () => {
  const target = () => config.selectTarget(doc({ repoNames: ["a-*"] }), {});

  test("CAH_REPO_NAMES sets the rule for every row of the shared table, ahead of the entry", () => {
    expect(REPO_TABLE.env.length).toBeGreaterThanOrEqual(4);
    for (const row of REPO_TABLE.env) {
      expect(rule.repoNamesOf(config.selectTarget(doc(row.entry), {}), { CAH_REPO_NAMES: row.env, ...QUIET }), row.why).toEqual(row.rule);
    }
  });

  test("an empty HAMH_REPO_NAMES does not hide CAH_REPO_NAMES, and the current name wins when both are set", () => {
    expect(rule.repoNamesOf(target(), { HAMH_REPO_NAMES: "", CAH_REPO_NAMES: "b-*", ...QUIET })).toEqual(["b-*"]);
    expect(rule.repoNamesOf(target(), { HAMH_REPO_NAMES: "c-*", CAH_REPO_NAMES: "b-*", ...QUIET })).toEqual(["c-*"]);
  });

  test("a CAH_REPO_NAMES that is not a rule is a refusal that names that variable", () => {
    for (const row of REPO_TABLE.envRefuse) {
      expect(() => rule.repoNamesOf(config.selectTarget(doc(row.entry), {}), { CAH_REPO_NAMES: row.env, ...QUIET }), row.why)
        .toThrow(new RegExp(`^CAH_REPO_NAMES: ${escape(row.reason)}`));
    }
  });
});

describe("the S3 gateway endpoint policy under the earlier variable name", () => {
  const withEnv = <T>(env: Record<string, string>, fn: () => T): T => {
    const before = Object.fromEntries(Object.keys(env).map((k) => [k, process.env[k]]));
    Object.assign(process.env, env);
    try {
      return fn();
    } finally {
      for (const [k, v] of Object.entries(before)) {
        if (v === undefined) delete process.env[k];
        else process.env[k] = v;
      }
    }
  };
  const policy = (env: Record<string, string>) => {
    const t = withEnv(env, () => template("direct", "network"));
    const eps = Object.values(resourcesOf(t, "AWS::EC2::VPCEndpoint")).map((r) => r.Properties as any).filter((e) => e.VpcEndpointType === "Gateway");
    expect(eps).toHaveLength(1);
    return eps[0].PolicyDocument;
  };

  test("CAH_AGENTCORE_STORAGE_ACCOUNTS names the owners of the session storage as HAMH_AGENTCORE_STORAGE_ACCOUNTS does", () => {
    const value = `${REGION}=111122223333`;
    const viaCurrent = policy({ [current("AGENTCORE_STORAGE_ACCOUNTS")]: value, ...QUIET });
    const viaEarlier = policy({ [earlier("AGENTCORE_STORAGE_ACCOUNTS")]: value, ...QUIET });
    expect(JSON.stringify(viaEarlier)).toContain("111122223333");
    expect(viaEarlier).toEqual(viaCurrent);
  });
});

describe("the CodeCommit repositories of the generation before the rename (COMPAT-CAH-DATA)", () => {
  const t = template("direct", "delivery");
  const roles = Object.values(resourcesOf(t, "AWS::IAM::Role")).map((r) => r.Properties as any);
  const statements = (name: string): Array<Record<string, any>> => roles.find((r) => r.RoleName === name).Policies.flatMap((p: any) => p.PolicyDocument.Statement);
  const sid = (st: Array<Record<string, any>>, s: string) => st.find((x) => x.Sid === s)!;
  const [CURRENT_ARN, EARLIER_ARN] = repositoryArns(REGION, ACCOUNT);

  test("the helper spells the earlier pattern as the stage's old prefix", () => {
    expect(EARLIER_REPOSITORY_PREFIX).toBe("cah-e2e-");
    expect(EARLIER_ARN).toBe(earlierRepositoryArn(REGION, ACCOUNT));
    expect(EARLIER_ARN).toBe(`arn:aws:codecommit:${REGION}:${ACCOUNT}:cah-e2e-*`);
  });

  test("repo-gateway reads, pushes to and opens pull requests in a repository of either prefix", () => {
    const st = statements("hamh-direct-repo-gateway");
    for (const id of ["ReadE2eRepositories", "PushHarnessBranches", "PullRequests"]) {
      expect(sid(st, id).Resource, id).toEqual([CURRENT_ARN, EARLIER_ARN]);
    }
  });

  test("repo-gateway creates a repository, a branch or an allowlist item only under the current prefix", () => {
    const st = statements("hamh-direct-repo-gateway");
    for (const id of ["CreateNewRepositories", "NewRepositoryMain", "NewRepositoryDefaultBranch"]) {
      expect(JSON.stringify(sid(st, id).Resource), id).not.toContain("cah");
    }
    expect(JSON.stringify(sid(st, "AllowlistNewRepositories").Condition)).not.toContain("cah");
    // The earlier prefix appears in exactly the three statements above and nowhere else in the role.
    const naming = st.filter((x) => JSON.stringify(x).includes("cah-e2e-")).map((x) => x.Sid).sort();
    expect(naming).toEqual(["PullRequests", "PushHarnessBranches", "ReadE2eRepositories"]);
  });

  test("the PR check builds in a repository of either prefix and approves and comments there", () => {
    const st = statements("hamh-direct-prcheck-build");
    for (const id of ["ReadE2eRepositories", "ApproveAndComment"]) expect(sid(st, id).Resource, id).toEqual([CURRENT_ARN, EARLIER_ARN]);
    const naming = st.filter((x) => JSON.stringify(x).includes("cah-e2e-")).map((x) => x.Sid).sort();
    expect(naming).toEqual(["ApproveAndComment", "ReadE2eRepositories"]);
  });

  test("the rule that starts the PR check fires for a pull request in a repository of either prefix", () => {
    const rules = Object.values(resourcesOf(t, "AWS::Events::Rule")).map((r) => r.Properties as any);
    expect(rules).toHaveLength(1);
    expect(rules[0].EventPattern.resources).toEqual([
      { prefix: `arn:aws:codecommit:${REGION}:${ACCOUNT}:hamh-e2e-` },
      { prefix: `arn:aws:codecommit:${REGION}:${ACCOUNT}:cah-e2e-` },
    ]);
  });

  test("no other role of the stage names the earlier prefix", () => {
    const named = roles.filter((r) => JSON.stringify(r.Policies).includes("cah-e2e-")).map((r) => r.RoleName).sort();
    expect(named).toEqual(["hamh-direct-prcheck-build", "hamh-direct-repo-gateway"]);
  });

  test("the whole delivery stack names the earlier prefix only in those two roles and the rule, and the other eight stacks not at all", () => {
    const app = buildApp({ stage: "direct", deliveryImage: DELIVERY, workerImage: IMAGE });
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const hits = app.node.children.filter((c) => "templateOptions" in c).map((c: any) => [c.stackName as string, JSON.stringify(Template.fromStack(c).toJSON()).includes("cah-e2e-")] as const);
    expect(hits.filter(([, named]) => named).map(([n]) => n)).toEqual(["hamh-direct-delivery"]);
  });
});

