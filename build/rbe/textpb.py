#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Generic utilities for working with text protobufs (without schema).
"""

import argparse
import collections
import dataclasses
import enum
import re
import sys
from collections.abc import Iterable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any, NoReturn


class TokenType(enum.Enum):
    FIELD_NAME = 0  # includes trailing ':' (optional for bracketed extensions)
    START_BLOCK = 1  # '<' or '{'
    END_BLOCK = 2  # '>' or '}'
    STRING_VALUE = 3  # quoted text, e.g. "string"
    OTHER_VALUE = 4  # non-string value
    SPACE = 5  # [ \t]*
    NEWLINE = 6  # [\n\r]


@dataclasses.dataclass
class Token(object):
    text: str
    type: TokenType


TextProtoDict = dict[str, Any]
TextProtoMapping = Mapping[str, Any]


_IDENT_PATTERN = r"[a-zA-Z_][a-zA-Z0-9_]*"
_EXT_IDENT_PATTERN = r"\[[a-zA-Z_](?:[a-zA-Z0-9_.-]++|/(?!/))*+\]"
_FIELD_KEY_PATTERN = f"(?:{_IDENT_PATTERN}|{_EXT_IDENT_PATTERN})"
# Possessive quantifiers (++ and *+) prevent backtracking inside string bodies,
# ensuring O(N) fail-fast behavior on unterminated strings.
_STRING_PATTERN = r'"(?:[^\\"\r\n]++|\\[^\r\n])*+"'
# Value token excluding delimiters, brackets, quotes, and comments.
# Note: Bracketed list syntax for repeated scalar values (e.g. `field: [a, b]`)
# is not supported; repeated fields in RBE logs and LOAS textprotos repeat the
# `field: value` entry instead.
_VALUE_PATTERN = r'[^ \t\r\n,;{}<>#/\[\]"]+'
_MAX_ERROR_SNIPPET_LEN = 100

_IDENT_RE = re.compile(_IDENT_PATTERN)
_FIELD_NAME_RE = re.compile(f"{_FIELD_KEY_PATTERN}:?")

# Combined token specification for _lex_line().
# Matching a single alternation regex with re.finditer() advances the scan
# offset inside the C regex engine and dispatches via Match.lastgroup, avoiding
# O(N^2) per-token substring slicing (line = line[len(tok):]) and sequential
# per-token regex probes in Python.
#
# Ordering note:
#   `FIELD_NAME` must precede `OTHER_VALUE` so `ident:` is lexed as a field name
#   rather than a value token containing `:`. Because `_IDENT_PATTERN` in
#   `FIELD_NAME` requires a trailing `:` (unlike `_EXT_IDENT_PATTERN`), unquoted
#   identifiers without a colon (e.g. booleans `true`/`false`, enum names, or
#   colon-less block field names) fall through to `OTHER_VALUE`.
_TOKEN_SPEC = (
    ("COMMENT", r"#|//"),
    ("SEPARATOR", r"[,;]+"),
    ("START_BLOCK", r"[<{]"),
    ("END_BLOCK", r"[>}]"),
    ("FIELD_NAME", f"(?:{_IDENT_PATTERN}:|{_EXT_IDENT_PATTERN}:?)"),
    ("STRING_VALUE", _STRING_PATTERN),
    ("OTHER_VALUE", _VALUE_PATTERN),
    ("SPACE", r"[ \t]+"),
    ("NEWLINE", r"\r?\n"),
    ("MISMATCH", r"."),
)
_LEX_TOKEN_RE = re.compile(
    "|".join(f"(?P<{name}>{pattern})" for name, pattern in _TOKEN_SPEC),
    re.DOTALL,
)
_TOKEN_TYPE_BY_GROUP = {
    "START_BLOCK": TokenType.START_BLOCK,
    "END_BLOCK": TokenType.END_BLOCK,
    "FIELD_NAME": TokenType.FIELD_NAME,
    "STRING_VALUE": TokenType.STRING_VALUE,
    "OTHER_VALUE": TokenType.OTHER_VALUE,
    "SPACE": TokenType.SPACE,
    "NEWLINE": TokenType.NEWLINE,
}


class ParseError(ValueError):
    def __init__(self, msg: str):
        super().__init__(msg)


def _lex_line(line: str) -> Iterable[Token]:
    for m in _LEX_TOKEN_RE.finditer(line):
        kind = m.lastgroup
        if kind == "COMMENT":
            # Comment extends to end of line
            break
        if kind == "SEPARATOR":
            # Skip optional field separators
            continue
        if kind == "MISMATCH" or kind is None:
            snippet = line[m.start() : m.start() + _MAX_ERROR_SNIPPET_LEN]
            raise ParseError(f'[textpb.lex] Unrecognized text: "{snippet}"')
        yield Token(text=m.group(0), type=_TOKEN_TYPE_BY_GROUP[kind])


def yield_verbose(items: Iterable[Any]) -> Iterable[Any]:
    for item in items:
        print(f"{item}")
        yield item


def _lex(lines: Iterable[str]) -> Iterable[Token]:
    """Divides proto text into tokens."""
    for line in lines:
        yield from _lex_line(line)


def _auto_dict(values: Sequence[Any]) -> Any:
    """Convert sequences of key-value pairs to dictionaries."""
    if len(values) == 0:
        return values

    if all(
        isinstance(v, Mapping) and v.keys() == {"key", "value"} for v in values
    ):
        # assume keys are unique quoted strings
        # 'key' and 'value' should not be repeated fields
        return {v["key"][0].text.strip('"'): v["value"][0] for v in values}

    return values


def _parse_block(tokens: Iterator[Token], top: bool) -> TextProtoDict:
    """Parse text proto tokens into a structure.

    Args:
      tokens: lexical tokens, without any spaces/newlines.

    Returns:
      dictionary representation of text proto.
    """
    # Without a schema, we cannot deduce whether a field is scalar or
    # repeated, so treat them as repeated (maybe singleton).
    result = collections.defaultdict(list)

    while True:
        try:
            field = next(tokens)
        except StopIteration:
            if top:
                break
            else:
                raise ParseError(
                    "Unexpected EOF, missing '>' or '}' end-of-block"
                )

        if field.type == TokenType.END_BLOCK:
            if top:
                raise ParseError(
                    "Unexpected end-of-block at top-level before EOF."
                )
            break

        if field.type == TokenType.FIELD_NAME:
            has_colon = field.text.endswith(":")
            key = field.text[:-1] if has_colon else field.text
        elif (
            field.type == TokenType.OTHER_VALUE
            and _IDENT_RE.fullmatch(field.text) is not None
        ):
            has_colon = False
            key = field.text
        else:
            raise ParseError(f"Expected a field name, but got {field}.")

        try:
            value_or_block = next(tokens)
        except StopIteration:
            raise ParseError(
                "Unexpected EOF, expecting a value or start-of-block."
            )

        value: TextProtoDict | Token
        if value_or_block.type == TokenType.START_BLOCK:
            value = _parse_block(tokens, top=False)
        elif has_colon and value_or_block.type in {
            TokenType.STRING_VALUE,
            TokenType.OTHER_VALUE,
        }:
            value = value_or_block  # a Token
        else:
            raise ParseError(f"Unexpected token: {value_or_block}")

        result[key].append(value)

    # End of block, post-process key-value pairs into dictionaries.
    return {k: _auto_dict(v) for k, v in result.items()}


def _parse_tokens(tokens: Iterator[Token]) -> TextProtoDict:
    return _parse_block(tokens, top=True)


def parse(lines: Iterable[str]) -> TextProtoDict:
    """Parse a text protobuf into a recursive dictionary.

    Args:
      lines: lines of text proto (spaces and line breaks are ignored)

    Returns:
      Structured representation of the proto.
      Fields are treated as either repeated (even if the original
      schema was scalar) or as key-value dictionaries.
    """
    # ignore spaces
    return _parse_tokens(
        token
        for token in _lex(lines)
        if token.type not in {TokenType.SPACE, TokenType.NEWLINE}
    )


_COMMENT_PATTERN = r"(?:#|//)[^\r\n]*+"
_WS_OR_COMMENT_PATTERN = rf"(?:[ \t\r\n,;]++|{_COMMENT_PATTERN})*+"
_WHITESPACE_OR_COMMENT_RE = re.compile(_WS_OR_COMMENT_PATTERN)

# Non-structural chunks skipped in a single C regex pass by _find_block_end():
#   1. Runs of ordinary characters (excluding delimiters, quotes, and comments)
#   2. Complete quoted string literals (ignoring any '{', '}', '<', '>', '#', '//' inside)
#   3. Line comments ('#' or '//', ignoring any quotes or delimiters until end-of-line)
#   4. Standalone '/' characters (e.g. inside '[type.googleapis.com/pkg.Message]')
#
# Performance note:
#   Skipping complete string literals and comments inside C avoids returning to
#   Python on every '"', '#', and '//' (eliminating thousands of Python<->C
#   transitions and Python backslash-counting loops when skipping large
#   submessages like `command` with 1,500+ quoted arguments).
#   The chunk alternatives begin with mutually exclusive patterns
#   ([^"{}<>#/], '"', '#' or '//', '/(?!/)'), and the possessive quantifiers
#   (++ and *+) lock in each matched chunk so the engine never backtracks across
#   already-scanned strings if a subsequent string is unterminated or a block is
#   unclosed at EOF.
_BLOCK_SKIP_CHUNKS = (
    r'[^"{}<>#/]++',
    _STRING_PATTERN,
    _COMMENT_PATTERN,
    r"/(?!/)",
)
_BLOCK_SKIP_PATTERN = f"(?:{'|'.join(_BLOCK_SKIP_CHUNKS)})*+"
_NEXT_DELIM_RE = re.compile(rf'{_BLOCK_SKIP_PATTERN}([{{}}<>"])')
_CLOSE_DELIM_BY_OPEN = {"{": "}", "<": ">"}

# RHS alternatives after '<key>:' in _index_fields().
_COLON_RHS_SPEC = (
    ("block", r"[{<]"),
    ("str", _STRING_PATTERN),
    ("val", _VALUE_PATTERN),
)
_COLON_RHS_PATTERN = "|".join(
    f"(?P<{name}>{pattern})" for name, pattern in _COLON_RHS_SPEC
)

# Fused single-step matcher for the hot loop of _index_fields().
#
# Performance note:
#   Fuses 4 separate per-field regex matches (leading whitespace/comments, field
#   identifier, post-colon whitespace/comments, and scalar value or opening
#   block delimiter) into a single C regex invocation per field:
#     - end_block:         closing '}' or '>' of the current block
#     - block / str / val: '<key> : <rhs>' (colon present)
#     - colonless_block:   '<key> { ... }' (colon omitted before submessage)
#   Capturing `(?P<key>...)` prior to `:` extracts the field name directly in C
#   without allocating an intermediate `key:` string slice. Dispatches via
#   `Match.lastgroup` on the happy path and delegates syntax errors (`m is None`
#   before EOF) to the cold `_raise_index_parse_error()` helper.
_FIELD_STEP_BRANCHES = (
    r"(?P<end_block>[>}])",
    (
        f"(?P<key>{_FIELD_KEY_PATTERN})"
        r"(?:"
        f":{_WS_OR_COMMENT_PATTERN}(?:{_COLON_RHS_PATTERN})"
        f"|{_WS_OR_COMMENT_PATTERN}(?P<colonless_block>[{{<])"
        r")"
    ),
)
_FIELD_STEP_RE = re.compile(
    f"{_WS_OR_COMMENT_PATTERN}(?:{'|'.join(_FIELD_STEP_BRANCHES)})"
)


@dataclasses.dataclass(slots=True)
class _FieldEntry:
    """Descriptor for an indexed, unparsed field in a text protobuf.

    Analogous to a C++ `std::string_view`, submessage boundaries are recorded as
    integer offset spans `[body_start, body_end]` within the original buffer
    rather than slicing or allocating intermediate substrings.

    Attributes:
      is_message: True if this field is a submessage ({ ... } or < ... >).
      body_start: Start index of the inner body (excluding opening delimiter).
      body_end: End index of the inner body (excluding closing delimiter).
      value_text: Raw text of the scalar value (empty if is_message is True).
      token_type: TokenType classification of the scalar value.
    """

    is_message: bool
    body_start: int = 0
    body_end: int = 0
    value_text: str = ""
    token_type: TokenType = TokenType.OTHER_VALUE


def _find_block_end(
    text: str, open_delim: str, start_pos: int, end_pos: int
) -> tuple[int, int]:
    """Finds the matching closing delimiter for a block starting at start_pos.

    Scans forward by balancing opening and closing delimiters ({}, <>) while
    skipping string literals and comments in C via `_NEXT_DELIM_RE`.

    Performance note:
      If a caller traverses deeply nested submessages down to depth D, the
      innermost characters may be scanned D times (once per ancestor message as
      each ancestor indexes its direct children). In practice, protobuf schemas
      are shallow (typically D <= 3), and unaccessed large blocks (like command)
      are scanned only once.
      While it is possible to eliminate redundant scanning by memoizing child
      delimiter spans during the first pass, that would introduce bookkeeping
      and allocation overhead for every intermediate block across the entire file,
      slowing down the common path. The stateless scanner is a deliberate
      trade-off favoring simplicity and zero allocations.
    """
    stack = [_CLOSE_DELIM_BY_OPEN[open_delim]]
    pos = start_pos
    while pos < end_pos:
        m = _NEXT_DELIM_RE.match(text, pos, end_pos)
        if m is None:
            break
        ch = m.group(1)
        idx = m.end() - 1
        if ch == '"':
            raise ParseError(f"Unterminated string starting at {idx}")
        if ch in "{<":
            stack.append(_CLOSE_DELIM_BY_OPEN[ch])
            pos = idx + 1
        else:  # ch in "}>"
            if not stack or ch != stack[-1]:
                raise ParseError(
                    f"Mismatched end-of-block: got '{ch}', expected '{stack[-1] if stack else None}'"
                )
            stack.pop()
            pos = idx + 1
            if not stack:
                return idx, pos
    raise ParseError("Unexpected EOF, missing '>' or '}' end-of-block")


def _raise_index_parse_error(text: str, pos: int, end: int) -> NoReturn:
    """Diagnoses and raises a ParseError when _FIELD_STEP_RE fails at pos."""
    m_field = _FIELD_NAME_RE.match(text, pos, end)
    if not m_field:
        m_tok = _LEX_TOKEN_RE.match(text, pos, end)
        tok = (
            m_tok.group(0)[:_MAX_ERROR_SNIPPET_LEN]
            if m_tok is not None
            else text[pos]
        )
        raise ParseError(f"Expected a field name, but got '{tok}'.")

    raw_field = m_field.group(0)
    has_colon = raw_field.endswith(":")
    key = raw_field[:-1] if has_colon else raw_field
    pos = m_field.end()

    m_ws = _WHITESPACE_OR_COMMENT_RE.match(text, pos, end)
    if m_ws:
        pos = m_ws.end()

    if pos >= end:
        raise ParseError("Unexpected EOF, expecting a value or start-of-block.")

    next_ch = text[pos]
    if not has_colon:
        raise ParseError(
            f"Expected ':' or start-of-block after field '{key}', but got '{next_ch}'."
        )
    if next_ch == '"':
        raise ParseError(f"Unterminated string starting at {pos}")
    if next_ch in "}>":
        raise ParseError("Unexpected token: TokenType.END_BLOCK")
    raise ParseError(f"Unexpected character: '{next_ch}'")


def _index_fields(
    text: str, start: int, end: int, top: bool
) -> dict[str, list[_FieldEntry]]:
    """Indexes direct child fields within text[start:end] without descending.

    Args:
      text: The source textproto string.
      start: Starting character offset of the block to index.
      end: Ending character offset of the block to index.
      top: True if this is the root document, False if inside a submessage.

    Returns:
      Dictionary mapping field names to a list of _FieldEntry descriptors.
    """
    pos = start
    fields: dict[str, list[_FieldEntry]] = collections.defaultdict(list)
    while pos < end:
        m = _FIELD_STEP_RE.match(text, pos, end)
        if m is None:
            m_ws = _WHITESPACE_OR_COMMENT_RE.match(text, pos, end)
            if m_ws:
                pos = m_ws.end()
            if pos >= end:
                break
            _raise_index_parse_error(text, pos, end)

        kind = m.lastgroup
        if kind == "str":
            fields[m.group("key")].append(
                _FieldEntry(
                    is_message=False,
                    value_text=m.group("str"),
                    token_type=TokenType.STRING_VALUE,
                )
            )
            pos = m.end()
        elif kind == "val":
            fields[m.group("key")].append(
                _FieldEntry(
                    is_message=False,
                    value_text=m.group("val"),
                    token_type=TokenType.OTHER_VALUE,
                )
            )
            pos = m.end()
        elif kind == "block" or kind == "colonless_block":
            # Submessage block: record the inner body span without descending
            # or parsing any nested fields inside it.
            body_start = m.end()
            body_end, pos = _find_block_end(
                text, text[body_start - 1], body_start, end
            )
            fields[m.group("key")].append(
                _FieldEntry(
                    is_message=True,
                    body_start=body_start,
                    body_end=body_end,
                )
            )
        else:  # kind == "end_block"
            if top:
                raise ParseError(
                    "Unexpected end-of-block at top-level before EOF."
                )
            break

    return fields


class LazyMapping(TextProtoMapping):
    """Lazy, hierarchical mapping representation of a text protobuf message.

    This class provides a drop-in replacement for the nested dictionary structure
    returned by `parse()`, while deferring tokenization and AST generation until
    individual fields are accessed. Conceptually analogous to hierarchical C++
    `std::string_view`s over a single shared text buffer, each `LazyMapping`
    windows into `text[start:end]` without copying memory or creating intermediate
    substring allocations.

    Behavior and Lifecycle:
      - Instantiation: Fast and non-allocating; only records the string buffer
        and character bounds `[start, end]`.
      - Shallow Indexing: On the first field access, `len()`, or key iteration,
        `_index_fields` scans only the direct child fields of this block,
        recording `_FieldEntry` descriptors. Nested submessages (`{ ... }` or
        `< ... >`) have their boundaries detected by balancing delimiters while
        respecting string literals and comments, but their inner contents are not
        descended into or tokenized.
      - On-Demand Materialization: Accessing `mapping[key]` materializes only
        that specific field:
          * Submessages are wrapped in child `LazyMapping` instances restricted
            to their inner body spans.
          * Scalar values are converted into `Token` instances (`Token(text, type)`).
          * Repeated submessages containing `key` and `value` fields (e.g. map
            fields like `output_file_digests` or `platform`) are converted to
            dictionaries (`{k: v}`), matching eager `_auto_dict` behavior.
        Materialized values are cached so subsequent lookups are O(1).
      - Eager Conversion: Calling `.to_dict()` recursively evaluates all fields
        and child mappings into a standard Python `dict`.
      - Equality: Compares equal (`==`) to standard Python `dict`s with identical
        contents.

    Example:
      >>> msg = parse_lazy('command: { args: "echo" } status: OK')
      >>> msg["status"][0].text
      'OK'
      >>> # The "command" submessage body is only parsed if accessed:
      >>> msg["command"][0]["args"][0].text
      '"echo"'
    """

    def __init__(
        self,
        text: str,
        start: int = 0,
        end: int | None = None,
        top: bool = False,
    ):
        self._text = text
        self._start = start
        self._end = len(text) if end is None else end
        self._top = top
        self._fields: dict[str, list[_FieldEntry]] | None = None
        self._cache: dict[str, Any] = {}

    def _index(self) -> dict[str, list[_FieldEntry]]:
        if self._fields is None:
            self._fields = _index_fields(
                self._text, self._start, self._end, top=self._top
            )
        return self._fields

    def __getitem__(self, key: str) -> Any:
        # Return already-materialized field from cache if available.
        if key in self._cache:
            return self._cache[key]

        # Trigger shallow indexing of direct fields on first access.
        fields = self._index()
        if key not in fields:
            raise KeyError(key)

        # Materialize only the requested field's entries:
        # - Submessages are wrapped in child LazyMapping instances without
        #   parsing their internal contents.
        # - Scalars are converted into Token instances.
        entries = fields[key]
        values: list[Any] = []
        for entry in entries:
            if entry.is_message:
                values.append(
                    LazyMapping(
                        self._text,
                        start=entry.body_start,
                        end=entry.body_end,
                        top=False,
                    )
                )
            else:
                values.append(
                    Token(text=entry.value_text, type=entry.token_type)
                )

        # Convert repeated key-value pairs (e.g. output_file_digests, platform)
        # to dictionaries, matching eager parse() behavior.
        result = _auto_dict(values)
        self._cache[key] = result
        return result

    def __iter__(self) -> Iterator[str]:
        return iter(self._index())

    def __len__(self) -> int:
        return len(self._index())

    def __contains__(self, key: object) -> bool:
        return key in self._index()

    def __repr__(self) -> str:
        return f"LazyMapping({dict(self.items())})"

    def __eq__(self, other: Any) -> bool:
        """Compares this mapping with another mapping for equality.

        Note:
          Unless identity (`other is self`) or length checks fail early, equality
          comparison fully materializes this mapping (and any child LazyMappings)
          into standard Python dicts to perform deep recursive comparison. This is
          primarily intended for unit tests and assertions; avoid using equality
          checks on large LazyMappings in performance-critical hot paths.
        """
        if other is self:
            return True
        if not isinstance(other, Mapping) or len(self) != len(other):
            return False
        return dict(self.items()) == dict(other.items())

    def to_dict(self) -> TextProtoDict:
        """Recursively converts this LazyMapping into a standard Python dict."""

        def _conv(v: Any) -> Any:
            if isinstance(v, LazyMapping):
                return v.to_dict()
            if isinstance(v, list):
                return [_conv(item) for item in v]
            if isinstance(v, dict):
                return {k: _conv(val) for k, val in v.items()}
            return v

        return {k: _conv(self[k]) for k in self}


def parse_lazy(text: str) -> LazyMapping:
    """Parse a text protobuf lazily into a recursive mapping.

    Blocks and fields are indexed on demand without tokenizing or parsing
    unaccessed submessages.

    Unlike `parse()`, which accepts an `Iterable[str]` of lines, `parse_lazy()`
    operates directly on the complete flat text of the protobuf. Callers reading
    from disk should pass `Path(...).read_text()` or `f.read()` to avoid the
    memory allocation and line-splitting overhead of `readlines()`.

    Args:
      text: Complete text of the text protobuf (e.g. `Path.read_text()`).

    Returns:
      Lazy mapping representation of the text proto. Fields are treated as
      repeated (lists) or as key-value dictionaries.
    """
    return LazyMapping(text, start=0, end=len(text), top=True)


def walk_messages(msg: TextProtoMapping) -> Iterator[TextProtoMapping]:
    """Yields msg and all nested Mapping blocks within it in pre-order."""
    yield msg
    for values in msg.values():
        if isinstance(values, Mapping):
            yield from walk_messages(values)
        elif isinstance(values, Sequence) and not isinstance(
            values, (str, bytes)
        ):
            for item in values:
                if isinstance(item, Mapping):
                    yield from walk_messages(item)


def _main_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Parse any text proto and show its representation.",
        argument_default=None,
    )
    parser.add_argument(
        "input",
        type=Path,
        metavar="FILE",
        help="The text proto file to parse",
    )
    return parser


_MAIN_ARG_PARSER = _main_arg_parser()


def main(argv: Sequence[str]) -> int:
    args = _MAIN_ARG_PARSER.parse_args(argv)
    with open(args.input) as f:
        data = parse(f)
    print(data)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
