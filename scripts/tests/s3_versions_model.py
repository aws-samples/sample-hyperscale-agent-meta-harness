"""A model of S3 version listing and multi-object delete, and of what the AWS CLI v2 does with them (used by the stand-in AWS CLIs of the setup tests).

The stand-ins used to answer the one query string that `scripts/setup/empty-bucket.sh` sent and to cut the combined list at `--max-items`, so a script
that drifted from the real behavior still passed. This module keeps the rules of the real services instead of the shape of one query:

- ListObjectVersions returns at most `max-keys` entries per call (1,000 at most, 1,000 by default), versions and delete markers counted together, in key
  order. A truncated answer names the markers for the next call. [S3 API reference, ListObjectVersions: "By default, the action returns up to 1,000 key
  names. The response might contain fewer keys but will never contain more."]
- `aws s3api list-object-versions` paginates by itself. `--max-items N` counts the first result key (`Versions`) only: the CLI reads whole pages until it holds
  N versions, cuts the versions to N, and keeps every delete marker of the pages it read. So a bucket with delete markers among its versions gives a list of
  more than N entries. `--no-paginate` makes one call and answers its page as it is. A manual `--max-keys` turns pagination off by itself, and
  `--no-paginate` together with `--max-items`, `--page-size` or `--starting-token` is an error. [AWS CLI user guide, pagination; botocore PageIterator
  truncates the primary result key only. The numbers of the reviewed shapes were measured with the real CLI v2.37.2 against a loopback S3.]
- DeleteObjects takes up to 1,000 keys; more (or none) is MalformedXML and nothing is deleted. A key that is not there counts as deleted. An object under
  Object Lock in governance mode is an `Errors` entry unless the call carries the bypass. With `Quiet` only the errors come back. [S3 API reference,
  DeleteObjects.]

Standard library only.
"""

from __future__ import annotations

from typing import Any, Callable, Dict, List, Optional

MAX_KEYS = 1000
DELETE_LIMIT = 1000


class ServiceError(Exception):
    """An error the service answers with (the AWS CLI prints `An error occurred (<code>) when calling the <operation> operation: <message>` and exits 254)."""

    def __init__(self, code: str, message: str, operation: str, exit_code: int = 254) -> None:
        super().__init__(message)
        self.code, self.operation, self.exit_code = code, operation, exit_code

    def text(self) -> str:
        return f"An error occurred ({self.code}) when calling the {self.operation} operation: {self}"


class UsageError(Exception):
    """The AWS CLI refuses the arguments before it calls anything (exit 252)."""


Entry = Dict[str, Any]  # {"Key": str, "VersionId": str, "marker": bool}


def s3_page(entries: List[Entry], bucket: str = "bucket", max_keys: Optional[int] = None, key_marker: str = "", version_id_marker: str = "") -> Dict[str, Any]:
    """One ListObjectVersions call over `entries`, which are in S3 order (by key; the versions of a key newest first)."""
    limit = MAX_KEYS if max_keys is None else min(int(max_keys), MAX_KEYS)
    if limit < 1:
        raise ServiceError("InvalidArgument", "Argument maxKeys must be an integer between 0 and 2147483647", "ListObjectVersions", 254)
    rest = entries
    if key_marker:
        at = next((i for i, e in enumerate(entries) if e["Key"] == key_marker and (not version_id_marker or e["VersionId"] == version_id_marker)), None)
        rest = entries[at + 1:] if at is not None else [e for e in entries if e["Key"] > key_marker]
    page = rest[:limit]
    truncated = len(rest) > limit
    out: Dict[str, Any] = {"IsTruncated": truncated, "KeyMarker": key_marker, "VersionIdMarker": version_id_marker or "", "MaxKeys": limit, "Name": bucket, "Prefix": ""}
    if truncated:
        out["NextKeyMarker"], out["NextVersionIdMarker"] = page[-1]["Key"], page[-1]["VersionId"]
    versions = [{"ETag": '"d41d8cd98f00b204e9800998ecf8427e"', "Size": 1, "StorageClass": "STANDARD", "Key": e["Key"], "VersionId": e["VersionId"], "IsLatest": True,
                 "LastModified": "2026-10-30T00:00:00+00:00"} for e in page if not e.get("marker")]
    markers = [{"Key": e["Key"], "VersionId": e["VersionId"], "IsLatest": True, "LastModified": "2026-10-30T00:00:00+00:00"} for e in page if e.get("marker")]
    if versions:
        out["Versions"] = versions
    if markers:
        out["DeleteMarkers"] = markers
    return out


def cli_list(entries: List[Entry], bucket: str = "bucket", no_paginate: bool = False, max_keys: Optional[int] = None, page_size: Optional[int] = None,
             max_items: Optional[int] = None, starting_token: Optional[str] = None) -> Dict[str, Any]:
    """What `aws s3api list-object-versions` prints (before --query) for these arguments."""
    if no_paginate and (max_items is not None or page_size is not None or starting_token is not None):
        raise UsageError("Cannot specify --no-paginate along with pagination arguments: " + ", ".join(
            n for n, v in (("--max-items", max_items), ("--page-size", page_size), ("--starting-token", starting_token)) if v is not None))
    if no_paginate or max_keys is not None:
        return s3_page(entries, bucket, max_keys)
    size = MAX_KEYS if page_size is None else min(int(page_size), MAX_KEYS)
    versions: List[Dict[str, Any]] = []
    markers: List[Dict[str, Any]] = []
    key_marker, version_marker = "", ""
    more = False
    while True:
        page = s3_page(entries, bucket, size, key_marker, version_marker)
        versions += page.get("Versions", [])
        markers += page.get("DeleteMarkers", [])
        more = page["IsTruncated"]
        if max_items is not None and len(versions) >= int(max_items):
            more = more or len(versions) > int(max_items)
            versions = versions[: int(max_items)]
            break
        if not more:
            break
        key_marker, version_marker = page["NextKeyMarker"], page["NextVersionIdMarker"]
    out: Dict[str, Any] = {}
    if versions:
        out["Versions"] = versions
    if markers:
        out["DeleteMarkers"] = markers
    if more:
        out["NextToken"] = "model-token"
    return out


def delete_objects(objects: List[Dict[str, str]], quiet: bool = False, bypass: bool = False, protected: Optional[Callable[[str, str], bool]] = None,
                   remove: Optional[Callable[[str, str], None]] = None) -> Dict[str, Any]:
    """DeleteObjects for these {Key, VersionId} objects. `protected(key, version)` says an object is under governance retention; `remove(key, version)` deletes one."""
    if not objects or len(objects) > DELETE_LIMIT:
        raise ServiceError("MalformedXML", "The XML you provided was not well-formed or did not validate against our published schema", "DeleteObjects", 254)
    deleted: List[Dict[str, str]] = []
    errors: List[Dict[str, str]] = []
    for o in objects:
        key, vid = o["Key"], o.get("VersionId", "")
        if protected is not None and protected(key, vid) and not bypass:
            errors.append({"Key": key, "VersionId": vid, "Code": "AccessDenied", "Message": "Access Denied because object protected by object lock."})
            continue
        if remove is not None:
            remove(key, vid)
        deleted.append({"Key": key, "VersionId": vid})
    out: Dict[str, Any] = {}
    if deleted and not quiet:
        out["Deleted"] = deleted
    if errors:
        out["Errors"] = errors
    return out


def entries_from_versions(versions: Dict[str, List[Dict[str, Any]]]) -> List[Entry]:
    """The entries of a bucket kept as {key: [{"id": version id, "marker": bool}, ...]} (oldest version first), in S3 order."""
    out: List[Entry] = []
    for key in sorted(versions):
        for v in reversed(versions[key]):
            out.append({"Key": key, "VersionId": v["id"], "marker": bool(v["marker"])})
    return out


def shape(versions: int, markers: int, layout: str = "markers-first") -> List[Entry]:
    """A bucket of `versions` object versions and `markers` delete markers, in S3 order. `markers-first`: every marker sorts before every version;
    `versions-first`: after; `interleaved`: the keys alternate, so a page of the listing holds both kinds."""
    out: List[Entry] = []
    if layout == "interleaved":
        v = m = 0
        n = 0
        while v < versions or m < markers:
            if v < versions:
                out.append({"Key": f"k/{n:07d}.txt", "VersionId": f"v{v:07d}", "marker": False})
                v += 1
                n += 1
            if m < markers:
                out.append({"Key": f"k/{n:07d}.txt", "VersionId": f"m{m:07d}", "marker": True})
                m += 1
                n += 1
        return out
    mark, ver = ("a-mark", "b-obj") if layout == "markers-first" else ("b-mark", "a-obj")
    out += [{"Key": f"{mark}/{i:06d}.txt", "VersionId": f"m{i:06d}", "marker": True} for i in range(markers)]
    out += [{"Key": f"{ver}/{i:06d}.txt", "VersionId": f"v{i:06d}", "marker": False} for i in range(versions)]
    out.sort(key=lambda e: (e["Key"], e["VersionId"]))
    return out
