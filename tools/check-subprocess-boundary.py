#!/usr/bin/env python3
"""Enforce the single production boundary for external process execution.

The runtime process broker lives in ``src/subprocess.rs``.  This check is
deliberately conservative: it lexes enough Rust to ignore comments/literals,
tracks ``use`` and ``extern crate`` forms which can alias ``std::process``, and
rejects direct ``Command`` construction everywhere else.  It additionally
resolves local ``macro_rules!`` calls for the narrow metavariable path shapes
that can hide ``Command::new``; arbitrary macro expansion remains outside this
lexical check. It scans production sources under ``src`` plus ``build.rs``;
separate test/example programs are outside this runtime-boundary contract. It
is not intended to be a Rust parser or a replacement for ``cargo check``.

There are three narrowly scoped legacy exceptions.  ``build.rs`` keeps its
one build-time probe, while the two existing ffmpeg fixture groups in
``src/decoder.rs`` and ``src/mxf_qc.rs`` may keep their exact test-only uses.
The exception is an exact path/kind/count contract, rather than a wildcard or
an exemption for an entire source tree.  This makes a removed or changed
legacy site fail closed instead of silently widening the bypass.
"""

from __future__ import annotations

import argparse
import bisect
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
BROKER_RELATIVE_PATH = "src/subprocess.rs"


@dataclass(frozen=True)
class AllowRule:
    """Exact legacy exception contract for one source file."""

    expected_kinds: tuple[tuple[str, int], ...]
    test_only: bool
    # Each fingerprint includes the constructor kind/path and its first
    # argument.  Keeping it separate from the kind counts prevents a
    # ``pkg-config`` exception from silently becoming a ``curl`` exception.
    expected_fingerprints: tuple[tuple[str, int], ...] = ()

    @property
    def expected(self) -> dict[str, int]:
        return dict(self.expected_kinds)

    @property
    def fingerprints(self) -> dict[str, int]:
        return dict(self.expected_fingerprints)


# Keep this list explicit.  In particular, do not turn it into a glob or add
# a broad ``tests``/``src`` exemption: every occurrence and its shape matters.
ALLOWLIST: dict[str, AllowRule] = {
    "build.rs": AllowRule(
        expected_kinds=(("qualified-command-new", 1),),
        test_only=False,
        expected_fingerprints=(
            ('qualified-command-new|std::process::Command::new|"pkg-config"', 1),
        ),
    ),
    "src/decoder.rs": AllowRule(
        expected_kinds=(("qualified-command-new", 1),),
        test_only=True,
        expected_fingerprints=(
            ('qualified-command-new|std::process::Command::new|"ffmpeg"', 1),
        ),
    ),
    "src/mxf_qc.rs": AllowRule(
        expected_kinds=(
            ("command-new", 4),
            ("qualified-command-import", 1),
        ),
        test_only=True,
        expected_fingerprints=(('command-new|Command::new|"ffmpeg"', 4),),
    ),
}

ALLOWED_EXCEPTION_PATHS = frozenset(ALLOWLIST)
ALLOWED_EXCEPTION_KINDS = frozenset(
    {
        "command-new",
        "command",
        "command-alias",
        "macro-command-new",
        "macro-process-command-new",
        "macro-qualified-command-new",
        "module-command",
        "module-command-new",
        "root-command",
        "root-command-new",
        "qualified-command-import",
        "qualified-command-new",
        "ufcs-command-new",
    }
)


class BoundaryError(Exception):
    """One or more subprocess-boundary contract violations."""


@dataclass(frozen=True)
class Token:
    text: str
    start: int
    end: int
    line: int
    column: int


@dataclass(frozen=True)
class UseImport:
    path: tuple[str, ...]
    alias: str | None
    token_index: int


@dataclass(frozen=True)
class Finding:
    relative_path: str
    line: int
    column: int
    kind: str
    token_index: int
    in_cfg_test: bool
    fingerprint: str | None = None

    def describe(self) -> str:
        return (
            f"{self.relative_path}:{self.line}:{self.column}: "
            f"direct std::process::Command use ({self.kind})"
        )


@dataclass(frozen=True)
class SourceScan:
    relative_path: str
    findings: tuple[Finding, ...]
    structural_errors: tuple[str, ...]


@dataclass(frozen=True)
class MacroUse:
    """A process-construction shape in a macro transcriber."""

    parameter: str
    kind: str
    token_index: int


@dataclass(frozen=True)
class MacroArm:
    """The fragment parameters and relevant uses of one macro arm."""

    parameters: tuple[str, ...]
    uses: tuple[MacroUse, ...]


@dataclass(frozen=True)
class MacroDefinition:
    """A local ``macro_rules!`` definition and its token range."""

    name: str
    start: int
    end: int
    arms: tuple[MacroArm, ...]


def _line_starts(source: str) -> list[int]:
    starts = [0]
    for index, character in enumerate(source):
        if character == "\n":
            starts.append(index + 1)
    return starts


def _location(starts: Sequence[int], position: int) -> tuple[int, int]:
    line_index = bisect.bisect_right(starts, position) - 1
    return line_index + 1, position - starts[line_index] + 1


def _is_identifier_start(character: str) -> bool:
    return character == "_" or character.isalpha() or character == "_"


def _is_identifier_continue(character: str) -> bool:
    return _is_identifier_start(character) or character.isdecimal()


def _raw_literal_end(source: str, start: int) -> int | None:
    """Return the end of a raw string/byte string beginning at *start*."""

    index = start
    if source.startswith("br", index) or source.startswith("rb", index):
        index += 2
    elif source.startswith("r", index):
        index += 1
    else:
        return None

    hashes = 0
    while index < len(source) and source[index] == "#":
        hashes += 1
        index += 1
    if index >= len(source) or source[index] != '"':
        return None

    closing = '"' + ("#" * hashes)
    end = source.find(closing, index + 1)
    if end < 0:
        return len(source)
    return end + len(closing)


def _char_literal_end(source: str, start: int) -> int | None:
    """Return the end of a char literal, or ``None`` for a Rust lifetime."""

    if source[start] != "'":
        return None
    if start + 2 >= len(source) or source[start + 1] in {"\n", "\r"}:
        return None
    if source[start + 1] != "\\":
        # A non-escaped Rust char is exactly one Unicode scalar followed by
        # its closing quote.  ``'a`` without that immediate quote is a
        # lifetime, and must not consume the rest of the line/module.
        return start + 3 if source[start + 2] == "'" else None

    cursor = start + 2
    while cursor < len(source) and source[cursor] not in {"\n", "\r"}:
        if source[cursor] == "\\":
            cursor += 2
        elif source[cursor] == "'":
            return cursor + 1
        else:
            cursor += 1
    return None


def lex(source: str) -> tuple[Token, ...]:
    """Lex code tokens while omitting comments and string/char literals."""

    starts = _line_starts(source)
    tokens: list[Token] = []
    index = 0
    length = len(source)

    while index < length:
        character = source[index]

        if character.isspace():
            index += 1
            continue

        if source.startswith("//", index):
            newline = source.find("\n", index + 2)
            index = length if newline < 0 else newline + 1
            continue

        if source.startswith("/*", index):
            index += 2
            depth = 1
            while index < length and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            continue

        raw_end = _raw_literal_end(source, index)
        if raw_end is not None:
            index = raw_end
            continue

        # Byte strings and ordinary strings.  A character literal is skipped
        # with the same escape handling.  A bare apostrophe introducing a
        # lifetime is emitted as punctuation so it cannot swallow the rest of
        # a Rust module while looking for its closing quote.
        literal_prefix = 0
        char_end = _char_literal_end(source, index) if character == "'" else None
        if character == "'" and char_end is None:
            line, column = _location(starts, index)
            tokens.append(Token(character, index, index + 1, line, column))
            index += 1
            continue
        if character == '"':
            literal_prefix = 0
        elif character in "bBcC" and index + 1 < length and source[index + 1] in {
            '"',
        }:
            literal_prefix = 1
        if char_end is not None:
            index = char_end
            continue
        if literal_prefix or character == '"':
            quote_index = index + literal_prefix
            quote = source[quote_index]
            cursor = quote_index + 1
            while cursor < length:
                if source[cursor] == "\\":
                    cursor += 2
                elif source[cursor] == quote:
                    cursor += 1
                    break
                else:
                    cursor += 1
            index = cursor
            continue

        # Rust raw identifiers are written ``r#name``.  Keep the source span
        # anchored at the ``r`` for useful diagnostics, but normalize the
        # token text to ``name`` so aliases and path matching cannot evade the
        # boundary with ``r#std``/``r#process`` spellings.  Raw strings have
        # already been consumed above, so this does not conflict with them.
        if (
            character == "r"
            and index + 2 < length
            and source[index + 1] == "#"
            and _is_identifier_start(source[index + 2])
        ):
            end = index + 3
            while end < length and _is_identifier_continue(source[end]):
                end += 1
            line, column = _location(starts, index)
            tokens.append(Token(source[index + 2 : end], index, end, line, column))
            index = end
            continue

        if _is_identifier_start(character):
            end = index + 1
            while end < length and _is_identifier_continue(source[end]):
                end += 1
            line, column = _location(starts, index)
            tokens.append(Token(source[index:end], index, end, line, column))
            index = end
            continue

        if source.startswith("::", index):
            line, column = _location(starts, index)
            tokens.append(Token("::", index, index + 2, line, column))
            index += 2
            continue

        line, column = _location(starts, index)
        tokens.append(Token(character, index, index + 1, line, column))
        index += 1

    return tuple(tokens)


class _UseParser:
    def __init__(self, tokens: Sequence[Token], start: int, end: int) -> None:
        self.tokens = tokens
        self.index = start
        self.end = end

    def _peek(self) -> str | None:
        if self.index >= self.end:
            return None
        return self.tokens[self.index].text

    def _take(self, expected: str | None = None) -> Token:
        if self.index >= self.end:
            raise ValueError("unexpected end of use declaration")
        token = self.tokens[self.index]
        if expected is not None and token.text != expected:
            raise ValueError(f"expected {expected!r}, found {token.text!r}")
        self.index += 1
        return token

    def _parse_tree(
        self,
        prefix: tuple[str, ...] = (),
        prefix_index: int | None = None,
    ) -> list[UseImport]:
        if self._peek() == "::":
            self._take("::")

        path_index = self.index if prefix_index is None else prefix_index
        segments: list[str] = []
        while True:
            next_text = self._peek()
            if next_text is None or not _is_identifier_start(next_text[0]):
                break
            segments.append(self._take().text)
            if self._peek() != "::":
                break
            self._take("::")
            if self._peek() in {"{", "*", None}:
                break

        if not segments and self._peek() not in {"{", "*"}:
            raise ValueError("use tree is missing a path")

        if segments == ["self"] and prefix:
            full_path = prefix
        else:
            full_path = prefix + tuple(segments)

        if self._peek() == "{":
            self._take("{")
            imports: list[UseImport] = []
            while self._peek() != "}":
                if self._peek() is None:
                    raise ValueError("unterminated use tree group")
                imports.extend(self._parse_tree(full_path, path_index))
                if self._peek() == ",":
                    self._take(",")
                elif self._peek() != "}":
                    raise ValueError("expected ',' or '}' in use tree group")
            self._take("}")
            return imports

        if self._peek() == "*":
            self._take("*")
            return [UseImport(full_path + ("*",), None, path_index)]

        alias: str | None = None
        if self._peek() == "as":
            self._take("as")
            alias_token = self._take()
            if not _is_identifier_start(alias_token.text[0]):
                raise ValueError("use alias must be an identifier")
            alias = alias_token.text
        return [UseImport(full_path, alias, path_index)]

    def parse(self) -> list[UseImport]:
        imports = self._parse_tree()
        if self.index != self.end:
            raise ValueError(
                f"unexpected token {self.tokens[self.index].text!r} in use declaration"
            )
        return imports


def _use_declarations(
    tokens: Sequence[Token],
) -> tuple[list[tuple[int, int]], list[UseImport], list[str]]:
    """Return use spans/imports and fail-closed parser diagnostics."""

    spans: list[tuple[int, int]] = []
    imports: list[UseImport] = []
    errors: list[str] = []
    for start, token in enumerate(tokens):
        if token.text != "use":
            continue
        depth = 0
        end = None
        for cursor in range(start + 1, len(tokens)):
            text = tokens[cursor].text
            if text in {"{", "(", "["}:
                depth += 1
            elif text in {"}", ")", "]"}:
                depth -= 1
            elif text == ";" and depth == 0:
                end = cursor
                break
        if end is None:
            errors.append(f"{token.line}:{token.column}: use declaration has no ';'")
            continue
        spans.append((start, end))
        try:
            imports.extend(_UseParser(tokens, start + 1, end).parse())
        except ValueError as error:
            errors.append(f"{token.line}:{token.column}: cannot parse use declaration: {error}")
    return spans, imports, errors


def _brace_pairs(tokens: Sequence[Token]) -> dict[int, int]:
    pairs: dict[int, int] = {}
    stack: list[int] = []
    for index, token in enumerate(tokens):
        if token.text == "{":
            stack.append(index)
        elif token.text == "}":
            if not stack:
                continue
            opening = stack.pop()
            pairs[opening] = index
    return pairs


_OPEN_DELIMITERS = frozenset(("(", "[", "{"))
_CLOSE_DELIMITERS = frozenset((")", "]", "}"))
_MATCHING_DELIMITER = {"(": ")", "[": "]", "{": "}"}


def _delimiter_pairs(tokens: Sequence[Token]) -> dict[int, int]:
    """Return matching pairs for all Rust grouping delimiters."""

    pairs: dict[int, int] = {}
    stack: list[tuple[str, int]] = []
    for index, token in enumerate(tokens):
        if token.text in _OPEN_DELIMITERS:
            stack.append((token.text, index))
        elif token.text in _CLOSE_DELIMITERS and stack:
            opening, opening_index = stack[-1]
            if _MATCHING_DELIMITER[opening] == token.text:
                stack.pop()
                pairs[opening_index] = index
    return pairs


def _macro_parameters(tokens: Sequence[Token], start: int, end: int) -> tuple[str, ...]:
    """Collect fragment names declared in a macro matcher."""

    parameters: list[str] = []
    seen: set[str] = set()
    for index in range(start, max(start, end - 2)):
        if (
            tokens[index].text != "$"
            or not _is_identifier_start(tokens[index + 1].text[0])
            or tokens[index + 2].text != ":"
        ):
            continue
        name = tokens[index + 1].text
        if name not in seen:
            seen.add(name)
            parameters.append(name)
    return tuple(parameters)


def _macro_arm_end(tokens: Sequence[Token], start: int, end: int) -> int:
    """Find the top-level separator or end of a macro transcriber."""

    depth = 0
    for index in range(start, end):
        text = tokens[index].text
        if text in _OPEN_DELIMITERS:
            depth += 1
        elif text in _CLOSE_DELIMITERS:
            depth = max(0, depth - 1)
        elif depth == 0 and text in {";", ","}:
            return index
    return end


def _macro_uses(
    tokens: Sequence[Token],
    start: int,
    end: int,
    parameters: Sequence[str],
) -> tuple[MacroUse, ...]:
    """Find process-construction paths in a macro transcriber.

    Only the two path shapes which can hide a direct std process construction
    are retained.  A generic ``$x::new`` is harmless until a call supplies the
    known ``std::process::Command`` path; resolving that call is what avoids
    flagging ordinary path-building macros.
    """

    parameter_set = set(parameters)
    uses: list[MacroUse] = []
    seen: set[tuple[int, str]] = set()
    for index in range(start, end):
        if (
            tokens[index].text != "$"
            or index + 1 >= end
            or tokens[index + 1].text not in parameter_set
        ):
            continue
        parameter = tokens[index + 1].text
        # The metavariable may be inserted into either side of the canonical
        # path, e.g. ``std::$p::Command::new`` or
        # ``std::process::$c::new``.  These are distinct from a generic
        # ``$x::new`` until the invocation supplies a known std path.
        if (
            index >= 2
            and tuple(token.text for token in tokens[index - 2 : index])
            == ("std", "::")
            and tuple(token.text for token in tokens[index + 2 : index + 6])
            == ("::", "Command", "::", "new")
        ):
            key = (index, "macro-process-command-new")
            if key not in seen:
                seen.add(key)
                uses.append(MacroUse(parameter, "macro-process-command-new", index))
            continue
        if (
            index >= 4
            and tuple(token.text for token in tokens[index - 4 : index])
            == ("std", "::", "process", "::")
            and tuple(token.text for token in tokens[index + 2 : index + 4])
            == ("::", "new")
        ):
            key = (index, "macro-command-new")
            if key not in seen:
                seen.add(key)
                uses.append(MacroUse(parameter, "macro-command-new", index))
            continue
        qualified_end = index + 8
        if qualified_end <= end and tuple(
            token.text for token in tokens[index + 2 : qualified_end]
        ) == ("::", "process", "::", "Command", "::", "new"):
            key = (index, "macro-qualified-command-new")
            if key not in seen:
                seen.add(key)
                uses.append(
                    MacroUse(parameter, "macro-qualified-command-new", index)
                )
            continue
        constructor_end = index + 4
        if constructor_end <= end and tuple(
            token.text for token in tokens[index + 2 : constructor_end]
        ) == ("::", "new"):
            key = (index, "macro-command-new")
            if key not in seen:
                seen.add(key)
                uses.append(MacroUse(parameter, "macro-command-new", index))
    return tuple(uses)


def _macro_definitions(tokens: Sequence[Token]) -> tuple[MacroDefinition, ...]:
    """Parse enough ``macro_rules!`` structure to inspect local expanders."""

    pairs = _delimiter_pairs(tokens)
    definitions: list[MacroDefinition] = []
    index = 0
    while index + 3 < len(tokens):
        if tokens[index].text != "macro_rules" or tokens[index + 1].text != "!":
            index += 1
            continue
        name = tokens[index + 2].text
        opening = index + 3
        if tokens[opening].text != "{" or opening not in pairs:
            index += 1
            continue
        closing = pairs[opening]
        arms: list[MacroArm] = []
        arm_start = opening + 1
        cursor = arm_start
        depth = 0
        while cursor < closing:
            text = tokens[cursor].text
            if text in _OPEN_DELIMITERS:
                depth += 1
            elif text in _CLOSE_DELIMITERS:
                depth = max(0, depth - 1)
            elif (
                text == "="
                and cursor + 1 < closing
                and tokens[cursor + 1].text == ">"
                and depth == 0
            ):
                expansion_start = cursor + 2
                expansion_end = _macro_arm_end(tokens, expansion_start, closing)
                parameters = _macro_parameters(tokens, arm_start, cursor)
                uses = _macro_uses(tokens, expansion_start, expansion_end, parameters)
                arms.append(MacroArm(parameters, uses))
                if expansion_end >= closing:
                    break
                arm_start = expansion_end + 1
                cursor = arm_start
                depth = 0
                continue
            cursor += 1
        definitions.append(MacroDefinition(name, index, closing, tuple(arms)))
        index = closing + 1
    return tuple(definitions)


def _macro_argument_segments(
    tokens: Sequence[Token], start: int, end: int
) -> tuple[tuple[Token, ...], ...]:
    """Split one macro invocation's arguments at top-level commas."""

    if start >= end:
        return ()
    segments: list[tuple[Token, ...]] = []
    segment_start = start
    depth = 0
    for index in range(start, end):
        text = tokens[index].text
        if text in _OPEN_DELIMITERS:
            depth += 1
        elif text in _CLOSE_DELIMITERS:
            depth = max(0, depth - 1)
        elif text == "," and depth == 0:
            segments.append(tuple(tokens[segment_start:index]))
            segment_start = index + 1
    segments.append(tuple(tokens[segment_start:end]))
    return tuple(segments)


_RELATIVE_PATH_ROOTS = frozenset(("self", "crate", "super"))


def _path_from_tokens(segment: Sequence[Token]) -> tuple[str, ...] | None:
    """Return a simple ``foo::bar`` path from a token sequence."""

    if not segment:
        return None
    texts = [token.text for token in segment]
    if texts and texts[0] == "::":
        texts = texts[1:]
    if not texts or len(texts) % 2 == 0:
        return None
    if any(
        (index % 2 == 0 and not _is_identifier_start(text[0]))
        or (index % 2 == 1 and text != "::")
        for index, text in enumerate(texts)
    ):
        return None
    return tuple(texts[::2])


def _path_from_macro_argument(segment: Sequence[Token]) -> tuple[str, ...] | None:
    """Return a simple ``foo::bar`` path from a macro argument."""

    return _path_from_tokens(segment)


def _normalize_relative_path(path: tuple[str, ...]) -> tuple[str, ...]:
    """Strip Rust module-relative roots for same-file alias resolution."""

    start = 0
    while start < len(path) and path[start] in _RELATIVE_PATH_ROOTS:
        start += 1
    return path[start:]


def _path_at(tokens: Sequence[Token], start: int) -> tuple[tuple[str, ...], int] | None:
    """Read one contiguous path and return ``(segments, exclusive_end)``."""

    if start >= len(tokens) or not _is_identifier_start(tokens[start].text[0]):
        return None
    segments = [tokens[start].text]
    cursor = start + 1
    while (
        cursor + 1 < len(tokens)
        and tokens[cursor].text == "::"
        and _is_identifier_start(tokens[cursor + 1].text[0])
    ):
        segments.append(tokens[cursor + 1].text)
        cursor += 2
    return tuple(segments), cursor


def _ufcs_path_at(tokens: Sequence[Token], start: int) -> tuple[tuple[str, ...], int] | None:
    """Read the simple ``<Type as Trait>::new`` UFCS spelling."""

    if start >= len(tokens) or tokens[start].text != "<":
        return None
    depth = 1
    cursor = start + 1
    as_index: int | None = None
    closing: int | None = None
    while cursor < len(tokens):
        text = tokens[cursor].text
        if text == "<":
            depth += 1
        elif text == ">":
            depth -= 1
            if depth == 0:
                closing = cursor
                break
        elif text == "as" and depth == 1 and as_index is None:
            as_index = cursor
        cursor += 1
    if closing is None or closing + 2 >= len(tokens):
        return None
    if tokens[closing + 1].text != "::" or tokens[closing + 2].text != "new":
        return None
    path_end = as_index if as_index is not None else closing
    path = _path_from_tokens(tokens[start + 1 : path_end])
    if path is None:
        return None
    return path, closing + 3


def _skip_source_trivia(source: str, start: int) -> int:
    """Skip whitespace and comments while fingerprinting a call argument."""

    cursor = start
    while cursor < len(source):
        if source[cursor].isspace():
            cursor += 1
            continue
        if source.startswith("//", cursor):
            newline = source.find("\n", cursor + 2)
            cursor = len(source) if newline < 0 else newline + 1
            continue
        if source.startswith("/*", cursor):
            cursor += 2
            depth = 1
            while cursor < len(source) and depth:
                if source.startswith("/*", cursor):
                    depth += 1
                    cursor += 2
                elif source.startswith("*/", cursor):
                    depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            continue
        break
    return cursor


def _quoted_source_end(source: str, start: int) -> int | None:
    """Return the end of an ordinary or prefixed quoted literal."""

    if start >= len(source):
        return None
    quote = start
    if source[start] in "bBcC" and start + 1 < len(source):
        quote = start + 1
    if source[quote] != '"':
        return None
    cursor = quote + 1
    while cursor < len(source):
        if source[cursor] == "\\":
            cursor += 2
        elif source[cursor] == '"':
            return cursor + 1
        else:
            cursor += 1
    return None


def _first_constructor_argument(source: str, new_end: int) -> str | None:
    """Return a stable first-argument spelling for a ``new(...)`` call."""

    opening = source.find("(", new_end)
    if opening < 0:
        return None
    cursor = _skip_source_trivia(source, opening + 1)
    if cursor >= len(source):
        return None
    if source[cursor] == ")":
        return "<empty>"

    argument_start = cursor
    raw_end = _raw_literal_end(source, cursor)
    quoted_end = raw_end if raw_end is not None else _quoted_source_end(source, cursor)
    if quoted_end is not None:
        return source[argument_start:quoted_end]

    depth = 0
    while cursor < len(source):
        raw_end = _raw_literal_end(source, cursor)
        quoted_end = raw_end if raw_end is not None else _quoted_source_end(source, cursor)
        if quoted_end is not None:
            cursor = quoted_end
            continue
        character = source[cursor]
        if character in "([{":
            depth += 1
        elif character in ")]}":
            if depth == 0:
                break
            depth -= 1
        elif character == "," and depth == 0:
            break
        cursor += 1
    normalized = " ".join(source[argument_start:cursor].split())
    return normalized[:256] if normalized else "<empty>"


def _constructor_fingerprint(
    source: str, tokens: Sequence[Token], start: int, kind: str
) -> str | None:
    """Build a path/argument fingerprint for an identifiable constructor."""

    if "command-new" not in kind or kind.startswith("macro-"):
        return None
    new_index = next(
        (
            index
            for index in range(start, min(len(tokens), start + 128))
            if tokens[index].text == "new"
            and index > start
            and tokens[index - 1].text == "::"
        ),
        None,
    )
    if new_index is None:
        return None
    argument = _first_constructor_argument(source, tokens[new_index].end)
    if argument is None:
        return None
    path = "".join(token.text for token in tokens[start : new_index + 1])
    return f"{kind}|{path}|{argument}"


def _cfg_test_ranges(tokens: Sequence[Token]) -> tuple[tuple[int, int], ...]:
    """Find item ranges governed by an exact ``#[cfg(test)]`` attribute."""

    pairs = _brace_pairs(tokens)
    ranges: list[tuple[int, int]] = []
    for index in range(len(tokens) - 6):
        if [tokens[index + offset].text for offset in range(7)] != [
            "#",
            "[",
            "cfg",
            "(",
            "test",
            ")",
            "]",
        ]:
            continue
        item_start = index + 7
        if item_start >= len(tokens):
            ranges.append((item_start, len(tokens) - 1))
            continue

        opening = next(
            (
                cursor
                for cursor in range(item_start, len(tokens))
                if tokens[cursor].text in {"{", ";"}
            ),
            None,
        )
        if opening is None:
            ranges.append((item_start, len(tokens) - 1))
        elif tokens[opening].text == ";":
            ranges.append((item_start, opening))
        else:
            ranges.append((item_start, pairs.get(opening, len(tokens) - 1)))
    return tuple(ranges)


def _in_ranges(index: int, ranges: Iterable[tuple[int, int]]) -> bool:
    return any(start <= index <= end for start, end in ranges)


def _relative_path(root: Path, path: Path) -> str:
    return path.relative_to(root).as_posix()


def _validate_allowlist(allowlist: Mapping[str, AllowRule]) -> None:
    keys = set(allowlist)
    if keys != ALLOWED_EXCEPTION_PATHS:
        extra = sorted(keys - ALLOWED_EXCEPTION_PATHS)
        missing = sorted(ALLOWED_EXCEPTION_PATHS - keys)
        details: list[str] = []
        if extra:
            details.append(f"extra={extra}")
        if missing:
            details.append(f"missing={missing}")
        raise BoundaryError(
            "subprocess exception allowlist is stale or too broad ("
            + ", ".join(details)
            + ")"
        )

    for relative, rule in allowlist.items():
        path = Path(relative)
        if path.is_absolute() or ".." in path.parts or path.suffix != ".rs":
            raise BoundaryError(
                f"subprocess exception allowlist has an unsafe path: {relative!r}"
            )
        if (
            not isinstance(rule, AllowRule)
            or not rule.expected_kinds
            or not rule.expected_fingerprints
        ):
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} is not exact"
            )
        expected = rule.expected
        if any(not isinstance(count, int) or count <= 0 for count in expected.values()):
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} has invalid counts"
            )
        unknown_kinds = set(expected) - ALLOWED_EXCEPTION_KINDS
        if unknown_kinds:
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} has unknown kinds: "
                f"{sorted(unknown_kinds)}"
            )
        fingerprints = rule.fingerprints
        if len(fingerprints) != len(rule.expected_fingerprints):
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} has duplicate fingerprints"
            )
        if any(
            not isinstance(fingerprint, str)
            or not fingerprint
            or not isinstance(count, int)
            or count <= 0
            for fingerprint, count in fingerprints.items()
        ):
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} has invalid fingerprints"
            )
        fingerprint_kinds = {
            fingerprint.split("|", 1)[0] for fingerprint in fingerprints
        }
        unknown_fingerprint_kinds = fingerprint_kinds - set(expected)
        if unknown_fingerprint_kinds:
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} has fingerprints for "
                f"unlisted kinds: {sorted(unknown_fingerprint_kinds)}"
            )
        expected_constructor_count = sum(
            count for kind, count in expected.items() if "command-new" in kind
        )
        if sum(fingerprints.values()) != expected_constructor_count:
            raise BoundaryError(
                f"subprocess exception allowlist entry {relative!r} must fingerprint "
                f"all constructor occurrences"
            )
        if relative == "build.rs" and rule.test_only:
            raise BoundaryError("build.rs exception must not be marked test-only")
        if relative != "build.rs" and not rule.test_only:
            raise BoundaryError(
                f"subprocess exception {relative!r} must be restricted to #[cfg(test)]"
            )


def _source_scan(relative_path: str, source: str) -> SourceScan:
    tokens = lex(source)
    use_spans, imports, structural_errors = _use_declarations(tokens)
    cfg_test_ranges = _cfg_test_ranges(tokens)
    findings: list[Finding] = []
    seen: set[tuple[int, str, int | None]] = set()

    # Alias targets are kept as canonical paths.  Resolving imports in source
    # order also closes chained forms such as ``use std as s; use
    # s::process as p; p::Command::new(...)``.
    aliases: dict[str, tuple[str, ...]] = {"std": ("std",)}
    root_aliases: set[str] = set()
    process_aliases: set[str] = set()
    command_aliases: set[str] = set()

    # Rust 2015-style root aliases remain valid in a 2021 crate and otherwise
    # bypass the `use std as ...` tracking below.
    for index in range(len(tokens) - 4):
        declaration = tuple(token.text for token in tokens[index : index + 5])
        if declaration[:4] != ("extern", "crate", "std", "as"):
            continue
        alias = declaration[4]
        if _is_identifier_start(alias[0]):
            aliases[alias] = ("std",)
            root_aliases.add(alias)

    def add(index: int, kind: str, identity: int | None = None) -> None:
        # Normal lexical matches are deduplicated because import aliases can
        # identify the same token more than once.  Macro invocations pass the
        # transcriber token as an identity so two generated constructors in
        # one invocation still count as two boundary uses.
        key = (index, kind, identity)
        if key in seen or index >= len(tokens):
            return
        seen.add(key)
        token = tokens[index]
        findings.append(
            Finding(
                relative_path=relative_path,
                line=token.line,
                column=token.column,
                kind=kind,
                token_index=index,
                in_cfg_test=_in_ranges(index, cfg_test_ranges),
                fingerprint=_constructor_fingerprint(source, tokens, index, kind),
            )
        )

    def is_in_use(index: int) -> bool:
        return any(start <= index <= end for start, end in use_spans)

    # Parse imports first so aliases can be recognized in the rest of the
    # file.  A process-module import is rejected even if it never reaches a
    # Command call: it is an easy alias-based bypass of this boundary.
    def resolve_alias_path(path: tuple[str, ...]) -> tuple[str, ...]:
        path = _normalize_relative_path(path)
        if not path:
            return path
        target = aliases.get(path[0])
        if target is None:
            return path
        return _normalize_relative_path(target + path[1:])

    # Rust import resolution is independent of declaration order. Iterate to
    # a fixed point so a reverse-ordered chain such as `use s::process as p;
    # use std as s;` cannot evade the boundary.
    for _ in range(len(imports) + 1):
        changed = False
        for imported in imports:
            resolved = resolve_alias_path(imported.path)
            alias: str | None = imported.alias
            if resolved == ("std",):
                if alias:
                    changed |= aliases.get(alias) != resolved
                    aliases[alias] = resolved
                    root_aliases.add(alias)
                continue
            if resolved == ("std", "process"):
                alias = alias or "process"
                changed |= aliases.get(alias) != resolved
                aliases[alias] = resolved
                process_aliases.add(alias)
                add(imported.token_index, "process-module-import")
                continue
            if resolved == ("std", "process", "*"):
                add(imported.token_index, "process-module-import")
                continue
            if resolved == ("std", "process", "Command"):
                alias = alias or "Command"
                changed |= aliases.get(alias) != resolved
                aliases[alias] = resolved
                command_aliases.add(alias)
                add(imported.token_index, "qualified-command-import")
                continue

            # Keep explicit non-process aliases available for chained imports.
            # A later pass will overwrite an unresolved target once its own
            # root alias becomes known.
            if alias:
                changed |= aliases.get(alias) != resolved
                aliases[alias] = resolved
        if not changed:
            break

    # Parse contiguous paths once, then resolve their first segment through the
    # same alias table used for imports.  This keeps ordinary path matching and
    # ``self::``/``crate::``/``super::`` prefixes on one canonical path.
    def direct_kind(path: tuple[str, ...], constructor: bool) -> str | None:
        normalized = _normalize_relative_path(path)
        if not normalized:
            return None
        first = normalized[0]
        if first == "std":
            return "qualified-command-new" if constructor else "qualified-command"
        if first in root_aliases:
            return "root-command-new" if constructor else "root-command"
        if first in process_aliases:
            return "module-command-new" if constructor else "module-command"
        if first == "Command":
            return "command-new" if constructor else None
        if first in command_aliases:
            return "command-new-alias" if constructor else None
        return None

    # A path beginning in the middle of ``std::process::Command::new`` would
    # otherwise be seen as a second alias candidate.  Restrict starts to the
    # first segment; relative roots are retained and normalized above.
    for index, token in enumerate(tokens):
        if index > 0 and tokens[index - 1].text in {"$", "::", "<", "as"}:
            continue
        parsed = _path_at(tokens, index)
        if parsed is None:
            continue
        path, _ = parsed
        if is_in_use(index):
            continue
        constructor = len(path) >= 2 and path[-1] == "new"
        command_path = path[:-1] if constructor else path
        if resolve_alias_path(command_path) != ("std", "process", "Command"):
            continue
        kind = direct_kind(command_path, constructor)
        if kind is not None:
            add(index, kind)

    # Cover the common, non-generic UFCS forms without pretending to parse all
    # Rust type grammar: ``<Command>::new()`` and
    # ``<std::process::Command as Trait>::new()``.
    for index, token in enumerate(tokens):
        if token.text != "<" or is_in_use(index):
            continue
        parsed = _ufcs_path_at(tokens, index)
        if parsed is None:
            continue
        path, _ = parsed
        if resolve_alias_path(path) == ("std", "process", "Command"):
            add(index, "ufcs-command-new")

    # A lexical scan cannot see through a macro metavariable.  Resolve only
    # local macro_rules! calls whose argument is itself a simple, known std
    # path.  This catches e.g. `$p::process::Command::new` with `spawn!(std)`
    # and `$c::new` with `make!(Command)` while leaving generic path-builder
    # macros alone when they are called with an unrelated type.
    macro_definitions = _macro_definitions(tokens)
    macro_by_name: dict[str, list[MacroDefinition]] = {}
    macro_ranges: list[tuple[int, int]] = []
    for definition in macro_definitions:
        macro_by_name.setdefault(definition.name, []).append(definition)
        macro_ranges.append((definition.start, definition.end))
    delimiter_pairs = _delimiter_pairs(tokens)
    for index in range(len(tokens) - 2):
        name = tokens[index].text
        if (
            name not in macro_by_name
            or tokens[index + 1].text != "!"
            or tokens[index + 2].text not in _OPEN_DELIMITERS
            or _in_ranges(index, macro_ranges)
        ):
            continue
        closing = delimiter_pairs.get(index + 2)
        if closing is None:
            continue
        arguments = _macro_argument_segments(tokens, index + 3, closing)
        for definition in macro_by_name[name]:
            for arm in definition.arms:
                if len(arm.parameters) != len(arguments):
                    continue
                bindings = {
                    parameter: resolve_alias_path(path)
                    for parameter, argument in zip(arm.parameters, arguments)
                    if (path := _path_from_macro_argument(argument)) is not None
                }
                for use in arm.uses:
                    resolved = bindings.get(use.parameter)
                    expected = (
                        ("std",)
                        if use.kind == "macro-qualified-command-new"
                        else ("std", "process")
                        if use.kind == "macro-process-command-new"
                        else ("std", "process", "Command")
                    )
                    if resolved == expected:
                        add(index, use.kind, use.token_index)

    findings.sort(key=lambda finding: (finding.token_index, finding.kind))
    return SourceScan(relative_path, tuple(findings), tuple(structural_errors))


def _all_rust_sources(root: Path) -> list[tuple[str, Path]]:
    sources: list[tuple[str, Path]] = []
    build = root / "build.rs"
    if build.is_file():
        sources.append(("build.rs", build))
    src = root / "src"
    if src.is_dir():
        sources.extend(
            (_relative_path(root, path), path)
            for path in sorted(src.rglob("*.rs"))
            if path.is_file()
        )
    return sources


def check_repository(
    root: Path = REPOSITORY_ROOT,
    *,
    allowlist: Mapping[str, AllowRule] | None = None,
) -> tuple[SourceScan, ...]:
    """Check *root* and return deterministic scans, or raise BoundaryError."""

    root = Path(root).resolve()
    if not root.is_dir():
        raise BoundaryError(f"repository root is not a directory: {root}")
    if allowlist is None:
        allowlist = ALLOWLIST
    _validate_allowlist(allowlist)

    broker = root / BROKER_RELATIVE_PATH
    if not broker.is_file():
        raise BoundaryError(
            f"required subprocess broker is missing: {BROKER_RELATIVE_PATH}"
        )

    source_paths = dict(_all_rust_sources(root))
    stale = sorted(path for path in allowlist if path not in source_paths)
    if stale:
        raise BoundaryError(
            "subprocess exception allowlist is stale; missing source file(s): "
            + ", ".join(stale)
        )

    scans: list[SourceScan] = []
    violations: list[str] = []
    for relative, path in sorted(source_paths.items()):
        try:
            source = path.read_text(encoding="utf-8")
        except (OSError, UnicodeError) as error:
            violations.append(f"{relative}: cannot read Rust source: {error}")
            continue
        scan = _source_scan(relative, source)
        scans.append(scan)
        if relative == BROKER_RELATIVE_PATH:
            continue
        if scan.structural_errors:
            violations.extend(f"{relative}: {error}" for error in scan.structural_errors)

        rule = allowlist.get(relative)
        if rule is None:
            violations.extend(finding.describe() for finding in scan.findings)
            continue

        if rule.test_only:
            violations.extend(
                f"{finding.describe()} is outside an exact #[cfg(test)] exception"
                for finding in scan.findings
                if not finding.in_cfg_test
            )

        expected = rule.expected
        actual = Counter(finding.kind for finding in scan.findings)
        for kind, count in sorted(actual.items()):
            if kind not in expected:
                violations.append(
                    f"{relative}: unexpected allowlisted subprocess use kind {kind!r} "
                    f"(found {count})"
                )
            elif count > expected[kind]:
                violations.append(
                    f"{relative}: subprocess exception allowlist permits only "
                    f"{expected[kind]} {kind} occurrence(s), found {count}"
                )
        for kind, count in sorted(expected.items()):
            if actual.get(kind, 0) < count:
                violations.append(
                    f"{relative}: stale subprocess exception allowlist expects "
                    f"{count} {kind} occurrence(s), found {actual.get(kind, 0)}"
                )
        expected_fingerprints = rule.fingerprints
        actual_fingerprints = Counter(
            finding.fingerprint
            for finding in scan.findings
            if finding.fingerprint is not None
        )
        if actual_fingerprints != expected_fingerprints:
            violations.append(
                f"{relative}: subprocess exception allowlist fingerprint mismatch "
                f"(expected {dict(sorted(expected_fingerprints.items()))}, "
                f"found {dict(sorted(actual_fingerprints.items()))})"
            )

    if violations:
        raise BoundaryError("\n".join(violations))
    return tuple(scans)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=REPOSITORY_ROOT,
        help="repository root to scan (default: %(default)s)",
    )
    args = parser.parse_args(argv)
    try:
        scans = check_repository(args.root)
    except BoundaryError as error:
        print(f"subprocess boundary error:\n{error}", file=sys.stderr)
        return 1
    broker_count = sum(
        len(scan.findings)
        for scan in scans
        if scan.relative_path == BROKER_RELATIVE_PATH
    )
    legacy_count = sum(
        len(scan.findings)
        for scan in scans
        if scan.relative_path in ALLOWED_EXCEPTION_PATHS
    )
    print(
        "subprocess boundary check passed: "
        f"{len(scans)} Rust source file(s); "
        f"broker-owned use(s)={broker_count}; "
        f"legacy allowlisted use(s)={legacy_count}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
