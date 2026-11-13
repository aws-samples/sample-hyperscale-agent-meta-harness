"""Accounts files for tests: the shared fixture `fixtures/accounts-three.json` and a writer for variants, with placeholder accounts and profiles.

The committed `accounts.json` is the maintainers' own file (or, in a published tree, a neutral one-entry file), so a test that needs a particular layout builds it here
and points `HAMH_ACCOUNTS_FILE` at it. The three entries have the roles the product knows: `history` (an earlier account whose records stay valid), `active` (the default)
and `test` (chosen only by name). The account ids are placeholders that are not the ids of any real account.

    from accounts_fixture import HISTORY, ACTIVE, TEST, write_accounts
    path = write_accounts(folder)                      # history, active, test: the bytes of fixtures/accounts-three.json
    env = {"HAMH_ACCOUNTS_FILE": str(path)}             # an absolute path, as the account-file override wants it
    profile = ACTIVE["profile"]                         # my-profile; HISTORY["profile"] is history-profile and TEST["profile"] is test-profile

`fixtures/accounts-three.json` is the same document as a file (a test that needs the file as it is reads it from there, or copies it with `write_accounts`);
`test_accounts_fixture.py` keeps the two equal. The values are placeholders: the account ids 111111111111, 222222222222 and 333333333333 and the profile names
history-profile, my-profile (the profile of the committed neutral accounts.json) and test-profile are no real account or profile.

Standard library only.
"""

from __future__ import annotations

import json
from pathlib import Path

FIXTURE = Path(__file__).resolve().parent / "fixtures" / "accounts-three.json"  # the shared file: history, active and test

HISTORY = {"account": "111111111111", "region": "us-west-2", "profile": "history-profile", "role": "history"}
ACTIVE = {"account": "222222222222", "region": "us-east-1", "profile": "my-profile", "role": "active"}
TEST = {"account": "333333333333", "region": "us-east-1", "profile": "test-profile", "role": "test"}

_AZS = {
    "us-west-2": ({"live": ["us-west-2a", "us-west-2b", "us-west-2c"], "drill": ["us-west-2a"], "direct": ["us-west-2a"]},
                  {"live": ["usw2-az1", "usw2-az2", "usw2-az3"], "drill": ["usw2-az1"], "direct": ["usw2-az1"]}),
    "us-east-1": ({"live": ["us-east-1b", "us-east-1c", "us-east-1d"], "drill": ["us-east-1b"], "direct": ["us-east-1b"]},
                  {"live": ["use1-az1", "use1-az2", "use1-az4"], "drill": ["use1-az1"], "direct": ["use1-az1"]}),
}
# the test account maps its zone letters to the AgentCore zone ids differently from the active one (zone a is use1-az1 there, zone b here)
_TEST_AZS = ({"live": ["us-east-1a", "us-east-1b", "us-east-1c"], "drill": ["us-east-1a"], "direct": ["us-east-1a"]},
             {"live": ["use1-az1", "use1-az2", "use1-az4"], "drill": ["use1-az1"], "direct": ["use1-az1"]})
_LOGGING = {"history": "read-only", "active": "enable-and-restore", "test": "read-only"}


def entry(core: dict, **over) -> dict:
    """A whole entry for one of HISTORY, ACTIVE and TEST (or a dict with the same four keys): the zones of its Region and the logging mode of its role."""
    azs, ids = _TEST_AZS if core["role"] == "test" and core["region"] == "us-east-1" else _AZS[core["region"]]
    e = {**core, "azs": {k: list(v) for k, v in azs.items()}, "azIds": {k: list(v) for k, v in ids.items()}, "invocationLogging": _LOGGING[core["role"]]}
    e.update(over)
    return e


def document(entries=None, selector: str = "HAMH_AWS_PROFILE") -> dict:
    """The accounts document: schemaVersion 1, the selector word and the entries (history, active, test by default)."""
    return {"schemaVersion": 1, "selector": selector, "accounts": [entry(c) if "azs" not in c else c for c in (entries or (HISTORY, ACTIVE, TEST))]}


def write_accounts(folder, name: str = "accounts.json", entries=None, selector: str = "HAMH_AWS_PROFILE") -> Path:
    """Write the document to folder/name and return the absolute path (what HAMH_ACCOUNTS_FILE wants)."""
    path = Path(folder).resolve() / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document(entries, selector), indent=2) + "\n", encoding="utf-8")
    return path


def accounts_three(folder, name: str = "accounts.json") -> Path:
    """Copy the shared file `fixtures/accounts-three.json` to folder/name and return the absolute path (what HAMH_ACCOUNTS_FILE wants)."""
    path = Path(folder).resolve() / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(FIXTURE.read_bytes())
    return path
