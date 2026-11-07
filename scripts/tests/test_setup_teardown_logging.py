"""`setup.sh --teardown` puts the Bedrock model invocation logging setting back to the state a deploy recorded.

A deploy in the mode `enable-and-restore` turns the account's model invocation logging on when it is unset. Before it does, it writes the state it found to the SSM
parameter `/hamh/<stage>/invocation-logging-prior` (scripts/lib/deploy.py `invocation_logging`). The repository's own teardown (scripts/lib/teardown.py
`restore_invocation_logging`) puts that state back while the setting still names the stage's log group. The local teardown (scripts/setup/teardown.sh) used to delete the
record with the other parameters and leave the setting on a log group that goes with the stage.

The cases run the real `setup.sh` and `scripts/setup/*.sh` against the stand-in AWS CLI of setup_fake_aws.py, which keeps the account's setting (`bedrock_logging`) and the
record (`logging_record`). The contract:

- a record whose state is unset: the configuration is deleted; a record that holds a configuration: that configuration is put back;
- the restore comes before the first stack is deleted, and says what it restored;
- no record, a record without the stage tags, or a setting that names another destination: nothing is written, and the output says why;
- a restore that fails is a failed step (exit 8, a FAILED line) and the record stays, so that a second teardown can try again;
- a dry run writes nothing and says what it would restore.

No live AWS account was used.
"""

from __future__ import annotations

import json
import unittest

from test_setup_teardown_local import TeardownBase, TEAR

STAGE_GROUP = "/hamh/direct/bedrock-invocations"
RECORD_NAME = "/hamh/direct/invocation-logging-prior"
OURS = {"cloudWatchConfig": {"logGroupName": STAGE_GROUP, "roleArn": "arn:aws:iam::210987654321:role/hamh-direct-bedrock-logging"},
        "textDataDeliveryEnabled": True, "imageDataDeliveryEnabled": False, "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False}
PRIOR = {"cloudWatchConfig": {"logGroupName": "/team/model-calls", "roleArn": "arn:aws:iam::210987654321:role/team-model-logging"},
         "textDataDeliveryEnabled": True, "imageDataDeliveryEnabled": True, "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False}
PRIOR_S3 = {"s3Config": {"bucketName": "team-model-calls", "keyPrefix": "bedrock"}, "textDataDeliveryEnabled": True,
            "imageDataDeliveryEnabled": False, "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False}
THEIRS = {"cloudWatchConfig": {"logGroupName": "/someone/else", "roleArn": "arn:aws:iam::210987654321:role/else"}, "textDataDeliveryEnabled": True,
          "imageDataDeliveryEnabled": False, "embeddingDataDeliveryEnabled": False, "videoDataDeliveryEnabled": False}
UNSET = {"loggingConfig": None}


class LoggingBase(TeardownBase):
    def setting(self):
        """The account's setting after the run (None while unset)."""
        return self.world().get("bedrock_logging")

    def record_left(self) -> bool:
        return RECORD_NAME in self.world().get("params", {})

    def logging_writes(self) -> list:
        return [c for c in self.normalized_calls()
                if c[:2] in (["bedrock", "put-model-invocation-logging-configuration"], ["bedrock", "delete-model-invocation-logging-configuration"])]

    def first_stack_delete(self) -> int:
        return self.position("cloudformation", "delete-stack")


class RestoreCase(LoggingBase):
    def test_a_record_of_an_unset_state_deletes_the_configuration_the_deploy_made(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIsNone(self.setting(), "the setting is unset again, as the record says it was")
        self.assertEqual(self.logging_writes(), [["bedrock", "delete-model-invocation-logging-configuration"]])
        text = p.stdout + p.stderr
        self.assertIn("Restored Bedrock model invocation logging", text)
        self.assertIn("unset", text)
        self.assertIn(RECORD_NAME, text)

    def test_a_record_that_holds_a_cloudwatch_configuration_puts_that_configuration_back(self) -> None:
        self.stage(logging_record={"loggingConfig": PRIOR}, bedrock_logging=OURS)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.setting(), PRIOR)
        puts = [c for c in self.logging_writes() if c[1].startswith("put-")]
        self.assertEqual(len(puts), 1, self.logging_writes())
        sent = puts[0][puts[0].index("--logging-config") + 1]
        self.assertEqual(json.loads(sent), PRIOR, "the configuration is sent as recorded, nothing else")
        self.assertIn("/team/model-calls", p.stdout + p.stderr)

    def test_a_record_that_holds_an_s3_configuration_puts_that_configuration_back(self) -> None:
        self.stage(logging_record={"loggingConfig": PRIOR_S3}, bedrock_logging=OURS)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.setting(), PRIOR_S3)
        self.assertIn("team-model-calls", p.stdout + p.stderr)

    def test_the_restore_comes_before_the_first_stack_is_deleted(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS)
        self.assertEqual(self.tear().returncode, 0)
        self.assertLess(self.position("bedrock", "delete-model-invocation-logging-configuration"), self.first_stack_delete(),
                        "the log group the setting names goes with the observability stack, so the setting is put back first")

    def test_the_record_goes_with_the_other_parameters_after_the_restore(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS)
        self.assertEqual(self.tear().returncode, 0)
        self.assertFalse(self.record_left())

    def test_the_restore_is_made_in_the_region_of_the_stage(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS)
        self.assertEqual(self.tear().returncode, 0)
        writes = [c for c in self.calls() if "delete-model-invocation-logging-configuration" in c]
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0][writes[0].index("--region") + 1], "us-east-1")

    def test_a_second_teardown_finds_the_setting_already_as_recorded(self) -> None:
        """The first run restored the setting and could not finish; the record is still there on the second run, which must not write again."""
        self.stage(logging_record=UNSET, bedrock_logging=None)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.logging_writes(), [])
        self.assertIn("already unset", p.stdout + p.stderr)


class LeftAloneCase(LoggingBase):
    def test_without_a_record_the_setting_is_not_touched_and_the_output_says_why(self) -> None:
        self.stage(bedrock_logging=OURS)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.setting(), OURS)
        self.assertEqual(self.logging_writes(), [])
        text = " ".join((p.stdout + p.stderr).split())
        self.assertIn("Left Bedrock model invocation logging as it is", text)
        self.assertIn(f"no record at {RECORD_NAME}", text)

    def test_without_a_record_and_without_a_setting_nothing_is_written(self) -> None:
        self.stage()
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.logging_writes(), [])

    def test_a_setting_that_names_another_destination_is_not_overwritten(self) -> None:
        self.stage(logging_record={"loggingConfig": PRIOR}, bedrock_logging=THEIRS)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.setting(), THEIRS)
        self.assertEqual(self.logging_writes(), [])
        text = " ".join((p.stdout + p.stderr).split())
        self.assertIn("/someone/else", text)
        self.assertIn(STAGE_GROUP, text)

    def test_a_record_without_the_stage_tags_is_not_read_and_not_deleted(self) -> None:
        self.stage(logging_record=UNSET, logging_record_tagged=False, bedrock_logging=OURS)
        p = self.tear()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.setting(), OURS)
        self.assertEqual(self.logging_writes(), [])
        self.assertTrue(self.record_left(), "a parameter without the stage tags is not the stage's")
        self.assertNotIn(["ssm", "get-parameter", "--name", RECORD_NAME], [c[:4] for c in self.normalized_calls()])


class FailureCase(LoggingBase):
    def test_a_restore_that_fails_is_a_failed_step_and_the_record_stays(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS,
                   faults=[{"match": "delete-model-invocation-logging-configuration", "exit": 254,
                            "stderr": "An error occurred (AccessDeniedException) when calling the DeleteModelInvocationLoggingConfiguration operation: not allowed"}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        text = p.stdout + p.stderr
        self.assertIn("FAILED: could not restore Bedrock model invocation logging", text)
        self.assertIn("AccessDeniedException", text)
        self.assertEqual(self.setting(), OURS, "nothing was written")
        self.assertTrue(self.record_left(), "the record stays so that a second teardown can try again")
        self.assertIn(f"LEFT: SSM parameter {RECORD_NAME} is still there", text)
        self.assertIn("aws bedrock delete-model-invocation-logging-configuration", text, "the way to do it by hand is named")

    def test_a_failed_restore_does_not_keep_the_stacks_from_being_deleted(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS,
                   faults=[{"match": "delete-model-invocation-logging-configuration", "exit": 254, "stderr": "An error occurred (InternalFailure): boom"}])
        self.tear()
        self.assertEqual(self.stacks_left(), [])

    def test_a_record_that_is_not_readable_as_a_state_is_a_failed_step_and_writes_nothing(self) -> None:
        self.stage(logging_record="this is not a record", bedrock_logging=OURS)
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.setting(), OURS)
        self.assertEqual(self.logging_writes(), [])
        self.assertIn("FAILED: could not read the state recorded at " + RECORD_NAME, p.stdout + p.stderr)
        self.assertTrue(self.record_left())

    def test_a_setting_that_cannot_be_read_is_a_failed_step_and_the_record_stays(self) -> None:
        self.stage(logging_record=UNSET, bedrock_logging=OURS,
                   faults=[{"match": "get-model-invocation-logging-configuration", "exit": 254,
                            "stderr": "An error occurred (AccessDeniedException) when calling the GetModelInvocationLoggingConfiguration operation: not allowed"}])
        p = self.tear()
        self.assertEqual(p.returncode, 8, self.out(p))
        self.assertEqual(self.logging_writes(), [])
        self.assertTrue(self.record_left())
        self.assertIn("FAILED: could not read the Bedrock model invocation logging setting", p.stdout + p.stderr)


class PlanCase(LoggingBase):
    def test_a_dry_run_says_what_it_would_restore_and_writes_nothing(self) -> None:
        self.stage(logging_record={"loggingConfig": PRIOR}, bedrock_logging=OURS)
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.logging_writes(), [])
        self.assertEqual(self.setting(), OURS)
        text = " ".join(p.stdout.split())
        self.assertIn("restore Bedrock model invocation logging", text)
        self.assertIn("/team/model-calls", text)

    def test_the_plan_says_the_setting_stays_when_there_is_no_record(self) -> None:
        self.stage(bedrock_logging=OURS)
        p = self.run_full(*TEAR, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        text = " ".join(p.stdout.split())
        self.assertIn("Bedrock model invocation logging stays as it is", text)


if __name__ == "__main__":
    unittest.main()
