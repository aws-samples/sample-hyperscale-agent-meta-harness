"""The contract between the parts of the setup feature, tested with the real files of each side.

The feature was built in four parts that meet at a few names and formats: the wizard (`setup.sh` and its helper files), the account-local runner (the
template `infra/setup-runner.yaml`, `scripts/setup/runner-build.sh` and `runner_state.py`), the client build (`names.sh`, `build-client.sh`) and the account
file override (`HAMH_ACCOUNTS_FILE`, `scripts/lib/aws_target.py`). Each part has its own tests with a stand-in for the others. This file is where the stand-ins
are checked against the real thing:

  start     what `setup.sh` passes to `codebuild start-build` is what the runner script reads and what the template's project can use: the variable names, the actions, the keys
            (checked against the runner's own patterns), the stack outputs it reads, the S3 source of the project.
  result    the result.json and the phases.jsonl lines that the runner script really writes (runner-build.sh run in its test rig, for a deploy, a deploy the guard
            refused and a teardown) are read by the wizard's own reader, and every path the wizard reads exists in them.
  account   the account file the wizard generates is accepted by the account-file override, its digest is the one in the key of the object, and it carries what the
            runner's account-file phase demands.
  client    what `build-client.sh` prints and writes is accepted by the runner's check and found by the wizard's reader of SHA256SUMS; the platform the wizard picks
            for a machine is the one `names.sh` picks.
  stacks    the wizard (`pf_stage`) and the runner (the outputs phase of `runner-build.sh`) call the same CloudFormation stack states complete, for every state there is.
  dox       every file of scripts/setup/ is named in its AGENTS.md.

Run on a Linux machine with the tools pinned in `tools.lock`:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_setup_contract.py
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess  # nosec B404: tests start the scripts under test with argument lists, never a shell
import sys
import unittest
from pathlib import Path
from typing import Dict, List, Set

import setup_runner_support as sup
import test_build_client
import test_setup_full_units as units
import test_setup_runner_build as rb
from setup_full_support import FullBase
from setup_waits import limit

REPO = sup.REPO
SETUP_DIR = REPO / "scripts" / "setup"
ALL_PLATFORMS = ("darwin-arm64", "linux-amd64", "linux-arm64")

# The paths of result.json that the wizard reads (see `rn_res`, `cl_flat_get` and the summary of the helper files). `$n` stands for a phase number and
# `$platform` for a client platform. A new read in the wizard has to be added here, which is the moment to check that the runner writes it.
DEPLOY_READS = {
    "state", "sourceCommit", "configDigest", "action", "failedPhase", "detail", "endpoint", "stageUnchanged", "failedStep",
    "phases.$n.name", "phases.$n.state", "phases.$n.seconds",
    "client.$platform.key", "client.$platform.sha256",
    "client.$platform.admin.key", "client.$platform.admin.sha256",
}
# What the wizard reads from the result of the build that prints the CDK bootstrap template (SETUP_ACTION=bootstrap-template).
TEMPLATE_READS = {"bootstrapTemplate.key", "bootstrapTemplate.sha256", "bootstrapTemplate.bytes"}
WIZARD_READS = DEPLOY_READS | TEMPLATE_READS
PIN = "engine/policy/cdk-bootstrap-template.pin"


def helper_text(*names: str) -> str:
    return "".join((SETUP_DIR / n).read_text(encoding="utf-8") for n in names)


class StartContractCase(FullBase):
    """setup.sh against the runner script and the template."""

    def started(self, *args: str) -> dict:
        p = self.run_full(*args)
        self.assertEqual(p.returncode, 0, self.out(p))
        return self.start_build_env()

    def test_the_variables_of_the_build_are_variables_the_runner_script_reads(self) -> None:
        env = self.started(*self.ARGS, "--keep-runner")
        read = set(re.findall(r"\bSETUP_[A-Z0-9_]+\b", sup.read("scripts/setup/runner-build.sh")))
        self.assertLessEqual(set(env), read, "setup.sh passes a variable that runner-build.sh never reads")
        self.assertLessEqual({"SETUP_SOURCE_KEY", "SETUP_CONFIG_KEY"}, set(env), "runner-build.sh stops in inputs without these two")

    def test_the_keys_are_keys_the_runner_script_accepts(self) -> None:
        commit = self.commit()
        env = self.started(*self.ARGS, "--keep-runner")
        text = sup.read("scripts/setup/runner-build.sh")
        re_src = re.search(r"re_src='([^']+)'", text).group(1)
        re_cfg = re.search(r"re_cfg='([^']+)'", text).group(1)
        self.assertRegex(env["SETUP_SOURCE_KEY"], re_src)
        self.assertRegex(env["SETUP_CONFIG_KEY"], re_cfg)
        self.assertEqual(env["SETUP_SOURCE_KEY"], f"source/{commit}.zip")

    def test_the_hashes_setup_sh_sends_are_the_hashes_of_the_objects_it_uploaded(self) -> None:
        commit = self.commit()
        env = self.started(*self.ARGS, "--keep-runner")
        self.assertEqual(env["SETUP_SOURCE_SHA256"], hashlib.sha256(self.s3_bytes(f"source/{commit}.zip")).hexdigest())
        self.assertEqual(env["SETUP_CONFIG_SHA256"], hashlib.sha256(self.s3_bytes(env["SETUP_CONFIG_KEY"])).hexdigest())
        self.assertTrue(env["SETUP_CONFIG_KEY"].startswith(f"config/accounts-{env['SETUP_CONFIG_SHA256'][:12]}"), "the 12 digits of the key are the start of the 64")

    def test_the_parameters_setup_sh_passes_are_the_parameters_of_the_template_and_fit_their_patterns(self) -> None:
        self.started(*self.ARGS, "--keep-runner")
        call = [c for c in self.normalized_calls() if c[:4] == ["cloudformation", "deploy", "--stack-name", "hamh-setup-runner"]][0]
        at = call.index("--parameter-overrides") + 1
        passed = {}
        while at < len(call) and not call[at].startswith("--"):
            key, value = call[at].split("=", 1)
            passed[key] = value
            at += 1
        declared = sup.template()["Parameters"]
        self.assertLessEqual(set(passed), set(declared), "setup.sh passes a parameter the template does not have")
        self.assertLessEqual({k for k, v in declared.items() if "Default" not in v}, set(passed), "a parameter without a default that setup.sh does not pass")
        for key, value in passed.items():
            if "AllowedPattern" in declared[key]:
                self.assertRegex(value, declared[key]["AllowedPattern"], key)

    def test_the_variable_the_buildspec_exports_is_the_one_the_wizard_looks_up(self) -> None:
        self.assertEqual(sup.buildspec()["env"]["exported-variables"], ["SETUP_RESULT_SHA256"])
        self.assertIn("SETUP_RESULT_SHA256", helper_text("runner.sh"))
        self.assertIn("SETUP_RESULT_SHA256", sup.read("scripts/setup/runner-build.sh"))

    def test_the_client_name_is_the_one_names_sh_defines(self) -> None:
        env = self.started(*self.ARGS, "--keep-runner")
        names = subprocess.run(["bash", str(SETUP_DIR / "names.sh")], capture_output=True, text=True, env={"PATH": os.environ["PATH"]}, timeout=limit(60))  # nosec B603: argument list, no shell; B607: bash on PATH
        self.assertEqual(names.returncode, 0, names.stderr)
        defined = dict(line.split("=", 1) for line in names.stdout.splitlines() if "=" in line)
        self.assertEqual(env["SETUP_BINARY_NAME"], defined["HAMH_CLIENT_BIN"])

    def test_a_teardown_starts_no_build_and_the_actions_setup_sh_passes_are_actions_the_runner_script_accepts(self) -> None:
        """The teardown runs on the user's machine with the user's credentials: no build of the runner is started for it, and the runner script has no teardown action."""
        self.set_scenario(stage_deployed=True, runner_present=True, bootstrap_present=True)
        p = self.run_full("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", "210987654321")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn("SETUP_ACTION must be deploy or bootstrap-template", sup.read("scripts/setup/runner-build.sh"))
        for helper in ("runner.sh", "full.sh", "teardown.sh"):
            self.assertNotIn("SETUP_ACTION=teardown", helper_text(helper), helper)

    def test_the_template_build_setup_sh_starts_in_a_fresh_account_is_an_action_the_runner_accepts(self) -> None:
        self.set_scenario(bootstrap_present=False)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.start_build_env(0)["SETUP_ACTION"], "bootstrap-template")
        self.assertIn("SETUP_ACTION must be deploy or bootstrap-template", sup.read("scripts/setup/runner-build.sh"))

    def test_the_wizard_and_the_runner_read_the_pin_of_the_bootstrap_template_from_the_same_file(self) -> None:
        self.assertIn(f"SETUP_BOOTSTRAP_PIN={PIN}", helper_text("lib.sh"))
        self.assertIn(PIN, sup.read("scripts/setup/runner-build.sh"))
        pin = (REPO / PIN).read_text(encoding="utf-8")
        for script in ("scripts/setup/runner-build.sh", "scripts/setup/preflight.sh"):
            self.assertIn("sed -n 's/^sha256=//p'", sup.read(script), script)
        self.assertRegex(pin, r"(?m)^sha256=[0-9a-f]{64}$")

    def test_the_outputs_the_wizard_reads_are_the_outputs_of_the_template(self) -> None:
        read = set(re.findall(r"^\s*(\w+)\) RN_\w+=\$val ;;", helper_text("runner.sh"), re.M))
        self.assertEqual(read, set(sup.template()["Outputs"]), "setup.sh reads every output of infra/setup-runner.yaml and no other")

    def test_the_project_reads_its_source_from_the_bucket_setup_sh_uploads_to(self) -> None:
        project = next(r for r in sup.template()["Resources"].values() if r["Type"] == "AWS::CodeBuild::Project")["Properties"]
        source = project["Source"]
        self.assertEqual(source["Type"], "S3")
        location = sup.normalise(source["Location"])
        self.assertEqual(location, "hamh-setup-{account}-{region}/source/placeholder.zip", "start-build overrides this placeholder with <bucket>/<SETUP_SOURCE_KEY>")
        bucket = f"hamh-setup-{self.ACCOUNT}-{self.REGION}"
        self.assertEqual(self.bucket, bucket)
        self.started(*self.ARGS, "--keep-runner")
        start = [c for c in self.normalized_calls() if c[:2] == ["codebuild", "start-build"]][0]
        override = start[start.index("--source-location-override") + 1]
        self.assertEqual(override, f"{bucket}/{self.start_build_env()['SETUP_SOURCE_KEY']}")
        self.assertEqual(project["Source"]["BuildSpec"], "images/codebuild/setup-runner.yml")


class ResultContractCase(units.HelperCase):
    """What the runner script writes, read by the wizard."""

    def flat(self, doc: str) -> Dict[str, str]:
        p = self.sh('printf "%s" "$DOC" | json_flat', "json", DOC=doc)
        self.assertEqual(p.returncode, 0, p.stderr)
        return dict(line.split("\t", 1) for line in p.stdout.splitlines() if line)

    def run_runner(self, **env: str) -> str:
        rig = rb.Rig(self)
        proc = rig.run(**env)
        raw = rig.get(f"results/{rb.BUILD_ID}/result.json")
        self.assertIsNotNone(raw, proc.stdout + proc.stderr)
        self.runner_proc = proc
        self.runner_rig = rig
        return raw.decode()

    def test_the_wizard_reads_exactly_the_paths_listed_here(self) -> None:
        found: Set[str] = set()
        for name in ("runner.sh", "full.sh", "status.sh", "teardown.sh", "preflight.sh", "state.sh"):
            for m in re.finditer(r'rn_res\s+"?([A-Za-z0-9_.$]+)"?', (SETUP_DIR / name).read_text(encoding="utf-8")):
                path = m.group(1)
                if path.startswith("phases.$"):
                    path = re.sub(r"^phases\.\$\w+", "phases.$n", path)
                if path != "1" and not path.startswith("$"):
                    found.add(path)
        for m in re.finditer(r'cl_flat_get\s+"\$flat"\s+"([^"]+)"', helper_text("client.sh")):
            found.add(re.sub(r"\$platform", "$platform", m.group(1)))
        self.assertEqual(found, WIZARD_READS, "the wizard reads a path of result.json that this file does not list (or stopped reading one)")

    def test_every_path_the_wizard_reads_is_in_the_result_of_a_passing_deploy(self) -> None:
        flat = self.flat(self.run_runner())
        self.assertEqual(self.runner_proc.returncode, 0, self.runner_proc.stdout)
        for path in sorted(DEPLOY_READS):
            concrete = path.replace("$n", "0").replace("$platform", "linux-amd64")
            self.assertIn(concrete, flat, f"{path}: the runner's result.json has no {concrete}")
        self.assertEqual((flat["state"], flat["action"], flat["sourceCommit"]), ("SETUP-PASS", "deploy", rb.COMMIT))
        self.assertEqual(flat["endpoint"], rb.ENDPOINT)
        self.assertEqual(len(flat["client.linux-amd64.sha256"]), 64)

    def test_every_path_the_wizard_reads_of_a_template_build_is_in_its_result(self) -> None:
        flat = self.flat(self.run_runner(SETUP_ACTION="bootstrap-template"))
        self.assertEqual(self.runner_proc.returncode, 0, self.runner_proc.stdout + self.runner_proc.stderr)
        for path in sorted(TEMPLATE_READS | {"state", "action", "sourceCommit", "configDigest"}):
            self.assertIn(path, flat, f"{path}: the runner's result.json has no such member")
        self.assertEqual((flat["state"], flat["action"]), ("SETUP-PASS", "bootstrap-template"))
        sha, key = flat["bootstrapTemplate.sha256"], flat["bootstrapTemplate.key"]
        self.assertRegex(sha, r"^[0-9a-f]{64}$")
        self.assertEqual(key, f"bootstrap/cdk-bootstrap-{sha[:12]}.yaml", "the key has the shape rn_bootstrap accepts: bootstrap/cdk-bootstrap-<12 characters>.yaml")
        self.assertEqual(flat["bootstrapTemplate.bytes"], str(len(rb.TEMPLATE_BYTES)))
        self.assertEqual(self.runner_rig.get(key), rb.TEMPLATE_BYTES)

    def test_the_commit_and_digest_compare_the_way_the_wizard_compares_them(self) -> None:
        flat = self.flat(self.run_runner())
        p = self.sh('printf "%s|%s" "$(rn_first12 "$A")" "$(rn_first12 "$B")"', "lib", "json", "client", "runner", A=flat["sourceCommit"], B=flat["configDigest"])
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout, f"{rb.COMMIT[:12]}|{flat['configDigest'][:12]}")
        full_digest = hashlib.sha256(self.runner_rig.config).hexdigest()
        self.assertEqual(flat["configDigest"], full_digest[:12], "the runner writes the 12 digits the wizard puts in the key")

    def test_the_hash_the_wizard_takes_of_a_result_is_the_one_the_runner_records(self) -> None:
        self.run_runner()
        recorded = (self.runner_rig.work / "anchor.env").read_text().strip().split("=", 1)[1]
        file = self.tmp / "result.json"
        file.write_bytes(self.runner_rig.get(f"results/{rb.BUILD_ID}/result.json"))
        p = self.sh('setup_sha256 <"$F"', "lib", F=str(file))
        self.assertEqual((p.returncode, p.stdout.strip()), (0, recorded), p.stderr)

    def test_a_deploy_the_guard_refused_is_exit_7_for_the_wizard(self) -> None:
        doc = self.run_runner(FAKE_DEPLOY="guard")
        flat = self.flat(doc)
        self.assertEqual((flat["state"], flat["failedPhase"]), ("SETUP-FAIL", "guard"))
        p = self.sh('RN_ACTION=deploy; RES_FLAT=$(printf "%s" "$DOC" | json_flat); rn_fail_code', "lib", "json", "client", "runner", DOC=doc)
        self.assertEqual((p.returncode, p.stdout.strip()), (0, "7"), p.stderr)

    def test_any_other_failed_deploy_is_exit_5(self) -> None:
        deploy = self.run_runner(FAKE_DEPLOY="fail")
        p = self.sh('RN_ACTION=deploy; RES_FLAT=$(printf "%s" "$DOC" | json_flat); rn_fail_code', "lib", "json", "client", "runner", DOC=deploy)
        self.assertEqual(p.stdout.strip(), "5", p.stderr)

    def test_the_phase_lines_are_read_by_the_progress_reader(self) -> None:
        self.run_runner()
        lines = self.runner_rig.phase_lines()
        self.assertEqual([ln["name"] for ln in lines], rb.PHASES)
        for ln in lines:
            p = self.sh('printf "%s|%s|%s" "$(printf "%s" "$L" | json_value name)" "$(printf "%s" "$L" | json_value state)" "$(printf "%s" "$L" | json_value seconds)"',
                        "json", L=json.dumps(ln))
            self.assertEqual(p.stdout, f"{ln['name']}|ok|{ln['seconds']}", p.stderr)


class AccountFileContractCase(FullBase):
    """The file the wizard generates, against the override and the runner's account-file phase."""

    def uploaded_file(self) -> Path:
        p = self.run_full(*self.ARGS, "--keep-runner")
        self.assertEqual(p.returncode, 0, self.out(p))
        key = self.start_build_env()["SETUP_CONFIG_KEY"]
        path = self.tmp / "generated-accounts.json"
        path.write_bytes(self.s3_bytes(key))
        self.key = key
        return path

    def target(self, path: Path, *args: str) -> subprocess.CompletedProcess:
        env = {"PATH": os.environ["PATH"], "HAMH_ACCOUNTS_FILE": str(path), "HAMH_AWS_PROFILE": ""}
        return subprocess.run([sys.executable, "-I", "-B", str(REPO / "scripts" / "lib" / "aws_target.py"), *args], capture_output=True, text=True, env=env, timeout=limit(60))  # nosec B603: argument list, no shell

    def test_the_digest_in_the_key_is_the_digest_of_the_file(self) -> None:
        path = self.uploaded_file()
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        self.assertEqual(self.key, f"config/accounts-{digest[:12]}.json")
        shown = self.target(path, "digest")
        self.assertEqual(shown.returncode, 0, shown.stderr)
        self.assertIn(digest, shown.stdout, "the override reports the digest of the file in use (the deploy record names it)")

    def test_the_file_is_what_the_runner_account_file_phase_demands(self) -> None:
        path = self.uploaded_file()
        shell = self.target(path, "shell")
        self.assertEqual(shell.returncode, 0, shell.stderr)
        fields = dict(line.split("=", 1) for line in shell.stdout.splitlines() if line.startswith("HAMH_"))
        self.assertEqual((fields["HAMH_ACCOUNT"], fields["HAMH_REGION"]), (self.ACCOUNT, self.REGION))
        self.assertEqual(fields["HAMH_PROFILE"], "hamh-setup", "the runner writes a profile of this name for the container credentials")
        logging = self.target(path, "get", "invocationLogging")
        self.assertEqual(logging.stdout.strip(), "read-only", "the runner role cannot write the Bedrock invocation-logging setting")

    def test_the_runner_can_use_the_file_where_it_lays_it(self) -> None:
        """runner-build.sh lays the file at <work>/account/accounts.json with mode 0644 and exports HAMH_ACCOUNTS_FILE; the override wants an absolute path."""
        path = self.uploaded_file()
        laid = self.tmp / "account" / "accounts.json"
        laid.parent.mkdir()
        laid.write_bytes(path.read_bytes())
        laid.chmod(0o644)
        self.assertTrue(laid.is_absolute())
        self.assertEqual(self.target(laid, "shell").returncode, 0)


class ClientContractCase(units.HelperCase):
    """build-client.sh against the runner's check and the wizard's reader."""

    def build(self) -> test_build_client.BuildClientCase:
        rig = test_build_client.BuildClientCase("run_default")
        rig.setUp()
        self.addCleanup(rig.doCleanups)
        proc = rig.run_default()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.client_doc = json.loads(proc.stdout)
        return rig

    def test_the_runner_accepts_what_build_client_prints(self) -> None:
        rig = self.build()
        doc_file = rig.tmp / "client.json"
        doc_file.write_text(json.dumps(self.client_doc))
        checked = subprocess.run([sys.executable, "-I", "-B", str(sup.STATE_HELPER), "client-check", str(doc_file)], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell
        self.assertEqual(checked.returncode, 0, checked.stderr)
        rows = [line.split("\t") for line in checked.stdout.splitlines()]
        self.assertEqual([r[0] for r in rows], list(ALL_PLATFORMS))
        self.assertEqual({k: v for k, v in zip(self.client_doc, [r[1] for r in rows])}, {p: d["key"] for p, d in self.client_doc.items()})
        admin = subprocess.run([sys.executable, "-I", "-B", str(sup.STATE_HELPER), "client-check", "--part", "admin", str(doc_file)], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell
        self.assertEqual(admin.returncode, 0, admin.stderr)
        admin_rows = [line.split("\t") for line in admin.stdout.splitlines()]
        self.assertEqual([r[0] for r in admin_rows], list(ALL_PLATFORMS))
        self.assertEqual({k: v for k, v in zip(self.client_doc, [r[1] for r in admin_rows])}, {p: d["admin"]["key"] for p, d in self.client_doc.items()},
                         "the operator tool is checked and listed by the same helper, platform by platform")

    def test_the_wizard_finds_every_platform_in_the_sums_file_build_client_writes(self) -> None:
        rig = self.build()
        sums = (rig.out / "SHA256SUMS").read_text()
        for platform, entry in self.client_doc.items():
            key = entry["key"]
            p = self.sh('cl_sums_get "$S" "$P/$B" "$B" "$K" "${K#client/$ID/}"', "lib", "json", "client", S=sums, P=platform, B="ham", K=key, ID=rig.build_id)
            self.assertEqual((p.returncode, p.stdout), (0, entry["sha256"]), p.stderr)
            admin_key = entry["admin"]["key"]
            q = self.sh('cl_sums_get "$S" "$P/$B" "$B" "$K" "${K#client/$ID/}"', "lib", "json", "client", S=sums, P=platform, B="ham-admin", K=admin_key, ID=rig.build_id)
            self.assertEqual((q.returncode, q.stdout), (0, entry["admin"]["sha256"]), q.stderr)

    def test_the_keys_have_the_layout_the_wizard_downloads_from(self) -> None:
        rig = self.build()
        for platform, entry in self.client_doc.items():
            self.assertEqual(entry["key"], f"client/{rig.build_id}/{platform}/ham")
            self.assertEqual(entry["admin"]["key"], f"client/{rig.build_id}/{platform}/ham-admin")

    def test_the_wizard_and_names_sh_pick_the_same_platform_for_a_machine(self) -> None:
        stub = self.tmp / "unamebin"
        stub.mkdir(exist_ok=True)
        uname = stub / "uname"
        uname.write_text('#!/bin/sh\ncase "$1" in -s) echo "$UNAME_S" ;; -m) echo "$UNAME_M" ;; *) echo "$UNAME_S" ;; esac\n')
        uname.chmod(0o755)
        for system, machine in (("Darwin", "arm64"), ("Linux", "x86_64"), ("Linux", "aarch64"), ("Linux", "arm64"), ("Darwin", "x86_64"), ("FreeBSD", "amd64")):
            with self.subTest(system=system, machine=machine):
                env = {"PATH": f"{stub}:{self.env()['PATH']}", "UNAME_S": system, "UNAME_M": machine}
                wizard = self.sh("cl_platform 2>/dev/null; echo \"rc=$?\"", "lib", "json", "client", **env)
                names = self.sh('ham_host_platform "$UNAME_S" "$UNAME_M" 2>/dev/null; echo "rc=$?"', **env)
                self.assertEqual(wizard.stdout.strip(), names.stdout.strip(), (wizard.stderr, names.stderr))


# Every state a CloudFormation stack can be in (the StackStatus values of the API reference); the wizard lists all but DELETE_COMPLETE when it looks for the stacks of a stage.
ALL_STACK_STATES = (
    "CREATE_IN_PROGRESS", "CREATE_FAILED", "CREATE_COMPLETE", "ROLLBACK_IN_PROGRESS", "ROLLBACK_FAILED", "ROLLBACK_COMPLETE", "DELETE_IN_PROGRESS", "DELETE_FAILED",
    "DELETE_COMPLETE", "UPDATE_IN_PROGRESS", "UPDATE_COMPLETE_CLEANUP_IN_PROGRESS", "UPDATE_COMPLETE", "UPDATE_FAILED", "UPDATE_ROLLBACK_IN_PROGRESS",
    "UPDATE_ROLLBACK_FAILED", "UPDATE_ROLLBACK_COMPLETE_CLEANUP_IN_PROGRESS", "UPDATE_ROLLBACK_COMPLETE", "REVIEW_IN_PROGRESS", "IMPORT_IN_PROGRESS", "IMPORT_COMPLETE",
    "IMPORT_ROLLBACK_IN_PROGRESS", "IMPORT_ROLLBACK_FAILED", "IMPORT_ROLLBACK_COMPLETE",
)
# The states that a stage built on: a stack that was made, was updated, was imported, or whose last update was undone (it holds the state before that update).
COMPLETE_STACK_STATES = {"CREATE_COMPLETE", "UPDATE_COMPLETE", "IMPORT_COMPLETE", "UPDATE_ROLLBACK_COMPLETE"}


class StackStateContractCase(units.HelperCase):
    """Which stack states are complete: the wizard decides it for a stage that exists (`pf_stage` in preflight.sh), the runner decides it for the stage it has just deployed
    (`stacks_complete` in runner-build.sh, called by the outputs phase). They disagreed once: a stack left in UPDATE_ROLLBACK_COMPLETE by a failed update counted for the
    wizard and stopped the runner after a deploy that had passed."""

    def wizard_calls_complete(self, status: str) -> bool:
        body = f"""
aws_run() {{ AWS_OUT=$(for s in network security data packages control orchestration runtimes delivery observability; do printf 'hamh-direct-%s\\t%s\\n' "$s" "{status}"; done); AWS_ERR=; return 0; }}
pf_deploy_record() {{ return 0; }}
pf_control_endpoint() {{ CTRL_ENDPOINT=; }}
pf_bootstrap_version() {{ :; }}
rn_describe() {{ :; }}
STAGE=direct
pf_stage
printf '%s' "$STAGE_STATE"
"""
        p = self.sh(body, "lib", "preflight")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(p.stdout, {"none", "partial", "complete", "deploying", "broken"}, p.stderr)
        return p.stdout == "complete"

    def runner_calls_complete(self, status: str) -> bool:
        rig = rb.Rig(self)
        rig.state["stack_status"] = status
        proc = rig.run()
        return proc.returncode == 0 and {ln["name"]: ln["state"] for ln in rig.phase_lines()}.get("outputs") == "ok"

    def test_the_list_of_states_below_is_every_state_the_wizard_looks_for_and_the_one_it_leaves_out(self) -> None:
        p = self.sh('printf "%s" "$SETUP_LIVE_STATUSES"', "lib")
        self.assertEqual(sorted(p.stdout.split() + ["DELETE_COMPLETE"]), sorted(ALL_STACK_STATES))

    def test_the_wizard_and_the_runner_call_the_same_states_complete(self) -> None:
        for status in ALL_STACK_STATES:
            if status == "DELETE_COMPLETE":
                continue  # a deleted stack is not listed by the wizard (SETUP_LIVE_STATUSES); the runner's refusal of it is a case of test_setup_runner_build
            with self.subTest(status=status):
                wizard, runner = self.wizard_calls_complete(status), self.runner_calls_complete(status)
                self.assertEqual(wizard, status in COMPLETE_STACK_STATES, f"the wizard on {status}")
                self.assertEqual(runner, status in COMPLETE_STACK_STATES, f"the runner on {status}")


class DoxContractCase(unittest.TestCase):
    def test_every_file_of_scripts_setup_is_named_in_its_agents_md(self) -> None:
        text = (SETUP_DIR / "AGENTS.md").read_text(encoding="utf-8")
        missing: List[str] = [p.name for p in sorted(SETUP_DIR.iterdir()) if p.is_file() and p.name != "AGENTS.md" and p.name not in text]
        self.assertEqual(missing, [], "scripts/setup/AGENTS.md names every file of the folder")


if __name__ == "__main__":
    unittest.main()
