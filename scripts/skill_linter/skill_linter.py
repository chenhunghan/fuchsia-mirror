# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Skill and workflow linter for SKILL.md, saved workflows, and .agents JSON configs."""

import argparse
import ast
import collections.abc
import json
import logging
import os
import re
import sys
import textwrap
import types
from collections.abc import Collection, Mapping, Sequence, Set
from typing import Any, TypedDict

import yaml

# Constants for skill validation and formatting.
MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
LINE_LENGTH = 80
_WARN_SKILL_LINES = 500
_MAX_SKILL_LINES = 800
_WARN_SKILL_BYTES = 35000
_MAX_SKILL_BYTES = 46080

_LINK_REFERENCE_RE = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*")
_LIST_ITEM_RE = re.compile(r"^\s*([-*+]|\d+\.)\s+")

_ALLOWED_SKILL_FRONTMATTER_KEYS = frozenset(
    {
        "name",
        "description",
        "tools",
        "license",
        "allowed-tools",
        "metadata",
        "compatibility",
        "support_tier",
        "version",
        "tags",
        "category",
        "disable-model-invocation",
        "disable-slash-command",
    }
)

_ALLOWED_WORKFLOW_MANIFEST_KEYS = frozenset(
    {"name", "description", "entry", "args", "library"}
)

_ALLOWED_DISCOVERY_CONFIG_KEYS = frozenset(
    {"entries", "inherits", "exclude", "comment"}
)

_ALLOWED_CALL_TOOLS = frozenset(
    {
        "edit_file",
        "invoke_subagent",
        "manage_task",
        "read_file",
        "search_web",
        "send_message",
        "swarm",
        "write_file",
    }
)

_ALLOWED_WORKFLOW_WORKSPACES = frozenset({"inherit", "branch", "share"})
_ALLOWED_WORKFLOW_MODELS = frozenset({"inherit", "flash_lite", "flash", "pro"})

_DISCOVERY_JSON_FILENAMES = frozenset(
    {
        "agents.json",
        "plugins.json",
        "rules.json",
        "skills.json",
        "workflows.json",
    }
)

# Grandfathered skills pre-dating the 800-line / 45 KiB ceiling and .agents/ plural rule.
_LEGACY_OVERSIZED_SKILLS = frozenset(
    {
        "src/devices/skills/autoda-fix/SKILL.md",
        "src/devices/skills/driver-lab/SKILL.md",
        "zircon/skills/cpp-to-rust-rubric/SKILL.md",
        "zircon/skills/kernel-review/SKILL.md",
    }
)

_LEGACY_SINGULAR_DOT_AGENT_PREFIXES = (
    "build/beads/.agent/",
    "src/storage/.agent/",
)

_POSITIVE_TRIGGER_RE = re.compile(
    r"\b(use when|use this|use for|trigger|invoke when|whenever)\b",
    re.IGNORECASE,
)
_NEGATIVE_TRIGGER_RE = re.compile(
    r"\b(don't use|do not use|not for|avoid using|never use)\b",
    re.IGNORECASE,
)
_BINARY_IN_DESC_RE = re.compile(
    r"(/google/bin/releases/|/google/data/ro/|\bblaze run //)"
)
_FILLER_NAME_RE = re.compile(
    r"(?:^|-)(skill|guide|playbook|how-to|checklist|helper|utils)(?:$|-)"
)
_LOCAL_MACHINE_PATH_RE = re.compile(
    r"(/usr/local/google/home/|/google/src/cloud/|/Users/[A-Za-z0-9._-]+/)"
)
_CALL_TOOL_SNIPPET_RE = re.compile(
    r"""\bcall_tool\(\s*["']([A-Za-z0-9_-]+)["']"""
)
_MUTATION_CMD_RE = re.compile(
    r"\b(issues\s+mutate|gerrit\s+(?:comments|vote|submit|abandon)|jiri\s+upload|git\s+push)\b"
)
_ARG_TABLE_HEADER_RE = re.compile(
    r"^\|\s*(?:Argument|Parameter|Arg|Flag)\s*\|", re.IGNORECASE
)
_INLINE_CODE_OR_LINK_RE = re.compile(
    r"`+[^`]*`+|\[(?:`+[^`]*`+|[^\]])*\]\(([^)]*)\)"
)
_REF_LINK_DEF_RE = re.compile(r"^\s{0,3}\[(?:`+[^`]*`+|[^\]])+\]:\s*(\S+)")
_FENCE_RE = re.compile(r"^(`{3,})(.*)$")
_NEGATIVE_MARKER_RE = re.compile(
    r"#\s*(?:bad|wrong|avoid|invalid|do not|don't)\b", re.IGNORECASE
)
_SKIP_LINK_SCHEMES = (
    "http://",
    "https://",
    "mailto:",
    "go/",
    "sidecar://",
    "conversation://",
)
_AGENT_KW_ALLOWED = types.MappingProxyType(
    {
        "workspace": _ALLOWED_WORKFLOW_WORKSPACES,
        "model": _ALLOWED_WORKFLOW_MODELS,
    }
)
_CLI_RUNNER_MODULE_ATTRS = types.MappingProxyType(
    {
        "subprocess": frozenset(
            {
                "Popen",
                "call",
                "check_call",
                "check_output",
                "getoutput",
                "getstatusoutput",
                "run",
            }
        ),
        "os": frozenset(
            {
                "execl",
                "execle",
                "execlp",
                "execlpe",
                "execv",
                "execve",
                "execvp",
                "execvpe",
                "popen",
                "spawnl",
                "spawnle",
                "spawnlp",
                "spawnlpe",
                "spawnv",
                "spawnve",
                "spawnvp",
                "spawnvpe",
                "system",
            }
        ),
        "asyncio": frozenset(
            {"create_subprocess_exec", "create_subprocess_shell"}
        ),
    }
)
_CLI_RUNNER_DIRECT_NAMES = frozenset(
    {
        "Popen",
        "call",
        "check_call",
        "check_output",
        "create_subprocess_exec",
        "create_subprocess_shell",
        "exec_cmd",
        "getoutput",
        "getstatusoutput",
        "popen",
        "run",
        "run_cli",
        "run_cmd",
        "run_command",
        "sh",
        "system",
    }
)


class Finding(TypedDict, total=False):
    filepath: str
    message: str
    level: str
    replacements: list[str]
    line: int


class _UniqueKeyLoader(yaml.SafeLoader):
    """YAML SafeLoader that rejects duplicate mapping keys.

    PyYAML silently overwrites duplicate keys, whereas Jetski's Go yaml.v3
    skill loader rejects them at runtime.
    """

    def construct_mapping(
        self, node: yaml.MappingNode, deep: bool = False
    ) -> dict[Any, Any]:
        """Constructs a YAML mapping while checking for duplicate keys.

        Args:
          node: The YAML mapping node to construct.
          deep: Whether to recursively construct nested objects.

        Returns:
          The constructed dictionary.

        Raises:
          yaml.constructor.ConstructorError: If a duplicate or unhashable key is
            present.
        """
        seen: set[Any] = set()
        for key_node, _ in node.value:
            if key_node.tag == "tag:yaml.org,2002:merge":
                continue
            key: Any = self.construct_object(key_node, deep=deep)
            if not isinstance(key, collections.abc.Hashable):
                raise yaml.constructor.ConstructorError(
                    "while constructing a mapping",
                    node.start_mark,
                    "found unhashable key",
                    key_node.start_mark,
                )
            if key in seen:
                raise yaml.constructor.ConstructorError(
                    "while constructing a mapping",
                    node.start_mark,
                    f"duplicate YAML key '{key}' in frontmatter",
                    key_node.start_mark,
                )
            seen.add(key)
        return super().construct_mapping(node, deep=deep)


def _parse_skill_frontmatter(
    content: str,
) -> tuple[dict[str, Any], str]:
    """Parses YAML frontmatter from SKILL.md content with duplicate-key checks.

    Args:
      content: Full raw SKILL.md content.

    Returns:
      A tuple of (metadata_dict, body_content).

    Raises:
      ValueError: If frontmatter delimiters are missing, YAML parsing fails,
        or the parsed YAML value is not a mapping.
    """
    if not content.startswith("---"):
        raise ValueError(
            "Could not find or parse YAML frontmatter (must start with '---')."
        )
    parts = content.split("---", 2)
    if len(parts) < 3:
        raise ValueError(
            "Could not find closing '---' delimiter for YAML frontmatter."
        )

    frontmatter = parts[1]
    try:
        meta = yaml.load(frontmatter, Loader=_UniqueKeyLoader)
    except yaml.YAMLError as e:
        raise ValueError(f"Invalid YAML frontmatter: {e}") from e

    if not isinstance(meta, dict):
        raise ValueError("YAML frontmatter must be a key-value mapping.")

    return meta, parts[2]


def _suggest_valid_name(name: str) -> str:
    """Suggests a valid name by replacing invalid characters.

    Converts to lowercase, replaces underscores with hyphens, removes
    non-alphanumeric characters (except hyphens), and truncates to
    MAX_NAME_LENGTH.

    Args:
      name: The original name string.

    Returns:
      A suggested valid name string.
    """
    suggested = name.lower()
    suggested = suggested.replace("_", "-")
    suggested = re.sub(r"[^a-z0-9-]", "", suggested)
    return suggested[:MAX_NAME_LENGTH].strip("-")


def _pre_process(text: str, tab_size: int) -> str:
    """Pre-processes text to standardize list item indentation.

    Fixes bullet points and numbered lists that have excessive spacing.
    Standardizes child list items, code blocks, and continuation paragraphs
    under numbered list items to match the 4-space list indentation.
    Skips lines within code blocks.

    Args:
      text: The input markdown text.
      tab_size: The indentation size to use for numbered lists.

    Returns:
      The processed text with standardized indentation.
    """
    lines = text.split("\n")
    processed = []
    in_top_level_code_block = False
    in_nested_code_block = False
    nested_code_block_shifted = False
    active_num_indent: int | None = None

    for line in lines:
        stripped = line.strip()

        # Handle top level code block
        if in_top_level_code_block:
            if stripped.startswith("```"):
                in_top_level_code_block = False
            processed.append(line)
            continue

        # Handle nested code block
        if in_nested_code_block:
            if stripped.startswith("```"):
                if nested_code_block_shifted and active_num_indent is not None:
                    target_3 = " " * (active_num_indent + 3)
                    if line.startswith(
                        target_3 + "```"
                    ) and not line.startswith(target_3 + " ```"):
                        line = " " + line
                in_nested_code_block = False
                nested_code_block_shifted = False
            else:
                if nested_code_block_shifted and active_num_indent is not None:
                    target_3 = " " * (active_num_indent + 3)
                    if line.startswith(target_3):
                        line = " " + line
            processed.append(line)
            continue

        if not stripped:
            processed.append(line)
            continue

        if stripped.startswith("#") or re.match(r"^([-*_]\s*){3,}$", stripped):
            active_num_indent = None

        # Check for numbered list item
        num_match = re.match(r"^(\s*)(\d+\.)( +)(.*)$", line)
        if num_match:
            indent_str = num_match.group(1)
            marker_str = num_match.group(2)
            rest = num_match.group(4)
            repl = " " if tab_size == 2 else "  "
            line = f"{indent_str}{marker_str}{repl}{rest}"
            if tab_size == 4:
                active_num_indent = len(indent_str)
                in_nested_code_block = False
                nested_code_block_shifted = False
            processed.append(line)
            continue

        # Check if non-empty line terminates active_num_indent
        if active_num_indent is not None:
            indent_len = len(line) - len(line.lstrip(" "))
            if indent_len <= active_num_indent:
                active_num_indent = None

        # Code block opening
        if stripped.startswith("```"):
            if active_num_indent is not None and tab_size == 4:
                target_3 = " " * (active_num_indent + 3)
                if line.startswith(target_3 + "```") and not line.startswith(
                    target_3 + " ```"
                ):
                    line = " " + line
                    nested_code_block_shifted = True
                in_nested_code_block = True
            else:
                in_top_level_code_block = True
            processed.append(line)
            continue

        if active_num_indent is not None and tab_size == 4:
            target_3 = " " * (active_num_indent + 3)
            target_5 = " " * (active_num_indent + 5)
            # 3-space bullet point
            bullet_3_match = re.match(
                r"^" + target_3 + r"([-*+])( +)(.*)$", line
            )
            if bullet_3_match:
                marker = bullet_3_match.group(1)
                rest = bullet_3_match.group(3)
                line = " " + target_3 + marker + " " + rest
                processed.append(line)
                continue
            # 5-space sub-bullet
            bullet_5_match = re.match(
                r"^" + target_5 + r"([-*+])( +)(.*)$", line
            )
            if bullet_5_match:
                marker = bullet_5_match.group(1)
                rest = bullet_5_match.group(3)
                line = " " + target_5 + marker + " " + rest
                processed.append(line)
                continue
            # 5-space continuation
            if line.startswith(target_5) and not line.startswith(
                target_5 + " "
            ):
                line = " " + line
                processed.append(line)
                continue
            # 3-space continuation or paragraph
            if line.startswith(target_3) and not line.startswith(
                target_3 + " "
            ):
                line = " " + line
                processed.append(line)
                continue

        # Match bullet points (-, *, +) at the start of the line with excessive spacing.
        # Capture group 1: indentation and bullet.
        # Capture group 2: extra spaces.
        match = re.search(r"^(\s*[-*+])( +)(?=\S)", line)
        if match:
            repl = " "
            line = match.group(1) + repl + line[match.end() :]
        else:
            # Match numbered lists (e.g., 1.) at the start of the line.
            match = re.search(r"^(\s*\d+\.)( +)(?=\S)", line)
            if match:
                repl = " " if tab_size == 2 else "  "
                line = match.group(1) + repl + line[match.end() :]
        processed.append(line)
    return "\n".join(processed)


def _post_process(text: str) -> str:
    """Post-processes text to clean up whitespace.

    Removes trailing whitespace while preserving Markdown hard line breaks
    (at least two spaces at the end of a line), collapses multiple empty
    lines, and ensures the file ends with a newline.

    Args:
      text: The input markdown text.

    Returns:
      The processed text with clean whitespace.
    """
    # Remove trailing whitespace, but preserve Markdown hard line breaks (two spaces).
    text = re.sub(
        r"[ \t]+$",
        lambda m: "  " if m.group(0).endswith("  ") else "",
        text,
        flags=re.MULTILINE,
    )
    # Collapse triple newlines or more into double newlines.
    text = re.sub(r"\n{3,}", "\n\n", text)
    if not text.endswith("\n"):
        text += "\n"
    return text


def _format_paragraph(paragraph: list[str], width: int) -> list[str]:
    """Formats a single paragraph of markdown text.

    Handles list items by indenting subsequent lines to match the marker.
    Preserves hard line breaks in normal paragraphs.

    Args:
      paragraph: A list of lines forming a paragraph.
      width: The maximum line width for wrapping.

    Returns:
      A list of wrapped and formatted lines.
    """
    if not paragraph:
        return []

    first_line = paragraph[0]
    # Check if this is a list item.
    list_match = re.search(r"^(\s*)([-*+]|\d+\.)\s+", first_line)

    if list_match:
        marker = list_match.group(0)
        marker_len = len(marker)
        subsequent_indent = " " * marker_len

        first_line_text = first_line[marker_len:].strip()
        other_lines_text = " ".join([line.strip() for line in paragraph[1:]])
        full_text = (first_line_text + " " + other_lines_text).strip()

        wrapper = textwrap.TextWrapper(
            width=width,
            initial_indent=marker,
            subsequent_indent=subsequent_indent,
            break_long_words=False,
            break_on_hyphens=False,
        )
        return wrapper.wrap(full_text)
    else:
        # Normal paragraph.
        indent_match = re.search(r"^\s*", first_line)
        indent = indent_match.group(0) if indent_match else ""

        # Preserve Markdown hard line breaks (two spaces at end of line).
        has_hard_break = paragraph[-1].endswith("  ")
        content = " ".join([line.strip() for line in paragraph])
        if has_hard_break:
            content = content.rstrip() + "  "

        wrapper = textwrap.TextWrapper(
            width=width,
            initial_indent=indent,
            subsequent_indent=indent,
            break_long_words=False,
            break_on_hyphens=False,
        )
        wrapped_lines = wrapper.wrap(content)
        if has_hard_break and wrapped_lines:
            wrapped_lines[-1] = wrapped_lines[-1].rstrip() + "  "
        return wrapped_lines


def _is_table_separator(line: str) -> bool:
    """Checks if a line is a markdown table separator (e.g., |---|).

    Args:
      line: The line to check.

    Returns:
      True if the line is a table separator, False otherwise.
    """
    stripped = line.strip()
    return "|" in line and bool(re.fullmatch(r"[|\-\s:]+", stripped))


def _get_table_line_indices(lines: list[str]) -> set[int]:
    """Identifies all lines that are part of a markdown table.

    Args:
      lines: The markdown lines to evaluate.

    Returns:
      A set of integer indices representing table lines.
    """
    table_indices = set()
    in_table = False
    for idx, line in enumerate(lines):
        stripped = line.strip()
        if not stripped:
            in_table = False
            continue

        if (
            idx + 1 < len(lines)
            and "|" in line
            and _is_table_separator(lines[idx + 1])
        ):
            table_indices.add(idx)

        if _is_table_separator(line):
            in_table = True
            table_indices.add(idx)
            continue

        if in_table:
            if "|" in line or stripped.startswith(":"):
                table_indices.add(idx)
            else:
                in_table = False

    return table_indices


def format_markdown_with_issues(
    text: str, *, tab_size: int = 4, width: int = LINE_LENGTH
) -> tuple[str, list[str]]:
    """Formats markdown text and identifies specific linter issues.

    Identifies paragraphs, lists, code blocks, and tables to apply
    appropriate formatting while preserving structure. Tracks specific
    issues like long lines, trailing whitespace, missing empty lines around
    lists and code blocks, etc.

    Args:
      text: The input markdown text.
      tab_size: Indentation size for lists.
      width: Maximum line length (defaults to LINE_LENGTH).

    Returns:
      A tuple containing the formatted markdown string and a list of detected
      issues.
    """
    issues = []

    raw_lines = text.split("\n")

    in_code_block = False
    has_consecutive_empty_lines = False
    has_trailing_whitespace = False
    consecutive_empty_count = 0

    for line in raw_lines:
        stripped = line.strip()
        if stripped.startswith("```"):
            in_code_block = not in_code_block
            consecutive_empty_count = 0
            continue

        if in_code_block:
            continue

        if not stripped:
            consecutive_empty_count += 1
            if consecutive_empty_count >= 2:
                has_consecutive_empty_lines = True
        else:
            consecutive_empty_count = 0

        stripped_len = len(line.rstrip(" \t"))
        trailing_len = len(line) - stripped_len
        if trailing_len > 0 and trailing_len != 2:
            has_trailing_whitespace = True

    if has_consecutive_empty_lines:
        issues.append("consecutive empty lines")
    if has_trailing_whitespace:
        issues.append("trailing whitespace")

    preprocessed_text = _pre_process(text, tab_size)
    if preprocessed_text != text:
        issues.append("excessive list item spacing")

    lines = preprocessed_text.split("\n")
    formatted_lines = []
    code_blocks: list[str] = []
    current_code_block: list[str] = []

    in_code_block = False
    current_paragraph: list[str] = []
    in_table = False
    in_list = False
    has_missing_empty_line_before_list = False
    has_missing_empty_line_after_list = False
    has_missing_empty_line_before_code_block = False
    has_missing_empty_line_after_code_block = False
    just_closed_code_block = False
    # Whether the last emitted non-empty block is a list item paragraph. A
    # list item may directly follow another list item (tight list), but must
    # be separated by an empty line from any other preceding block.
    last_block_is_list_item = False

    def flush_paragraph() -> None:
        nonlocal current_paragraph, last_block_is_list_item
        if current_paragraph:
            last_block_is_list_item = bool(
                _LIST_ITEM_RE.match(current_paragraph[0])
            )
        formatted_lines.extend(_format_paragraph(current_paragraph, width))
        current_paragraph[:] = []

    def emit(line: str) -> None:
        nonlocal last_block_is_list_item
        if line.strip():
            last_block_is_list_item = False
        formatted_lines.append(line)

    def needs_empty_line() -> bool:
        return bool(formatted_lines) and bool(formatted_lines[-1].strip())

    def end_list_with_empty_line() -> None:
        # A non-indented block (heading, blockquote, table, or link reference
        # definition) directly after a list must be separated by an empty line.
        nonlocal in_list, has_missing_empty_line_after_list
        if in_list and needs_empty_line():
            formatted_lines.append("")
            has_missing_empty_line_after_list = True
        in_list = False

    for line in lines:
        stripped_line = line.strip()
        is_indented = line.startswith((" ", "\t"))

        if in_code_block:
            if stripped_line.startswith("```"):
                current_code_block.append(line)
                placeholder = f"__SKILL_LINTER_CODE_BLOCK_{len(code_blocks)}__"
                code_blocks.append("\n".join(current_code_block))
                emit(placeholder)
                current_code_block = []
                in_code_block = False
                if not is_indented:
                    in_list = False
                just_closed_code_block = True
            else:
                current_code_block.append(line)
            continue

        if just_closed_code_block:
            just_closed_code_block = False
            if stripped_line:
                formatted_lines.append("")
                has_missing_empty_line_after_code_block = True

        if stripped_line.startswith("```"):
            flush_paragraph()
            if needs_empty_line():
                formatted_lines.append("")
                has_missing_empty_line_before_code_block = True
            current_code_block.append(line)
            in_code_block = True
            # Code blocks indented under a list item belong to that item.
            if not is_indented:
                in_list = False
            continue

        if not stripped_line:
            flush_paragraph()
            in_table = False
            # A list stays active across empty lines; it only ends at the
            # next non-indented block that is not a list item.
            formatted_lines.append(line)
            continue

        if in_table:
            if "|" in line or stripped_line.startswith(":"):
                emit(line)
                continue
            else:
                in_table = False

        if _is_table_separator(line):
            if current_paragraph:
                header_line = current_paragraph.pop()
                flush_paragraph()
                if not header_line.startswith((" ", "\t")):
                    end_list_with_empty_line()
                emit(header_line)
            else:
                flush_paragraph()
            emit(line)
            in_table = True
            in_list = False
            continue

        if stripped_line.startswith("#") or stripped_line.startswith(">"):
            flush_paragraph()
            if not is_indented:
                end_list_with_empty_line()
            emit(line)
            continue

        if _LINK_REFERENCE_RE.match(stripped_line):
            flush_paragraph()
            end_list_with_empty_line()
            emit(line)
            continue

        if _LIST_ITEM_RE.match(line):
            flush_paragraph()
            if needs_empty_line() and not last_block_is_list_item:
                formatted_lines.append("")
                has_missing_empty_line_before_list = True
            in_list = True
            current_paragraph.append(line)
            continue

        # Non-indented text directly following a list item is a lazy
        # continuation of that item, so the list remains active. After an
        # empty line, non-indented text starts a new paragraph that ends the
        # list.
        if not is_indented and not current_paragraph and not needs_empty_line():
            in_list = False
        current_paragraph.append(line)
        if line.endswith("  "):
            flush_paragraph()

    flush_paragraph()

    # If a file ends inside a code block, preserve it.
    if in_code_block and current_code_block:
        placeholder = f"__SKILL_LINTER_CODE_BLOCK_{len(code_blocks)}__"
        code_blocks.append("\n".join(current_code_block))
        formatted_lines.append(placeholder)

    formatted_text = _post_process("\n".join(formatted_lines))

    for idx, code_block in enumerate(code_blocks):
        placeholder = f"__SKILL_LINTER_CODE_BLOCK_{idx}__"
        formatted_text = formatted_text.replace(placeholder, code_block)

    prep_table_indices = _get_table_line_indices(lines)

    in_code_block = False
    has_long_lines = False
    for idx, line in enumerate(lines):
        if idx in prep_table_indices:
            continue
        stripped = line.strip()
        if stripped.startswith("```"):
            in_code_block = not in_code_block
            continue
        if in_code_block:
            continue
        if not stripped:
            continue
        if _LINK_REFERENCE_RE.match(stripped):
            continue
        if len(line) > width:
            has_long_lines = True
            break

    if has_long_lines:
        issues.append(f"lines exceeding {width} characters")

    if has_missing_empty_line_before_list:
        issues.append("missing empty line before list")

    if has_missing_empty_line_after_list:
        issues.append("missing empty line after list")

    if has_missing_empty_line_before_code_block:
        issues.append("missing empty line before code block")

    if has_missing_empty_line_after_code_block:
        issues.append("missing empty line after code block")

    if formatted_text != text and not issues:
        issues.append("formatting inconsistencies")

    return formatted_text, issues


def format_markdown(
    text: str, *, tab_size: int = 4, width: int = LINE_LENGTH
) -> str:
    """Formats markdown text to fit within the specified width.

    Args:
      text: The input markdown text.
      tab_size: Indentation size for lists.
      width: Maximum line length (defaults to LINE_LENGTH).

    Returns:
      The formatted markdown string.
    """
    formatted, _ = format_markdown_with_issues(
        text, tab_size=tab_size, width=width
    )
    return formatted


def _validate_name(
    meta: dict[str, Any], fixit: bool
) -> tuple[list[str], list[str]]:
    """Validates the 'name' field in the skill metadata.

    Checks for presence, type, length, and valid characters. If fixit is
    True, attempts to automatically fix issues.

    Args:
      meta: The metadata dictionary.
      fixit: Whether to apply fixes.

    Returns:
      A tuple of (errors, fixes_applied).
    """
    errors = []
    fixes_applied: list[str] = []
    if "name" not in meta:
        errors.append('Missing required field "name" in frontmatter.')
        return errors, fixes_applied

    name = meta["name"]
    if not isinstance(name, str) or not name.strip():
        errors.append('Field "name" must be a non-empty string.')
        return errors, fixes_applied

    has_length_error = len(name) > MAX_NAME_LENGTH
    has_char_error = (
        not re.fullmatch(r"^[a-z0-9-]+$", name)
        or name.startswith("-")
        or name.endswith("-")
        or "--" in name
    )

    if has_length_error and not fixit:
        errors.append(f'Field "name" exceeds {MAX_NAME_LENGTH} characters.')

    if has_char_error and not fixit:
        errors.append(
            'Field "name" must contain only lowercase letters, numbers, and'
            " single hyphens."
        )

    if fixit and (has_length_error or has_char_error):
        suggested_name = re.sub(r"-{2,}", "-", _suggest_valid_name(name))
        if suggested_name != name and suggested_name:
            meta["name"] = suggested_name
            fixes_applied.append(f'Fix name to "{suggested_name}"')
        elif not suggested_name:
            if has_length_error:
                errors.append(
                    f'Field "name" exceeds {MAX_NAME_LENGTH} characters.'
                )
            if has_char_error:
                errors.append(
                    'Field "name" must contain only lowercase letters,'
                    " numbers, and single hyphens."
                )

    if "<" in name or ">" in name:
        if not fixit:
            errors.append('Field "name" cannot contain XML tags.')
    return errors, fixes_applied


def _validate_description(
    meta: dict[str, Any], fixit: bool
) -> tuple[list[str], list[str]]:
    """Validates the 'description' field in the skill metadata.

    Checks for presence, emptiness, length, and XML tags. If fixit is
    True, attempts to automatically fix issues by stripping tags.

    Args:
      meta: The metadata dictionary.
      fixit: Whether to apply fixes.

    Returns:
      A tuple of (errors, fixes_applied).
    """
    errors = []
    fixes_applied: list[str] = []
    if "description" not in meta:
        errors.append('Missing required field "description" in frontmatter.')
        return errors, fixes_applied

    description = meta["description"]
    if not isinstance(description, str) or not description.strip():
        errors.append('Field "description" cannot be empty.')
        return errors, fixes_applied

    if len(description) > MAX_DESCRIPTION_LENGTH:
        errors.append(
            f'Field "description" exceeds {MAX_DESCRIPTION_LENGTH} characters.'
        )

    if "<" in description or ">" in description:
        if not fixit:
            errors.append('Field "description" cannot contain XML tags.')
        else:
            cleaned_description = re.sub(r"<[^>]+>", "", description)
            cleaned_description = re.sub(
                r" +", " ", cleaned_description
            ).strip()
            if cleaned_description != description:
                meta["description"] = cleaned_description
                fixes_applied.append("Strip XML tags from description.")
            else:
                errors.append(
                    'Field "description" cannot contain angle brackets (< or >).'
                )
    return errors, fixes_applied


def _generate_frontmatter_lines(
    key: str, val: Any, indent: int = 0
) -> list[str]:
    """Generates lines for the YAML frontmatter.

    Handles lists, dicts, None values, and wraps long descriptions using scalar
    blocks if necessary.

    Args:
      key: The metadata key.
      val: The metadata value.
      indent: Current indentation level in spaces.

    Returns:
      A list of lines for the frontmatter.
    """
    lines = []
    prefix = " " * indent
    if isinstance(val, list):
        lines.append(f"{prefix}{key}:")
        for item in val:
            if isinstance(item, dict):
                first = True
                for sub_k, sub_v in item.items():
                    sub_lines = _generate_frontmatter_lines(
                        str(sub_k), sub_v, indent + 4
                    )
                    if first and sub_lines:
                        sub_lines[0] = f"{prefix}  - " + sub_lines[0].lstrip()
                        first = False
                    lines.extend(sub_lines)
            else:
                lines.append(f"{prefix}  - {item}")
    elif isinstance(val, dict):
        lines.append(f"{prefix}{key}:")
        for sub_k, sub_v in val.items():
            lines.extend(
                _generate_frontmatter_lines(str(sub_k), sub_v, indent + 2)
            )
    elif val is None:
        lines.append(f"{prefix}{key}:")
    elif (
        indent == 0
        and key == "description"
        and isinstance(val, str)
        and len(val) > LINE_LENGTH
    ):
        lines.append(f"{key}: >")
        wrapper = textwrap.TextWrapper(
            width=LINE_LENGTH - 2, initial_indent="  ", subsequent_indent="  "
        )
        lines.extend(wrapper.wrap(val))
    elif isinstance(val, str) and (
        val[:1] in "@!&*{}[]|>'\"%`#,?:" or ": " in val or " #" in val
    ):
        lines.append(f"{prefix}{key}: {json.dumps(val)}")
    else:
        lines.append(f"{prefix}{key}: {val}")
    return lines


def _build_finding(
    rel_path: str,
    errors: list[str],
    warnings: list[str],
    fixes_applied: list[str],
    new_content: str,
    original_content: str,
) -> Finding | None:
    """Builds a Finding dict if there are errors, warnings, or suggested fixes.

    Args:
      rel_path: Relative path to the file.
      errors: List of error messages.
      warnings: List of warning messages.
      fixes_applied: List of fixes applied.
      new_content: The newly formatted content.
      original_content: The original file content.

    Returns:
      A Finding dict or None if no findings.
    """
    if not (new_content != original_content or errors or warnings):
        return None

    level = "error" if errors else "warning"
    messages = []
    if errors:
        messages.append("Errors:\n" + "\n".join(f"- {e}" for e in errors))
    if warnings:
        messages.append("Warnings:\n" + "\n".join(f"- {w}" for w in warnings))
    if fixes_applied and not messages:
        messages.append(
            "Suggested fixes:\n" + "\n".join(f"- {f}" for f in fixes_applied)
        )

    finding_message = "\n\n".join(messages) or "Skill linter findings."
    if fixes_applied or (new_content != original_content):
        finding_message += "\n\nYou can run fx format-code to fix these issues."

    finding: Finding = {
        "filepath": rel_path,
        "message": finding_message,
        "level": level,
    }
    if new_content != original_content:
        finding["replacements"] = [new_content]
    return finding


def lint_single_skill(
    skill_file: str,
    fixit: bool,
    stdout_mode: bool,
    suggest_fix_in_json_mode: bool = False,
) -> tuple[int, list[Finding]]:
    """Lints and formats a single SKILL.md file.

    Reads the file, parses metadata (including duplicate YAML key checks),
    validates fields, formats content, and writes back or outputs findings
    depending on the mode.

    Args:
      skill_file: Path to the SKILL.md file.
      fixit: Whether to apply fixes in-place.
      stdout_mode: Whether to output fixed content to stdout.
      suggest_fix_in_json_mode: Whether to return findings as JSON.

    Returns:
      A tuple of (exit_code, findings).
    """
    findings = []
    rel_path = os.path.relpath(skill_file)

    try:
        with open(skill_file, "r", encoding="utf-8") as f:
            original_content = f.read()
    except OSError as e:
        msg = f"Error reading '{skill_file}': {e}"
        if suggest_fix_in_json_mode:
            findings.append(
                Finding(filepath=rel_path, message=msg, level="error")
            )
            return 0, findings
        logging.error(msg)
        return 1, findings

    try:
        meta, content = _parse_skill_frontmatter(original_content)
    except ValueError as e:
        msg = f"Error in '{skill_file}': {e}"
        if suggest_fix_in_json_mode:
            findings.append(
                Finding(filepath=rel_path, message=msg, level="error")
            )
            return 0, findings
        logging.error(msg)
        return 1, findings

    content = content.lstrip("\n")
    errors = []
    warnings = []
    fixes_applied: list[str] = []

    unexpected_keys = set(meta.keys()) - _ALLOWED_SKILL_FRONTMATTER_KEYS
    if unexpected_keys:
        errors.append(
            "Unexpected frontmatter key(s): "
            + ", ".join(sorted(str(k) for k in unexpected_keys))
            + "."
        )

    name_errors, name_fixes = _validate_name(
        meta, fixit or suggest_fix_in_json_mode
    )
    errors.extend(name_errors)
    fixes_applied.extend(name_fixes)

    description_errors, description_fixes = _validate_description(
        meta, fixit or suggest_fix_in_json_mode
    )
    errors.extend(description_errors)
    fixes_applied.extend(description_fixes)

    formatted_content, markdown_issues = format_markdown_with_issues(content)
    if formatted_content != content:
        issues_str = ", ".join(markdown_issues)
        if not fixit and not suggest_fix_in_json_mode:
            warnings.append(f"Markdown body contains {issues_str}.")
        else:
            fixes_applied.append(f"Address markdown body: {issues_str}.")
            content = formatted_content

    skill_name = os.path.basename(os.path.dirname(skill_file))

    ordered_keys = ["name", "description"]
    frontmatter_lines = []

    for key in ordered_keys:
        if key in meta:
            frontmatter_lines.extend(
                _generate_frontmatter_lines(key, meta[key])
            )

    for key in meta:
        if key not in ordered_keys:
            frontmatter_lines.extend(
                _generate_frontmatter_lines(key, meta[key])
            )

    new_frontmatter = "\n".join(frontmatter_lines) + "\n"
    new_content = f"---\n{new_frontmatter}---\n\n{content}"

    if suggest_fix_in_json_mode:
        finding = _build_finding(
            rel_path,
            errors,
            warnings,
            fixes_applied,
            new_content,
            original_content,
        )
        if finding:
            findings.append(finding)
        return 0, findings

    if warnings:
        for warn in warnings:
            logging.warning(f"[{skill_name}] Warning: {warn}")

    if errors:
        for err in errors:
            logging.error(f"[{skill_name}] Error: {err}")

    has_autofixes = bool(
        fixes_applied
        or (formatted_content != content)
        or (new_content != original_content)
    )
    if has_autofixes and not suggest_fix_in_json_mode:
        logging.warning(
            f"[{skill_name}] You can run fx format-code to fix these issues."
        )

    if stdout_mode:
        print(new_content, end="")
    elif not suggest_fix_in_json_mode:
        if fixit and new_content != original_content:
            try:
                with open(skill_file, "w", encoding="utf-8") as f:
                    f.write(new_content)
                logging.info(f"[{skill_name}] Fixed in-place.")
            except OSError as e:
                logging.error(f"Error writing to '{skill_file}': {e}")
                return 1, findings
        elif not errors and not warnings and new_content == original_content:
            logging.info(f"PASSED: '{skill_name}' is valid.")
    return (1 if errors else 0), findings


def _rel_filepath(abs_path: str, repo_root: str | None) -> str:
    """Computes a normalized repository-relative path for SHAC findings.

    Args:
      abs_path: Absolute or relative file path.
      repo_root: Optional repository root path.

    Returns:
      Forward-slash relative filepath string.
    """
    base = os.path.abspath(repo_root) if repo_root is not None else os.getcwd()
    try:
        rel = os.path.relpath(os.path.abspath(abs_path), base)
    except ValueError:
        rel = abs_path
    return rel.replace(os.sep, "/")


def _resolve_local_target(
    target: str,
    source_dir: str,
    repo_root: str | None,
    fuchsia_root: str | None,
) -> bool:
    """Checks whether a local link or config path exists in the checkout.

    Resolves root-anchored ('//' or '/') paths against both repo_root and
    fuchsia_root so links from vendor/google into fuchsia.git succeed.

    Args:
      target: Link target or JSON config path string.
      source_dir: Directory containing the referencing file.
      repo_root: Optional SCM root of the current repository.
      fuchsia_root: Optional Fuchsia checkout root directory.

    Returns:
      True if any candidate path exists on disk, False otherwise.
    """
    roots = tuple(
        os.path.abspath(r) for r in (repo_root, fuchsia_root) if r is not None
    )
    if target.startswith("/"):
        rel = target[2:] if target.startswith("//") else target[1:]
        return any(os.path.exists(os.path.join(r, rel)) for r in roots) or (
            not roots and os.path.exists(rel)
        )

    candidates = (
        os.path.join(source_dir, target),
        *(os.path.join(r, target) for r in roots),
    )
    return any(os.path.exists(c) for c in candidates)


def _iter_markdown_prose_and_code(
    lines: Sequence[str], start_lineno: int
) -> tuple[
    Sequence[tuple[int, str, str]],
    Sequence[tuple[int, str, Sequence[str]]],
]:
    """Splits Markdown lines into prose lines and fenced code blocks.

    Args:
      lines: Raw Markdown body lines.
      start_lineno: 1-based line number of lines[0].

    Returns:
      A tuple (prose_lines, code_blocks), where prose_lines is a sequence of
      (lineno, raw_line, stripped_inline_code_line) and code_blocks is a
      sequence of (opening_lineno, fence_header, block_lines).
    """
    prose_lines: list[tuple[int, str, str]] = []
    code_blocks: list[tuple[int, str, tuple[str, ...]]] = []

    in_code = False
    fence_ticks = 0
    fence_lineno = start_lineno
    fence_hdr = ""
    cur_block: list[str] = []

    for offset, raw in enumerate(lines):
        lineno = start_lineno + offset
        stripped = raw.strip()
        if (m_fence := _FENCE_RE.match(stripped)) is not None:
            ticks = len(m_fence.group(1))
            rest = m_fence.group(2).strip()
            if not in_code and "`" not in rest:
                in_code = True
                fence_ticks = ticks
                fence_lineno = lineno
                fence_hdr = rest
                cur_block = []
                continue
            if in_code and ticks >= fence_ticks and not rest:
                code_blocks.append((fence_lineno, fence_hdr, tuple(cur_block)))
                in_code = False
                fence_ticks = 0
                cur_block = []
                continue

        if in_code:
            cur_block.append(raw)
        else:
            no_inline = re.sub(r"`+[^`]*`+", "", raw)
            prose_lines.append((lineno, raw, no_inline))

    if in_code and cur_block:
        code_blocks.append((fence_lineno, fence_hdr, tuple(cur_block)))

    return tuple(prose_lines), tuple(code_blocks)


def _check_markdown_links_and_snippets(
    abs_file: str,
    rel_path: str,
    lines: Sequence[str],
    start_lineno: int,
    repo_root: str | None,
    fuchsia_root: str | None,
) -> Sequence[Finding]:
    """Validates Markdown links, local paths, and Jetski snippet invocations.

    Args:
      abs_file: Absolute path to the Markdown file.
      rel_path: Repository-relative path for findings.
      lines: Markdown lines to inspect.
      start_lineno: 1-based line number corresponding to lines[0].
      repo_root: Optional SCM repository root.
      fuchsia_root: Optional Fuchsia checkout root.

    Returns:
      A sequence of Finding dicts with line numbers.
    """
    findings: list[Finding] = []
    source_dir = os.path.dirname(os.path.abspath(abs_file))
    prose_lines, code_blocks = _iter_markdown_prose_and_code(
        lines, start_lineno
    )

    for lineno, raw_line, no_inline in prose_lines:
        if (m_local := _LOCAL_MACHINE_PATH_RE.search(no_inline)) is not None:
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=lineno,
                    level="error",
                    message=(
                        f'Machine-local path "{m_local.group(0)}" is not'
                        " portable; use repository-relative paths instead."
                    ),
                )
            )

        targets: list[str] = []
        for m_link in _INLINE_CODE_OR_LINK_RE.finditer(raw_line):
            if m_link.group(1) is not None:
                targets.append(m_link.group(1))
        if (ref_match := _REF_LINK_DEF_RE.match(raw_line)) is not None:
            targets.append(ref_match.group(1))

        for raw_target in targets:
            parts = raw_target.strip().split()
            if not parts:
                continue
            raw_url = parts[0]
            if raw_url.startswith("<") and raw_url.endswith(">"):
                target = raw_url[1:-1].strip()
            else:
                target = raw_url
            if target.startswith("file:"):
                findings.append(
                    Finding(
                        filepath=rel_path,
                        line=lineno,
                        level="error",
                        message=(
                            f'Invalid file URI link "{target}": RFC 8089 parses'
                            " the authority segment as a hostname. Use a"
                            ' relative Markdown path (e.g. "references/...").'
                        ),
                    )
                )
                continue

            clean_target = target.split("#", 1)[0]
            if (
                not clean_target
                or clean_target.startswith(_SKIP_LINK_SCHEMES)
                or "{" in clean_target
                or "}" in clean_target
            ):
                continue

            if (
                repo_root is not None or fuchsia_root is not None
            ) and not _resolve_local_target(
                clean_target, source_dir, repo_root, fuchsia_root
            ):
                findings.append(
                    Finding(
                        filepath=rel_path,
                        line=lineno,
                        level="error",
                        message=(
                            f'Broken local link "{raw_target}": target file'
                            " does not exist in the checkout."
                        ),
                    )
                )

    # Check fenced code blocks for invalid Jetski API / CLI examples (skipping
    # blocks explicitly marked as negative examples).
    for fence_lineno, _, block_lines in code_blocks:
        block_text = "\n".join(block_lines)
        if _NEGATIVE_MARKER_RE.search(block_text):
            continue
        for idx, code_line in enumerate(block_lines):
            code_lineno = fence_lineno + 1 + idx
            for m_tool in _CALL_TOOL_SNIPPET_RE.finditer(code_line):
                tool_name = m_tool.group(1)
                if tool_name not in _ALLOWED_CALL_TOOLS:
                    allowed_str = ", ".join(sorted(_ALLOWED_CALL_TOOLS))
                    findings.append(
                        Finding(
                            filepath=rel_path,
                            line=code_lineno,
                            level="error",
                            message=(
                                f'call_tool("{tool_name}", ...) is invalid and'
                                " raises ToolError at runtime. Allowed"
                                f" call_tool names: {allowed_str}."
                            ),
                        )
                    )
            if re.search(r"(?:^|\s)fx\s+workflow\b", code_line):
                findings.append(
                    Finding(
                        filepath=rel_path,
                        line=code_lineno,
                        level="error",
                        message=(
                            '"fx workflow" is not a valid Fuchsia CLI command;'
                            " invoke saved workflows via /<workflow-name> or"
                            " the run_workflow tool."
                        ),
                    )
                )

    return tuple(findings)


def _lint_skill_semantic(
    skill_file: str,
    repo_root: str | None,
    fuchsia_root: str | None,
) -> Sequence[Finding]:
    """Runs semantic, structural, and link lint rules on a SKILL.md file.

    Args:
      skill_file: Path to SKILL.md.
      repo_root: Optional SCM repository root.
      fuchsia_root: Optional Fuchsia checkout root.

    Returns:
      A sequence of Finding dicts.
    """
    findings: list[Finding] = []
    abs_file = os.path.abspath(skill_file)
    rel_path = _rel_filepath(abs_file, repo_root)

    try:
        with open(abs_file, "r", encoding="utf-8") as f:
            raw_content = f.read()
    except OSError as e:
        findings.append(
            Finding(
                filepath=rel_path,
                line=1,
                level="error",
                message=f"Could not read SKILL.md: {e}",
            )
        )
        return tuple(findings)

    try:
        meta, body = _parse_skill_frontmatter(raw_content)
    except ValueError as e:
        # Already surfaced as an error by lint_single_skill; include here if
        # _lint_skill_semantic is invoked standalone via --lint-json.
        findings.append(
            Finding(
                filepath=rel_path,
                line=1,
                level="error",
                message=str(e),
            )
        )
        return tuple(findings)

    parts = raw_content.split("---", 2)
    fm_line_count = parts[1].count("\n") + 2
    if body.startswith("\r\n"):
        body_after_delim = body[2:]
    elif body.startswith("\n"):
        body_after_delim = body[1:]
    else:
        body_after_delim = body
    body_lines = body_after_delim.split("\n")

    raw_name = meta.get("name")
    name = raw_name.strip() if isinstance(raw_name, str) else ""
    raw_desc = meta.get("description")
    description = raw_desc.strip() if isinstance(raw_desc, str) else ""
    skill_dir = os.path.dirname(abs_file)
    folder_name = os.path.basename(skill_dir)

    # 1. Singular .agent/ directory check.
    if "/.agent/" in f"/{rel_path}" and not rel_path.startswith(
        _LEGACY_SINGULAR_DOT_AGENT_PREFIXES
    ):
        findings.append(
            Finding(
                filepath=rel_path,
                line=1,
                level="error",
                message=(
                    f'Skill path "{rel_path}" uses singular ".agent/"; use'
                    ' plural ".agents/" so Jetski discovers it.'
                ),
            )
        )

    # 2. Folder name vs YAML name & naming conventions (allowing nested
    # subsystem suites under skills/<topic>/<task>[/<lang>]/SKILL.md).
    rel_parts = tuple(rel_path.split("/"))
    skills_indices = tuple(
        i for i, seg in enumerate(rel_parts[:-1]) if seg == "skills"
    )
    suite_dirs = (
        rel_parts[skills_indices[-1] + 1 : -1]
        if skills_indices
        else (folder_name,)
    )
    if name and folder_name and not folder_name.startswith("tmp"):
        if len(suite_dirs) >= 2:
            if (
                underscore_dir := next(
                    (d for d in suite_dirs if "_" in d), None
                )
            ) is not None:
                findings.append(
                    Finding(
                        filepath=rel_path,
                        line=2,
                        level="warning",
                        message=(
                            f'Nested skill folder "{underscore_dir}" uses'
                            " underscores; prefer kebab-case."
                        ),
                    )
                )
        elif folder_name.replace("_", "-") != name:
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=2,
                    level="warning",
                    message=(
                        f'Enclosing folder "{folder_name}" does not match YAML'
                        f' skill name "{name}".'
                    ),
                )
            )
        elif folder_name != name:
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=2,
                    level="warning",
                    message=(
                        f'Skill folder "{folder_name}" uses underscores; prefer'
                        f' kebab-case matching "{name}".'
                    ),
                )
            )

    if name and _FILLER_NAME_RE.search(name):
        findings.append(
            Finding(
                filepath=rel_path,
                line=2,
                level="warning",
                message=(
                    f'Skill name "{name}" contains filler words (skill, guide,'
                    " playbook, how-to, checklist, helper, utils); prefer"
                    " concise <domain>-<action> or <verb>-<noun> naming."
                ),
            )
        )

    # 3. Progressive Disclosure & trigger contract in description.
    if description:
        if _BINARY_IN_DESC_RE.search(description):
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=3,
                    level="error",
                    message=(
                        'Frontmatter "description" contains executable binary'
                        " or build paths, violating Progressive Disclosure."
                        " Move command invocations into the SKILL.md body."
                    ),
                )
            )
        if not _POSITIVE_TRIGGER_RE.search(description):
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=3,
                    level="warning",
                    message=(
                        'Frontmatter "description" should include an explicit'
                        ' positive trigger (e.g. "Use when...").'
                    ),
                )
            )
        if name != "review" and not _NEGATIVE_TRIGGER_RE.search(description):
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=3,
                    level="warning",
                    message=(
                        'Frontmatter "description" should include an explicit'
                        ' negative scope boundary (e.g. "Don\'t use for...") to'
                        " prevent co-triggering collisions."
                    ),
                )
            )

    # 4. Size / view_file truncation limits & auxiliary skill dir files.
    line_count = len(raw_content.splitlines())
    byte_count = len(raw_content.encode("utf-8"))
    if line_count > _MAX_SKILL_LINES or byte_count > _MAX_SKILL_BYTES:
        level = "warning" if rel_path in _LEGACY_OVERSIZED_SKILLS else "error"
        findings.append(
            Finding(
                filepath=rel_path,
                line=1,
                level=level,
                message=(
                    f"SKILL.md is {line_count} lines ({byte_count} bytes),"
                    f" exceeding the {_MAX_SKILL_LINES}-line /"
                    f" {_MAX_SKILL_BYTES}-byte view_file ceiling. Offload"
                    " detailed rubrics and templates to references/*.md."
                ),
            )
        )
    elif line_count > _WARN_SKILL_LINES or byte_count > _WARN_SKILL_BYTES:
        findings.append(
            Finding(
                filepath=rel_path,
                line=1,
                level="warning",
                message=(
                    f"SKILL.md is {line_count} lines ({byte_count} bytes),"
                    f" exceeding the {_WARN_SKILL_LINES}-line /"
                    f" {_WARN_SKILL_BYTES}-byte soft limit. Consider moving"
                    " detailed material to references/*.md."
                ),
            )
        )

    for extra_doc in ("README.md", "CHANGELOG.md"):
        if os.path.isfile(os.path.join(skill_dir, extra_doc)):
            findings.append(
                Finding(
                    filepath=rel_path,
                    line=1,
                    level="warning",
                    message=(
                        f'Skill directory contains "{extra_doc}"; agents only'
                        " load SKILL.md and linked files under references/."
                    ),
                )
            )

    # 5. Links, machine-local paths, and Jetski API snippets.
    findings.extend(
        _check_markdown_links_and_snippets(
            abs_file,
            rel_path,
            body_lines,
            fm_line_count,
            repo_root,
            fuchsia_root,
        )
    )

    return tuple(findings)


def _validate_args_schema(
    args_schema: Any, rel_manifest: str
) -> tuple[Set[str] | None, bool, Sequence[Finding]]:
    """Validates workflow.json 'args' JSON Schema structure.

    Args:
      args_schema: Parsed 'args' value from workflow.json.
      rel_manifest: Relative path to workflow.json for findings.

    Returns:
      A tuple (declared_props, has_safe_dry_run, findings), where
      declared_props is the set of property names (or None if 'args' is
      invalid/unspecified) and has_safe_dry_run indicates whether 'dry_run'
      defaults to True.
    """
    findings: list[Finding] = []
    if not isinstance(args_schema, dict):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message='"args" in workflow.json must be a JSON Schema object.',
            )
        )
        return None, False, tuple(findings)

    if args_schema.get("type") != "object":
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=(
                    '"args.type" in workflow.json must be "object" so'
                    " run_workflow can validate structured arguments."
                ),
            )
        )

    props = args_schema.get("properties", {})
    if not isinstance(props, dict):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message='"args.properties" in workflow.json must be an object.',
            )
        )
        return None, False, tuple(findings)

    for prop_name, prop_spec in props.items():
        if not isinstance(prop_spec, dict):
            findings.append(
                Finding(
                    filepath=rel_manifest,
                    line=1,
                    level="error",
                    message=(
                        f'Property "{prop_name}" in workflow.json'
                        " args.properties must be a schema object."
                    ),
                )
            )
            continue
        desc = prop_spec.get("description")
        if not isinstance(desc, str) or not desc.strip():
            findings.append(
                Finding(
                    filepath=rel_manifest,
                    line=1,
                    level="warning",
                    message=(
                        f'Argument "{prop_name}" in workflow.json'
                        ' args.properties is missing a "description" (agents'
                        " rely on manifest arg descriptions, not README.md)."
                    ),
                )
            )

    required = args_schema.get("required", [])
    if not isinstance(required, list) or not all(
        isinstance(r, str) for r in required
    ):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=(
                    '"args.required" in workflow.json must be an array of'
                    " property names."
                ),
            )
        )
    else:
        for req_key in required:
            if req_key not in props:
                findings.append(
                    Finding(
                        filepath=rel_manifest,
                        line=1,
                        level="error",
                        message=(
                            f'Required argument "{req_key}" in workflow.json is'
                            ' not defined in "args.properties".'
                        ),
                    )
                )

    dry_run_spec = props.get("dry_run")
    has_safe_dry_run = (
        isinstance(dry_run_spec, dict) and dry_run_spec.get("default") is True
    )
    return frozenset(props.keys()), has_safe_dry_run, tuple(findings)


def _extract_cmd_expr_text(
    expr: ast.AST,
    var_cmds: Mapping[str, str] | None = None,
) -> str:
    """Extracts static command text from a string, f-string, or sequence AST node.

    Args:
      expr: Python AST expression representing a CLI command or argument list.
      var_cmds: Optional mapping of local variable names to extracted command
        strings.

    Returns:
      Space-joined static string tokens from expr, or an empty string.
    """
    if isinstance(expr, ast.Constant) and isinstance(expr.value, str):
        return expr.value
    if isinstance(expr, ast.JoinedStr):
        return "".join(
            v.value
            for v in expr.values
            if isinstance(v, ast.Constant) and isinstance(v.value, str)
        )
    if isinstance(expr, (ast.List, ast.Tuple)):
        return " ".join(
            tok
            for elt in expr.elts
            if (tok := _extract_cmd_expr_text(elt, var_cmds))
        )
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Add):
        left = _extract_cmd_expr_text(expr.left, var_cmds)
        right = _extract_cmd_expr_text(expr.right, var_cmds)
        return " ".join(part for part in (left, right) if part)
    if (
        isinstance(expr, ast.Name)
        and var_cmds is not None
        and expr.id in var_cmds
    ):
        return var_cmds[expr.id]
    return ""


def _is_cli_runner_call(node: ast.Call) -> bool:
    """Checks whether an AST Call node invokes a subprocess or CLI runner.

    Args:
      node: AST Call node to inspect.

    Returns:
      True if the call targets a known subprocess/OS/CLI execution function.
    """
    if isinstance(node.func, ast.Attribute) and isinstance(
        node.func.value, ast.Name
    ):
        allowed = _CLI_RUNNER_MODULE_ATTRS.get(node.func.value.id)
        return allowed is not None and node.func.attr in allowed
    if isinstance(node.func, ast.Name):
        return node.func.id in _CLI_RUNNER_DIRECT_NAMES
    return False


def _lint_workflow_script(
    script_path: str,
    rel_script: str,
    declared_args: Set[str] | None,
    has_safe_dry_run: bool,
) -> Sequence[Finding]:
    """Lints a dynamic workflow Python entry script using AST inspection.

    Checks syntax, call_tool() allowlist, agent(workspace=, model=) literals,
    args.get() / args[...] key sync against workflow.json, and unguarded
    external mutation commands.

    Args:
      script_path: Absolute path to workflow.py (or custom entry script).
      rel_script: Repository-relative path for findings.
      declared_args: Set of argument names declared in workflow.json, or None.
      has_safe_dry_run: Whether workflow.json declares dry_run defaulting to
        True.

    Returns:
      A sequence of Finding dicts.
    """
    findings: list[Finding] = []
    try:
        with open(script_path, "r", encoding="utf-8") as f:
            source = f.read()
    except OSError as e:
        findings.append(
            Finding(
                filepath=rel_script,
                line=1,
                level="error",
                message=f"Could not read workflow entry script: {e}",
            )
        )
        return tuple(findings)

    try:
        tree = ast.parse(source, filename=rel_script)
    except SyntaxError as e:
        findings.append(
            Finding(
                filepath=rel_script,
                line=e.lineno if e.lineno is not None else 1,
                level="error",
                message=f"Python syntax error in workflow script: {e.msg}",
            )
        )
        return tuple(findings)

    var_cmds: dict[str, str] = {}
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and node.value is not None:
            text = _extract_cmd_expr_text(node.value, var_cmds)
            if text:
                for target in node.targets:
                    if isinstance(target, ast.Name):
                        prev = var_cmds.get(target.id)
                        var_cmds[target.id] = (
                            f"{prev} {text}" if prev is not None else text
                        )
        elif (
            isinstance(node, ast.AnnAssign)
            and isinstance(node.target, ast.Name)
            and node.value is not None
        ):
            text = _extract_cmd_expr_text(node.value, var_cmds)
            if text:
                prev = var_cmds.get(node.target.id)
                var_cmds[node.target.id] = (
                    f"{prev} {text}" if prev is not None else text
                )

    used_args: dict[str, int] = {}
    mutation_lineno: int | None = None

    for node in ast.walk(tree):
        # Detect args["key"] subscript access.
        if (
            isinstance(node, ast.Subscript)
            and isinstance(node.value, ast.Name)
            and node.value.id == "args"
            and isinstance(node.slice, ast.Constant)
            and isinstance(node.slice.value, str)
        ):
            used_args.setdefault(node.slice.value, node.lineno)

        if not isinstance(node, ast.Call):
            continue

        # Detect external mutation commands passed to subprocess/CLI runners.
        if mutation_lineno is None and _is_cli_runner_call(node):
            arg_texts = tuple(
                t
                for a in node.args
                if (t := _extract_cmd_expr_text(a, var_cmds))
            )
            candidates = list(arg_texts)
            if len(arg_texts) > 1:
                candidates.append(" ".join(arg_texts))
            for kw in node.keywords:
                if kw.arg in ("args", "cmd"):
                    kw_text = _extract_cmd_expr_text(kw.value, var_cmds)
                    if kw_text:
                        candidates.append(kw_text)
            if any(_MUTATION_CMD_RE.search(cand) for cand in candidates):
                mutation_lineno = node.lineno

        # Detect args.get("key", ...).
        if (
            isinstance(node.func, ast.Attribute)
            and isinstance(node.func.value, ast.Name)
            and node.func.value.id == "args"
            and node.func.attr == "get"
            and node.args
            and isinstance(node.args[0], ast.Constant)
            and isinstance(node.args[0].value, str)
        ):
            used_args.setdefault(node.args[0].value, node.lineno)

        # Detect call_tool("<tool>", ...).
        if isinstance(node.func, ast.Name) and node.func.id == "call_tool":
            if (
                node.args
                and isinstance(node.args[0], ast.Constant)
                and isinstance(node.args[0].value, str)
            ):
                tool_name = node.args[0].value
                if tool_name not in _ALLOWED_CALL_TOOLS:
                    allowed_str = ", ".join(sorted(_ALLOWED_CALL_TOOLS))
                    findings.append(
                        Finding(
                            filepath=rel_script,
                            line=node.lineno,
                            level="error",
                            message=(
                                f'call_tool("{tool_name}", ...) is disallowed'
                                " in run_workflow and raises ToolError."
                                f" Allowed tools: {allowed_str}."
                            ),
                        )
                    )

        # Detect agent(..., workspace="...", model="...").
        if isinstance(node.func, ast.Name) and node.func.id == "agent":
            for kw in node.keywords:
                allowed = (
                    _AGENT_KW_ALLOWED.get(kw.arg)
                    if kw.arg is not None
                    else None
                )
                if (
                    allowed is not None
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                    and kw.value.value not in allowed
                ):
                    allowed_str = ", ".join(sorted(allowed))
                    findings.append(
                        Finding(
                            filepath=rel_script,
                            line=kw.lineno,
                            level="error",
                            message=(
                                f'Invalid agent {kw.arg}="{kw.value.value}".'
                                f" Allowed values: {allowed_str}."
                            ),
                        )
                    )

    if used_args:
        if declared_args is None:
            first_key, first_line = next(iter(used_args.items()))
            findings.append(
                Finding(
                    filepath=rel_script,
                    line=first_line,
                    level="warning",
                    message=(
                        f'Workflow reads args["{first_key}"], but'
                        ' workflow.json does not define an "args" schema.'
                    ),
                )
            )
        else:
            for key, lineno in sorted(
                used_args.items(), key=lambda item: item[1]
            ):
                if key not in declared_args:
                    findings.append(
                        Finding(
                            filepath=rel_script,
                            line=lineno,
                            level="warning",
                            message=(
                                f'Argument "{key}" read from args in'
                                " workflow.py is not declared in workflow.json"
                                ' "args.properties".'
                            ),
                        )
                    )

    if mutation_lineno is not None and not has_safe_dry_run:
        findings.append(
            Finding(
                filepath=rel_script,
                line=mutation_lineno,
                level="warning",
                message=(
                    "Workflow invokes external mutation commands via Python"
                    " (which bypasses Jetski's per-tool confirmation prompt)"
                    ' without a "dry_run" boolean property defaulting to true'
                    " in workflow.json."
                ),
            )
        )

    return tuple(findings)


def _lint_workflow_dir(
    workflow_dir: str,
    repo_root: str | None,
    fuchsia_root: str | None,
) -> Sequence[Finding]:
    """Lints a saved dynamic workflow directory (.agents/workflows/<name>/).

    Validates workflow.json manifest schema, entry script AST rules, argument
    synchronization, and optional README.md anti-patterns.

    Args:
      workflow_dir: Path to the workflow directory.
      repo_root: Optional SCM repository root.
      fuchsia_root: Optional Fuchsia checkout root.

    Returns:
      A sequence of Finding dicts across files in the workflow directory.
    """
    findings: list[Finding] = []
    abs_dir = os.path.abspath(workflow_dir)
    dir_name = os.path.basename(abs_dir)
    manifest_path = os.path.join(abs_dir, "workflow.json")
    rel_manifest = _rel_filepath(manifest_path, repo_root)

    if not os.path.isfile(manifest_path):
        try:
            rem_entries = tuple(os.listdir(abs_dir))
        except OSError as e:
            return (
                Finding(
                    filepath=_rel_filepath(abs_dir, repo_root),
                    line=1,
                    level="error",
                    message=f"Could not read workflow directory: {e}",
                ),
            )
        if all(e.startswith(".") or e == "__pycache__" for e in rem_entries):
            return ()
        findings.append(
            Finding(
                filepath=_rel_filepath(abs_dir, repo_root),
                line=1,
                level="error",
                message=(
                    f'Saved workflow directory "{dir_name}" is missing required'
                    ' "workflow.json" manifest.'
                ),
            )
        )
        return tuple(findings)

    try:
        with open(manifest_path, "r", encoding="utf-8") as f:
            manifest = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=f"Invalid JSON in workflow.json: {e}",
            )
        )
        return tuple(findings)

    if not isinstance(manifest, dict):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message="workflow.json root must be a JSON object.",
            )
        )
        return tuple(findings)

    unknown_keys = set(manifest.keys()) - _ALLOWED_WORKFLOW_MANIFEST_KEYS
    if unknown_keys:
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=(
                    "Unexpected key(s) in workflow.json: "
                    + ", ".join(sorted(unknown_keys))
                    + ". Allowed keys: "
                    + ", ".join(sorted(_ALLOWED_WORKFLOW_MANIFEST_KEYS))
                    + "."
                ),
            )
        )

    raw_name = manifest.get("name")
    wf_name = raw_name.strip() if isinstance(raw_name, str) else ""
    if not wf_name or not re.fullmatch(r"^[a-z0-9-]+$", wf_name):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=(
                    'workflow.json "name" must be a non-empty kebab-case'
                    " string ([a-z0-9-])."
                ),
            )
        )
    elif wf_name != dir_name and not dir_name.startswith("tmp"):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=(
                    f'workflow.json name "{wf_name}" must match enclosing'
                    f' directory "{dir_name}".'
                ),
            )
        )

    raw_library = manifest.get("library", False)
    if not isinstance(raw_library, bool):
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message='workflow.json "library" must be a boolean.',
            )
        )
        is_library = False
    else:
        is_library = raw_library

    raw_desc = manifest.get("description")
    desc = raw_desc.strip() if isinstance(raw_desc, str) else ""
    if not desc:
        findings.append(
            Finding(
                filepath=rel_manifest,
                line=1,
                level="error",
                message=(
                    'workflow.json "description" cannot be empty (it is the'
                    " only workflow summary injected into agent context)."
                ),
            )
        )

    declared_args: Set[str] | None
    if "args" in manifest:
        declared_args, has_safe_dry_run, arg_findings = _validate_args_schema(
            manifest["args"], rel_manifest
        )
        findings.extend(arg_findings)
    else:
        declared_args = None
        has_safe_dry_run = False

    if not is_library:
        if "entry" in manifest:
            raw_entry_val = manifest["entry"]
            raw_entry = (
                raw_entry_val.strip() if isinstance(raw_entry_val, str) else ""
            )
        else:
            raw_entry = "workflow.py"
        norm_entry = os.path.normpath(raw_entry) if raw_entry else ""
        if not raw_entry:
            findings.append(
                Finding(
                    filepath=rel_manifest,
                    line=1,
                    level="error",
                    message='workflow.json "entry" must be a non-empty string.',
                )
            )
        elif (
            os.path.isabs(raw_entry)
            or norm_entry == ".."
            or norm_entry.startswith(f"..{os.sep}")
            or norm_entry.startswith("../")
        ):
            findings.append(
                Finding(
                    filepath=rel_manifest,
                    line=1,
                    level="error",
                    message=(
                        f'workflow.json entry "{raw_entry}" must stay inside'
                        " the workflow directory."
                    ),
                )
            )
        else:
            entry_abs = os.path.join(abs_dir, norm_entry)
            if not os.path.isfile(entry_abs):
                findings.append(
                    Finding(
                        filepath=rel_manifest,
                        line=1,
                        level="error",
                        message=(
                            f'Workflow entry script "{raw_entry}" does not'
                            " exist."
                        ),
                    )
                )
            elif entry_abs.endswith(".py"):
                findings.extend(
                    _lint_workflow_script(
                        entry_abs,
                        _rel_filepath(entry_abs, repo_root),
                        declared_args,
                        has_safe_dry_run,
                    )
                )

    readme_path = os.path.join(abs_dir, "README.md")
    if os.path.isfile(readme_path):
        rel_readme = _rel_filepath(readme_path, repo_root)
        try:
            with open(readme_path, "r", encoding="utf-8") as f:
                readme_lines = f.read().splitlines()
        except OSError as e:
            findings.append(
                Finding(
                    filepath=rel_readme,
                    line=1,
                    level="error",
                    message=f"Could not read workflow README.md: {e}",
                )
            )
            return tuple(findings)

        for idx, line in enumerate(readme_lines, start=1):
            if _ARG_TABLE_HEADER_RE.match(line.strip()):
                findings.append(
                    Finding(
                        filepath=rel_readme,
                        line=idx,
                        level="warning",
                        message=(
                            "Workflow README.md duplicates the arguments schema"
                            " table (agents only see workflow.json, so README"
                            " argument tables drift). Put argument details in"
                            " workflow.json instead."
                        ),
                    )
                )
                break

        if wf_name:
            for idx, line in enumerate(readme_lines, start=1):
                if re.search(rf"/run-workflow\s+{re.escape(wf_name)}\b", line):
                    findings.append(
                        Finding(
                            filepath=rel_readme,
                            line=idx,
                            level="warning",
                            message=(
                                f'Saved workflow "{wf_name}" exposes the'
                                f' "/{wf_name}" slash command directly rather'
                                f' than "/run-workflow {wf_name}".'
                            ),
                        )
                    )
                    break

        findings.extend(
            _check_markdown_links_and_snippets(
                readme_path,
                rel_readme,
                readme_lines,
                1,
                repo_root,
                fuchsia_root,
            )
        )

    return tuple(findings)


def _lint_discovery_json(
    json_path: str,
    repo_root: str | None,
    fuchsia_root: str | None,
) -> Sequence[Finding]:
    """Lints a Jetski JSON discovery config (.agents/{skills,workflows,...}.json).

    Validates allowed keys, entry structure, local path existence, and dual
    path registration in vendor/google/.agents/{skills,workflows}.json.

    Args:
      json_path: Path to the JSON configuration file.
      repo_root: Optional SCM repository root.
      fuchsia_root: Optional Fuchsia checkout root.

    Returns:
      A sequence of Finding dicts.
    """
    findings: list[Finding] = []
    abs_json = os.path.abspath(json_path)
    rel_json = _rel_filepath(abs_json, repo_root)
    # Relative paths in .agents/*.json resolve against the parent of .agents/
    # first, falling back to repo_root / fuchsia_root.
    dot_dir = os.path.dirname(abs_json)
    config_base = (
        os.path.dirname(dot_dir)
        if os.path.basename(dot_dir) in (".agents", "_agents")
        else dot_dir
    )

    try:
        with open(abs_json, "r", encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        findings.append(
            Finding(
                filepath=rel_json,
                line=1,
                level="error",
                message=f"Invalid JSON in discovery config: {e}",
            )
        )
        return tuple(findings)

    if not isinstance(data, dict):
        findings.append(
            Finding(
                filepath=rel_json,
                line=1,
                level="error",
                message="Discovery config root must be a JSON object.",
            )
        )
        return tuple(findings)

    unknown = set(data.keys()) - _ALLOWED_DISCOVERY_CONFIG_KEYS
    if unknown:
        findings.append(
            Finding(
                filepath=rel_json,
                line=1,
                level="error",
                message=(
                    "Unexpected top-level key(s) in discovery config: "
                    + ", ".join(sorted(unknown))
                    + "."
                ),
            )
        )

    entry_paths: list[str] = []
    for section in ("entries", "inherits"):
        if section not in data:
            continue
        items = data[section]
        if not isinstance(items, list):
            findings.append(
                Finding(
                    filepath=rel_json,
                    line=1,
                    level="error",
                    message=f'"{section}" must be an array of objects.',
                )
            )
            continue
        for item in items:
            if not isinstance(item, dict) or not isinstance(
                item.get("path"), str
            ):
                findings.append(
                    Finding(
                        filepath=rel_json,
                        line=1,
                        level="error",
                        message=(
                            f'Each item in "{section}" must be an object with a'
                            ' string "path" field.'
                        ),
                    )
                )
                continue
            p = item["path"].strip()
            if not p:
                findings.append(
                    Finding(
                        filepath=rel_json,
                        line=1,
                        level="error",
                        message=f'Empty "path" in "{section}".',
                    )
                )
                continue
            if section == "entries":
                entry_paths.append(p)
            if p.startswith(("~", "/google/", "google3/", "configs/")):
                continue
            norm_p = p.replace("\\", "/").lstrip("/")
            if norm_p.startswith("vendor/"):
                p_parts = norm_p.split("/")
                vendor_rel = (
                    os.path.join(p_parts[0], p_parts[1])
                    if len(p_parts) >= 2
                    else p_parts[0]
                )
                search_roots = (
                    config_base,
                    *(
                        os.path.abspath(r)
                        for r in (repo_root, fuchsia_root)
                        if r is not None
                    ),
                )
                if not any(
                    os.path.isdir(os.path.join(r, vendor_rel))
                    for r in search_roots
                ):
                    continue
            if not _resolve_local_target(
                p, config_base, repo_root, fuchsia_root
            ):
                findings.append(
                    Finding(
                        filepath=rel_json,
                        line=1,
                        level="error",
                        message=(
                            f'Configured {section} path "{p}" does not exist in'
                            " the checkout."
                        ),
                    )
                )

    # Check vendor/google dual-path convention when inherited by //.agents/.
    norm_abs = abs_json.replace(os.sep, "/")
    if norm_abs.endswith(
        (
            "/vendor/google/.agents/skills.json",
            "/vendor/google/.agents/workflows.json",
        )
    ):
        entry_set = frozenset(entry_paths)
        for p in entry_paths:
            if p in (".", "..") or p.startswith(
                (
                    "./",
                    "../",
                    "/",
                    "~",
                    "vendor/google/",
                    "google3/",
                    "configs/",
                )
            ):
                continue
            paired = f"vendor/google/{p}"
            if paired not in entry_set:
                findings.append(
                    Finding(
                        filepath=rel_json,
                        line=1,
                        level="warning",
                        message=(
                            f'Entry "{p}" in vendor/google discovery config is'
                            f' missing paired "{paired}" entry needed when'
                            " inherited from the Fuchsia checkout root."
                        ),
                    )
                )

    return tuple(findings)


def _find_enclosing_workflow_dir(path: str) -> str | None:
    """Finds the enclosing saved workflow directory for a file path, if any.

    Args:
      path: File or directory path.

    Returns:
      Absolute path to the workflow directory (<root>/.agents/workflows/<name>
      or a directory containing workflow.json), or None.
    """
    abs_path = os.path.abspath(path)
    cur = os.path.dirname(abs_path) if os.path.isfile(abs_path) else abs_path

    if os.path.isfile(os.path.join(cur, "workflow.json")):
        return cur

    parts = cur.split(os.sep)
    for idx in range(len(parts) - 2):
        if (
            parts[idx] in (".agents", "_agents")
            and parts[idx + 1] == "workflows"
        ):
            return os.sep.join(parts[: idx + 3])
    return None


def _lint_customization_paths(
    paths: Collection[str],
    repo_root: str | None,
    fuchsia_root: str | None,
) -> Sequence[Finding]:
    """Runs semantic and structural lint rules across skills, workflows, and JSON configs.

    Args:
      paths: Collection of file or directory paths to lint.
      repo_root: Optional SCM repository root for relative path reporting.
      fuchsia_root: Optional Fuchsia checkout root for cross-repo path
        resolution.

    Returns:
      A sequence of Finding dicts.
    """
    findings: list[Finding] = []
    seen_skills: set[str] = set()
    seen_workflows: set[str] = set()
    seen_jsons: set[str] = set()

    for raw_path in paths:
        abs_p = os.path.abspath(raw_path)

        wf_dir = _find_enclosing_workflow_dir(abs_p)
        if wf_dir is not None:
            if wf_dir not in seen_workflows and os.path.isdir(wf_dir):
                seen_workflows.add(wf_dir)
                findings.extend(
                    _lint_workflow_dir(wf_dir, repo_root, fuchsia_root)
                )
            continue

        if os.path.isfile(abs_p):
            base = os.path.basename(abs_p)
            parent_base = os.path.basename(os.path.dirname(abs_p))
            if base == "SKILL.md" and abs_p not in seen_skills:
                seen_skills.add(abs_p)
                findings.extend(
                    _lint_skill_semantic(abs_p, repo_root, fuchsia_root)
                )
            elif (
                base in _DISCOVERY_JSON_FILENAMES
                and parent_base in (".agents", "_agents")
                and abs_p not in seen_jsons
            ):
                seen_jsons.add(abs_p)
                findings.extend(
                    _lint_discovery_json(abs_p, repo_root, fuchsia_root)
                )
        elif os.path.isdir(abs_p):
            skill_md = os.path.join(abs_p, "SKILL.md")
            if os.path.isfile(skill_md):
                if skill_md not in seen_skills:
                    seen_skills.add(skill_md)
                    findings.extend(
                        _lint_skill_semantic(skill_md, repo_root, fuchsia_root)
                    )
            else:
                try:
                    entries = sorted(os.listdir(abs_p))
                except OSError as e:
                    findings.append(
                        Finding(
                            filepath=_rel_filepath(abs_p, repo_root),
                            line=1,
                            level="error",
                            message=f"Could not read directory '{abs_p}': {e}",
                        )
                    )
                    continue
                for entry in entries:
                    subdir = os.path.join(abs_p, entry)
                    if not os.path.isdir(subdir):
                        continue
                    sub_wf = _find_enclosing_workflow_dir(subdir)
                    if sub_wf is not None:
                        if sub_wf not in seen_workflows and os.path.isdir(
                            sub_wf
                        ):
                            seen_workflows.add(sub_wf)
                            findings.extend(
                                _lint_workflow_dir(
                                    sub_wf, repo_root, fuchsia_root
                                )
                            )
                        continue
                    sub_skill = os.path.join(subdir, "SKILL.md")
                    if (
                        os.path.isfile(sub_skill)
                        and sub_skill not in seen_skills
                    ):
                        seen_skills.add(sub_skill)
                        findings.extend(
                            _lint_skill_semantic(
                                sub_skill, repo_root, fuchsia_root
                            )
                        )

    return tuple(findings)


def do_lint(
    root_path: str,
    fixit: bool,
    stdout_mode: bool,
    suggest_fix_in_json_mode: bool = False,
) -> tuple[int, list[Finding]]:
    """Orchestrates the formatting/metadata linting process for a file or directory.

    Args:
      root_path: Path to a file or directory to lint.
      fixit: Whether to apply fixes in-place.
      stdout_mode: Whether to output fixed content to stdout.
      suggest_fix_in_json_mode: Whether to return findings as JSON.

    Returns:
      A tuple of (max_exit_code, all_findings).
    """
    if os.path.isfile(root_path):
        return lint_single_skill(
            root_path, fixit, stdout_mode, suggest_fix_in_json_mode
        )
    elif os.path.exists(os.path.join(root_path, "SKILL.md")):
        return lint_single_skill(
            os.path.join(root_path, "SKILL.md"),
            fixit,
            stdout_mode,
            suggest_fix_in_json_mode,
        )
    else:
        all_findings: list[Finding] = []
        try:
            subdirs = [
                os.path.join(root_path, d)
                for d in os.listdir(root_path)
                if os.path.isdir(os.path.join(root_path, d))
            ]
        except OSError as e:
            msg = f"Error reading directory '{root_path}': {e}"
            if suggest_fix_in_json_mode:
                all_findings.append(
                    Finding(
                        filepath=os.path.relpath(root_path),
                        message=msg,
                        level="error",
                    )
                )
            else:
                logging.error(msg)
            return 0 if suggest_fix_in_json_mode else 1, all_findings

        max_code = 0
        for subdir in subdirs:
            if os.path.exists(os.path.join(subdir, "SKILL.md")):
                code, findings = lint_single_skill(
                    os.path.join(subdir, "SKILL.md"),
                    fixit,
                    stdout_mode,
                    suggest_fix_in_json_mode,
                )
                all_findings.extend(findings)
                max_code = max(max_code, code)

        return max_code, all_findings


def main() -> None:
    """Main entry point for the skill and workflow linter.

    Parses command line arguments, configures logging, and runs the
    linting process on specified paths.
    """
    parser = argparse.ArgumentParser(
        description="Skill, dynamic workflow, and .agents config validator."
    )
    parser.add_argument(
        "--fixit",
        action="store_true",
        help="Suggest and autofix validation errors.",
    )
    parser.add_argument(
        "--suggest-fix",
        action="store_true",
        help="Applies fixes and outputs the result directly to stdout.",
    )
    parser.add_argument(
        "--suggest-fix-in-json",
        action="store_true",
        help="Output formatting and frontmatter findings as a JSON array.",
    )
    parser.add_argument(
        "--lint-json",
        action="store_true",
        help=(
            "Run semantic skill, workflow, and .agents JSON lint checks and"
            " output findings as a JSON array."
        ),
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help=(
            "Also run semantic skill, workflow, and .agents JSON lint checks"
            " in CLI mode."
        ),
    )
    parser.add_argument(
        "--repo-root",
        default=None,
        help="SCM repository root for relative paths and link resolution.",
    )
    parser.add_argument(
        "--fuchsia-root",
        default=None,
        help="Fuchsia checkout root for cross-repository path resolution.",
    )
    parser.add_argument(
        "paths",
        nargs="+",
        help="Paths to SKILL.md, workflow, or .agents config files/directories.",
    )

    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(message)s",
        stream=sys.stderr,
    )

    if args.lint_json:
        lint_findings = _lint_customization_paths(
            args.paths,
            repo_root=args.repo_root,
            fuchsia_root=args.fuchsia_root,
        )
        print(json.dumps(lint_findings, indent=2))
        sys.exit(0)

    all_findings = []
    any_error = False
    for path in args.paths:
        if (
            args.strict
            and os.path.isfile(path)
            and os.path.basename(path) != "SKILL.md"
        ):
            continue
        code, findings = do_lint(
            path,
            args.fixit or args.suggest_fix,
            args.suggest_fix,
            args.suggest_fix_in_json,
        )
        all_findings.extend(findings)
        if code != 0:
            any_error = True

    if args.strict and not args.suggest_fix and not args.suggest_fix_in_json:
        semantic_findings = _lint_customization_paths(
            args.paths,
            repo_root=args.repo_root,
            fuchsia_root=args.fuchsia_root,
        )
        for fd in semantic_findings:
            loc = f"{fd['filepath']}:{fd.get('line', 1)}"
            if fd["level"] == "error":
                logging.error(f"[{loc}] Error: {fd['message']}")
                any_error = True
            else:
                logging.warning(f"[{loc}] Warning: {fd['message']}")

    if args.suggest_fix_in_json:
        print(json.dumps(all_findings, indent=2))

    sys.exit(1 if any_error else 0)


if __name__ == "__main__":
    main()
