"""State, recovery and the lifecycle commands of `setup.sh`.

AWS holds the authoritative state: stack outputs, the CodeBuild build and the manifest `result.json`. A lease object in the setup bucket keeps two
setups from racing. A local receipt only caches identifiers. An interrupted or repeated setup attaches to the build it recorded and never starts a second
one; losing the connection is not a failed build. `--status`, `--doctor`, `--update` and `--teardown` read or change the same state, with exit codes
0 to 3 as before and 4 to 8 as defines them.
"""

from __future__ import annotations

import hashlib
import json
import signal
import unittest

import setup_fake_aws
from setup_full_support import STUB_ADMIN, FullBase, golden_accounts

GOLDEN_EAST = golden_accounts("us-east-1")
DIGEST = hashlib.sha256(GOLDEN_EAST).hexdigest()


class StateBase(FullBase):
    def lease_json(self, commit: str, build_id: str, digest: str = DIGEST, owner: str = "someone-else:4242") -> bytes:
        return json.dumps({"schema": "1", "owner": owner, "buildId": build_id, "sourceCommit": commit, "configDigest": digest,
                           "startedAt": "2026-10-29T17:00:00Z"}).encode()

    def seed_runner(self) -> None:
        w = self.open_world()
        w.make_runner()
        w.save()

    def seed_finished_build(self, commit: str, action: str = "deploy", final: str = "SUCCEEDED") -> str:
        """A build of this checkout that ended in the account: its record, its result.json and its client files."""
        self.seed_runner()
        self.seed_object(f"config/accounts-{DIGEST[:12]}.json", GOLDEN_EAST)
        env = {"SETUP_SOURCE_KEY": f"source/{commit}.zip", "SETUP_CONFIG_KEY": f"config/accounts-{DIGEST[:12]}.json", "SETUP_STAGE": "direct",
               "SETUP_AZS": "1", "SETUP_BINARY_NAME": "ham"}
        if action != "deploy":
            env["SETUP_ACTION"] = action
        bid = self.seed_build(final, env)
        w = self.open_world()
        setup_fake_aws.Build(w, bid).finish(setup_fake_aws._build_cfg(w), self.bucket)
        w.save()
        return bid


class InterruptionCase(StateBase):
    def test_losing_the_connection_is_exit_6_and_the_next_run_attaches_to_the_same_build(self) -> None:
        self.set_scenario(connection_lost=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)
        self.assertIn("keeps running", self.out(p))
        self.assertIn("./setup.sh", self.out(p))
        self.assertFalse(self.installed().exists())
        self.assertIn("hamh-setup-runner", self.world()["stacks"], "the runner stays while its build runs")
        self.set_scenario(connection_lost=False)
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "the second run must attach, not start another build")
        self.assertIn("ttach", q.stdout)
        self.assertTrue(self.installed().is_file())

    def test_losing_the_connection_during_the_template_build_says_the_next_run_starts_it_again(self) -> None:
        """The build that prints the CDK bootstrap template takes no lease, so a second run cannot attach to it: the text must not promise that."""
        self.set_scenario(bootstrap_present=False, connection_lost=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 6, self.out(p))
        flat = " ".join(self.out(p).split())
        self.assertIn("keeps running", flat)
        self.assertIn("takes no lease", flat)
        self.assertNotIn("to attach to it", flat)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)
        self.set_scenario(connection_lost=False)
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 3, "the first template build, the template build again and the deploy build")

    def test_the_lease_and_the_receipt_name_the_build_while_it_runs(self) -> None:
        commit = self.commit()
        self.set_scenario(connection_lost=True)
        self.assertEqual(self.run_full(*self.ARGS).returncode, 6)
        bid = sorted(self.world()["builds"])[0]
        lock_text = self.world()["params"]["/hamh/direct/deploy-lock"]
        lock = setup_fake_aws.parse_record(lock_text)
        self.assertEqual((lock["build"], lock["commit"], lock["route"]), (bid, commit, "runner"))
        r = self.receipt()
        self.assertEqual((r["buildId"], r["state"], r["sourceCommit"]), (bid, "started", commit))
        # the token of the lock names this run's hold of it (no credential): it is the only line allowed the word
        for text in ("\n".join(line for line in lock_text.splitlines() if not line.startswith("token=")), self.receipt_path().read_text()):
            self.assertNotRegex(text, r"(?i)secret|token|password|AKIA")

    def test_a_fresh_machine_attaches_from_the_lease_alone(self) -> None:
        self.set_scenario(connection_lost=True)
        self.assertEqual(self.run_full(*self.ARGS).returncode, 6)
        import shutil
        shutil.rmtree(self.home / ".config" / "ham" / "setup")
        self.set_scenario(connection_lost=False)
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_ctrl_c_while_following_the_build_leaves_it_running_and_exits_6(self) -> None:
        self.set_scenario(build={"hang": True})
        proc = self.popen(*self.ARGS, HAMH_SETUP_POLL_SECONDS="1")
        self.follow_started(proc)
        done = proc.interrupt(signal.SIGINT)
        self.assertEqual(done.returncode, 6, done.output)
        self.assertIn("keeps running", done.output)
        self.assertEqual(self.count_calls("codebuild", "stop-build"), 0, "interrupting the wizard never cancels the build")
        self.set_scenario(build={})
        q = self.run_full(*self.ARGS)
        self.assertEqual(q.returncode, 0, self.out(q))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_a_poll_that_fails_once_or_twice_is_not_the_end(self) -> None:
        self.set_scenario(faults=[{"match": "codebuild batch-get-builds", "exit": 255, "stderr": "Could not connect to the endpoint URL", "times": 2}])
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)


class LeaseCase(StateBase):
    OTHER = "1" * 40

    def seeded_conflict(self, status: str = "IN_PROGRESS") -> str:
        self.seed_runner()
        env = {"SETUP_SOURCE_KEY": f"source/{self.OTHER}.zip", "SETUP_CONFIG_KEY": "config/accounts-aaaaaaaaaaaa.json", "SETUP_STAGE": "direct", "SETUP_AZS": "1",
               "SETUP_BINARY_NAME": "ham"}
        bid = self.seed_build(status, env)
        self.seed_object("lease/direct.json", self.lease_json(self.OTHER, bid, digest="a" * 64))
        return bid

    def test_another_setup_with_other_inputs_in_progress_is_exit_7_and_nothing_is_started(self) -> None:
        bid = self.seeded_conflict()
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertEqual(self.mutations(), [], "the conflict is found before anything is uploaded or deployed")
        self.assertIn(bid, p.stderr)
        self.assertIn(self.OTHER[:12], p.stderr)
        self.assertFalse(self.installed().exists())

    def test_a_lease_whose_build_has_ended_is_stale_and_is_replaced(self) -> None:
        self.seeded_conflict(status="FAILED")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_the_same_inputs_in_progress_attach_whoever_started_them(self) -> None:
        commit = self.commit()
        self.seed_runner()
        env = {"SETUP_SOURCE_KEY": f"source/{commit}.zip", "SETUP_CONFIG_KEY": f"config/accounts-{DIGEST[:12]}.json", "SETUP_STAGE": "direct", "SETUP_AZS": "1",
               "SETUP_BINARY_NAME": "ham"}
        self.seed_object(f"config/accounts-{DIGEST[:12]}.json", GOLDEN_EAST)
        bid = self.seed_build("IN_PROGRESS", env)
        self.seed_object("lease/direct.json", self.lease_json(commit, bid))
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn(bid, p.stdout)

    def test_a_finished_build_of_this_checkout_is_used_not_repeated(self) -> None:
        commit = self.commit()
        bid = self.seed_finished_build(commit)
        self.seed_object("lease/direct.json", self.lease_json(commit, bid))
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertEqual(self.installed().read_bytes(), self.client_file.read_bytes())

    def test_a_lease_whose_build_codebuild_does_not_know_is_stale(self) -> None:
        self.seed_runner()
        self.seed_object("lease/direct.json", self.lease_json(self.OTHER, "hamh-setup-runner-build:00000000-0000-4000-8000-a99999999999", digest="a" * 64))
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_a_lease_whose_build_cannot_be_looked_at_is_exit_6_and_nothing_is_changed(self) -> None:
        bid = self.seeded_conflict(status="IN_PROGRESS")
        self.set_scenario(connection_lost=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn(bid, p.stderr)

    def test_the_lease_is_taken_with_a_condition_so_two_setups_cannot_both_win(self) -> None:
        # the deploy lock is created without --overwrite: SSM refuses a second create (ParameterAlreadyExists); only its holder overwrites it, with the record of the build
        # and with a new start time while it installs the client
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        puts = [c for c in self.normalized_calls() if c[:4] == ["ssm", "put-parameter", "--name", "/hamh/direct/deploy-lock"]]
        self.assertGreaterEqual(len(puts), 2)
        self.assertNotIn("--overwrite", puts[0])
        for later in puts[1:]:
            self.assertIn("--overwrite", later)

    def test_the_lease_is_released_when_the_build_ends(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        self.assertFalse(self.s3_exists("lease/direct.json"))
        self.assertNotIn("/hamh/direct/deploy-lock", self.world().get("params", {}))


class StatusCase(StateBase):
    def status(self, *extra: str, **env: str):
        return self.run_full("--status", "--profile", "work", "--region", "us-east-1", *extra, **env)

    def test_nothing_deployed_is_exit_0_and_says_so(self) -> None:
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=not-deployed")
        self.assertEqual(self.mutations(), [])
        self.assertFalse((self.home / ".config" / "ham" / "setup").exists(), "a status check writes nothing")

    def test_a_deployed_stage_reports_the_endpoint_and_the_client(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        self.run_full("--client-only", "--yes", "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT)
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=deployed")
        self.assertIn(self.ENDPOINT, p.stdout)
        self.assertRegex(p.stdout, r"(?mi)^\s*client:.*installed")
        self.assertRegex(p.stdout, r"(?mi)^\s*client:.*endpoint matches")
        self.assertEqual(self.mutations(), [])

    def test_a_client_that_points_elsewhere_is_reported(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        self.run_full("--client-only", "--yes", "--profile", "work", "--region", "us-east-1", "--endpoint", "https://zzz999.execute-api.us-east-1.amazonaws.com/direct")
        p = self.status()
        self.assertRegex(p.stdout, r"(?mi)^\s*client:.*points (to|at) another")

    def test_status_says_where_the_operator_tool_is_or_that_it_is_missing(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        self.run_full("--client-only", "--yes", "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT)
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?mi)^\s*operator tool:\s+not installed")
        self.write_stub("ham-admin", STUB_ADMIN)
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?mi)^\s*operator tool:\s+installed at \S*ham-admin")
        self.assertEqual(self.mutations(), [])

    def test_a_running_build_is_exit_6(self) -> None:
        self.set_scenario(connection_lost=True)
        self.assertEqual(self.run_full(*self.ARGS).returncode, 6)
        self.set_scenario(connection_lost=False, build={"hang": True})
        p = self.status()
        self.assertEqual(p.returncode, 6, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=deploying")
        self.assertIn("IN_PROGRESS", p.stdout)

    def test_a_failed_last_build_is_exit_5_and_names_the_phase(self) -> None:
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "gomod", "detail": "module store fill failed"})
        self.assertEqual(self.run_full(*self.ARGS).returncode, 5)
        p = self.status()
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^STATUS .*state=failed")
        self.assertIn("gomod", p.stdout)

    def test_a_runner_that_was_kept_is_listed(self) -> None:
        self.assertEqual(self.run_full(*self.ARGS, "--keep-runner").returncode, 0)
        p = self.status()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?mi)^\s*runner:.*installed")
        self.assertIn(self.bucket, p.stdout)

    def test_a_teardown_that_leaves_only_the_agentcore_held_network_is_exit_9_and_the_status_after_it_is_exit_8(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, eni_stuck=True)
        t = self.run_full("--teardown", "--yes", "--acknowledge-teardown", "--account-id", "210987654321", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(t.returncode, 9, self.out(t))
        p = self.status()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("RETAINED", p.stdout)

    def test_a_status_needs_no_confirmation_and_no_terminal(self) -> None:
        self.assertEqual(self.status("--yes").returncode, 0)
        self.assertEqual(self.run_full("--status", "--no-input", "--profile", "work", "--region", "us-east-1").returncode, 0)


class DoctorCase(StateBase):
    def doctor(self, **env: str):
        return self.run_full("--doctor", "--profile", "work", "--region", "us-east-1", **env)

    def deployed_with_client(self) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True)
        self.ham_on_path()
        self.run_full("--client-only", "--yes", "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT)
        (self.logs / "ham.log").write_text("")

    def test_a_healthy_setup_passes_every_check_and_runs_the_clients_own_doctor(self) -> None:
        self.deployed_with_client()
        p = self.doctor()
        self.assertEqual(p.returncode, 0, self.out(p))
        for check in ("aws-cli", "identity", "stage", "endpoint", "client"):
            self.assertRegex(p.stdout, rf"(?m)^PASS {check}\b")
        self.assertNotRegex(p.stdout, r"(?m)^FAIL ")
        self.assertEqual(self.log_lines().count("doctor"), 1)
        self.assertEqual(self.mutations(), [])

    def test_the_operator_tool_is_a_warning_when_it_is_missing_and_a_pass_when_it_runs(self) -> None:
        self.deployed_with_client()
        p = self.doctor()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^WARN operator-tool\b.*not installed")
        self.assertIn("./setup.sh --update", p.stdout)
        self.write_stub("ham-admin", STUB_ADMIN)
        p = self.doctor()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^PASS operator-tool\b")
        self.assertEqual(self.log_lines("ham-admin.log"), ["--version"], "the doctor asks the tool for its version and does nothing else with it")

    def test_an_operator_tool_that_does_not_run_is_a_failed_check(self) -> None:
        self.deployed_with_client()
        self.write_stub("ham-admin", "#!/bin/sh\nexit 3\n")
        p = self.doctor()
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^FAIL operator-tool\b")

    def test_a_failing_client_doctor_is_exit_3(self) -> None:
        self.deployed_with_client()
        p = self.doctor(FAKE_DOCTOR="PASS identity ok\nFAIL api no", FAKE_DOCTOR_RC="1")
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^FAIL client\b")

    def test_a_stage_that_is_not_there_is_a_failed_check_and_exit_3(self) -> None:
        self.ham_on_path()
        p = self.doctor()
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^FAIL stage\b")

    def test_a_stack_that_is_not_complete_is_a_failed_check(self) -> None:
        self.deployed_with_client()
        w = self.open_world()
        w.w["stacks"]["hamh-direct-data"]["status"] = "UPDATE_ROLLBACK_FAILED"
        w.save()
        p = self.doctor()
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertIn("UPDATE_ROLLBACK_FAILED", p.stdout)

    def test_a_client_pointing_at_another_endpoint_is_a_failed_check(self) -> None:
        self.deployed_with_client()
        w = self.open_world()
        w.w["stacks"]["hamh-direct-control"]["outputs"]["V1ApiUrl"] = "https://zzz999.execute-api.us-east-1.amazonaws.com/direct"
        w.save()
        p = self.doctor()
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^FAIL endpoint\b")

    def test_a_kept_runner_is_a_warning_not_a_failure(self) -> None:
        self.deployed_with_client()
        self.seed_runner()
        p = self.doctor()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^WARN runner\b")


class UpdateCase(StateBase):
    def deployed(self) -> str:
        self.assertEqual(self.run_full(*self.ARGS).returncode, 0)
        return self.commit()

    UPDATE = ("--update", "--profile", "work", "--region", "us-east-1", "--yes")

    def test_an_update_builds_the_chosen_revision_and_installs_the_new_client(self) -> None:
        self.deployed()
        new = self.commit_more()
        self.client_file.write_text(self.client_file.read_text() + "# a newer client\n")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.start_build_env(1)["SETUP_SOURCE_KEY"], f"source/{new}.zip")
        self.assertNotIn("SETUP_ACTION", self.start_build_env(1))
        self.assertTrue(self.installed().read_text().endswith("# a newer client\n"))
        self.assertEqual(self.receipt()["sourceCommit"], new)

    def test_an_update_needs_no_cost_acknowledgement_but_does_need_a_yes_or_a_terminal(self) -> None:
        self.deployed()
        self.commit_more()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1")
        self.assertEqual(p.returncode, 4, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_an_update_shows_the_revision_it_will_deploy(self) -> None:
        self.deployed()
        new = self.commit_more()
        p = self.run_full(*self.UPDATE)
        self.assertIn(new[:12], p.stdout)

    def test_the_running_agent_guard_message_is_kept_and_is_exit_7(self) -> None:
        self.deployed()
        self.commit_more()
        text = "DEPLOY-FAIL guard 2 running run(s) on hamh-direct-run pin the worker runtime: run-a, run-b"
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "guard", "detail": text})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertIn(text, p.stderr)
        self.assertIn("running run(s)", p.stderr)

    def test_a_refusal_by_the_guard_says_the_stage_is_unchanged_and_the_runner_is_removed(self) -> None:
        self.deployed()
        self.commit_more()
        text = "DEPLOY-FAIL guard 2 running run(s) on hamh-direct-run pin the worker runtime: run-a, run-b"
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "guard", "detail": text})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        lines = " ".join(p.stderr.split())
        self.assertIn("The stage is unchanged. The runner is removed.", lines)
        self.assertNotIn("remove everything", lines)
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])

    def test_another_failure_of_the_deploy_does_not_say_the_stage_is_unchanged(self) -> None:
        self.deployed()
        self.commit_more()
        self.set_scenario(build={"final": "FAILED", "result": "fail", "failed_phase": "deploy", "detail": "DEPLOY-FAIL stack hamh-direct-network"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        lines = " ".join(p.stderr.split())
        self.assertNotIn("The stage is unchanged", lines)
        self.assertIn("./setup.sh --teardown to delete the stage and the runner", lines)
        self.assertNotIn("remove everything", lines)

    def test_the_plan_of_an_update_says_how_long_it_takes_and_when_active_runs_stop_it(self) -> None:
        self.deployed()
        self.commit_more()
        p = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = " ".join(p.stdout.split())
        self.assertIn("duration: about 20 to 25 minutes with a reused Cline binary, plus about 21 to 27 minutes when the binary must be rebuilt", plan)
        self.assertIn("guard: Active runs stop ./setup.sh --update with exit 7 before anything is installed or built.", plan)
        self.assertIn("The build also refuses worker runtime changes while runs are active.", plan)
        self.assertNotIn("Active runs do not block", plan)
        self.assertNotIn("duration:", " ".join(self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run").stdout.split()), "a full setup states its own time elsewhere")

    def test_an_update_with_no_stage_is_exit_7_and_points_at_the_full_setup(self) -> None:
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("--full", p.stderr)

    def test_a_revision_must_be_a_full_commit_id_that_exists_in_the_clone(self) -> None:
        self.deployed()
        first = self.commit()
        self.commit_more()
        for bad in ("main", "HEAD", first[:10], "g" * 40, "0" * 40):
            with self.subTest(bad=bad):
                p = self.run_full(*self.UPDATE, "--revision", bad)
                self.assertEqual(p.returncode, 2, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)

    def test_an_older_revision_can_be_chosen_by_its_full_id(self) -> None:
        first = self.deployed()
        self.commit_more()
        p = self.run_full(*self.UPDATE, "--revision", first)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.start_build_env(1)["SETUP_SOURCE_KEY"], f"source/{first}.zip")

    def test_a_dirty_tree_is_refused_for_an_update_too(self) -> None:
        self.deployed()
        (self.repo / "README.md").write_text("edited\n")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 7, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1)


class TeardownCase(StateBase):
    """The invariants of `--teardown` that the other suites rely on; the rest of it is in test_setup_teardown_local.py."""

    TEAR = ("--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", "210987654321")

    def deployed_world(self, **more) -> None:
        self.set_scenario(stage_deployed=True, bootstrap_present=True, **more)

    def test_without_the_acknowledgement_nothing_is_deleted(self) -> None:
        self.deployed_world()
        for args in (("--teardown", "--yes"), ("--teardown", "--no-input"), ("--teardown",)):
            with self.subTest(args=args):
                p = self.run_full(*args, "--profile", "work", "--region", "us-east-1")
                self.assertEqual(p.returncode, 4, self.out(p))
                self.assertEqual(self.mutations(), [])
        self.assertIn("hamh-direct-control", self.world()["stacks"])

    def test_the_stage_is_deleted_with_the_callers_credentials_and_the_runner_goes_with_it(self) -> None:
        self.deployed_world()
        self.seed_runner()
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0, "no build of the runner is started to delete the stage")
        stacks = self.world()["stacks"]
        self.assertEqual([s for s in stacks if s.startswith("hamh-direct-")], [])
        self.assertNotIn("hamh-setup-runner", stacks)
        self.assertIn("CDKToolkit", stacks)
        self.assertGreaterEqual(self.count_calls("s3api", "delete-objects"), 1)
        self.assertIn("TEARDOWN", p.stdout)
        self.assertRegex(p.stdout, r"(?mi)^kept:.*CDK bootstrap")

    def test_the_inventory_is_read_back_from_the_account(self) -> None:
        self.deployed_world()
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertGreaterEqual(self.count_calls("cloudformation", "list-stacks"), 2, "once before the deletes and once after them")
        self.assertRegex(p.stdout, r"(?m)^TEARDOWN-INVENTORY 0 stacks")

    def test_agentcore_network_interfaces_that_hold_the_network_stack_are_retained_and_reported(self) -> None:
        self.deployed_world(eni_stuck=True)
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 9, self.out(p))
        retain = [c for c in self.normalized_calls() if c[:2] == ["cloudformation", "delete-stack"] and "--retain-resources" in c]
        self.assertEqual(len(retain), 1)
        self.assertEqual(retain[0][retain[0].index("--stack-name") + 1], "hamh-direct-network")
        self.assertEqual(sorted(retain[0][retain[0].index("--retain-resources") + 1:]), ["PrivateSubnet", "Vpc", "WorkerSecurityGroup"])
        text = p.stdout + p.stderr
        for needle in ("RETAINED", "subnet-0abc1234", "sg-0abc1234", "8 hours", "hamh-direct-network", "AgentCore-managed network interface", "eni-0agentcore1", "--network-wait-minutes"):
            self.assertIn(needle, text)
        self.assertNotIn("TEARDOWN-INVENTORY 0", text, "a retained resource is never reported as zero resources")
        self.assertEqual(self.receipt().get("retained", "").count("subnet-0abc1234"), 1)

    def test_a_runner_that_cannot_be_removed_is_exit_8(self) -> None:
        self.deployed_world(runner_delete_fails=True)
        self.seed_runner()
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertIn("RETAINED", p.stdout + p.stderr)
        self.assertIn("hamh-setup-runner", p.stdout + p.stderr)

    def test_with_nothing_to_remove_it_says_so_and_is_exit_0(self) -> None:
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertIn("nothing to tear down", p.stdout.lower())

    def test_a_kept_runner_is_removed_by_the_teardown_of_an_empty_stage(self) -> None:
        self.set_scenario(runner_present=True, bootstrap_present=True)
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("hamh-setup-runner", self.world()["stacks"])

    def test_the_client_binary_and_the_cdk_bootstrap_are_left_and_named(self) -> None:
        self.deployed_world()
        self.ham_on_path()
        p = self.run_full(*self.TEAR)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("CDKToolkit", p.stdout)
        self.assertEqual(self.count_calls("cloudformation", "delete-stack", "--stack-name", "CDKToolkit"), 0)


if __name__ == "__main__":
    unittest.main()
