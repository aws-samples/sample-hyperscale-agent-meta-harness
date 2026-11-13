"""An existing `CDKToolkit` stack is never changed, also when the account has no bootstrap parameter for the default qualifier.

setup.sh decides whether the account has a CDK bootstrap from the SSM parameter `/cdk-bootstrap/hnb659fds/version`. An account that was bootstrapped with another
qualifier has a `CDKToolkit` stack and no such parameter (its parameter carries its own qualifier). The documents promise that an existing `CDKToolkit` is never modified, but
the setup used to create the bootstrap with `aws cloudformation deploy --stack-name CDKToolkit`, which updates that stack. The setup now looks at the stack as well and stops with
exit 7, before anything is created, naming the two ways out.

Cases:

  RefusalCase     a settled `CDKToolkit` stack without the parameter (several statuses) stops the full setup on every route: the full setup, `--dry-run`, `--preflight-only`
                  and `--build-on local`. Nothing is created and no call touches the stack.
  AllowedCase     what stays as it was: no stack (the template is printed, checked and deployed), the parameter present (used as it is, with or without the stack), and a stack
                  that an earlier create left in `ROLLBACK_COMPLETE` (it holds nothing and is deleted and made again).
  SettlingCase    a stack that is still changing when the run starts is waited for; when it settles without the parameter, the run stops and leaves the stack alone; when it
                  settles with the parameter, the run uses it as it is.

Run on a Linux machine with the tools pinned in `tools.lock`:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_setup_bootstrap_stack.py
"""

from __future__ import annotations

import unittest

from setup_full_support import FullBase
from test_setup_script import STUB_CARGO

TOOLKIT = "CDKToolkit"
PARAMETER = "/cdk-bootstrap/hnb659fds/version"


class BootstrapBase(FullBase):
    def seed_toolkit(self, status: str = "CREATE_COMPLETE", **extra) -> None:
        self.set_scenario(bootstrap_present=False, stacks={TOOLKIT: {"status": status, "outputs": {}, **extra}})

    def touched_toolkit(self) -> list:
        """Every call that creates, changes or deletes the CDKToolkit stack."""
        return [c for c in self.normalized_calls() if c[:2] in (["cloudformation", "deploy"], ["cloudformation", "delete-stack"]) and TOOLKIT in c]

    def assert_refused(self, p, label: str) -> None:
        self.assertEqual(p.returncode, 7, f"{label}\n{self.out(p)}")
        flat = " ".join(p.stderr.split())
        self.assertIn(TOOLKIT, flat, label)
        self.assertIn(PARAMETER, flat, label)
        self.assertIn("never changes an existing CDKToolkit stack", flat, label)
        self.assertIn("default qualifier hnb659fds", flat, label)
        self.assertIn("cdk bootstrap --qualifier hnb659fds --toolkit-stack-name <name>", flat, label)
        self.assertEqual(self.touched_toolkit(), [], f"{label}: the stack was touched")


class RefusalCase(BootstrapBase):
    def refuse_status(self, status: str) -> None:
        self.seed_toolkit(status)
        p = self.run_full(*self.ARGS)
        self.assert_refused(p, status)
        self.assertIn(status, p.stderr)
        self.assertEqual(self.mutations(), [], f"{status}: nothing may be created")
        self.assertEqual(self.count_calls("codebuild", "start-build"), 0)
        self.assertIn("Nothing was created", p.stderr)
        self.assertFalse(self.installed().exists())

    def test_a_complete_stack_without_the_parameter_stops_the_full_setup_before_anything_is_created(self) -> None:
        self.refuse_status("CREATE_COMPLETE")

    def test_an_updated_stack_without_the_parameter_stops_it_too(self) -> None:
        self.refuse_status("UPDATE_COMPLETE")

    def test_a_stack_whose_update_rolled_back_stops_it_too(self) -> None:
        self.refuse_status("UPDATE_ROLLBACK_COMPLETE")

    def test_an_imported_stack_stops_it_too(self) -> None:
        self.refuse_status("IMPORT_COMPLETE")

    def test_the_trigger_is_present_the_same_account_with_the_parameter_goes_on(self) -> None:
        # TRIGGER PRESENT: the refusals mean something only if the same stack, with its parameter, is accepted.
        self.set_scenario(bootstrap_present=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.touched_toolkit(), [])

    def test_a_dry_run_reports_it(self) -> None:
        self.seed_toolkit()
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assert_refused(p, "--dry-run")
        self.assertEqual(self.mutations(), [])

    def test_a_preflight_reports_it(self) -> None:
        self.seed_toolkit()
        p = self.run_full("--full", "--profile", "work", "--region", "us-east-1", "--preflight-only")
        self.assert_refused(p, "--preflight-only")
        self.assertEqual(self.mutations(), [])

    def test_the_local_route_is_stopped_too(self) -> None:
        for n in ("cargo-zigbuild", "zig", "node", "docker", "python3"):  # python3: the local route needs one of 3.11 or later
            self.write_stub(n, "#!/bin/sh\nexit 0\n")
        self.write_stub("cargo", STUB_CARGO)
        self.seed_toolkit()
        p = self.run_full("--full", "--build-on", "local", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-costs")
        self.assert_refused(p, "--build-on local")
        self.assertEqual(self.mutations(), [])


class AllowedCase(BootstrapBase):
    def test_no_stack_and_no_parameter_prints_checks_and_deploys_the_template(self) -> None:
        self.set_scenario(bootstrap_present=False)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.count_calls("cloudformation", "deploy", "--stack-name", TOOLKIT), 1)

    def test_the_parameter_present_means_the_bootstrap_is_used_as_it_is(self) -> None:
        self.set_scenario(bootstrap_present=True)
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.touched_toolkit(), [])

    def test_a_stack_that_rolled_back_on_its_first_create_is_deleted_and_made_again(self) -> None:
        self.seed_toolkit("ROLLBACK_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.touched_toolkit()
        self.assertEqual([c[:2] for c in calls], [["cloudformation", "delete-stack"], ["cloudformation", "deploy"]], calls)


class SettlingCase(BootstrapBase):
    def test_a_stack_that_settles_without_the_parameter_stops_the_run_and_is_left_alone(self) -> None:
        self.seed_toolkit("CREATE_IN_PROGRESS", polls_left=2, then="CREATE_COMPLETE")
        p = self.run_full(*self.ARGS)
        self.assertEqual(p.returncode, 7, self.out(p))
        flat = " ".join(p.stderr.split())
        self.assertIn("never changes an existing CDKToolkit stack", flat)
        self.assertEqual([c for c in self.touched_toolkit() if c[:2] == ["cloudformation", "deploy"]], [], "no deploy over the stack that settled")
        self.assertIn("runner", flat.lower(), "the runner was installed before the wait, and the message says so")
        self.assertIn("./setup.sh --teardown", p.stderr)
        self.assertEqual(self.count_calls("codebuild", "start-build"), 1, "only the build that prints the template had started; the deploy build never does")


if __name__ == "__main__":
    unittest.main()
