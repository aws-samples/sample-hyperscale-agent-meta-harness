#!/usr/bin/env python3
"""research-key-put.py: store the Tavily API key in the stage account's Secrets Manager secret, and rotate it.

You type:  python3 scripts/research-key-put.py --stage direct
You see:   "Tavily API key: " and "Tavily API key again: " (typing is hidden), then four lines: the secret ARN, the new version id, how many
           characters were stored, and the tag written; the last line says what happens next. Nothing derived from the key is printed.
           Check later, without typing a key: python3 scripts/research-key-put.py --stage direct --check
           The check also says how many days ago the current key was stored and exits 5 when that is more than --max-age-days (default 30),
           and which provider a stage in auto mode (the default of a stage) uses: Tavily when a key is stored, AgentCore Web Search when none is.
           Go back to AgentCore Web Search: python3 scripts/research-key-put.py --stage direct --clear   (stores a blank version; no prompt, no key)
           Replace the key and prove it: python3 scripts/research-key-put.py --stage direct --rotate   (probe, store, wait for the route; below)
           Enterprise plan only: python3 scripts/research-key-put.py --stage direct --rotate-enterprise --ttl-hours 960 --key-limit 500

The stage's CDK stack creates the secret EMPTY under RESEARCH_STORE_NAME; this script only adds a version to it and never creates it. The research
Lambda (`research-api`) is the one principal the secret's resource policy lets read the value, so this script cannot read it back either.

--clear replaces the stored key by a blank version and tags the secret `research-key-stored-at=cleared-<UTC date>`. Secrets Manager refuses an empty
SecretString (minimum length 1), so the blank is one space, which the research Lambda reads as no key. It makes the same three AWS calls as the store, asks for
nothing, needs no terminal and reads nothing. The previous key stays in the secret with the label AWSPREVIOUS (Secrets Manager moves the label there) until
the next store; to revoke it, delete it in the Tavily dashboard. The script cannot read the value, so `--check` tells a key from a blank by the tag this script writes: a date
means a key was stored, `cleared-<date>` means it was cleared, and a secret with no such tag is reported as not known. A cleared secret has no key to rotate,
so the age limit does not apply to it.

--rotate asks for the new key like the plain store, then: (1) asks Tavily whether it accepts the key, with GET https://api.tavily.com/usage
from this machine (200 accepted: the key's usage and limit are printed and nothing else; 401 or 403 refused; 429 means the endpoint, which allows 10 calls in 10
minutes, is busy: the message says in how many minutes to retry, from `retry-after`); (2) stores the key only if Tavily accepted it, as a new version with the same
calls and tag as the plain store; (3) waits up to 6 minutes (ROUTE_WAIT_S, counted on the monotonic clock from the moment the put returned) for the research route to hold that version: every 15 seconds it invokes the research-api
Lambda with the key-state question (`aws lambda invoke`, which needs lambda:InvokeFunction on the function and no other permission), and the route answers with
the provider it would use now and the Secrets Manager VERSION ID of the key it holds, never the key and nothing derived from it; (4) prints when the old key can be deleted:
six minutes after the store, as a clock time in your zone. The research Lambda keeps a found key for 5 minutes and reads the secret again at once after Tavily refuses a key, so the wait always ends within
6 minutes. A stage whose research route predates the key-state question answers "not found": the script then says so, does not wait and exits 7.

--rotate-enterprise
reads an ADMIN key at the hidden prompt, makes one user-level key with POST https://api-key-generator.tavily.com/generate-keys (count 1, no scope),
probes it, stores it like --rotate and records the generation in the secret's tags (the secret policy forbids reading the value, so the tags are the record):
research-key-generations is a ledger of every generation that may still be active, oldest first, comma separated, each written request_id@version_id (the secret
version that holds its key; the tag holds 256 characters, room for three), and research-key-expires-at is the expiry of the newest. Previous generations are
deactivated with POST /deactivate-keys (the admin key as the bearer token) only after the tags were written, the route holds the new key and DEACTIVATE_AFTER_S
seconds have passed since the store (the longest an instance may still hold the old key), and each one only if `list-secret-version-ids` shows that its version is not
the current one; a generation that fails stays in the ledger, all of them are kept, and the new key stays. A key made by this run is deactivated again only when it is
certain that it was not stored: a put that failed or answered something unusable is followed by `list-secret-version-ids`, and only a call that succeeded and shows no new
version allows it; a new version means the key is stored and the run goes on; anything else deactivates nothing and prints the request id and the steps to do it by hand.
A plain store, --rotate and --clear leave the Enterprise tags as they are and print which generations are still active. Run one operator at a time: no lock is built. Choose --ttl-hours LONGER than the time between your rotations plus the time you need to notice a missed
one: a key that nobody rotates then expires by itself, and a TTL shorter than the interval leaves research with no key for the gap. `--check` prints the expiry of the generation that made the current key.

The age comes from `LastChangedDate` of describe-secret, which AWS sets to the last time the secret was modified in any way: a later tag or policy
change makes the key look younger than it is. The alarms of the stage (infra/lib/stacks/research-alarms.ts) use the creation time of the key's own
version, which research-api reads with the key. The `research-key-stored-at` tag, written by this script, is printed beside the age for comparison.

Account, Region and profile come from scripts/lib/aws_target.py (accounts.json; HAMH_AWS_PROFILE names the entry, unset means the active one).
The AWS CLI (`$HAMH_AWS_BIN`, else `aws` on PATH) is run with an argument list and no shell. A key is passed to it on STDIN only
(`--secret-string file:///dev/stdin`): it is never in argv, in the child's environment (a variable holding it is dropped), in a file, in
this script's output or in a log. An AWS or Tavily error text is printed with every occurrence of a key replaced by `<redacted>`. A key reaches Tavily only as the
bearer token of its request over TLS (http.client, which follows no redirect, so the token is never sent to a second host). After the first key is typed the
script prints fixed messages, status codes and AWS error codes from a short list (ALLOWED_AWS_CODES), never a response body or an error text. Every AWS CLI call has a
time limit of 60 seconds. The AWS CLI writes every request to ~/.aws/cli/history when `cli_history = enabled` is set for the profile, the secret string included; every
mode that takes a key asks the CLI itself (`aws configure get cli_history`, the same profile and Region: exit 1 with no output means not set) and refuses to run
when the value is enabled or the CLI cannot answer.

A key is read with getpass on a real terminal only. A stdin that is not a terminal is refused, so a key never comes from a pipe or a
file, and there is no option, environment variable or file that can carry it. It is asked twice and both entries must match; it must be
20 to 256 printable ASCII characters without spaces (no vendor prefix is required).

What Python cannot guarantee: the buffer this script owns (a bytearray) is overwritten after the store, but getpass returns an immutable
`str` and the child's input is an immutable `bytes` copy; the interpreter may keep those until they are collected, and the terminal and the
kernel pipe buffers are outside this script. A key does not outlive the process in any file the script writes (it writes none).

Exit codes: 0 stored or cleared (or, with --check, an AWSCURRENT version is present and not older than --max-age-days, or its age is unknown, or it is a cleared one); 1 an
AWS step failed; 2 refused (not a terminal, entries differ, the key is not acceptable or Tavily refuses it, the accounts file or the selection is wrong, bad arguments,
--max-age-days without --check, --clear together with --check or --max-age-days, the AWS CLI history is on or cannot be read, the ledger is unreadable or full); 3 a
prerequisite is missing (no AWS CLI, the secret does not exist: deploy the stage first); 4 with --check, the secret has no AWSCURRENT version; 5 with --check, the AWSCURRENT
version is older than --max-age-days; 6 (--rotate, --rotate-enterprise) Tavily could not be asked or answered something unreadable (busy, a timeout, a server error): nothing
was stored; 7 the key IS stored but the research route did not confirm it within 6 minutes (or cannot report, or cannot be invoked); 8 (--rotate-enterprise) the key is stored
and confirmed but a previous generation was not deactivated; 9 with --check, the recorded Enterprise generation that made the current key is past its expiry (it is named);
130 interrupted (Ctrl-C) after the key was typed: a summary says what was done and that nothing was undone. Python 3.8+, standard library only.
"""

from __future__ import annotations

import argparse
import datetime
import email.utils
import getpass
import http.client
import json
import math
import os
import re
import shutil
import socket
import ssl
import subprocess  # nosec B404: runs the AWS CLI with an argument list and no shell, nothing else
import sys
import time
import types
import urllib.parse
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping, NamedTuple, Optional, Sequence, TextIO, Tuple

ROOT = Path(__file__).resolve().parents[1]

# The name of the secret the research stack creates empty (a name, never a value). Pinned to infra/lib/config.ts `names.researchSecret` by
# scripts/tests/test_research_key_put.py. The constant has no name with the word secret in it: code scanners (bandit B105, CodeQL) read such a name as a
# stored credential and then report every message that prints it.
RESEARCH_STORE_NAME = "hamh-direct-research-tavily-api-key"
TAG_KEY = "research-key-stored-at"
# --rotate-enterprise records its generations in the secret's tags; the secret policy forbids reading the value, so the tags are the record. The ledger
# lists every generation whose key Tavily may still honor, oldest first, each as request_id@version_id (the secret version that holds its key). No mode
# removes the tags: a plain store, --rotate and --clear print which generations are still active.
TAG_GENERATIONS = "research-key-generations"
TAG_EXPIRES = "research-key-expires-at"  # the expiry of the newest generation
ENTERPRISE_TAGS = (TAG_GENERATIONS, TAG_EXPIRES)
LEDGER_MAX_CHARS = 256  # the longest tag value; three entries of 73 characters fit
# --clear: Secrets Manager refuses an empty SecretString (minimum length 1), so the blank version is one space, and the tag value says it was cleared.
BLANK_VALUE = " "
CLEARED_PREFIX = "cleared-"
KEY_MIN, KEY_MAX = 20, 256
# The AWS calls the script makes. None reads the secret value (get-secret-value is never called). The first three are the plain store and the check;
# rotation adds the version list (read-only, no value), one lambda invoke (the key-state question) and the question about the CLI history, which is
# `configure get cli_history` and nothing else.
AWS_SUBCOMMANDS = ("put-secret-value", "tag-resource", "describe-secret")
ROTATE_SUBCOMMANDS = ("untag-resource", "list-secret-version-ids")
LAMBDA_SUBCOMMANDS = ("invoke",)
CONFIGURE_SUBCOMMANDS = ("get",)
AWS_CALL_TIMEOUT_S = 60  # every AWS CLI call; a call that hangs ends here
EXIT_OK, EXIT_AWS, EXIT_REFUSED, EXIT_PREREQ, EXIT_ABSENT, EXIT_OLD = 0, 1, 2, 3, 4, 5
EXIT_UNASKED, EXIT_UNCONFIRMED, EXIT_NOT_DEACTIVATED, EXIT_EXPIRED = 6, 7, 8, 9
EXIT_INTERRUPTED = 130
# --max-age-days: the default matches HAMH_TAVILY_KEY_MAX_AGE_DAYS of the stage alarms (infra/lib/config.ts DEFAULT_TAVILY_KEY_MAX_AGE_DAYS).
DEFAULT_MAX_AGE_DAYS, MAX_AGE_DAYS_LIMIT = 30, 3650
REDACTED = "<redacted>"
MAX_ERROR_CHARS = 2000
# After the first key is typed the script prints fixed messages, status codes and these AWS error codes, never an error text.
ALLOWED_AWS_CODES = frozenset({
    "AccessDeniedException", "ResourceNotFoundException", "InvalidRequestException", "InvalidParameterException", "InvalidParameterValueException",
    "LimitExceededException", "ThrottlingException", "TooManyRequestsException", "InternalServiceError", "InternalFailure", "ServiceException",
    "ServiceUnavailable", "ServiceUnavailableException", "EncryptionFailure", "DecryptionFailure", "ResourceExistsException", "PreconditionNotMetException",
    "MalformedPolicyDocumentException", "RequestExpired", "ExpiredToken", "ExpiredTokenException", "UnrecognizedClientException", "InvalidSignatureException",
    "NoCredentials", "ParamValidation", "RequestTimeoutException", "ResourceConflictException", "InvalidClientTokenId"})
PROVIDER_WORDS = frozenset({"tavily", "tavily-keyless", "agentcore", "fixture"})
DEPLOY_HINT = (f"the secret {RESEARCH_STORE_NAME} does not exist in this account: deploy the stage first (scripts/deploy.sh); its stack creates the "
               "secret empty, and this script never creates it")

FUNCTION_NAME = "hamh-direct-research-api"
# The wait: the research Lambda keeps a found key 5 minutes, so every instance holds a new version within 5 minutes of the store; 6 minutes leaves margin.
ROUTE_POLL_S, ROUTE_WAIT_S = 15, 360
# The previous generation goes only after the longest an instance may still hold the old key (the 5 minute cache and the 30 second re-read window).
DEACTIVATE_AFTER_S = 330
# Tavily: GET /usage on the API host; generate and deactivate on the key generator host (Enterprise plan only).
USAGE_URL = "https://api.tavily.com/usage"
GENERATE_URL = "https://api-key-generator.tavily.com/generate-keys"
DEACTIVATE_URL = "https://api-key-generator.tavily.com/deactivate-keys"
USAGE_WINDOW_MINUTES = 10  # the usage endpoint allows 10 calls in 10 minutes
HTTP_TIMEOUT_S = 20.0
MAX_BODY_BYTES = 65536
TTL_MIN, TTL_MAX = 1, 8760
LIMIT_MIN, LIMIT_MAX = 1, 100000
USER_AGENT = "ham-research-key-put"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")


class _Parser(argparse.ArgumentParser):
    """An argument error never echoes a value: a key typed where an option belongs must not reach the screen or a log."""

    def error(self, message: str):  # type: ignore[override]
        if "unrecognized arguments" in message:
            message = "unrecognized arguments (values are never echoed; the key is typed at the prompt, not given as an argument)"
        else:
            message = re.sub(r"'[^']*'", "'...'", message)
        self.print_usage(sys.stderr)
        sys.stderr.write(f"{self.prog}: error: {message}\n")
        raise SystemExit(EXIT_REFUSED)


def _age_limit(text: str) -> int:
    """`--max-age-days`: digits only, 1 to MAX_AGE_DAYS_LIMIT."""
    if not re.fullmatch(r"[0-9]{1,5}", text) or not (1 <= int(text) <= MAX_AGE_DAYS_LIMIT):
        raise argparse.ArgumentTypeError(f"must be a whole number from 1 to {MAX_AGE_DAYS_LIMIT}")
    return int(text)


def _whole(low: int, high: int) -> Callable[[str], int]:
    def parse(text: str) -> int:
        if not re.fullmatch(r"[0-9]{1,6}", text) or not (low <= int(text) <= high):
            raise argparse.ArgumentTypeError(f"must be a whole number from {low} to {high}")
        return int(text)
    return parse


def _key_name(text: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,64}", text):
        raise argparse.ArgumentTypeError("must be 1 to 64 letters, digits, dots, underscores or hyphens")
    return text


def build_parser() -> argparse.ArgumentParser:
    p = _Parser(prog="research-key-put.py", description="Store or rotate the Tavily API key in the stage's Secrets Manager secret.",
                allow_abbrev=False)
    p.add_argument("--stage", required=True, choices=["direct"], help="the stage (only direct exists)")
    p.add_argument("--check", action="store_true",
                   help="only report whether an AWSCURRENT version exists and how long ago the secret was last changed "
                        "(never reads the value, no prompt)")
    p.add_argument("--clear", action="store_true",
                   help="store a blank version, so a stage in auto mode uses AgentCore Web Search again (no prompt, no key, never reads the value)")
    p.add_argument("--max-age-days", type=_age_limit, default=None, metavar="N",
                   help=f"with --check: exit {EXIT_OLD} when the key was stored more than N days ago (1 to {MAX_AGE_DAYS_LIMIT}, "
                        f"default {DEFAULT_MAX_AGE_DAYS})")
    p.add_argument("--rotate", action="store_true",
                   help="ask for the new key, probe it with Tavily, store it only if Tavily accepts it, wait up to 6 minutes for the research route to hold it, "
                        "and print the steps that remain for the old key")
    p.add_argument("--rotate-enterprise", action="store_true",
                   help="Enterprise plan only: ask for an admin key, make a new key with ttl_hours and a credit limit, probe and store it like --rotate, "
                        "then deactivate the previous generation")
    p.add_argument("--ttl-hours", type=_whole(TTL_MIN, TTL_MAX), default=None, metavar="N",
                   help=f"with --rotate-enterprise (required): hours before Tavily deactivates the new key, {TTL_MIN} to {TTL_MAX}; "
                        "choose a time longer than the interval between your rotations")
    p.add_argument("--key-limit", type=_whole(LIMIT_MIN, LIMIT_MAX), default=None, metavar="N",
                   help=f"with --rotate-enterprise (required): the credit limit of the new key, {LIMIT_MIN} to {LIMIT_MAX}")
    p.add_argument("--key-type", choices=["development", "production"], default=None,
                   help="with --rotate-enterprise: the type of the new key (default development, the stage's call rate is sized for it)")
    p.add_argument("--key-name", type=_key_name, default=None, metavar="NAME",
                   help="with --rotate-enterprise: the name of the new key (default hamh-<stage>-<UTC date>)")
    return p


def redact(text: str, key: str) -> str:
    """`text` with every spelling of `key` replaced: raw, and as it appears inside a JSON string."""
    if not key:
        return text
    for form in (key, json.dumps(key)[1:-1]):
        text = text.replace(form, REDACTED)
    return text


def redact_all(text: str, keys: Sequence[str]) -> str:
    """`text` with every held key replaced, the longest first: a key that is a substring of a longer one must not leave a piece of the longer one."""
    for key in sorted({k for k in keys if k}, key=len, reverse=True):
        text = redact(text, key)
    return text


def wipe(buf: bytearray) -> None:
    """Overwrite a buffer this script owns with zeros."""
    buf[:] = bytes(len(buf))


def problem_with(key: str) -> Optional[str]:
    """Why `key` is not acceptable, or None. The text never contains the key."""
    if not (KEY_MIN <= len(key) <= KEY_MAX):
        return f"the key must be {KEY_MIN} to {KEY_MAX} characters"
    if not key.isascii() or not all(0x21 <= ord(c) <= 0x7E for c in key):
        return "the key must be printable ASCII without spaces, tabs, newlines or control characters"
    return None


def find_aws(env: Mapping[str, str]) -> Optional[str]:
    override = env.get("HAMH_AWS_BIN")
    if override:
        return override if os.path.isfile(override) and os.access(override, os.X_OK) else None
    return shutil.which("aws", path=env.get("PATH"))


class Cli:
    """The AWS CLI for one profile and Region. `held` are keys typed or made in this run: no call lets one into the child's environment."""

    def __init__(self, aws: str, profile: str, region: str, env: Mapping[str, str], held: Sequence[str] = ()) -> None:
        self.aws, self.profile, self.region, self.env = aws, profile, region, dict(env)
        self.held = tuple(k for k in held if k)

    def holding(self, keys: Sequence[str]) -> "Cli":
        """The same CLI, bound to the keys every later call must keep out of the child's environment (set once, after the prompt)."""
        return Cli(self.aws, self.profile, self.region, self.env, keys)

    def run(self, args: Sequence[str], stdin: Optional[bytes] = None, key: str = "", also: Sequence[str] = ()) -> Tuple[int, str, str]:
        service, call = args[0], args[1]
        allowed = ((service == "secretsmanager" and call in AWS_SUBCOMMANDS + ROTATE_SUBCOMMANDS)
                   or (service == "lambda" and call in LAMBDA_SUBCOMMANDS)
                   or (service == "configure" and call in CONFIGURE_SUBCOMMANDS and list(args[2:3]) == ["cli_history"]))
        if not allowed:
            raise ValueError(f"{service} {call} is not one of the calls this script makes")
        cmd = [self.aws, *args, "--profile", self.profile, "--region", self.region, "--output", "json", "--no-cli-pager"]
        # The child gets this process's environment minus anything that holds a key.
        held = [k for k in (key, *also, *self.held) if k]
        child_env = {k: v for k, v in self.env.items() if not any(h in v for h in held)}
        try:
            p = subprocess.run(cmd, input=stdin if stdin is not None else b"", capture_output=True, env=child_env,  # nosec B603: fixed executable, argument list, no shell  # nosemgrep: dangerous-subprocess-use-audit -- the argument list is built here from fixed parts, never a shell string
                               timeout=AWS_CALL_TIMEOUT_S, check=False)
        except subprocess.TimeoutExpired:
            return 124, "", "the call timed out"
        return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")

    def describe(self) -> Tuple[int, str, str]:
        return self.run(["secretsmanager", "describe-secret", "--secret-id", RESEARCH_STORE_NAME])


def _not_found(text: str) -> bool:
    return "ResourceNotFoundException" in text


def _aws_target():
    sys.path.insert(0, str(ROOT / "scripts" / "lib"))
    import aws_target  # noqa: PLC0415

    return aws_target


_AWS_ERROR = re.compile(r"\(([A-Za-z]+)\)")


def aws_error_word(text: str) -> str:
    """The AWS error code in a CLI error text when it is one of ALLOWED_AWS_CODES, else an empty string. Nothing else of the text is ever printed."""
    for m in _AWS_ERROR.finditer(text):
        if m.group(1) in ALLOWED_AWS_CODES:
            return m.group(1)
    return ""


def _aws_failure(rc: int, text: str) -> str:
    """The fixed words for a failed CLI call: the allowlisted code, the time limit, or nothing more than that the CLI failed."""
    if rc == 124:
        return f"the call timed out after {AWS_CALL_TIMEOUT_S} seconds"
    word = aws_error_word(text)
    return f"AWS answered {word}" if word else "the AWS CLI failed"


def _history_refusal(cli: Cli, target, err: TextIO) -> int:
    """0, or EXIT_REFUSED after saying why. The AWS CLI writes every request, the secret string included, to ~/.aws/cli/history/history.db when the profile has
    `cli_history = enabled`; the CLI itself is asked (`aws configure get cli_history`, the same profile and Region as every other call): the value `enabled`, in any
    letter case, stops the run, exit 1 with no output means not set, and a CLI that cannot answer stops it too (the answer is not guessed from the config file)."""
    rc, so, se = cli.run(["configure", "get", "cli_history"])
    if rc == 0:
        if so.strip().lower() == "enabled":
            err.write(f"refused: the AWS CLI history is on for the profile {target.profile} (cli_history = enabled): the CLI would write the key into "
                      f"~/.aws/cli/history/history.db. Turn it off first: aws configure set cli_history disabled --profile {target.profile}\n")
            return EXIT_REFUSED
        return EXIT_OK
    if rc == 1 and not so.strip() and not se.strip():
        return EXIT_OK
    err.write(f"refused: cannot tell whether the AWS CLI history is on for the profile {target.profile} (`aws configure get cli_history` did not answer): "
              "the CLI would keep the key in ~/.aws/cli/history/history.db if it is. Fix the profile, or turn the history off, and run again\n")
    return EXIT_REFUSED


_ISO = re.compile(r"(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?(Z|[+-]\d{2}:?\d{2})")


def parse_changed(value: object) -> Optional[datetime.datetime]:
    """`LastChangedDate` as the AWS CLI prints it, as an aware UTC time, or None when it is missing or unreadable.

    CLI v2 prints ISO 8601 with an offset ("2030-01-01T12:00:00.123000-06:00" or "...Z"); CLI v1 prints a Unix time in seconds, as a number
    or as text. A time with no offset is unreadable, never guessed."""
    utc = datetime.timezone.utc
    try:
        if isinstance(value, bool):
            return None
        if isinstance(value, (int, float)):
            return datetime.datetime.fromtimestamp(value, utc)
        if not isinstance(value, str):
            return None
        text = value.strip()
        if re.fullmatch(r"[0-9]+(\.[0-9]+)?", text):
            return datetime.datetime.fromtimestamp(float(text), utc)
        m = _ISO.fullmatch(text)
        if not m:
            return None
        y, mo, d, h, mi, sec = (int(m.group(i)) for i in range(1, 7))
        micro = int((m.group(7) or "0")[:6].ljust(6, "0"))
        zone = m.group(8)
        if zone == "Z":
            tz = utc
        else:
            sign = -1 if zone[0] == "-" else 1
            digits = zone[1:].replace(":", "")
            tz = datetime.timezone(sign * datetime.timedelta(hours=int(digits[:2]), minutes=int(digits[2:])))
        return datetime.datetime(y, mo, d, h, mi, sec, micro, tzinfo=tz).astimezone(utc)
    except (ValueError, OverflowError, OSError):
        return None


def _tag_map(doc: Mapping[str, object]) -> Dict[str, str]:
    """All string tags of a describe-secret answer."""
    tags = doc.get("Tags")
    found: Dict[str, str] = {}
    for tag in tags if isinstance(tags, list) else []:
        if isinstance(tag, dict) and isinstance(tag.get("Key"), str) and isinstance(tag.get("Value"), str):
            found[str(tag["Key"])] = str(tag["Value"])
    return found


def _stored_at_tag(doc: Mapping[str, object]) -> Optional[str]:
    """The value of the `research-key-stored-at` tag in a describe-secret answer, if it has one."""
    return _tag_map(doc).get(TAG_KEY)


_ID = r"[A-Za-z0-9-]{8,64}"
_LEDGER_ENTRY = re.compile(rf"({_ID})@({_ID})")


def _dedupe(entries: Sequence[Tuple[str, str]]) -> List[Tuple[str, str]]:
    seen = set()
    kept: List[Tuple[str, str]] = []
    for rid, vid in entries:
        if rid not in seen:
            seen.add(rid)
            kept.append((rid, vid))
    return kept


def parse_ledger(value: Optional[str]) -> Optional[List[Tuple[str, str]]]:
    """The ledger tag as [(request_id, version_id)], oldest first and without duplicates; [] for no ledger; None when the value is not a ledger, so that nothing
    destructive is ever decided from it."""
    if value is None or value == "":
        return []
    if len(value) > LEDGER_MAX_CHARS:
        return None
    entries = []
    for part in value.split(","):
        m = _LEDGER_ENTRY.fullmatch(part)
        if not m:
            return None
        entries.append((m.group(1), m.group(2)))
    return _dedupe(entries)


def format_ledger(entries: Sequence[Tuple[str, str]]) -> str:
    return ",".join(f"{rid}@{vid}" for rid, vid in _dedupe(entries))


def _current_version(doc: Mapping[str, object]) -> Optional[str]:
    stages = doc.get("VersionIdsToStages")
    for version, labels in (stages.items() if isinstance(stages, dict) else []):
        if isinstance(labels, list) and "AWSCURRENT" in labels:
            return str(version)
    return None


def _ledger_notice(doc: Mapping[str, object], out: TextIO) -> None:
    """A plain store, --rotate and --clear leave the Enterprise tags alone and say which generations are still active in Tavily and until when."""
    tags = _tag_map(doc)
    raw = tags.get(TAG_GENERATIONS)
    entries = parse_ledger(raw) if raw is not None else []
    if entries is None:
        out.write(f"note: the tag {TAG_GENERATIONS} is unreadable, so the Enterprise generations it should list cannot be named; it is left as it is\n")
        return
    if not entries:
        return
    expires = tags.get(TAG_EXPIRES)
    until = f"; the newest expires at {expires}" if expires else ""
    out.write("note: Enterprise generations are still active in Tavily: request " + ", ".join(rid for rid, _ in entries) + until + ". This run leaves the tags "
              "as they are. Deactivate them with POST /deactivate-keys and an admin key, or let them expire.\n")


def _age_lines(doc: Mapping[str, object], now: datetime.datetime, limit: int, out: TextIO) -> int:
    """Prints the age of the current key and the limit; returns EXIT_OK, or EXIT_OLD when the key is older than `limit` days."""
    changed = parse_changed(doc.get("LastChangedDate"))
    if changed is None:
        out.write("age: unknown (describe-secret gave no readable LastChangedDate)\n")
        return EXIT_OK
    days = max(0.0, (now.astimezone(datetime.timezone.utc) - changed).total_seconds() / 86400.0)
    tag = _stored_at_tag(doc)
    tag_text = f"tag {TAG_KEY}={tag}" if tag else f"no {TAG_KEY} tag"
    stamp = changed.strftime("%Y-%m-%dT%H:%M:%SZ")
    out.write(f"age: stored {days:.1f} days ago (secret last changed {stamp}; {tag_text})\n")
    out.write(f"limit: {limit} days\n")
    if days > limit:
        out.write(f"OLD: the stored key is older than {limit} days; rotate it\n")
        return EXIT_OLD
    return EXIT_OK


def _ledger_lines(doc: Mapping[str, object], now: datetime.datetime, out: TextIO) -> int:
    """What the ledger says: the generation that made the CURRENT version with its expiry (EXIT_EXPIRED when that one is past it), and every other generation
    that is still active. A generation's expiry is recorded for the newest entry only, so it is not guessed for an older one."""
    tags = _tag_map(doc)
    raw = tags.get(TAG_GENERATIONS)
    if raw is None:
        return EXIT_OK
    entries = parse_ledger(raw)
    if entries is None:
        out.write(f"ledger: the tag {TAG_GENERATIONS} is unreadable; fix or remove it before the next --rotate-enterprise\n")
        return EXIT_OK
    if not entries:
        return EXIT_OK
    current = _current_version(doc)
    holder = next((e for e in entries if e[1] == current), None)
    code = EXIT_OK
    if holder is not None:
        rid, vid = holder
        out.write(f"generation: request {rid} made the current key (version {vid})\n")
        expires = tags.get(TAG_EXPIRES)
        if holder == entries[-1] and expires is not None:
            when = parse_changed(expires)
            if when is None:
                out.write(f"expires_at: {expires} (unreadable)\n")
            else:
                days = (when - now.astimezone(datetime.timezone.utc)).total_seconds() / 86400.0
                note = f"in {days:.1f} days" if days >= 0 else f"EXPIRED {-days:.1f} days ago"
                out.write(f"expires_at: {expires} ({note})\n")
                if days < 0:
                    out.write(f"EXPIRED: the generation that made the current key (request {rid}, version {vid}) expired at {expires}; store a new key\n")
                    code = EXIT_EXPIRED
        else:
            out.write(f"expiry not recorded for request {rid}\n")
    for rid, vid in entries:
        if (rid, vid) != holder:
            out.write(f"still active: request {rid} (version {vid}, not the current key); deactivate it with POST /deactivate-keys or let it expire\n")
    return code


PROVIDER_TAVILY = "auto: Tavily is used (a key is stored)"
PROVIDER_AGENTCORE = "auto: AgentCore Web Search is used (no key)"
PROVIDER_NOTE = ("note: this is the choice of a stage in auto mode (HAMH_RESEARCH_PROVIDER unset or auto); a stage pinned to another word does what the pin says. "
                 f"It is read from the tag {TAG_KEY}, not from the value, which this script cannot read")
_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")


def _provider_lines(doc: Mapping[str, object], out: TextIO) -> Optional[str]:
    """Which provider a stage in auto mode uses while an AWSCURRENT version is present. The value is not readable here, so the tag this script writes tells a
    stored key (a date) from a cleared one (`cleared-<date>`); any other state is reported as not known. Returns "cleared" when the version is a blank one."""
    tag = _stored_at_tag(doc)
    kind: Optional[str] = None
    if tag is not None and _DATE.fullmatch(tag):
        out.write(PROVIDER_TAVILY + "\n")
        kind = "stored"
    elif tag is not None and tag.startswith(CLEARED_PREFIX) and _DATE.fullmatch(tag[len(CLEARED_PREFIX):]):
        out.write(f"auto: AgentCore Web Search is used (no key; cleared on {tag[len(CLEARED_PREFIX):]})\n")
        kind = "cleared"
    else:
        out.write(f"auto: cannot tell whether the current version is a key or blank (no usable {TAG_KEY} tag: it was not stored with this script); "
                  "the value was not read\n")
    out.write(PROVIDER_NOTE + "\n")
    return kind


def _check(cli: Cli, out: TextIO, err: TextIO, now: datetime.datetime, limit: int) -> int:
    rc, so, se = cli.describe()
    if rc != 0:
        if _not_found(se):
            out.write(f"absent: the secret {RESEARCH_STORE_NAME} does not exist. {DEPLOY_HINT}\n")
            return EXIT_ABSENT
        err.write(f"FAIL describe-secret: {se.strip()[:MAX_ERROR_CHARS]}\n")
        return EXIT_AWS
    try:
        doc = json.loads(so)
    except ValueError:
        err.write("FAIL describe-secret: the CLI answered something that is not JSON\n")
        return EXIT_AWS
    stages: Dict[str, List[str]] = doc.get("VersionIdsToStages") or {}
    current = [v for v, s in stages.items() if "AWSCURRENT" in (s or [])]
    if current:
        out.write(f"present: secret {RESEARCH_STORE_NAME} has an AWSCURRENT version ({current[0]}); the value was not read\n")
        if _provider_lines(doc, out) == "cleared":
            # A blank version has no key to rotate: no age limit applies, however old the version is.
            out.write("age: not applicable (the stored key was cleared)\n")
            return EXIT_OK
        rc = _age_lines(doc, now, limit, out)
        return EXIT_EXPIRED if _ledger_lines(doc, now, out) == EXIT_EXPIRED else rc
    out.write(f"absent: secret {RESEARCH_STORE_NAME} exists but has no AWSCURRENT version; run without --check to store the key\n")
    out.write(PROVIDER_AGENTCORE + "\n" + PROVIDER_NOTE + "\n")
    return EXIT_ABSENT


def _json_doc(text: str) -> Dict[str, Any]:
    try:
        doc = json.loads(text)
    except ValueError:
        return {}
    return doc if isinstance(doc, dict) else {}


def _clear(cli: Cli, target, clock: Callable[[], datetime.datetime], out: TextIO, err: TextIO) -> int:
    """--clear: a blank version of the secret and the tag that says so. The same three AWS calls as the store; nothing is read or typed."""
    rc, pre, se = cli.describe()
    if rc != 0:
        if _not_found(se):
            err.write(f"{DEPLOY_HINT}\n")
            return EXIT_PREREQ
        err.write(f"FAIL describe-secret: {se.strip()[:MAX_ERROR_CHARS]}\n")
        return EXIT_AWS
    out.write(f"Clearing the stored Tavily key in secret {RESEARCH_STORE_NAME} (account {target.account}, {target.region}, profile {target.profile}).\n")
    rc, so, se = cli.run(["secretsmanager", "put-secret-value", "--secret-id", RESEARCH_STORE_NAME, "--secret-string", "file:///dev/stdin"],
                         stdin=BLANK_VALUE.encode("ascii"))
    if rc != 0:
        if _not_found(se):
            err.write(f"{DEPLOY_HINT}\n")
            return EXIT_PREREQ
        err.write(f"FAIL put-secret-value: {se.strip()[:MAX_ERROR_CHARS]}\n")
        return EXIT_AWS
    try:
        doc = json.loads(so)
        arn, version = str(doc["ARN"]), str(doc["VersionId"])
    except (ValueError, KeyError, TypeError):
        err.write("FAIL put-secret-value: the CLI answered without an ARN and a version id; check the secret with --check before trying again\n")
        return EXIT_AWS
    out.write(f"secret: {arn}\nversion: {version}\ncleared: the stored key is replaced by a blank version\n")
    value = CLEARED_PREFIX + clock().astimezone(datetime.timezone.utc).date().isoformat()
    rc, _so, se = cli.run(["secretsmanager", "tag-resource", "--secret-id", RESEARCH_STORE_NAME, "--tags", f"Key={TAG_KEY},Value={value}"])
    if rc != 0:
        err.write(f"FAIL tag-resource: {se.strip()[:MAX_ERROR_CHARS]}\nthe key IS cleared (version {version}); only the tag {TAG_KEY} is missing\n")
        return EXIT_AWS
    out.write(f"tagged: {TAG_KEY}={value}\n")
    _ledger_notice(_json_doc(pre), out)
    out.write("next: nothing to restart; the research route reads the secret again within 5 minutes, and a stage in auto mode then searches with "
              "AgentCore Web Search (no key; queries of at most 200 characters; no page fetch). A stage pinned to tavily answers no_key. "
              "The previous key stays in the secret as its previous version until the next store; delete it in the Tavily dashboard to revoke it. "
              "To store a key again, run this script without --clear.\n")
    return EXIT_OK


# ---- Tavily over HTTPS ---------------------------------------------------------------------------------------------------------------------


class HttpAnswer(NamedTuple):
    status: int
    headers: Dict[str, str]  # keys lower-cased
    body: bytes


HttpFn = Callable[[str, str, Mapping[str, str], Optional[bytes], float], HttpAnswer]


class _VerifiedTls(http.client.HTTPConnection):
    """An HTTP connection over TLS 1.2 or later with the platform's default context: the certificate chain and the host name are verified
    (`ssl.create_default_context()` requires both) and the context sets its own floor of TLS 1.2. It does what http.client.HTTPSConnection does and
    names its context where it wraps the socket."""

    def connect(self) -> None:
        raw = socket.create_connection((self.host, self.port), self.timeout)
        ctx = ssl.create_default_context()
        ctx.minimum_version = ssl.TLSVersion.TLSv1_2  # the floor is set here, not left to the platform's default; verification stays on
        self.sock = ctx.wrap_socket(raw, server_hostname=self.host)


def default_http(method: str, url: str, headers: Mapping[str, str], body: Optional[bytes], timeout: float) -> HttpAnswer:
    """One request; an HTTP error status is an answer. A network failure or a timeout is an OSError. No redirect is followed (http.client never does), so a
    bearer token is sent to the one host named here. TLS is verified with the platform's certificate store. Plain http is allowed for a loopback address only (the
    tests' server). The body is read up to MAX_BODY_BYTES."""
    parts = urllib.parse.urlsplit(url)
    host = parts.hostname or ""
    if parts.scheme == "https":
        conn: http.client.HTTPConnection = _VerifiedTls(host, parts.port or 443, timeout=timeout)
    elif parts.scheme == "http" and host in LOOPBACK_HOSTS:
        conn = http.client.HTTPConnection(host, parts.port or 80, timeout=timeout)
    else:
        raise ValueError("only https is used (plain http only to a loopback address)")
    target = (parts.path or "/") + (("?" + parts.query) if parts.query else "")
    try:
        conn.request(method, target, body=body, headers=dict(headers))
        resp = conn.getresponse()
        data = resp.read(MAX_BODY_BYTES)
        return HttpAnswer(resp.status, {k.lower(): v for k, v in resp.getheaders()}, data)
    except http.client.HTTPException as exc:
        raise OSError(f"the connection failed ({exc.__class__.__name__})") from None
    finally:
        conn.close()


def retry_minutes(headers: Mapping[str, str], now: Optional[datetime.datetime] = None) -> int:
    """How many minutes to wait after a 429: `retry-after` as whole seconds (1 to 86400) or as an HTTP date (up to a day ahead of `now`), rounded up; a date in
    the past is a minute; anything else is the window of the usage endpoint."""
    for name, value in headers.items():
        if name.lower() != "retry-after":
            continue
        text = value.strip()
        if re.fullmatch(r"[0-9]{1,6}", text):
            return int(math.ceil(int(text) / 60.0)) if 1 <= int(text) <= 86400 else USAGE_WINDOW_MINUTES
        if now is not None:
            try:
                when = email.utils.parsedate_to_datetime(text)
            except (TypeError, ValueError, IndexError):
                return USAGE_WINDOW_MINUTES
            if when.tzinfo is None:
                when = when.replace(tzinfo=datetime.timezone.utc)
            seconds = (when - now).total_seconds()
            if seconds <= 0:
                return 1
            if seconds <= 86400:
                return int(math.ceil(seconds / 60.0))
        return USAGE_WINDOW_MINUTES
    return USAGE_WINDOW_MINUTES


def _minutes_text(minutes: int) -> str:
    return f"{minutes} minute{'s' if minutes != 1 else ''}"


class Probe(NamedTuple):
    kind: str  # accepted | refused | exhausted | busy | failed
    message: str
    usage: Optional[int] = None
    limit: Optional[int] = None


def _count(value: object) -> Optional[int]:
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None


def probe_key(http: HttpFn, key: str, now: Optional[datetime.datetime] = None) -> Probe:
    """Asks Tavily whether it accepts `key`: GET /usage. Prints nothing; the message never holds the key."""
    try:
        answer = http("GET", USAGE_URL, {"Authorization": "Bearer " + key, "Accept": "application/json", "User-Agent": USER_AGENT}, None, HTTP_TIMEOUT_S)
    except (OSError, ValueError) as exc:
        return Probe("failed", f"the request failed ({exc.__class__.__name__})")
    if answer.status in (401, 403):
        return Probe("refused", f"Tavily refuses this key (status {answer.status})")
    if answer.status == 429:
        return Probe("busy", f"the usage endpoint of Tavily allows {USAGE_WINDOW_MINUTES} calls in {USAGE_WINDOW_MINUTES} minutes and answered 429: "
                             f"retry in {_minutes_text(retry_minutes(answer.headers, now))}")
    if answer.status != 200:
        return Probe("failed", f"Tavily answered status {answer.status}")
    inner = _json_doc(answer.body.decode("utf-8", "replace")).get("key")
    usage = _count(inner.get("usage")) if isinstance(inner, dict) else None
    limit_raw = inner.get("limit") if isinstance(inner, dict) else None
    # The documented answer carries `limit` (null when the key has none); a body without the member is not the usage answer.
    if usage is None or "limit" not in inner or (limit_raw is not None and _count(limit_raw) is None):
        return Probe("failed", "Tavily answered 200 but the body is not the usage answer")
    limit = _count(limit_raw)
    if limit is not None and usage >= limit:
        return Probe("exhausted", f"Tavily accepts the key but it has no credits left (usage {usage} of {limit})", usage, limit)
    return Probe("accepted", "", usage, limit)


def _probe_exit(probe: Probe, err: TextIO) -> int:
    err.write(f"{'refused' if probe.kind in ('refused', 'exhausted') else 'FAIL probe'}: {probe.message}; nothing was stored\n")
    return EXIT_REFUSED if probe.kind in ("refused", "exhausted") else EXIT_UNASKED


# ---- the research route's answer ---------------------------------------------------------------------------------------------------------


def key_state_event() -> Dict[str, Any]:
    """The event of the key-state question: an API Gateway REST (v1) proxy event for POST /internal/research/key-state with no caller identity, which
    research-api reads like the five routes' events. It is the same JSON as fixtures/research/key-state-event.json (both tests pin it)."""
    path = "/internal/research/key-state"
    return {
        "resource": path, "path": path, "httpMethod": "POST",
        "headers": {"content-type": "application/json"}, "multiValueHeaders": {"content-type": ["application/json"]},
        "queryStringParameters": None, "multiValueQueryStringParameters": None, "pathParameters": None, "stageVariables": None,
        "isBase64Encoded": False, "body": "{}",
        "requestContext": {"accountId": "000000000000", "resourceId": "operator", "stage": "direct", "requestId": "operator-key-state",
                           "resourcePath": path, "httpMethod": "POST", "apiId": "operator", "path": path, "identity": {}},
    }


class RouteAnswer(NamedTuple):
    kind: str  # state | unknown | cannot | retry
    provider: Optional[str] = None
    version: Optional[str] = None
    detail: str = ""


def _json_docs(text: str) -> List[Any]:
    """Every JSON document in `text`, in order (`aws lambda invoke ... /dev/stdout` prints the function's payload, then its own metadata)."""
    docs: List[Any] = []
    decoder = json.JSONDecoder()
    i = 0
    while i < len(text):
        if text[i].isspace():
            i += 1
            continue
        try:
            doc, i = decoder.raw_decode(text, i)
        except ValueError:
            break
        docs.append(doc)
    return docs


_TRANSIENT = re.compile(r"TooManyRequests|Throttl|ServiceException|ServiceUnavailable|RequestTimeout|timed out|Could not connect|Read timeout", re.I)
_VERSION_ID = re.compile(r"[A-Za-z0-9-]{8,64}")


def ask_route(cli: Cli, keys: Sequence[str]) -> RouteAnswer:
    """One key-state question to research-api. Only the provider word and the version id are kept, and only in their expected shapes."""
    rc, so, se = cli.run(["lambda", "invoke", "--function-name", FUNCTION_NAME, "--cli-binary-format", "raw-in-base64-out",
                          "--payload", json.dumps(key_state_event()), "/dev/stdout"], also=keys)
    if rc != 0:
        if _TRANSIENT.search(se):
            return RouteAnswer("retry", detail=aws_error_word(se))
        return RouteAnswer("cannot", detail=aws_error_word(se))
    docs = _json_docs(so)
    meta = docs[1] if len(docs) > 1 and isinstance(docs[1], dict) else {}
    payload = docs[0] if docs else None
    if meta.get("FunctionError") or not isinstance(payload, dict):
        return RouteAnswer("retry", detail="the function failed")
    status = payload.get("statusCode")
    if status == 404:
        return RouteAnswer("unknown")
    body = payload.get("body")
    if status != 200 or not isinstance(body, str):
        return RouteAnswer("retry", detail=f"status {status}" if isinstance(status, int) else "")
    doc = _json_doc(body)
    provider, version = doc.get("provider"), doc.get("versionId")
    if not isinstance(provider, str) or provider not in PROVIDER_WORDS or (version is not None and not (isinstance(version, str) and _VERSION_ID.fullmatch(version))):
        return RouteAnswer("retry", detail="unreadable answer")
    return RouteAnswer("state", provider, version)


class WaitResult(NamedTuple):
    outcome: str  # confirmed | timeout | unknown | cannot
    last: RouteAnswer


def wait_for_route(cli: Cli, version: str, started: float, mono: Callable[[], float], sleep: Callable[[float], None], keys: Sequence[str]) -> WaitResult:
    """Asks every ROUTE_POLL_S seconds until the route reports `version` under the provider tavily, for up to ROUTE_WAIT_S seconds after `started` (a reading of
    `mono`, the monotonic clock). An answer that arrives after the deadline is not accepted, whatever it says."""
    deadline = started + ROUTE_WAIT_S
    last = RouteAnswer("retry")
    while True:
        answer = ask_route(cli, keys)
        now = mono()
        if answer.kind in ("unknown", "cannot"):
            return WaitResult(answer.kind, answer)
        if answer.kind == "state":
            last = answer
        if now > deadline:
            return WaitResult("timeout", last)
        if answer.kind == "state" and answer.provider == "tavily" and answer.version == version:
            return WaitResult("confirmed", answer)
        remaining = deadline - now
        if remaining <= 0:
            return WaitResult("timeout", last)
        sleep(min(float(ROUTE_POLL_S), remaining))


# ---- the rotation modes ----------------------------------------------------------------------------------------------------------------------


def _ask_key(ask: Callable[[str], str], label: str, err: TextIO) -> Tuple[Optional[str], int]:
    """The key twice at the hidden prompt; (None, exit code) when it is refused or the owner cancels."""
    try:
        first = ask(f"{label}: ")
        bad = problem_with(first)
        if bad:
            err.write(f"refused: {bad}\n")
            return None, EXIT_REFUSED
        second = ask(f"{label} again: ")
    except (EOFError, KeyboardInterrupt):
        err.write("cancelled: nothing was stored\n")
        return None, EXIT_REFUSED
    if first != second:
        err.write("refused: the two entries differ; nothing was stored\n")
        return None, EXIT_REFUSED
    return first, EXIT_OK


class Generated(NamedTuple):
    key: str
    request_id: str
    expires_at: str


_REQUEST_ID = re.compile(r"[A-Za-z0-9-]{8,64}")
_EXPIRES = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:?[0-9]{2})")


def _bearer(key: str) -> Dict[str, str]:
    return {"Authorization": "Bearer " + key, "Accept": "application/json", "Content-Type": "application/json", "User-Agent": USER_AGENT}


def _deactivate(http: HttpFn, admin: str, request_id: str) -> Optional[str]:
    """POST /deactivate-keys for one generation request; None when Tavily deactivated it, else why not in fixed words (a status or an exception class)."""
    try:
        answer = http("POST", DEACTIVATE_URL, _bearer(admin), json.dumps({"request_id": request_id}).encode("ascii"), HTTP_TIMEOUT_S)
    except (OSError, ValueError) as exc:
        return f"the request failed ({exc.__class__.__name__})"
    if answer.status != 200:
        return f"Tavily answered status {answer.status}"
    return None


def _by_hand(request_id: str) -> str:
    return f"to deactivate it by hand: POST {DEACTIVATE_URL} with an admin key as the bearer token and the body {{\"request_id\": \"{request_id}\"}}"


def _cleanup_generation(http: HttpFn, admin: str, request_id: Optional[str], out: TextIO, err: TextIO) -> None:
    """A key made by this run that is certain not to be stored is deactivated again: nothing else holds it. Only called when no store was attempted or the
    secret was read back and holds no new version."""
    if not request_id or not _REQUEST_ID.fullmatch(request_id):
        return
    why = _deactivate(http, admin, request_id)
    if why is None:
        out.write(f"cleanup: the key made by this run (request {request_id}) was deactivated again\n")
    else:
        err.write(f"cleanup: the key made by this run (request {request_id}) is NOT deactivated ({why}); it expires by itself at its ttl; {_by_hand(request_id)}\n")


def _generate(http: HttpFn, admin: str, args, stage: str, today: str, now: datetime.datetime, err: TextIO, out: TextIO) -> Tuple[Optional[Generated], int]:
    """POST /generate-keys: one user-level key. (None, exit code) with a message when Tavily refuses or answers something this script will not store."""
    body = {"permission_level": "user", "name": args.key_name or f"hamh-{stage}-{today}", "key_type": args.key_type or "development",
            "key_limit": args.key_limit, "ttl_hours": args.ttl_hours, "count": 1}
    try:
        answer = http("POST", GENERATE_URL, _bearer(admin), json.dumps(body).encode("ascii"), HTTP_TIMEOUT_S)
    except (OSError, ValueError) as exc:
        err.write(f"FAIL generate-keys: the request failed ({exc.__class__.__name__}); nothing was stored\n")
        return None, EXIT_UNASKED
    if answer.status in (401, 403):
        err.write(f"refused: Tavily refuses the admin key (status {answer.status}) or the plan does not include key generation "
                  "(Enterprise plan only); nothing was stored\n")
        return None, EXIT_REFUSED
    if answer.status == 429:
        err.write(f"FAIL generate-keys: Tavily answered 429: retry in {_minutes_text(retry_minutes(answer.headers, now))}; nothing was stored\n")
        return None, EXIT_UNASKED
    doc = _json_doc(answer.body.decode("utf-8", "replace"))
    request_id = doc.get("request_id") if isinstance(doc.get("request_id"), str) else None
    if answer.status != 200:
        err.write(f"FAIL generate-keys: Tavily answered status {answer.status}; nothing was stored\n")
        return None, EXIT_UNASKED
    made = doc.get("keys")
    expires = doc.get("expires_at")
    problems = []
    if not (isinstance(made, list) and len(made) == 1 and isinstance(made[0], str) and problem_with(made[0]) is None):
        problems.append("it did not return exactly one usable key")
    if not (isinstance(expires, str) and _EXPIRES.fullmatch(expires)):
        problems.append("expires_at is missing or unreadable")
    if not (request_id and _REQUEST_ID.fullmatch(request_id)):
        problems.append("request_id is missing or unreadable")
    if problems:
        err.write("FAIL generate-keys: Tavily answered 200 but " + "; and ".join(problems) + "; nothing was stored\n")
        _cleanup_generation(http, admin, request_id, out, err)
        return None, EXIT_UNASKED
    return Generated(made[0], str(request_id), str(expires)), EXIT_OK


class Stored(NamedTuple):
    outcome: str  # stored | not_stored | unknown
    version: Optional[str]
    code: int  # the exit code when the key is not stored
    mono: float  # the monotonic time right after the put returned
    wall: datetime.datetime  # the wall time of the same moment
    treated: bool  # the put answer was unusable and the version comes from the version list


def _list_versions(cli: Cli) -> Optional[Dict[str, List[str]]]:
    """{version id: staging labels} from list-secret-version-ids (no value), or None when the call failed or the answer is not that list."""
    rc, so, _se = cli.run(["secretsmanager", "list-secret-version-ids", "--secret-id", RESEARCH_STORE_NAME])
    versions = _json_doc(so).get("Versions") if rc == 0 else None
    if not isinstance(versions, list):
        return None
    found: Dict[str, List[str]] = {}
    for entry in versions:
        if not isinstance(entry, dict) or not isinstance(entry.get("VersionId"), str):
            return None
        labels = entry.get("VersionStages")
        found[entry["VersionId"]] = [x for x in labels if isinstance(x, str)] if isinstance(labels, list) else []
    return found


def _put_secret(cli: Cli, key: str, buf: bytearray, before: Sequence[str], mono: Callable[[], float], clock: Callable[[], datetime.datetime],
                out: TextIO, err: TextIO) -> Stored:
    """put-secret-value with the key on stdin. A put that failed or answered something unusable is followed by the version list, compared with the versions seen
    before the put: a new version means the key is stored; a list that succeeded with no new version means it is not; anything else is unknown."""
    rc, so, se = cli.run(["secretsmanager", "put-secret-value", "--secret-id", RESEARCH_STORE_NAME, "--secret-string", "file:///dev/stdin"], stdin=bytes(buf), key=key)
    stored_mono = mono()
    stored_wall = clock()
    if rc == 0:
        try:
            doc = json.loads(so)
            arn, version = str(doc["ARN"]), str(doc["VersionId"])
        except (ValueError, KeyError, TypeError):
            err.write("FAIL put-secret-value: the CLI answered without an ARN and a version id\n")
        else:
            out.write(f"secret: {arn}\nversion: {version}\n")
            return Stored("stored", version, EXIT_OK, stored_mono, stored_wall, False)
    else:
        err.write(f"FAIL put-secret-value: {_aws_failure(rc, se)}\n")
    code = EXIT_PREREQ if rc != 0 and _not_found(se) else EXIT_AWS
    if code == EXIT_PREREQ:
        err.write(f"{DEPLOY_HINT}\n")
    versions = _list_versions(cli)
    if versions is None:
        return Stored("unknown", None, code, stored_mono, stored_wall, False)
    new = [v for v in versions if v not in before]
    if not new:
        return Stored("not_stored", None, code, stored_mono, stored_wall, False)
    if len(new) == 1:
        out.write(f"version: {new[0]}\nthe put answer was not usable, but the secret holds a new version: treated as stored\n")
        return Stored("stored", new[0], EXIT_OK, stored_mono, stored_wall, True)
    return Stored("unknown", None, code, stored_mono, stored_wall, False)


def _write_tags(cli: Cli, pairs: Sequence[Tuple[str, str]], as_json: bool, version: str, out: TextIO, err: TextIO) -> bool:
    """tag-resource. The ledger holds commas, so a write that carries it is one JSON array; the plain pairs stay the plain store's shorthand."""
    tags = [json.dumps([{"Key": k, "Value": v} for k, v in pairs])] if as_json else [f"Key={k},Value={v}" for k, v in pairs]
    rc, _so, se = cli.run(["secretsmanager", "tag-resource", "--secret-id", RESEARCH_STORE_NAME, "--tags", *tags])
    if rc != 0:
        err.write(f"FAIL tag-resource: {_aws_failure(rc, se)}\nthe key IS stored (version {version}); only the tags are missing\n")
        return False
    out.write("tagged: " + ", ".join(f"{k}={v}" for k, v in pairs) + "\n")
    return True


def _route_report(result: WaitResult, version: str, out: TextIO, err: TextIO) -> bool:
    """Prints what the wait found; True when the route holds the new version."""
    if result.outcome == "confirmed":
        out.write(f"route: the research route holds the new key (version {version}); this came from one running instance of the function, and any other "
                  "instance reads the new version within 5 minutes of the store, or at once if Tavily refuses the old key\n")
        return True
    if result.outcome == "unknown":
        err.write(f"FAIL route: this stage's research route cannot report its key yet (it answered not found: the research stack was deployed before the "
                  f"key-state question existed; deploy it, scripts/deploy.sh). The key IS stored (version {version}) and the route reads it within 5 minutes "
                  "of the store by its cache rule, but that is not confirmed. Do not delete the old key until then.\n")
    elif result.outcome == "cannot":
        word = result.last.detail or "an AWS error"
        err.write(f"FAIL route: cannot ask the research route ({word}): the profile needs lambda:InvokeFunction on {FUNCTION_NAME}. "
                  f"The key IS stored (version {version}); the route reads it within 5 minutes of the store by its cache rule, but that is not confirmed. "
                  "Do not delete the old key until then.\n")
    else:
        seen = (f"last answer: provider {result.last.provider}, version {result.last.version or 'none'}" if result.last.kind == "state"
                else "it gave no readable answer")
        err.write(f"FAIL route: the research route did not report version {version} within {ROUTE_WAIT_S // 60} minutes ({seen}). The key IS stored "
                  "(version " + version + "). Do not delete the old key yet; look at the log group of the research-api function for errors reading the secret.\n")
    return False


def _rotate(args, cli: Cli, target, clock: Callable[[], datetime.datetime], mono: Callable[[], float], tz: datetime.tzinfo,
            sleep: Callable[[float], None], ask: Callable[[str], str], is_tty: Callable[[], bool], http: HttpFn, out: TextIO, err: TextIO) -> int:
    enterprise = bool(args.rotate_enterprise)
    if not is_tty():
        err.write("refused: run this in a terminal; the key is typed at a hidden prompt and is never read from a pipe or a file\n")
        return EXIT_REFUSED
    refused = _history_refusal(cli, target, err)
    if refused:
        return refused
    rc, pre, se = cli.describe()
    if rc != 0:
        if _not_found(se):
            err.write(f"{DEPLOY_HINT}\n")
            return EXIT_PREREQ
        err.write(f"FAIL describe-secret: {se.strip()[:MAX_ERROR_CHARS]}\n")
        return EXIT_AWS
    doc = _json_doc(pre)
    stages = doc.get("VersionIdsToStages")
    raw = _tag_map(doc).get(TAG_GENERATIONS)
    ledger = parse_ledger(raw) if raw is not None else []
    if enterprise:
        if ledger is None:
            err.write(f"refused: the tag {TAG_GENERATIONS}, the ledger of Enterprise generations, is unreadable; fix or remove it first, or a generation that is "
                      "still active could be forgotten\n")
            return EXIT_REFUSED
        if len(format_ledger(ledger)) + 1 + 73 > LEDGER_MAX_CHARS:
            err.write("refused: the ledger of Enterprise generations in the secret's tags is full; deactivate the generations it lists (they are named by "
                      "`--check`) or let them expire, then run again\n")
            return EXIT_REFUSED

    out.write(f"Paste the {'admin ' if enterprise else 'new '}Tavily API key at the prompt (typing is hidden). "
              f"It {'makes a new key that ' if enterprise else ''}is stored in secret {RESEARCH_STORE_NAME} "
              f"(account {target.account}, {target.region}, profile {target.profile}).\n")
    typed, code = _ask_key(ask, "Tavily admin API key" if enterprise else "Tavily API key", err)
    if typed is None:
        return code
    ctx = types.SimpleNamespace(
        args=args, cli=cli.holding([typed]), clock=clock, mono=mono, tz=tz, sleep=sleep, http=http, out=out, err=err, enterprise=enterprise,
        typed=typed, admin=typed if enterprise else "", held=[typed], today=clock().astimezone(datetime.timezone.utc).date().isoformat(),
        before=list(stages) if isinstance(stages, dict) else [], ledger=ledger or [], doc=doc, gen=None, version=None, stored_mono=0.0,
        stored_wall=clock(), tags_ok=True)
    try:
        return _rotation(ctx)
    except KeyboardInterrupt:
        return _interrupted(ctx)
    finally:
        ctx.typed = ctx.admin = ""


def _rotation(ctx) -> int:
    out, err = ctx.out, ctx.err
    key = ctx.typed
    if ctx.enterprise:
        gen, code = _generate(ctx.http, ctx.admin, ctx.args, ctx.args.stage, ctx.today, ctx.clock(), err, out)
        if gen is None:
            return code
        ctx.gen = gen
        ctx.held = [ctx.admin, gen.key]
        ctx.cli = ctx.cli.holding(ctx.held)
        out.write(f"generated: a user-level key for {ctx.args.ttl_hours} hours with {ctx.args.key_limit} credits (request {gen.request_id}, expires {gen.expires_at})\n")
        key = gen.key
    probe = probe_key(ctx.http, key, ctx.clock())
    if probe.kind != "accepted":
        code = _probe_exit(probe, err)
        if ctx.gen:
            _cleanup_generation(ctx.http, ctx.admin, ctx.gen.request_id, out, err)
        return code
    limit_text = f"of {probe.limit} credits" if probe.limit is not None else "(no limit set)"
    out.write(f"probe: Tavily accepts the key (usage {probe.usage} {limit_text})\n")

    buf = bytearray(key.encode("ascii"))
    try:
        stored = _put_secret(ctx.cli, key, buf, ctx.before, ctx.mono, ctx.clock, out, err)
        if stored.outcome != "stored":
            return _not_stored(ctx, stored)
        ctx.version, ctx.stored_mono, ctx.stored_wall = stored.version, stored.mono, stored.wall
        pairs = [(TAG_KEY, ctx.today)]
        if ctx.gen:
            pairs += [(TAG_EXPIRES, ctx.gen.expires_at), (TAG_GENERATIONS, format_ledger([*ctx.ledger, (ctx.gen.request_id, ctx.version)]))]
        ctx.tags_ok = _write_tags(ctx.cli, pairs, bool(ctx.gen), ctx.version, out, err)
        if not ctx.gen:
            _ledger_notice(ctx.doc, out)
        result = wait_for_route(ctx.cli, ctx.version, ctx.stored_mono, ctx.mono, ctx.sleep, ctx.held)
        confirmed = _route_report(result, ctx.version, out, err)
        final = EXIT_OK if confirmed else EXIT_UNCONFIRMED
        if ctx.gen:
            final = _finish_enterprise(ctx, confirmed, final)
        elif confirmed:
            when = (ctx.stored_wall + datetime.timedelta(seconds=ROUTE_WAIT_S)).astimezone(ctx.tz)
            out.write(f"next: delete the old key in the Tavily dashboard after {when.strftime('%H:%M %Z')} (https://app.tavily.com, API Keys), six minutes after "
                      "the store. The route reads the secret again once after Tavily refuses a key, which covers an instance that is late. The previous version "
                      "stays in the secret with the label AWSPREVIOUS until the next store.\n")
        if final == EXIT_OK and not ctx.tags_ok:
            final = EXIT_AWS
        return final
    finally:
        wipe(buf)


def _not_stored(ctx, stored: Stored) -> int:
    """The key was not stored for certain, or it is not known. A key made by this run is deactivated again only in the first case."""
    out, err, gen = ctx.out, ctx.err, ctx.gen
    if stored.outcome == "not_stored":
        if gen:
            _cleanup_generation(ctx.http, ctx.admin, gen.request_id, out, err)
        return stored.code
    err.write("cannot tell whether the key was stored: the list of secret versions could not be read, or it shows more than one new version. ")
    if gen:
        err.write(f"The key made by this run (request {gen.request_id}, expires {gen.expires_at}) is NOT deactivated, because it may be the current key. "
                  f"Check with `python3 scripts/research-key-put.py --stage direct --check`; if the secret does not hold it, {_by_hand(gen.request_id)}. "
                  "It also expires by itself at its ttl.\n")
    else:
        err.write("Run `python3 scripts/research-key-put.py --stage direct --check` before trying again.\n")
    return stored.code


def _interrupted(ctx) -> int:
    """Ctrl-C after the prompt: what was done, and that nothing was undone."""
    out, err = ctx.out, ctx.err
    lines = ["interrupted: nothing was undone."]
    if ctx.gen and ctx.version is None:
        lines.append(f"The key made by this run (request {ctx.gen.request_id}, expires {ctx.gen.expires_at}) is NOT deactivated and may not be stored: check with "
                     f"`python3 scripts/research-key-put.py --stage direct --check`; if the secret does not hold it, {_by_hand(ctx.gen.request_id)}.")
    if ctx.version:
        lines.append(f"The new key IS stored (version {ctx.version}). Do not delete the old key yet; the research route may not hold the new key.")
        if ctx.gen:
            old = [rid for rid, _ in ctx.ledger if rid != ctx.gen.request_id]
            lines.append("Previous generations are NOT deactivated" + (": " + ", ".join(old) if old else "") + ". The ledger in the secret's tags lists them; "
                         "`--check` names them.")
    err.write(" ".join(lines) + "\n")
    return EXIT_INTERRUPTED


def _finish_enterprise(ctx, confirmed: bool, final: int) -> int:
    """After the store and the wait: deactivate the previous generations the ledger lists, only when the route holds the new key, the hold has passed and the tags
    were written; each one only if its version is not the current one. The ledger is rewritten to the generations that are still active."""
    out, err, gen, args = ctx.out, ctx.err, ctx.gen, ctx.args
    out.write(f"ttl: this key expires at {gen.expires_at}; rotate before then. Choose --ttl-hours longer than the time between your rotations plus the time "
              "you need to notice a missed one: an unrotated key then expires by itself, and a TTL shorter than that leaves research with no key for the gap.\n")
    if args.ttl_hours < 24 * DEFAULT_MAX_AGE_DAYS:
        out.write(f"WARNING: --ttl-hours {args.ttl_hours} is {args.ttl_hours} hours, shorter than {DEFAULT_MAX_AGE_DAYS} days, the age at which the stage "
                  "alarms about a stored key: this key expires before that alarm could fire, so a missed rotation leaves research with no key\n")
    out.write("note: run one operator at a time; no lock is built, and two rotations at once can overwrite each other's ledger\n")
    old = [(rid, vid) for rid, vid in ctx.ledger if rid != gen.request_id]
    if not old:
        out.write("previous generation: no previous generation is recorded in the secret's tags; if an older key exists (made in the Tavily dashboard), "
                  "delete it there\n")
        return final
    names = ", ".join(rid for rid, _ in old)
    if not ctx.tags_ok:
        err.write(f"previous generation {names} is NOT deactivated: the tags could not be written, so the ledger cannot say which generations are active. "
                  "They expire at their own ttl; the new generation is " + gen.request_id + "\n")
        return final
    if not confirmed:
        err.write(f"previous generation {names} is NOT deactivated: the route has not confirmed the new key. It expires by itself at its own ttl; "
                  f"the ledger in the tag {TAG_GENERATIONS} keeps it\n")
        return final
    held_for = ctx.mono() - ctx.stored_mono
    while held_for < DEACTIVATE_AFTER_S:
        wait = DEACTIVATE_AFTER_S - held_for
        out.write(f"previous generation: waiting {int(wait)} seconds, the longest an instance may still hold the old key, before it is deactivated\n")
        ctx.sleep(wait)
        held_for = ctx.mono() - ctx.stored_mono
    versions = _list_versions(ctx.cli)
    if versions is None:
        err.write(f"cannot check which version of the secret is current, so previous generation {names} is NOT deactivated; the ledger keeps it\n")
        return EXIT_NOT_DEACTIVATED
    current = next((v for v, labels in versions.items() if "AWSCURRENT" in labels), None)
    failed: List[Tuple[str, str]] = []
    for rid, vid in old:
        if vid == current:
            failed.append((rid, vid))
            err.write(f"previous generation {rid} holds the current key (version {vid}): it is NOT deactivated\n")
            continue
        why = _deactivate(ctx.http, ctx.admin, rid)
        if why is None:
            out.write(f"previous generation: request {rid} deactivated\n")
        else:
            failed.append((rid, vid))
            err.write(f"FAIL deactivate-keys: the previous generation (request {rid}) is NOT deactivated ({why}); the new key stays. {_by_hand(rid)}, "
                      "or let it expire at its own ttl\n")
    written = format_ledger([*ctx.ledger, (gen.request_id, ctx.version)])
    wanted = format_ledger([*failed, (gen.request_id, ctx.version)])
    code = EXIT_NOT_DEACTIVATED if failed else final
    if wanted != written and not _write_tags(ctx.cli, [(TAG_GENERATIONS, wanted)], True, ctx.version, out, err):
        err.write(f"the ledger in the tag {TAG_GENERATIONS} may list generations that are already deactivated\n")
        code = code if failed else EXIT_AWS
    return code


def main(argv: Optional[Sequence[str]] = None, *, env: Optional[Mapping[str, str]] = None,
         is_tty: Optional[Callable[[], bool]] = None, prompt: Optional[Callable[[str], str]] = None,
         stdout: Optional[TextIO] = None, stderr: Optional[TextIO] = None,
         now: Optional[Callable[[], datetime.datetime]] = None, http: Optional[HttpFn] = None,
         sleep: Optional[Callable[[float], None]] = None, mono: Optional[Callable[[], float]] = None, tz: Optional[datetime.tzinfo] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.clear and (args.check or args.max_age_days is not None):
        parser.error("--clear cannot be used with --check or --max-age-days")
    if args.max_age_days is not None and not args.check:
        parser.error("--max-age-days belongs to --check")
    if args.rotate and (args.rotate_enterprise or args.clear or args.check or args.max_age_days is not None):
        parser.error("--rotate cannot be used with --rotate-enterprise, --clear, --check or --max-age-days")
    if args.rotate_enterprise and (args.clear or args.check or args.max_age_days is not None):
        parser.error("--rotate-enterprise cannot be used with --clear, --check or --max-age-days")
    if not args.rotate_enterprise and any(v is not None for v in (args.ttl_hours, args.key_limit, args.key_type, args.key_name)):
        parser.error("--ttl-hours, --key-limit, --key-type and --key-name belong to --rotate-enterprise")
    if args.rotate_enterprise and (args.ttl_hours is None or args.key_limit is None):
        parser.error("--rotate-enterprise needs --ttl-hours and --key-limit")
    env = dict(os.environ) if env is None else dict(env)
    out, err = stdout or sys.stdout, stderr or sys.stderr
    is_tty = is_tty or sys.stdin.isatty
    ask = prompt or getpass.getpass
    clock = now or (lambda: datetime.datetime.now(datetime.timezone.utc))
    pause = sleep or time.sleep
    tick = mono or time.monotonic
    local = tz or datetime.datetime.now().astimezone().tzinfo or datetime.timezone.utc

    aws_target = _aws_target()
    try:
        target = aws_target.select(env)
    except aws_target.TargetError as exc:
        err.write(f"aws_target: {exc}\n")
        return EXIT_REFUSED
    aws = find_aws(env)
    if aws is None:
        want = env.get("HAMH_AWS_BIN") or "aws on PATH"
        err.write(f"the AWS CLI was not found ({want}): install it, or set HAMH_AWS_BIN to its path\n")
        return EXIT_PREREQ
    cli = Cli(aws, target.profile, target.region, env)

    if args.check:
        return _check(cli, out, err, clock(), args.max_age_days or DEFAULT_MAX_AGE_DAYS)

    if args.clear:
        return _clear(cli, target, clock, out, err)

    if args.rotate or args.rotate_enterprise:
        return _rotate(args, cli, target, clock, tick, local, pause, ask, is_tty, http or default_http, out, err)

    if not is_tty():
        err.write("refused: run this in a terminal; the key is typed at a hidden prompt and is never read from a pipe or a file\n")
        return EXIT_REFUSED
    refused = _history_refusal(cli, target, err)
    if refused:
        return refused
    rc, pre, se = cli.describe()
    if rc != 0:
        if _not_found(se):
            err.write(f"{DEPLOY_HINT}\n")
            return EXIT_PREREQ
        err.write(f"FAIL describe-secret: {se.strip()[:MAX_ERROR_CHARS]}\n")
        return EXIT_AWS

    out.write(f"Paste the Tavily API key at the prompt (typing is hidden). It is stored in secret {RESEARCH_STORE_NAME} "
              f"(account {target.account}, {target.region}, profile {target.profile}).\n")
    try:
        first = ask("Tavily API key: ")
        bad = problem_with(first)
        if bad:
            err.write(f"refused: {bad}\n")
            return EXIT_REFUSED
        second = ask("Tavily API key again: ")
    except (EOFError, KeyboardInterrupt):
        err.write("cancelled: nothing was stored\n")
        return EXIT_REFUSED
    if first != second:
        err.write("refused: the two entries differ; nothing was stored\n")
        return EXIT_REFUSED
    key = first
    buf = bytearray(key.encode("ascii"))
    del first, second
    try:
        rc, so, se = cli.run(["secretsmanager", "put-secret-value", "--secret-id", RESEARCH_STORE_NAME, "--secret-string", "file:///dev/stdin"],
                             stdin=bytes(buf), key=key)
        if rc != 0:
            if _not_found(se):
                err.write(f"{DEPLOY_HINT}\n")
                return EXIT_PREREQ
            err.write(f"FAIL put-secret-value: {redact(se.strip(), key)[:MAX_ERROR_CHARS]}\n")
            return EXIT_AWS
        try:
            doc = json.loads(so)
            arn, version = str(doc["ARN"]), str(doc["VersionId"])
        except (ValueError, KeyError, TypeError):
            err.write("FAIL put-secret-value: the CLI answered without an ARN and a version id; "
                      "check the secret with --check before trying again\n")
            return EXIT_AWS
        out.write(f"secret: {arn}\nversion: {version}\nstored: {len(key)} characters\n")
        date = clock().astimezone(datetime.timezone.utc).date().isoformat()
        rc, _so, se = cli.run(["secretsmanager", "tag-resource", "--secret-id", RESEARCH_STORE_NAME, "--tags", f"Key={TAG_KEY},Value={date}"],
                             key=key)
        if rc != 0:
            err.write(f"FAIL tag-resource: {redact(se.strip(), key)[:MAX_ERROR_CHARS]}\n"
                      f"the key IS stored (version {version}); only the tag {TAG_KEY} is missing\n")
            return EXIT_AWS
        out.write(f"tagged: {TAG_KEY}={date}\n")
        _ledger_notice(_json_doc(pre), out)
        out.write("next: nothing to restart; the research route reads the secret again within 5 minutes. "
                                    "To probe a key before it is stored and wait for the route, use --rotate.\n")
        return EXIT_OK
    finally:
        wipe(buf)
        key = ""


if __name__ == "__main__":
    sys.exit(main())
