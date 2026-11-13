"""Small HTTP helpers shared by the API oracle tests. No proxy is ever used (trust_env=False):
the clean room sets HTTP_PROXY for pip and npm, and 127.0.0.1 must never go through it."""

from __future__ import annotations

import uuid
from typing import Any

import httpx
import pytest


def fail(message: str) -> None:
    pytest.fail(message, pytrace=False)


def expect(condition: object, message: str) -> None:
    if not condition:
        pytest.fail(message, pytrace=False)


def uniq(prefix: str = "oracle") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:12]}"


def new_client(base_url: str) -> httpx.Client:
    return httpx.Client(base_url=base_url, trust_env=False, timeout=30.0, follow_redirects=False)


def describe(resp: httpx.Response | None) -> str:
    if resp is None:
        return "no response"
    body = resp.text
    if len(body) > 300:
        body = body[:300] + "..."
    return f"HTTP {resp.status_code} content-type={resp.headers.get('content-type')!r} body={body!r}"


def is_2xx(resp: httpx.Response) -> bool:
    return 200 <= resp.status_code < 300


def is_4xx(resp: httpx.Response) -> bool:
    return 400 <= resp.status_code < 500


def is_5xx(resp: httpx.Response) -> bool:
    return resp.status_code >= 500


def json_or_none(resp: httpx.Response) -> Any:
    try:
        return resp.json()
    except ValueError:
        return None


def list_tasks(http: httpx.Client) -> list[dict]:
    r = http.get("/api/tasks")
    expect(r.status_code == 200, f"GET /api/tasks must answer 200; got {describe(r)}")
    body = json_or_none(r)
    expect(isinstance(body, list), f"GET /api/tasks must return a JSON array; got {describe(r)}")
    for item in body:
        expect(isinstance(item, dict), f"every element of GET /api/tasks must be a JSON object; got {item!r}")
    return body


def create_task(http: httpx.Client, title: str) -> dict:
    """POST a task and return the created task as the API reports it (fails the test if it does not)."""
    r = http.post("/api/tasks", json={"title": title})
    expect(is_2xx(r), f"POST /api/tasks with a title must succeed (2xx); got {describe(r)}")
    body = json_or_none(r)
    expect(isinstance(body, dict), f"POST /api/tasks must return the created task as a JSON object; got {describe(r)}")
    expect("id" in body and body["id"] is not None and body["id"] != "", f"the created task has no usable `id`: {describe(r)}")
    return body


def tasks_titled(tasks: list[dict], title: str) -> list[dict]:
    return [t for t in tasks if t.get("title") == title]


def task_url(task_id: object) -> str:
    return f"/api/tasks/{task_id}"
