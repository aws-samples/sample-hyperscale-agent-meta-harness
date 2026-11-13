"""Where the four lanes meet: the wiring of `./setup.sh --update` between the settings collector (setup), the runner route, the local route and the deploy engine.

The rows of the inventory have their tests in `test_setup_deploy_settings.py` (the deploy engine), `test_setup_update_scenarios.py` (setup) and `test_setup_update_runner.py`
(the runner and the lock). These tests hold the joints between them:

- one chain for the explicit settings (CONTRACT C4): what the user exported is collected and checked once (`pf_explicit_settings`), shown in the plan, sent to the runner's build
  as the one override `SETUP_EXPLICIT_SETTINGS`, decoded by `runner-build.sh` and exported to `deploy.sh`; the local route exports the same lines into its deploy. Nothing the
  caller's shell holds under a `SETUP_*` name counts.
- the deploy lock on the local route (C6): read before the question, taken after it, its token handed to `deploy.sh`, released at every end.
- the build never skips a deploy that carries settings or `--force-deploy` (C7).
- the stage of a release before the record keeps the zone of its network (Addendum 3, A3.2).

They run the real `setup.sh` from a stand-in checkout against the stand-in AWS CLI, and `runner-build.sh` in the rig of `test_setup_runner_build.py`.
"""

from __future__ import annotations

import base64
import hashlib
import subprocess  # nosec B404: the tests start bash with argument lists, never a shell string
import unittest
from pathlib import Path

import setup_fake_aws as fake
from setup_waits import limit
import test_setup_runner_build as rb
from test_setup_update_guard import RUNS
from test_setup_update_runner import NOW, RunnerUpdateBase, fake_now, inputs_digest
import test_setup_deploy_settings as ds
import test_setup_full_flow as flow
from test_setup_update_scenarios import REGISTRY, ScenarioBase

REPO = Path(__file__).resolve().parents[2]
REAL_REGISTRY = REPO / "scripts" / "lib" / "stage-settings.tsv"
LIB = REPO / "scripts" / "setup" / "lib.sh"
RUNNER = REPO / "scripts" / "setup" / "runner.sh"
PREFLIGHT = REPO / "scripts" / "setup" / "preflight.sh"
# what a caller's shell may hold from an earlier experiment: none of it is a setting of this run
INHERITED = {"SETUP_EXPLICIT_SETTINGS": base64.b64encode(b"HAMH_TOOLS_MODE=off\n").decode(), "SETUP_EXPLICIT_LINES": "HAMH_UTILITY_MODEL=off"}
# the deploy stand-in of the local route, which also notes the deploy lock it runs under
LOCK_NOTE = ('{ aws ssm get-parameter --name /hamh/direct/deploy-lock --query Parameter.Value --output text 2>/dev/null || echo NO-LOCK; echo --; } >> "$STUB_LOG/deploy-lock.log"\n'
             'case "${FAKE_DEPLOY:-pass}" in')
LOCAL_DEPLOY = ScenarioBase.DEPLOY.replace('case "${FAKE_DEPLOY:-pass}" in', LOCK_NOTE, 1)
assert LOCAL_DEPLOY != ScenarioBase.DEPLOY


def decoded(env: dict) -> str:
    return base64.b64decode(env.get("SETUP_EXPLICIT_SETTINGS", "")).decode()


class IntBase(ScenarioBase, RunnerUpdateBase):
    """The helpers of the setup rows and of the runner rows together."""

    DEPLOY = LOCAL_DEPLOY

    def local_stage(self) -> None:
        self.stage()
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()

    def locks_seen(self) -> list:
        """The deploy lock each run of the local deploy stand-in found, as a dict of its lines (None: no lock)."""
        out, cur = [], []
        for ln in self.log_lines("deploy-lock.log"):
            if ln == "--":
                out.append(None if cur == ["NO-LOCK"] else fake.parse_record("\n".join(cur)))
                cur = []
            else:
                cur.append(ln)
        return out

    def deploy_runs(self) -> int:
        return len(self.log_lines("deploy.log"))


class SettingsChainCase(IntBase):
    """C4: one chain from the user's shell to the environment of the deploy, on both routes."""

    def test_b812_int_an_exported_log_retention_reaches_the_deploy_of_the_runner_build(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        p = self.run_full(*self.UPDATE, HAMH_LOG_RETENTION_DAYS="30", **INHERITED)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("HAMH_LOG_RETENTION_DAYS=30 (explicit", plan)
        for name in ("HAMH_TOOLS_MODE", "HAMH_UTILITY_MODEL"):
            self.assertNotIn(f"{name}=off", plan, "a SETUP_* variable of the caller's shell is no setting of this run")
        env = self.deploy_starts()[-1]
        self.assertEqual(decoded(env), "HAMH_LOG_RETENTION_DAYS=30\n", "exactly the exported setting, in one override")
        self.assertNotIn("\n", env["SETUP_EXPLICIT_SETTINGS"])
        self.assertNotIn("SETUP_EXPLICIT_LINES", env, "the lines travel only as the one base64 override")
        self.assertRegex(env.get("SETUP_DEPLOY_LOCK_TOKEN", ""), r"^[0-9a-f]{32}$")
        # the other half of the route: runner-build.sh, given the very override setup.sh sent, hands the setting and the lock's token to deploy.sh
        rig = rb.Rig(self)
        (rig.tree / "scripts/lib/stage-settings.tsv").write_text(REGISTRY)
        rig.put_source()
        proc = rig.run(SETUP_EXPLICIT_SETTINGS=env["SETUP_EXPLICIT_SETTINGS"], SETUP_DEPLOY_LOCK_TOKEN=env["SETUP_DEPLOY_LOCK_TOKEN"], SETUP_EXPLICIT_LINES="HAMH_UTILITY_MODEL=off")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        got = rig.calls("deploy")[0]["env"]
        self.assertEqual((got.get("HAMH_LOG_RETENTION_DAYS"), got.get("HAMH_DEPLOY_LOCK_TOKEN")), ("30", env["SETUP_DEPLOY_LOCK_TOKEN"]))
        self.assertIsNone(got.get("HAMH_UTILITY_MODEL"), "a SETUP_EXPLICIT_LINES in the build's environment sets nothing")

    def test_b812_int_an_exported_log_retention_reaches_the_local_deploy(self) -> None:
        self.local_stage()
        p = self.run_local(HAMH_LOG_RETENTION_DAYS="30", **INHERITED)
        self.assertEqual(p.returncode, 0, self.out(p))
        got = self.deploy_env()[-1]
        self.assertEqual(got.get("HAMH_LOG_RETENTION_DAYS"), "30")
        for name in ("HAMH_TOOLS_MODE", "HAMH_UTILITY_MODEL"):
            self.assertNotIn(name, got, "a SETUP_* variable of the caller's shell is no setting of this run")
        self.assertIn("HAMH_LOG_RETENTION_DAYS=30 (explicit", self.plan(p))
        self.assertNotIn("=off", " ".join(ln for ln in p.stdout.splitlines() if "setting:" in ln))

    def test_b812_int_a_setting_the_caller_only_inherited_reaches_neither_route(self) -> None:
        # no HAMH_* setting is exported: the inherited SETUP_* variables alone must leave the build and the local deploy without a setting
        self.with_registry()
        self.deployed()
        self.commit_more()
        p = self.run_full(*self.UPDATE, **INHERITED)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("SETUP_EXPLICIT_SETTINGS", self.deploy_starts()[-1])
        self.assertNotIn("(explicit", self.plan(p))

    def test_b812_int_force_deploy_and_client_only_are_this_runs_own(self) -> None:
        self.set_scenario(nine_stacks=True)
        # a first setup whose shell holds both words: its build is an ordinary deploy build
        first = self.run_full(*self.ARGS, SETUP_CLIENT_ONLY="1", SETUP_FORCE_DEPLOY="1")
        self.assertEqual(first.returncode, 0, self.out(first))
        env = self.deploy_starts()[0]
        self.assertNotIn("SETUP_CLIENT_ONLY", env)
        self.assertNotIn("SETUP_FORCE_DEPLOY", env)
        head = self.commit()
        RunnerUpdateBase.seed_record(self, status="deployed", commit=head, tree="f" * 64, version="5.13", accounts=env["SETUP_CONFIG_SHA256"], operator="x", azs="use1-az1",
                                     updated="2026-11-10T16:00:00Z")
        # the stage already runs the commit: an inherited SETUP_FORCE_DEPLOY does not turn the client-only build into a deploy
        p = self.run_full(*self.UPDATE, SETUP_FORCE_DEPLOY="1")
        self.assertEqual(p.returncode, 0, self.out(p))
        env = self.deploy_starts()[-1]
        self.assertEqual(env.get("SETUP_CLIENT_ONLY"), "1")
        self.assertNotIn("SETUP_FORCE_DEPLOY", env)
        # --force-deploy is the way: the build deploys and is told not to skip its deploy phase
        q = self.run_full(*self.UPDATE, "--force-deploy")
        self.assertEqual(q.returncode, 0, self.out(q))
        env = self.deploy_starts()[-1]
        self.assertEqual(env.get("SETUP_FORCE_DEPLOY"), "1")
        self.assertNotIn("SETUP_CLIENT_ONLY", env)

    def test_b812_int_one_encoder_and_one_collector(self) -> None:
        runner, preflight = RUNNER.read_text(), PREFLIGHT.read_text()
        self.assertNotIn("rn_settings_b64", runner, "lib.sh has the one encoder (setup_base64)")
        self.assertNotRegex(runner, r"(?m)^SETUP_EXPLICIT_LINES=\$\{SETUP_EXPLICIT_LINES", "runner.sh takes no lines from the environment")
        self.assertRegex(LIB.read_text(), r"(?m)^unset SETUP_EXPLICIT_LINES SETUP_EXPLICIT_SETTINGS\b", "the chain starts from nothing")
        self.assertNotRegex(preflight + runner + (REPO / "scripts/setup/full.sh").read_text(), r"(?<![A-Z_])EXPLICIT_LINES", "one name for the lines: SETUP_EXPLICIT_LINES")
        driver = '. "$1"; setup_base64'
        for lines in ("HAMH_COMPUTE_INSTANCES=on\n", "HAMH_LOG_RETENTION_DAYS=30\nHAMH_TOOLS_MODE=off\nHAMH_UTILITY_PURPOSES=run-names,tags\n", "AB\n", "A=b\tc\n"):
            with self.subTest(lines=lines):
                p = subprocess.run(["bash", "-c", driver, "driver", str(LIB)], input=lines, capture_output=True, text=True, timeout=limit(60))  # nosec B603: bash with an argument list, no shell, the text is test data on stdin; B607: bash looked up on PATH, as the scripts under test use it
                self.assertEqual((p.returncode, p.stdout), (0, base64.b64encode(lines.encode()).decode()), p.stderr)
        p = subprocess.run(["bash", "-c", driver, "driver", str(LIB)], input="HAMH_X=café\n", capture_output=True, text=True, timeout=limit(60))  # nosec B603: bash with an argument list, no shell, as above; B607: bash looked up on PATH, as above
        self.assertNotEqual(p.returncode, 0, "only printable ASCII is encoded")


class BuildSkipCase(unittest.TestCase):
    """C7 in the build: `runner-build.sh` skips a deploy an earlier build finished, but never one that carries settings or --force-deploy."""

    def passed_once(self) -> rb.Rig:
        rig = rb.Rig(self)
        (rig.tree / "scripts/lib/stage-settings.tsv").write_text(REGISTRY)
        rig.put_source()
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        rig.mark_deployed()
        return rig

    def second(self, rig: rb.Rig, **extra: str) -> dict:
        rig.build_id = "hamh-setup-runner:99999999-2222-3333-4444-555555555557"
        proc = rig.run(CODEBUILD_BUILD_ID=rig.build_id, **extra)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return {line["name"]: line["state"] for line in rig.phase_lines(rig.build_id)}

    def test_b812_int_a_repeated_build_without_settings_still_skips_its_deploy(self) -> None:
        rig = self.passed_once()
        self.assertEqual(self.second(rig)["deploy"], "skipped", "TRIGGER PRESENT: the same commit and account file, and the account says they are deployed")
        self.assertEqual(len(rig.calls("deploy")), 1)

    def test_b812_int_a_build_with_explicit_settings_runs_its_deploy(self) -> None:
        rig = self.passed_once()
        states = self.second(rig, SETUP_EXPLICIT_SETTINGS=base64.b64encode(b"HAMH_LOG_RETENTION_DAYS=30\n").decode())
        self.assertEqual(states["deploy"], "ok", "the setting has to reach deploy.sh")
        self.assertEqual(rig.calls("deploy")[-1]["env"].get("HAMH_LOG_RETENTION_DAYS"), "30")
        self.assertEqual(len(rig.calls("deploy")), 2)

    def test_b812_int_a_build_with_force_deploy_runs_its_deploy(self) -> None:
        rig = self.passed_once()
        self.assertEqual(self.second(rig, SETUP_FORCE_DEPLOY="1")["deploy"], "ok")
        self.assertEqual(len(rig.calls("deploy")), 2)
        bad = rb.Rig(self)
        proc = bad.run(SETUP_FORCE_DEPLOY="yes")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(bad.result()["failedPhase"], "inputs")


class LocalLockCase(IntBase):
    """C6 on the local route: the lock is read before the question, taken after it, handed to deploy.sh and released at every end."""

    def test_b812_int_the_local_route_holds_the_deploy_lock_for_its_deploy_and_releases_it(self) -> None:
        self.local_stage()
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        seen = self.locks_seen()
        self.assertEqual(len(seen), 1)
        self.assertIsNotNone(seen[0], "the deploy runs under the lock")
        self.assertEqual(seen[0].get("route"), "local")
        self.assertEqual(seen[0].get("commit"), self.commit())
        token = self.deploy_env()[-1].get("HAMH_DEPLOY_LOCK_TOKEN", "")
        self.assertRegex(token, r"^[0-9a-f]{32}$")
        self.assertEqual(seen[0].get("token"), token, "deploy.sh gets the token of the lock setup holds")
        self.assertIsNone(self.lock(), "the lock is released when the deploy has ended")
        puts = [c for c in self.normalized_calls() if c[:2] == ["ssm", "put-parameter"] and fake.LOCK_PARAM in c]
        self.assertEqual(len(puts), 1)
        self.assertNotIn("--overwrite", puts[0], "the lock is created, never overwritten into being")

    def test_b812_int_the_local_route_releases_the_lock_when_the_deploy_fails_or_refuses(self) -> None:
        self.local_stage()
        for answer, code in (("fail", 5), ("refused-wait", 7), ("refused", 5), ("before-change", 5), ("guard", 7)):
            with self.subTest(deploy=answer):
                p = self.run_local(FAKE_DEPLOY=answer)
                self.assertEqual(p.returncode, code, self.out(p))
                self.assertIsNotNone(self.locks_seen()[-1], "the deploy ran under the lock")
                self.assertIsNone(self.lock(), "and the lock is released")

    def test_b812_int_the_local_route_stops_before_the_question_when_another_setup_holds_the_lock(self) -> None:
        self.local_stage()
        before = len(self.mutations())
        self.seed_lock(token="cd" * 16, holder="other-host:4242", identity=f"arn:aws:iam::{self.ACCOUNT}:user/someone-else", route="runner", started=fake_now(),
                       commit="b" * 40, inputs="0" * 64, build="")
        p = self.run_local()
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = self.flat(p)
        for words in ("other-host:4242", "Nothing was changed."):
            self.assertIn(words, flat)
        self.assertEqual(len(self.mutations()), before, "nothing is created before the refusal")
        self.assertEqual(self.deploy_runs(), 0, "scripts/deploy.sh did not start")
        self.assertEqual(self.lock_fields().get("token"), "cd" * 16, "the holder keeps its lock")
        # the build of a runner-route setup is running: the local route starts no deploy beside it
        head = self.commit()
        bid = self.seed_running(head)
        self.seed_lock(token="ab" * 16, holder="laptop-2:77", identity=f"arn:aws:iam::{self.ACCOUNT}:user/someone-else", route="runner", started=fake_now(), commit=head,
                       inputs="0" * 64, build=bid)
        q = self.run_local()
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn(bid, self.out(q))
        self.assertEqual(self.deploy_runs(), 0)
        self.assertEqual(self.lock_fields().get("token"), "ab" * 16)

    def test_b812_int_the_local_route_never_attaches_to_a_runner_build_of_the_same_inputs(self) -> None:
        self.local_stage()
        first = self.run_local()
        self.assertEqual(first.returncode, 0, self.out(first))
        runs = self.deploy_runs()
        # the account file of this local update and no setting: the inputs a runner-route lock of the same profile name would record
        config = hashlib.sha256((self.receipt_dir() / "accounts-local.json").read_bytes()).hexdigest()
        head = self.commit()
        bid = self.seed_running(head)
        self.seed_lock(token="ab" * 16, holder="laptop-2:77", identity=f"arn:aws:iam::{self.ACCOUNT}:user/setup-test", route="runner", started=fake_now(), commit=head,
                       inputs=inputs_digest(config), build=bid)
        p = self.run_local()
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn(bid, self.out(p))
        self.assertIn("--build-on local", self.flat(p))
        self.assertEqual(self.deploy_runs(), runs, "scripts/deploy.sh did not start a second deploy beside the build")
        self.assertEqual(self.lock_fields().get("token"), "ab" * 16, "the lock of the running build stays its own")

    def test_b812_int_the_local_route_replaces_a_stale_lock(self) -> None:
        self.local_stage()
        self.seed_lock(token="34" * 16, holder="gone-host:1", identity="x", route="local", started=fake_now(7200), commit="b" * 40, inputs="0" * 64, build="")
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.locks_seen()[-1].get("token"), self.deploy_env()[-1].get("HAMH_DEPLOY_LOCK_TOKEN"), "the deploy ran under this run's own lock")
        self.assertIsNone(self.lock())

    def test_b812_int_a_long_local_deploy_renews_its_lock(self) -> None:
        # a lock that names no build counts as stale after 30 minutes, and a deploy on this machine can take longer: the holder renews it while the deploy prints
        self.local_stage()
        p = self.run_local(HAMH_SETUP_LOCK_REFRESH_SECONDS="0")
        self.assertEqual(p.returncode, 0, self.out(p))
        renewals = [c for c in self.normalized_calls() if c[:2] == ["ssm", "put-parameter"] and fake.LOCK_PARAM in c and "--overwrite" in c]
        self.assertGreaterEqual(len(renewals), 1, "the holder overwrites its own lock with a new start time")
        self.assertIsNone(self.lock(), "and still releases it at the end")


class LocalSameDeploymentCase(IntBase):
    """C7 on the local route: the stage already runs the commit, so scripts/deploy.sh is not run and active runs do not stop the update."""

    def test_b812_int_a_local_update_of_the_commit_the_stage_runs_builds_only_the_client(self) -> None:
        self.stage(nine_stacks=True)
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()
        first = self.run_local()
        self.assertEqual(first.returncode, 0, self.out(first))
        runs = self.deploy_runs()
        self.assertEqual(runs, 1)
        config = hashlib.sha256((self.receipt_dir() / "accounts-local.json").read_bytes()).hexdigest()
        RunnerUpdateBase.seed_record(self, status="deployed", commit=self.commit(), tree="f" * 64, version="5.13", accounts=config, operator="x", azs="use1-az1",
                                     updated="2026-11-10T16:00:00Z")
        # what the deploy wrote beside the record: the source tree and the account file the stage itself names as deployed
        self.seed_live("f" * 64, config)
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        cargo = len(self.log_lines("cargo.log"))
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the stage already runs this commit", p.stdout + p.stderr)
        self.assertEqual(self.deploy_runs(), runs, "TRIGGER PRESENT: the record names this commit and this account file; scripts/deploy.sh is not run again")
        self.assertGreater(len(self.log_lines("cargo.log")), cargo, "the client is still built here")
        # --force-deploy deploys anyway: the active runs stop it, and without them the deploy runs
        q = self.run_local("--force-deploy")
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertEqual(self.deploy_runs(), runs)
        self.set_scenario(executions={})
        r = self.run_local("--force-deploy")
        self.assertEqual(r.returncode, 0, self.out(r))
        self.assertEqual(self.deploy_runs(), runs + 1)

    def seed_live(self, tree: str, config: str) -> None:
        w = self.open_world()
        w.put_param(fake.SOURCE_TREE_PARAM, tree, dict(fake.PROJECT_TAGS))
        w.put_param(fake.CONFIG_DIGEST_PARAM, config, dict(fake.PROJECT_TAGS))
        w.save()

    def test_b812_fix_a_local_update_deploys_when_the_stage_no_longer_runs_what_its_record_names(self) -> None:
        # Addendum 4, A4.9 (F10): a deploy of an earlier release, which writes no record, changed the stage after the record was written. The record still says deployed
        # for this tree; the parameters the stage itself keeps say another tree, and the local route has no build that would notice.
        self.stage(nine_stacks=True)
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()
        self.assertEqual(self.run_local().returncode, 0)
        runs = self.deploy_runs()
        config = hashlib.sha256((self.receipt_dir() / "accounts-local.json").read_bytes()).hexdigest()
        RunnerUpdateBase.seed_record(self, status="deployed", commit=self.commit(), tree="f" * 64, version="5.14", accounts=config, operator="x", azs="use1-az1",
                                     updated="2026-11-10T16:00:00Z")
        for name, tree, cfg in (("another source tree", "e" * 64, config), ("another account file", "f" * 64, "d" * 64)):
            with self.subTest(live=name):
                self.seed_live(tree, cfg)
                p = self.run_local()
                self.assertEqual(p.returncode, 0, self.out(p))
                runs += 1
                self.assertEqual(self.deploy_runs(), runs, "scripts/deploy.sh runs: the stage does not run what its record names")
                self.assertNotIn("the stage already runs this commit", p.stdout)
        reads = [c for c in self.normalized_calls() if c[:2] == ["ssm", "get-parameter"] and (fake.SOURCE_TREE_PARAM in c or fake.CONFIG_DIGEST_PARAM in c)]
        self.assertTrue(reads, "the two parameters are read")
        # control: the stage runs what the record names, so only the client is built
        self.seed_live("f" * 64, config)
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.deploy_runs(), runs)
        self.assertIn("the stage already runs this commit", p.stdout + p.stderr)


class RecordCommitCase(IntBase):
    """Addendum 4, A4.1 (F5, F17): the record of a deploy on the runner route names the commit the user asked for, so the next update is judged against a commit the
    clone holds; the stage that already runs the target is found before the commit order is looked at, and by its source tree when the commits differ."""

    def release_stage(self) -> str:
        """A stage that a full setup of this release deployed on the runner route: the build of a commit that holds the registry wrote the stage's record."""
        self.set_scenario(nine_stacks=True)
        self.with_registry(REAL_REGISTRY.read_text())
        head = self.deployed()
        rec = self.open_world().stage_record()
        self.assertIsNotNone(rec, "TRIGGER PRESENT: the deploy of the build wrote /hamh/direct/settings")
        self.assertEqual(rec["status"], "deployed")
        return head

    def deploys(self) -> int:
        return self.world().get("n_deploys", 0)

    def test_b812_fix_the_record_of_a_runner_deploy_names_the_commit_of_the_clone(self) -> None:
        head = self.release_stage()
        rec = self.open_world().stage_record()
        self.assertEqual(rec["commit"], head, "the commit the user asked for, not the commit the build made of the archive")
        import sys
        sys.path.insert(0, str(REPO / "scripts" / "lib"))
        try:
            import digests
        finally:
            sys.path.pop(0)
        self.assertEqual(rec["tree"], digests._hash_paths(self.repo, digests.DEPLOYABLE_PATHS, head), "the source tree deploy.py records for that commit")
        self.assertEqual(rec["accounts"], self.deploy_starts()[0]["SETUP_CONFIG_SHA256"])
        self.assertEqual(rec["operator"], f"arn:aws:iam::{self.ACCOUNT}:user/setup-test")
        self.assertEqual(rec["azs"], "use1-az1")
        params = self.world()["params"]
        self.assertEqual((params[fake.SOURCE_TREE_PARAM], params[fake.CONFIG_DIGEST_PARAM]), (rec["tree"], rec["accounts"]))

    def test_b812_fix_a_second_update_of_a_newer_commit_deploys_it(self) -> None:
        self.release_stage()
        first = self.write_commit({"infra/first-change.yaml": "a: 1\n"}, "a change the stage deploys")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.open_world().stage_record()["commit"], first)
        deploys = self.deploys()
        second = self.write_commit({"infra/second-change.yaml": "a: 2\n"}, "a newer change")
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertNotIn("is not in this clone", self.out(q))
        env = self.deploy_starts()[-1]
        self.assertEqual(env["SETUP_SOURCE_KEY"], f"source/{second}.zip")
        self.assertNotIn("SETUP_CLIENT_ONLY", env, "a commit with another source tree is deployed")
        self.assertEqual(self.deploys(), deploys + 1)
        self.assertIn(f"commit: {first[:12]} -> {second[:12]}", self.plan(q))
        self.assertEqual(self.open_world().stage_record()["commit"], second)
        self.assertFalse(self.runner_installed())

    def test_b812_fix_an_update_of_the_commit_the_stage_runs_builds_only_the_client(self) -> None:
        self.release_stage()
        deploys = self.deploys()
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the stage already runs this commit", p.stdout + p.stderr)
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.deploys(), deploys, "no stage resource changes")
        # a commit that changes nothing the stage deploys (a document) has the same source tree: the client alone is built for it
        self.commit_more("NOTES.md")
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.deploys(), deploys)

    def test_b812_fix_a_retry_after_a_post_deploy_failure_does_not_deploy_again(self) -> None:
        self.release_stage()
        head = self.write_commit({"infra/change.yaml": "a: 1\n"}, "a change the stage deploys")
        deploys = self.deploys()
        self.set_scenario(build={"result": "fail", "failed_phase": "client", "detail": "build-client.sh exit 1: cargo zigbuild failed"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("the retry does not deploy it again", self.said(p))
        self.assertEqual(self.deploys(), deploys + 1, "TRIGGER PRESENT: the deploy of the failed build finished")
        self.assertEqual(self.open_world().stage_record()["commit"], head)
        self.set_scenario(build={}, executions={"hamh-direct-run": RUNS})
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.deploys(), deploys + 1, "the retry deploys nothing")
        self.assertTrue(self.installed().is_file())

    def test_b812_fix_a_record_whose_commit_no_clone_holds_is_judged_by_its_source_tree(self) -> None:
        # the record a build wrote before it handed the user's commit to the deploy: commit= is the commit the build made of the archive
        self.set_scenario(record_commit="synthetic")
        head = self.release_stage()
        rec = self.open_world().stage_record()
        self.assertNotEqual(rec["commit"], head, "TRIGGER PRESENT: the recorded commit is not in the clone")
        deploys = self.deploys()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("the stage already runs this commit", p.stdout + p.stderr)
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.deploys(), deploys)
        # another source tree: git cannot place the recorded commit, so the update stops before anything is created and names git pull and --revision (C8)
        newer = self.write_commit({"infra/change.yaml": "a: 1\n"}, "a change the stage deploys")
        before = len(self.mutations())
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn("is not in this clone", self.flat(q))
        self.assertEqual(len(self.mutations()), before)
        r = self.run_full(*self.UPDATE, "--revision", newer)
        self.assertEqual(r.returncode, 0, self.out(r))
        self.assertNotIn("SETUP_CLIENT_ONLY", self.deploy_starts()[-1])


class SourceCommitSeamCase(IntBase):
    """Addendum 4, A4.1, the joint of its two sides. runner-build.sh hands the commit the user asked for to scripts/deploy.sh as HAMH_SOURCE_COMMIT, scripts/lib/deploy.py
    writes it as commit= of the stage's record, and the next `./setup.sh --update` reads that record. Here the record is the one deploy.py itself writes, on the stand-ins of
    test_setup_deploy_settings.py, in a tree whose commits no clone of the user holds (as the build of the setup runner deploys from a source archive); setup.sh then reads
    those bytes, unchanged, from the stand-in AWS CLI of the setup tests. The two stand-ins play the same Region and zone and different accounts (the stand-in build of the
    deploy engine accepts only the accounts of its fixture), so the operator the record names is another identity than the caller of the setup stand-in: the updates here name
    --change-operator, which leaves the record as deploy.py wrote it."""

    UPDATE_AS = (*IntBase.UPDATE, "--change-operator")

    def deploy_writes_record(self, **env: str) -> tuple:
        """One deploy of scripts/lib/deploy.py with an account file as setup generates it (StageShapeBase: one entry with role active and the caller as operator): the text of
        /hamh/direct/settings it wrote at its end, and the HEAD of the tree it deployed."""
        engine = ds.StageShapeBase("deploy")  # any attribute names the case: it is used as a tool here and runs no test of its own
        engine.setUp()
        self.addCleanup(engine.doCleanups)
        proc = engine.ok(**env)
        puts = engine.puts(ds.SETTINGS)
        self.assertEqual(len(puts), 2, f"the deploying record and the deployed record: {proc.stdout}")
        return puts[-1][puts[-1].index("--value") + 1], ds.td.git(engine.root, "rev-parse", "HEAD")

    def stage_record_is(self, text: str) -> None:
        w = self.open_world()
        w.put_param(fake.SETTINGS_PARAM, text, dict(fake.PROJECT_TAGS))
        w.save()

    def in_clone(self, commit: str) -> bool:
        return subprocess.run(["git", "-C", str(self.repo), "cat-file", "-e", f"{commit}^{{commit}}"], capture_output=True, timeout=limit(60)).returncode == 0  # nosec B603: git with an argument list, no shell; B607: bare name of git looked up on PATH, as the scripts under test do

    def test_b812_fix_the_record_deploy_py_writes_for_the_runner_is_the_one_setup_judges_the_next_update_by(self) -> None:
        self.set_scenario(nine_stacks=True)
        self.with_registry(REAL_REGISTRY.read_text())
        first = self.deployed()
        # the build of the setup runner: deploy.py runs in the tree of the source archive and is given the commit the user asked for
        text, archive_head = self.deploy_writes_record(HAMH_SOURCE_COMMIT=first)
        rec = fake.parse_record(text)
        self.assertFalse(self.in_clone(archive_head), "TRIGGER PRESENT: the tree deploy.py deployed has a commit of its own, which the user's clone does not hold")
        self.assertEqual(rec["commit"], first, "commit= of the record is the value of HAMH_SOURCE_COMMIT")
        self.assertEqual((rec["status"], rec["schema"]), ("deployed", "1"))
        self.assertEqual((rec["operator"], rec["azs"]), (f"arn:aws:iam::{ds.THIRD}:role/Admin", "use1-az1"), "the operator of the deploy engine's stand-in, the zone of both")
        self.assertEqual(set(rec["set"]), {"HAMH_KEEP_TAG", "HAMH_STAGE_KIND"}, "the two settings every deploy records")
        # the stage holds those bytes, and the user updates to a later commit of the same clone
        self.stage_record_is(text)
        newer = self.write_commit({"infra/change.yaml": "a: 1\n"}, "a change the stage deploys")
        deploys = self.world().get("n_deploys", 0)
        p = self.run_full(*self.UPDATE_AS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("is not in this clone", self.out(p))
        self.assertIn(f"commit: {first[:12]} (VERSION {rec['version']}) -> {newer[:12]}", self.plan(p), "setup placed the recorded commit, with the VERSION deploy.py recorded, against the target")
        env = self.deploy_starts()[-1]
        self.assertEqual(env["SETUP_SOURCE_KEY"], f"source/{newer}.zip")
        self.assertNotIn("SETUP_CLIENT_ONLY", env, "the later commit is deployed")
        self.assertEqual(self.world().get("n_deploys", 0), deploys + 1)
        # the control: without the variable deploy.py records the HEAD of the tree it deployed, and setup cannot place that commit against any target (C8)
        text, archive_head = self.deploy_writes_record()
        self.assertEqual(fake.parse_record(text)["commit"], archive_head)
        self.stage_record_is(text)
        last = self.write_commit({"infra/change.yaml": "a: 2\n"}, "one more change")
        before = len(self.mutations())
        q = self.run_full(*self.UPDATE_AS)
        self.assertEqual(q.returncode, 7, self.out(q))
        self.assertIn(f"runs commit {archive_head}", self.flat(q))
        self.assertIn("is not in this clone", self.flat(q))
        self.assertIn(f"--revision {last}", self.flat(q))
        self.assertEqual(len(self.mutations()), before, "nothing is created")


class PlanLinesCase(IntBase):
    """Addendum 4, A4.14 (F3, F4, F19): what the plan of an update says before the question."""

    DRY = ("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")

    def names_line(self, p) -> str:
        lines = [" ".join(ln.split()) for ln in p.stdout.splitlines() if "run names:" in ln]
        self.assertEqual(len(lines), 1, p.stdout)
        return lines[0]

    def test_b812_fix_the_plan_says_that_a_stale_deploy_lock_is_replaced(self) -> None:
        self.deployed()
        self.commit_more()
        self.seed_lock(token="34" * 16, holder="gone-host:1", identity="x", route="manual", started=fake_now(7200), commit="b" * 40, inputs="0" * 64, build="")
        p = self.run_full(*self.DRY)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan(p)
        self.assertIn("deploy lock: stale", plan)
        for needle in ("gone-host:1", "older than 30 minutes", "replaced"):
            self.assertIn(needle, plan)
        self.assertEqual(self.lock_fields().get("token"), "34" * 16, "a dry run replaces nothing")
        # without a lock the plan has no such line
        w = self.open_world()
        w.w["params"].pop(fake.LOCK_PARAM)
        w.save()
        p = self.run_full(*self.DRY)
        self.assertNotIn("deploy lock:", self.plan(p))

    def test_b812_fix_the_run_names_line_shows_the_setting_that_applies(self) -> None:
        self.with_registry()
        flow.FullRunNamesPlanCase.with_catalog(self)
        self.deployed()
        self.commit_more()
        self.drop_record()
        # a stage without a record: the deploy keeps what the live stage has, and the plan does not claim to know it
        line = self.names_line(self.run_full(*self.DRY))
        self.assertIn("kept as the live stage has them", line)
        self.assertNotIn("--build-on local", line)
        # the record keeps off
        self.seed_record(HAMH_UTILITY_MODEL="off")
        line = self.names_line(self.run_full(*self.DRY))
        self.assertIn("off: HAMH_UTILITY_MODEL=off (kept: the stage's record)", line)
        # an exported value wins, on the runner route as on the local one
        line = self.names_line(self.run_full(*self.DRY, HAMH_UTILITY_MODEL="haiku"))
        self.assertIn("on:", line)
        self.assertIn("HAMH_UTILITY_MODEL=haiku (exported in this shell)", line)
        self.seed_record()
        line = self.names_line(self.run_full(*self.DRY, HAMH_UTILITY_MODEL="off"))
        self.assertIn("off: HAMH_UTILITY_MODEL=off (exported in this shell)", line)
        # the default of a stage whose record names no model: on in this Region, and the way to turn it off works on either route
        line = self.names_line(self.run_full(*self.DRY))
        self.assertIn("on: Claude Haiku 5.5 writes a short name and tags for each run in this Region", line)
        self.assertIn("HAMH_UTILITY_MODEL=off", line)
        self.assertNotIn("--build-on local", line)

    def test_b812_fix_a_stage_without_a_record_gets_no_image_and_commit_comparison_and_the_plan_says_so(self) -> None:
        self.with_registry()
        self.deployed()
        self.commit_more()
        self.drop_record()
        plan = self.plan(self.run_full(*self.DRY))
        self.assertIn("image: not compared", plan)
        self.assertIn("the stage has no record of the commit it runs", plan)
        self.assertIn("plus about 21 to 27 minutes when the binary must be rebuilt", plan)
        self.assertNotIn("commit: ", plan)
        # with a record whose commit is in the clone the plan compares (the stand-in checkout has no pins, so it says why it cannot)
        self.seed_record()
        plan = self.plan(self.run_full(*self.DRY))
        self.assertNotIn("the stage has no record of the commit it runs", plan)


class RetryHintCase(IntBase):
    """U34, the setup half (lane C ask 3): the active-runs refusal of a retry names --allow-active-runs when the last update of this commit failed after its deploy."""

    def test_b812_int_the_active_runs_refusal_names_the_switch_after_a_post_deploy_failure(self) -> None:
        self.set_scenario(nine_stacks=True)
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"result": "fail", "failed_phase": "client", "detail": "build-client.sh exit 1: cargo zigbuild failed"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        # the stage has no record that names this commit (the commit has no settings registry: a deploy that writes none), so the retry deploys again and the runs stop it
        self.assertIsNone(self.open_world().stage_record(), "TRIGGER PRESENT: no record, so the same-deployment skip does not apply")
        self.set_scenario(build={}, executions={"hamh-direct-run": RUNS})
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 7, self.out(q))
        flat = self.flat(q)
        self.assertIn("--allow-active-runs", flat)
        self.assertIn("client", flat)
        # a retry after a failure in the deploy itself: the switch is not named, because that deploy may change the worker runtime
        self.commit_more("OTHER.md")
        self.set_scenario(executions={}, build={"result": "fail", "failed_phase": "deploy", "detail": "DEPLOY-FAIL deploy cdk deploy failed"})
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 5, self.out(r))
        self.set_scenario(build={}, executions={"hamh-direct-run": RUNS})
        s = self.run_full(*self.UPDATE)
        self.assertEqual(s.returncode, 7, self.out(s))
        self.assertNotIn("--allow-active-runs", self.flat(s))


class ResumeClientCase(IntBase):
    """U29, the setup half (lane C ask 3): a client that was downloaded and checked but not installed is installed by the next update of the same commit without a build, and
    the endpoint is in the client's config before the runner is removed."""

    def test_b812_int_a_repeated_update_installs_the_client_it_downloaded_without_a_build(self) -> None:
        self.set_scenario(nine_stacks=True)
        self.deployed()
        head = self.commit_more()
        # the client is downloaded and checked, then `ham models` fails here, so the new client is not installed
        p = self.run_full(*self.UPDATE, FAKE_MODELS_RC="1")
        self.assertNotEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.receipt_dir() / "download" / "verified" / "SHA256SUMS").is_file(), "TRIGGER PRESENT: the checked client is kept in the receipt folder")
        cfg = self.deploy_starts()[-1]["SETUP_CONFIG_SHA256"]
        RunnerUpdateBase.seed_record(self, status="deployed", commit=head, tree="f" * 64, accounts=cfg, updated=NOW)
        starts = len(self.deploy_starts())
        # a dry run first: it says the checked copy is used again, and it installs nothing, so it does not say installed
        d = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(d.returncode, 0, self.out(d))
        self.assertIn("downloaded and checked earlier for this commit", self.said(d))
        self.assertNotRegex(self.said(d), r"checked earlier for this commit \(sha256 [0-9a-f]+\) is installed", "a dry run installs nothing")
        self.assertEqual(len(self.deploy_starts()), starts)
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts, "no build: the copy downloaded for this commit is installed")
        self.assertIn("downloaded and checked earlier", self.said(q))
        self.assertFalse(self.runner_installed())
        # a newer commit that the stage also runs: the kept copy is of the earlier commit, so the update builds the client again
        newer = self.commit_more("NEWER.md")
        RunnerUpdateBase.seed_record(self, status="deployed", commit=newer, tree="f" * 64, accounts=cfg, updated=NOW)
        r = self.run_full(*self.UPDATE)
        self.assertEqual(r.returncode, 0, self.out(r))
        self.assertEqual(len(self.deploy_starts()), starts + 1)
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")

    def test_b812_int_the_endpoint_is_in_the_config_before_the_runner_is_removed(self) -> None:
        self.deployed()
        self.commit_more()
        cfg = self.config_file()
        if cfg.is_file():
            cfg.write_text("")
        self.set_scenario(wait_seconds={"hamh-setup-runner": 60})
        before = len(self.normalized_calls())  # the first setup removed its runner too
        proc = self.popen(*self.UPDATE)

        def removing() -> bool:
            try:
                return any(c[:3] == ["cloudformation", "wait", "stack-delete-complete"] and "hamh-setup-runner" in c for c in self.normalized_calls()[before:])
            except ValueError:  # the stand-in was writing its log line while it was read
                return False

        proc.wait_ready(removing, 240, "the removal of the runner")
        text = cfg.read_text() if cfg.is_file() else ""
        self.assertIn(self.ENDPOINT, text, "the client points at the stage before the runner goes, so a setup stopped meanwhile leaves a configured client")
        proc.close()  # the stand-in waits 60 s on the delete; the test ends the group


class RevisionCase(IntBase):
    """U2, the setup half (lane C ask 3): a --revision whose runner-build.sh cannot carry the exported settings is refused before anything is created. The runner contract
    number of names.sh (U31) is the guard: a build that reads SETUP_EXPLICIT_SETTINGS came with contract 1, and a revision without it has no contract number."""

    def test_b812_int_a_revision_whose_build_cannot_carry_the_settings_is_refused(self) -> None:
        self.with_registry()
        self.deployed()
        build = self.repo / "scripts" / "setup" / "runner-build.sh"
        names = self.repo / "scripts" / "setup" / "names.sh"
        b_text, n_text = build.read_text(), names.read_text()
        self.assertIn("SETUP_EXPLICIT_SETTINGS", b_text, "the build of this clone carries the settings")
        self.assertRegex(n_text, r"(?m)^HAMH_RUNNER_CONTRACT=1$", "under runner contract 1")
        old = self.write_commit({"scripts/setup/runner-build.sh": "\n".join(ln for ln in b_text.splitlines() if "SETUP_EXPLICIT_SETTINGS" not in ln) + "\n",
                                 "scripts/setup/names.sh": "\n".join(ln for ln in n_text.splitlines() if not ln.startswith("HAMH_RUNNER_CONTRACT=")) + "\n"},
                                "a build from before the settings")
        self.write_commit({"scripts/setup/runner-build.sh": b_text, "scripts/setup/names.sh": n_text}, "this release again")
        starts, before = len(self.deploy_starts()), len(self.mutations())
        p = self.run_full(*self.UPDATE, "--revision", old, HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn(f"git checkout {old}", p.stderr)
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        self.assertEqual(len(self.deploy_starts()), starts, "no build is started")


if __name__ == "__main__":
    unittest.main()
