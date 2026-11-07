#!/usr/bin/env python3
# COMPAT-CAH: this file reads the previous spellings (CAH_*) of its variables for the window of the previous names.
"""The AWS account, Region and profile every script, probe, CDK synth and Rust build targets
: one entry of `accounts.json` at the repository root.

  aws_target.py shell          HAMH_ACCOUNT=, HAMH_REGION=, HAMH_PROFILE= and AWS_PROFILE= lines (bash eval), then the previous spellings of the first three
  aws_target.py get <field>    one field: account, region, profile, role, invocationLogging,
                               operator (`role/<name>` or `user/<name>`, empty when the entry names none),
                               owner (the value of the `owner` tag of the entry's stage; see below),
                               repoNames (the repository-name rule of the entry's stage, comma-joined; see below),
                               azs.<stage>, azIds.<stage> (lists comma-joined)
  aws_target.py digest         `committed`, or the SHA-256 of the account file HAMH_ACCOUNTS_FILE names (the deploy record's
                               deploymentConfigDigest)

Selection: HAMH_AWS_PROFILE names the entry by its profile; unset or empty selects the one entry
whose role is `active`. AWS_PROFILE never selects: scripts set it from the selection. An unknown
profile, a malformed file or a bad field is a refusal: `aws_target: <reason>` on stderr, exit 2,
nothing on stdout.

Previous names: every HAMH_X variable this file reads is read first under that name and second under its
previous spelling (the CAH_ prefix). The previous name prints one notice on stderr per name and process (stdout of `shell` is eval'd by the caller, so a notice never goes
there) unless HAMH_NO_DEPRECATION_NOTICES=1. A variable that is set and empty counts as unset. HAMH_AWS_PROFILE, HAMH_ACCOUNTS_FILE and HAMH_OWNER choose an account, the
account file or the owner: both spellings set with different values is a refusal (exit 2), never a fall-through to the active entry. For another variable the new name wins.

The file (schemaVersion 1): `selector` "HAMH_AWS_PROFILE" (a file that was generated before the rename names its previous spelling, and still loads) and `accounts`, each entry with the four
criteria fields `account` (12 digits), `region`, `profile`, `role` (`active` exactly once, else
`history` or `test`), plus `azs` and `azIds` per stage (names in the entry's Region, the same count as IDs; both name the
stage `direct`, which is the stage the product deploys, and may name `live` and `drill` as well), `invocationLogging` (`enable-and-restore`: deploy.sh records the prior state,
enables it only when unset and teardown.sh restores it; `read-only`: read and recorded, never
written).
The optional `operator` names the one identity that runs setup and later operates the stage: `{"kind": "role", "name": "<role name>"}` (the name as it
appears in the assumed-role ARN of the caller; an IAM Identity Center role is AWSReservedSSO_<permission set>_<16 hex>; no path) or `{"kind": "user", "name": "<user name>"}`.
A name has 1 to 64 letters, digits or + = . @ _ - (no path, no wildcard, no comma), and the object has the keys kind and name only. An entry without it keeps the IAM role
`Admin` the stage trusted before the field existed. The stage trusts exactly that identity wherever `Admin` was written (infra/lib/operator.ts holds the same rules in
TypeScript, and infra/test/fixtures/operator-identity.json is their one table); the deploy engine's caller check (scripts/lib/deploy.py) accepts that identity, and the
setup runner of the account because it deploys with its own role. `get operator` prints `role/<name>` or `user/<name>`, or nothing.
The Rust build (crates/harness-protocol/build.rs) reads the entry's other fields and does not read this one: no Rust code needs the identity at build time, and the worker
image key stays what it was.
The optional `owner` gives the value of the `owner` tag that every taggable resource of the entry's stage carries (and that the repository gateway puts on the CodeCommit
repositories it creates): 1 to 64 letters, digits or + = . @ _ - . Without it the value is the name of the entry's `operator`, and without that `hamh-user`. The environment variable
`HAMH_OWNER`, when it is set and not empty, wins over all three for the run.
`get owner` prints the value. `owner_is_named` is true when the run names the owner itself (a nonempty `HAMH_OWNER`
or an entry `owner`): a deploy of a stage that exists keeps the owner the stage has unless the run names one (scripts/lib/deploy.py `settle_owner`).
The optional `repoNames` is the stage's rule for the CodeCommit repository names a run may open a pull request in: a list of 1 to 8 entries, each an exact repository name (1 to 100
letters, digits, `.`, `_` or `-`), such a prefix of up to 99 characters followed by one `*`, or `*` alone. Without it the rule is the default `hamh-e2e-*`. The environment variable
`HAMH_REPO_NAMES` (comma separated, spaces around an entry are dropped), when it is set and not empty, wins over the entry for the run (`repo_names_of`; infra/lib/repo-names.ts holds the same
rules in TypeScript, and infra/test/fixtures/repo-names.json is their one table). `get repoNames` prints the list comma-joined, the form the CDK app hands the control-api and delivery functions
as `HAMH_REPO_NAMES`. `repo_name_allowed` is the matcher. The Rust build (crates/harness-protocol/build.rs) does not read this field, so the worker image key stays what it was.
A history entry keeps the values its records were written with; verify rows accept any entry's
account and Region on a record. A `test` entry is an extra account for deploys and live tests:
it is chosen only by HAMH_AWS_PROFILE, never by default, and never counts as the active entry.

The account-file override: HAMH_ACCOUNTS_FILE, an absolute path, names the file to read in place of the committed
accounts.json. `setup.sh` generates such a file for the account it deploys into, so the committed file is never edited. Unset or empty, the
committed file is read and every answer is what it was before the override existed. A relative path is a refusal (a build script runs in
another working directory than a wrapper, so it would name another file), and so are a missing file and a directory: a set variable never
falls back to the committed file. The file rules above apply to the override unchanged. Every consumer reads the variable by the same rule:
infra/lib/config.ts, crates/harness-protocol/build.rs (src/accounts.rs), scripts/node.sh (which mounts the file into the CDK container),
scripts/codebuild-run.sh (which carries it into the child builds) and scripts/lib/deploy.py (which records `config_digest`).
Python 3.8+, standard library only.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Mapping, Optional

ROOT = Path(__file__).resolve().parents[2]
SELECTOR = "HAMH_AWS_PROFILE"
SELECTOR_WORD_PREVIOUS = "CAH_AWS_PROFILE"  # COMPAT-CAH: the selector word of an account file generated before the rename; such a file still loads
FILE_HOOK = "HAMH_ACCOUNTS_FILE"  # the account-file override: an absolute path read in place of the committed accounts.json
ROLES = ("active", "history", "test")
STAGES = ("live", "drill", "direct")  # the stage names a file may use
DEPLOYED_STAGE = "direct"  # the one stage every file names: the stage the product deploys
LOGGING_MODES = ("enable-and-restore", "read-only")
CORE = ("account", "region", "profile", "role")
OPERATOR_KINDS = ("role", "user")
OPERATOR_NAME = re.compile(r"[A-Za-z0-9+=.@_-]{1,64}")
# The value of the `owner` tag of a stage whose entry names neither an `owner` nor an `operator`.
DEFAULT_OWNER = "hamh-user"
OWNER_NAME = re.compile(r"[A-Za-z0-9+=.@_-]{1,64}")
OWNER_ENV = "HAMH_OWNER"  # sets the owner of one run ahead of the account file
# The repository-name rule of a stage: the entries of `repoNames`, else this default, which is the rule the product had before it was a setting.
DEFAULT_REPO_NAMES = ("hamh-e2e-*",)
REPO_NAMES_ENV = "HAMH_REPO_NAMES"  # sets the rule of one run ahead of the account file
REPO_NAMES_MAX = 8
REPO_NAME_ENTRY = re.compile(r"(?:[A-Za-z0-9._-]{1,100}|[A-Za-z0-9._-]{1,99}\*|\*)")
# The role of the account-local runner that setup.sh installs (infra/setup-runner.yaml, RoleName): it deploys with its own role.
SETUP_RUNNER_ROLE = "hamh-setup-runner-role"

# ---- the previous spellings of the variables (COMPAT-CAH) --------------------------------------------------------------------------------------------------------
NOTICES_OFF = "HAMH_NO_DEPRECATION_NOTICES"  # 1 silences every notice about a previous name
SELECTING = (SELECTOR, FILE_HOOK, OWNER_ENV)  # the variables that choose an account, the account file or the owner: two different values are an error
_PREVIOUS_PREFIX = "CAH_"  # COMPAT-CAH: the prefix of the previous spellings
_NOTICED: set = set()  # the previous names already noticed in this process


class TargetError(Exception):
    """A malformed accounts file or an unknown selection."""


def previous_name(name: str) -> str:
    """The previous spelling of a HAMH_ variable name (CAH_ and the same suffix)."""
    return _PREVIOUS_PREFIX + name[len("HAMH_"):]


def env_get(name: str, env: Optional[Mapping[str, str]] = None) -> str:
    """The value of the variable `name` (a HAMH_ name) in `env` (the process environment when None): the new spelling when it is set and not empty, else the previous
    spelling, else "". The previous name prints one notice on stderr per name and process (never on stdout) unless HAMH_NO_DEPRECATION_NOTICES=1. For the variables in
    SELECTING both spellings set with different values is a TargetError; for any other variable the new name wins and the notice says so."""
    env = os.environ if env is None else env
    old_name = previous_name(name)
    new, old = env.get(name) or "", env.get(old_name) or ""
    if not old:
        return new
    if new and new != old and name in SELECTING:
        raise TargetError(f"{name} and {old_name} are both set with different values. A variable that chooses the account, the account file or the owner is never "
                          "ignored and never replaced by a default. Unset one of them. Nothing was changed.")
    if old_name not in _NOTICED:
        _NOTICED.add(old_name)
        if (env.get(NOTICES_OFF) or "").lower() not in ("1", "true", "yes"):
            takes = f" {name} takes precedence when both are set." if new else ""
            print(f"aws_target: {old_name} is the previous name of {name}; both work in this release.{takes} Set {name} instead. "
                  f"Set {NOTICES_OFF}=1 to silence this notice.", file=sys.stderr)
    return new or old


@dataclass(frozen=True)
class Operator:
    """The identity an entry's `operator` names: an IAM role by its name in the assumed-role ARN, or an IAM user by its name."""
    kind: str
    name: str

    @property
    def label(self) -> str:
        return f"{self.kind}/{self.name}"


@dataclass(frozen=True)
class Target:
    account: str
    region: str
    profile: str
    role: str
    azs: Dict[str, List[str]]
    az_ids: Dict[str, List[str]]
    invocation_logging: str
    operator: Optional[Operator] = None
    owner: Optional[str] = None
    repo_names: Optional[List[str]] = None

    def core(self) -> dict:
        """The four fields criteria `accounts` carries."""
        return {"account": self.account, "region": self.region, "profile": self.profile, "role": self.role}

    def pinned_regions(self, inference: Optional[List[str]] = None) -> List[str]:
        return pinned_regions(self.region, inference or list(INFERENCE_REGIONS))


# Bedrock inference Regions, in criteria order.
INFERENCE_REGIONS = ("us-west-2", "us-east-1", "us-east-2")


def pinned_regions(home: str, inference: List[str]) -> List[str]:
    """The pinned Regions in preference order: the home Region first, then the other inference
    Regions in criteria order (S6 gives a model the first pinned Region that lists it)."""
    if home not in inference:
        raise TargetError(f"home Region {home} is not an inference Region ({', '.join(inference)})")
    return [home, *[r for r in inference if r != home]]


def fold_previous(env: Mapping[str, str]) -> Dict[str, str]:
    """A copy of `env` in which each selector (SELECTING) is set under its new name only, with the value env_get finds, and absent when there is none (an empty value is
    dropped). For a caller that hands the environment to child processes and must not give them two spellings of one selector. Two different values are a TargetError."""
    out = dict(env)
    for name in SELECTING:
        value = env_get(name, env)
        out.pop(previous_name(name), None)
        out.pop(name, None)
        if value:
            out[name] = value
    return out


def with_selector(env: Mapping[str, str], name: str, value: str) -> Dict[str, str]:
    """A copy of `env` with the selector `name` (a HAMH_ name) set to `value` and its previous spelling dropped, for a caller that chooses the selector itself for a child."""
    out = dict(env)
    out.pop(previous_name(name), None)
    out[name] = value
    return out


def accounts_path(env: Optional[Mapping[str, str]] = None) -> Path:
    """The file to read: HAMH_ACCOUNTS_FILE when it is set and not empty (an absolute path, else a refusal), else the committed accounts.json."""
    value = env_get(FILE_HOOK, env)
    if not value:
        return ROOT / "accounts.json"
    if not os.path.isabs(value):
        raise TargetError(f"{FILE_HOOK} must be an absolute path (got {value!r})")
    return Path(value)


def config_digest(env: Optional[Mapping[str, str]] = None) -> str:
    """`committed` when the committed file is in use, else the SHA-256 of the bytes of the file HAMH_ACCOUNTS_FILE names."""
    if not env_get(FILE_HOOK, env):
        return "committed"
    p = accounts_path(env)
    try:
        return hashlib.sha256(p.read_bytes()).hexdigest()
    except OSError as exc:
        raise TargetError(f"{p}: {exc}") from None


def parse_operator(raw: object, where: str) -> Operator:
    """The operator an entry names, or a TargetError that starts with `where`. infra/lib/operator.ts `parseOperator` applies the same rules to the same words."""
    if not isinstance(raw, dict):
        raise TargetError(f'{where}: operator must be an object with kind (role or user) and name (an IAM name), like {{"kind": "role", "name": "Admin"}}')
    kind, name = raw.get("kind"), raw.get("name")
    if kind not in OPERATOR_KINDS:
        raise TargetError(f"{where}: operator.kind must be role or user")
    if not isinstance(name, str) or not OPERATOR_NAME.fullmatch(name):
        raise TargetError(f"{where}: operator.name must be 1 to 64 letters, digits or + = . @ _ - (a name, not a path, a pattern or an ARN)")
    if len(raw) != 2:
        raise TargetError(f"{where}: operator has the keys kind and name only")
    return Operator(kind=kind, name=name)


def parse_owner(raw: object, where: str) -> str:
    """The owner an entry names, or a TargetError that starts with `where`. infra/lib/owner.ts `parseOwner` applies the same rule to the same words."""
    if not isinstance(raw, str) or not OWNER_NAME.fullmatch(raw):
        raise TargetError(f"{where}: owner must be 1 to 64 letters, digits or + = . @ _ - (a tag value: no path, no pattern, no ARN)")
    return raw


def parse_repo_names(raw: object, where: str) -> List[str]:
    """The rule an entry (or `HAMH_REPO_NAMES`) names, or a TargetError that starts with `where`. infra/lib/repo-names.ts `parseRepoNames` applies the same rules to the same words."""
    if not isinstance(raw, list) or not 1 <= len(raw) <= REPO_NAMES_MAX:
        raise TargetError(f'{where}: repoNames must be a list of 1 to 8 repository names, like ["my-app", "team-*"]')
    for e in raw:
        if not isinstance(e, str) or not REPO_NAME_ENTRY.fullmatch(e):
            raise TargetError(f"{where}: repoNames entry {json.dumps(e, ensure_ascii=False)} must be a repository name (1 to 100 letters, digits, '.', '_' or '-'), "
                              "such a prefix followed by '*', or '*'")
    for i, e in enumerate(raw):
        if e in raw[:i]:
            raise TargetError(f"{where}: repoNames lists {json.dumps(e, ensure_ascii=False)} twice")
    return list(raw)


def repo_names_of(t: Target, env: Optional[Mapping[str, str]] = None) -> List[str]:
    """The repository-name rule of the stage of this entry: `HAMH_REPO_NAMES` of `env` (the process environment when None; comma separated, spaces around an entry dropped) when it is
    set and not empty, else the entry's own `repoNames`, else the default `hamh-e2e-*`. A `HAMH_REPO_NAMES` that is not a rule is a TargetError."""
    names = env_get(REPO_NAMES_ENV, env)
    if names:
        return parse_repo_names([e.strip() for e in names.split(",")], REPO_NAMES_ENV)
    return list(t.repo_names) if t.repo_names is not None else list(DEFAULT_REPO_NAMES)


def repo_name_allowed(rule: List[str], name: str) -> bool:
    """Whether the rule covers a repository name: an exact entry equals it, a prefix entry (`team-*`) starts it, `*` covers every name. Names are case sensitive."""
    return any(name.startswith(e[:-1]) if e.endswith("*") else name == e for e in rule)


def owner_of(t: Target, env: Optional[Mapping[str, str]] = None) -> str:
    """The value of the `owner` tag of the stage of this entry: `HAMH_OWNER` of `env` (the process environment when None) when it is set and not empty, else its own
    `owner`, else the name of its `operator`, else `hamh-user`. A `HAMH_OWNER` that is not a tag value is a TargetError."""
    owner = env_get(OWNER_ENV, env)
    if owner:
        return parse_owner(owner, OWNER_ENV)
    return t.owner if t.owner is not None else (t.operator.name if t.operator else DEFAULT_OWNER)


def owner_is_named(t: Target, env: Optional[Mapping[str, str]] = None) -> bool:
    """Whether the run names the owner itself: a nonempty `HAMH_OWNER` or an `owner` in the entry. Without either, `owner_of` derives the value from the operator
    (or gives `hamh-user`), and a deploy of a stage that exists keeps the owner the stage has (scripts/lib/deploy.py `settle_owner`)."""
    return bool(env_get(OWNER_ENV, env)) or t.owner is not None


def operator_may_deploy(account: str, op: Operator, caller_arn: str) -> bool:
    """Whether the caller ARN of `sts get-caller-identity` may deploy the stage of an entry that names `op`: that identity, or the setup runner of the account.
    The root user, a federated user, another role, another user and another account may not. By identity: no wildcard on either side (infra/lib/operator.ts
    `operatorMayDeploy` is the same rule)."""
    sts = f"arn:aws:sts::{account}:assumed-role/"
    iam = f"arn:aws:iam::{account}:user/"
    if caller_arn.startswith(sts):
        parts = caller_arn[len(sts):].split("/")
        if len(parts) != 2 or not parts[0] or not parts[1]:
            return False
        return parts[0] == SETUP_RUNNER_ROLE or (op.kind == "role" and parts[0] == op.name)
    if caller_arn.startswith(iam):
        parts = caller_arn[len(iam):].split("/")
        return op.kind == "user" and all(parts) and parts[-1] == op.name
    return False


def _stage_lists(e: dict, key: str, where: str) -> Dict[str, List[str]]:
    v = e.get(key)
    if not isinstance(v, dict) or DEPLOYED_STAGE not in v or any(s not in STAGES for s in v):
        raise TargetError(f"{where}: {key} must name the stage {DEPLOYED_STAGE}, and may name {' and '.join(s for s in STAGES if s != DEPLOYED_STAGE)}")
    for s in v:
        if not isinstance(v[s], list) or not all(isinstance(x, str) and x for x in v[s]):
            raise TargetError(f"{where}: {key}.{s} must be a list of names")
    return {s: list(v[s]) for s in STAGES if s in v}


def _entry(e: dict, i: int) -> Target:
    where = f"accounts[{i}]"
    if not isinstance(e, dict):
        raise TargetError(f"{where} is not an object")
    if not re.fullmatch(r"[0-9]{12}", str(e.get("account", ""))):
        raise TargetError(f"{where}: account must be 12 digits")
    if not re.fullmatch(r"[a-z]{2}(-[a-z]+)+-[0-9]", str(e.get("region", ""))):
        raise TargetError(f"{where}: region {e.get('region')!r} is not a Region name")
    if not isinstance(e.get("profile"), str) or not re.fullmatch(r"[A-Za-z0-9._-]+", e["profile"]):
        raise TargetError(f"{where}: profile must be a profile name")
    if e.get("role") not in ROLES:
        raise TargetError(f"{where}: role must be one of {', '.join(ROLES)}")
    if e.get("invocationLogging") not in LOGGING_MODES:
        raise TargetError(f"{where}: invocationLogging must be one of {', '.join(LOGGING_MODES)}")
    azs, ids = _stage_lists(e, "azs", where), _stage_lists(e, "azIds", where)
    if set(azs) != set(ids):
        raise TargetError(f"{where}: azs and azIds must name the same stages")
    for s in azs:
        if not azs[s] or len(azs[s]) != len(ids[s]):
            raise TargetError(f"{where}: azs.{s} and azIds.{s} must pair up one to one")
        for az in azs[s]:
            if not (az.startswith(e["region"]) and re.fullmatch(r"[a-z]", az[len(e["region"]):])):
                raise TargetError(f"{where}: {az} is not in {e['region']}")
    operator = parse_operator(e["operator"], where) if "operator" in e else None
    owner = parse_owner(e["owner"], where) if "owner" in e else None
    repo_names = parse_repo_names(e["repoNames"], where) if "repoNames" in e else None
    return Target(account=e["account"], region=e["region"], profile=e["profile"], role=e["role"], azs=azs,
                  az_ids=ids, invocation_logging=e["invocationLogging"], operator=operator, owner=owner, repo_names=repo_names)


def load(path: Optional[Path] = None) -> List[Target]:
    """Every entry of the accounts file, in file order, after the file rules."""
    p = Path(path) if path else accounts_path()
    try:
        doc = json.loads(p.read_text())
    except (OSError, ValueError) as exc:
        raise TargetError(f"{p}: {exc}") from None
    if not isinstance(doc, dict) or doc.get("schemaVersion") != 1 or doc.get("selector") not in (SELECTOR, SELECTOR_WORD_PREVIOUS):
        raise TargetError(f"{p}: expected schemaVersion 1 and selector {SELECTOR}")
    raw = doc.get("accounts")
    if not isinstance(raw, list) or not raw:
        raise TargetError(f"{p}: accounts must be a non-empty list")
    out = [_entry(e, i) for i, e in enumerate(raw)]
    seen = set()
    for t in out:
        if t.profile in seen:
            raise TargetError(f"{p}: profile {t.profile} twice")
        seen.add(t.profile)
    if len({(t.account, t.region) for t in out}) != len(out):
        raise TargetError(f"{p}: an account and Region pair appears twice")
    if [t.role for t in out].count("active") != 1:
        raise TargetError(f"{p}: exactly one active entry is required")
    return out


def accounts(env: Optional[Mapping[str, str]] = None) -> List[Target]:
    return load(accounts_path(env))


def select(env: Optional[Mapping[str, str]] = None) -> Target:
    """The entry HAMH_AWS_PROFILE names, else the active entry."""
    entries = load(accounts_path(env))
    want = env_get(SELECTOR, env)
    if not want:
        return next(t for t in entries if t.role == "active")
    for t in entries:
        if t.profile == want:
            return t
    raise TargetError(f"{SELECTOR}={want} names no entry of {accounts_path(env)} "
                      f"(profiles: {', '.join(t.profile for t in entries)})")


def module_target() -> Target:
    """select() for a script library's module-level ACCOUNT, REGION and PROFILE: a refusal prints
    `aws_target: <reason>` and exits 2, as a wrapper sourcing lib/aws-target.sh would."""
    try:
        return select()
    except TargetError as exc:
        print(f"aws_target: {exc}", file=sys.stderr)
        raise SystemExit(2) from None


def is_known(account: str, region: str, env: Optional[Mapping[str, str]] = None) -> bool:
    """True when (account, Region) is one entry of the accounts file (a record's header check)."""
    return any(t.account == account and t.region == region for t in accounts(env))


def field(t: Target, name: str) -> str:
    simple = {"account": t.account, "region": t.region, "profile": t.profile, "role": t.role,
              "invocationLogging": t.invocation_logging}
    if name in simple:
        return simple[name]
    if name == "operator":
        return t.operator.label if t.operator else ""
    if name == "owner":
        return owner_of(t)
    if name == "repoNames":
        return ",".join(repo_names_of(t))
    kind, _, stage = name.partition(".")
    if kind in ("azs", "azIds") and stage in STAGES:
        names = (t.azs if kind == "azs" else t.az_ids).get(stage)
        if names is None:
            raise TargetError(f"{name}: the entry names no {stage} stage")
        return ",".join(names)
    raise TargetError(f"unknown field {name}")


def main(argv: List[str]) -> int:
    try:
        if argv == ["shell"]:
            t = select()
            sys.stdout.write(f"HAMH_ACCOUNT={t.account}\nHAMH_REGION={t.region}\nHAMH_PROFILE={t.profile}\n"
                             f"AWS_PROFILE={t.profile}\n"
                             f"CAH_ACCOUNT={t.account}\nCAH_REGION={t.region}\nCAH_PROFILE={t.profile}\n")  # COMPAT-CAH: the previous spellings, for the window
            return 0
        if len(argv) == 2 and argv[0] == "get":
            value = field(select(), argv[1])
            print(value)
            return 0
        if argv == ["digest"]:
            print(config_digest())
            return 0
        raise TargetError("usage: aws_target.py shell | get <field> | digest")
    except TargetError as exc:
        print(f"aws_target: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
