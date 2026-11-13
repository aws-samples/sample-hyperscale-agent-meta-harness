"""The runner route of `./setup.sh --update` in every state a user can leave a stage in.

An update on the default route installs the setup runner in the user's account, uploads the source, takes the stage's deploy lock, starts one CodeBuild build of
scripts/setup/runner-build.sh and follows it. These tests hold that route to the rows of the matrix that belong to the runner: the stage settings reach the
build's deploy (U2), the lock keeps two updates from deploying at once (U17, U18), a stage that already runs the commit gets only a client (U24), a deploy that
refuses before any stack change says the stage is unchanged (U28), and a read that fails or an interruption after the build started resumes the same build (U29,
U30). They run `setup.sh` from a stand-in checkout against the stand-in AWS CLI of setup_fake_aws.py, and `runner-build.sh` in the rig of
test_setup_runner_build.py with a stub `deploy.sh`.

`StandInCase` holds the parts of the stand-in that these rows need: SSM parameters kept in the world (a create-only put, an overwrite, a delete, tags on a
create), the stage with the Instances part, the stage record and the refusal of a build whose deploy resolves the Instances setting to off.
"""

from __future__ import annotations

import base64
import contextlib
import hashlib
import io
import json
import os
import re
import subprocess  # nosec B404: the tests start bash and git with argument lists, never a shell string
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import setup_fake_aws as fake
from setup_waits import limit
import test_setup_runner_build as rb
from test_setup_full_state import DIGEST
from test_setup_update_guard import GUARD_TEXT, RUNS, UpdateBase

NOW = "2026-11-10T16:00:00Z"


def b64_lines(*lines: str) -> str:
    """SETUP_EXPLICIT_SETTINGS of CONTRACT C4: base64 without line wrap of `HAMH_<NAME>=value` lines."""
    return base64.b64encode(("\n".join(lines) + "\n").encode()).decode()


class StandInCase(unittest.TestCase):
    """The stand-in AWS CLI as the rows of lane C use it."""

    def setUp(self) -> None:
        td = tempfile.TemporaryDirectory(prefix="b812-standin-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(td.cleanup)
        self.dir = Path(td.name)
        self.state = self.dir / "state"
        self.scenario_path = self.dir / "scenario.json"
        self.scenario: dict = {"bootstrap_present": True}
        self.write_scenario()

    def write_scenario(self, **kw) -> None:
        self.scenario.update(kw)
        self.scenario_path.write_text(json.dumps(self.scenario))

    def aws(self, *argv: str):
        """One call of the stand-in: (exit code, stdout, stderr)."""
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, {"FAKE_AWS_STATE": str(self.state), "FAKE_AWS_SCENARIO": str(self.scenario_path)}):
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = fake.main(list(argv))
        return rc, out.getvalue(), err.getvalue()

    def world(self) -> fake.World:
        self.state.mkdir(parents=True, exist_ok=True)
        return fake.World(self.state, self.scenario_path)

    # ---- SSM parameters ----
    def test_a_put_without_overwrite_creates_once_and_then_says_the_parameter_exists(self) -> None:
        rc, out, err = self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "token=aa", "--type", "String",
                                "--tags", "Key=project,Value=hyperscale-agent-meta-harness", "Key=stage,Value=direct", "--output", "json")
        self.assertEqual(rc, 0, err)
        self.assertEqual(json.loads(out)["Version"], 1)
        rc, out, err = self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "token=bb", "--type", "String", "--output", "json")
        self.assertEqual(rc, 254)
        self.assertIn("(ParameterAlreadyExists)", err)
        w = self.world().w
        self.assertEqual(w["params"][fake.LOCK_PARAM], "token=aa", "a refused create leaves the holder's value")
        self.assertEqual(w["param_tags"][fake.LOCK_PARAM], {"project": "hyperscale-agent-meta-harness", "stage": "direct"})

    def test_an_overwrite_replaces_the_value_keeps_the_tags_and_refuses_new_tags(self) -> None:
        self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "token=aa", "--type", "String", "--tags", "Key=project,Value=p")
        rc, out, err = self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "token=aa\nbuild=b1", "--type", "String", "--overwrite", "--output", "json")
        self.assertEqual(rc, 0, err)
        self.assertEqual(json.loads(out)["Version"], 2)
        rc, out, err = self.aws("ssm", "get-parameter", "--name", fake.LOCK_PARAM, "--output", "json")
        self.assertEqual((rc, json.loads(out)["Parameter"]["Value"], json.loads(out)["Parameter"]["Version"]), (0, "token=aa\nbuild=b1", 2))
        self.assertEqual(self.world().w["param_tags"][fake.LOCK_PARAM], {"project": "p"})
        rc, _, err = self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "x", "--type", "String", "--overwrite", "--tags", "Key=a,Value=b")
        self.assertEqual(rc, 254)
        self.assertIn("tags and overwrite can't be used together", err)

    def test_a_delete_removes_the_parameter_and_a_second_delete_says_not_found(self) -> None:
        self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "token=aa", "--type", "String")
        self.assertEqual(self.aws("ssm", "delete-parameter", "--name", fake.LOCK_PARAM)[0], 0)
        self.assertNotIn(fake.LOCK_PARAM, self.world().w["params"])
        rc, _, err = self.aws("ssm", "delete-parameter", "--name", fake.LOCK_PARAM)
        self.assertEqual(rc, 254)
        self.assertIn("(ParameterNotFound)", err)
        rc, _, err = self.aws("ssm", "get-parameter", "--name", fake.LOCK_PARAM)
        self.assertEqual(rc, 254)
        self.assertIn("(ParameterNotFound)", err)

    def test_an_empty_value_is_refused_as_ssm_refuses_it(self) -> None:
        rc, _, err = self.aws("ssm", "put-parameter", "--name", fake.LOCK_PARAM, "--value", "", "--type", "String")
        self.assertEqual(rc, 254)
        self.assertIn("ValidationException", err)

    # ---- the stage record and the Instances part ----
    def test_the_scenario_seeds_the_record_the_lock_and_other_parameters_with_the_stage_tags(self) -> None:
        self.write_scenario(stage_settings={"status": "deployed", "commit": "a" * 40, "set": {"HAMH_COMPUTE_INSTANCES": "on"}},
                            deploy_lock="token=ab\nholder=h:1", params={"/hamh/direct/other": "x"})
        w = self.world()
        self.assertEqual(w.param(fake.SETTINGS_PARAM), f"schema=1\nstatus=deployed\ncommit={'a' * 40}\nset.HAMH_COMPUTE_INSTANCES=on")
        self.assertEqual(w.stage_record()["set"], {"HAMH_COMPUTE_INSTANCES": "on"})
        self.assertEqual(w.param(fake.LOCK_PARAM), "token=ab\nholder=h:1")
        for name in (fake.SETTINGS_PARAM, fake.LOCK_PARAM, "/hamh/direct/other"):
            self.assertEqual(w.w["param_tags"][name], fake.PROJECT_TAGS)

    def test_an_instances_stage_has_the_outputs_and_the_marker_and_a_deploy_keeps_them(self) -> None:
        self.write_scenario(stage_deployed=True, instances_part=True)
        w = self.world()
        outputs = w.w["stacks"]["hamh-direct-runtimes"]["outputs"]
        for key in fake.INSTANCES_OUTPUTS:
            self.assertIn(key, outputs)
        self.assertEqual(len([n for n in w.w["stacks"] if n.startswith("hamh-direct-")]), 9, "an Instances stage is the product's nine stacks")
        self.assertIn(fake.INSTANCES_MARKER, w.w["params"])
        self.assertTrue(w.has_instances_part())
        w.deploy_stage()
        self.assertTrue(w.has_instances_part(), "a deploy rebuilds the stacks and keeps the part")

    def test_the_default_nine_stack_stage_has_no_instances_part(self) -> None:
        self.write_scenario(stage_deployed=True, nine_stacks=True)
        w = self.world()
        self.assertFalse(w.has_instances_part())
        self.assertNotIn(fake.INSTANCES_MARKER, w.w.get("params", {}))

    # ---- the build's deploy and the Instances setting ----
    def build(self, w: fake.World, **env: str) -> fake.Build:
        base = {"SETUP_SOURCE_KEY": f"source/{'c' * 40}.zip", "SETUP_CONFIG_KEY": "config/accounts-0123456789ab.json", "SETUP_STAGE": "direct", "SETUP_AZS": "1",
                "SETUP_BINARY_NAME": "ham"}
        w.w["n_builds"] += 1
        bid = f"hamh-setup-runner-build:00000000-0000-4000-8000-a{w.w['n_builds']:011d}"
        w.w["builds"][bid] = {"status": "IN_PROGRESS", "phase": "QUEUED", "polls": 0, "env": {**base, **env}, "log": []}
        w.make_runner()
        return fake.Build(w, bid)

    def run_build(self, w: fake.World, b: fake.Build) -> dict:
        for _ in range(10):
            b.advance()
        bucket = w.runner_names()["SetupBucket"]
        return json.loads(w.get(bucket, f"results/{b.bid}/result.json"))

    def test_a_deploy_that_resolves_the_setting_to_off_on_an_instances_stage_fails_as_the_runner_files_it(self) -> None:
        self.write_scenario(stage_deployed=True, instances_part=True, deploy_settings="env")
        w = self.world()
        b = self.build(w)
        result = self.run_build(w, b)
        self.assertEqual(b.rec["status"], "FAILED")
        self.assertEqual((result["state"], result["failedPhase"]), ("SETUP-FAIL", "deploy"))
        self.assertTrue(result["detail"].startswith("instances-off the stage has the Instances part"), result["detail"])
        self.assertTrue(w.has_instances_part(), "the refused deploy changed nothing")

    def test_an_explicit_on_reaches_the_deploy_and_the_part_stays(self) -> None:
        self.write_scenario(stage_deployed=True, instances_part=True, deploy_settings="env")
        w = self.world()
        b = self.build(w, SETUP_EXPLICIT_SETTINGS=b64_lines("HAMH_COMPUTE_INSTANCES=on"))
        result = self.run_build(w, b)
        self.assertEqual((b.rec["status"], result["state"]), ("SUCCEEDED", "SETUP-PASS"))
        self.assertTrue(w.has_instances_part())

    def test_the_default_deploy_keeps_the_part_from_the_record_or_from_the_live_stage(self) -> None:
        # CONTRACT C3: explicit > the record's set. line > the live stage when there is no record
        for scenario, status in (({}, "SUCCEEDED"), ({"stage_settings": {"status": "deployed", "set": {"HAMH_COMPUTE_INSTANCES": "on"}}}, "SUCCEEDED"),
                                 ({"stage_settings": {"status": "deployed"}}, "FAILED")):
            with self.subTest(scenario=scenario):
                self.state = self.dir / f"state-{status}-{len(scenario)}-{len(json.dumps(scenario))}"
                self.scenario = {"bootstrap_present": True, "stage_deployed": True, "instances_part": True, **scenario}
                self.write_scenario()
                w = self.world()
                b = self.build(w)
                result = self.run_build(w, b)
                self.assertEqual(b.rec["status"], status)
                if status == "FAILED":
                    # this release's deploy.py refuses before any stack change, which runner-build.sh files as the phase refused
                    self.assertEqual(result["failedPhase"], "refused")
                    self.assertTrue(result["detail"].startswith("reason=instances-off wait=no stage=unchanged :: "), result["detail"])

    def test_an_old_runner_passes_nothing_so_even_an_explicit_on_is_refused(self) -> None:
        self.write_scenario(stage_deployed=True, instances_part=True, deploy_settings="old")
        w = self.world()
        b = self.build(w, SETUP_EXPLICIT_SETTINGS=b64_lines("HAMH_COMPUTE_INSTANCES=on"))
        self.run_build(w, b)
        self.assertEqual(b.rec["status"], "FAILED")

    def test_a_stage_without_the_part_deploys_as_before(self) -> None:
        self.write_scenario(stage_deployed=True, nine_stacks=True, deploy_settings="env")
        w = self.world()
        b = self.build(w)
        self.run_build(w, b)
        self.assertEqual(b.rec["status"], "SUCCEEDED")
        self.assertFalse(w.has_instances_part())


# ---- the rows ---------------------------------------------------------------------------------------------------------------------------------------------------------------

def inputs_digest(config_sha: str = DIGEST, settings_b64: str = "") -> str:
    """What runner.sh records as `inputs` in the deploy lock: the account file and the explicit settings of the run."""
    return hashlib.sha256(f"config={config_sha}\nsettings={settings_b64}\n".encode()).hexdigest()


class RunnerUpdateBase(UpdateBase):
    """An update on the runner route against the stand-in, with the deploy lock read from the stand-in's SSM parameters."""

    def lock(self):
        return self.world().get("params", {}).get(fake.LOCK_PARAM)

    def said(self, p) -> str:
        """Everything the run printed, on both streams, in one line (warn lines go to stdout, err and detail lines to stderr)."""
        return " ".join(self.out(p).split())

    def lock_fields(self) -> dict:
        return fake.parse_record(self.lock() or "")

    def seed_lock(self, **fields: str) -> None:
        w = self.open_world()
        w.put_param(fake.LOCK_PARAM, "\n".join(f"{k}={v}" for k, v in fields.items()), dict(fake.PROJECT_TAGS))
        w.save()

    def seed_record(self, **fields) -> None:
        w = self.open_world()
        w.put_param(fake.SETTINGS_PARAM, fake.settings_record_text(fields), dict(fake.PROJECT_TAGS))
        w.save()

    def deploy_starts(self) -> list:
        """The start-build calls of deploy builds (the bootstrap template build has SETUP_ACTION)."""
        out = []
        for n, c in enumerate([c for c in self.normalized_calls() if c[:2] == ["codebuild", "start-build"]]):
            env = self.start_build_env(n)
            if env.get("SETUP_ACTION", "deploy") == "deploy":
                out.append(env)
        return out

    def runner_deploys(self) -> int:
        return sum(1 for c in self.normalized_calls() if c[:4] == ["cloudformation", "deploy", "--stack-name", "hamh-setup-runner"])

    def build_env(self, commit: str, **extra: str) -> dict:
        return {"SETUP_SOURCE_KEY": f"source/{commit}.zip", "SETUP_CONFIG_KEY": f"config/accounts-{DIGEST[:12]}.json", "SETUP_STAGE": "direct", "SETUP_AZS": "1",
                "SETUP_BINARY_NAME": "ham", **extra}

    def seed_running(self, commit: str, **extra: str) -> str:
        """A build of the runner that is running: the runner stack is there, and CodeBuild lists the build."""
        w = self.open_world()
        w.make_runner()
        w.save()
        return self.seed_build("IN_PROGRESS", self.build_env(commit, **extra))


class U2InstancesCase(RunnerUpdateBase):
    """U2: an update of a stage with the Instances part keeps the part: the setting reaches the build's deploy."""

    def instances_stage(self) -> None:
        # the deploy of the stand-in reads only its environment here, so the part survives only when setup.sh carries the setting to it
        self.set_scenario(nine_stacks=True, deploy_settings="env")
        # the settings registry of the commit that is built: setup.sh collects only the settings it lists
        (self.repo / "scripts" / "lib").mkdir(parents=True, exist_ok=True)
        (self.repo / "scripts" / "lib" / "stage-settings.tsv").write_text("HAMH_COMPUTE_INSTANCES|yes|refuse-off|^(on|off)$|The Instances compute type.\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "the settings registry")
        self.deployed()
        w = self.open_world()
        w.add_instances_part()
        w.save()
        self.commit_more()

    def test_b812_runner_update_keeps_the_instances_part_and_passes_it_to_the_deploy(self) -> None:
        self.instances_stage()
        self.assertTrue(self.open_world().has_instances_part(), "TRIGGER PRESENT: the stage has the Instances part")
        p = self.run_full(*self.UPDATE, HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(p.returncode, 0, self.out(p))
        env = self.deploy_starts()[-1]
        self.assertEqual(fake.explicit_settings(env), {"HAMH_COMPUTE_INSTANCES": "on"}, "the setting is in the build's environment")
        self.assertRegex(env.get("SETUP_DEPLOY_LOCK_TOKEN", ""), r"^[0-9a-f]{32}$", "the build carries the token of the deploy lock")
        self.assertTrue(self.open_world().has_instances_part(), "the part and its machines stay")
        self.assertNotIn("instances-off", self.out(p))
        self.assertNotIn("partial deployment changes", self.flat(p))
        self.assertFalse(self.runner_installed())
        self.assertIsNone(self.lock(), "the lock is released at the end")
        # the runner half: runner-build.sh decodes the setting and exports it, with the lock's token, into the environment of deploy.sh
        rig = rb.Rig(self)
        (rig.tree / "scripts/lib/stage-settings.tsv").write_text("# NAME|KEPT|CHANGE|PATTERN|NOTE\nHAMH_COMPUTE_INSTANCES|yes|refuse-off|^(on|off)$|The Instances compute type.\n")
        rig.put_source()
        rig.state["stacks"]["hamh-direct-runtimes"]["InstancesCapacityProviderArn"] = "arn:aws:bedrock-agentcore:us-east-1:123456789012:capacity-provider/x-y"
        token = "ab" * 16
        proc = rig.run(FAKE_DEPLOY="instances", SETUP_EXPLICIT_SETTINGS=b64_lines("HAMH_COMPUTE_INSTANCES=on"), SETUP_DEPLOY_LOCK_TOKEN=token)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        deploy_env = rig.calls("deploy")[0]["env"]
        self.assertEqual((deploy_env.get("HAMH_COMPUTE_INSTANCES"), deploy_env.get("HAMH_DEPLOY_LOCK_TOKEN")), ("on", token))
        refused = rb.Rig(self)
        (refused.tree / "scripts/lib/stage-settings.tsv").write_text("HAMH_COMPUTE_INSTANCES|yes|refuse-off|^(on|off)$|The Instances compute type.\n")
        refused.put_source()
        proc = refused.run(FAKE_DEPLOY="instances", SETUP_EXPLICIT_SETTINGS=b64_lines("HAMH_COMPUTE_INSTANCES=maybe"))
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(refused.result()["failedPhase"], "inputs", "a value the registry's pattern refuses stops the build before the deploy")
        self.assertEqual(refused.calls("deploy"), [])
        # the other half of the row (Addendum 4, A4.15): nothing is exported. The deploy of this release keeps the part from the stage's record or the live stage, and setup
        # sends no setting of its own
        self.set_scenario(deploy_settings="resolve")
        (self.repo / "infra" / "change.yaml").write_text("a: 1\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "a change the stage deploys")
        deploys = self.world().get("n_deploys", 0)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        env = self.deploy_starts()[-1]
        self.assertNotIn("SETUP_CLIENT_ONLY", env, "TRIGGER PRESENT: this update deploys")
        self.assertEqual(fake.explicit_settings(env), {}, "setup sends nothing you did not export")
        self.assertEqual(self.world().get("n_deploys", 0), deploys + 1)
        self.assertIn("Instances part: present, kept", " ".join(q.stdout.split()))
        self.assertTrue(self.open_world().has_instances_part(), "the part and its machines stay without the export")
        self.assertNotIn("instances-off", self.out(q))


class U16ActiveRunsCase(RunnerUpdateBase):
    """U16 [ok]: active runs stop the update before the runner is installed; with --allow-active-runs the build's own guard decides."""

    def test_b812_active_runs_stop_the_update_before_the_runner_is_installed(self) -> None:
        self.deployed()
        self.commit_more()
        before = len(self.mutations())
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("The stage is unchanged and no runner was installed.", self.flat(p))
        self.assertIn("ham cancel <runId>", self.flat(p))
        self.assertEqual(len(self.mutations()), before)
        self.assertFalse(self.runner_installed())
        self.set_scenario(build={"result": "fail", "failed_phase": "guard", "detail": GUARD_TEXT.replace("DEPLOY-FAIL guard ", "")})
        q = self.run_full(*self.UPDATE, "--allow-active-runs")
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn("The stage is unchanged", self.flat(q))
        self.assertFalse(self.runner_installed(), "the runner of a refused update is removed")
        self.assertIsNone(self.lock())


class U17TwoUpdatesCase(RunnerUpdateBase):
    """U17: two updates at the same time never run two deploys; the second stops before it installs the runner."""

    def test_b812_two_updates_at_once_never_run_two_deploys(self) -> None:
        first = self.deployed()
        head = self.commit_more()
        starts = len(self.deploy_starts())
        # the race: another setup of the same commit and inputs has taken the lock and not yet started its build
        self.seed_lock(token="cd" * 16, holder="other-host:4242", identity=f"arn:aws:iam::{self.ACCOUNT}:user/setup-test", route="runner", started=fake_now(),
                       commit=head, inputs=inputs_digest(), build="")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.deploy_starts()), starts, "no second build")
        self.assertEqual(self.runner_deploys(), 1, "the runner is not installed again (only the full setup installed it)")
        self.assertIn("other-host:4242", self.out(p))
        # another identity with other inputs and a running build: refused before the runner is installed or its bucket policy rewritten
        bid = self.seed_running(first)
        self.seed_lock(token="ef" * 16, holder="laptop-2:77", identity=f"arn:aws:iam::{self.ACCOUNT}:user/someone-else", route="runner", started=fake_now(),
                       commit=first, inputs="0" * 64, build=bid)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn(bid, self.out(q))
        self.assertEqual(self.runner_deploys(), 1)
        self.assertEqual(len(self.deploy_starts()), starts)


def fake_now(seconds_ago: int = 0) -> str:
    import datetime
    t = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=seconds_ago)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


class U18HeldCase(RunnerUpdateBase):
    """U18: a held lock or a failed runner stack is named with its way out before the confirmation."""

    def test_b812_a_held_lease_or_failed_runner_stack_is_named_with_its_way_out_before_the_confirmation(self) -> None:
        first = self.deployed()
        self.commit_more()
        before = len(self.mutations())
        bid = self.seed_running(first)
        self.seed_lock(token="12" * 16, holder="build-box:999", identity=f"arn:aws:iam::{self.ACCOUNT}:user/setup-test", route="runner", started="2026-11-10T15:00:00Z",
                       commit=first, inputs=inputs_digest(), build=bid)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = self.flat(p)
        for words in ("build-box:999", "2026-11-10T15:00:00Z", first, f"git checkout {first}", "your own earlier setup"):
            self.assertIn(words, flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created before the refusal")
        # a lock that never got a build id and is older than 30 minutes is stale: the update goes on and builds once
        self.seed_lock(token="34" * 16, holder="gone-host:1", identity="x", route="runner", started=fake_now(7200), commit=first, inputs="0" * 64, build="")
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        # a runner stack that an earlier removal left in DELETE_FAILED is named before the confirmation, with the commands that remove it
        w = self.open_world()
        w.make_runner()
        w.w["stacks"]["hamh-setup-runner"]["status"] = "DELETE_FAILED"
        w.save()
        self.commit_more("MORE.md")
        before = len(self.mutations())
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 7, self.out(r))
        flat = self.flat(r)
        self.assertIn("DELETE_FAILED", flat)
        self.assertIn("aws cloudformation delete-stack --stack-name hamh-setup-runner --role-arn arn:aws:iam::", flat)
        self.assertIn("./setup.sh --full", flat)
        self.assertEqual(len(self.mutations()), before)
        self.assertEqual(self.runner_deploys(), 2, "the runner is not installed after the confirmation (the full setup and the update of part 2 installed it)")


class A44RunnerInUseCase(RunnerUpdateBase):
    """Addendum 4, A4.4 (F6): a setup keeps the deploy lock until it has removed its runner, a lock whose build succeeded stays its holder's while the holder renews it,
    and no setup removes the runner while a build of it runs or another setup holds the lock."""

    def lock_calls(self, calls: list, op: str, overwrite=None) -> list:
        """The positions of the calls of this ssm operation on the deploy lock (put-parameter with or without --overwrite)."""
        return [i for i, c in enumerate(calls) if c[:2] == ["ssm", op] and fake.LOCK_PARAM in c and (overwrite is None or ("--overwrite" in c) == overwrite)]

    def runner_deletes(self, calls: list) -> list:
        return [i for i, c in enumerate(calls) if c[:2] == ["cloudformation", "delete-stack"] and "hamh-setup-runner" in c]

    def test_b812_fix_the_holder_gives_the_lock_back_after_it_removed_the_runner_and_renews_it_meanwhile(self) -> None:
        self.deployed()
        self.commit_more()
        n = len(self.normalized_calls())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.normalized_calls()[n:]
        removed, released = self.runner_deletes(calls), self.lock_calls(calls, "delete-parameter")
        self.assertTrue(removed and released, "TRIGGER PRESENT: the update removed its runner and released its lock")
        self.assertGreater(released[-1], removed[-1], "the lock is given back as the last step, after the runner is gone")
        renewals = self.lock_calls(calls, "put-parameter", overwrite=True)
        self.assertGreaterEqual(len(renewals), 3, "the record of the build, and a new start time after the build ended and before the runner goes")
        self.assertLess(renewals[-1], removed[-1])
        value = calls[renewals[-1]][calls[renewals[-1]].index("--value") + 1]
        self.assertRegex(value, r"(?m)^build=hamh-setup-runner-build:", "a renewal keeps the build the lock records")
        self.assertIsNone(self.lock())
        self.assertFalse(self.runner_installed())

    def test_b812_fix_a_lock_whose_build_succeeded_stays_its_holders_while_it_installs_its_client(self) -> None:
        first = self.deployed()
        self.commit_more()
        # another setup's build (other inputs) succeeded a minute ago: that setup is downloading its client from the runner's bucket and has not removed its runner yet
        w = self.open_world()
        w.make_runner()
        w.save()
        bid = self.seed_build("SUCCEEDED", self.build_env(first))
        self.seed_lock(token="ab" * 16, holder="laptop-2:77", identity=f"arn:aws:iam::{self.ACCOUNT}:user/someone-else", route="runner", started=fake_now(60), commit=first,
                       inputs="0" * 64, build=bid)
        before, installs = len(self.mutations()), self.runner_deploys()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = self.flat(p)
        for words in ("laptop-2:77", bid, "succeeded", "Nothing was changed."):
            self.assertIn(words, flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created under the other setup")
        self.assertEqual(self.runner_deploys(), installs, "the runner and its bucket policy are not rewritten")
        self.assertEqual(self.lock_fields().get("token"), "ab" * 16, "the holder keeps its lock")
        self.assertTrue(self.runner_installed())
        # 31 minutes without a renewal: the holder is gone, so the lock is stale and the update goes on
        self.seed_lock(token="ab" * 16, holder="laptop-2:77", identity=f"arn:aws:iam::{self.ACCOUNT}:user/someone-else", route="runner", started=fake_now(1900), commit=first,
                       inputs="0" * 64, build=bid)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertIsNone(self.lock())
        # a holder that was a setup of this machine and has ended is gone at once
        self.commit_more("MORE.md")
        w = self.open_world()
        w.make_runner()
        w.save()
        import socket
        self.seed_lock(token="cd" * 16, holder=f"{socket.gethostname()}:999999", identity="x", route="runner", started=fake_now(60), commit=first, inputs="0" * 64, build=bid)
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 0, self.out(r))

    def test_b812_fix_a_leftover_runner_stays_while_a_build_of_it_runs_and_goes_under_the_lock_afterwards(self) -> None:
        self.deployed()
        # a runner that another setup installed; its build runs without a deploy lock (the build that prints the CDK bootstrap template takes none)
        w = self.open_world()
        w.make_runner()
        w.save()
        bid = self.seed_build("IN_PROGRESS", self.build_env(self.commit(), SETUP_ACTION="bootstrap-template"))
        self.set_scenario(template_build={"hang": True})
        p = self.run_full(*self.ARGS)  # the stage is deployed: this run builds nothing and would remove a runner that is left
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.runner_installed(), "the runner that another setup's build uses stays")
        self.assertIn(bid, self.said(p))
        self.assertIsNone(self.lock())
        # the build has ended: the next run removes the runner, with the deploy lock taken before and given back after
        w = self.open_world()
        w.w["builds"][bid]["status"] = "SUCCEEDED"
        w.save()
        n = len(self.normalized_calls())
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertFalse(self.runner_installed())
        calls = self.normalized_calls()[n:]
        taken, removed, released = self.lock_calls(calls, "put-parameter", overwrite=False), self.runner_deletes(calls), self.lock_calls(calls, "delete-parameter")
        self.assertTrue(taken and removed and released, (taken, removed, released))
        self.assertLess(taken[0], removed[0], "the lock is taken before the runner is removed")
        self.assertLess(removed[-1], released[-1], "and given back after")
        self.assertIsNone(self.lock())

    def test_b812_fix_a_leftover_runner_stays_when_another_setup_takes_the_lock_meanwhile(self) -> None:
        self.deployed()
        w = self.open_world()
        w.make_runner()
        w.save()
        # another setup takes the deploy lock after this run read it and a moment before this run asks for it (the stand-in creates the parameter when the create arrives)
        self.set_scenario(on_call=[{"match": f"ssm put-parameter --name {fake.LOCK_PARAM}", "params": {fake.LOCK_PARAM: dict(fake.PROJECT_TAGS)}}])
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.runner_installed(), "the runner stays: another setup holds the deploy lock")
        self.assertIn("deploy lock", self.said(p))
        self.assertIsNotNone(self.lock(), "the other setup's lock is left alone")


class A44SharedLockCase(RunnerUpdateBase):
    """Addendum 4, A4.4, the two cases the first fix left: two setups that follow the same build share its deploy lock, and the one that ends first leaves the runner and the
    lock to the other; a teardown does not delete under a setup whose build succeeded and that still installs its client."""

    TEAR = ("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", fake.ACCOUNT)
    JOIN = "codebuild batch-get-builds"

    def lock_puts(self, calls: list) -> list:
        """The values this run wrote over the deploy lock, in order."""
        return [c[c.index("--value") + 1] for c in calls if c[:2] == ["ssm", "put-parameter"] and fake.LOCK_PARAM in c and "--overwrite" in c]

    def runner_deletes(self, calls: list) -> list:
        return [i for i, c in enumerate(calls) if c[:2] == ["cloudformation", "delete-stack"] and "hamh-setup-runner" in c]

    def held(self) -> dict:
        """The lines of the deploy lock as seed_lock takes them."""
        fields = self.lock_fields()
        fields.pop("set", None)
        return fields

    def test_b812_fix_a_setup_that_joins_a_build_leaves_the_runner_and_the_lock_to_the_setup_that_started_it(self) -> None:
        import socket
        self.deployed()
        self.commit_more()
        starts = len(self.deploy_starts())
        # the setup that started the build: its follow is cut off, and the build keeps running
        self.set_scenario(connection_lost=True)
        self.assertEqual(self.run_full(*self.UPDATE).returncode, 6)
        self.set_scenario(connection_lost=False)
        held = self.held()
        self.assertTrue(held.get("build"), "TRIGGER PRESENT: the lock records the build")
        # that setup runs on another machine and is still at work: it renewed the lock half a minute ago
        self.seed_lock(**{**held, "holder": "other-host:4242", "started": fake_now(30)})
        n = len(self.normalized_calls())
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "TRIGGER PRESENT: the second setup started no build of its own: it follows the first one's")
        self.assertTrue(self.installed().is_file(), "the second setup installed its client")
        calls = self.normalized_calls()[n:]
        puts = self.lock_puts(calls)
        self.assertTrue(puts, "the second setup writes itself into the lock")
        self.assertRegex(puts[0], rf"(?m)^parties=other-host:4242@[0-9TZ:-]+,{re.escape(socket.gethostname())}:\d+@[0-9TZ:-]+$",
                         "the lock names both setups at work, so the first one does not remove the runner under the second")
        self.assertEqual(self.runner_deletes(calls), [], "the runner stays: the setup that started the build still uses it")
        self.assertTrue(self.runner_installed())
        left = self.held()
        self.assertEqual((left.get("token"), left.get("holder"), left.get("build")), (held["token"], "other-host:4242", held["build"]), "the lock is left to the other setup")
        self.assertNotIn(socket.gethostname(), self.lock() or "", "the setup that ended took itself out of the lock")
        said = self.said(q)
        for words in ("is not removed", "other-host:4242", "follows the same build"):
            self.assertIn(words, said)
        # the other setup never comes back: 31 minutes without a renewal, and the next update of the commit removes the runner and the lock
        self.seed_lock(**{**left, "started": fake_now(1900)})
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 0, self.out(r))
        self.assertFalse(self.runner_installed())
        self.assertIsNone(self.lock())

    def test_b812_fix_the_setup_that_started_a_build_leaves_the_runner_and_the_lock_to_a_setup_that_joined_it(self) -> None:
        import socket
        self.deployed()
        self.commit_more()
        starts = len(self.deploy_starts())
        # while this setup follows its build, a setup of the same commit and inputs on another machine attaches to the build and writes itself into the lock
        self.set_scenario(on_call=[{"match": self.JOIN, "lock_join": {"party": "other-host:4242"}}])
        n = len(self.normalized_calls())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "TRIGGER PRESENT: this setup started the build")
        self.assertTrue(self.installed().is_file())
        calls = self.normalized_calls()[n:]
        self.assertTrue(any("other-host:4242@" in v for v in self.lock_puts(calls)), "TRIGGER PRESENT: the other setup was in the lock when this one renewed it")
        self.assertEqual(self.runner_deletes(calls), [], "the runner stays for the setup that still downloads its client from the runner's bucket")
        self.assertTrue(self.runner_installed())
        left = self.lock_fields()
        self.assertEqual(left.get("holder"), "other-host:4242", "the lock is left to the setup that joined")
        self.assertRegex(left.get("token", ""), r"^[0-9a-f]{32}$")
        self.assertNotIn(socket.gethostname(), self.lock() or "")
        said = self.said(p)
        for words in ("is not removed", "other-host:4242", "follows the same build"):
            self.assertIn(words, said)

    def test_b812_fix_a_joined_setup_that_stopped_renewing_or_has_ended_does_not_keep_the_runner(self) -> None:
        import socket
        self.deployed()
        self.commit_more()
        # a setup that joined and has not renewed the lock for 31 minutes is gone
        self.set_scenario(on_call=[{"match": self.JOIN, "lock_join": {"party": "other-host:4242", "ago": 1900}}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("event-0", self.world().get("faults_seen", {}), "TRIGGER PRESENT: the other setup wrote itself into the lock")
        self.assertFalse(self.runner_installed())
        self.assertIsNone(self.lock())
        # so is one of this machine whose process has ended
        self.commit_more("MORE.md")
        self.set_scenario(on_call=[{"match": "unused"}, {"match": self.JOIN, "lock_join": {"party": f"{socket.gethostname()}:999999"}}])
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertIn("event-1", self.world().get("faults_seen", {}), "TRIGGER PRESENT")
        self.assertFalse(self.runner_installed())
        self.assertIsNone(self.lock())

    def test_b812_fix_a_lock_stays_held_while_any_setup_under_it_is_at_work(self) -> None:
        import socket
        first = self.deployed()
        self.commit_more()
        w = self.open_world()
        w.make_runner()
        w.save()
        bid = self.seed_build("SUCCEEDED", self.build_env(first))
        gone = f"{socket.gethostname()}:999999"
        # the setup that wrote the lock last has ended, and a second setup under the same lock renewed it a minute ago: the lock is not stale for a setup of other inputs
        self.seed_lock(token="ab" * 16, holder=gone, identity="x", route="runner", started=fake_now(60), commit=first, inputs="0" * 64, build=bid,
                       parties=f"{gone}@{fake_now(60)},laptop-2:77@{fake_now(60)}")
        before = len(self.mutations())
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn("laptop-2:77", self.said(p))
        self.assertEqual(len(self.mutations()), before)
        self.assertEqual(self.lock_fields().get("token"), "ab" * 16)
        # without the second setup the lock is stale at once
        self.seed_lock(token="ab" * 16, holder=gone, identity="x", route="runner", started=fake_now(60), commit=first, inputs="0" * 64, build=bid)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertIsNone(self.lock())

    def test_b812_fix_a_setup_renews_the_lock_while_it_follows_its_build(self) -> None:
        self.deployed()
        self.commit_more()
        n = len(self.normalized_calls())
        # HAMH_SETUP_LOCK_REFRESH_SECONDS=0: every look at the build is due for a renewal (600 seconds by default)
        p = self.run_full(*self.UPDATE, HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.normalized_calls()[n:]
        start = next(i for i, c in enumerate(calls) if c[:2] == ["codebuild", "start-build"])
        looks = [i for i, c in enumerate(calls) if i > start and c[:2] == ["codebuild", "batch-get-builds"] and any("currentPhase" in a for a in c)]
        self.assertGreaterEqual(len(looks), 2, "TRIGGER PRESENT: the run looked at its build more than once")
        between = [c for c in calls[looks[0]:looks[-1]] if c[:2] == ["ssm", "put-parameter"] and fake.LOCK_PARAM in c and "--overwrite" in c]
        self.assertTrue(between, "the lock got a new start time between the looks at the build, so a setup that joins a long build finds its holder at work")
        self.assertRegex(between[0][between[0].index("--value") + 1], r"(?m)^build=hamh-setup-runner-build:")
        # by default nothing is renewed in a build this short: the calls of a run stay what they were
        self.commit_more("MORE.md")
        n = len(self.normalized_calls())
        self.assertEqual(self.run_full(*self.UPDATE).returncode, 0)
        calls = self.normalized_calls()[n:]
        start = next(i for i, c in enumerate(calls) if c[:2] == ["codebuild", "start-build"])
        looks = [i for i, c in enumerate(calls) if i > start and c[:2] == ["codebuild", "batch-get-builds"] and any("currentPhase" in a for a in c)]
        self.assertEqual([c for c in calls[looks[0]:looks[-1]] if c[:2] == ["ssm", "put-parameter"] and fake.LOCK_PARAM in c], [])

    def test_b812_fix_a_teardown_waits_for_a_setup_whose_build_succeeded_and_that_still_installs(self) -> None:
        first = self.deployed()
        w = self.open_world()
        w.make_runner()
        w.save()
        bid = self.seed_build("SUCCEEDED", self.build_env(first))
        # the build of an update succeeded a minute ago: its setup downloads the client from the runner's bucket and has not removed its runner yet
        lock = dict(token="ab" * 16, holder="laptop-2:77", identity=f"arn:aws:iam::{self.ACCOUNT}:user/someone-else", route="runner", commit=first, inputs="0" * 64, build=bid)
        self.seed_lock(**lock, started=fake_now(60))
        before = len(self.mutations())
        t = self.run_full(*self.TEAR)
        self.assertEqual(t.returncode, 7, self.out(t))
        said = self.said(t)
        for words in ("laptop-2:77", bid, "still installing", "Nothing is deleted"):
            self.assertIn(words, said)
        self.assertEqual(len(self.mutations()), before, "nothing is deleted under a setup that still uses the runner")
        self.assertTrue(self.runner_installed())
        self.assertEqual(self.lock_fields().get("token"), "ab" * 16)
        # 31 minutes without a renewal: that setup is gone, and the teardown goes on
        self.seed_lock(**lock, started=fake_now(1900))
        u = self.run_full(*self.TEAR)
        self.assertNotEqual(u.returncode, 7, self.out(u))
        self.assertNotIn("still installing", self.said(u))
        self.assertGreater(len(self.mutations()), before, "TRIGGER PRESENT: the teardown deleted")
        self.assertNotIn(fake.LOCK_PARAM, self.world().get("params", {}), "the teardown removes the lock nobody renews")


class U24SameCommitCase(RunnerUpdateBase):
    """U24: the stage already runs the commit: the update builds only the client, ignores active runs and changes no stage resource."""

    def test_b812_same_commit_skips_the_deploy_and_ignores_active_runs(self) -> None:
        self.set_scenario(nine_stacks=True)
        head = self.deployed()
        cfg = self.deploy_starts()[0]["SETUP_CONFIG_SHA256"]
        self.seed_record(status="deployed", commit=head, tree="f" * 64, version="5.13", accounts=cfg, operator="x", azs="use1-az1", updated=NOW)
        deploys_before = self.world().get("n_deploys", 0)
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the stage already runs this commit", p.stdout + p.stderr)
        env = self.deploy_starts()[-1]
        self.assertEqual(env.get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.world().get("n_deploys", 0), deploys_before, "no stage resource changes: the build deployed nothing")
        self.assertTrue(self.installed().is_file())
        # --force-deploy deploys anyway, so the active runs stop it
        q = self.run_full(*self.UPDATE, "--force-deploy")
        self.assertEqual(q.returncode, 7, self.out(q))
        # the runner half: SETUP_CLIENT_ONLY skips the deploy, gomod and scanner-rules phases and still builds the client
        rig = rb.Rig(self)
        self.assertEqual(rig.run().returncode, 0, "the first build deploys")
        rig.mark_deployed()
        deploys = len(rig.calls("deploy"))
        rig.build_id = "hamh-setup-runner:99999999-2222-3333-4444-555555555556"
        proc = rig.run(CODEBUILD_BUILD_ID=rig.build_id, SETUP_CLIENT_ONLY="1")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), deploys, "the client-only build runs no deploy")
        states = {line["name"]: line["state"] for line in rig.phase_lines(rig.build_id)}
        self.assertEqual((states["deploy"], states["client"]), ("skipped", "ok"))
        self.assertEqual([c["argv"][0] for c in rig.calls("gomod")][-1:], ["check"], "gomod still checks the store (and fills one that an earlier build left unfinished)")
        self.assertIn(states["gomod"], ("ok", "skipped"))
        other = rb.Rig(self)
        proc = other.run(SETUP_CLIENT_ONLY="1")
        self.assertNotEqual(proc.returncode, 0, "a client-only build on a stage that does not run this tree refuses")
        self.assertEqual(other.result()["failedPhase"], "refused")
        self.assertEqual(other.calls("deploy"), [])


class U28RefusedCase(RunnerUpdateBase):
    """U28: a deploy refused before any stack change says the stage is unchanged, names the action and removes the runner."""

    def test_b812_a_deploy_refused_before_any_stack_change_says_unchanged_and_removes_the_runner(self) -> None:
        self.deployed()
        self.commit_more()
        action = "The CDK bootstrap of the account is version 21; this release needs 30. Run cdk bootstrap with this release, then ./setup.sh --update."
        self.set_scenario(build={"result": "fail", "failed_phase": "refused", "detail": f"reason=bootstrap wait=no stage=unchanged :: {action}"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        flat = self.flat(p)
        self.assertIn("The stage is unchanged.", flat)
        self.assertIn(action, flat)
        self.assertNotIn("partial deployment changes", flat)
        self.assertFalse(self.runner_installed())
        self.assertIsNone(self.lock())
        self.set_scenario(build={"result": "fail", "failed_phase": "refused", "detail": "reason=lock wait=yes stage=unchanged :: Another deploy holds the stage; wait for it."})
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn("The stage is unchanged.", self.flat(q))
        # the runner half: deploy.py's DEPLOY-REFUSED line (exit 3) is the phase refused with the reason, the wait word and the sentence
        rig = rb.Rig(self)
        proc = rig.run(FAKE_DEPLOY="refused")
        self.assertNotEqual(proc.returncode, 0)
        res = rig.result()
        self.assertEqual(res["failedPhase"], "refused")
        self.assertTrue(res["detail"].startswith("reason=bootstrap wait=no stage=unchanged :: "), res["detail"])
        # a deploy that fails after its first stack change still says the stage may hold partial changes
        self.set_scenario(build={"result": "fail", "failed_phase": "deploy", "detail": "DEPLOY-FAIL deploy cdk deploy failed"})
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 5, self.out(r))
        self.assertIn("partial deployment changes", self.flat(r))


class U29ResumeCase(RunnerUpdateBase):
    """U29: a failed read after the build started is exit 6 and keeps the lock; the next update finishes from the same build. No interruption leads to a second build."""

    def test_b812_a_read_failure_or_interruption_after_the_build_started_resumes_the_same_build(self) -> None:
        self.deployed()
        head = self.commit_more()
        starts = len(self.deploy_starts())
        self.set_scenario(faults=[{"match": "result.json", "exit": 255, "times": 1, "stderr": 'Could not connect to the endpoint URL: "https://s3.us-east-1.amazonaws.com/"'}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertNotIn("wrote no result", self.out(p))
        self.assertTrue(self.runner_installed(), "the runner stays for the next run")
        self.assertTrue(self.lock_fields().get("build"), "the lock keeps the build")
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the next run finishes from the same build")
        self.assertIsNone(self.lock())
        # a setup stopped between taking the lock and start-build: the build it started carries the lock's token and is attached to, never started twice
        self.commit_more("AGAIN.md")
        head = self.commit()
        token = "56" * 16
        bid = self.seed_running(head, SETUP_DEPLOY_LOCK_TOKEN=token)
        self.seed_lock(token=token, holder="other-host:1", identity="x", route="runner", started=fake_now(), commit=head, inputs=inputs_digest(), build="")
        starts = len(self.deploy_starts())
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 0, self.out(r))
        self.assertEqual(len(self.deploy_starts()), starts, f"attached to {bid}, no second build")
        # one that was stopped before start-build on this machine: its process is gone, so the lock is stale and one build runs
        self.commit_more("ONCE.md")
        head = self.commit()
        import socket
        self.seed_lock(token="78" * 16, holder=f"{socket.gethostname()}:999999", identity="x", route="runner", started=fake_now(), commit=head, inputs=inputs_digest(),
                       build="")
        s = self.run_full(*self.UPDATE)
        self.assertEqual(s.returncode, 0, self.out(s))
        self.assertEqual(len(self.deploy_starts()), starts + 1)


class U30AttachCase(RunnerUpdateBase):
    """U30 [ok]: an interrupted follow of an update leaves the build running; the next update attaches to it."""

    def test_b812_an_interrupted_follow_attaches_to_the_same_build(self) -> None:
        self.deployed()
        self.commit_more()
        starts = len(self.deploy_starts())
        self.set_scenario(connection_lost=True)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertIn("keeps running", self.out(p))
        self.assertEqual(len(self.deploy_starts()), starts + 1)
        self.set_scenario(connection_lost=False)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the second run attaches")
        self.assertIn("ttach", q.stdout)


class U33InterruptedDeployCase(RunnerUpdateBase):
    """U33: a build that ended while it changed the stage is named as the user's own on the next run, which waits for the stack and deploys again."""

    def test_b812_a_build_that_ended_mid_deploy_is_named_as_the_users_own_on_the_next_run(self) -> None:
        self.set_scenario(nine_stacks=True)
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"final": "TIMED_OUT", "result": "none", "failed_phase": "deploy"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        flat = self.flat(p)
        self.assertIn("while it was changing the stage", flat)
        self.assertIn("./setup.sh --status", flat)
        self.assertEqual(self.lock_fields().get("mark"), "deploy-interrupted")
        w = self.open_world()
        w.w["stacks"]["hamh-direct-runtimes"].update(status="UPDATE_ROLLBACK_IN_PROGRESS", polls_left=2, then="UPDATE_ROLLBACK_COMPLETE")
        w.save()
        self.set_scenario(build={}, list_settles=True)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        flat = self.said(q)
        self.assertIn("while it was changing the stage", flat)
        self.assertNotIn("another operation", flat)
        self.assertIsNone(self.lock())


class U34RetryCase(RunnerUpdateBase):
    """U34: a retry after a failure after the deploy is not stopped by active runs: the stage already runs the commit."""

    def test_b812_a_retry_after_a_post_deploy_failure_is_not_stopped_by_active_runs(self) -> None:
        self.set_scenario(nine_stacks=True)
        self.deployed()
        head = self.commit_more()
        self.set_scenario(build={"result": "fail", "failed_phase": "client", "detail": "build-client.sh exit 1: cargo zigbuild failed"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        cfg = self.deploy_starts()[-1]["SETUP_CONFIG_SHA256"]
        # what deploy.py recorded when its deploy of the failed build finished
        self.seed_record(status="deployed", commit=head, tree="f" * 64, accounts=cfg, updated=NOW)
        self.set_scenario(build={}, executions={"hamh-direct-run": RUNS})
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")
        self.assertTrue(self.installed().is_file())


class U36LeftRunnerCase(RunnerUpdateBase):
    """U36: a runner left after a successful update is removed by a command that only removes it, never by a rebuild."""

    def test_b812_a_runner_left_after_a_successful_update_is_removed_without_a_rebuild(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(runner_delete_fails=True)
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("RETAINED", p.stdout)
        self.set_scenario(runner_delete_fails=False)
        d = self.run_full(*self.UPDATE, "--dry-run")
        flat = self.said(d)
        self.assertNotIn("Run ./setup.sh --update to finish", flat)
        self.assertIn("./setup.sh --full", flat)
        starts = len(self.deploy_starts())
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts, "removing the runner builds nothing")
        self.assertFalse(self.runner_installed())


class U39BeforeBuildCase(RunnerUpdateBase):
    """U39: an update that fails after the runner install and before the build starts removes the runner, says the stage is unchanged and names the quota or permission."""

    def test_b812_an_update_that_fails_before_the_build_starts_removes_the_runner_it_installed(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(faults=[{"match": "codebuild start-build", "exit": 254, "times": 1,
                                   "stderr": "An error occurred (AccountLimitExceededException) when calling the StartBuild operation: Cannot have more than 0 builds in queue for the account"}])
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        flat = self.flat(p)
        self.assertIn("The stage is unchanged", flat)
        self.assertIn("quota", flat)
        self.assertFalse(self.runner_installed(), "the runner this run installed is removed")
        self.assertIsNone(self.lock())
        self.set_scenario(faults=[{"match": "codebuild start-build", "exit": 254, "times": 1, "after": 1,
                                   "stderr": f"An error occurred (AccessDeniedException) when calling the StartBuild operation: User: arn:aws:iam::{self.ACCOUNT}:user/setup-test is not authorized to perform: codebuild:StartBuild on resource: hamh-setup-runner-build"}])
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 5, self.out(q))
        self.assertIn("codebuild:StartBuild", self.flat(q))
        self.assertFalse(self.runner_installed())


class A23StageUnchangedCase(RunnerUpdateBase):
    """Addendum A2.3 (U28): a deploy step that failed before deploy.py wrote status=deploying left the stage unchanged; a failure after the deploy leaves the stage on the new commit."""

    def test_b812_a_deploy_step_that_failed_before_any_stack_change_says_the_stage_is_unchanged(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"result": "fail", "failed_phase": "deploy", "detail": "synth cdk synth failed: Error: Cannot find module aws-cdk-lib",
                                 "result_extra": {"stageUnchanged": True, "failedStep": "synth"}})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertIn("The stage is unchanged.", said)
        self.assertIn("synth", said)
        self.assertNotIn("partial deployment changes", said)
        self.assertFalse(self.runner_installed(), "the runner of the update is removed")
        self.assertIsNone(self.lock())
        # the runner half: DEPLOY-FAIL <step> followed by DEPLOY-STAGE unchanged, and a refusal, are written into result.json as stageUnchanged and failedStep
        rig = rb.Rig(self)
        rig.run(FAKE_DEPLOY="stage-unchanged")
        res = rig.result()
        self.assertEqual((res["failedPhase"], res.get("stageUnchanged"), res.get("failedStep")), ("deploy", True, "synth"))
        self.assertTrue(res["detail"].startswith("synth "), res["detail"])
        refused = rb.Rig(self)
        refused.run(FAKE_DEPLOY="refused")
        res = refused.result()
        self.assertEqual((res["failedPhase"], res.get("stageUnchanged"), res.get("failedStep")), ("refused", True, "bootstrap"))
        failed = rb.Rig(self)
        failed.run(FAKE_DEPLOY="fail")
        res = failed.result()
        self.assertEqual((res["failedPhase"], res.get("stageUnchanged"), res.get("failedStep")), ("deploy", False, "stack"))
        passed = rb.Rig(self)
        self.assertEqual(passed.run().returncode, 0)
        self.assertEqual((passed.result().get("stageUnchanged"), passed.result().get("failedStep")), (False, None))

    def test_b812_a_failure_after_the_deploy_says_the_stage_runs_the_commit_and_names_the_step(self) -> None:
        self.deployed()
        head = self.commit_more()
        self.set_scenario(build={"result": "fail", "failed_phase": "gomod", "detail": "GOMOD-FILL FAIL account 999999999999"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertIn(f"The stage runs commit {head}", said)
        self.assertIn("only the step gomod failed", said)
        self.assertNotIn("partial deployment changes", said)
        self.assertTrue(self.runner_installed(), "the runner stays for the retry")
        self.assertIsNone(self.lock())


class A25StatusTeardownCase(RunnerUpdateBase):
    """Addendum A2.5: --status and --teardown read the deploy lock, and the teardown removes the lock and the record of the stage."""

    TEAR = ("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", fake.ACCOUNT)

    def test_b812_status_and_teardown_read_the_deploy_lock(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(connection_lost=True)
        self.assertEqual(self.run_full(*self.UPDATE).returncode, 6)
        self.set_scenario(connection_lost=False, build={"hang": True})
        st = self.run_full("--status", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(st.returncode, 6, self.out(st))
        self.assertIn("IN_PROGRESS", st.stdout)
        before = len(self.mutations())
        t = self.run_full(*self.TEAR)
        self.assertEqual(t.returncode, 7, self.out(t))
        self.assertIn("a setup is running", self.said(t))
        self.assertEqual(len(self.mutations()), before, "nothing is deleted under a running setup")
        # a lock that names no build yet: a setup is about to start one
        self.seed_lock(token="9a" * 16, holder="other-host:5", identity="x", route="runner", started=fake_now(), commit="0" * 40, inputs="0" * 64, build="")
        u = self.run_full(*self.TEAR)
        self.assertEqual(u.returncode, 7, self.out(u))
        self.assertIn("deploy lock", self.said(u))

    def test_b812_teardown_removes_the_lock_and_the_settings_record(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, nine_stacks=True, stage_settings={"status": "deployed", "commit": "a" * 40, "set": {"HAMH_TOOLS_MODE": "on"}},
                          deploy_lock=f"token={'bc' * 16}\nholder=old-host:1\nidentity=x\nroute=runner\nstarted=2026-11-01T00:00:00Z\ncommit={'a' * 40}\ninputs={'0' * 64}\nbuild=")
        w = self.open_world()
        w.save()
        self.assertIn(fake.SETTINGS_PARAM, self.world()["params"])
        t = self.run_full(*self.TEAR)
        self.assertIn(t.returncode, (0, 8, 9), self.out(t))
        params = self.world().get("params", {})
        self.assertNotIn(fake.LOCK_PARAM, params, "the teardown removes the deploy lock")
        self.assertNotIn(fake.SETTINGS_PARAM, params, "the teardown removes the record of the stage")


class ShellHelpersCase(unittest.TestCase):
    """The shell helper of runner.sh that stands for scripts/lib/digests.py: the source tree of a commit. (The base64 of the explicit settings is setup_base64 of lib.sh;
    test_setup_update_integration.py holds it.)"""

    RUNNER = fake.REPO / "scripts" / "setup" / "runner.sh"
    LIB = fake.REPO / "scripts" / "setup" / "lib.sh"
    # sources lib.sh and runner.sh and calls the function named by the third argument
    DRIVER = '. "$1"; . "$2"; "$3"'

    def call(self, function: str, **env: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", "-c", self.DRIVER, "driver", str(self.LIB), str(self.RUNNER), function], capture_output=True, text=True,  # nosec B603: argument list, no shell string of test data, program under test that is a repository script; B607: bare name of bash looked up on PATH, as the scripts under test do
                              env={**os.environ, **env}, timeout=limit(60))

    def digests(self):
        sys.path.insert(0, str(fake.REPO / "scripts" / "lib"))
        try:
            import digests
            return digests
        finally:
            sys.path.pop(0)

    def test_the_deployable_paths_are_those_of_digests(self) -> None:
        m = re.search(r'^RN_DEPLOYABLE_PATHS="([^"]+)"$', self.RUNNER.read_text(), re.M)
        self.assertIsNotNone(m)
        self.assertEqual(tuple(m.group(1).split()), self.digests().DEPLOYABLE_PATHS)
        self.assertEqual(fake.DEPLOYABLE_PATHS, self.digests().DEPLOYABLE_PATHS, "the stand-in's build hashes the same paths")

    def test_the_stand_in_makes_the_source_tree_of_an_archive_as_git_does(self) -> None:
        # the stand-in's build writes tree= of the stage's record from the archive alone; it must be the digest git gives for the commit (files, a program, a link, folders)
        td = tempfile.TemporaryDirectory(prefix="b812-tree-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(td.cleanup)
        repo = Path(td.name) / "repo"
        for rel, text in (("infra/a.yaml", "a: 1\n"), ("infra/bin/run.sh", "#!/bin/sh\n"), ("infra/bin.txt", "sorts before the folder bin\n"), ("crates/x/y/z.rs", "fn main() {}\n"),
                          ("fixtures/policy/p.json", "{}\n"), ("fixtures/other/o.json", "{}\n"), ("accounts.json", "{}\n"), ("README.md", "outside the deployable paths\n")):
            (repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (repo / rel).write_text(text)
        (repo / "infra/bin/run.sh").chmod(0o755)
        (repo / "engine").mkdir()
        (repo / "engine/link").symlink_to("../infra/a.yaml")
        env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@test.invalid", GIT_COMMITTER_NAME="t",
                   GIT_COMMITTER_EMAIL="t@test.invalid")
        for argv in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "tree"], ["archive", "--format=zip", "-o", str(Path(td.name) / "a.zip"), "HEAD"]):
            p = subprocess.run(["git", "-C", str(repo), *argv], capture_output=True, text=True, env=env, timeout=limit(60))  # nosec B603: git with an argument list, no shell; B607: bare name of git looked up on PATH, as the scripts under test do
            self.assertEqual(p.returncode, 0, p.stderr)
        files = fake.archive_files((Path(td.name) / "a.zip").read_bytes())
        self.assertEqual(files["infra/bin/run.sh"][0], "100755")
        self.assertEqual(files["engine/link"][0], "120000")
        self.assertEqual(fake.source_tree_of(files), self.digests().source_tree(repo))
        self.assertIsNone(fake.git_tree_id(files, "statemachines"))

    def test_the_target_tree_is_the_source_tree_that_deploy_records(self) -> None:
        head = subprocess.run(["git", "-C", str(fake.REPO), "rev-parse", "HEAD"], capture_output=True, text=True, timeout=limit(60)).stdout.strip()  # nosec B603: git with an argument list, no shell; B607: bare name of git looked up on PATH, as the scripts under test do
        p = self.call("rn_target_tree", SELF_DIR=str(fake.REPO), SRC_COMMIT=head)
        self.assertEqual(p.returncode, 0, p.stderr)
        d = self.digests()
        self.assertEqual(p.stdout.strip(), d._hash_paths(fake.REPO, d.DEPLOYABLE_PATHS, head))
        self.assertEqual(p.stdout.strip(), d.source_tree(fake.REPO), "HEAD of this checkout")


if __name__ == "__main__":
    unittest.main()
