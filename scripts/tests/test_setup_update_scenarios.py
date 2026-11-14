"""The states a user can be in when they run `./setup.sh --update`, one test per row of the inventory.

Each test is named after its row (`test_b812_...`) and runs the real `setup.sh` from a stand-in checkout against the stand-in AWS CLI (setup_fake_aws.py), as the other
`test_setup_*` suites do. The rows cover the preflight and the plan (the stage's settings record `/hamh/<stage>/settings`, the Instances part, the commit the stage runs, the
operator and the zone the stage was deployed with), the local route (`--build-on local`: what reaches scripts/deploy.sh, where the deploy records go, what a failure says), where
the client is installed and when a new client may replace the one that runs, and the messages of the early refusals and of the one confirmation.

The settings registry `scripts/lib/stage-settings.tsv` belongs to the deploy engine; the tests write a stand-in copy of it into the stand-in checkout only (REGISTRY below).
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import subprocess  # nosec B404: the tests start bash with an argument list, never a shell string
import sys
import unittest
from pathlib import Path

import setup_fake_aws
from setup_waits import limit
import test_setup_full_flow as flow
from test_setup_full_state import DIGEST
from test_setup_script import STUB_HAM
from test_setup_update_guard import UpdateBase

ACCOUNT = setup_fake_aws.ACCOUNT
RUNS = ["01RUNAAAAAAAAAAAAAAAAAAAAA", "01RUNBBBBBBBBBBBBBBBBBBBBB"]
SETTINGS_PARAM = "/hamh/direct/settings"
INSTANCES_PARAM = "/hamh/direct/instances-capacity-provider-id"
PROVIDER_ARN = f"arn:aws:bedrock-agentcore:us-east-1:{ACCOUNT}:capacity-provider/hamh_direct_instances_aaaaaaaa-cp1abcdefg"
OLD_ENDPOINT = "https://old0123456.execute-api.us-west-2.amazonaws.com/direct"
UPDATE_LOCAL = ("--update", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes")
# The registry of this checkout: the cases of the keep tag and the stage kind take their two rows from it.
REAL_REGISTRY = Path(__file__).resolve().parents[2] / "scripts" / "lib" / "stage-settings.tsv"
# A stand-in of the deploy engine's registry (C1 of the frozen contract: NAME|KEPT|CHANGE|PATTERN|NOTE; a PATTERN may hold `|`, the NOTE never does).
REGISTRY = """# stand-in of scripts/lib/stage-settings.tsv for the tests of setup.sh
HAMH_COMPUTE_INSTANCES|yes|refuse-off|^(on|off)$|Whether the stage has the Instances part (on or off).
HAMH_INSTANCES_TYPES|yes|replace|^[a-z0-9.]+(,[a-z0-9.]+)*$|The EC2 instance types of the Instances part, comma separated.
HAMH_TOOLS_MODE|yes|free|^(on|off)$|Whether the worker runs the tool layer (on or off).
HAMH_UTILITY_MODEL|yes|free|^[A-Za-z0-9._:-]+$|The model that writes run names and tags, or off.
HAMH_LOG_RETENTION_DAYS|yes|decrease|^[0-9]+$|How many days the stage keeps its logs.
HAMH_SLICE_DEADLINE_S|no|free|^[0-9]+$|A test override of the slice deadline in seconds.
HAMH_TAVILY_ALARMS|yes|remove|^(on|off)$|The alarms on the stored search key, on or off.
"""
# scripts/deploy.sh of the local route: it records its arguments and the variables it was given, and plays the deploy engine's answers (FAKE_DEPLOY).
DEPLOY = r'''#!/bin/sh
printf "%s|%s|%s\n" "$*" "${HAMH_ACCOUNTS_FILE:-}" "${HAMH_AWS_PROFILE:-}" >> "$STUB_LOG/deploy.log"
{ printenv | grep -E '^(HAMH_|CAH_|DEPLOY_RECORD_DIR=|DEPLOY_CRITERIA=)' | sort; echo --; } >> "$STUB_LOG/deploy-env.log"  # COMPAT-CAH: the earlier spelling must not reach the deploy
case " $* " in *" --relabel "*)
  # The release label of an update that deploys nothing (scripts/deploy.sh --relabel), which these cases count apart from the deploys
  case "${FAKE_RELABEL:-pass}" in
    pass) echo "DEPLOY-LABEL stage=direct version=${FAKE_LABEL_VERSION:-9.99} was=9.98"; exit 0 ;;
    refused) echo "DEPLOY-REFUSED reason=not-deployed wait=no stage=unchanged :: the stage does not run this source tree"; exit 3 ;;
    fail) echo "DEPLOY-FAIL release-label could not write the parameter"; exit 1 ;;
  esac ;;
esac
case "${FAKE_DEPLOY:-pass}" in
  instances) [ "${HAMH_COMPUTE_INSTANCES:-}" = on ] || { echo "DEPLOY-FAIL instances-off the stage has the Instances part and HAMH_COMPUTE_INSTANCES is not set"; exit 1; } ;;
  fail) echo "deploy: cdk deploy hamh-direct-network"; echo "DEPLOY-FAIL stack hamh-direct-network UPDATE_FAILED"; exit 1 ;;
  guard) echo "DEPLOY-FAIL guard 2 running run(s) on hamh-direct-run pin the worker runtime: run-a run-b"; exit 1 ;;
  refused) echo "DEPLOY-REFUSED reason=owner wait=no stage=unchanged :: export HAMH_OWNER=keepme, or unset HAMH_OWNER"; exit 3 ;;
  refused-wait) echo "DEPLOY-REFUSED reason=lock wait=yes stage=unchanged :: another deploy holds the lock; wait until it ends, then run the update again"; exit 3 ;;
  before-change) echo "DEPLOY-FAIL synth cdk synth failed: run npm ci in infra and try again"; echo "DEPLOY-STAGE unchanged"; exit 1 ;;
esac
aws cloudformation deploy --stack-name hamh-direct-control --template-file infra/setup-runner.yaml --capabilities CAPABILITY_NAMED_IAM >/dev/null
echo "DEPLOY-PASS stage=direct digest=aaaaaaaaaaaa sourceTree=bbbbbbbbbbbb"
'''
# A client that the build produced and that does not run on this machine (it holds the header name, as every build of the client does).
BROKEN_CLIENT = "#!/bin/sh\n# x-hamh-backend-version\nprintf '%s\\n' \"$*\" >> \"${STUB_LOG:?}/ham-broken.log\"\necho 'dyld: Library not loaded' >&2\nexit 134\n"
# The client with `config migrate`, which copies the settings of the earlier folder (the real command is the client's).
MIGRATING = STUB_HAM.replace('case "$1 $2" in\n', 'case "$1 $2" in\n  "config migrate")\n    mkdir -p "$(dirname "$CFG")"; '
                                                  'cp "${XDG_CONFIG_HOME:-$HOME/.config}/cah/config.toml" "$CFG"; echo "migrated"; exit 0;;\n', 1)  # COMPAT-CAH
assert MIGRATING != STUB_HAM


def lines_of(text: str) -> list:
    return [ln for ln in text.splitlines() if ln.strip()]


class ScenarioBase(UpdateBase):
    """An update of a stage that exists, with the helpers the rows share."""

    # ---- the stand-in checkout --------------------------------------------------------------------
    def with_registry(self, text: str = REGISTRY) -> None:
        d = self.repo / "scripts" / "lib"
        d.mkdir(parents=True, exist_ok=True)
        (d / "stage-settings.tsv").write_text(text)
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the settings registry")

    def write_commit(self, files: dict, message: str = "change") -> str:
        for rel, text in files.items():
            p = self.repo / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.commit()

    # ---- the stage --------------------------------------------------------------------------------
    def stage(self, **more) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, **more)
        self.open_world().save()

    def seed_record(self, commit: str | None = None, version: str = "", operator: str | None = None, azs: str = "use1-az1", status: str = "deployed",
                    **settings: str) -> None:
        lines = ["schema=1", f"status={status}", f"commit={commit or self.commit()}", "tree=" + "1" * 64, f"version={version}", f"accounts={DIGEST}",
                 f"operator={operator or f'arn:aws:iam::{ACCOUNT}:user/setup-test'}", f"azs={azs}", "updated=2026-11-10T16:00:00Z"]
        lines += [f"set.{k}={v}" for k, v in settings.items()]
        w = self.open_world()
        w.w.setdefault("params", {})[SETTINGS_PARAM] = "\n".join(lines)
        w.save()

    def drop_record(self) -> None:
        """The stage as a release before the record left it: no /hamh/<stage>/settings (the build of a commit that holds the registry writes one)."""
        w = self.open_world()
        w.w.get("params", {}).pop(SETTINGS_PARAM, None)
        w.save()

    def seed_instances(self) -> None:
        w = self.open_world()
        w.w["stacks"]["hamh-direct-runtimes"]["outputs"].update(InstancesCapacityProviderArn=PROVIDER_ARN,
                                                                 WorkerPythonInstancesRuntimeId=setup_fake_aws.INSTANCES_RUNTIME_ID)
        w.w.setdefault("params", {})[INSTANCES_PARAM] = "hamh_direct_instances_aaaaaaaa-cp1abcdefg"
        w.save()

    def reset_world(self) -> None:
        if self.state.exists():
            shutil.rmtree(self.state)

    # ---- the local route ----------------------------------------------------------------------------
    DEPLOY = DEPLOY

    def local_checkout(self) -> None:
        flow.FullLocalBuildCase.make_local_checkout(self)
        flow.FullLocalBuildCase.tools_on_path(self, *flow.FullLocalBuildCase.TOOLS)

    def run_local(self, *extra: str, **env: str):
        return self.run_full(*UPDATE_LOCAL, *extra, STUB_HAM_SOURCE=str(self.ham_source), **env)

    def deploy_env(self) -> list:
        """The HAMH_* and DEPLOY_* variables each run of the stand-in deploy got, one dict per run."""
        out, cur = [], {}
        for ln in self.log_lines("deploy-env.log"):
            if ln == "--":
                out.append(cur)
                cur = {}
            elif "=" in ln:
                k, v = ln.split("=", 1)
                cur[k] = v
        return out

    # ---- what a run leaves ----------------------------------------------------------------------------
    def receipt_dir(self) -> Path:
        return self.receipt_path().parent

    def generated_accounts(self) -> dict:
        return json.loads((self.receipt_dir() / "accounts-generated.json").read_text())

    def plan(self, p) -> str:
        return " ".join(p.stdout.split())


class B812RowsCase(ScenarioBase):
    # ---- U1 ---------------------------------------------------------------------------------------------
    def test_b812_default_stage_update_deploys_the_commit_installs_the_client_and_removes_the_runner(self) -> None:
        self.deployed()
        for machine in ("the machine of the receipt, with a runner left over", "a second machine with no receipt"):
            with self.subTest(machine=machine):
                if machine.startswith("a second"):
                    shutil.rmtree(self.home / ".config" / "ham" / "setup")
                else:
                    self.seed_runner()
                new = self.commit_more(f"CHANGES-{len(machine)}.md")
                self.client_file.write_text(STUB_HAM + f"# client of {new}\n")
                builds = self.count_calls("codebuild", "start-build")
                p = self.run_full(*self.UPDATE)
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertEqual(self.count_calls("codebuild", "start-build"), builds + 1)
                self.assertEqual(self.start_build_env(builds)["SETUP_SOURCE_KEY"], f"source/{new}.zip")
                self.assertTrue(self.installed().read_text().endswith(f"# client of {new}\n"), "the new client is installed")
                self.assertFalse(self.runner_installed(), "the runner is removed after the client is in place")
                self.assertIn("models", self.log_lines())
                cfg = self.config_file().read_text()
                self.assertIn(f'apiEndpoint = "{self.ENDPOINT}"', cfg)
                self.assertIn('region = "us-east-1"', cfg)
                self.assertEqual(self.receipt()["sourceCommit"], new)

    # ---- U3 ---------------------------------------------------------------------------------------------
    def test_b812_local_update_of_an_instances_stage_keeps_the_part_or_stops_before_anything_is_created(self) -> None:
        self.stage(nine_stacks=True)
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()
        # control: a stage without the part deploys with nothing said about it
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("HAMH_COMPUTE_INSTANCES", self.deploy_env()[-1])
        # TRIGGER PRESENT: the stage has the Instances part
        self.seed_instances()
        self.assertIn(INSTANCES_PARAM, self.world()["params"])
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        # Addendum 4, A4.17: setup exports nothing you did not export; the deploy keeps the part from the stage's record or the live stage, on both routes alike
        self.assertNotIn("HAMH_COMPUTE_INSTANCES", self.deploy_env()[-1], "nothing counts as explicit that you did not export")
        plan = self.plan(p)
        self.assertIn("Instances part: present, kept", plan)
        self.assertNotIn("the deploy gets HAMH_COMPUTE_INSTANCES=on", plan)
        # an exported on reaches the deploy as your own value
        p = self.run_local(FAKE_DEPLOY="instances", HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.deploy_env()[-1].get("HAMH_COMPUTE_INSTANCES"), "on")
        # an explicit request to remove the part is refused before anything is created
        deploys = len(self.log_lines("deploy.log"))
        before = len(self.mutations())
        p = self.run_local(HAMH_COMPUTE_INSTANCES="off")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        self.assertEqual(len(self.log_lines("deploy.log")), deploys, "scripts/deploy.sh did not run")
        flat = " ".join(p.stderr.split())
        self.assertIn("HAMH_COMPUTE_INSTANCES=off", flat)
        self.assertIn("Instances part", flat)
        self.assertIn("Nothing was changed.", flat)
        # A2.2: `default` returns to the code default, which is off: refused the same way
        p = self.run_local(HAMH_COMPUTE_INSTANCES="default")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        self.assertEqual(len(self.log_lines("deploy.log")), deploys)

    # ---- U12 ---------------------------------------------------------------------------------------------
    def test_b812_an_exported_stage_setting_reaches_the_deploy_on_both_routes_or_is_refused(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        self.drop_record()
        # the plan shows each setting and where it comes from
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_TOOLS_MODE="on", HAMH_SLICE_DEADLINE_S="")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("HAMH_TOOLS_MODE=on (explicit", plan)
        self.assertIn("settings: read from the live stage by the deploy and kept", plan, "a stage without a record keeps what the live stage has")
        self.seed_record(HAMH_LOG_RETENTION_DAYS="365")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", CAH_TOOLS_MODE="on")  # COMPAT-CAH: the earlier spelling is folded first
        plan = self.plan(p)
        self.assertIn("HAMH_LOG_RETENTION_DAYS=365 (kept", plan)
        self.assertIn("HAMH_TOOLS_MODE=on (explicit", plan)
        # A2.1: an exported empty value counts as unset, so the stage's value is kept; only `default` goes back to the code default
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_LOG_RETENTION_DAYS="")
        self.assertIn("HAMH_LOG_RETENTION_DAYS=365 (kept", self.plan(p))
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_LOG_RETENTION_DAYS="default")
        self.assertIn("HAMH_LOG_RETENTION_DAYS: 365 -> default (explicit", self.plan(p))
        # a value the registry does not accept is refused before the question, on both routes, naming the setting and what it is
        flow.FullLocalBuildCase.tools_on_path(self, *flow.FullLocalBuildCase.TOOLS)
        for route in ((), ("--build-on", "local")):
            with self.subTest(route=route or "runner"):
                before = len(self.mutations())
                p = self.run_full("--update", *route, "--profile", "work", "--region", "us-east-1", "--yes", HAMH_TOOLS_MODE="maybe", STUB_HAM_SOURCE=str(self.ham_source))
                self.assertEqual(p.returncode, 2, self.out(p))
                flat = " ".join(p.stderr.split())
                self.assertIn("HAMH_TOOLS_MODE", flat)
                self.assertIn("Whether the worker runs the tool layer", flat)
                self.assertEqual(len(self.mutations()), before)
        # a name that looks like a stage setting and is none is refused; a documented control of the same family is not
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_INSTANCES_TYPE="m7g.large")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("HAMH_INSTANCES_TYPE", p.stderr)
        self.assertIn("HAMH_INSTANCES_TYPES", p.stderr)
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_TOOLS_URL="http://127.0.0.1:8123")
        self.assertEqual(p.returncode, 0, self.out(p))
        # HAMH_REPO_NAMES is the same value as --repo-name: in the plan and in the account file
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_REPO_NAMES="team-*")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("repository names: team-*", self.plan(p))
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--repo-name", "other-*", "--dry-run", HAMH_REPO_NAMES="team-*")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("HAMH_REPO_NAMES", p.stderr)
        # the row itself (Addendum 4, A4.15): the exported setting reaches the deploy on BOTH routes. The runner route: one override of the build ...
        p = self.run_full(*self.UPDATE, HAMH_TOOLS_MODE="on")
        self.assertEqual(p.returncode, 0, self.out(p))
        env = self.start_build_env(self.count_calls("codebuild", "start-build") - 1)
        self.assertEqual(base64.b64decode(env.get("SETUP_EXPLICIT_SETTINGS", "")).decode(), "HAMH_TOOLS_MODE=on\n", "the runner route carries the exported setting to its build")
        # ... and the local route: the environment of scripts/deploy.sh
        self.local_checkout()
        p = self.run_local(HAMH_TOOLS_MODE="on")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.deploy_env()[-1].get("HAMH_TOOLS_MODE"), "on", "the local route hands the same setting to scripts/deploy.sh")

    # The runner half of U12: setup builds SETUP_EXPLICIT_SETTINGS (preflight.sh pf_explicit_settings), and rn_start of runner.sh passes it as one override of the build.
    def test_b812_an_exported_stage_setting_reaches_the_runner_build(self) -> None:
        """U12, the runner route: the exported settings reach the build in one override."""
        self.with_registry()
        self.deployed()
        self.commit_more()
        p = self.run_full(*self.UPDATE, HAMH_TOOLS_MODE="on", HAMH_UTILITY_MODEL="off")
        self.assertEqual(p.returncode, 0, self.out(p))
        env = self.start_build_env(1)
        self.assertIn("SETUP_EXPLICIT_SETTINGS", env, "the runner route passes the exported settings")
        self.assertEqual(base64.b64decode(env["SETUP_EXPLICIT_SETTINGS"]).decode(), "HAMH_TOOLS_MODE=on\nHAMH_UTILITY_MODEL=off\n")

    def test_b812_an_exported_stage_setting_reaches_the_local_deploy(self) -> None:
        """U12, the local route: the same settings reach scripts/deploy.sh."""
        self.stage()
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()
        p = self.run_local(HAMH_TOOLS_MODE="on", CAH_UTILITY_MODEL="off")  # COMPAT-CAH
        self.assertEqual(p.returncode, 0, self.out(p))
        got = self.deploy_env()[-1]
        self.assertEqual(got.get("HAMH_TOOLS_MODE"), "on")
        self.assertEqual(got.get("HAMH_UTILITY_MODEL"), "off")
        self.assertIn("HAMH_TOOLS_MODE=on (explicit", self.plan(p))

    # ---- Addendum 3, A3.1: the keep tag and the kind of the stage ------------------------------------------------------------------------------------------
    def test_b812_the_keep_tag_and_the_stage_kind_are_kept_and_checked_like_every_setting(self) -> None:
        """Setup's half of A3.1, with the two rows of the real registry: a record's value is shown as kept, an exported change names its class, and a value
        the row does not take ends the run with exit 2 before the plan and the question, on both routes, with nothing created."""
        rows = {ln.split("|", 1)[0]: ln for ln in REAL_REGISTRY.read_text().splitlines() if ln.startswith(("HAMH_KEEP_TAG|", "HAMH_STAGE_KIND|"))}
        self.assertEqual(sorted(rows), ["HAMH_KEEP_TAG", "HAMH_STAGE_KIND"], "TRIGGER PRESENT: the registry has the rows of the keep tag and the stage kind")
        self.with_registry(REGISTRY + "".join(row + "\n" for row in rows.values()))
        self.deployed()
        self.commit_more()
        self.seed_record(HAMH_KEEP_TAG="on", HAMH_STAGE_KIND="test")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("HAMH_KEEP_TAG=on (kept: the stage's record)", plan)
        self.assertIn("HAMH_STAGE_KIND=test (kept: the stage's record)", plan)
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_KEEP_TAG="off")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("HAMH_KEEP_TAG: on -> off (explicit: exported in this shell; the stage's record had on; change class remove)", self.plan(p))
        flow.FullLocalBuildCase.tools_on_path(self, *flow.FullLocalBuildCase.TOOLS)
        for name, value in (("HAMH_KEEP_TAG", "maybe"), ("HAMH_STAGE_KIND", "staging")):
            note = rows[name].rsplit("|", 1)[1]
            for route in ((), ("--build-on", "local")):
                with self.subTest(name=name, route=route or "runner"):
                    before = len(self.mutations())
                    p = self.run_full("--update", *route, "--profile", "work", "--region", "us-east-1", "--yes", STUB_HAM_SOURCE=str(self.ham_source), **{name: value})
                    self.assertEqual(p.returncode, 2, self.out(p))
                    flat = " ".join(p.stderr.split())
                    self.assertIn(f"{name}={value} is not a value this setting takes", flat)
                    self.assertIn(" ".join(note.split()), flat)
                    self.assertNotIn("What setup will do", p.stdout, "the run ends before the plan and its question")
                    self.assertEqual(len(self.mutations()), before, "nothing is created")

    # ---- U13 ---------------------------------------------------------------------------------------------
    def test_b812_update_keeps_the_operator_role_and_zone_the_stage_was_deployed_with(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        # the zone of the record, not the lowest one that is available now
        self.seed_record(azs="use1-az2")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        entry = self.generated_accounts()["accounts"][0]
        self.assertEqual(entry["azIds"]["direct"], ["use1-az2"])
        self.assertEqual(entry["azs"]["direct"], ["us-east-1c"])
        self.assertEqual(entry["operator"], {"kind": "user", "name": "setup-test"})
        # another operator than the stage's: refused before anything is created, unless --change-operator
        self.seed_record(operator=f"arn:aws:iam::{ACCOUNT}:role/StageAdmin")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        flat = " ".join(p.stderr.split())
        for needle in ("role StageAdmin", "user setup-test", "--change-operator", "Nothing was changed."):
            self.assertIn(needle, flat)
        p = self.run_full(*self.UPDATE, "--change-operator")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("operator: role StageAdmin -> user setup-test", self.plan(p))
        # a role session of the recorded role is the same operator
        self.seed_record(operator=f"arn:aws:iam::{ACCOUNT}:role/StageAdmin")
        self.set_scenario(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/StageAdmin/someone")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("->", " ".join(ln for ln in p.stdout.splitlines() if "operator:" in ln))
        # the Region does not list the stage's zone as available now: the stage keeps it all the same (Addendum 4, A4.11), and the subnets of its network name it
        self.set_scenario(arn=f"arn:aws:iam::{ACCOUNT}:user/setup-test", azs=[["us-east-1b", "use1-az1"], ["us-east-1d", "use1-az4"]],
                          subnet_zones=[["us-east-1c", "use1-az2"]])
        self.seed_record(azs="use1-az2")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        entry = self.generated_accounts()["accounts"][0]
        self.assertEqual((entry["azIds"]["direct"], entry["azs"]["direct"]), (["use1-az2"], ["us-east-1c"]), "the zone of the record, not the lowest zone that is available")
        self.assertIn("zone: us-east-1c (use1-az2), kept from the stage's record", self.plan(p))
        # the network of the stage runs elsewhere than its record says: an update in either zone would replace subnets, so it stops before anything is created
        self.set_scenario(subnet_zones=[["us-east-1d", "use1-az4"]], azs=[["us-east-1b", "use1-az1"], ["us-east-1d", "use1-az4"]])
        self.seed_record(azs="use1-az2")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("use1-az2", p.stderr)
        self.assertEqual(len(self.mutations()), before)

    # ---- U13, a stage without a record (CONTRACT Addendum 3, A3.2) ----------------------------------------------------------------
    def test_b812_a_stage_without_a_record_keeps_its_live_zone(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        self.drop_record()
        # a release deployed the stage, so it has no record; its network runs in use1-az2, and the lowest zone the Region offers is use1-az1
        self.set_scenario(subnet_zones=[["us-east-1c", "use1-az2"], ["us-east-1c", "use1-az2"]])
        self.assertIsNone(self.open_world().stage_record(), "TRIGGER PRESENT: the stage has no record")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        entry = self.generated_accounts()["accounts"][0]
        self.assertEqual((entry["azIds"]["direct"], entry["azs"]["direct"]), (["use1-az2"], ["us-east-1c"]), "the zone of the stage's network, not the lowest zone")
        plan = self.plan(p)
        self.assertIn("zone: us-east-1c (use1-az2), kept from the stage's network", plan)
        self.assertIn("operator: user setup-test (kept: the operator the stage's control function trusts", plan)
        reads = [c for c in self.normalized_calls() if c[:2] == ["ec2", "describe-subnets"]]
        self.assertTrue(any("Name=tag:aws:cloudformation:stack-name,Values=hamh-direct-network" in c for c in reads), reads)
        # the Region does not list the zone as available now: the stage's zone is kept all the same, because another zone replaces the subnets
        # (each update that succeeded wrote the stage's record; the stage of this test is one without it)
        self.drop_record()
        self.set_scenario(subnet_zones=[["us-east-1c", "use1-az2"]], azs=[["us-east-1b", "use1-az1"], ["us-east-1d", "use1-az4"]])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.generated_accounts()["accounts"][0]["azIds"]["direct"], ["use1-az2"])
        # a network in more zones than the stage takes: an update would replace subnets, so it stops before anything is created
        self.drop_record()
        self.set_scenario(subnet_zones=[["us-east-1c", "use1-az2"], ["us-east-1d", "use1-az4"]], azs=None)
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("use1-az2", p.stderr)
        self.assertIn("use1-az4", p.stderr)
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        # a network whose subnets name no zone: the lowest zone, as for a new stage
        self.set_scenario(subnet_zones=None)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.generated_accounts()["accounts"][0]["azIds"]["direct"], ["use1-az1"])
        self.assertNotIn("kept from the stage's network", self.plan(p))
        # with a record, its zone decides and the subnets are not read
        self.set_scenario(subnet_zones=[["us-east-1d", "use1-az4"]])
        self.seed_record(azs="use1-az2")
        calls = len(self.normalized_calls())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.generated_accounts()["accounts"][0]["azIds"]["direct"], ["use1-az2"])
        self.assertFalse([c for c in self.normalized_calls()[calls:] if c[:2] == ["ec2", "describe-subnets"]])

    # ---- U14 ---------------------------------------------------------------------------------------------
    def test_b812_stage_states_decide_the_update_as_before(self) -> None:
        cases = (
            ("unfinished", dict(stage_deployed=True, unfinished_deploy=True), 0, "did not finish"),
            ("rolled back update", dict(stacks={"hamh-direct-network": {"status": "UPDATE_ROLLBACK_COMPLETE", "outputs": {}},
                                                "hamh-direct-control": {"status": "UPDATE_ROLLBACK_COMPLETE", "outputs": {"V1ApiUrl": self.ENDPOINT}}}), 0, ""),
            ("being changed", dict(stacks={"hamh-direct-network": {"status": "UPDATE_IN_PROGRESS", "outputs": {}},
                                           "hamh-direct-control": {"status": "UPDATE_COMPLETE", "outputs": {"V1ApiUrl": self.ENDPOINT}}}), 7, "hamh-direct-network UPDATE_IN_PROGRESS"),
            ("broken", dict(stacks={"hamh-direct-network": {"status": "DELETE_FAILED", "outputs": {}},
                                    "hamh-direct-control": {"status": "UPDATE_COMPLETE", "outputs": {"V1ApiUrl": self.ENDPOINT}}}), 7, "console"),
            ("previous names", dict(legacy_stage_deployed=True), 7, "./setup.sh --teardown"),
            ("bootstrap of another qualifier", dict(stage_deployed=True, bootstrap_present=False,
                                                    stacks={"CDKToolkit": {"status": "CREATE_COMPLETE", "outputs": {}}}), 7, "hnb659fds"),
        )
        for name, scenario, code, needle in cases:
            with self.subTest(state=name):
                self.reset_world()
                self.scenario = {"client_file": str(self.client_file), "admin_file": str(self.admin_file)}
                self.set_scenario(bootstrap_present=scenario.pop("bootstrap_present", True), **scenario)
                self.ham_on_path()
                builds = self.count_calls("codebuild", "start-build")
                mutations = len(self.mutations())
                p = self.run_full(*self.UPDATE)
                self.assertEqual(p.returncode, code, self.out(p))
                if needle:
                    self.assertIn(needle, self.out(p))
                if code == 0:
                    self.assertEqual(self.count_calls("codebuild", "start-build"), builds + 1, "the deploy runs again")
                else:
                    self.assertEqual(len(self.mutations()), mutations, "nothing was created")
                    self.assertEqual(self.count_calls("codebuild", "start-build"), builds)

    # ---- U15 ---------------------------------------------------------------------------------------------
    def test_b812_no_stage_here_points_at_the_region_or_profile_and_legacy_leftovers_at_only_legacy(self) -> None:
        # nothing anywhere: the full setup is the way
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("./setup.sh --full", p.stderr)
        self.assertEqual(self.mutations(), [])
        # the stage lives in the other supported Region: that Region is named before the full setup
        self.set_scenario(other_region_stacks={"us-west-2": [["hamh-direct-network", "CREATE_COMPLETE"], ["hamh-direct-control", "UPDATE_COMPLETE"]]})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        err = p.stderr
        self.assertIn("--region us-west-2", err)
        self.assertLess(err.index("--region us-west-2"), err.index("--full"), "the Region comes first, so following the advice does not make a second stage")
        self.assertEqual(self.mutations(), [])
        # a receipt of this machine names the stage in another account: the profile of that run is named
        self.set_scenario(other_region_stacks={})
        other = self.home / ".config" / "ham" / "setup" / "999988887777" / "us-east-1"
        other.mkdir(parents=True)
        (other / "direct.json").write_text(json.dumps({"schema": "1", "account": "999988887777", "region": "us-east-1", "stage": "direct",
                                                       "profile": "otherwork", "endpoint": "https://zzz0123456.execute-api.us-east-1.amazonaws.com/direct",
                                                       "state": "installed"}))
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("--profile otherwork", p.stderr)
        self.assertLess(p.stderr.index("--profile otherwork"), p.stderr.index("--full"))
        # leftovers of the previous names beside a current stage: step 1 removes only the leftovers
        self.reset_world()
        self.set_scenario(stage_deployed=True, legacy_runner_present=True, other_region_stacks={})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("1. Run ./setup.sh --teardown --only legacy", p.stderr)
        self.assertEqual(self.mutations(), [])

    # ---- U19 ---------------------------------------------------------------------------------------------
    def test_b812_settings_only_in_the_earlier_folder_choose_the_account_and_stay_consistent(self) -> None:
        self.stage()
        old = self.home / ".config" / "cah"  # COMPAT-CAH: the settings folder of the earlier name
        old.mkdir(parents=True)
        (old / "config.toml").write_text('awsProfile = "legacyprof"\nregion = "us-east-1"\napiEndpoint = "https://old0123456.execute-api.us-east-1.amazonaws.com/direct"\n')
        with open(self.home / ".aws" / "config", "a") as fh:
            fh.write("\n[profile legacyprof]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n")
        migrating = self.tmp / "migrating-client"
        migrating.write_text(MIGRATING)
        self.set_scenario(client_file=str(migrating))
        p = self.run_full("--update", "--yes", AWS_PROFILE="work", AWS_REGION="us-west-2")
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = [c for c in self.calls() if c != ["--version"]]
        self.assertTrue(calls)
        wrong = [" ".join(c) for c in calls if "--profile" not in c or c[c.index("--profile") + 1] != "legacyprof" or c[c.index("--region") + 1] != "us-east-1"]
        self.assertEqual(wrong, [], "the profile and the Region of the earlier settings choose the account for every call")
        cfg = self.config_file().read_text()
        self.assertIn('awsProfile = "legacyprof"', cfg)
        self.assertIn('region = "us-east-1"', cfg)
        self.assertIn(f'apiEndpoint = "{self.ENDPOINT}"', cfg)
        self.assertIn("config migrate", self.log_lines())

    # ---- U20 ---------------------------------------------------------------------------------------------
    def test_b812_an_earlier_client_name_is_replaced_and_the_new_endpoint_written(self) -> None:
        for earlier in ("recognised", "foreign"):
            with self.subTest(earlier=earlier):
                self.reset_world()
                shutil.rmtree(self.home / ".config", ignore_errors=True)
                shutil.rmtree(self.prefix, ignore_errors=True)
                self.stage()
                bindir = self.prefix / "bin"
                bindir.mkdir(parents=True)
                old = bindir / "cah"  # COMPAT-CAH: the earlier command name
                old.write_text("#!/bin/sh\n# x-cah-backend-version\necho cah\n" if earlier == "recognised" else "#!/bin/sh\necho another tool\n")  # COMPAT-CAH
                old.chmod(0o755)
                cfg = self.config_file()
                cfg.parent.mkdir(parents=True, exist_ok=True)
                cfg.write_text(f'apiEndpoint = "{OLD_ENDPOINT}"\nregion = "us-east-1"\n')
                p = self.run_full(*self.UPDATE)
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertTrue(self.installed().is_file())
                sums = (self.receipt_dir() / "download" / "verified" / "SHA256SUMS").read_text()
                self.assertIn(hashlib.sha256(self.installed().read_bytes()).hexdigest(), sums)
                if earlier == "recognised":
                    self.assertTrue(old.is_symlink())
                    self.assertEqual(os.readlink(old), "ham")
                else:
                    self.assertFalse(old.is_symlink())
                    self.assertIn("another tool", old.read_text())
                self.assertIn(f"api_endpoint changed from {OLD_ENDPOINT} to {self.ENDPOINT}", p.stdout)

    # ---- U21 ---------------------------------------------------------------------------------------------
    def test_b812_a_client_elsewhere_on_path_is_updated_or_named(self) -> None:
        opt = self.home / "opt"
        self.assertEqual(self.run_full(*self.ARGS, "--prefix", str(opt)).returncode, 0)
        self.assertTrue((opt / "bin" / "ham").is_file())
        # the update replaces the client where the receipt says it was installed
        self.commit_more()
        self.client_file.write_text(STUB_HAM + "# newer\n")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((opt / "bin" / "ham").read_text().endswith("# newer\n"))
        self.assertFalse(self.installed().exists(), "nothing is installed in the default folder")
        # another client starts first on the PATH: both paths are named
        self.ham_on_path()
        self.commit_more("MORE.md")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        warned = " ".join(ln for ln in p.stdout.splitlines() if ln.startswith("warn"))
        self.assertIn(str(self.stub / "ham"), warned)
        self.assertIn(str(opt / "bin" / "ham"), warned)
        # no receipt: the client the PATH finds is the one replaced
        shutil.rmtree(self.home / ".config" / "ham" / "setup")
        self.commit_more("THIRD.md")
        self.client_file.write_text(STUB_HAM + "# third\n")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.stub / "ham").read_text().endswith("# third\n"))
        # a program of the client's name that is not this client is never overwritten without a check
        shutil.rmtree(self.home / ".config" / "ham" / "setup")
        (self.stub / "ham").unlink()
        foreign = self.installed()
        foreign.parent.mkdir(parents=True, exist_ok=True)
        foreign.write_text("#!/bin/sh\necho another program\n")
        self.commit_more("FOURTH.md")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn(str(foreign), p.stderr)
        self.assertIn("--prefix", p.stderr)
        self.assertEqual(len(self.mutations()), before, "refused before anything is created")
        self.assertIn("another program", foreign.read_text())

    # ---- U22 ---------------------------------------------------------------------------------------------
    def test_b812_a_new_client_that_cannot_run_never_replaces_the_working_one(self) -> None:
        self.deployed()
        working = self.installed().read_bytes()
        self.commit_more()
        broken = self.tmp / "broken-client"
        broken.write_text(BROKEN_CLIENT)
        self.set_scenario(client_file=str(broken))
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertEqual(self.installed().read_bytes(), working, "the client that runs stays in place")
        self.assertFalse(self.runner_installed(), "the runner is removed on this exit too")
        flat = " ".join(p.stderr.split())
        self.assertIn(str(self.installed()), flat)
        self.assertIn("stays", flat)
        # a new client that runs replaces it, and the earlier one is kept beside it
        self.set_scenario(client_file=str(self.client_file))
        self.client_file.write_text(STUB_HAM + "# runs\n")
        self.commit_more("AGAIN.md")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.installed().read_text().endswith("# runs\n"))
        self.assertEqual(Path(str(self.installed()) + ".prev").read_bytes(), working)
        # an Intel Mac, and a terminal under Rosetta: refused before anything is created, with the way that works
        for arch, translated in (("Intel", "0"), ("Rosetta", "1")):
            with self.subTest(mac=arch):
                self.write_stub("uname", "#!/bin/sh\ncase $1 in -s) echo Darwin;; -m) echo x86_64;; *) echo host;; esac\n")
                self.write_stub("sysctl", f"#!/bin/sh\necho {translated}\n")
                before = len(self.mutations())
                p = self.run_full(*self.UPDATE)
                self.assertEqual(p.returncode, 1, self.out(p))
                self.assertEqual(len(self.mutations()), before)
                self.assertIn("--build-on local", p.stderr)
                self.assertIn("sysctl.proc_translated", p.stderr)
                if arch == "Rosetta":
                    self.assertIn("Rosetta", p.stderr)

    # ---- U23 ---------------------------------------------------------------------------------------------
    def test_b812_local_update_keeps_records_in_the_receipt_folder_and_names_update_on_failure(self) -> None:
        self.stage()
        self.local_checkout()
        self.ham_on_path()
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        got = self.deploy_env()[-1]
        for var in ("DEPLOY_RECORD_DIR", "DEPLOY_CRITERIA"):
            with self.subTest(var=var):
                self.assertIn(var, got)
                self.assertTrue(got[var].startswith(str(self.receipt_dir())), got[var])
        p = self.run_local(FAKE_DEPLOY="fail")
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("./setup.sh --update --build-on local", p.stderr)
        self.assertNotIn("--full", p.stderr)
        for mode, code in (("guard", 7), ("refused", 5), ("refused-wait", 7), ("before-change", 5)):
            with self.subTest(mode=mode):
                p = self.run_local(FAKE_DEPLOY=mode)
                self.assertEqual(p.returncode, code, self.out(p))
                self.assertIn("The stage is unchanged.", p.stderr)
                self.assertNotIn("partial", p.stderr)
        self.assertIn("export HAMH_OWNER=keepme", self.run_local(FAKE_DEPLOY="refused").stderr, "the deploy's action is named")
        self.assertIn("run npm ci in infra", self.run_local(FAKE_DEPLOY="before-change").stderr, "the step that failed and its action are named")

    # ---- U25 ---------------------------------------------------------------------------------------------
    def test_b812_an_older_commit_than_the_deployed_one_needs_an_explicit_revision(self) -> None:
        self.with_registry()
        old = self.deployed()
        new = self.commit_more()
        self.seed_record(commit=new, version="5.20")
        # a descendant: the plan names the change
        newer = self.commit_more("NEWER.md")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f"commit: {new[:12]} (VERSION 5.20) -> {newer[:12]}", self.plan(p))
        # a stale clone: refused before anything is created, naming git pull and both commits
        self.git("checkout", "-q", old)
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        for needle in ("git pull", new[:12], old[:12], "5.20", "--revision"):
            self.assertIn(needle, p.stderr)
        # the recorded commit is not in this clone: git cannot tell, so the VERSIONs decide. This clone has no VERSION file, so the
        # answer is the stop, and its way forward is the one that works for a commit no pull can bring: a new clone
        self.git("checkout", "-q", newer)
        self.seed_record(commit="f" * 40, version="5.30")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("new clone of the repository", self.flat(p))
        self.assertNotIn("git pull", p.stderr)
        # --revision says the older commit is meant
        self.seed_record(commit=new, version="5.20")
        p = self.run_full(*self.UPDATE, "--revision", old)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.start_build_env(self.count_calls("codebuild", "start-build") - 1)["SETUP_SOURCE_KEY"], f"source/{old}.zip")

    # ---- U26 ---------------------------------------------------------------------------------------------
    def test_b812_revision_with_build_on_local_deploys_that_revision_or_is_refused(self) -> None:
        self.stage()
        self.local_checkout()
        self.ham_on_path()
        older = self.commit()
        head = self.commit_more()
        p = self.run_local("--revision", older)
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn(f"git checkout {older}", p.stderr)
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.log_lines("deploy.log"), [])
        p = self.run_local("--revision", head)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.log_lines("deploy.log")), 1)
        self.assertIn(head, p.stdout)

    # ---- U27 ---------------------------------------------------------------------------------------------
    def test_b812_early_refusals_and_confirmation_messages_name_the_right_options(self) -> None:
        self.deployed()
        first = self.commit()
        self.commit_more()
        for args in (("--update", "--profile", "work", "--region", "us-east-1"), ("--update", "--profile", "work", "--region", "us-east-1", "--no-input")):
            with self.subTest(args=args[-1]):
                p = self.run_full(*args)
                self.assertEqual(p.returncode, 4, self.out(p))
                self.assertIn("--yes", p.stderr)
                self.assertNotIn("--acknowledge-costs", p.stderr, "an update needs only --yes")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--keep-runner", "--dry-run")
        plan = self.plan(p)
        self.assertNotIn("removed afterwards", plan)
        self.assertIn("kept (--keep-runner)", plan)
        # a dirty tree stops the update also with a --revision other than HEAD (Addendum 4, A4.13): the build takes the git archive of that commit, and setup.sh, the
        # runner template and the bootstrap pin run as they are in the working tree, with the authority of the deploy
        (self.repo / "infra" / "setup-runner.yaml").write_text("edited: not the template of any commit\n")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE, "--revision", first)
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = " ".join(p.stderr.split())
        for needle in ("not committed", "infra/setup-runner.yaml", "--revision", "Nothing was changed."):
            self.assertIn(needle, flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        self.git("checkout", "--", "infra/setup-runner.yaml")
        # the options of the update go with --update only
        for flag in ("--force-deploy", "--change-operator"):
            with self.subTest(flag=flag):
                p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", flag, "--dry-run")
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn(flag, p.stderr)
                self.assertIn(flag, self.run_full("--help").stdout)
        # A2.6: the account's CDK bootstrap (version 21 in the stand-in) is older than the commit needs: refused before the question, naming the upgrade
        self.write_commit({"scripts/lib/bootstrap-min-version.txt": "30\n"}, "needs bootstrap 30")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        flat = " ".join(p.stderr.split())
        for needle in ("21", "30", "cdk bootstrap", "Nothing was changed."):
            self.assertIn(needle, flat)
        self.write_commit({"scripts/lib/bootstrap-min-version.txt": "21\n"}, "needs bootstrap 21")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        # the broken stage of an update names the way out a full setup names (U14, the message)
        w = self.open_world()
        w.w["stacks"]["hamh-direct-network"]["status"] = "DELETE_FAILED"
        w.save()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("console", p.stderr)
        self.assertIn("./setup.sh --teardown", p.stderr)

    # ---- U31 ---------------------------------------------------------------------------------------------
    def test_b812_a_revision_with_another_runner_contract_is_refused_before_anything_is_created(self) -> None:
        self.deployed()
        names = self.repo / "scripts" / "setup" / "names.sh"
        text = names.read_text()
        self.assertRegex(text, r"(?m)^HAMH_RUNNER_CONTRACT=\d+$", "names.sh carries the runner contract number")
        other = self.write_commit({"scripts/setup/names.sh": text.replace("HAMH_RUNNER_CONTRACT=", "HAMH_RUNNER_CONTRACT=9", 1)}, "another runner contract")
        without = self.write_commit({"scripts/setup/names.sh": "\n".join(ln for ln in text.splitlines() if not ln.startswith("HAMH_RUNNER_CONTRACT=")) + "\n"}, "no contract")
        self.write_commit({"scripts/setup/names.sh": text}, "back to this contract")
        for rev in (other, without):
            with self.subTest(rev=rev[:12]):
                before = len(self.mutations())
                p = self.run_full(*self.UPDATE, "--revision", rev)
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn(f"git checkout {rev}", p.stderr)
                self.assertEqual(len(self.mutations()), before)

    # ---- U32 ---------------------------------------------------------------------------------------------
    def test_b812_an_update_of_another_stage_says_it_repoints_the_client_before_the_confirmation(self) -> None:
        self.stage()
        cfg = self.config_file()
        cfg.parent.mkdir(parents=True, exist_ok=True)
        cfg.write_text(f'apiEndpoint = "{OLD_ENDPOINT}"\nregion = "us-west-2"\n')
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1")  # no terminal: the plan is shown and the run stops at the question
        self.assertEqual(p.returncode, 4, self.out(p))
        self.assertIn(f"client config: api_endpoint {OLD_ENDPOINT} -> {self.ENDPOINT}, region us-west-2 -> us-east-1", self.plan(p))
        cfg.write_text(f'apiEndpoint = "{self.ENDPOINT}"\nregion = "us-east-1"\n')
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertNotIn("->", " ".join(ln for ln in p.stdout.splitlines() if "client config:" in ln))

    # ---- U37 ---------------------------------------------------------------------------------------------
    def test_b812_local_update_checks_python_and_the_docker_daemon_before_the_confirmation(self) -> None:
        self.stage()
        self.local_checkout()
        self.ham_on_path()
        for missing, stub, needle in (("python", ("python3", "#!/bin/sh\nexit 1\n"), "Python 3.11"), ("docker daemon", ("docker", "#!/bin/sh\n[ \"$1\" = info ] && exit 1\nexit 0\n"), "docker info")):
            with self.subTest(missing=missing):
                flow.FullLocalBuildCase.tools_on_path(self, *flow.FullLocalBuildCase.TOOLS)
                self.write_stub(*stub)
                p = self.run_local()
                self.assertEqual(p.returncode, 1, self.out(p))
                self.assertIn(needle, p.stderr)
                self.assertEqual(self.calls(), [], "no AWS call, nothing created")
                self.assertEqual(self.log_lines("deploy.log"), [])

    # ---- U38 ---------------------------------------------------------------------------------------------
    def test_b812_allow_active_runs_names_what_the_update_changes_under_running_runs(self) -> None:
        self.stage(nine_stacks=True, executions={"hamh-direct-run": RUNS})
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--allow-active-runs", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        for run in RUNS:
            self.assertIn(run, plan)
        self.assertIn("2 running run(s)", plan)
        self.assertIn("the Lambda functions and the run state machine change under them", plan)

    # ---- U40 ---------------------------------------------------------------------------------------------
    def test_b812_update_keeps_edited_rules(self) -> None:
        self.deployed()
        base = self.home / ".config" / "ham"
        mine = {"rules/00-my-rules.md": "my own rules, edited\n", "rules/50-team.md": "team rules\n", "skills/review/SKILL.md": "my skill\n",
                "continue.md": "keep going my way\n", "review.md": "review my way\n"}
        for rel, text in mine.items():
            (base / rel).parent.mkdir(parents=True, exist_ok=True)
            (base / rel).write_text(text)
        self.commit_more()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("rules init", self.log_lines())
        for rel, text in mine.items():
            with self.subTest(file=rel):
                self.assertEqual((base / rel).read_text(), text)

    # ---- U41 ---------------------------------------------------------------------------------------------
    def test_b812_the_plan_says_whether_the_update_rebuilds_the_image_or_the_fork(self) -> None:
        pin = "# image key pin\n{}\n"
        lock = '[cline]\ntag = "cli-v3.0.71"\nbinarySha256 = "{}"\n\n[opencode]\nbinarySha256 = "{}"\n'
        self.write_commit({"engine/policy/image-key.pin": pin.format("a" * 64), "engine/lock.toml": lock.format("b" * 64, "c" * 64)}, "pins")
        deployed = self.deployed()
        self.seed_record(commit=deployed)
        cases = (("no new image", {"docs.md": "words\n"}, "no new worker image"),
                 ("new image", {"engine/policy/image-key.pin": pin.format("d" * 64)}, "a new worker image"),
                 ("fork rebuild", {"engine/lock.toml": lock.format("e" * 64, "c" * 64)}, "the Cline binary is rebuilt"))
        for name, files, needle in cases:
            with self.subTest(case=name):
                self.git("checkout", "-q", deployed)
                self.write_commit(files, name)
                p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertIn(needle, self.plan(p))
                self.assertNotIn("plus about 21 to 27 minutes when the binary must be rebuilt", self.plan(p), "the plan knows which case applies")
        # with --allow-active-runs, a new image is refused before the build: the build's guard would refuse it after the long Lambda build
        self.git("checkout", "-q", deployed)
        self.write_commit({"engine/policy/image-key.pin": pin.format("f" * 64)}, "another image")
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE, "--allow-active-runs")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        self.assertIn("new worker image", p.stderr)
        # a stage that records no commit keeps the range
        w = self.open_world()
        w.w["params"].pop(SETTINGS_PARAM)
        w.save()
        self.set_scenario(executions={})
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertIn("plus about 21 to 27 minutes when the binary must be rebuilt", self.plan(p))


class A410ClientFolderCase(ScenarioBase):
    """Addendum 4, A4.10 (F12): a folder the client cannot be written to is found before the confirmation, with exit 1 and --prefix named, and never after the deploy."""

    def read_only(self, path: Path) -> Path:
        path.mkdir(parents=True, exist_ok=True)
        path.chmod(0o555)
        self.addCleanup(path.chmod, 0o755)
        return path

    @unittest.skipIf(os.geteuid() == 0, "the root user can write every folder")
    def test_b812_fix_a_client_folder_that_cannot_be_written_stops_the_update_before_anything_is_created(self) -> None:
        self.deployed()
        self.commit_more()
        cases = (("--prefix names a folder whose bin is read-only", ("--prefix", str(self.read_only(self.home / "ro" / "bin").parent))),
                 ("--prefix names a folder that cannot be created", ("--prefix", str(self.read_only(self.home / "locked") / "deeper" / "prefix"))))
        for name, extra in cases:
            with self.subTest(case=name):
                before, builds = len(self.mutations()), self.count_calls("codebuild", "start-build")
                p = self.run_full(*self.UPDATE, *extra)
                self.assertEqual(p.returncode, 1, self.out(p))
                flat = " ".join(p.stderr.split())
                for needle in ("--prefix", "Nothing was created."):
                    self.assertIn(needle, flat)
                self.assertEqual(len(self.mutations()), before, "nothing is created")
                self.assertEqual(self.count_calls("codebuild", "start-build"), builds)
        # the folder the last setup installed the client in (the receipt) has become read-only: the same stop, before the long deploy
        self.read_only(self.prefix / "bin")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn(str(self.prefix / "bin"), p.stderr)
        self.assertIn("--prefix", p.stderr)
        self.assertEqual(len(self.mutations()), before)
        # a folder that does not exist yet and can be created is fine
        fresh = self.home / "fresh" / "prefix"
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", "--prefix", str(fresh))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(fresh.exists(), "a dry run creates nothing")


class A48CanonicalCase(ScenarioBase):
    """Addendum 4, A4.8 (F11, F24): setup uses the one form of a value that the deploy writes into the record (trimmed, case-free words in lower case, numbers without
    leading zeros, lists without spaces), and an exported value of a setting the stage does not keep always counts as a change."""

    SETUP_DIR = REAL_REGISTRY.parents[1] / "setup"

    def release_stage(self, **sets: str) -> str:
        """A stage that a full setup of this release deployed (the real registry is in the commit, so the build wrote the record), with these `set.` lines in it."""
        self.set_scenario(nine_stacks=True)
        self.with_registry(REAL_REGISTRY.read_text())
        head = self.deployed()
        w = self.open_world()
        self.assertIn(SETTINGS_PARAM, w.w["params"], "TRIGGER PRESENT: the deploy wrote the stage's record")
        w.w["params"][SETTINGS_PARAM] += "".join(f"\nset.{k}={v}" for k, v in sets.items())
        w.save()
        return head

    def last_start(self) -> dict:
        return self.start_build_env(self.count_calls("codebuild", "start-build") - 1)

    def test_b812_fix_an_exported_value_in_another_spelling_is_the_recorded_value(self) -> None:
        self.release_stage(HAMH_TOOLS_MODE="on", HAMH_TAVILY_KEY_MAX_AGE_DAYS="30")
        deploys = self.world().get("n_deploys", 0)
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.UPDATE, HAMH_TOOLS_MODE=" ON ", HAMH_TAVILY_KEY_MAX_AGE_DAYS="030")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("HAMH_TOOLS_MODE=on (explicit", plan)
        self.assertIn("HAMH_TAVILY_KEY_MAX_AGE_DAYS=30 (explicit", plan)
        env = self.last_start()
        self.assertEqual(env.get("SETUP_CLIENT_ONLY"), "1", "the values are the recorded ones, so nothing is deployed and the running runs do not stop the update")
        self.assertEqual(base64.b64decode(env["SETUP_EXPLICIT_SETTINGS"]).decode(), "HAMH_TOOLS_MODE=on\nHAMH_TAVILY_KEY_MAX_AGE_DAYS=30\n", "the build gets the one form")
        self.assertEqual(self.world().get("n_deploys", 0), deploys)
        # a value that is another value deploys, and the runs stop it before anything is created
        q = self.run_full(*self.UPDATE, HAMH_TOOLS_MODE="OFF")
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn("running run(s)", self.flat(q))

    def test_b812_fix_an_exported_setting_the_stage_does_not_keep_always_deploys(self) -> None:
        self.release_stage()
        for value in ("default", "0600"):
            with self.subTest(value=value):
                deploys = self.world().get("n_deploys", 0)
                p = self.run_full(*self.UPDATE, HAMH_SLICE_DEADLINE_S=value)
                self.assertEqual(p.returncode, 0, self.out(p))
                env = self.last_start()
                self.assertNotIn("SETUP_CLIENT_ONLY", env, "the deploy applies or removes the override; a client-only build would leave the stage as it is")
                self.assertEqual(base64.b64decode(env["SETUP_EXPLICIT_SETTINGS"]).decode(), f"HAMH_SLICE_DEADLINE_S={value.lstrip('0')}\n")
                self.assertEqual(self.world().get("n_deploys", 0), deploys + 1)
                self.assertNotIn("the stage already runs this commit", p.stdout)

    def test_b812_fix_the_canonical_form_is_the_one_of_the_deploy(self) -> None:
        sys.path.insert(0, str(REAL_REGISTRY.parent))
        try:
            import deploy  # noqa: PLC0415  the deploy engine's canonical() and its three lists are the reference
        finally:
            sys.path.pop(0)
        text = (self.SETUP_DIR / "preflight.sh").read_text()
        for var, want in (("PF_NUMBER_SETTINGS", deploy.NUMBER_SETTINGS), ("PF_LOWER_SETTINGS", deploy.LOWER_SETTINGS), ("PF_LIST_SETTINGS", deploy.LIST_SETTINGS)):
            with self.subTest(list=var):
                import re
                m = re.search(rf'(?m)^{var}="([^"]+)"$', text)
                self.assertIsNotNone(m, f"preflight.sh defines {var}")
                self.assertEqual(sorted(m.group(1).split()), sorted(want))
        cases = [("HAMH_TOOLS_MODE", " ON\t"), ("HAMH_TOOLS_MODE", "off"), ("HAMH_COMPUTE_INSTANCES", "On"), ("HAMH_LOG_RETENTION_DAYS", " 365 "), ("HAMH_TAVILY_KEY_MAX_AGE_DAYS", "007"),
                 ("HAMH_SLICE_DEADLINE_S", "000600"), ("HAMH_PR_CHECK_COMPUTE", "2XLarge"), ("HAMH_UTILITY_MODEL", "Haiku"), ("HAMH_UTILITY_PURPOSES", "Run-Name , run-TAGS"),
                 ("HAMH_INSTANCES_TYPES", "m7g.4xlarge , c7g.large"), ("HAMH_AGENTCORE_STORAGE_ACCOUNTS", f", us-east-1={ACCOUNT} ,, us-west-2={ACCOUNT}+999988887777 ,"),
                 ("HAMH_RESEARCH_PROVIDER", "tavily"), ("HAMH_RESEARCH_ALARM_TOPIC_ARN", f"arn:aws:sns:us-east-1:{ACCOUNT}:Alarms"), ("HAMH_STAGE_KIND", "TEST"), ("HAMH_KEEP_TAG", "oFF")]
        driver = '. "$1/lib.sh"; . "$1/preflight.sh"; pf_canonical "$2" "$(pf_trim "$3")"'
        for name, value in cases:
            with self.subTest(setting=name, value=value):
                p = subprocess.run(["bash", "-c", driver, "driver", str(self.SETUP_DIR), name, value], capture_output=True, text=True, timeout=limit(60))  # nosec B603: bash with an argument list, the value is an argument and never part of the script; B607: bash looked up on PATH, as the scripts under test use it
                self.assertEqual((p.returncode, p.stdout), (0, deploy.canonical(name, value.strip(deploy.TOOLS_SPACE))), p.stderr)


class A46LiveOperatorCase(ScenarioBase):
    """Addendum 4, A4.6 (F2, F9): a stage without a record, or whose record names no operator, still says whom it trusts: the control function of the deployed stage names
    the operator, and an update by another identity needs --change-operator as it does for a recorded operator."""

    DRY = ("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")

    def template_reads(self, since: int = 0) -> list:
        return [c for c in self.normalized_calls()[since:] if c[:2] == ["cloudformation", "get-template"]]

    def admin_stage(self) -> None:
        """A stage that a release before the record deployed from an account entry without an operator: it trusts the sessions of the IAM role Admin."""
        self.stage(operator=["role", "Admin"])
        self.assertIsNone(self.open_world().stage_record(), "TRIGGER PRESENT: the stage has no record")

    def test_b812_fix_an_update_of_a_stage_without_a_record_by_another_identity_needs_change_operator(self) -> None:
        self.admin_stage()
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)  # the caller is the IAM user setup-test
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = " ".join(p.stderr.split())
        for needle in ("role Admin", "user setup-test", "--change-operator", "Nothing was changed."):
            self.assertIn(needle, flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        reads = self.template_reads()
        self.assertEqual(len(reads), 1, "one read of the deployed control template")
        self.assertIn("hamh-direct-control", reads[0])
        self.assertIn("TemplateBody.Resources.ControlApi.Properties.Environment.Variables.OPERATOR_PRINCIPALS", reads[0])
        # with --change-operator the plan names the change, and the stage trusts the caller afterwards
        p = self.run_full(*self.UPDATE, "--change-operator")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("operator: role Admin -> user setup-test", self.plan(p))
        self.assertEqual(self.generated_accounts()["accounts"][0]["operator"], {"kind": "user", "name": "setup-test"})

    def test_b812_fix_the_operator_the_stage_trusts_updates_it_without_a_question(self) -> None:
        self.admin_stage()
        self.set_scenario(arn=f"arn:aws:sts::{ACCOUNT}:assumed-role/Admin/someone")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        line = " ".join(ln for ln in p.stdout.splitlines() if "operator:" in ln)
        self.assertIn("operator: role Admin (kept: the operator the stage's control function trusts", " ".join(line.split()))
        self.assertNotIn("->", line)
        # a stage whose operator is an IAM user (the two patterns of a user, without and with a path)
        self.reset_world()
        self.stage(operator=["user", "setup-test"])
        self.set_scenario(arn=f"arn:aws:iam::{ACCOUNT}:user/setup-test")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("operator: user setup-test (kept: the operator the stage's control function trusts", self.plan(p))

    def test_b812_fix_a_record_without_an_operator_is_completed_from_the_stage(self) -> None:
        # a deploy from an account entry without an operator writes an empty operator= line: the stage trusts the IAM role Admin
        self.admin_stage()
        self.with_registry()
        w = self.open_world()
        w.w.setdefault("params", {})[SETTINGS_PARAM] = "\n".join(["schema=1", "status=deployed", f"commit={self.commit()}", "tree=" + "1" * 64, "version=", f"accounts={DIGEST}",
                                                               "operator=", "azs=use1-az1", "updated=2026-11-10T16:00:00Z"])
        w.save()
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("role Admin", " ".join(p.stderr.split()))
        self.assertEqual(len(self.mutations()), before)
        # a record that names its operator is the answer, and the template is not read
        self.seed_record(operator=f"arn:aws:iam::{ACCOUNT}:user/setup-test")
        n = len(self.normalized_calls())
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.template_reads(n), [])

    def test_b812_fix_a_stage_that_names_no_operator_takes_the_caller_and_the_plan_says_so(self) -> None:
        for name, scenario in (("the template cannot be read", dict(faults=[{"match": "cloudformation get-template", "exit": 254,
                                                                           "stderr": "An error occurred (AccessDenied) when calling the GetTemplate operation: not authorized to perform: cloudformation:GetTemplate"}])),
                               ("the template names no operator", dict(control_template="no-operator"))):
            with self.subTest(case=name):
                self.reset_world()
                self.scenario = {"client_file": str(self.client_file), "admin_file": str(self.admin_file)}
                self.stage(operator=["role", "Admin"], **scenario)
                p = self.run_full(*self.DRY)
                self.assertEqual(p.returncode, 0, self.out(p))
                plan = self.plan(p)
                self.assertIn("operator: user setup-test (your credentials:", plan)
                self.assertIn("from now on the stage trusts this identity", plan)


class A45EarlierSpellingCase(ScenarioBase):
    """Addendum 4, A4.5 (F8): the earlier spelling of every stage setting counts as the setting: it is checked, shown in the plan and carried to the deploy on both routes,
    and two spellings with different values end the run before anything is created."""

    OLD = "CAH_LOG_RETENTION_DAYS"  # COMPAT-CAH: the earlier spelling of a setting that names.sh never folded
    DRY = ("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")

    def test_b812_fix_the_earlier_spelling_of_a_stage_setting_is_checked_shown_and_carried_to_the_build(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        self.seed_record(HAMH_LOG_RETENTION_DAYS="365")
        p = self.run_full(*self.DRY, **{self.OLD: "30"})
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("HAMH_LOG_RETENTION_DAYS: 365 -> 30 (explicit", self.plan(p), "the plan shows what the deploy would get")
        self.assertIn(f"{self.OLD} is the previous name of HAMH_LOG_RETENTION_DAYS", p.stderr)
        p = self.run_full(*self.UPDATE, **{self.OLD: "30"})
        self.assertEqual(p.returncode, 0, self.out(p))
        env = self.start_build_env(self.count_calls("codebuild", "start-build") - 1)
        self.assertEqual(base64.b64decode(env.get("SETUP_EXPLICIT_SETTINGS", "")).decode(), "HAMH_LOG_RETENTION_DAYS=30\n", "the runner route carries it")
        # a value the setting does not take is refused under the earlier spelling as under the current one
        self.commit_more("MORE.md")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE, **{self.OLD: "soon"})
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("HAMH_LOG_RETENTION_DAYS=soon is not a value this setting takes", " ".join(p.stderr.split()))
        # both spellings with different values: setup never chooses between them
        for old, new, values in ((self.OLD, "HAMH_LOG_RETENTION_DAYS", ("30", "365")), ("CAH_TOOLS_MODE", "HAMH_TOOLS_MODE", ("on", "off"))):  # COMPAT-CAH
            with self.subTest(setting=new):
                p = self.run_full(*self.UPDATE, **{old: values[0], new: values[1]})
                self.assertEqual(p.returncode, 2, self.out(p))
                flat = " ".join(p.stderr.split())
                for word in (f"{old}={values[0]}", f"{new}={values[1]}", "differ", "Nothing was changed."):
                    self.assertIn(word, flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        # the same value under both names is one setting
        p = self.run_full(*self.DRY, **{self.OLD: "30", "HAMH_LOG_RETENTION_DAYS": "30"})
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("HAMH_LOG_RETENTION_DAYS=30 (explicit", self.plan(p))

    def test_b812_fix_the_local_deploy_gets_the_setting_under_its_current_name_only(self) -> None:
        self.stage()
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()
        p = self.run_local(**{self.OLD: "30"})
        self.assertEqual(p.returncode, 0, self.out(p))
        got = self.deploy_env()[-1]
        self.assertEqual(got.get("HAMH_LOG_RETENTION_DAYS"), "30", "the deploy of the local route gets the same explicit value as the runner's build")
        self.assertNotIn(self.OLD, got, "and no second spelling of it, which the deploy would read by itself and the plan would not show")
        self.assertIn("HAMH_LOG_RETENTION_DAYS=30 (explicit", self.plan(p))


class PlanChangeClassCase(ScenarioBase):
    """The read-only check of the two real stages (2026-11-11): an exported lower HAMH_LOG_RETENTION_DAYS showed in the plan of a stage without a record only as
    `(explicit: exported in this shell)`, with nothing about the events it deletes. The plan could name the class of a change only when the stage's record held another value. It
    now says what a value of a classed setting can cost whenever the record cannot show the change: a stage without a record, and a record with no line for the setting."""

    DECREASE = "a lower number than the stage has now deletes what is older, and that cannot be undone"
    REMOVE = "an off or empty value removes what the setting holds"
    REPLACE = "a value other than the stage has now replaces what the setting describes"

    def dry(self, **env: str):
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", **env)
        self.assertEqual(p.returncode, 0, self.out(p))
        return self.plan(p)

    def test_b812_the_plan_says_what_an_exported_classed_setting_can_cost_when_the_record_cannot_show_the_change(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        self.drop_record()
        # a stage without a record: the deploy reads the live value, the plan cannot, so the plan names the class
        plan = self.dry(HAMH_LOG_RETENTION_DAYS="7", HAMH_TAVILY_ALARMS="off", HAMH_INSTANCES_TYPES="m7g.large", HAMH_TOOLS_MODE="on")
        self.assertIn("HAMH_LOG_RETENTION_DAYS=7 (explicit: exported in this shell; " + self.DECREASE + ")", plan)
        self.assertIn("HAMH_TAVILY_ALARMS=off (explicit: exported in this shell; " + self.REMOVE + ")", plan)
        self.assertIn("HAMH_INSTANCES_TYPES=m7g.large (explicit: exported in this shell; " + self.REPLACE + ")", plan)
        self.assertIn("HAMH_TOOLS_MODE=on (explicit: exported in this shell)", plan, "a setting whose change costs nothing gets no warning")
        # a record with no line for the setting (the stage runs the default): the same
        self.seed_record(HAMH_TOOLS_MODE="on")
        plan = self.dry(HAMH_LOG_RETENTION_DAYS="7")
        self.assertIn("HAMH_LOG_RETENTION_DAYS=7 (explicit: exported in this shell; " + self.DECREASE + ")", plan)
        # a record that holds another value shows the change itself, as before
        self.seed_record(HAMH_LOG_RETENTION_DAYS="365")
        plan = self.dry(HAMH_LOG_RETENTION_DAYS="7")
        self.assertIn("HAMH_LOG_RETENTION_DAYS: 365 -> 7 (explicit", plan)
        self.assertIn("change class decrease", plan)
        # the value the record already holds changes nothing, so nothing is warned about
        plan = self.dry(HAMH_LOG_RETENTION_DAYS="365")
        self.assertIn("HAMH_LOG_RETENTION_DAYS=365 (explicit: exported in this shell)", plan)


def setup_words(name: str) -> list:
    """The words of a variable that scripts/setup/preflight.sh sets on one line (NAME="a b c")."""
    text = (Path(__file__).resolve().parents[2] / "scripts" / "setup" / "preflight.sh").read_text()
    for ln in text.splitlines():
        if ln.startswith(name + "="):
            return ln.split("=", 1)[1].strip('"').split()
    raise AssertionError(f"{name} is not set in scripts/setup/preflight.sh")


def fake_explicit(env: dict) -> dict:
    return setup_fake_aws.explicit_settings(env)


class B833DefaultChangeCase(ScenarioBase):
    """Release 5.15 changed the default of HAMH_INSTANCES_TYPES from m7g.4xlarge to m7g.4xlarge,m6g.4xlarge. The update
    of a 5.14 stage with the Instances part whose record had no line for the types printed a plan that said nothing, installed the runner, built, and was refused for consent.
    The deploy now keeps the stage's machine (test_setup_deploy_settings.B833DefaultChangeCase); the plan names that before anything is installed, in the words of the
    deploy's line, with the export that takes the new default. The checkout carries the real registry and the real defaults file of this release."""

    OLD = "m7g.4xlarge"
    RUNTIMES_READ = ["cloudformation", "get-template", "--stack-name", "hamh-direct-runtimes"]

    def deploy_module(self):
        lib = str(Path(__file__).resolve().parents[2] / "scripts" / "lib")
        if lib not in sys.path:
            sys.path.insert(0, lib)
        import deploy  # noqa: PLC0415
        return deploy

    def line(self, name: str = "HAMH_INSTANCES_TYPES", kept: str = OLD, change: str = "replace") -> str:
        d = self.deploy_module()
        return f"setting: {name}={kept} ({d.held_sentence(name, kept, d.SETTING_DEFAULTS[name], change)})"

    def stage_on_the_old_default(self, with_part: bool = True) -> None:
        """A stage as 5.14 left it: deployed, a record with version 5.14 that has no line for the machine, and (with_part) the Instances part on the machine of 5.14."""
        self.with_registry(REAL_REGISTRY.read_text())
        self.write_commit({"scripts/lib/stage-defaults.tsv": (REAL_REGISTRY.parent / "stage-defaults.tsv").read_text()}, "the defaults")
        self.set_scenario(nine_stacks=True)
        self.deployed()
        if with_part:
            self.seed_instances()
            w = self.open_world()
            w.w["instances_shape"] = {"HAMH_INSTANCES_TYPES": self.OLD}
            w.save()
        self.seed_record(version="5.14", HAMH_KEEP_TAG="off", HAMH_STAGE_KIND="official", **({"HAMH_COMPUTE_INSTANCES": "on"} if with_part else {}))
        self.commit_more()

    def runtimes_reads(self) -> int:
        return len([c for c in self.normalized_calls() if c[:4] == self.RUNTIMES_READ])

    def test_b833_a_new_default_that_would_replace_resources_is_kept_and_named_in_the_plan(self) -> None:
        self.stage_on_the_old_default()
        self.assertNotEqual(self.deploy_module().SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"], self.OLD, "TRIGGER PRESENT: this release's default is another machine list")
        # the dry run shows it, and only reads
        before = len(self.mutations())
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(self.line(), self.plan(p))
        self.assertEqual(len(self.mutations()), before)
        # the plain update: the line comes before anything is installed, and the update goes through
        builds = self.count_calls("codebuild", "start-build")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn(self.line(), plan)
        self.assertLess(plan.index(self.line()), plan.index("Installing the runner stack"), "the plan says it before the runner is installed")
        self.assertEqual(self.runtimes_reads(), 2, "one read of the runtimes template for each run")
        self.assertEqual(fake_explicit(self.start_build_env(builds)), {}, "setup sends nothing you did not export: the deploy keeps the machine itself")
        # the record the deploy left holds the machine as the stage's own value, and the next update says it is kept by the record
        self.assertEqual(self.open_world().stage_record()["set"].get("HAMH_INSTANCES_TYPES"), self.OLD)
        self.commit_more("MORE.md")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn(f"HAMH_INSTANCES_TYPES={self.OLD} (kept: the stage's record)", plan)
        self.assertNotIn("this release's default is", plan)

    def test_b833_the_plan_names_no_kept_default_where_the_deploy_takes_the_value(self) -> None:
        self.stage_on_the_old_default()
        # an exported value is the consent: it is shown as yours, and no kept line
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_INSTANCES_TYPES="default")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("HAMH_INSTANCES_TYPES=default (explicit: exported in this shell", plan)
        self.assertNotIn("this release's default is", plan)
        # a record that names the machine keeps it as before
        self.seed_record(version="5.14", HAMH_COMPUTE_INSTANCES="on", HAMH_INSTANCES_TYPES=self.OLD)
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertIn(f"HAMH_INSTANCES_TYPES={self.OLD} (kept: the stage's record)", self.plan(p))
        self.assertNotIn("this release's default is", self.plan(p))
        # a stage that already runs the default has nothing to keep
        self.seed_record(version="5.14", HAMH_COMPUTE_INSTANCES="on")
        w = self.open_world()
        w.w["instances_shape"] = {}
        w.save()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertNotIn("this release's default is", self.plan(p))

    def test_b833_a_stage_without_the_instances_part_reads_nothing_more(self) -> None:
        self.stage_on_the_old_default(with_part=False)
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("this release's default is", self.plan(p))
        self.assertEqual(self.runtimes_reads(), 0, "the machine settings mean nothing without the part")

    def test_b833_a_template_that_cannot_be_read_is_said_and_stops_nothing(self) -> None:
        self.stage_on_the_old_default()
        self.set_scenario(faults=[{"match": "cloudformation get-template --stack-name hamh-direct-runtimes", "exit": 254,
                                   "stderr": "An error occurred (AccessDenied) when calling the GetTemplate operation: not authorized"}])
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("HAMH_INSTANCES_TYPES, HAMH_INSTANCES_ROOT_FREE_GIB, HAMH_INSTANCES_WORKSPACE_GIB, HAMH_INSTANCES_LIFETIME_S, HAMH_INSTANCES_IDLE_S: the plan could not "
                      "read what the stage runs (the deployed template of the stack hamh-direct-runtimes could not be read: An error occurred (AccessDenied)", plan)
        self.assertIn("the deploy reads it, keeps the stage's value where this release's default would need your consent, and says so", plan)
        self.assertEqual(self.runtimes_reads(), 1, "one read, whatever the number of settings")

    def retry_record(self, explicit: str | None = None, **sets: str) -> str:
        """The record as an update that did not finish left it: status deploying, the `set.` lines given, and (explicit not None) the `explicit=` line of this
        release. A record without that line is one that 5.14 or 5.15 wrote."""
        w = self.open_world()
        rec = w.w["params"][SETTINGS_PARAM].replace("status=deployed", "status=deploying")
        rec += "".join(f"\nset.{k}={v}" for k, v in sets.items())
        if explicit is not None:
            rec += f"\nexplicit={explicit}"
        w.w["params"][SETTINGS_PARAM] = rec
        w.save()
        return rec

    def old_default_again(self) -> None:
        """The stage of stage_on_the_old_default once more, after a run of the same test changed it: the record of 5.14 and the machine of 5.14."""
        self.seed_record(version="5.14", HAMH_KEEP_TAG="off", HAMH_STAGE_KIND="official", HAMH_COMPUTE_INSTANCES="on")
        w = self.open_world()
        w.w["instances_shape"] = {"HAMH_INSTANCES_TYPES": self.OLD}
        w.save()

    def takes_line(self, given: str, target: str) -> str:
        return (f"setting: HAMH_INSTANCES_TYPES: {self.OLD} -> {target} (an update of the stage that did not finish was given HAMH_INSTANCES_TYPES={given}, and "
                "this update carries it on; it replaces the machines of the Instances part)")

    def test_b833_the_plan_of_a_retry_carries_on_what_the_unfinished_update_was_given(self) -> None:
        """Review finding 2: an update of this release exported HAMH_INSTANCES_TYPES=default and failed before the runtimes stack. Its record: status deploying, no
        line for the types, explicit=HAMH_INSTANCES_TYPES; the provider still allows m7g.4xlarge. The retry's deploy takes the default and replaces the provider
        (test_setup_deploy_settings.B833DefaultChangeCase), so the plan says so before anything is installed, and the stand-in's build does the same."""
        self.stage_on_the_old_default()
        default = self.deploy_module().SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"]
        self.retry_record(explicit="HAMH_INSTANCES_TYPES")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertNotIn("this release's default is", plan, "the stage does not keep its machine")
        self.assertIn(self.takes_line("default", default), plan)
        # the update itself: the line comes before the runner install, and the build takes the default as the deploy does
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertLess(plan.index(self.takes_line("default", default)), plan.index("Installing the runner stack"))
        w = self.open_world()
        self.assertEqual(w.instances_shape()["HAMH_INSTANCES_TYPES"], default, "the stage runs the default now")
        rec = w.stage_record()
        self.assertEqual(rec["status"], "deployed")
        self.assertNotIn("HAMH_INSTANCES_TYPES", rec["set"], "the stage follows the default again")
        # a value the unfinished update was given: the plan names it once, with what it replaces
        self.old_default_again()
        self.retry_record(explicit="HAMH_INSTANCES_TYPES", HAMH_INSTANCES_TYPES="m7g.8xlarge")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn(self.takes_line("m7g.8xlarge", "m7g.8xlarge"), plan)
        self.assertNotIn("HAMH_INSTANCES_TYPES=m7g.8xlarge (kept: the stage's record)", plan)
        # a name the explicit= line does not hold is kept as on a finished record
        self.old_default_again()
        self.retry_record(explicit="HAMH_LOG_RETENTION_DAYS")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertIn(self.line(), self.plan(p))

    def test_b833_an_update_whose_record_cannot_say_what_its_unfinished_update_was_given_stops_before_anything_is_created(self) -> None:
        """The record of an update that did not finish, written by 5.14 or 5.15 (no explicit= line), with no line for the types while the stage runs m7g.4xlarge:
        it cannot say whether that update was given HAMH_INSTANCES_TYPES=default, and the deploy asks for consent before any stack change
        (test_setup_deploy_settings.B833DefaultChangeCase). Setup knows it from the plan's reads, so it stops before anything is installed, with exit 7 and the two
        exports. An export answers it. A value such a record holds is that update's choice and goes ahead, with a plan line. When the plan cannot read the stage,
        it says so and the deploy refuses after the build, with exit 5 and the stage unchanged."""
        self.stage_on_the_old_default()
        default = self.deploy_module().SETTING_DEFAULTS["HAMH_INSTANCES_TYPES"]
        self.retry_record()
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        out = " ".join(self.out(p).split())
        self.assertIn("the stage's settings record is of an update that did not finish, written by a release that does not record the settings that update was given", out)
        self.assertIn(f"HAMH_INSTANCES_TYPES goes from {self.OLD} to {default} (replace), and taking it replaces the machines of the Instances part", out)
        self.assertIn(f"export HAMH_INSTANCES_TYPES={self.OLD} to keep what the stage has now, or HAMH_INSTANCES_TYPES=default for {default}, then run setup.sh again.", out)
        self.assertIn("Nothing was changed.", out)
        self.assertNotIn("Installing the runner stack", out)
        self.assertEqual(len(self.mutations()), before, "nothing was created")
        # an export answers it
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", HAMH_INSTANCES_TYPES=self.OLD)
        self.assertEqual(p.returncode, 0, self.out(p))
        # a value the record holds is the choice of that update (the rule of the release that wrote it)
        self.old_default_again()
        self.retry_record(HAMH_INSTANCES_TYPES="m7g.8xlarge")
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(self.takes_line("m7g.8xlarge", "m7g.8xlarge"), self.plan(p))
        # the plan cannot read the stage: it says so, and the deploy refuses after the build
        self.old_default_again()
        self.retry_record()
        self.set_scenario(faults=[{"match": "cloudformation get-template --stack-name hamh-direct-runtimes", "exit": 254,
                                   "stderr": "An error occurred (AccessDenied) when calling the GetTemplate operation: not authorized"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        out = " ".join(self.out(p).split())
        self.assertIn("the stage's record is of an update that did not finish; the deploy reads the values and says on its DEPLOY-SETTING lines what it carries on, "
                      "keeps or asks your consent for", out)
        self.assertIn("The stage is unchanged.", out)
        self.assertIn(f"export HAMH_INSTANCES_TYPES={self.OLD} to keep what the stage has now, or HAMH_INSTANCES_TYPES=default for {default}", out)
        self.assertEqual(self.open_world().instances_shape()["HAMH_INSTANCES_TYPES"], self.OLD, "the machine is as it was")

    def test_b833_the_plan_says_the_words_of_the_deploy_and_pins_the_defaults_it_cannot_read(self) -> None:
        """The sentence is one text in two programs, and a setting of a consent class without a reader in setup must not get a new default unnoticed: its default is
        the one every release that writes a record (5.14 on) had, until setup can read the setting."""
        d = self.deploy_module()
        repo = Path(__file__).resolve().parents[2]
        bash = ("NL=$'\\n'; . scripts/setup/full.sh >/dev/null 2>&1 || true; "
                "full_held_sentence HAMH_LOG_RETENTION_DAYS 365 30 decrease; printf '\\n'; full_held_sentence HAMH_RESEARCH_ALARM_TOPIC_ARN arn '' remove")
        out = subprocess.run(["bash", "-c", bash], cwd=str(repo), capture_output=True, text=True, timeout=limit(30))  # nosec B603: bash with an argument list and a fixed script of this test; B607: bash looked up on PATH, as the scripts under test use it
        self.assertEqual(out.stdout.splitlines(), [d.held_sentence("HAMH_LOG_RETENTION_DAYS", "365", "30", "decrease"),
                                                   d.held_sentence("HAMH_RESEARCH_ALARM_TOPIC_ARN", "arn", "", "remove")], out.stderr)
        readers = setup_words("PF_LIVE_READERS")
        self.assertEqual(set(readers), set(d.INSTANCES_SETTINGS))
        pinned = {"HAMH_RESEARCH_ALARM_TOPIC_ARN": "", "HAMH_TAVILY_ALARMS": "on", "HAMH_LOG_RETENTION_DAYS": "30", "HAMH_AGENTCORE_STORAGE_ACCOUNTS": ""}
        unread = [r.split("|")[0] for r in REAL_REGISTRY.read_text().splitlines() if r and not r.startswith("#") and r.split("|")[1] == "yes"
                  and r.split("|")[2] in d.CONSENT_CLASSES and r.split("|")[0] not in readers and r.split("|")[0] not in d.ROLE_DEFAULTS]
        self.assertEqual(sorted(unread), sorted(pinned))
        for name in unread:
            self.assertEqual(d.SETTING_DEFAULTS[name], pinned[name], f"{name}: a new default of a setting the plan cannot read needs a reader in pf_live_get first")


class B838OldHistoryCase(ScenarioBase):
    """Rows U45 to U54: the stage records a commit that this clone does not hold (the public history was started again once, so no clone of the new
    history has the commits that a stage was deployed from before). Git cannot place such a commit, so the VERSIONs decide (Addendum 7): a recorded VERSION that is lower than
    the clone's goes ahead; the same, a higher one or one that is no N.NN stops with the way forward that works (a new clone, or --revision on purpose). The tree rule (A4.1) and
    --revision come before that rule, and a recorded commit that the clone holds is judged by git alone. The form N.NN is the one of `ham_core::update::parse_version`
    (`b804_versions_compare_as_numbers` pins its examples in Rust)."""

    SETUP_DIR = REAL_REGISTRY.parents[1] / "setup"
    MISSING = "f" * 40
    DRY = ("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")

    def stage_at(self, recorded: str, target: str, status: str = "deployed", commit: str | None = None) -> str:
        """The stage a full setup deployed, whose record then says it runs `commit` (default: a commit that no clone holds) at VERSION `recorded`; this clone gets a commit that is
        release `target` (its VERSION file, and a change to a file the stage deploys). Returns the target commit. It may run again after `reset_world` in one test."""
        if not (self.repo / "scripts" / "lib" / "stage-settings.tsv").exists():
            self.with_registry()
        self.deployed()
        self.releases = getattr(self, "releases", 0) + 1
        head = self.write_commit({"VERSION": f"{target}\n", "infra/release.yaml": f"release {target} ({self.releases})\n"}, f"release {target}")
        self.seed_record(commit=commit or self.MISSING, version=recorded, status=status)
        self.client_file.write_text(STUB_HAM.replace("ham v9.99", f"ham v{target}"))
        return head

    def stopped(self, p, target: str, before: int, *needles: str, older: bool = False) -> str:
        """The update ended with exit 7 before anything was created, with the way forward that works. A pull cannot join two histories, so it is not the advice for a commit this
        clone does not hold, except where the stop says that the clone is older than the stage (older=True: the stage runs a later release): there it is one of two ways, with a new clone."""
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        flat = self.flat(p)
        for needle in ("is not in this clone", "new clone of the repository", f"--revision {target}", "Nothing was changed.", *needles):
            self.assertIn(needle, flat)
        if older:
            self.assertIn("git pull", flat)
            self.assertNotIn("another history of the repository", flat, "the stop says that the clone is older: it does not blame another history")
        else:
            for banned in ("git pull", "Bring your clone up to date"):
                self.assertNotIn(banned, self.out(p), "a pull cannot join two histories, so it is not the advice for a commit this clone does not hold")
        return flat

    # ---- U45 ---------------------------------------------------------------------------------------------
    def test_b838_a_missing_recorded_commit_with_a_lower_version_proceeds_and_the_plan_says_why(self) -> None:
        target = self.stage_at("5.15", "5.17")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn(f"commit: {self.MISSING[:12]} (VERSION 5.15) -> {target[:12]} (VERSION 5.17)", plan)
        for needle in ("the commit the stage records is not in this clone", "its VERSION 5.15 is lower than this clone's 5.17", "image: not compared"):
            self.assertIn(needle, plan)
        self.assertNotIn("git pull", plan)
        starts = self.count_calls("codebuild", "start-build")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        env = self.start_build_env(starts)
        self.assertEqual(env["SETUP_SOURCE_KEY"], f"source/{target}.zip", "the update deploys the commit of this clone")
        self.assertNotIn("SETUP_CLIENT_ONLY", env, "a full build: the stage runs another source tree")
        self.assertEqual(self.open_world().stage_record()["commit"], target)
        self.assertFalse(self.runner_installed(), "the runner is removed as after every update")
        self.assertTrue(self.installed().is_file())

    # ---- U46 ---------------------------------------------------------------------------------------------
    def test_b838_a_missing_recorded_commit_with_the_same_version_stops_and_names_a_new_clone(self) -> None:
        target = self.stage_at("5.17", "5.17")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        flat = self.stopped(p, target, before, "(VERSION 5.17)", "its VERSION is the same as this clone's", "so setup cannot tell whether", "If this clone is old or shallow",
                            "A new clone that stops the same way means that the stage was deployed from another history of the repository")
        self.assertIn(f"commit {self.MISSING}", flat)
        # a user who already runs from a new clone (as `ham update` does) reads the way that can change the outcome first
        self.assertLess(flat.index(f"--revision {target}"), flat.index("new clone of the repository"))

    # ---- U47 ---------------------------------------------------------------------------------------------
    def test_b838_a_missing_recorded_commit_with_a_higher_version_stops_and_says_the_clone_is_older(self) -> None:
        target = self.stage_at("5.18", "5.17")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        flat = self.stopped(p, target, before, "(VERSION 5.18)", "that VERSION is higher than this clone's (VERSION 5.17)", "so the stage runs a later release than this clone",
                            "Bring this clone up to date first", older=True)
        self.assertNotIn("cannot tell whether", flat, "a clone that is older than the stage can tell that its commit is not newer")
        self.assertNotIn("so this clone is older than the stage, so", flat)
        self.assertIn("git clone <the repository URL>", flat)

    # ---- U48 ---------------------------------------------------------------------------------------------
    def test_b838_a_version_that_is_no_n_nn_stops_and_says_what_cannot_be_read(self) -> None:
        target = self.stage_at("5.15", "5.17")
        before = len(self.mutations())
        # the stage's record: no VERSION, a VERSION that is no N.NN, a counter of one or three digits, an extra part
        for recorded in ("", "5.x", "5", "5.9", "5.015", "v5.15", "5.15.1", "5.15-dev"):
            with self.subTest(recorded=recorded):
                self.seed_record(commit=self.MISSING, version=recorded)
                p = self.run_full(*self.UPDATE)
                flat = self.stopped(p, target, before, "a VERSION cannot be read as N.NN")
                self.assertIn("this clone has 5.17", flat)
                self.assertIn(f"the stage records {recorded or 'none'}", flat)
                self.assertNotIn("looks like", flat, "no example VERSION that a later release makes wrong")
                self.assertLess(flat.index(f"--revision {target}"), flat.index("new clone of the repository"))
        # this clone's own VERSION: no file, or a text that is no N.NN
        self.seed_record(commit=self.MISSING, version="5.15")
        for version in (None, "5.x", "5.7"):
            with self.subTest(clone=version):
                files = {"infra/release.yaml": f"release {version}\n"}
                if version is not None:
                    files["VERSION"] = f"{version}\n"
                else:
                    (self.repo / "VERSION").unlink()
                head = self.write_commit(files, f"a clone whose VERSION is {version}")
                p = self.run_full(*self.UPDATE)
                flat = self.stopped(p, head, before, "cannot be read")
                self.assertIn(f"this clone has {version or 'none'}", flat)

    # ---- U49 ---------------------------------------------------------------------------------------------
    def test_b838_the_versions_are_compared_as_numbers_per_part(self) -> None:
        driver = '. "$1/lib.sh"; . "$1/preflight.sh"; pf_version_cmp "$2" "$3"'
        cases = [("5.15", "5.17", "lower"), ("5.17", "5.17", "equal"), ("5.18", "5.17", "higher"), ("5.08", "5.09", "lower"), ("5.09", "5.10", "lower"),
                 ("5.10", "5.09", "higher"), ("5.09", "5.09", "equal"), ("5.99", "6.00", "lower"), ("6.00", "5.99", "higher"), ("4.42", "5.00", "lower"),
                 ("5.00", "4.42", "higher"), ("9.99", "10.00", "lower"), ("10.00", "9.99", "higher"), ("05.17", "5.17", "equal"), ("10.02", "10.02", "equal"),
                 # unreadable: the forms `ham_core::update::parse_version` also refuses, and the forms setup refuses in addition (a major of ten digits cannot be added up)
                 ("", "5.17", "unreadable"), ("5.17", "", "unreadable"), ("5.1", "5.17", "unreadable"), ("5.140", "5.17", "unreadable"), ("v5.14", "5.17", "unreadable"),
                 ("5", "5.17", "unreadable"), ("15", "5.17", "unreadable"), ("5.1x", "5.17", "unreadable"), (" 5.14", "5.17", "unreadable"), ("5.15 ", "5.17", "unreadable"),
                 ("5.15\n", "5.17", "unreadable"), ("5.15.1", "5.17", "unreadable"), (".15", "5.17", "unreadable"), ("5.", "5.17", "unreadable"), ("-5.15", "5.17", "unreadable"),
                 ("5.17", "5.x", "unreadable"), ("1234567890.00", "5.17", "unreadable")]
        for recorded, clone, want in cases:
            with self.subTest(recorded=recorded, clone=clone):
                p = subprocess.run(["bash", "-c", driver, "driver", str(self.SETUP_DIR), recorded, clone], capture_output=True, text=True, timeout=limit(60))  # nosec B603: bash with an argument list, the values are arguments and never part of the script; B607: bash looked up on PATH, as the scripts under test use it
                self.assertEqual((p.returncode, p.stdout.strip()), (0, want), p.stderr)
        # the same through setup, on a counter below and above ten and across a major
        for recorded, clone, code in (("5.09", "5.10", 0), ("5.10", "5.09", 7), ("5.99", "6.00", 0), ("6.00", "5.99", 7), ("4.42", "5.00", 0)):
            with self.subTest(update=f"{recorded} -> {clone}"):
                self.reset_world()
                shutil.rmtree(self.home / ".config", ignore_errors=True)
                shutil.rmtree(self.prefix, ignore_errors=True)
                self.stage_at(recorded, clone)
                p = self.run_full(*self.DRY)
                self.assertEqual(p.returncode, code, self.out(p))

    # ---- U50 ---------------------------------------------------------------------------------------------
    def test_b838_the_tree_and_revision_rules_come_before_the_version_rule(self) -> None:
        # regression: passes before the change. A target with the source tree the record names is never refused, whatever the VERSIONs say.
        self.set_scenario(nine_stacks=True)
        self.with_registry(REAL_REGISTRY.read_text())
        self.deployed()
        w = self.open_world()
        record = w.w["params"][SETTINGS_PARAM]
        lines = [ln for ln in record.splitlines() if not ln.startswith(("commit=", "version="))]
        w.w["params"][SETTINGS_PARAM] = "\n".join([*lines, f"commit={self.MISSING}", "version=5.30"])
        w.save()
        self.commit_more("NOTES.md")
        deploys = self.world().get("n_deploys", 0)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.start_build_env(self.count_calls("codebuild", "start-build") - 1).get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.world().get("n_deploys", 0), deploys, "the stage already runs this source tree")
        # --revision names the commit on purpose: no VERSION decides, whichever way it points
        for recorded in ("5.30", "5.17", "5.15", ""):
            with self.subTest(revision_with=recorded):
                self.reset_world()
                shutil.rmtree(self.home / ".config", ignore_errors=True)
                shutil.rmtree(self.prefix, ignore_errors=True)
                target = self.stage_at(recorded, "5.17")
                starts = self.count_calls("codebuild", "start-build")
                p = self.run_full(*self.UPDATE, "--revision", target)
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertEqual(self.start_build_env(starts)["SETUP_SOURCE_KEY"], f"source/{target}.zip")

    # ---- U51 ---------------------------------------------------------------------------------------------
    def test_b838_a_recorded_commit_that_the_clone_holds_is_judged_by_git_and_not_by_version(self) -> None:
        # regression: passes before the change. The VERSION plays no part when git can place the recorded commit.
        self.with_registry()
        self.deployed()
        high = self.write_commit({"VERSION": "5.30\n", "infra/release.yaml": "release 5.30\n"}, "release 5.30")
        low = self.write_commit({"VERSION": "5.20\n", "infra/release.yaml": "release 5.20\n"}, "a later commit whose VERSION is lower")
        # the target is an ancestor of the recorded commit: it stops, although its VERSION is the higher one, and it names git pull
        self.git("checkout", "-q", high)
        self.seed_record(commit=low, version="5.20")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        for needle in ("git pull", "older commit", f"--revision {high}"):
            self.assertIn(needle, self.flat(p))
        self.assertNotIn("is not in this clone", self.flat(p))
        # the target is a descendant: it goes ahead, although its VERSION is the lower one
        self.git("checkout", "-q", low)
        self.seed_record(commit=high, version="5.30")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f"commit: {high[:12]} (VERSION 5.30) -> {low[:12]} (VERSION 5.20)", self.plan(p))
        self.assertNotIn("is not in this clone", self.plan(p))
        # a commit on another branch (neither an ancestor nor a descendant) goes ahead as before; the plan shows both commits
        self.git("checkout", "-q", "-b", "side", high)
        side = self.write_commit({"side.md": "a branch\n"}, "a commit on another branch")
        self.git("checkout", "-q", low)
        self.seed_record(commit=side, version="5.40")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f"commit: {side[:12]} (VERSION 5.40) -> {low[:12]} (VERSION 5.20)", self.plan(p))

    # ---- U52 ---------------------------------------------------------------------------------------------
    def test_b838_the_record_of_an_unfinished_update_is_judged_the_same_way(self) -> None:
        # the commit of such a record is the one the unfinished update was given: it is judged like a finished one
        target = self.stage_at("5.15", "5.17", status="deploying")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("its VERSION 5.15 is lower than this clone's 5.17", self.plan(p))
        self.seed_record(commit=self.MISSING, version="5.17", status="deploying")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.stopped(p, target, before, "its VERSION is the same as this clone's")

    # ---- U53 ---------------------------------------------------------------------------------------------
    def test_b838_the_other_guards_still_judge_an_update_that_the_version_rule_lets_through(self) -> None:
        target = self.stage_at("5.15", "5.17")
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        flat = self.flat(p)
        self.assertIn("running run(s)", flat)
        self.assertNotIn("is not in this clone", flat, "the commit order let the update through; the active runs stop it")
        # with --allow-active-runs the build's own guard decides: the plan says that the image is not compared and that the build refuses a changed worker runtime
        p = self.run_full(*self.DRY, "--allow-active-runs")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        for needle in ("image: not compared", "the build refuses only when the worker runtime changes", f"-> {target[:12]} (VERSION 5.17)"):
            self.assertIn(needle, plan)

    # ---- U54 ---------------------------------------------------------------------------------------------
    def test_b838_the_update_ham_runs_from_a_fresh_clone_of_the_new_history_goes_ahead(self) -> None:
        """`ham update` fetches a clone of the public main and runs `<clone>/setup.sh --update --prefix <prefix> --yes` in it (crates/ham/src/update.rs, run_setup): no --profile,
        no --region, no --revision. The clone here is the one a user gets after the history was started again: a new root commit with the files of the old tree, and a release
        commit on top. Its commits are not the ones the stage records."""
        self.with_registry()
        old = self.deployed()
        self.seed_record(commit=old, version="5.15")
        fresh = self.tmp / "fresh-clone"
        head = self.new_history_tree(fresh)
        self.assertFalse(subprocess.run(["git", "-C", str(fresh), "cat-file", "-e", f"{old}^{{commit}}"], capture_output=True, timeout=limit(60)).returncode == 0,  # nosec B603: git with an argument list, no shell; B607: bare name of git looked up on PATH, as the scripts under test do
                         "TRIGGER PRESENT: the clone of the new history does not hold the commit the stage records")
        self.client_file.write_text(STUB_HAM.replace("ham v9.99", "ham v5.17"))
        starts = self.count_calls("codebuild", "start-build")
        p = self.run_setup("--update", "--prefix", str(self.prefix), "--yes", script=fresh / "setup.sh", AWS_PROFILE="work", AWS_REGION="us-east-1")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.start_build_env(starts)["SETUP_SOURCE_KEY"], f"source/{head}.zip")
        self.assertEqual(self.open_world().stage_record()["commit"], head)
        self.assertIn("its VERSION 5.15 is lower than this clone's 5.17", " ".join(p.stdout.split()))
        # the same clone with a stage that runs the same VERSION stops, and names a new clone: the way ham itself cannot help
        self.seed_record(commit=old, version="5.17")
        before = len(self.mutations())
        p = self.run_setup("--update", "--prefix", str(self.prefix), "--yes", script=fresh / "setup.sh", AWS_PROFILE="work", AWS_REGION="us-east-1")
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before)
        self.assertIn("new clone of the repository", self.flat(p))
        self.assertIn(f"--revision {head}", self.flat(p))

    # ---- U54 and U51, the git side of the rule (review finding: the answer of git does not depend on the kind of clone or on the version of git) ----------------------------
    def new_history_tree(self, dest: Path, release: str = "5.17") -> str:
        """A repository `dest` that holds the new history after a restart: a root commit with the files of the old tree, and a release commit on top. Returns its head."""
        dest.mkdir()
        for rel in self.git("ls-files").splitlines():
            (dest / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(self.repo / rel, dest / rel)
        self.git("init", "-q", "-b", "main", cwd=dest)
        self.git("add", "-A", cwd=dest)
        self.git("commit", "-q", "-m", "root of the new history", cwd=dest)
        (dest / "VERSION").write_text(f"{release}\n")
        (dest / "infra").mkdir(exist_ok=True)
        (dest / "infra" / "release.yaml").write_text(f"release {release}\n")
        self.git("add", "-A", cwd=dest)
        self.git("commit", "-q", "-m", f"release {release}", cwd=dest)
        return self.git("rev-parse", "HEAD", cwd=dest)

    def blobless_clone(self, old: str, name: str) -> tuple[Path, str]:
        """The clone `ham update` makes (`git clone --no-checkout --filter=blob:none`, then a detached checkout) of an origin that holds the new history on main and still serves
        the old commit `old` by its id: its branch was deleted, as after a history restart, but the commit stays in the object store of the origin. Returns the clone and its head."""
        new = self.tmp / f"new-history-{name}"
        head = self.new_history_tree(new)
        origin = self.tmp / f"origin-{name}.git"
        self.git("init", "-q", "--bare", str(origin), cwd=self.tmp)
        self.git("config", "uploadpack.allowFilter", "true", cwd=origin)
        self.git("config", "uploadpack.allowAnySHA1InWant", "true", cwd=origin)
        self.git("push", "-q", str(origin), f"{old}:refs/heads/old")
        self.git("push", "-q", str(origin), "HEAD:refs/heads/main", cwd=new)
        self.git("push", "-q", str(origin), ":refs/heads/old")
        clone = self.tmp / f"clone-{name}"
        self.git("clone", "-q", "--no-checkout", "--filter=blob:none", f"file://{origin}", str(clone), cwd=self.tmp)
        self.git("checkout", "-q", "--detach", "origin/main", cwd=clone)
        return clone, head

    def holds(self, clone: Path, commit: str, lazy: bool) -> bool:
        """Whether `git cat-file -e` finds the commit in the clone; with lazy=True it may fetch a missing commit from the origin (which changes the clone)."""
        env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
        env.pop("GIT_NO_LAZY_FETCH", None)
        if not lazy:
            env["GIT_NO_LAZY_FETCH"] = "1"
        p = subprocess.run(["git", "-C", str(clone), "cat-file", "-e", f"{commit}^{{commit}}"], capture_output=True, env=env, timeout=limit(60))  # nosec B603: git with an argument list, no shell; B607: bare name of git looked up on PATH, as the scripts under test do
        return p.returncode == 0

    def update_from(self, clone: Path, *extra: str):
        """The update `ham update` runs: the setup.sh of the clone, with --prefix and --yes, and no --profile, --region or --revision."""
        return self.run_setup("--update", "--prefix", str(self.prefix), "--yes", *extra, script=clone / "setup.sh", AWS_PROFILE="work", AWS_REGION="us-east-1")

    def test_b838_a_blobless_clone_whose_origin_serves_the_old_commit_still_judges_it_missing(self) -> None:
        """The clone `ham update` makes is a blobless partial clone, and a lookup of a missing commit in it fetches the commit from the origin when the origin serves it by id
        (GitHub may). The answer for the recorded commit must not depend on that: it is not in the clone, so the VERSIONs decide, as in a full clone."""
        self.with_registry()
        old = self.deployed()
        probe, _ = self.blobless_clone(old, "probe")
        self.assertTrue(self.holds(probe, old, lazy=True), "TRIGGER PRESENT: the origin serves the old commit by its id, so a plain lookup fetches it into the clone")
        clone, head = self.blobless_clone(old, "ham")
        if self.holds(clone, old, lazy=False):
            self.fail("a clone of the new history holds the old commit")
        self.client_file.write_text(STUB_HAM.replace("ham v9.99", "ham v5.17"))
        # the same VERSION stops: git is not asked to place a commit that the clone does not hold
        self.seed_record(commit=old, version="5.17")
        before = len(self.mutations())
        p = self.update_from(clone)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        flat = self.flat(p)
        for needle in (f"runs commit {old}", "is not in this clone", "its VERSION is the same as this clone's", f"--revision {head}"):
            self.assertIn(needle, flat)
        self.assertFalse(self.holds(clone, old, lazy=False), "the lookup of setup left the clone as it was: it fetched nothing")
        # a lower VERSION goes ahead and the plan says that the commit is not in the clone
        self.seed_record(commit=old, version="5.15")
        starts = self.count_calls("codebuild", "start-build")
        p = self.update_from(clone)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the commit the stage records is not in this clone", self.plan(p))
        self.assertEqual(self.start_build_env(starts)["SETUP_SOURCE_KEY"], f"source/{head}.zip")
        self.assertFalse(self.holds(clone, old, lazy=False), "the update did not fetch the old commit either")

    def test_b838_a_git_that_ignores_the_no_lazy_fetch_variable_reaches_the_same_answer(self) -> None:
        """Git before 2.44 ignores GIT_NO_LAZY_FETCH, so the lookup of the recorded commit fetches it from an origin that serves it. The commit is then held, with no history in
        common with the clone's, and setup judges it by the VERSIONs as it does a commit the clone does not hold."""
        self.with_registry()
        old = self.deployed()
        clone, head = self.blobless_clone(old, "old-git")
        real = shutil.which("git")
        wrapper = self.stub / "git"
        wrapper.unlink()
        wrapper.write_text(f'#!/bin/sh\nunset GIT_NO_LAZY_FETCH\nexec "{real}" "$@"\n')
        wrapper.chmod(0o755)
        self.client_file.write_text(STUB_HAM.replace("ham v9.99", "ham v5.17"))
        self.seed_record(commit=old, version="5.17")
        before = len(self.mutations())
        p = self.update_from(clone)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        flat = self.flat(p)
        for needle in (f"runs commit {old}", "has no history in common with this clone", "its VERSION is the same as this clone's", f"--revision {head}"):
            self.assertIn(needle, flat)
        self.seed_record(commit=old, version="5.15")
        starts = self.count_calls("codebuild", "start-build")
        p = self.update_from(clone)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        for needle in ("the commit the stage records has no history in common with this clone", "its VERSION 5.15 is lower than this clone's 5.17"):
            self.assertIn(needle, plan)
        self.assertEqual(self.start_build_env(starts)["SETUP_SOURCE_KEY"], f"source/{head}.zip")

    def test_b838_a_recorded_commit_that_the_clone_holds_without_history_in_common_is_judged_by_version(self) -> None:
        """A commit of another history that the clone holds (fetched into it) cannot be placed by git either: no common ancestor. Before this check it went ahead whatever its VERSION."""
        self.with_registry()
        old = self.deployed()
        fresh = self.tmp / "fresh-clone"
        head = self.new_history_tree(fresh)
        self.git("fetch", "-q", str(self.repo), old, cwd=fresh)
        self.assertTrue(self.holds(fresh, old, lazy=False), "TRIGGER PRESENT: the clone holds the old commit")
        self.client_file.write_text(STUB_HAM.replace("ham v9.99", "ham v5.17"))
        self.seed_record(commit=old, version="5.17")
        before = len(self.mutations())
        p = self.update_from(fresh)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        flat = self.flat(p)
        for needle in (f"runs commit {old}", "has no history in common with this clone", "its VERSION is the same as this clone's", f"--revision {head}"):
            self.assertIn(needle, flat)
        self.seed_record(commit=old, version="5.15")
        p = self.update_from(fresh)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the commit the stage records has no history in common with this clone", self.plan(p))

    def test_b838_the_plan_says_that_a_version_orders_releases_only(self) -> None:
        """The plan of an update that goes ahead on the VERSIONs alone says what the VERSION is worth: it orders releases and says nothing about the content of a commit. A stage
        deployed from the user's own fork or from commits that were never published, with a lower VERSION, goes ahead too, so the plan asks the user to check the line."""
        self.stage_at("5.15", "5.17")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        for needle in ("so setup treats this update as a newer release", "A VERSION orders releases only",
                       "if the stage was deployed from your own fork or from commits that you did not publish, check this line before you answer"):
            self.assertIn(needle, plan)
        self.assertNotIn("takes this update as a newer release", plan)


if __name__ == "__main__":
    unittest.main()
