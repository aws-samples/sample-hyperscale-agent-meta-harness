"""Digests, shared by scripts/deploy.sh (writes them to SSM) and verify-all (PRECHECK).

- source_tree: SHA-256 over the git tree hashes of the deployable paths at HEAD.
- image_key: SHA-256 over the git hashes of what goes into a worker image (agentd's local crate
  closure, the Cargo files, accounts.json,
  engine/ except engine/catalog/ and engine/policy/, the image sources, images/tools (the pinned tool installers
  the image runs) and images/services, the native services installer).
  deploy.sh tags the image `in-<key>`, so a commit that leaves those inputs alone reuses the image
  and keeps imageDigest. The image never reads engine/catalog/ (the published model catalog) or
  engine/policy/ (the engine-policy versions agentd reads from the table).
- worker_image_key: `image_key` over the crate closure of the worker binary (agentd), exactly the key deploy.py
  tags the worker image with. Any function here takes a tree-ish `rev` (default HEAD), so scripts/image-key-guard.sh can pin
  the tree a commit is about to make.
- image_digest: SHA-256 over the sorted image digests the live endpoints serve plus the lock SHA-256.
- deployment_digest: SHA-256 over sourceTree, those image digests, the lock SHA-256, the ASL
  versions and the template hashes of the 9 named stacks of stage direct (absent stacks hash as `absent`).
"""

from __future__ import annotations

import hashlib
import json
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
from pathlib import Path

# accounts.json sets the account, Region and AZs every binary and stack is built for.
DEPLOYABLE_PATHS = ("crates", "infra", "images", "engine", "statemachines", "schemas", "fixtures/policy", "accounts.json")

# The 9 stacks of stage direct (infra/lib/config.ts STACK_NAMES; the digest order, deploy order comes from the CDK manifest).
STACK_NAMES = ("network", "security", "data", "packages", "control", "orchestration",
               "runtimes", "delivery", "observability")

# Worker image inputs besides agentd's crate closure, and the paths under them the image never reads.
# The recipe module and rust-toolchain.toml decide the bytes of the cross-built agentd without being crates:
# argv, CARGO_HOME remap, and the zig, cargo-zigbuild and rustc pins live in scripts/lib/agentd_build.py.
# LICENSE and NOTICE are copied into the image, so a change of either changes it.
IMAGE_PATHS = ("Cargo.toml", "Cargo.lock", "accounts.json", "engine", "images/worker-python",
               "images/codebuild/worker-python.yml", "images/services", "images/tools", "fixtures/cheat-suite",
               "scripts/lib/agentd_build.py", "rust-toolchain.toml", "LICENSE", "NOTICE")
IMAGE_EXCLUDED = ("engine/catalog", "engine/policy")


def _tree(root: Path, path: str, rev: str = "HEAD") -> str:
    proc = subprocess.run(["git", "-C", str(root), "rev-parse", f"{rev}:{path}"], capture_output=True, text=True)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool
    return proc.stdout.strip() if proc.returncode == 0 else "none"


def _children(root: Path, path: str, rev: str = "HEAD") -> list[str]:
    proc = subprocess.run(["git", "-C", str(root), "ls-tree", "-z", "--name-only", f"{rev}:{path}"],  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary; B607: bare name of a standard tool such as git, bash, tar or aws looked up on the PATH of the host running this tool
                          capture_output=True, text=True)
    return [f"{path}/{n}" for n in proc.stdout.split("\0") if n] if proc.returncode == 0 else []


def _without(root: Path, paths, excluded, rev: str = "HEAD") -> list[str]:
    """`paths` with each excluded path removed: a path that contains one is replaced by its children."""
    out: list[str] = []
    for p in paths:
        if p in excluded:
            continue
        if any(e.startswith(p + "/") for e in excluded):
            out.extend(_without(root, _children(root, p, rev), excluded, rev))
        else:
            out.append(p)
    return out


def _hash_paths(root: Path, paths, rev: str = "HEAD") -> str:
    h = hashlib.sha256()
    for p in paths:
        h.update(f"{p}:{_tree(root, p, rev)}\n".encode())
    return h.hexdigest()


def source_tree(root: Path) -> str:
    return _hash_paths(root, DEPLOYABLE_PATHS)


def local_crate_closure(metadata: dict, package: str) -> list[str]:
    """Workspace-relative directories of the path packages `package` builds from (normal and build deps)."""
    root = metadata["workspace_root"].rstrip("/") + "/"
    local = {p["id"]: p for p in metadata["packages"] if p.get("source") is None}
    start = next((i for i, p in local.items() if p["name"] == package), None)
    if start is None:
        raise KeyError(package)
    edges = {n["id"]: n.get("deps", []) for n in metadata["resolve"]["nodes"]}
    seen, todo = {start}, [start]
    while todo:
        for dep in edges.get(todo.pop(), []):
            kinds = {k.get("kind") for k in dep.get("dep_kinds", [{"kind": None}])}
            if dep["pkg"] in local and dep["pkg"] not in seen and kinds & {None, "build"}:
                seen.add(dep["pkg"])
                todo.append(dep["pkg"])
    dirs = {str(Path(local[i]["manifest_path"]).parent) for i in seen}
    return sorted(d[len(root):] if d.startswith(root) else d for d in dirs)


def image_key(root: Path, crate_dirs: list[str], rev: str = "HEAD") -> str:
    return _hash_paths(root, [*sorted(crate_dirs), *_without(root, IMAGE_PATHS, IMAGE_EXCLUDED, rev)], rev)


# The cargo packages whose binaries the worker image carries.
WORKER_IMAGE_PACKAGES = ("agentd",)


def worker_image_key(root: Path, metadata: dict, rev: str = "HEAD") -> str:
    """The key the worker image is tagged `in-<key>` with: the crate closure of the worker binary
    plus the image paths, at tree-ish `rev`."""
    crates = sorted({d for name in WORKER_IMAGE_PACKAGES for d in local_crate_closure(metadata, name)})
    return image_key(root, crates, rev)


def image_digest(image_digests: list[str], lock_sha256: str) -> str:
    h = hashlib.sha256()
    for d in sorted(image_digests):
        h.update(f"image:{d}\n".encode())
    h.update(f"lock:{lock_sha256}\n".encode())
    return h.hexdigest()


def template_hash(body) -> str:
    """Canonical JSON over key order; a non-JSON (YAML) body hashes as its text."""
    if isinstance(body, str):
        try:
            body = json.loads(body)
        except ValueError:
            return hashlib.sha256(body.encode()).hexdigest()
    return hashlib.sha256(json.dumps(body, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def deployment_digest(source_tree: str, image_digests: list[str], lock_sha256: str,
                      asl_versions: dict[str, str], template_hashes: dict[str, str | None]) -> str:
    h = hashlib.sha256()
    h.update(f"sourceTree:{source_tree}\n".encode())
    for d in sorted(image_digests):
        h.update(f"image:{d}\n".encode())
    h.update(f"lock:{lock_sha256}\n".encode())
    for name in sorted(asl_versions):
        h.update(f"asl:{name}={asl_versions[name]}\n".encode())
    for name in STACK_NAMES:
        h.update(f"stack:{name}={template_hashes.get(name) or 'absent'}\n".encode())
    return h.hexdigest()
