"""Shared fake controls for deploy and deployment-preflight tests."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess  # nosec B404: the fakes build CompletedProcess answers and start no program
import time
from pathlib import Path


TEST_ACCOUNT = f"{int.from_bytes(hashlib.sha256(b'preflight-account').digest()[:8], 'big') % 10**12:012d}"
TEST_OTHER_ACCOUNT = f"{(int(TEST_ACCOUNT) + 1) % 10**12:012d}"


class FakeAwsRecorder:
    """A subprocess.run replacement that records and answers approved preflight reads."""

    def __init__(self, responses: dict[tuple[str, str], object] | None = None) -> None:
        self.responses = responses or {}
        self.calls: list[dict[str, object]] = []

    def __call__(self, argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        self.calls.append({"argv": list(argv), **kwargs})
        pair = (argv[1], argv[2])
        response = self.responses.get(pair)
        if isinstance(response, BaseException):
            raise response
        if isinstance(response, subprocess.CompletedProcess):
            return response
        defaults = {
            ("sts", "get-caller-identity"): {"Account": TEST_ACCOUNT},
            ("ec2", "describe-availability-zones"): {
                "AvailabilityZones": [{"ZoneName": "us-east-1a", "ZoneId": "use1-az1"}]
            },
            ("ec2", "describe-vpcs"): {"Vpcs": []},
        }
        return subprocess.CompletedProcess(argv, 0, json.dumps(response if response is not None else defaults[pair]), "")


def apply_fake_aws_control(pair: tuple[str, str], arguments: list[str] | None = None) -> subprocess.CompletedProcess[str] | None:
    """Apply pair-scoped dispatcher controls used by script-based deploy tests."""
    arguments = arguments or []
    stack = os.environ.get("FAKE_AWS_RESPONSE_STACK")
    if stack and ("--stack-name" not in arguments or arguments[arguments.index("--stack-name") + 1] != stack):
        return None
    secret = os.environ.get("FAKE_AWS_STDERR", "")
    seeded_stdout = os.environ.get("FAKE_AWS_STDOUT", "")
    selected = tuple(os.environ.get("FAKE_AWS_RESPONSE_PAIR", "").split())
    if selected == pair:
        mode = os.environ.get("FAKE_AWS_RESPONSE_MODE", "")
        if mode == "denied":
            return subprocess.CompletedProcess([], 254, seeded_stdout, "AccessDenied " + secret)
        if mode == "nonzero":
            return subprocess.CompletedProcess([], 3, seeded_stdout, "failed " + secret)
        if mode == "empty":
            return subprocess.CompletedProcess([], 0, "", "")
        if mode == "malformed-json":
            return subprocess.CompletedProcess([], 0, "{", "")
        if mode == "malformed-shape":
            return subprocess.CompletedProcess([], 0, '{"unexpected": []}', "")
        if mode in ("multiple-stacks", "duplicate-output-keys"):
            region = arguments[arguments.index("--region") + 1]
            endpoint = f"https://abc123.execute-api.{region}.amazonaws.com/direct"
            rows = [{"OutputKey": "V1ApiUrl", "OutputValue": endpoint}]
            stacks = [{"Outputs": rows}]
            if mode == "multiple-stacks":
                stacks.append({"Outputs": []})
            else:
                rows.append({"OutputKey": "V1ApiUrl", "OutputValue": endpoint})
            return subprocess.CompletedProcess([], 0, json.dumps({"Stacks": stacks}), "")
    delayed = tuple(os.environ.get("FAKE_AWS_DELAY_PAIR", "").split())
    if delayed == pair:
        time.sleep(float(os.environ.get("FAKE_AWS_DELAY_S", "0")))
    return None


def write_control_endpoint(body: dict, region: str) -> None:
    """Add the fake control stack endpoint unless a test selects another producer value."""
    if os.environ.get("FAKE_NO_V1_API_URL") == "1":
        return
    value = os.environ.get("FAKE_V1_API_URL")
    if value is None:
        value = "not-an-endpoint" if os.environ.get("FAKE_MALFORMED_V1_API_URL") == "1" else (
            f"https://abc123.execute-api.{region}.amazonaws.com/direct"
        )
    body.setdefault("outputs", {})["V1ApiUrl"] = value


def selected_region(root: Path, profile: str) -> str:
    """Read the selected Region from the fake deploy's accounts file."""
    path = Path(os.environ.get("HAMH_ACCOUNTS_FILE") or root / "accounts.json")
    document = json.loads(path.read_text(encoding="utf-8"))
    return next(entry["region"] for entry in document["accounts"] if entry["profile"] == profile)
