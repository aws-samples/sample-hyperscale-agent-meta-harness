#!/usr/bin/env python3
"""Read-only deployment preflight for stage ``direct``.

The module has one AWS process boundary, ``AwsReader.read``. Registration
validates the full check composition, but never grants operation authority.
"""

from __future__ import annotations

import ipaddress
import json
import math
import os
import re
import subprocess  # nosec B404: one closed, argument-list-only AWS read boundary
import sys
from dataclasses import dataclass
from types import MappingProxyType
from typing import Callable, Literal, Mapping, Sequence, TypeAlias

import aws_target

JsonScalar: TypeAlias = None | bool | int | float | str
FrozenJson: TypeAlias = JsonScalar | tuple["FrozenJson", ...] | Mapping[str, "FrozenJson"]
READ_TIMEOUT_S = 30
PROJECT = "hyperscale-agent-meta-harness"
VPC_CIDR = "10.50.0.0/16"


@dataclass(frozen=True)
class Context:
    stage: str
    region: str
    profile: str
    expected_account: str
    expected_azs: tuple[tuple[str, str], ...]
    wanted_cidr: str
    owned_tags: tuple[tuple[str, str], ...]
    # The operator the selected entry names, or None: the deploy refuses another caller of the account (reason=caller), so the preflight does too.
    operator: aws_target.Operator | None = None


@dataclass(frozen=True)
class ReadSpec:
    read_id: str
    service: str
    operation: str
    arguments: Callable[[Context], tuple[str, ...]]


@dataclass(frozen=True)
class ReadResult:
    read_id: str
    state: Literal["OK", "UNKNOWN"]
    reason_code: str
    document: Mapping[str, FrozenJson] | None


@dataclass(frozen=True)
class CheckResult:
    check_id: str
    state: Literal["PASS", "FAIL", "UNKNOWN"]
    reason_code: str
    facts: Mapping[str, FrozenJson]


@dataclass(frozen=True)
class CheckSpec:
    check_id: str
    read_ids: tuple[str, ...]
    evaluate: Callable[[Context, Mapping[str, ReadResult]], CheckResult]
    halt_on_nonpass: bool = False


@dataclass(frozen=True)
class BoundRead:
    spec: ReadSpec
    service_arguments: tuple[str, ...]


@dataclass(frozen=True)
class Registration:
    reads: tuple[BoundRead, ...]
    checks: tuple[CheckSpec, ...]
    common_count: int


def _reject_json_constant(_value: str) -> object:
    raise ValueError("JSON constant is not supported")


def _finite_json_float(value: str) -> float:
    number = float(value)
    if not math.isfinite(number):
        raise ValueError("JSON number must be finite")
    return number


def _freeze(value: object) -> FrozenJson:
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("JSON number must be finite")
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return tuple(_freeze(item) for item in value)
    if isinstance(value, dict) and all(isinstance(key, str) for key in value):
        return MappingProxyType({key: _freeze(item) for key, item in value.items()})
    raise ValueError("JSON value has an unsupported shape")


def _facts(**values: object) -> Mapping[str, FrozenJson]:
    return MappingProxyType({key: _freeze(value) for key, value in values.items()})


def _read_document(result: ReadResult) -> object:
    return result.document if result.state == "OK" else None


def evaluate_account(doc: object, expected_account: str) -> CheckResult:
    if not isinstance(doc, Mapping):
        return CheckResult("CALLER_ACCOUNT", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
    account = doc.get("Account")
    if not isinstance(account, str) or not re.fullmatch(r"[0-9]{12}", account):
        return CheckResult("CALLER_ACCOUNT", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
    if account != expected_account:
        return CheckResult("CALLER_ACCOUNT", "FAIL", "ACCOUNT_MISMATCH", _facts())
    return CheckResult("CALLER_ACCOUNT", "PASS", "ACCOUNT_MATCH", _facts(account=account))


def evaluate_az_mapping(doc: object, expected: Sequence[tuple[str, str]]) -> CheckResult:
    names = [name for name, _ in expected]
    if len(names) != len(set(names)) or not names or any(not isinstance(name, str) or not isinstance(zone_id, str)
                                                         for name, zone_id in expected):
        return CheckResult("AZ_MAPPING", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
    if not isinstance(doc, Mapping) or not isinstance(doc.get("AvailabilityZones"), (list, tuple)):
        return CheckResult("AZ_MAPPING", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
    rows = doc["AvailabilityZones"]
    found: dict[str, str] = {}
    for row in rows:
        if not isinstance(row, Mapping):
            return CheckResult("AZ_MAPPING", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
        name, zone_id = row.get("ZoneName"), row.get("ZoneId")
        if not isinstance(name, str) or not isinstance(zone_id, str) or name in found:
            return CheckResult("AZ_MAPPING", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
        found[name] = zone_id
    if len(rows) != len(expected) or set(found) != set(names):
        return CheckResult("AZ_MAPPING", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
    mapping = tuple((name, found[name]) for name in names)
    if mapping != tuple(expected):
        return CheckResult("AZ_MAPPING", "FAIL", "AZ_MAPPING_MISMATCH", _facts())
    return CheckResult("AZ_MAPPING", "PASS", "AZ_MAPPING_MATCH", _facts(mapping=mapping))


def vpc_overlap_details(
    doc: object,
    wanted_cidr: str,
) -> tuple[str, tuple[tuple[str, str, tuple[tuple[str, str], ...]], ...]] | None:
    """Validate every VPC row and return overlapping blocks without printing them."""
    try:
        wanted = ipaddress.ip_network(wanted_cidr, strict=False)
    except (TypeError, ValueError):
        return None
    if not isinstance(doc, Mapping) or not isinstance(doc.get("Vpcs"), (list, tuple)):
        return None
    overlaps: list[tuple[str, str, tuple[tuple[str, str], ...]]] = []
    for vpc in doc["Vpcs"]:
        if not isinstance(vpc, Mapping):
            return None
        tags_raw = vpc.get("Tags", ())
        associations = vpc.get("CidrBlockAssociationSet", ())
        if not isinstance(tags_raw, (list, tuple)) or not isinstance(associations, (list, tuple)):
            return None
        tags: dict[str, str] = {}
        for tag in tags_raw:
            if not isinstance(tag, Mapping) or not isinstance(tag.get("Key"), str) or not isinstance(tag.get("Value"), str):
                return None
            tags[tag["Key"]] = tag["Value"]
        vpc_id, primary = vpc.get("VpcId"), vpc.get("CidrBlock")
        if not isinstance(vpc_id, str) or not isinstance(primary, str):
            return None
        cidrs: list[str] = [primary]
        for association in associations:
            if not isinstance(association, Mapping) or not isinstance(association.get("CidrBlock"), str):
                return None
            cidrs.append(association["CidrBlock"])
        try:
            networks = tuple(
                (cidr, ipaddress.ip_network(cidr, strict=False))
                for cidr in dict.fromkeys(cidrs)
            )
        except (TypeError, ValueError):
            return None
        frozen_tags = tuple(sorted(tags.items()))
        overlaps.extend((vpc_id, cidr, frozen_tags) for cidr, network in networks if network.overlaps(wanted))
    return str(wanted), tuple(overlaps)


def evaluate_vpc_overlap(doc: object, wanted_cidr: str, owned_tags: Mapping[str, str]) -> CheckResult:
    details = vpc_overlap_details(doc, wanted_cidr)
    if details is None:
        return CheckResult("VPC_OVERLAP", "UNKNOWN", "MALFORMED_RESPONSE", _facts())
    wanted, overlaps = details
    foreign = tuple(
        (vpc_id, cidr)
        for vpc_id, cidr, tags in overlaps
        if not all(dict(tags).get(key) == value for key, value in owned_tags.items())
    )
    if foreign:
        return CheckResult("VPC_OVERLAP", "FAIL", "VPC_OVERLAP", _facts(cidr=wanted, overlap_count=len(foreign)))
    return CheckResult("VPC_OVERLAP", "PASS", "VPC_CLEAR", _facts(cidr=wanted, overlaps=()))


def _account_check(context: Context, reads: Mapping[str, ReadResult]) -> CheckResult:
    result = reads["caller-identity"]
    if result.state != "OK":
        return CheckResult("CALLER_ACCOUNT", "UNKNOWN", result.reason_code, _facts())
    document = _read_document(result)
    checked = evaluate_account(document, context.expected_account)
    if checked.state != "PASS" or context.operator is None:
        return checked
    # The deploy's caller rule (deploy.py check_account, aws_target.operator_may_deploy): the entry's operator or the setup runner role of the account.
    arn = document.get("Arn") if isinstance(document, Mapping) else None
    if not isinstance(arn, str) or not aws_target.operator_may_deploy(context.expected_account, context.operator, arn):
        return CheckResult("CALLER_ACCOUNT", "FAIL", "CALLER_NOT_OPERATOR", checked.facts)
    return checked


def _az_check(context: Context, reads: Mapping[str, ReadResult]) -> CheckResult:
    result = reads["direct-azs"]
    if result.state != "OK":
        return CheckResult("AZ_MAPPING", "UNKNOWN", result.reason_code, _facts())
    return evaluate_az_mapping(_read_document(result), context.expected_azs)


def _vpc_check(context: Context, reads: Mapping[str, ReadResult]) -> CheckResult:
    result = reads["vpcs"]
    if result.state != "OK":
        return CheckResult("VPC_OVERLAP", "UNKNOWN", result.reason_code, _facts())
    return evaluate_vpc_overlap(_read_document(result), context.wanted_cidr, dict(context.owned_tags))


def _no_arguments(_context: Context) -> tuple[str, ...]:
    return ()


def _zone_arguments(context: Context) -> tuple[str, ...]:
    return ("--zone-names", *(name for name, _ in context.expected_azs))


COMMON_READS = (
    ReadSpec("caller-identity", "sts", "get-caller-identity", _no_arguments),
    ReadSpec("direct-azs", "ec2", "describe-availability-zones", _zone_arguments),
    ReadSpec("vpcs", "ec2", "describe-vpcs", _no_arguments),
)
COMMON_CHECKS = (
    CheckSpec("CALLER_ACCOUNT", ("caller-identity",), _account_check, halt_on_nonpass=True),
    CheckSpec("AZ_MAPPING", ("direct-azs",), _az_check),
    CheckSpec("VPC_OVERLAP", ("vpcs",), _vpc_check),
)


def _safe_arguments(context: Context, service: str, operation: str, arguments: tuple[str, ...]) -> bool:
    pair = (service, operation)
    if pair == ("sts", "get-caller-identity"):
        return arguments == ()
    if pair == ("ec2", "describe-availability-zones"):
        return arguments == ("--zone-names", *(name for name, _ in context.expected_azs))
    if pair == ("ec2", "describe-vpcs"):
        return arguments == ()
    return False


SAFE_READS = frozenset({
    ("sts", "get-caller-identity"),
    ("ec2", "describe-availability-zones"),
    ("ec2", "describe-vpcs"),
})


def _validate_bound(context: Context, bound: BoundRead) -> None:
    pair = (bound.spec.service, bound.spec.operation)
    if pair not in SAFE_READS or not _safe_arguments(context, *pair, bound.service_arguments):
        raise ValueError("AWS read is not authorized")


def register(
    context: Context,
    reads: Sequence[ReadSpec],
    common: Sequence[CheckSpec],
    engine: Sequence[CheckSpec] = (),
) -> Registration:
    read_ids = [spec.read_id for spec in reads]
    if any(not isinstance(read_id, str) or not read_id for read_id in read_ids) or len(read_ids) != len(set(read_ids)):
        raise ValueError("read IDs must be unique and non-empty")
    bound: list[BoundRead] = []
    for spec in reads:
        arguments = spec.arguments(context)
        if not isinstance(arguments, tuple) or not all(isinstance(argument, str) for argument in arguments):
            raise ValueError("read arguments must be a tuple of strings")
        item = BoundRead(spec, arguments)
        _validate_bound(context, item)
        bound.append(item)
    checks = tuple(common) + tuple(engine)
    check_ids = [spec.check_id for spec in checks]
    if any(not isinstance(check_id, str) or not check_id for check_id in check_ids) or len(check_ids) != len(set(check_ids)):
        raise ValueError("check IDs must be unique and non-empty")
    available = set(read_ids)
    for spec in checks:
        if not isinstance(spec.read_ids, tuple) or len(spec.read_ids) != len(set(spec.read_ids)):
            raise ValueError("check dependencies must be a unique tuple")
        if any(read_id not in available for read_id in spec.read_ids):
            raise ValueError("check dependency is not registered")
    return Registration(tuple(bound), checks, len(common))


def _authorized_argv(context: Context, bound: BoundRead, aws_bin: str) -> list[str]:
    _validate_bound(context, bound)
    return [aws_bin, bound.spec.service, bound.spec.operation, *bound.service_arguments,
            "--region", context.region, "--output", "json"]


class AwsReader:
    def __init__(
        self,
        context: Context,
        registration: Registration,
        *,
        aws_bin: str,
        parent_environment: Mapping[str, str],
    ) -> None:
        self.context = context
        self.registration = registration
        self.aws_bin = aws_bin
        self.parent_environment = MappingProxyType(dict(parent_environment))

    def read(self, read_id: str) -> ReadResult:
        try:
            bound = next(item for item in self.registration.reads if item.spec.read_id == read_id)
        except StopIteration:
            raise KeyError(read_id) from None
        argv = _authorized_argv(self.context, bound, self.aws_bin)
        environment = dict(self.parent_environment)
        environment["AWS_PROFILE"] = self.context.profile
        environment["AWS_PAGER"] = ""
        environment["AWS_CLI_AUTO_PROMPT"] = "off"
        environment["AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"] = "true"
        for key in tuple(environment):
            if key in ("AWS_REGION", "AWS_DEFAULT_REGION", "AWS_ENDPOINT_URL") or key.startswith("AWS_ENDPOINT_URL_"):
                environment.pop(key)
        try:
            process = subprocess.run(  # nosemgrep: dangerous-subprocess-use-audit -- the argv is one of three AWS reads of the closed read policy, rechecked just before; no shell
                argv, capture_output=True, text=True, encoding="utf-8", errors="strict",
                env=environment, timeout=READ_TIMEOUT_S,
            )  # nosec B603: closed AWS read policy rechecked immediately before launch
        except FileNotFoundError:
            return ReadResult(read_id, "UNKNOWN", "CLI_MISSING", None)
        except subprocess.TimeoutExpired:
            return ReadResult(read_id, "UNKNOWN", "TIMEOUT", None)
        except UnicodeDecodeError:
            return ReadResult(read_id, "UNKNOWN", "MALFORMED_RESPONSE", None)
        except Exception:
            return ReadResult(read_id, "UNKNOWN", "INTERNAL_ERROR", None)
        if process.returncode != 0:
            denied = re.search(r"\b(?:AccessDenied|AccessDeniedException|UnauthorizedOperation)\b", process.stderr or "")
            return ReadResult(read_id, "UNKNOWN", "ACCESS_DENIED" if denied else "AWS_ERROR", None)
        if not process.stdout.strip():
            return ReadResult(read_id, "UNKNOWN", "MALFORMED_RESPONSE", None)
        try:
            parsed = json.loads(
                process.stdout,
                parse_constant=_reject_json_constant,
                parse_float=_finite_json_float,
            )
            frozen = _freeze(parsed)
        except (ValueError, TypeError):
            return ReadResult(read_id, "UNKNOWN", "MALFORMED_RESPONSE", None)
        if not isinstance(frozen, Mapping):
            return ReadResult(read_id, "UNKNOWN", "MALFORMED_RESPONSE", None)
        return ReadResult(read_id, "OK", "READ_OK", frozen)


def run_checks(
    context: Context,
    registration: Registration,
    reader: AwsReader,
) -> tuple[CheckResult, ...]:
    cache: dict[str, ReadResult] = {}
    results: list[CheckResult] = []
    for check in registration.checks:
        for read_id in check.read_ids:
            if read_id not in cache:
                cache[read_id] = reader.read(read_id)
        declared = MappingProxyType({read_id: cache[read_id] for read_id in check.read_ids})
        result = check.evaluate(context, declared)
        if result.check_id != check.check_id:
            raise ValueError("evaluator returned the wrong check ID")
        results.append(result)
        if check.halt_on_nonpass and result.state != "PASS":
            break
    return tuple(results)


def _masked_account(value: object) -> str:
    return "********" + value[-4:] if isinstance(value, str) and re.fullmatch(r"[0-9]{12}", value) else "<unavailable>"


def _print_results(context: Context, results: Sequence[CheckResult], *, expected_count: int) -> int:
    for result in results:
        suffix = ""
        if result.check_id == "CALLER_ACCOUNT":
            suffix = " account=" + _masked_account(result.facts.get("account"))
        print(f"DEPLOY-PREFLIGHT {result.state} {result.check_id} {result.reason_code}{suffix}")
    if len(results) == expected_count and all(result.state == "PASS" for result in results):
        print(f"DEPLOY-PREFLIGHT READY stage={context.stage} region={context.region} "
              f"account={_masked_account(context.expected_account)}")
        return 0
    print(f"DEPLOY-PREFLIGHT NOT-READY stage={context.stage}")
    return 1


def main(argv: Sequence[str], *, environ: Mapping[str, str] | None = None, aws_bin: str | None = None) -> int:
    environment = os.environ if environ is None else environ
    if list(argv) != ["--stage", "direct"]:
        print("DEPLOY-PREFLIGHT USAGE arguments")
        return 2
    try:
        target = aws_target.select(environment)
        expected = tuple(zip(target.azs["direct"], target.az_ids["direct"]))
        context = Context(
            stage="direct", region=target.region, profile=target.profile, expected_account=target.account,
            expected_azs=expected, wanted_cidr=VPC_CIDR,
            owned_tags=(("project", PROJECT), ("stage", "direct")), operator=target.operator,
        )
        registration = register(context, COMMON_READS, COMMON_CHECKS, engine=())
        reader = AwsReader(context, registration, aws_bin=aws_bin or environment.get("DEPLOY_AWS_BIN", "aws"),
                           parent_environment=environment)
        results = run_checks(context, registration, reader)
        return _print_results(context, results, expected_count=len(registration.checks))
    except aws_target.TargetError:
        print("DEPLOY-PREFLIGHT USAGE target-config")
        return 2
    except Exception:
        result = CheckResult("PREFLIGHT", "UNKNOWN", "INTERNAL_ERROR", _facts())
        context = Context("direct", "<unavailable>", "", "", (), VPC_CIDR, ())
        return _print_results(context, (result,), expected_count=1)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
