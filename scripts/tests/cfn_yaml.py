"""A small YAML loader for the CloudFormation template and the CodeBuild buildspec that the setup runner ships.

Test support, not a test. The repository's tests use the Python standard library only, and it has no YAML parser. The two files
`infra/setup-runner.yaml` and `images/codebuild/setup-runner.yml` are written in a restricted style so that this loader reads them
exactly as PyYAML does; `test_setup_runner_cfn_yaml.py` holds the loader to that and, where PyYAML is installed, compares the two on the real files.

Supported: block mappings and sequences (a sequence may sit at the indent of its key), plain, single-quoted and double-quoted scalars,
the literal and folded block scalars (`|`, `>`, with the chomping marks `-` and `+`), one-line flow sequences and mappings, comments, and the
CloudFormation short tags (`!Ref`, `!Sub`, `!GetAtt`, `!Join` ...), which load as the long form (`{"Ref": ...}`, `{"Fn::Sub": ...}`).
Not supported, and refused with a YamlError: anchors and aliases, multi-line plain or quoted scalars, documents after the first, tabs for
indentation, a duplicate key.

Scalars resolve like YAML 1.1 for the cases a template holds: `true` and `false` in any of the three letter cases are booleans, `null`, `~` and
an empty value are None, a whole number is an int, a decimal such as `0.2` is a float, everything else is a string.
"""

from __future__ import annotations

import json
import re
from typing import Any, List, Optional, Tuple


class YamlError(ValueError):
    """A construct outside the supported style, or a malformed line."""


LONG_FORM = {"Ref": "Ref", "Condition": "Condition"}
INT = re.compile(r"^-?[0-9]+$")
FLOAT = re.compile(r"^-?[0-9]+\.[0-9]+$")
BOOL_TRUE = {"true", "True", "TRUE"}
BOOL_FALSE = {"false", "False", "FALSE"}
NULLS = {"null", "Null", "NULL", "~", ""}


def _tag_key(tag: str) -> str:
    name = tag[1:]
    return LONG_FORM.get(name, "Fn::" + name)


def _plain(text: str) -> Any:
    if text in NULLS:
        return None
    if text in BOOL_TRUE:
        return True
    if text in BOOL_FALSE:
        return False
    if INT.match(text):
        return int(text)
    if FLOAT.match(text):
        return float(text)
    return text


def _double_quoted(text: str, where: str) -> str:
    try:
        return json.loads(text)
    except ValueError:
        pass
    out, i = [], 1
    while i < len(text) - 1:
        c = text[i]
        if c == "\\" and i + 1 < len(text) - 1:
            n = text[i + 1]
            out.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\", "/": "/"}.get(n, "\\" + n))
            i += 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _strip_comment(line: str) -> str:
    """The line without a trailing comment: a `#` that starts the line or follows a space, outside quotes."""
    quote: Optional[str] = None
    for i, c in enumerate(line):
        if quote:
            if c == quote:
                if quote == "'" and line[i + 1:i + 2] == "'":
                    continue
                quote = None
            elif c == "\\" and quote == '"':
                continue
            continue
        if c in "'\"" and (i == 0 or line[i - 1] in " \t[{,:-!"):
            quote = c
        elif c == "#" and (i == 0 or line[i - 1] in " \t"):
            return line[:i].rstrip()
    return line.rstrip()


class _Flow:
    """A one-line flow collection or a scalar inside one."""

    def __init__(self, text: str, where: str) -> None:
        self.s, self.i, self.where = text, 0, where

    def err(self, msg: str) -> YamlError:
        return YamlError(f"{self.where}: {msg} in {self.s!r}")

    def ws(self) -> None:
        while self.i < len(self.s) and self.s[self.i] in " \t":
            self.i += 1

    def value(self) -> Any:
        self.ws()
        if self.i >= len(self.s):
            raise self.err("a value is missing")
        c = self.s[self.i]
        if c == "!":
            j = self.i
            while j < len(self.s) and self.s[j] not in " \t,]}":
                j += 1
            tag = self.s[self.i:j]
            self.i = j
            inner = self.value()
            return _tagged(tag, inner, self.where)
        if c == "[":
            self.i += 1
            items: List[Any] = []
            self.ws()
            if self.s[self.i:self.i + 1] == "]":
                self.i += 1
                return items
            while True:
                items.append(self.value())
                self.ws()
                n = self.s[self.i:self.i + 1]
                self.i += 1
                if n == "]":
                    return items
                if n != ",":
                    raise self.err("expected , or ]")
        if c == "{":
            self.i += 1
            out: dict = {}
            self.ws()
            if self.s[self.i:self.i + 1] == "}":
                self.i += 1
                return out
            while True:
                self.ws()
                key = self.scalar(stop=":,}]")
                self.ws()
                if self.s[self.i:self.i + 1] != ":":
                    raise self.err("expected : after a key")
                self.i += 1
                if key in out:
                    raise self.err(f"duplicate key {key!r}")
                out[key] = self.value()
                self.ws()
                n = self.s[self.i:self.i + 1]
                self.i += 1
                if n == "}":
                    return out
                if n != ",":
                    raise self.err("expected , or }")
        return _plain_or_quoted(self.scalar(stop=",]}", resolve=True), self.where)

    def scalar(self, stop: str, resolve: bool = False) -> Any:
        self.ws()
        c = self.s[self.i:self.i + 1]
        if c in ("'", '"'):
            j = self.i + 1
            while j < len(self.s):
                if self.s[j] == c:
                    if c == "'" and self.s[j + 1:j + 2] == "'":
                        j += 2
                        continue
                    break
                if self.s[j] == "\\" and c == '"':
                    j += 1
                j += 1
            if j >= len(self.s):
                raise self.err("unterminated quote")
            raw = self.s[self.i:j + 1]
            self.i = j + 1
            text = _double_quoted(raw, self.where) if c == '"' else raw[1:-1].replace("''", "'")
            return _Quoted(text)
        j = self.i
        while j < len(self.s):
            ch = self.s[j]
            if ch in stop and not (ch == ":" and j + 1 < len(self.s) and self.s[j + 1] not in " \t,]}"):
                break
            j += 1
        text = self.s[self.i:j].strip()
        self.i = j
        return text if not resolve else text


class _Quoted(str):
    """A quoted scalar: always a string, never resolved to a bool, a number or None."""


def _plain_or_quoted(value: Any, where: str) -> Any:
    if isinstance(value, _Quoted):
        return str(value)
    return _plain(value)


def _tagged(tag: str, inner: Any, where: str) -> Any:
    key = _tag_key(tag)
    if key == "Fn::GetAtt" and isinstance(inner, str):
        parts = inner.split(".", 1)
        return {key: parts}
    return {key: inner}


def parse_flow(text: str, where: str) -> Any:
    flow = _Flow(text, where)
    val = flow.value()
    flow.ws()
    if flow.i != len(flow.s):
        raise flow.err("text after the value")
    return val


def parse_inline(text: str, where: str) -> Any:
    """The value part of a line: a tagged value, a flow collection, a quoted or a plain scalar."""
    text = text.strip()
    if text.startswith("!"):
        sp = re.match(r"(![A-Za-z:]+)\s*(.*)$", text)
        if not sp:
            raise YamlError(f"{where}: bad tag in {text!r}")
        tag, rest = sp.group(1), sp.group(2)
        if rest == "":
            return ("tag", tag)
        return _tagged(tag, parse_inline(rest, where), where)
    if text.startswith("[") or text.startswith("{"):
        return parse_flow(text, where)
    if text.startswith("&") or text.startswith("*"):
        raise YamlError(f"{where}: anchors and aliases are not supported")
    if text[:1] in ("'", '"'):
        flow = _Flow(text, where)
        val = flow.scalar(stop="")
        flow.ws()
        if flow.i != len(flow.s):
            raise flow.err("text after a quoted scalar")
        return str(val)
    return _plain(text)


Line = Tuple[int, str, int]


def _tokenize(text: str) -> List[Tuple[int, str, str, int]]:
    """(indent, comment-free text, raw line, line number) for every line; blank and comment lines keep an empty text."""
    out = []
    for n, raw in enumerate(text.split("\n"), 1):
        if "\t" in raw[: len(raw) - len(raw.lstrip(" \t"))]:
            raise YamlError(f"line {n}: a tab in the indentation")
        stripped = _strip_comment(raw)
        indent = len(stripped) - len(stripped.lstrip(" "))
        out.append((indent, stripped.strip(), raw, n))
    return out


def _find_key(text: str) -> Optional[int]:
    """Index of the colon that ends a mapping key in `text`, or None. A key is plain or quoted; `a:b` is one scalar."""
    if text[:1] in ("'", '"'):
        quote = text[0]
        j = 1
        while j < len(text):
            if text[j] == quote:
                if quote == "'" and text[j + 1:j + 2] == "'":
                    j += 2
                    continue
                break
            if text[j] == "\\" and quote == '"':
                j += 1
            j += 1
        rest = text[j + 1:]
        return j + 1 if rest.startswith(":") and (len(rest) == 1 or rest[1] == " ") else None
    if text[:1] in ("[", "{", "!", "-"):
        if text[:2] != "- " and text != "-":
            if text[:1] in ("[", "{", "!"):
                return None
    for m in re.finditer(r":(?= |$)", text):
        return m.start()
    return None


def _key(text: str, where: str) -> str:
    text = text.strip()
    if text[:1] in ("'", '"'):
        return str(_Flow(text, where).scalar(stop=""))
    return text


class _Parser:
    def __init__(self, text: str) -> None:
        self.rows = _tokenize(text)
        self.n = len(self.rows)

    def skip_blank(self, i: int) -> int:
        while i < self.n and self.rows[i][1] == "":
            i += 1
        return i

    def where(self, i: int) -> str:
        return f"line {self.rows[i][3]}"

    # ---- block structures ----
    def block(self, i: int, indent: int) -> Tuple[Any, int]:
        i = self.skip_blank(i)
        if i >= self.n:
            return None, i
        ind, text, _, _ = self.rows[i]
        if ind < indent:
            return None, i
        if text.startswith("- ") or text == "-":
            return self.sequence(i, ind)
        if _find_key(text) is not None:
            return self.mapping(i, ind)
        value = parse_inline(text, self.where(i))
        if isinstance(value, tuple):
            raise YamlError(f"{self.where(i)}: a tag with no value")
        return value, i + 1

    def sequence(self, i: int, indent: int) -> Tuple[List[Any], int]:
        items: List[Any] = []
        while True:
            i = self.skip_blank(i)
            if i >= self.n:
                return items, i
            ind, text, _, _ = self.rows[i]
            if ind != indent or not (text.startswith("- ") or text == "-"):
                if ind > indent:
                    raise YamlError(f"{self.where(i)}: unexpected indentation")
                return items, i
            rest = text[1:].lstrip(" ")
            offset = ind + (len(text) - len(rest))
            if rest == "":
                value, i = self.block(i + 1, indent + 1)
                items.append(value)
                continue
            if _find_key(rest) is not None and not rest.startswith(("'", '"')) or (rest[:1] in ("'", '"') and _find_key(rest) is not None):
                # an inline mapping start: the key sits at `offset`
                self.rows[i] = (offset, rest, self.rows[i][2], self.rows[i][3])
                value, i = self.mapping(i, offset)
                items.append(value)
                continue
            value = self.scalar_value(i, rest, indent)
            if isinstance(value, tuple) and value[0] == "block":
                items.append(value[1])
                i = value[2]
            else:
                items.append(value)
                i += 1

    def mapping(self, i: int, indent: int) -> Tuple[dict, int]:
        out: dict = {}
        while True:
            i = self.skip_blank(i)
            if i >= self.n:
                return out, i
            ind, text, _, _ = self.rows[i]
            if ind < indent:
                return out, i
            if ind > indent:
                raise YamlError(f"{self.where(i)}: unexpected indentation")
            if text.startswith("- ") or text == "-":
                return out, i
            colon = _find_key(text)
            if colon is None:
                raise YamlError(f"{self.where(i)}: expected a key in {text!r}")
            key = _key(text[:colon], self.where(i))
            rest = text[colon + 1:].strip()
            if key in out:
                raise YamlError(f"{self.where(i)}: duplicate key {key!r}")
            if rest == "":
                j = self.skip_blank(i + 1)
                if j < self.n and self.rows[j][0] > indent:
                    value, i = self.block(j, self.rows[j][0])
                elif j < self.n and self.rows[j][0] == indent and (self.rows[j][1].startswith("- ") or self.rows[j][1] == "-"):
                    value, i = self.sequence(j, indent)
                else:
                    value, i = None, i + 1
                out[key] = value
                continue
            value = self.scalar_value(i, rest, indent)
            if isinstance(value, tuple) and value[0] == "block":
                out[key] = value[1]
                i = value[2]
            else:
                out[key] = value
                i += 1

    def scalar_value(self, i: int, rest: str, parent_indent: int) -> Any:
        """The value after `key:` or `- ` on row i: a block scalar (returns ("block", text, next row)), a tag over a nested block, or an inline value."""
        where = self.where(i)
        tag = None
        body = rest
        m = re.match(r"(![A-Za-z:]+)\s*(.*)$", rest)
        if m and m.group(2) in ("|", ">", "|-", ">-", "|+", ">+") or (m and m.group(2) == ""):
            tag, body = m.group(1), m.group(2)
        if body in ("|", ">", "|-", ">-", "|+", ">+"):
            text, nxt = self.block_scalar(i, body, parent_indent)
            return ("block", {_tag_key(tag): text} if tag else text, nxt)
        if tag and body == "":
            j = self.skip_blank(i + 1)
            if j < self.n and self.rows[j][0] > parent_indent:
                value, nxt = self.block(j, self.rows[j][0])
                return ("block", {_tag_key(tag): value}, nxt)
            raise YamlError(f"{where}: a tag with no value")
        value = parse_inline(rest, where)
        if isinstance(value, tuple):
            raise YamlError(f"{where}: a tag with no value")
        return value

    def block_scalar(self, i: int, style: str, parent_indent: int) -> Tuple[str, int]:
        folded = style[0] == ">"
        chomp = style[1:] if len(style) > 1 else ""
        j = i + 1
        lines: List[str] = []
        block_indent: Optional[int] = None
        while j < self.n:
            raw = self.rows[j][2]
            if raw.strip() == "":
                lines.append("")
                j += 1
                continue
            ind = len(raw) - len(raw.lstrip(" "))
            if ind <= parent_indent:
                break
            if block_indent is None:
                block_indent = ind
            if ind < block_indent:
                break
            lines.append(raw[block_indent:])
            j += 1
        # trailing blank lines belong to the chomping, not to the text
        trailing = 0
        while lines and lines[-1] == "":
            lines.pop()
            trailing += 1
        if folded:
            text = _fold(lines)
        else:
            text = "\n".join(lines)
        if not lines:
            return "", j
        if chomp == "-":
            return text, j
        if chomp == "+":
            return text + "\n" * (1 + trailing), j
        return text + "\n", j


def _fold(lines: List[str]) -> str:
    out: List[str] = []
    buf: List[str] = []
    for ln in lines:
        if ln == "":
            if buf:
                out.append(" ".join(buf))
                buf = []
            out.append("")
        elif ln.startswith(" "):
            if buf:
                out.append(" ".join(buf))
                buf = []
            out.append(ln)
        else:
            buf.append(ln)
    if buf:
        out.append(" ".join(buf))
    text = ""
    for k, part in enumerate(out):
        if k and part != "" and out[k - 1] != "":
            text += "\n"
        elif k and part == "":
            text += "\n"
        text += part
    return text


def load(text: str) -> Any:
    if "\n---" in "\n" + text and re.search(r"(?m)^---\s*$", text) and not re.match(r"\A---\s*\n", text):
        raise YamlError("a second document")
    body = re.sub(r"\A---\s*\n", "", text)
    p = _Parser(body)
    value, i = p.block(0, 0)
    i = p.skip_blank(i)
    if i < p.n:
        raise YamlError(f"{p.where(i)}: text after the document")
    return value
