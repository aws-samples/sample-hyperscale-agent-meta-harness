"""`./setup.sh --update --allow-active-runs --allow-new-runs` lets the stage keep starting runs while the update deploys.

The stage's API service starts no new runs while a deploy holds the lock `/hamh/<stage>/deploy-lock`. That refusal is the default of an update, and
this option is its way off, for one update: setup writes the line `starts=yes` into every text of the lock it holds, and the API service starts runs under a
lock with that exact line (crates/control-api `admission`, tests/b841_starts.rs). The option goes with `--allow-active-runs`, because a run that starts during
the update is a run that is active under it; the plan says what that means for such a run. A run can then start at any moment of the update, so the look
that counts the active runs decides nothing about the worker runtime: an update that needs a new worker image is refused before anything is created, and
the deploy refuses every new worker runtime version under such a lock.

The line of the lock decides, not the options of the run that reads it. The option is no part of the `inputs` the lock records, so a rerun without it (the
command setup prints for the way on, another user's `ham update` of the same commit) finds the build of such an update, attaches to it or finishes from it and
never starts a second deploy beside it. A run that attaches keeps the line it found and never adds one: the deploy of a lock without the line makes worker
runtime versions that a run started under it would be pinned to. What such a run says about new runs is read from the lock.

The tests run `setup.sh` from a stand-in checkout against the stand-in AWS CLI (setup_fake_aws.py), as the other `test_setup_*` suites do.
"""

from __future__ import annotations

import hashlib
import re
import socket
import subprocess  # nosec B404: the tests start bash with an argument list, never a shell string
import unittest
from pathlib import Path

from setup_waits import limit
from test_setup_update_guard import RUNS, UpdateBase
from test_setup_update_runner import RunnerUpdateBase, inputs_digest
from test_setup_update_scenarios import ScenarioBase

REPO = Path(__file__).resolve().parents[2]
LOCK = "/hamh/direct/deploy-lock"
BOTH = ("--allow-active-runs", "--allow-new-runs")


def lock_lines(text: str) -> dict:
    return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


def function_body(text: str, name: str) -> str:
    m = re.search(r"^" + re.escape(name) + r"\(\) \{\n(.*?)^\}", text, re.M | re.S)
    if m is None:
        raise AssertionError(f"no function {name}")
    return m.group(1)


class NewRunsLockCase(RunnerUpdateBase):
    """The lock of an update that lets runs start."""

    def lock_writes(self) -> list:
        return [t for t in self.world().get("lock_writes", [])]

    def plan_text(self, p) -> str:
        return " ".join(p.stdout.split())

    def test_b841_starts_every_write_of_the_lock_says_starts_yes_while_the_update_deploys(self) -> None:
        self.deployed()
        self.commit_more()
        before = len(self.lock_writes())
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()[before:]
        self.assertGreaterEqual(len(writes), 2, "the create and at least one write after it")
        print("B841-STARTS TRIGGER PRESENT: the first write is the lock of a deploy (no deploys=no line):", "deploys=no" not in writes[0].splitlines())
        self.assertNotIn("deploys=no", writes[0].splitlines(), "the update deploys: its lock would close the stage")
        for n, text in enumerate(writes):
            self.assertIn("starts=yes", text.splitlines(), f"write {n} of the lock: {text!r}")
        self.assertIsNone(self.world().get("params", {}).get(LOCK), "the lock is given back at the end")

    def test_b841_starts_an_update_without_the_option_writes_no_such_line(self) -> None:
        self.deployed()
        self.commit_more()
        before = len(self.lock_writes())
        p = self.run_full(*self.UPDATE, "--allow-active-runs")
        self.assertEqual(p.returncode, 0, self.out(p))
        writes = self.lock_writes()[before:]
        self.assertTrue(writes)
        for text in writes:
            self.assertNotIn("starts", lock_lines(text), text)

    def test_b841_starts_the_option_is_no_part_of_the_inputs_so_a_rerun_without_it_finds_the_build(self) -> None:
        self.deployed()
        self.commit_more()
        before = len(self.lock_writes())
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 0, self.out(p))
        cfg = self.deploy_starts()[-1]["SETUP_CONFIG_SHA256"]
        plain = inputs_digest(cfg)
        with_option = hashlib.sha256(f"config={cfg}\nsettings=\nstarts=yes\n".encode()).hexdigest()
        first = lock_lines(self.lock_writes()[before])
        self.assertEqual(first.get("starts"), "yes", "TRIGGER PRESENT: the lock of this update lets runs start")
        self.assertEqual(first["inputs"], plain, "the inputs are the account file and the settings: the digest every setup of this commit and these settings computes")
        self.assertNotEqual(first["inputs"], with_option)

    def test_b841_starts_the_plan_says_that_the_stage_keeps_starting_runs_and_what_that_means_for_a_run(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(executions={"hamh-direct-run": RUNS})
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", *BOTH)
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan_text(p)
        self.assertIn("new runs: the stage keeps starting runs while this update deploys (--allow-new-runs).", plan)
        self.assertIn("A run that starts during the update runs under it like the active runs: the Lambda functions and the run state machine change under it; its worker runtime stays.", plan)
        self.assertIn("pull <runId> --snapshot downloads the latest saved copy of its work.", plan)
        self.assertIn("The build refuses every new worker runtime version, with runs active or not.", plan)
        # without the option the plan names the default and its switch
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", "--allow-active-runs")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = self.plan_text(p)
        self.assertIn("new runs: the stage starts no new runs while this update deploys; --allow-new-runs lets it keep starting them.", plan)
        self.assertNotIn("the stage keeps starting runs while this update deploys", plan)

    def test_b841_starts_the_option_is_a_usage_error_outside_an_update_and_without_allow_active_runs(self) -> None:
        base = ("--profile", "work", "--region", "us-east-1")
        needs = "--allow-new-runs goes with --allow-active-runs: a run that starts during the update is active under it"
        cases = [
            (("--full", *base, "--yes", "--acknowledge-costs", "--allow-new-runs"), "--allow-new-runs goes with --update"),
            (("--teardown", *base, "--allow-new-runs"), "--allow-new-runs goes with --update"),
            (("--status", *base, "--allow-new-runs"), "--allow-new-runs goes with --update"),
            ((*base, "--allow-new-runs"), "--allow-new-runs goes with --update"),
            (("--update", *base, "--allow-new-runs"), needs),
            (("--update", *base, "--wait-for-runs", "5", "--allow-new-runs"), needs),
            (("--update", *base, "--wait-for-runs", "5", *BOTH), "give --wait-for-runs or --allow-active-runs, not both"),
        ]
        before = len(self.mutations())
        for args, needle in cases:
            with self.subTest(args=args):
                p = self.run_full(*args)
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn(needle, p.stderr)
        self.assertEqual(len(self.mutations()), before, "a usage error changes nothing")

    def test_b841_starts_the_help_names_the_option_and_what_it_needs(self) -> None:
        text = " ".join(self.run_setup("--help").stdout.split())
        start = text.index("--update build and deploy")
        about = text[start:text.index("--revision ID", start)]
        self.assertIn("--allow-new-runs", about)
        self.assertIn("with --allow-active-runs, lets the stage keep starting runs while the update deploys", about)


class NewRunsRerunCase(RunnerUpdateBase):
    """A rerun after an update with --allow-new-runs ended early, and a run that attaches with other options than the lock was taken with."""

    KEEPS = "new runs: the stage keeps starting runs while that build deploys: the update that started it was given --allow-new-runs."
    CLOSED = ("new runs: the stage starts no new runs while that build deploys: the update that started it was not given --allow-new-runs, and a run that "
              "attaches does not change its lock.")

    def lock_writes(self) -> list:
        return [t for t in self.world().get("lock_writes", [])]

    def held(self) -> dict:
        fields = self.lock_fields()
        fields.pop("set", None)
        return fields

    def cut_off(self, *options: str):
        """An update whose follow loses its connection: exit 6, the build keeps running and the lock records it. Returns the deploy builds before it and the run."""
        self.deployed()
        self.commit_more()
        starts = len(self.deploy_starts())
        self.set_scenario(connection_lost=True)
        p = self.run_full(*self.UPDATE, *options)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the update started its build")
        self.set_scenario(connection_lost=False)
        return starts, p

    def test_b841_starts_a_rerun_without_the_option_attaches_to_the_build_of_an_update_with_it(self) -> None:
        starts, p = self.cut_off(*BOTH)
        held = self.held()
        print("B841-RERUN TRIGGER PRESENT: the lock says starts=yes and records a running build:", held.get("starts") == "yes" and bool(held.get("build")))
        self.assertEqual(held.get("starts"), "yes")
        self.assertTrue(held.get("build"))
        self.assertIn("Run ./setup.sh again with the same options to attach to it", self.said(p))
        self.assertNotIn("starts no new runs", self.said(p))
        # the plain update: what a user types who lost the options, and what `ham update` of another user of the stage runs
        before = len(self.lock_writes())
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the rerun attaches to the build: no second deploy of the stage")
        said = self.said(q)
        self.assertIn(f"a build of this commit is already running ({held['build']}); setup attaches to it instead of starting another", said)
        self.assertIn(self.KEEPS, said)
        self.assertNotIn("starts no new runs", said, "no sentence of the run that attached says that the lock closes the stage")
        writes = self.lock_writes()[before:]
        self.assertTrue(writes, "the run writes itself into the lock it follows")
        for n, text in enumerate(writes):
            self.assertIn("starts=yes", text.splitlines(), f"write {n} of the run that attached keeps the line: {text!r}")
        self.assertIsNone(self.lock(), "the lock is given back at the end")

    def test_b841_starts_a_rerun_without_the_option_finds_the_build_that_the_lock_did_not_record(self) -> None:
        starts, _ = self.cut_off(*BOTH)
        held = self.held()
        bid = held["build"]
        # what an update leaves that was stopped while start-build was under way, or whose write of the build id failed: the build runs with the lock's token, the
        # lock records no build, and the setup process of this machine that held it has ended
        self.seed_lock(**{**held, "build": "", "holder": f"{socket.gethostname()}:999999"})
        build = self.world().get("builds", {}).get(bid, {})
        print("B841-RERUN TRIGGER PRESENT: the build runs with the lock's token and the lock records none:",
              build.get("status") == "IN_PROGRESS" and build.get("env", {}).get("SETUP_DEPLOY_LOCK_TOKEN") == held["token"] and not self.held().get("build"))
        self.assertEqual((build.get("status"), build.get("env", {}).get("SETUP_DEPLOY_LOCK_TOKEN")), ("IN_PROGRESS", held["token"]))
        self.assertEqual(self.held().get("starts"), "yes")
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, f"the rerun finds {bid} by the lock's token: a second deploy never starts beside the first")
        said = self.said(q)
        self.assertNotIn("stale", said, "a lock whose build carries its token is not stale")
        # the stand-in moves the build on with every look, so the rerun follows it or reads its result: either way it is this build
        self.assertRegex(said, r"note: a build of this commit (is already running|already succeeded) \(" + re.escape(bid) + r"\)")
        self.assertNotIn("starts no new runs", said)
        self.assertIsNone(self.lock())

    def test_b841_starts_a_rerun_without_the_option_finishes_from_the_build_whose_result_could_not_be_read(self) -> None:
        self.deployed()
        self.commit_more()
        starts = len(self.deploy_starts())
        self.set_scenario(faults=[{"match": "result.json", "exit": 255, "times": 1, "stderr": 'Could not connect to the endpoint URL: "https://s3.us-east-1.amazonaws.com/"'}])
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertIn("the next run finishes from the same build without building again: run ./setup.sh --update again", self.said(p))
        held = self.held()
        self.assertEqual((held.get("starts"), held.get("deploys")), ("yes", "no"), "TRIGGER PRESENT: the lock of the update with the option stays, with the build that succeeded")
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the command setup named finishes from the same build without building again")
        self.assertIn(f"a build of this commit already succeeded ({held['build']}); setup reads its result instead of building again", self.said(q))
        self.assertIsNone(self.lock())

    def test_b841_starts_a_run_that_attaches_never_adds_the_line_to_a_lock_without_it(self) -> None:
        starts, _ = self.cut_off("--allow-active-runs")
        held = self.held()
        print("B841-RERUN TRIGGER PRESENT: the lock of the update without the option has no starts line:", "starts" not in held and bool(held.get("build")))
        self.assertNotIn("starts", held)
        before = len(self.lock_writes())
        q = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the run with the option attaches to the build that runs")
        writes = self.lock_writes()[before:]
        self.assertTrue(writes)
        for n, text in enumerate(writes):
            self.assertNotIn("starts", lock_lines(text), f"write {n}: the deploy under this lock was started without the guard of the option: {text!r}")
        said = self.said(q)
        self.assertIn(self.CLOSED, said)
        self.assertNotIn("the stage keeps starting runs", said)

    def test_b841_starts_a_run_that_attached_and_stops_following_says_what_the_lock_says(self) -> None:
        self.cut_off(*BOTH)
        # the look of the preflight goes through and every look of the follow fails: the run attaches and then loses its connection
        self.set_scenario(faults=[{"match": "builds[0].[buildStatus,currentPhase]", "exit": 255,
                                   "stderr": 'Could not connect to the endpoint URL: "https://codebuild.us-east-1.amazonaws.com/"'}])
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 6, self.out(q))
        said = self.said(q)
        self.assertIn("Attaching to the running build", said, "TRIGGER PRESENT: the run attached before it lost its connection")
        self.assertIn("Stopped following the build", said)
        self.assertIn("The deploy lock keeps the build for that run.", said)
        self.assertIn(self.KEEPS, said)
        self.assertNotIn("starts no new runs", said, "the lock says starts=yes: the stage keeps starting runs while the build deploys")
        self.assertEqual(self.held().get("starts"), "yes", "the lock keeps the line for the next run")


class NewRunsMessagesCase(unittest.TestCase):
    """What setup says about new runs when a run ends early, held to the code: a lock with the line never closes the stage."""

    RUNNER = (REPO / "scripts/setup/runner.sh").read_text(encoding="utf-8")

    def test_b841_starts_the_line_is_the_one_the_api_service_reads(self) -> None:
        self.assertIn("""[ "$RN_LOCK_STARTS" != yes ] || printf '\\nstarts=yes'""", function_body(self.RUNNER, "rn_lock_value"))
        # a run that takes a lock of its own writes the line when it was given the option; a run that adopts a lock, or writes a shared one again, keeps what the lock says
        for name in ("rn_lease_take", "rn_lease_take_soft"):
            self.assertIn('[ "${ALLOW_NEW_RUNS:-0}" != 1 ] || RN_LOCK_STARTS=yes', function_body(self.RUNNER, name), name)
        for name in ("rn_lock_adopt", "rn_lock_sync"):
            self.assertIn("rn_lock_get starts", function_body(self.RUNNER, name), name)
            self.assertNotIn("ALLOW_NEW_RUNS", function_body(self.RUNNER, name), f"{name} reads the lock, never the options of this run")
        for name in ("rn_inputs_digest", "rn_lock_value", "rn_lock_closes"):
            self.assertNotIn("ALLOW_NEW_RUNS", function_body(self.RUNNER, name), f"{name}: the option of this run decides nothing about a lock it shares")
        admission = (REPO / "crates/control-api/src/admission.rs").read_text(encoding="utf-8")
        self.assertIn('const STARTS_ALLOWED: &str = "starts=yes";', admission)
        deploy = (REPO / "scripts/lib/deploy.py").read_text(encoding="utf-8")
        self.assertIn('held.get("starts") == "yes"', deploy, "the deploy reads the same line for its guard")

    def test_b841_starts_no_sentence_says_that_the_stage_starts_no_new_runs_under_such_a_lock(self) -> None:
        closes = function_body(self.RUNNER, "rn_lock_closes")
        self.assertIn('[ "$RN_LOCK_OPEN" != 1 ] && [ "$RN_LOCK_STARTS" != yes ]', closes)
        for name in ("rn_lock_open", "rn_release_kept", "rn_lock_stays_said"):
            body = function_body(self.RUNNER, name)
            self.assertIn("starts no new runs", body, name)
            self.assertIn("rn_lock_closes", body, f"{name} says that the stage starts no new runs only for a lock that closes it")
        said = [ln for ln in self.RUNNER.splitlines() if "The stage starts no new runs while this build deploys" in ln]
        self.assertEqual(len(said), 1)
        at = self.RUNNER.splitlines().index(said[0])
        self.assertIn("rn_lock_closes", self.RUNNER.splitlines()[at - 1], "the sentence of a run that stops following its build")


class NewRunsAfterThePlanCase(unittest.TestCase):
    """A run that attaches to another setup's build after its plan was printed (that setup took the lock in the same moment): the plan named this run's own option, and
    the lock of the build decides. `rn_lock_starts_said` of runner.sh says it, only when the two differ."""

    KEEPS = "warn the stage keeps starting runs while that build deploys: the update that started it was given --allow-new-runs."
    CLOSED = ("warn --allow-new-runs does not apply: the stage starts no new runs while that build deploys. The update that started it was not given the option, and a "
              "run that attaches does not change its lock.")

    def said(self, lock_starts: str, option: str, lock_open: str = "0", legacy: str = "0") -> str:
        script = ('set -u\n. scripts/setup/names.sh\n. scripts/setup/lib.sh\n. scripts/setup/runner.sh\n'
                  'RN_LOCK_STARTS=$1 ALLOW_NEW_RUNS=$2 RN_LOCK_OPEN=$3 RN_LOCK_LEGACY=$4\nrn_lock_starts_said\n')
        p = subprocess.run(["bash", "-c", script, "bash", lock_starts, option, lock_open, legacy], capture_output=True, text=True, cwd=str(REPO), timeout=limit(60))  # nosec B603: argument list, a fixed script of this file; B607: bash on PATH as the scripts under test use it
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        return " ".join((p.stdout + p.stderr).split())

    def test_b841_starts_a_run_that_attaches_after_its_plan_says_what_the_lock_of_the_build_says(self) -> None:
        self.assertEqual(self.said("yes", "0"), self.KEEPS, "the lock lets runs start and this run was not given the option")
        self.assertEqual(self.said("", "1"), self.CLOSED, "this run was given the option and the lock it attached to has no such line")
        # the plan was right: nothing more is said
        self.assertEqual(self.said("yes", "1"), "")
        self.assertEqual(self.said("", "0"), "")
        # a lock that does not close the stage anyway (deploys=no), and the lease of an earlier release, which closes nothing
        self.assertEqual(self.said("", "1", lock_open="1"), "")
        self.assertEqual(self.said("yes", "0", lock_open="1"), "")
        self.assertEqual(self.said("", "1", legacy="1"), "")

    def test_b841_starts_the_run_that_attaches_after_its_plan_calls_it(self) -> None:
        full = (REPO / "scripts/setup/full.sh").read_text(encoding="utf-8")
        at = full.index('say_log "Another setup started the same build meanwhile; attaching to $RN_BUILD_ID"')
        after = full[at:full.index("rn_follow", at)]
        self.assertIn("rn_lock_join", after)
        self.assertIn("rn_lock_starts_said", after, "the sentence comes before the run follows the build")


class NewRunsImageCase(ScenarioBase):
    """An update that lets runs start keeps the worker runtime: a new worker image is refused before anything is created, with no run active."""

    def test_b841_starts_an_update_that_needs_a_new_worker_image_is_refused_before_anything_is_created(self) -> None:
        pin = "# image key pin\n{}\n"
        lock = '[cline]\ntag = "cli-v3.0.71"\nbinarySha256 = "{}"\n\n[opencode]\nbinarySha256 = "{}"\n'
        self.write_commit({"engine/policy/image-key.pin": pin.format("a" * 64), "engine/lock.toml": lock.format("b" * 64, "c" * 64)}, "pins")
        deployed = self.deployed()
        self.seed_record(commit=deployed)
        self.write_commit({"engine/policy/image-key.pin": pin.format("d" * 64)}, "another image")
        self.set_scenario(executions={})
        before = len(self.mutations())
        # with --allow-active-runs alone and no run active the update goes ahead to the question: the image is the build's to guard
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run", "--allow-active-runs")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("a new worker image", self.plan(p))
        print("B841-STARTS TRIGGER PRESENT: the update needs a new worker image and no run is active, so the look alone lets it through:",
              "a new worker image" in self.plan(p) and "running run(s)" not in self.plan(p))
        # with --allow-new-runs it is refused here
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(len(self.mutations()), before, "nothing is created")
        flat = self.flat(p)
        self.assertIn("a new worker image", flat)
        self.assertIn("--allow-new-runs lets runs start while it deploys", flat)
        self.assertIn("--allow-new-runs lets through only an update that keeps the worker runtime.", flat)
        self.assertIn("./setup.sh --update --wait-for-runs MINUTES", flat)
        self.assertIn("Nothing was changed.", flat)
        # an update that keeps the image goes ahead
        self.git("checkout", "-q", deployed)
        self.write_commit({"docs.md": "words\n"}, "words")
        p = self.run_full(*self.UPDATE, *BOTH)
        self.assertEqual(p.returncode, 0, self.out(p))


if __name__ == "__main__":
    unittest.main()
