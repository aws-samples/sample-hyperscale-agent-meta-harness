import { readdirSync, readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { phase2Images, resourcesOf, synth } from "./helpers.ts";

// CodeBuild refuses an inline buildspec of more than 25,600 characters: `UpdateProject` answered
// "Max buildspec length is 25600" and the lanes stack of the multi-agent line rolled back (deploy of main 22526ac,
// 2026-09-28). A StartBuild buildspecOverride (scripts/codebuild-run.sh sends a file of images/codebuild) is the same string.
// The guard fails at 22,000 so every change keeps margin.
// UTF-8 bytes are measured: never fewer than the characters CodeBuild counts.
const CODEBUILD_MAX = 25_600;
const GUARD = 22_000;
const size = (text: string): number => Buffer.byteLength(text, "utf8");

describe("buildspec length guard", () => {
  test("the guard sits below CodeBuild's limit", () => {
    expect(GUARD).toBeLessThan(CODEBUILD_MAX);
  });

  test(`direct: no CodeBuild project inlines a buildspec of ${GUARD} bytes or more`, () => {
    const checked: string[] = [];
    for (const [stack, tpl] of synth("direct", phase2Images("direct")).templates) {
      for (const [id, r] of Object.entries(resourcesOf(tpl, "AWS::CodeBuild::Project"))) {
        const p = r.Properties as any;
        for (const [where, source] of [["Source", p.Source], ...((p.SecondarySources ?? []) as any[]).map((x, i) => [`SecondarySources[${i}]`, x])]) {
          const spec = source?.BuildSpec;
          if (spec === undefined) continue;
          const text = typeof spec === "string" ? spec : JSON.stringify(spec);
          checked.push(`${stack}/${id}.${where}`);
          expect(size(text), `${stack}/${id}.${where}.BuildSpec is ${size(text)} bytes`).toBeLessThan(GUARD);
        }
      }
    }
    // The image-build project takes its buildspec from the StartBuild override and the PR check names buildspec.yml of the source,
    // so nothing is inlined and the loop checks what exists.
    expect(checked.length).toBeGreaterThanOrEqual(1);
  });

  test(`every buildspec file under images/codebuild is under ${GUARD} bytes`, () => {
    const checked: string[] = [];
    for (const dir of ["../../images/codebuild/"]) {
      const url = new URL(dir, import.meta.url);
      for (const name of readdirSync(url).filter((n) => /\.ya?ml$/.test(n)).sort()) {
        const text = readFileSync(new URL(name, url), "utf8");
        checked.push(`${dir}${name}`);
        expect(size(text), `${dir}${name} is ${size(text)} bytes`).toBeLessThan(GUARD);
      }
    }
    expect(checked.filter((c) => c.startsWith("../../images/codebuild/")).length).toBeGreaterThanOrEqual(4);
  });
});

// The image builds run on CodeBuild ARM in whichever account and Region deploy.sh
// targets, so no build spec names one. CodeBuild sets AWS_REGION to "The AWS Region where the build
// is running" (CodeBuild user guide, environment variables in build environments).
describe("image build specs are Region- and account-neutral", () => {
  const dir = new URL("../../images/codebuild/", import.meta.url);
  const specs = readdirSync(dir).filter((n) => /\.ya?ml$/.test(n)).sort();

  test("no image build spec names a Region or an account", () => {
    for (const name of specs) {
      const text = readFileSync(new URL(name, dir), "utf8");
      expect(text, name).not.toMatch(/\b(us|eu|ap|ca|sa)-(east|west|north|south|central|northeast|southeast)-[0-9]\b/);
      expect(text, name).not.toMatch(/\b[0-9]{12}\b/);
    }
  });

  test("every ECR login uses the build's own Region", () => {
    let logins = 0;
    for (const name of specs) {
      for (const line of readFileSync(new URL(name, dir), "utf8").split("\n").filter((l) => l.includes("get-login-password"))) {
        logins += 1;
        expect(line, name).toContain('aws ecr get-login-password --region "$AWS_REGION" |');
      }
    }
    // worker-python, delivery, image-secret-scan.
    expect(logins).toBeGreaterThanOrEqual(3);
  });
});
