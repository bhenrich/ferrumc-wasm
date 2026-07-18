#!/usr/bin/env python3
"""Reject unsupported public claims from the Git index."""

from __future__ import annotations

import argparse
import hashlib
import html
import json
import os
import posixpath
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath


PLUGIN_SOURCE = re.compile(
    r"^server/(?:crates/ferrumc-plugin-[^/]+|plugins/[^/]+)/src/.+\.rs$"
)
PLUGIN_ANCHOR = re.compile(
    r"\bplug-?ins?\b|"
    r"\b(?:native|server|third[- ]party)\s+extensions?\b|"
    r"\bextension[- ](?:api|host|runtime|loader|sdk|system)\b|"
    r"\btrusted native\b|\bcdylib\b|\bcapability manifests?\b",
    re.I,
)
EXTENSION_HEADING = re.compile(r"^(?:add[- ]ons?|extensions?)$", re.I)
PLUGIN_TERM = re.compile(r"\b(?:sandbox|sandboxed|isolated)\b", re.I)
WORLD_FASTEST = re.compile(r"\bworld(?:['’])s\s+fastest\b", re.I)
NO_DATA_LOSS = re.compile(r"\bno\s+data\s+loss\b", re.I)
HEADING = re.compile(r"^\s{0,3}(#{1,6})\s+(.*?)\s*$")
NEW_ITEM = re.compile(r"^\s*(?:[-+*]|\d+[.)])\s+|^\s*\|")
NEGATION = re.compile(
    r"\b(?:no|not|never|without|lacks?|does\s+not|doesn't|"
    r"is\s+not|isn't|are\s+not|aren't)\b",
    re.I,
)
LOCAL_BOUNDARY = re.compile(
    r"[–—]|\b(?:and|although|because|but|despite|however|nevertheless|or|"
    r"since|though|whereas|while|yet)\b",
    re.I,
)
CONDITION_BOUNDARY = re.compile(r"\b(?:if|provided|unless|when|whenever)\b", re.I)
MARKDOWN_LINK = re.compile(r"!?\[([^\]]+)\]\([^)]*\)")
MARKDOWN_REFERENCE = re.compile(r"!?\[([^\]]+)\]\[[^\]]*\]")
MARKDOWN_REFERENCE_DEFINITION = re.compile(
    r"^\s{0,3}\[[^\]]+\]:\s*\S+(?:\s+.*)?$", re.M
)
HTML_COMMENT = re.compile(r"<!--.*?-->", re.S)
HTML_TAG = re.compile(r"</?[A-Za-z][^>]*>")
INTERROGATIVE = re.compile(
    r"^(?:are|can|could|did|do|does|how|is|should|what|when|where|which|"
    r"who|why|will|would)\b",
    re.I,
)
POST_NEGATION = re.compile(
    r"\b(?:can(?:not|'t)|do|does|did|is|are|was|were|will|would)\s+"
    r"(?:not|never)\b|\b(?:is|are|was|were|remains?)\s+impossible\b|"
    r"\b(?:aren't|can't|couldn't|didn't|doesn't|don't|isn't|wasn't|"
    r"weren't|won't|wouldn't)\b|\bcannot\b",
    re.I,
)
INTERPOSED_POST_NEGATION = re.compile(
    r"^(?:can|could|did|do|does|is|are|was|were|will|would)\s*,"
    r"[^,\n]{1,80},\s*(?:not|never|impossible)\b",
    re.I,
)
LEADING_CONDITION = re.compile(
    r"^,?\s*(?:if|provided|unless|when|whenever)\b.{0,120}?"
    r"(?:,\s*)?(?=(?:can|could|did|do|does|is|are|was|were|will|would)\s+"
    r"(?:not|never)\b|cannot\b)",
    re.I,
)
NEGATIVE_COMPATIBILITY_OBJECT = re.compile(
    r"^(?:(?:across|for|with)\s+(?:no|neither)\b|"
    r"(?:has|offers?|provides?)\s+no\b)",
    re.I,
)
RELATIVE_SUBJECT = re.compile(r"^(?:that|whereas|which|while|who)\b", re.I)
UI_STATUS_SUFFIX = re.compile(
    r"^(?:/\s*[\w .-]+|with\s+a\s+(?:green\s+)?ping\s+bar"
    r"(?:\s+before\b[^.]*)?)?[.]?$",
    re.I,
)
UI_STATUS_SUBJECT = re.compile(
    r"\b(?:display|indicator|it|label|list|response|status|ui)\s+"
    r"(?:displays?|reads?|replies?|reports?|shows?)\s*$",
    re.I,
)
VERSION = re.compile(
    r"\b(?:v?\d+\.\d+(?:\.\d+)?|protocol(?:\s+version)?\s*\d+|"
    r"abi[_ -](?:major|minor)|abi\s+v?\d+|struct_size|versioned\s+c\s+abi)\b",
    re.I,
)
ABI_POLICY = re.compile(
    r"\b(?:abi[_ -](?:major|minor)|abi\s+v?\d+|struct_size|versioned\s+c\s+abi)\b",
    re.I,
)
TARGET = re.compile(
    r"\b(?:platform|target)[- ]specific\b|\b(?:linux|windows|macos|freebsd)\b",
    re.I,
)
TESTED_RANGE = re.compile(
    r"\btested\s+(?:against|with|on)\b.*\b\d+\.\d+(?:\.\d+)?\b", re.I
)
COMPATIBILITY = (
    (
        "absolute compatibility",
        re.compile(r"\b(?:fully|completely|universally|100\s*%)\s+compatible\b", re.I),
        "never",
    ),
    (
        "absolute compatibility noun",
        re.compile(
            r"\b(?:full|complete|universal|100\s*%)\s+compatibility\b",
            re.I,
        ),
        "never",
    ),
    (
        "target compatibility",
        re.compile(
            r"\bcompatible\s+with\s+(?:minecraft(?:\s+java(?:\s+edition)?)?|"
            r"vanilla|paper|spigot|bukkit|fabric|clients?|servers?|plugins?|"
            r"api|abi|protocol)\b",
            re.I,
        ),
        "version",
    ),
    ("cross-version compatibility", re.compile(r"\bworks?\s+across\b", re.I), "tested"),
    (
        "universal support",
        re.compile(
            r"\bsupports?\s+(?:all|any|every)\s+"
            r"(?:minecraft\s+)?"
            r"(?:versions?|clients?|servers?|platforms?|protocols?|plugins?)\b",
            re.I,
        ),
        "never",
    ),
    (
        "universal interoperability",
        re.compile(
            r"\bworks?\s+with\s+(?:all|any|every)\s+"
            r"(?:minecraft\s+)?(?:versions?|clients?|servers?|platforms?|"
            r"protocols?|plugins?)\b",
            re.I,
        ),
        "version",
    ),
    (
        "client interoperability",
        re.compile(
            r"\bworks?\s+with\s+(?:minecraft(?:\s+java(?:\s+edition)?)?"
            r"(?:\s+clients?)?|vanilla|paper|spigot|bukkit|fabric|clients?|"
            r"servers?|protocols?|plugins?)\b",
            re.I,
        ),
        "version",
    ),
    (
        "client support",
        re.compile(
            r"\bsupports?\s+(?:minecraft(?:\s+java(?:\s+edition)?)?"
            r"(?:\s+clients?)?|vanilla|paper|spigot|bukkit|fabric|clients?|"
            r"servers?|protocols?|plugins?)\b",
            re.I,
        ),
        "version",
    ),
    (
        "drop-in compatibility",
        re.compile(
            r"\b(?:is|are|provides?|offers?|acts?\s+as)\s+(?:an?\s+)?"
            r"drop[- ]in(?:\s+(?:replacement|compatible))?\b",
            re.I,
        ),
        "never",
    ),
    ("stable ABI", re.compile(r"\bstable\s+abi\b", re.I), "abi"),
    (
        "compatibility predicate",
        re.compile(
            r"\b(?:is|are|was|were|be|been|being|remains?|stays?|becomes?)\s+"
            r"(?:[\w%]+(?:[- /][\w%]+){0,3}[- ])?compatible\b",
            re.I,
        ),
        "scope",
    ),
    (
        "coordinated compatibility",
        re.compile(
            r"^(?:also\s+)?(?:(?:fully|completely|protocol|api|abi)[- ]*)?"
            r"compatible\b",
            re.I,
        ),
        "scope",
    ),
    (
        "compatibility guarantee",
        re.compile(
            r"\b(?:advertises?|claims?|delivers?|ensures?|guarantees?|maintains?|"
            r"has|offers?|preserves?|promises?|provides?)\s+"
            r"(?:\w+\s+){0,3}compatibility\b",
            re.I,
        ),
        "scope",
    ),
    (
        "compatibility quality",
        re.compile(
            r"\b(?:abi|api|client|plugin|protocol|server)?[- ]?compatibility\s+"
            r"(?:is|remains?|stays?)\s+(?:\w+\s+){0,2}"
            r"(?:complete|excellent|full|guaranteed|maintained|provided|"
            r"stable|supported)\b",
            re.I,
        ),
        "scope",
    ),
    (
        "universal compatibility noun",
        re.compile(
            r"\bcompatibility\s+with\s+(?:all|any|every)\s+"
            r"(?:minecraft\s+)?(?:versions?|clients?|servers?|platforms?|"
            r"protocols?|plugins?)\b",
            re.I,
        ),
        "version",
    ),
    (
        "named compatibility",
        re.compile(
            r"\b(?:ferrumc|minecraft|paper|spigot|bukkit|fabric|vanilla)"
            r"[- ]compatible\s+(?:clients?|servers?|plugins?|protocols?)\b",
            re.I,
        ),
        "version",
    ),
    (
        "generic compatibility",
        re.compile(r"\b(?:compatible|compatibility)\b", re.I),
        "generic",
    ),
)
ALLOWLIST_FIELDS = {
    "rule",
    "path",
    "term",
    "start_line",
    "end_line",
    "sha256",
    "reason",
}
ADR_PATH = "docs/adr/0008-trusted-native-plugins-c-abi.md"
ALLOWLIST_PATH = "scripts/forbidden-claims-allowlist.json"
ADR_BLOCK = (
    "FerrumC will support **trusted native plugins** through a versioned C ABI. That\n"
    "phrase is the required public term. No public copy, SDK page, rustdoc, or error\n"
    'may describe this runtime as a "sandbox"; capability checks do not change the\n'
    "process-wide trust model.\n"
)
ADR_START_LINE = 27
ADR_END_LINE = 30
ADR_DIGEST = hashlib.sha256(ADR_BLOCK.encode()).hexdigest()


class CheckError(RuntimeError):
    """A malformed source tree or allowlist."""


@dataclass(frozen=True)
class Block:
    path: str
    start: int
    end: int
    raw: str
    text: str
    lines: tuple[tuple[int, str], ...]
    plugin_context: bool

    @property
    def digest(self) -> str:
        return hashlib.sha256(self.raw.encode()).hexdigest()

    def line_for(self, pattern: re.Pattern[str]) -> int:
        return next((number for number, text in self.lines if pattern.search(text)), self.start)


@dataclass(frozen=True)
class Finding:
    rule: str
    path: str
    line: int
    term: str
    message: str
    block_start: int
    block_end: int
    block_digest: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.rule}: {self.message}"


@dataclass(frozen=True)
class RustToken:
    kind: str
    raw: str
    start: int
    end: int
    value: str = ""


def git(root: Path, *args: str) -> bytes:
    process = subprocess.run(
        ["git", "-C", str(root), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if process.returncode:
        detail = process.stderr.decode(errors="replace").strip()
        raise CheckError(f"git {' '.join(args)} failed: {detail}")
    return process.stdout


def index_paths(root: Path) -> set[str]:
    if git(root, "ls-files", "--unmerged", "-z"):
        raise CheckError("the Git index has unresolved entries")
    return {part.decode() for part in git(root, "ls-files", "-z").split(b"\0") if part}


def index_text(root: Path, path: str) -> str:
    try:
        return git(root, "show", f":{path}").decode()
    except UnicodeDecodeError as error:
        raise CheckError(f"indexed public source is not UTF-8: {path}") from error


def build_block(
    path: str,
    rows: list[tuple[int, str, str]],
    context: str,
    force_plugin: bool,
) -> Block:
    source_text = "\n".join(row[2] for row in rows)
    text = re.sub(r"\s+", " ", render_public_text(source_text)).strip()
    path_context = any("plugin" in part.casefold() for part in PurePosixPath(path).parts)
    visible_context = f"{render_public_text(context)} {text}"
    return Block(
        path=path,
        start=rows[0][0],
        end=rows[-1][0],
        raw="".join(row[1] for row in rows),
        text=text,
        lines=tuple((row[0], row[2]) for row in rows),
        plugin_context=force_plugin
        or path_context
        or PLUGIN_ANCHOR.search(visible_context) is not None,
    )


def markdown_blocks(path: str, text: str, force_plugin: bool = False) -> list[Block]:
    blocks: list[Block] = []
    headings: dict[int, str] = {}
    path_plugin = force_plugin or any(
        "plugin" in part.casefold() for part in PurePosixPath(path).parts
    )
    plugin_levels: dict[int, bool] = {0: path_plugin}
    pending: list[tuple[int, str, str]] = []

    def context() -> str:
        return " ".join(headings[level] for level in sorted(headings))

    def flush() -> None:
        if pending:
            level = max(headings, default=0)
            block = build_block(
                path,
                pending.copy(),
                context(),
                any(plugin_levels.values()),
            )
            blocks.append(block)
            if block.plugin_context:
                plugin_levels[level] = True
            pending.clear()

    for number, raw in enumerate(text.splitlines(keepends=True), 1):
        body = raw.rstrip("\r\n")
        match = HEADING.match(body)
        if match:
            flush()
            level = len(match.group(1))
            for old in tuple(headings):
                if old > level:
                    del headings[old]
            headings[level] = match.group(2)
            visible_context = render_public_text(context())
            visible_heading = render_public_text(match.group(2)).strip()
            parent_plugin = any(
                active for old, active in plugin_levels.items() if old < level
            )
            for old in tuple(plugin_levels):
                if old >= level:
                    del plugin_levels[old]
            section_plugin = (
                path_plugin
                or parent_plugin
                or EXTENSION_HEADING.fullmatch(visible_heading) is not None
                or PLUGIN_ANCHOR.search(visible_context) is not None
            )
            plugin_levels[level] = section_plugin
            blocks.append(
                build_block(
                    path,
                    [(number, raw, match.group(2))],
                    context(),
                    section_plugin,
                )
            )
        elif not body.strip():
            flush()
        else:
            if pending and NEW_ITEM.match(body):
                flush()
            pending.append((number, raw, body))
    flush()
    return blocks


def decode_rust_string(body: str, path: str, line: int) -> str:
    output: list[str] = []
    index = 0
    escapes = {
        "0": "\0",
        "n": "\n",
        "r": "\r",
        "t": "\t",
        "\\": "\\",
        '"': '"',
        "'": "'",
    }
    while index < len(body):
        if body[index] != "\\":
            output.append(body[index])
            index += 1
            continue
        index += 1
        if index == len(body):
            raise CheckError(f"invalid rustdoc string escape at {path}:{line}")
        escaped = body[index]
        if escaped in escapes:
            output.append(escapes[escaped])
            index += 1
        elif escaped == "x":
            digits = body[index + 1 : index + 3]
            if len(digits) != 2 or not re.fullmatch(r"[0-9a-fA-F]{2}", digits):
                raise CheckError(f"invalid rustdoc hex escape at {path}:{line}")
            output.append(chr(int(digits, 16)))
            index += 3
        elif escaped == "u" and body[index + 1 : index + 2] == "{":
            close = body.find("}", index + 2)
            digits = body[index + 2 : close].replace("_", "") if close >= 0 else ""
            if not digits or not re.fullmatch(r"[0-9a-fA-F]{1,6}", digits):
                raise CheckError(f"invalid rustdoc Unicode escape at {path}:{line}")
            try:
                output.append(chr(int(digits, 16)))
            except ValueError as error:
                raise CheckError(
                    f"invalid rustdoc Unicode scalar at {path}:{line}"
                ) from error
            index = close + 1
        elif escaped in "\r\n":
            if escaped == "\r" and body[index + 1 : index + 2] == "\n":
                index += 1
            index += 1
            while index < len(body) and body[index].isspace():
                index += 1
        else:
            raise CheckError(f"unsupported rustdoc string escape at {path}:{line}")
    return "".join(output)


def raw_string_at(text: str, start: int) -> tuple[int, str, bool, bool] | None:
    for prefix in ("br", "cr", "r"):
        if not text.startswith(prefix, start):
            continue
        quote = start + len(prefix)
        while quote < len(text) and text[quote] == "#":
            quote += 1
        if quote >= len(text) or text[quote] != '"':
            continue
        hashes = text[start + len(prefix) : quote]
        terminator = '"' + hashes
        end = text.find(terminator, quote + 1)
        if end < 0:
            return len(text), text[quote + 1 :], prefix == "r", False
        return end + len(terminator), text[quote + 1 : end], prefix == "r", True
    return None


def quoted_string_at(
    text: str, start: int, path: str
) -> tuple[int, str, bool] | None:
    prefix = ""
    quote = start
    if text[start : start + 2] in {'b"', 'c"'}:
        prefix = text[start]
        quote += 1
    elif text[start : start + 1] != '"':
        return None
    index = quote + 1
    while index < len(text):
        if text[index] == "\\":
            index += 2
            continue
        if text[index] == '"':
            body = text[quote + 1 : index]
            line = text.count("\n", 0, start) + 1
            value = decode_rust_string(body, path, line) if not prefix else body
            return index + 1, value, not prefix
        index += 1
    raise CheckError(
        f"unterminated Rust string at {path}:{text.count(chr(10), 0, start) + 1}"
    )


def char_literal_end(text: str, start: int) -> int | None:
    quote = start + 1 if text[start : start + 2] == "b'" else start
    if text[quote : quote + 1] != "'":
        return None
    index = quote + 1
    if index >= len(text) or text[index] in "\r\n":
        return None
    if text[index] == "\\":
        index += 2
        if text[index - 1 : index] == "u" and text[index : index + 1] == "{":
            close = text.find("}", index + 1)
            if close < 0:
                return None
            index = close + 1
    else:
        index += 1
    return index + 1 if text[index : index + 1] == "'" else None


def lex_rust(path: str, text: str) -> list[RustToken]:
    tokens: list[RustToken] = []
    index = 0
    while index < len(text):
        if text[index].isspace():
            index += 1
            continue
        if text.startswith("//", index):
            end = text.find("\n", index + 2)
            end = len(text) if end < 0 else end
            raw = text[index:end]
            is_doc = raw.startswith("//!") or (
                raw.startswith("///") and not raw.startswith("////")
            )
            value = raw[3:] if is_doc else ""
            tokens.append(
                RustToken("doc-comment" if is_doc else "comment", raw, index, end, value)
            )
            index = end
            continue
        if text.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(text) and depth:
                if text.startswith("/*", end):
                    depth += 1
                    end += 2
                elif text.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            if depth:
                line = text.count("\n", 0, index) + 1
                raise CheckError(f"unterminated Rust block comment at {path}:{line}")
            raw = text[index:end]
            is_doc = raw.startswith("/*!") or (
                raw.startswith("/**") and not raw.startswith("/***")
            )
            value = raw[3:-2] if is_doc else ""
            tokens.append(
                RustToken("doc-comment" if is_doc else "comment", raw, index, end, value)
            )
            index = end
            continue
        raw_string = raw_string_at(text, index)
        if raw_string is not None:
            end, value, textual, terminated = raw_string
            if not terminated:
                line = text.count("\n", 0, index) + 1
                raise CheckError(f"unterminated Rust raw string at {path}:{line}")
            tokens.append(
                RustToken(
                    "string" if textual else "nontext-string",
                    text[index:end],
                    index,
                    end,
                    value,
                )
            )
            index = end
            continue
        quoted = quoted_string_at(text, index, path)
        if quoted is not None:
            end, value, textual = quoted
            tokens.append(
                RustToken(
                    "string" if textual else "nontext-string",
                    text[index:end],
                    index,
                    end,
                    value,
                )
            )
            index = end
            continue
        char_end = char_literal_end(text, index)
        if char_end is not None:
            tokens.append(RustToken("char", text[index:char_end], index, char_end))
            index = char_end
            continue
        if text[index].isalpha() or text[index] == "_":
            end = index + 1
            while end < len(text) and (text[end].isalnum() or text[end] == "_"):
                end += 1
            tokens.append(RustToken("ident", text[index:end], index, end, text[index:end]))
            index = end
            continue
        tokens.append(RustToken("punct", text[index], index, index + 1, text[index]))
        index += 1
    return tokens


def parse_doc_expression(
    tokens: list[RustToken], start: int, path: str, line: int
) -> tuple[list[tuple[str, str]], int]:
    if start >= len(tokens):
        raise CheckError(f"missing rustdoc expression at {path}:{line}")
    token = tokens[start]
    if token.kind == "string":
        return [("text", token.value)], start + 1
    if token.kind != "ident" or token.value not in {"concat", "include_str"}:
        raise CheckError(f"unsupported rustdoc expression at {path}:{line}")
    if (
        start + 2 >= len(tokens)
        or tokens[start + 1].raw != "!"
        or tokens[start + 2].raw != "("
    ):
        raise CheckError(f"malformed rustdoc macro at {path}:{line}")
    if token.value == "include_str":
        if start + 3 >= len(tokens) or tokens[start + 3].kind != "string":
            raise CheckError(f"unsupported rustdoc include path at {path}:{line}")
        end = start + 4
        if end < len(tokens) and tokens[end].raw == ",":
            end += 1
        if end >= len(tokens) or tokens[end].raw != ")":
            raise CheckError(f"malformed rustdoc include at {path}:{line}")
        return [("include", tokens[start + 3].value)], end + 1

    pieces: list[tuple[str, str]] = []
    index = start + 3
    while index < len(tokens) and tokens[index].raw != ")":
        part, index = parse_doc_expression(tokens, index, path, line)
        if any(kind != "text" for kind, _ in part):
            raise CheckError(f"unsupported included rustdoc concat at {path}:{line}")
        pieces.extend(part)
        if index < len(tokens) and tokens[index].raw == ",":
            index += 1
        elif index >= len(tokens) or tokens[index].raw != ")":
            raise CheckError(f"malformed rustdoc concat at {path}:{line}")
    if index >= len(tokens):
        raise CheckError(f"unterminated rustdoc concat at {path}:{line}")
    return pieces, index + 1


def matching_token(
    tokens: list[RustToken],
    start: int,
    opening: str,
    closing: str,
    path: str,
    line: int,
) -> int:
    depth = 0
    for index in range(start, len(tokens)):
        if tokens[index].raw == opening:
            depth += 1
        elif tokens[index].raw == closing:
            depth -= 1
            if depth == 0:
                return index
    raise CheckError(f"unterminated rustdoc metadata at {path}:{line}")


def rustdoc_blocks(path: str, text: str) -> tuple[list[Block], set[str]]:
    tokens = lex_rust(path, text)
    blocks: list[Block] = []
    includes: set[str] = set()
    pending: list[RustToken] = []

    def flush_comments() -> None:
        if not pending:
            return
        start = text.count("\n", 0, pending[0].start) + 1
        end = text.count("\n", 0, pending[-1].end) + 1
        lines: list[tuple[int, str]] = []
        for token in pending:
            number = text.count("\n", 0, token.start) + 1
            cleaned = " ".join(
                re.sub(r"^\s*\*?\s?", "", row)
                for row in token.value.splitlines() or [""]
            )
            lines.append((number, cleaned))
        raw = text[pending[0].start : pending[-1].end]
        source_doc = "\n".join(row for _, row in lines)
        normalized = re.sub(r"\s+", " ", render_public_text(source_doc)).strip()
        blocks.append(Block(path, start, end, raw, normalized, tuple(lines), True))
        pending.clear()

    for token in tokens:
        if token.kind == "doc-comment":
            if pending:
                gap = text[pending[-1].end : token.start]
                if gap.strip() or gap.count("\n") > 1:
                    flush_comments()
            pending.append(token)
        else:
            flush_comments()
    flush_comments()

    filtered = [token for token in tokens if token.kind != "comment"]

    def add_attribute_text(first: RustToken, last: RustToken, value: str) -> None:
        line = text.count("\n", 0, first.start) + 1
        raw = text[first.start : last.end]
        blocks.append(build_block(path, [(line, raw, value)], "", True))

    index = 0
    while index < len(filtered):
        if filtered[index].raw != "#":
            index += 1
            continue
        opening = index + 1
        if opening < len(filtered) and filtered[opening].raw == "!":
            opening += 1
        if opening >= len(filtered) or filtered[opening].raw != "[":
            index += 1
            continue
        depth = 1
        closing = opening + 1
        while closing < len(filtered) and depth:
            if filtered[closing].raw == "[":
                depth += 1
            elif filtered[closing].raw == "]":
                depth -= 1
            closing += 1
        if depth:
            line = text.count("\n", 0, filtered[index].start) + 1
            raise CheckError(f"unterminated Rust attribute at {path}:{line}")
        attribute = filtered[opening + 1 : closing - 1]
        cursor = 0
        while cursor + 1 < len(attribute):
            if not (
                attribute[cursor].kind == "ident"
                and attribute[cursor].value == "doc"
            ):
                cursor += 1
                continue
            if attribute[cursor + 1].raw == "=":
                line = text.count("\n", 0, attribute[cursor].start) + 1
                pieces, next_cursor = parse_doc_expression(
                    attribute, cursor + 2, path, line
                )
                include_pieces = [value for kind, value in pieces if kind == "include"]
                if include_pieces:
                    if len(pieces) != 1:
                        raise CheckError(f"mixed rustdoc include at {path}:{line}")
                    includes.add(include_pieces[0])
                else:
                    doc = "".join(value for _, value in pieces)
                    add_attribute_text(
                        attribute[cursor],
                        attribute[next_cursor - 1],
                        doc,
                    )
                cursor = next_cursor
                continue
            if attribute[cursor + 1].raw != "(":
                cursor += 1
                continue

            line = text.count("\n", 0, attribute[cursor].start) + 1
            metadata_end = matching_token(
                attribute, cursor + 1, "(", ")", path, line
            )
            metadata = attribute[cursor + 2 : metadata_end]
            metadata_cursor = 0
            while metadata_cursor < len(metadata):
                token = metadata[metadata_cursor]
                if token.kind != "ident" or token.value != "alias":
                    if (
                        metadata_cursor + 1 < len(metadata)
                        and metadata[metadata_cursor + 1].raw == "("
                    ):
                        metadata_cursor = (
                            matching_token(
                                metadata,
                                metadata_cursor + 1,
                                "(",
                                ")",
                                path,
                                line,
                            )
                            + 1
                        )
                    else:
                        metadata_cursor += 1
                    continue
                if metadata_cursor + 1 >= len(metadata):
                    raise CheckError(f"malformed rustdoc alias at {path}:{line}")
                separator = metadata[metadata_cursor + 1].raw
                if separator == "=":
                    if (
                        metadata_cursor + 2 >= len(metadata)
                        or metadata[metadata_cursor + 2].kind != "string"
                    ):
                        raise CheckError(
                            f"unsupported rustdoc alias at {path}:{line}"
                        )
                    value = metadata[metadata_cursor + 2]
                    add_attribute_text(token, value, value.value)
                    metadata_cursor += 3
                    continue
                if separator != "(":
                    raise CheckError(f"malformed rustdoc alias at {path}:{line}")
                alias_end = matching_token(
                    metadata,
                    metadata_cursor + 1,
                    "(",
                    ")",
                    path,
                    line,
                )
                alias_cursor = metadata_cursor + 2
                while alias_cursor < alias_end:
                    value = metadata[alias_cursor]
                    if value.raw == ",":
                        alias_cursor += 1
                        continue
                    if value.kind != "string":
                        raise CheckError(
                            f"unsupported rustdoc alias list at {path}:{line}"
                        )
                    add_attribute_text(token, value, value.value)
                    alias_cursor += 1
                metadata_cursor = alias_end + 1
            cursor = metadata_end + 1
        index = closing
    return blocks, includes


def include_path(source: str, relative: str) -> str:
    if relative.startswith("/"):
        raise CheckError(f"absolute rustdoc include in {source}")
    resolved = posixpath.normpath(posixpath.join(posixpath.dirname(source), relative))
    if resolved == ".." or resolved.startswith("../"):
        raise CheckError(f"rustdoc include escapes the repository: {source}")
    return resolved


def collect_blocks(root: Path) -> tuple[list[Block], int]:
    paths = index_paths(root)
    blocks: list[Block] = []
    read: set[str] = set()
    includes: set[str] = set()
    for path in sorted(paths):
        if path == "README.md" or path.startswith("docs/"):
            blocks.extend(markdown_blocks(path, index_text(root, path)))
            read.add(path)
        elif PLUGIN_SOURCE.match(path):
            text = index_text(root, path)
            rust_blocks, rust_includes = rustdoc_blocks(path, text)
            blocks.extend(rust_blocks)
            includes.update(
                include_path(path, relative) for relative in rust_includes
            )
            read.add(path)
    for path in sorted(includes):
        if path not in paths:
            raise CheckError(f"rustdoc includes untracked path: {path}")
        blocks.extend(markdown_blocks(path, index_text(root, path), True))
        read.add(path)
    return blocks, len(read)


def clauses(text: str) -> list[str]:
    starts = [0]
    for offset, character in enumerate(text):
        if character not in ".;:!?|":
            continue
        before = text[offset - 1] if offset else ""
        after = text[offset + 1] if offset + 1 < len(text) else ""
        if character != "." or not (before.isdigit() and after.isdigit()):
            starts.append(offset + 1)
    starts.append(len(text) + 1)
    return [
        text[starts[index] : starts[index + 1]].strip()
        for index in range(len(starts) - 1)
        if text[starts[index] : starts[index + 1]].strip()
    ]


def scope_end(text: str, kind: str) -> int | None:
    versions = list(VERSION.finditer(text))
    abi_policies = list(ABI_POLICY.finditer(text))
    targets = list(TARGET.finditer(text))
    tested_ranges = list(TESTED_RANGE.finditer(text))
    if kind == "version":
        evidence = versions
    if kind == "abi":
        evidence = abi_policies + targets if abi_policies and targets else []
    elif kind == "tested":
        evidence = tested_ranges + targets if tested_ranges and targets else []
    elif kind in {"generic", "scope"}:
        evidence = versions + abi_policies + targets + tested_ranges
    elif kind != "version":
        evidence = []
    return max((item.end() for item in evidence), default=None)


def qualified(clause: str, match: re.Match[str], kind: str) -> bool:
    if kind == "never":
        return False
    if scope_end(clause[match.start() :], kind) is not None:
        return True

    prefix = clause[: match.start()]
    evidence_end = scope_end(prefix, kind)
    if evidence_end is None:
        return False
    qualifier = prefix[evidence_end:]
    if re.search(r"[()[\]{}]", qualifier):
        return False
    if re.search(r"\b(?:against|for|on|targeting|under)\b", prefix, re.I):
        return True
    return len(re.findall(r"\b\w+\b", qualifier)) <= 3


def post_negated_claim(clause: str, match: re.Match[str]) -> bool:
    suffix = clause[match.end() :].lstrip()
    if NEGATIVE_COMPATIBILITY_OBJECT.match(suffix):
        return True

    condition = LEADING_CONDITION.match(suffix)
    if condition is not None:
        matched = match.group().casefold()
        prefix = clause[: match.start()]
        condition_binds_to_claim = (
            "compatibility" in matched
            or matched.startswith("being ")
            or re.fullmatch(r"\s*being(?:\s+\w+){0,3}\s*", prefix, re.I)
            is not None
        )
        if condition_binds_to_claim:
            suffix = suffix[condition.end() :].lstrip()
    if POST_NEGATION.match(suffix) or INTERPOSED_POST_NEGATION.match(suffix):
        return True

    if CONDITION_BOUNDARY.match(suffix):
        return False
    if suffix.startswith((",", "(", "[", "{")):
        return False
    post = POST_NEGATION.search(suffix)
    if post is None:
        return False
    subject = suffix[: post.start()]
    if RELATIVE_SUBJECT.match(subject):
        return False
    return (
        re.fullmatch(r"(?:[a-z][\w'-]*\s+){1,5}", subject, re.I) is not None
    )


def negated_claim(clause: str, match: re.Match[str]) -> bool:
    matched = match.group()
    positive_not = re.search(r"\bnot\s+(?:just|merely|only)\b", matched, re.I)
    if positive_not is None and NEGATION.search(matched):
        return True
    if post_negated_claim(clause, match):
        return True
    prefix = clause[: match.start()].rstrip()
    if re.search(r"\bnot\s+(?:just|merely|only)\s*$", prefix, re.I):
        return False
    return (
        re.search(
            r"\b(?:(?:are|aren't|be|been|being|can|can't|did|do|does|"
            r"is|isn't|was|were|will|won't)\s+)?(?:not|never)\s*$",
            prefix,
            re.I,
        )
        is not None
        or re.search(
            r"\b(?:did|do|does|is|was|were)\s+not"
            r"(?:\s+\w+){0,3}\s*$",
            prefix,
            re.I,
        )
        is not None
        or re.search(
            r"\b(?:neither(?:\s+\w+){1,2}|no(?:\s+\w+){1,2}|"
            r"nobody|none|nothing)(?:\s+(?:are|has|have|is))?"
            r"(?:\s+(?:abi|api|completely|fully|protocol))?\s*$",
            prefix,
            re.I,
        )
        is not None
        or re.search(
            r"\b(?:are|guarantees?|has|have|is|offers?|promises?|provides?|"
            r"with)\s+no\s*$",
            prefix,
            re.I,
        )
        is not None
        or re.search(r"\bwithout(?:\s+\w+){0,2}\s*$", prefix, re.I) is not None
        or re.search(r"\blacks?(?:\s+\w+){0,2}\s*$", prefix, re.I) is not None
        or re.search(
            r"\bno\s+(?:\w+\s+){0,2}(?:has|have|is|are)\s*$",
            prefix,
            re.I,
        )
        is not None
    )


def claim_segments(text: str) -> list[str]:
    return [
        segment.strip()
        for clause in clauses(text)
        for segment in LOCAL_BOUNDARY.split(clause)
        if segment.strip()
    ]


def neutral_compatibility(clause: str, match: re.Match[str]) -> bool:
    word = match.group().casefold()
    prefix = clause[: match.start()]
    suffix = clause[match.end() :]
    if re.search(r"\bc[- ]compatible\b", match.group(), re.I):
        return True
    if word == "compatible":
        if re.search(r"\bc[- ]\s*$", prefix, re.I):
            return True
        if re.match(r"\s+additions?\b", suffix, re.I):
            return True
        return bool(
            match.group().isupper()
            and UI_STATUS_SUBJECT.search(prefix) is not None
            and UI_STATUS_SUFFIX.fullmatch(suffix.strip()) is not None
        )
    if clause.strip().casefold() == "compatibility":
        return True
    if re.search(r"\b(?:fixed|legacy\s+app)\s*$", prefix, re.I):
        return True
    if re.match(
        r"\s+(?:adapter|checks?|code|commitments?|costs?|dispatch|evidence|"
        r"ffi|gaps?|issues?|layer|limitations?|matrix|mechanism|method|notes?|"
        r"path|policy|requirements?|risks?|rules?|status|surface|tests?|"
        r"validation)\b",
        suffix,
        re.I,
    ):
        return True
    return (
        re.match(
            r"\s+(?:is\s+)?(?:checked|negotiated|tested|validated)\b",
            suffix,
            re.I,
        )
        is not None
    )


def render_public_text(text: str) -> str:
    rendered = HTML_COMMENT.sub("", text)
    rendered = MARKDOWN_REFERENCE_DEFINITION.sub("", rendered)
    rendered = MARKDOWN_LINK.sub(r"\1", rendered)
    rendered = MARKDOWN_REFERENCE.sub(r"\1", rendered)
    rendered = HTML_TAG.sub("", rendered)
    rendered = html.unescape(rendered)
    rendered = re.sub(r"[`*~]", "", rendered)
    return re.sub(
        r"(?<!\w)_{1,3}(?=\S)|(?<=\S)_{1,3}(?!\w)",
        "",
        rendered,
    )


def scan(blocks: list[Block]) -> list[Finding]:
    findings: list[Finding] = []
    seen: set[tuple[str, str, int, str]] = set()

    def add(
        block: Block,
        rule: str,
        match: re.Match[str],
        message: str,
        pattern: re.Pattern[str],
    ) -> None:
        key = (rule, block.path, block.line_for(pattern), match.group().casefold())
        if key in seen:
            return
        seen.add(key)
        findings.append(
            Finding(
                rule,
                block.path,
                key[2],
                match.group(),
                message,
                block.start,
                block.end,
                block.digest,
            )
    )

    for block in blocks:
        plain = render_public_text(block.text)
        if block.plugin_context:
            for match in PLUGIN_TERM.finditer(plain):
                term_pattern = re.compile(rf"\b{re.escape(match.group())}\b", re.I)
                add(
                    block,
                    "plugin-context-term",
                    match,
                    "unsupported plugin trust wording",
                    term_pattern,
                )
        for pattern, message in (
            (WORLD_FASTEST, "unsupported superlative claim"),
            (NO_DATA_LOSS, "unsupported durability claim"),
        ):
            for match in pattern.finditer(plain):
                add(block, "marketing-claim", match, message, pattern)
        for claim in claim_segments(plain):
            if claim.rstrip().endswith("?") and INTERROGATIVE.match(claim):
                continue
            for label, pattern, kind in COMPATIBILITY:
                for match in pattern.finditer(claim):
                    if neutral_compatibility(claim, match):
                        continue
                    if negated_claim(claim, match) or qualified(claim, match, kind):
                        continue
                    add(
                        block,
                        "unqualified-compatibility",
                        match,
                        f"{label} lacks explicit supported scope",
                        pattern,
                    )
    return findings


def load_allowlist(root: Path) -> list[dict[str, object]]:
    try:
        payload = json.loads(index_text(root, ALLOWLIST_PATH))
    except json.JSONDecodeError as error:
        raise CheckError(f"cannot read allowlist: {error}") from error
    if not isinstance(payload, dict):
        raise CheckError("allowlist must be a JSON object")
    entries = payload.get("exceptions")
    if payload.get("version") != 1 or not isinstance(entries, list) or len(entries) != 1:
        raise CheckError("allowlist must contain exactly the ADR-0008 exception")
    entry = entries[0]
    if not isinstance(entry, dict) or set(entry) != ALLOWLIST_FIELDS:
        raise CheckError("allowlist exception has invalid fields")
    if (
        entry["rule"] != "plugin-context-term"
        or entry["path"] != ADR_PATH
        or str(entry["term"]).casefold() != "sandbox"
        or entry["start_line"] != ADR_START_LINE
        or entry["end_line"] != ADR_END_LINE
        or entry["sha256"] != ADR_DIGEST
        or not re.fullmatch(r"[0-9a-f]{64}", str(entry["sha256"]))
        or not str(entry["reason"]).strip()
    ):
        raise CheckError("allowlist may contain only the exact ADR-0008 prohibition")
    return entries


def apply_allowlist(
    findings: list[Finding], entries: list[dict[str, object]]
) -> list[Finding]:
    remaining: list[Finding] = []
    used: set[int] = set()
    for finding in findings:
        matched = None
        for index, entry in enumerate(entries):
            if index in used:
                continue
            if (
                entry["rule"] == finding.rule
                and entry["path"] == finding.path
                and str(entry["term"]).casefold() == finding.term.casefold()
                and entry["start_line"] == finding.block_start
                and entry["end_line"] == finding.block_end
                and entry["sha256"] == finding.block_digest
            ):
                matched = index
                break
        if matched is None:
            remaining.append(finding)
        else:
            used.add(matched)
    for index, entry in enumerate(entries):
        if index not in used:
            remaining.append(
                Finding(
                    "stale-allowlist",
                    str(entry["path"]),
                    int(entry["start_line"]),
                    str(entry["term"]),
                    f"unused exception: {entry['reason']}",
                    int(entry["start_line"]),
                    int(entry["end_line"]),
                    str(entry["sha256"]),
                )
            )
    return remaining


def check(root: Path, entries: list[dict[str, object]]) -> tuple[list[Finding], int]:
    blocks, count = collect_blocks(root)
    return apply_allowlist(scan(blocks), entries), count


def fixture(base: Path, name: str, files: dict[str, str]) -> Path:
    root = base / name
    root.mkdir()
    git(root, "init", "--quiet")
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    git(root, "add", "--", *sorted(files))
    return root


def assert_case(
    root: Path,
    expected_rule: str | None,
    entries: list[dict[str, object]] | None = None,
) -> None:
    findings, _ = check(root, entries or [])
    if expected_rule is None and findings:
        raise CheckError(
            f"self-test {root.name} expected clean: {findings[0].render()}"
        )
    if expected_rule is not None and not any(item.rule == expected_rule for item in findings):
        raise CheckError(f"self-test {root.name} missed {expected_rule}")


def self_test(repo_root: Path) -> None:
    inherited_index = os.environ.pop("GIT_INDEX_FILE", None)
    scratch = repo_root / ".codex-tmp"
    existed = scratch.exists()
    scratch.mkdir(exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix="forbidden-claims-", dir=scratch) as temp:
            base = Path(temp)
            cases = (
                (
                    "clean",
                    {"README.md": "FerrumC targets Minecraft Java 1.21.8 (protocol 772).\n"},
                    None,
                ),
                (
                    "plugin",
                    {"docs/plugin.md": "## Plugins\n\nThis is a sandbox and is isolated.\n"},
                    "plugin-context-term",
                ),
                (
                    "plug-in",
                    {
                        "docs/extensions.md": (
                            "# Plug-in runtime\n\nThe runtime is sandboxed.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "extensions",
                    {
                        "docs/extensions.md": (
                            "# Extensions\n\nThe runtime is sandboxed.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "formatted-plugin-context",
                    {
                        "docs/runtime.md": (
                            "FerrumC loads plug**ins**. The runtime is isolated.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "entity-plugin-context",
                    {
                        "docs/runtime.md": (
                            "FerrumC loads plug&#105;ns. The runtime is isolated.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "linked-plugin-context",
                    {
                        "docs/runtime.md": (
                            "FerrumC loads [plug](target)ins. "
                            "The runtime is isolated.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "formatted-plugin-heading",
                    {
                        "docs/runtime.md": (
                            "# Plug**ins**\n\nThe runtime is isolated.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "hidden-comment-context",
                    {
                        "docs/bench.md": (
                            "# Benchmarks\n\n<!-- plugins -->\n\n"
                            "The runner uses isolated CPU cores.\n"
                        )
                    },
                    None,
                ),
                (
                    "hidden-link-target-context",
                    {
                        "docs/bench.md": (
                            "# Benchmarks\n\n"
                            "[documentation](https://plugin.example/)\n\n"
                            "The runner uses isolated CPU cores.\n"
                        )
                    },
                    None,
                ),
                (
                    "hidden-reference-target-context",
                    {
                        "docs/bench.md": (
                            "# Benchmarks\n\n"
                            "[documentation]: https://plugin.example/\n\n"
                            "The runner uses isolated CPU cores.\n"
                        )
                    },
                    None,
                ),
                (
                    "visible-link-label-context",
                    {
                        "docs/runtime.md": (
                            "# Runtime\n\n[Plugins](reference) are enabled.\n\n"
                            "The runtime is isolated.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "non-plugin",
                    {
                        "docs/bench.md": (
                            "The test used isolated cores.\n\n"
                            "Panic isolation was measured.\n"
                        )
                    },
                    None,
                ),
                (
                    "file-extensions",
                    {
                        "docs/storage.md": (
                            "# File extensions\n\n"
                            "The .mca extension is supported.\n\n"
                            "The parser uses isolated buffers.\n"
                        )
                    },
                    None,
                ),
                (
                    "plug-in-verb",
                    {
                        "docs/setup.md": (
                            "# Setup\n\nPlug in the cable.\n\n"
                            "Use an isolated supply.\n"
                        )
                    },
                    None,
                ),
                (
                    "rustdoc",
                    {"server/crates/ferrumc-plugin-demo/src/lib.rs": "//! A sandboxed runtime.\n"},
                    "plugin-context-term",
                ),
                (
                    "outer-block-rustdoc",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            "/** A sandboxed plugin runtime. */\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "inner-block-rustdoc",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            "/*! An isolated plugin runtime. */\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "raw-rustdoc",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![doc = r#"An isolated plugin runtime."#]\n'
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "concat-rustdoc",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![doc = concat!("A sand", "boxed plugin runtime.")]\n'
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "rustdoc-alias",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![doc(alias = "sandbox")]\n'
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "rustdoc-alias-list",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![doc(alias("safe", "sandboxed plugin"))]\n'
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "cfg-rustdoc-alias",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![cfg_attr(feature = "docs", doc(alias = "isolated"))]\n'
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "four-slashes",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            "//// A sandboxed non-doc comment.\n"
                        )
                    },
                    None,
                ),
                (
                    "string-doc-markers",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            'const A: &str = "/** A sandboxed plugin runtime. */";\n'
                            'const B: &str = r#"//! An isolated plugin runtime."#;\n'
                        )
                    },
                    None,
                ),
                (
                    "nested-block-rustdoc",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            "/** Before /* nested */ a sandboxed plugin runtime. */\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "marketing-world",
                    {"README.md": "The world's fastest server.\n"},
                    "marketing-claim",
                ),
                (
                    "marketing-loss",
                    {"README.md": "FerrumC promises no data loss.\n"},
                    "marketing-claim",
                ),
                (
                    "formatted-world",
                    {"README.md": "FerrumC is the world's **fastest** server.\n"},
                    "marketing-claim",
                ),
                (
                    "formatted-loss",
                    {"README.md": "FerrumC promises no **data loss**.\n"},
                    "marketing-claim",
                ),
                (
                    "linked-world",
                    {
                        "README.md": (
                            "FerrumC is the world's [fastest](bench.md) server.\n"
                        )
                    },
                    "marketing-claim",
                ),
                (
                    "linked-loss",
                    {
                        "README.md": (
                            "FerrumC promises no [data loss](policy.md).\n"
                        )
                    },
                    "marketing-claim",
                ),
                (
                    "html-world",
                    {
                        "README.md": (
                            "FerrumC is the world's <strong>fastest</strong> "
                            "server.\n"
                        )
                    },
                    "marketing-claim",
                ),
                (
                    "entity-world",
                    {"README.md": "FerrumC is the world&apos;s fastest server.\n"},
                    "marketing-claim",
                ),
                (
                    "entity-loss",
                    {"README.md": "FerrumC promises no&nbsp;data loss.\n"},
                    "marketing-claim",
                ),
                (
                    "works-across",
                    {"README.md": "FerrumC works across Rust versions.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "stable-abi",
                    {"README.md": "FerrumC has a stable ABI.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "qualified",
                    {
                        "README.md": (
                            "Compatible with Minecraft Java Edition 1.21.8 (protocol 772).\n"
                            "The list shows COMPATIBLE with a green ping bar.\n"
                            "Rust has no stable ABI.\n"
                            "Rust does not provide a stable ABI.\n"
                            "FerrumC is ABI compatible for `abi_major` 1.\n"
                            "FerrumC is ABI compatible for abi_minor 2.\n"
                            "FerrumC is ABI compatible through struct_size 64.\n"
                        )
                    },
                    None,
                ),
                (
                    "prefix-qualified",
                    {
                        "README.md": (
                            "For protocol 772, FerrumC is compatible.\n"
                            "Minecraft 1.21.8 clients are compatible.\n"
                            "Protocol 772 is compatible.\n"
                            "The ABI v1 plugin remains compatible.\n"
                            "On Linux the ABI v1 plugin is compatible.\n"
                        )
                    },
                    None,
                ),
                (
                    "conditional-qualified",
                    {
                        "README.md": (
                            "FerrumC is compatible when using protocol 772.\n"
                            "FerrumC is compatible if both sides use ABI v1.\n"
                            "FerrumC is compatible provided protocol 772 is used.\n"
                            "FerrumC is compatible with Paper when using "
                            "Minecraft 1.21.8.\n"
                        )
                    },
                    None,
                ),
                (
                    "generic-compatible",
                    {"README.md": "FerrumC is compatible.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "linked-compatible",
                    {
                        "README.md": (
                            "FerrumC is [compatible](SUPPORTED_VERSION.md).\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "entity-compatible",
                    {"README.md": "FerrumC is compat&#105;ble.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "protocol-compatible",
                    {"README.md": "FerrumC is protocol-compatible.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "slash-compatible",
                    {"README.md": "FerrumC is API/ABI compatible.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "coordinated-compatible",
                    {"README.md": "FerrumC is fast and compatible.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "compatibility-guarantee",
                    {"README.md": "FerrumC guarantees compatibility.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "compatibility-maintenance",
                    {"README.md": "FerrumC maintains compatibility.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "generic-compatibility",
                    {"README.md": "FerrumC boasts compatibility.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "full-compatibility",
                    {"README.md": "FerrumC has full compatibility.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "compatibility-quality",
                    {"README.md": "ABI compatibility is excellent.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "passive-compatibility",
                    {"README.md": "Compatibility is maintained.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "compatibility-heading",
                    {"README.md": "# Compatibility with every client\n"},
                    "unqualified-compatibility",
                ),
                (
                    "named-compatible",
                    {
                        "README.md": (
                            "FerrumC-compatible plugins are supported.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "universal-interoperability",
                    {
                        "README.md": (
                            "FerrumC works with every Minecraft client.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "client-interoperability",
                    {"README.md": "FerrumC works with Minecraft clients.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "client-support",
                    {"README.md": "FerrumC supports Minecraft clients.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "supports-every-client",
                    {
                        "README.md": (
                            "FerrumC supports every Minecraft client.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "supports-all-versions",
                    {
                        "README.md": (
                            "FerrumC supports all Minecraft versions.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "negative-compatibility",
                    {
                        "README.md": (
                            "FerrumC is not compatible.\n"
                            "FerrumC is not fully compatible.\n"
                            "FerrumC is not compatible with Paper.\n"
                            "FerrumC guarantees no compatibility.\n"
                        )
                    },
                    None,
                ),
                (
                    "negative-subjects",
                    {
                        "README.md": (
                            "No proxy is fully compatible.\n"
                            "No client is compatible.\n"
                            "No server is compatible with all clients.\n"
                            "Neither client is compatible.\n"
                            "Nothing is compatible.\n"
                        )
                    },
                    None,
                ),
                (
                    "negative-possession",
                    {
                        "README.md": (
                            "FerrumC lacks compatibility.\n"
                            "FerrumC lacks full compatibility.\n"
                            "FerrumC ships without compatibility.\n"
                            "FerrumC ships without full compatibility.\n"
                        )
                    },
                    None,
                ),
                (
                    "neutral-compatibility-terms",
                    {
                        "README.md": (
                            "# Compatibility\n\n"
                            "A C-compatible registration function.\n"
                            "This is a C-compatible registration function.\n"
                            "The compatibility adapter runs compatibility tests.\n"
                            "Version compatibility is checked in metadata.\n"
                            "The list shows COMPATIBLE with a green ping bar.\n"
                        )
                    },
                    None,
                ),
                (
                    "neutral-compatibility-status",
                    {
                        "README.md": (
                            "# Compatibility limitations\n\n"
                            "Compatibility risks remain.\n"
                            "Known compatibility issues are tracked.\n"
                            "Compatibility gaps are documented.\n"
                            "Compatibility evidence is recorded.\n"
                            "Compatibility status and notes follow.\n"
                        )
                    },
                    None,
                ),
                (
                    "compatible-behavior-claim",
                    {
                        "README.md": (
                            "FerrumC shows compatible behavior with every "
                            "Minecraft client.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "uppercase-compatible-product-claim",
                    {
                        "README.md": (
                            "FerrumC shows COMPATIBLE with a green ping bar.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "compatibility-question",
                    {"README.md": "Is FerrumC compatible?\n"},
                    None,
                ),
                (
                    "postpositive-negation",
                    {
                        "README.md": (
                            "A stable ABI is not guaranteed.\n"
                            "Being fully compatible is not a goal.\n"
                            "A stable ABI cannot be guaranteed.\n"
                            "Full compatibility is impossible.\n"
                            "A fully compatible implementation does not exist.\n"
                            "A fully compatible implementation will never exist.\n"
                        )
                    },
                    None,
                ),
                (
                    "postpositive-contractions",
                    {
                        "README.md": (
                            "A stable ABI isn't guaranteed.\n"
                            "Fully compatible clients aren't promised.\n"
                            "A stable ABI won't be guaranteed.\n"
                            "Full compatibility wouldn't be possible.\n"
                            "A fully compatible implementation doesn't exist.\n"
                            "A stable ABI can't be guaranteed.\n"
                        )
                    },
                    None,
                ),
                (
                    "postpositive-other-subject",
                    {
                        "README.md": (
                            "FerrumC is fully compatible, which Paper is not.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "parenthesized-other-subject",
                    {
                        "README.md": (
                            "FerrumC is compatible (Paper is not).\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "relative-other-subject",
                    {
                        "README.md": (
                            "FerrumC is compatible which Paper is not.\n"
                            "FerrumC is compatible which isn't true for Paper.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "conditional-other-subject",
                    {
                        "README.md": (
                            "FerrumC is compatible if Paper is not.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "interposed-negative-compatibility",
                    {
                        "README.md": (
                            "Compatibility, when enabled, is not guaranteed.\n"
                            "Compatibility is, in fact, not guaranteed.\n"
                            "FerrumC is compatible with no supported client.\n"
                            "Compatibility when enabled is not guaranteed.\n"
                            "Full compatibility when enabled cannot be guaranteed.\n"
                            "Being fully compatible when enabled is not a goal.\n"
                            "FerrumC has compatibility for no supported versions.\n"
                        )
                    },
                    None,
                ),
                (
                    "local-negation",
                    {
                        "README.md": (
                            "It is not experimental but is fully compatible.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "although-boundary",
                    {
                        "README.md": (
                            "It is not experimental although FerrumC is "
                            "fully compatible.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "while-boundary",
                    {
                        "README.md": (
                            "Protocol 772 is documented while FerrumC is "
                            "compatible with Paper.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "dash-boundary",
                    {
                        "README.md": (
                            "It is not experimental — FerrumC is fully compatible.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "because-negation",
                    {
                        "README.md": (
                            "FerrumC needs no proxy because it is fully compatible.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "conditional-negation",
                    {
                        "README.md": (
                            "FerrumC is fully compatible when the proxy is not "
                            "enabled.\n"
                            "FerrumC is compatible with Paper when the proxy "
                            "cannot start.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "local-version",
                    {
                        "README.md": (
                            "Protocol 772 is documented, and FerrumC is "
                            "compatible with Paper.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "because-version",
                    {
                        "README.md": (
                            "Protocol 772 is documented because FerrumC is "
                            "compatible with Paper.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "parenthesized-locality",
                    {
                        "README.md": (
                            "Protocol 772 (documented) FerrumC is compatible "
                            "with Paper.\n"
                        )
                    },
                    "unqualified-compatibility",
                ),
                (
                    "not-only-positive",
                    {"README.md": "FerrumC is not only fully compatible.\n"},
                    "unqualified-compatibility",
                ),
                (
                    "section-plugin-context",
                    {
                        "docs/runtime.md": (
                            "# Runtime\n\nFerrumC loads plugins.\n\nIt is isolated.\n"
                        )
                    },
                    "plugin-context-term",
                ),
                (
                    "include",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![doc = include_str!("../README.md")]\n'
                        ),
                        "server/crates/ferrumc-plugin-demo/README.md": "This plugin is isolated.\n",
                    },
                    "plugin-context-term",
                ),
                (
                    "raw-include",
                    {
                        "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                            '#![doc = include_str!(r"../README.md")]\n'
                        ),
                        "server/crates/ferrumc-plugin-demo/README.md": (
                            "This plugin is sandboxed.\n"
                        ),
                    },
                    "plugin-context-term",
                ),
            )
            for name, files, expected in cases:
                assert_case(fixture(base, name, files), expected)

            exception: dict[str, object] = {
                "rule": "plugin-context-term",
                "path": ADR_PATH,
                "term": "sandbox",
                "start_line": ADR_START_LINE,
                "end_line": ADR_END_LINE,
                "sha256": ADR_DIGEST,
                "reason": "self-test exact prohibition",
            }
            adr = "\n" * 26 + ADR_BLOCK
            allowlist_json = json.dumps(
                {"version": 1, "exceptions": [exception]}, indent=2
            )
            allowed = fixture(
                base,
                "allowed",
                {ADR_PATH: adr, ALLOWLIST_PATH: allowlist_json},
            )
            staged_allowlist = load_allowlist(allowed)
            assert_case(allowed, None, staged_allowlist)
            (allowed / ALLOWLIST_PATH).write_text('{"version": 0}\n')
            assert_case(allowed, None, load_allowlist(allowed))
            mutated = fixture(base, "mutated", {ADR_PATH: adr.replace('"sandbox"', '"sandboxed"')})
            assert_case(mutated, "stale-allowlist", [exception])

            changed_block = ADR_BLOCK.replace(
                "phrase is the required public term",
                "phrase remains the required public term",
            )
            changed_exception = exception | {
                "sha256": hashlib.sha256(changed_block.encode()).hexdigest()
            }
            tampered = fixture(
                base,
                "tampered-allowlist",
                {
                    ADR_PATH: "\n" * 26 + changed_block,
                    ALLOWLIST_PATH: json.dumps(
                        {"version": 1, "exceptions": [changed_exception]}
                    ),
                },
            )
            try:
                load_allowlist(tampered)
            except CheckError:
                pass
            else:
                raise CheckError("self-test accepted changed ADR prohibition text")

            unsupported = fixture(
                base,
                "unsupported-rustdoc",
                {
                    "server/crates/ferrumc-plugin-demo/src/lib.rs": (
                        '#![doc = env!("PLUGIN_DOC")]\n'
                    )
                },
            )
            try:
                check(unsupported, [])
            except CheckError:
                pass
            else:
                raise CheckError("self-test accepted unsupported rustdoc expression")

            indexed = fixture(base, "index-proof", {"README.md": "Scoped pre-alpha server.\n"})
            (indexed / "README.md").write_text("The world's fastest server.\n")
            assert_case(indexed, None)
            git(indexed, "add", "--", "README.md")
            (indexed / "README.md").write_text("Scoped pre-alpha server.\n")
            assert_case(indexed, "marketing-claim")
    finally:
        if inherited_index is not None:
            os.environ["GIT_INDEX_FILE"] = inherited_index
        if not existed:
            try:
                scratch.rmdir()
            except OSError:
                pass
    print(f"forbidden-claims self-test: ok ({len(cases) + 7} cases)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-index", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if not args.check_index and not args.self_test:
        raise CheckError("choose --check-index, --self-test, or both")
    root = Path(__file__).resolve().parent.parent
    if args.self_test:
        self_test(root)
    if args.check_index:
        allowlist = load_allowlist(root)
        findings, count = check(root, allowlist)
        if findings:
            for finding in findings:
                print(finding.render(), file=sys.stderr)
            return 1
        print(f"forbidden claims: index clean ({count} files scanned)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except CheckError as error:
        print(f"forbidden claims: {error}", file=sys.stderr)
        sys.exit(2)
