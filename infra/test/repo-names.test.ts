// The repository-name rule of a stage. A stage lets a run open a pull request only in a CodeCommit repository whose name the rule covers (and that the operator
// allowed). The rule is the optional `repoNames` of the account-file entry, or the environment variable `HAMH_REPO_NAMES` for one run; without either it is the
// default `hamh-e2e-*`. The words are one table: test/fixtures/repo-names.json (scripts/tests/test_repo_names.py runs it through scripts/lib/aws_target.py). The CDK
// app hands the rule to the control-api and delivery Lambdas as `HAMH_REPO_NAMES`, and the control stack prints it as the output `RepoNames` so that an update of the
// stage can keep it.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import * as config from "../lib/config.ts";
import * as rule from "../lib/repo-names.ts";

const TABLE = JSON.parse(readFileSync(new URL("./fixtures/repo-names.json", import.meta.url), "utf8"));
const EXAMPLE = fileURLToPath(new URL("../../fixtures/setup/accounts-generated.json", import.meta.url));
const EXAMPLE_DOC = JSON.parse(readFileSync(EXAMPLE, "utf8"));
const doc = (entry: Record<string, unknown>) => ({ ...EXAMPLE_DOC, accounts: [{ ...EXAMPLE_DOC.accounts[0], ...entry }] });
const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

describe("the repository-name rule", () => {
  test("the default is the prefix hamh-e2e-", () => {
    expect(rule.DEFAULT_REPO_NAMES).toEqual(["hamh-e2e-*"]);
    expect(TABLE.default).toEqual([...rule.DEFAULT_REPO_NAMES]);
  });

  test("every accepted entry gives the rule of the table", () => {
    expect(TABLE.accept.length).toBeGreaterThanOrEqual(9);
    for (const row of TABLE.accept) {
      const target = config.selectTarget(doc(row.entry), {});
      expect(rule.repoNamesOf(target, {}), row.why).toEqual(row.rule);
    }
  });

  test("HAMH_REPO_NAMES, when it is set and not empty, wins over the entry for every row of the table", () => {
    expect(rule.REPO_NAMES_ENV).toBe("HAMH_REPO_NAMES");
    for (const row of TABLE.env) {
      const target = config.selectTarget(doc(row.entry), {});
      expect(rule.repoNamesOf(target, { HAMH_REPO_NAMES: row.env }), row.why).toEqual(row.rule);
    }
  });

  test("an empty HAMH_REPO_NAMES is unset", () => {
    const target = config.selectTarget(doc({ repoNames: ["a-*"] }), {});
    expect(rule.repoNamesOf(target, { HAMH_REPO_NAMES: "" })).toEqual(["a-*"]);
    expect(rule.repoNamesOf(target, {})).toEqual(["a-*"]);
  });

  test("a HAMH_REPO_NAMES that is not a rule is a refusal that names the variable", () => {
    expect(TABLE.envRefuse.length).toBeGreaterThanOrEqual(5);
    for (const row of TABLE.envRefuse) {
      const target = config.selectTarget(doc(row.entry), {});
      expect(() => rule.repoNamesOf(target, { HAMH_REPO_NAMES: row.env }), row.why).toThrow(new RegExp(`^HAMH_REPO_NAMES: ${escape(row.reason)}`));
    }
  });

  test("every refused entry makes the whole file a refusal that names the entry", () => {
    expect(TABLE.refuse.length).toBeGreaterThanOrEqual(14);
    for (const row of TABLE.refuse) {
      expect(() => config.selectTarget(doc(row.entry), {}), row.why).toThrow(new RegExp(`^accounts\\.json accounts\\[0\\]: ${escape(row.reason)}`));
    }
  });

  test("an entry that is not selected is checked as well", () => {
    const second = { ...EXAMPLE_DOC.accounts[0], profile: "other", role: "test", account: "444455556666", repoNames: ["bad name"] };
    expect(() => config.selectTarget({ ...EXAMPLE_DOC, accounts: [EXAMPLE_DOC.accounts[0], second] }, {})).toThrow(/accounts\[1\]: repoNames entry/);
  });

  test("every cover row agrees with the matcher", () => {
    expect(TABLE.cover.length).toBeGreaterThanOrEqual(5);
    for (const row of TABLE.cover) {
      for (const name of row.allows) expect(rule.repoNameAllowed(row.rule, name), `${row.why}: ${name}`).toBe(true);
      for (const name of row.refuses) expect(rule.repoNameAllowed(row.rule, name), `${row.why}: ${name}`).toBe(false);
    }
  });

  test("the REPO_NAMES of the app is the rule applied to the selected entry and the environment, never a literal", () => {
    expect(config.REPO_NAMES).toEqual(rule.repoNamesOf(config.TARGET, process.env));
    const source = readFileSync(new URL("../lib/config.ts", import.meta.url), "utf8");
    expect(source).not.toMatch(/export const REPO_NAMES = \[/);
  });
});

describe("the stacks hand the rule to the Lambdas", () => {
  const control = readFileSync(new URL("../lib/stacks/control.ts", import.meta.url), "utf8");
  const delivery = readFileSync(new URL("../lib/stacks/delivery.ts", import.meta.url), "utf8");

  test("the control-api and delivery functions get HAMH_REPO_NAMES from REPO_NAMES", () => {
    expect(control).toMatch(/HAMH_REPO_NAMES: REPO_NAMES\.join\(","\)/);
    expect(delivery).toMatch(/HAMH_REPO_NAMES: REPO_NAMES\.join\(","\)/);
  });

  test("the control stack prints the rule as the output RepoNames, which an update of the stage reads", () => {
    expect(control).toMatch(/new CfnOutput\(this, "RepoNames", \{ value: REPO_NAMES\.join\(","\)/);
  });
});
