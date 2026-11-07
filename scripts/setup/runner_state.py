#!/usr/bin/env python3
"""runner_state.py: the JSON half of scripts/setup/runner-build.sh.

The runner script is bash and calls the AWS CLI; everything that builds, reads or checks JSON is here, so the script needs neither jq nor a shell
trick, and the files it writes to the setup bucket have one definition. Standard library only, Python 3.8 or later. Run it as `python3 -I -B`.

  json-get PATH                        read JSON on stdin and print the value at the dotted PATH (a list index is a number): a string as it is, anything else as
                                       JSON. Exit 1 when the path is not there.
  json-get-key KEY                     read a JSON object on stdin and print its member KEY, which may hold dots (a string as it is, anything else as JSON).
  list-keys [--suffix S]               read `aws s3api list-objects-v2` JSON on stdin and print the object keys, one per line, that end with S.
  zip-comment FILE                     print the comment of a zip archive (`git archive --format=zip` writes the commit id there).
  phase-line --build-id ID --stage S --commit C --digest D --name N --state ok|skipped|failed --seconds N [--detail T] [--from ID] [--extra K=V]...
                                       print one line of phases.jsonl: compact JSON with the schema, the build id, the stage, the source commit, the configuration
                                       digest, the phase, its state, its seconds, a detail of at most 500 printable characters on one line, the UTC time, and the extra
                                       members (a skipped phase names the build that did the work in `from`; the deploy names its `deploymentDigest`).
  completed --stage S --commit C --digest D FILE...
                                       read phases.jsonl files and print `<phase> TAB <build id> TAB <deployment digest> TAB <binary name>` for every phase that an earlier build
                                       finished with state ok for this stage, commit and configuration digest (the latest record of each phase). A failed or skipped
                                       record, another commit or another digest, and a line that is not JSON, count for nothing.
  client-check [--part client|admin] FILE
                                       check the client section a build-client.sh printed (platform to key, sha256, bytes, and the same three for the operator tool
                                       under admin) and print `<platform> TAB <key> TAB <bytes> TAB <sha256>` lines for the client, or for the operator tool with --part admin.
  stack-outputs --stack NAME           read `aws cloudformation describe-stacks` JSON on stdin and print {"<NAME>.<OutputKey>": value} as JSON.
  result --state S --build-id ID --stage S --commit C --digest D --region R --account A --endpoint E --deployment-digest X
         --phases FILE [--outputs FILE]... [--client FILE] [--failed-phase P] [--detail T] [--action A] [--bootstrap-template KEY SHA256 BYTES]
                                       print result.json, schema "1": state, sourceCommit, configDigest, buildId, stage, action (deploy unless given), region, account,
                                       endpoint, stackOutputs, client, phases (name, state, seconds, from the lines of FILE), failedPhase, detail, deploymentDigest, and
                                       bootstrapTemplate (key, sha256, bytes) when given: the CDK bootstrap template of a bootstrap-template build.

Exit codes: 0 done, 1 a value that is missing or invalid (the reason is on stderr), 2 a usage error.
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import sys
import zipfile
from typing import Any, Dict, List, Optional, Sequence, Tuple

SCHEMA = "1"
DETAIL_MAX = 500
HEX64 = re.compile(r"^[0-9a-f]{64}$")
PLATFORM = re.compile(r"^[a-z0-9]+-[a-z0-9]+$")
STATES = ("ok", "skipped", "failed")


def clean(text: Optional[str]) -> str:
    """One line of printable characters, at most DETAIL_MAX long."""
    out = "".join(c if c.isprintable() else " " for c in (text or ""))
    return " ".join(out.split())[:DETAIL_MAX]


def now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def cmd_json_get(args: argparse.Namespace) -> int:
    try:
        node: Any = json.load(sys.stdin)
    except ValueError as exc:
        sys.stderr.write("runner_state: not JSON: %s\n" % exc)
        return 1
    for part in args.path.split("."):
        if isinstance(node, list) and part.isdigit() and int(part) < len(node):
            node = node[int(part)]
        elif isinstance(node, dict) and part in node:
            node = node[part]
        else:
            sys.stderr.write("runner_state: no %s in the JSON\n" % args.path)
            return 1
    sys.stdout.write((node if isinstance(node, str) else json.dumps(node)) + "\n")
    return 0


def cmd_json_get_key(args: argparse.Namespace) -> int:
    try:
        doc = json.load(sys.stdin)
    except ValueError as exc:
        sys.stderr.write("runner_state: not JSON: %s\n" % exc)
        return 1
    if not isinstance(doc, dict) or args.key not in doc:
        sys.stderr.write("runner_state: no member %s in the JSON\n" % args.key)
        return 1
    node = doc[args.key]
    sys.stdout.write((node if isinstance(node, str) else json.dumps(node)) + "\n")
    return 0


def cmd_list_keys(args: argparse.Namespace) -> int:
    try:
        doc = json.load(sys.stdin)
    except ValueError as exc:
        sys.stderr.write("runner_state: not JSON: %s\n" % exc)
        return 1
    for item in (doc.get("Contents") or []) if isinstance(doc, dict) else []:
        key = item.get("Key")
        if isinstance(key, str) and key.endswith(args.suffix):
            sys.stdout.write(key + "\n")
    return 0


def cmd_zip_comment(args: argparse.Namespace) -> int:
    try:
        with zipfile.ZipFile(args.file) as z:
            sys.stdout.write(z.comment.decode("utf-8", "replace").strip() + "\n")
    except (OSError, zipfile.BadZipFile) as exc:
        sys.stderr.write("runner_state: %s is not a zip archive: %s\n" % (args.file, exc))
        return 1
    return 0


def cmd_phase_line(args: argparse.Namespace) -> int:
    if args.state not in STATES:
        sys.stderr.write("runner_state: state must be one of %s\n" % ", ".join(STATES))
        return 2
    line: Dict[str, Any] = {
        "schema": SCHEMA, "buildId": args.build_id, "stage": args.stage, "sourceCommit": args.commit, "configDigest": args.digest,
        "name": args.name, "state": args.state, "seconds": int(args.seconds), "detail": clean(args.detail), "at": now()}
    if args.from_build:
        line["from"] = args.from_build
    for item in args.extra or []:
        key, _, value = item.partition("=")
        if not key or key in line:
            sys.stderr.write("runner_state: bad --extra %r\n" % item)
            return 2
        line[key] = clean(value)
    sys.stdout.write(json.dumps(line, separators=(",", ":"), sort_keys=False) + "\n")
    return 0


def read_lines(path: str) -> List[Dict[str, Any]]:
    out: List[Dict[str, Any]] = []
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for raw in f:
                try:
                    doc = json.loads(raw)
                except ValueError:
                    continue
                if isinstance(doc, dict):
                    out.append(doc)
    except OSError:
        pass
    return out


def cmd_completed(args: argparse.Namespace) -> int:
    best: Dict[str, Tuple[str, int, Dict[str, Any]]] = {}
    order = 0
    for path in args.files:
        for doc in read_lines(path):
            order += 1
            if doc.get("state") != "ok" or doc.get("stage") != args.stage or doc.get("sourceCommit") != args.commit or doc.get("configDigest") != args.digest:
                continue
            name = doc.get("name")
            if not isinstance(name, str) or not isinstance(doc.get("buildId"), str):
                continue
            key = (str(doc.get("at") or ""), order)
            if name not in best or key >= best[name][:2]:
                best[name] = (key[0], key[1], doc)
    for name in sorted(best):
        doc = best[name][2]
        sys.stdout.write("%s\t%s\t%s\t%s\n" % (name, doc["buildId"], clean(str(doc.get("deploymentDigest") or "")), clean(str(doc.get("binary") or ""))))
    return 0


def check_file_entry(label: str, entry: Any, extra: Sequence[str] = ()) -> Optional[str]:
    """The three members of one published file (key below client/, 64 hex characters, a whole number of bytes), plus the members named in `extra`."""
    if not isinstance(entry, dict) or sorted(entry) != sorted(["bytes", "key", "sha256", *extra]):
        return "%s: expected exactly %s" % (label, ", ".join(["key", "sha256", "bytes", *extra]))
    if not isinstance(entry["key"], str) or not entry["key"].startswith("client/") or ".." in entry["key"]:
        return "%s: key must be an object key below client/" % label
    if not isinstance(entry["sha256"], str) or not HEX64.match(entry["sha256"]):
        return "%s: sha256 must be 64 lowercase hex characters" % label
    if isinstance(entry["bytes"], bool) or not isinstance(entry["bytes"], int) or entry["bytes"] < 0:
        return "%s: bytes must be a whole number" % label
    return None


def check_client(doc: Any) -> Optional[str]:
    if not isinstance(doc, dict) or not doc:
        return "the client section must be a JSON object with one member per platform"
    for platform, entry in doc.items():
        if not PLATFORM.match(platform):
            return "platform name %r is not like darwin-arm64" % platform
        problem = check_file_entry(platform, entry, ("admin",))
        if problem is None:
            problem = check_file_entry("%s admin" % platform, entry["admin"])
        if problem:
            return problem
    return None


def cmd_client_check(args: argparse.Namespace) -> int:
    try:
        with open(args.file, encoding="utf-8") as f:
            doc = json.load(f)
    except (OSError, ValueError) as exc:
        sys.stderr.write("runner_state: the client section is not JSON: %s\n" % clean(str(exc)))
        return 1
    problem = check_client(doc)
    if problem:
        sys.stderr.write("runner_state: %s\n" % problem)
        return 1
    for platform in sorted(doc):
        entry = doc[platform]["admin"] if args.part == "admin" else doc[platform]
        sys.stdout.write("%s\t%s\t%d\t%s\n" % (platform, entry["key"], entry["bytes"], entry["sha256"]))
    return 0


def cmd_stack_outputs(args: argparse.Namespace) -> int:
    try:
        doc = json.load(sys.stdin)
        outputs = doc["Stacks"][0].get("Outputs") or []
        out = {"%s.%s" % (args.stack, o["OutputKey"]): o["OutputValue"] for o in outputs}
    except (ValueError, KeyError, IndexError, TypeError) as exc:
        sys.stderr.write("runner_state: no outputs of %s: %s\n" % (args.stack, clean(str(exc))))
        return 1
    sys.stdout.write(json.dumps(out, sort_keys=True) + "\n")
    return 0


def load_json_file(path: str) -> Any:
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def cmd_result(args: argparse.Namespace) -> int:
    phases: List[Dict[str, Any]] = []
    for doc in read_lines(args.phases):
        if doc.get("buildId") == args.build_id:
            phases.append({"name": doc.get("name"), "state": doc.get("state"), "seconds": doc.get("seconds")})
    outputs: Dict[str, Any] = {}
    try:
        for path in args.outputs or []:
            outputs.update(load_json_file(path))
        client = load_json_file(args.client) if args.client else {}
    except (OSError, ValueError) as exc:
        sys.stderr.write("runner_state: cannot read a part of the result: %s\n" % clean(str(exc)))
        return 1
    result = {
        "schema": SCHEMA, "state": args.state, "sourceCommit": args.commit, "configDigest": args.digest, "buildId": args.build_id, "stage": args.stage,
        "action": clean(args.action) or "deploy", "region": args.region, "account": args.account, "endpoint": args.endpoint, "stackOutputs": outputs, "client": client, "phases": phases,
        "failedPhase": args.failed_phase or None, "detail": clean(args.detail), "deploymentDigest": args.deployment_digest}
    if args.bootstrap_template:
        key, sha, size = args.bootstrap_template
        if not key.startswith("bootstrap/") or ".." in key or not HEX64.match(sha) or not size.isdigit():
            sys.stderr.write("runner_state: --bootstrap-template wants a key below bootstrap/, 64 hex digits and a byte count\n")
            return 1
        result["bootstrapTemplate"] = {"key": key, "sha256": sha, "bytes": int(size)}
    sys.stdout.write(json.dumps(result, indent=1, sort_keys=False) + "\n")
    return 0


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="runner_state.py", allow_abbrev=False)
    sub = p.add_subparsers(dest="command", required=True)
    s = sub.add_parser("json-get")
    s.add_argument("path")
    s = sub.add_parser("json-get-key")
    s.add_argument("key")
    s = sub.add_parser("list-keys")
    s.add_argument("--suffix", default="")
    s = sub.add_parser("zip-comment")
    s.add_argument("file")
    s = sub.add_parser("phase-line")
    for name in ("build-id", "stage", "commit", "digest", "name", "state", "seconds"):
        s.add_argument("--" + name, required=True)
    s.add_argument("--detail", default="")
    s.add_argument("--from", dest="from_build", default="")
    s.add_argument("--extra", action="append")
    s = sub.add_parser("completed")
    for name in ("stage", "commit", "digest"):
        s.add_argument("--" + name, required=True)
    s.add_argument("files", nargs="*")
    s = sub.add_parser("client-check")
    s.add_argument("--part", choices=("client", "admin"), default="client")
    s.add_argument("file")
    s = sub.add_parser("stack-outputs")
    s.add_argument("--stack", required=True)
    s = sub.add_parser("result")
    for name in ("state", "build-id", "stage", "commit", "digest", "region", "account", "endpoint", "deployment-digest", "phases"):
        s.add_argument("--" + name, required=True)
    s.add_argument("--outputs", action="append")
    s.add_argument("--client")
    s.add_argument("--failed-phase", default="")
    s.add_argument("--detail", default="")
    s.add_argument("--action", default="deploy")
    s.add_argument("--bootstrap-template", nargs=3, metavar=("KEY", "SHA256", "BYTES"))
    return p


COMMANDS = {
    "json-get": cmd_json_get, "json-get-key": cmd_json_get_key, "list-keys": cmd_list_keys, "zip-comment": cmd_zip_comment, "phase-line": cmd_phase_line, "completed": cmd_completed,
    "client-check": cmd_client_check, "stack-outputs": cmd_stack_outputs, "result": cmd_result,
}


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parser().parse_args(argv)
    return COMMANDS[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
