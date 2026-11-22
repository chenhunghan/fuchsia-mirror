# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared utilities for detecting and extracting @bazel2gn: comment annotations."""

import ast
import dataclasses
import io
import re
import tokenize
from collections.abc import Sequence

from framework import FileContext


@dataclasses.dataclass(frozen=True)
class _SpanNode(ast.AST):
    """Synthetic AST node holding line and byte offset spans for FileContext.report."""

    lineno: int
    col_offset: int
    end_lineno: int
    end_col_offset: int


@dataclasses.dataclass(frozen=True)
class Bazel2gnCommentAnnotation:
    """Represents a matched @bazel2gn: comment annotation in a Bazel file.

    Attributes:
        directive: The annotation directive name (e.g. "path_overwrite", "raw_overwrite").
        comment_text: The full text of the comment (e.g. "# @bazel2gn:path_overwrite:...").
        lineno: 1-based line number of the comment.
        span_node: Synthetic AST node spanning the comment for reporting findings.
        targets: Target name(s) associated with this comment, or () if outside any target.
    """

    directive: str
    comment_text: str
    lineno: int
    span_node: ast.AST
    targets: tuple[str, ...]


def _find_enclosing_target_name(fc: FileContext, lineno: int) -> str | None:
    """Finds the enclosing named target for line number `lineno`.

    Walks `fc.calls` to find statement-level calls whose line range covers
    `lineno` and returns the enclosing call's `target_name`.
    """
    enclosing_target = None

    for call in fc.calls:
        if not call.target_name:
            continue
        start_line = getattr(call.node, "lineno", None)
        end_line = getattr(call.node, "end_lineno", start_line)
        if (
            start_line is not None
            and end_line is not None
            and start_line <= lineno <= end_line
        ):
            if call.is_top_level:
                return call.target_name
            enclosing_target = call.target_name

    return enclosing_target


def find_bazel2gn_comment_annotations(
    fc: FileContext,
    directive_regex: re.Pattern[str],
) -> Sequence[Bazel2gnCommentAnnotation]:
    """Finds all @bazel2gn: comments matching `directive_regex` in `fc`.

    Uses Python's tokenizer so `#` comments inside string literals or docstrings
    are ignored.

    Args:
        fc: The FileContext to scan.
        directive_regex: A compiled regex that matches against the comment string
            and has group 1 capturing the directive name (e.g.
            `re.compile(r"@bazel2gn:([a-zA-Z0-9_]*_overwrite)")`).

    Returns:
        A sequence of `Bazel2gnCommentAnnotation` objects in line order.
    """
    annotations = []
    source = "\n".join(fc.lines)

    try:
        tokens = tokenize.generate_tokens(io.StringIO(source).readline)
        for tok in tokens:
            if tok.type != tokenize.COMMENT:
                continue
            match = directive_regex.search(tok.string)
            if not match:
                continue

            directive = match.group(1)
            lineno = tok.start[0]
            start_char_col = tok.start[1]
            end_char_col = tok.end[1]

            line_text = (
                fc.lines[lineno - 1] if 1 <= lineno <= len(fc.lines) else ""
            )
            # Python's `tokenize.generate_tokens` returns character offsets in
            # `tok.start[1]` and `tok.end[1]`. In contrast, Python's standard
            # AST specification (since Python 3.8) and `FileContext.report`
            # expect `col_offset` and `end_col_offset` on AST nodes to be
            # 0-based UTF-8 byte offsets (which `_byte_offset_to_char_col`
            # converts to 1-based character columns for shac findings).
            # Convert character columns to UTF-8 byte offsets so `_SpanNode`
            # conforms to the AST contract.
            col_offset = len(line_text[:start_char_col].encode("utf-8"))
            end_col_offset = len(line_text[:end_char_col].encode("utf-8"))

            span_node = _SpanNode(
                lineno=lineno,
                col_offset=col_offset,
                end_lineno=tok.end[0],
                end_col_offset=end_col_offset,
            )

            target_name = _find_enclosing_target_name(fc, lineno)
            targets = (target_name,) if target_name is not None else ()

            annotations.append(
                Bazel2gnCommentAnnotation(
                    directive=directive,
                    comment_text=tok.string.strip(),
                    lineno=lineno,
                    span_node=span_node,
                    targets=targets,
                )
            )
    except tokenize.TokenError:
        # TokenError can occur on unclosed multiline strings or other tokenization
        # errors. AST parsing already validates syntax, so ignore tokenization
        # errors and return any annotations collected up to this point.
        pass

    return tuple(annotations)
