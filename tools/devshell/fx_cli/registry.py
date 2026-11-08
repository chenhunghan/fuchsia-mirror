#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""CLI for listing, validating, and managing registered `fx` host tools."""

import argparse
import json
import os
import re
import sys
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path, PurePosixPath
from typing import Any

_CMD_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*$")
_TARGET_RE = re.compile(r"^//[A-Za-z0-9_./-]+:[A-Za-z0-9_./+-]+$")
_SCRIPT_RE = re.compile(r"^//[^\s]+$")

_ALLOWED_ENTRY_KEYS = {
    "category",
    "contrib",
    "deprecated",
    "inject_prebuilt_paths",
    "script",
    "summary",
    "target",
}


# Valid tool categories allowed in fx_host_tools.json.
# 1. Input registry files must use one of these exact strings (enforced by
#    `_normalize_entry` and `fx registry check`).
# 2. "Run, inspect and debug" intentionally omits the Oxford comma to match the
#    hardcoded category heading in //scripts/fx-help.awk and
#    //tools/devshell/README.md so registered and legacy tools group under a
#    single heading in `fx help --full`.
class Category(StrEnum):
    BUILD = "Build"
    CONFIGURATION = "Configuration"
    DEVICE_DISCOVERY = "Device discovery"
    DEVICE_MANAGEMENT = "Device management"
    DOCUMENTATION = "Documentation"
    OTHER = "Other"
    RUN_INSPECT_AND_DEBUG = "Run, inspect and debug"
    SOFTWARE_DELIVERY = "Software delivery"
    SOURCE_TREE = "Source tree"
    TESTING = "Testing"
    UNKNOWN = "Unknown"


VALID_CATEGORIES: tuple[str, ...] = tuple(c.value for c in Category)


class EntryMode(StrEnum):
    TARGET = "target"
    SCRIPT = "script"


def _set_if_not_none(d: dict[str, Any], key: str, value: Any) -> None:
    if value is not None:
        d[key] = value


@dataclass
class RegistryEntry:
    """Normalized representation of a registered fx host tool or script."""

    name: str
    category: str = Category.OTHER
    summary: str = ""
    target: str | None = None
    script: str | None = None
    inject_prebuilt_paths: bool = False
    contrib: bool = False
    deprecated: bool = False
    source: str | None = None
    source_label: str | None = "core"

    @property
    def mode(self) -> EntryMode:
        return EntryMode.SCRIPT if self.script else EntryMode.TARGET

    @property
    def spec(self) -> str:
        return self.target or self.script or ""

    @property
    def is_script(self) -> bool:
        return bool(self.script)

    def to_dict(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "name": self.name,
            "category": self.category,
            "summary": self.summary,
            "inject_prebuilt_paths": self.inject_prebuilt_paths,
            "contrib": self.contrib,
            "deprecated": self.deprecated,
        }
        _set_if_not_none(result, "target", self.target)
        _set_if_not_none(result, "script", self.script)
        _set_if_not_none(result, "source", self.source)
        _set_if_not_none(result, "source_label", self.source_label)
        return result

    def __getitem__(self, key: str) -> Any:
        d = self.to_dict()
        if key in d:
            return d[key]
        raise KeyError(key)

    def __setitem__(self, key: str, value: Any) -> None:
        setattr(self, key, value)

    def get(self, key: str, default: Any = None) -> Any:
        return self.to_dict().get(key, default)

    def __contains__(self, key: str) -> bool:
        return key in self.to_dict()

    def keys(self) -> Any:
        return self.to_dict().keys()


class RegistryError(ValueError):
    """Raised when registry validation fails."""


def _is_normalized_rel_path(rel_path: str) -> bool:
    """Return True if rel_path is a normalized relative POSIX path without traversal."""
    if not rel_path or rel_path.startswith("/") or rel_path.endswith("/"):
        return False
    pure = PurePosixPath(rel_path)
    return (
        pure.as_posix() == rel_path
        and ".." not in pure.parts
        and "." not in pure.parts
    )


def _validate_cmd_name(name: str) -> None:
    if not _CMD_RE.match(name):
        raise RegistryError(
            f"Invalid tool name '{name}': must match {_CMD_RE.pattern}."
        )


def _validate_target_label(target: str) -> None:
    if not _TARGET_RE.match(target) or "///" in target:
        raise RegistryError(
            f"Invalid target label '{target}': must match {_TARGET_RE.pattern}."
        )
    pkg_part, target_part = target[2:].split(":", 1)
    if not _is_normalized_rel_path(pkg_part) or not _is_normalized_rel_path(
        target_part
    ):
        raise RegistryError(
            f"Invalid target label '{target}': path traversal or empty segments are not allowed."
        )


def _validate_script_path(script: str) -> None:
    if not _SCRIPT_RE.match(script) or script == "//" or "///" in script:
        raise RegistryError(
            f"Invalid script path '{script}': must match {_SCRIPT_RE.pattern}."
        )
    for ch in script:
        if ch in (" ", "\t", "\r", "\n"):
            raise RegistryError(
                f"Invalid script path '{script}': whitespace characters are not allowed."
            )
    if not _is_normalized_rel_path(script[2:]):
        raise RegistryError(
            f"Invalid script path '{script}': path traversal or empty segments are not allowed."
        )


def resolve_fuchsia_dir() -> Path:
    fuchsia_dir_env = os.environ.get("FUCHSIA_DIR")
    if fuchsia_dir_env:
        return Path(fuchsia_dir_env).resolve()
    return Path(__file__).resolve().parent.parent.parent.parent


def resolve_registry_path(
    fuchsia_dir: Path,
    vendor: str | None = None,
    registry_file: str | None = None,
) -> Path:
    if registry_file:
        return Path(registry_file)
    if vendor:
        return (
            fuchsia_dir
            / "vendor"
            / vendor
            / "scripts"
            / "devshell"
            / "fx_host_tools.json"
        )
    return fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"


def discover_registry_files(
    fuchsia_dir: Path,
    vendor: str | None = None,
    registry_file: str | None = None,
) -> list[tuple[str, Path]]:
    if registry_file:
        label = f"vendor/{vendor}" if vendor else "custom"
        return [(label, Path(registry_file))]
    if vendor:
        path = resolve_registry_path(fuchsia_dir, vendor=vendor)
        return [(f"vendor/{vendor}", path)]

    results: list[tuple[str, Path]] = []
    vendor_root = fuchsia_dir / "vendor"
    if vendor_root.is_dir():
        for vendor_dir in sorted(vendor_root.iterdir(), key=lambda p: p.name):
            if vendor_dir.is_dir():
                cand = (
                    vendor_dir / "scripts" / "devshell" / "fx_host_tools.json"
                )
                if cand.is_file():
                    results.append((f"vendor/{vendor_dir.name}", cand))

    core_path = fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
    if core_path.is_file():
        results.append(("core", core_path))

    return results


def _load_raw_registry(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RegistryError(f"Failed to read registry {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise RegistryError(
            f"Expected top-level JSON object in {path}, got {type(data).__name__}."
        )
    return data


def _normalize_entry(name: str, raw_value: Any) -> RegistryEntry:
    _validate_cmd_name(name)
    if isinstance(raw_value, str):
        if not raw_value:
            raise RegistryError(f"Entry '{name}' has an empty target string.")
        shorthand_target = (
            raw_value[1:] if raw_value.startswith("@//") else raw_value
        )
        _validate_target_label(shorthand_target)
        return RegistryEntry(name=name, target=shorthand_target)
    if not isinstance(raw_value, dict):
        raise RegistryError(
            f"Invalid entry for '{name}': expected string or object, got {type(raw_value).__name__}."
        )

    unknown_keys = set(raw_value.keys()) - _ALLOWED_ENTRY_KEYS
    if unknown_keys:
        raise RegistryError(
            f"Entry '{name}' has unknown keys: {sorted(unknown_keys)}."
        )

    target = raw_value.get("target")
    script = raw_value.get("script")
    if target is not None and (not isinstance(target, str) or not target):
        raise RegistryError(
            f"Entry '{name}' has an invalid 'target' field; expected non-empty string."
        )
    if script is not None and (not isinstance(script, str) or not script):
        raise RegistryError(
            f"Entry '{name}' has an invalid 'script' field; expected non-empty string."
        )
    if not (bool(target) ^ bool(script)):
        raise RegistryError(
            f"Entry '{name}' must specify exactly one of 'target' or 'script'."
        )

    category = raw_value.get("category", Category.OTHER)
    if not isinstance(category, str):
        raise RegistryError(
            f"Entry '{name}' field 'category' must be a string."
        )
    if category not in VALID_CATEGORIES:
        raise RegistryError(
            f"Entry '{name}' has invalid category '{category}'; must be one of {VALID_CATEGORIES}."
        )

    summary = raw_value.get("summary", "")
    if not isinstance(summary, str):
        raise RegistryError(f"Entry '{name}' field 'summary' must be a string.")
    if any(ch in summary for ch in ("\t", "\n", "\r")):
        raise RegistryError(
            f"Entry '{name}' field 'summary' must not contain tabs or newlines."
        )
    if "summary" in raw_value and not summary.strip():
        raise RegistryError(
            f"Entry '{name}' field 'summary' must not be empty or whitespace-only."
        )

    for bool_key in ("contrib", "deprecated", "inject_prebuilt_paths"):
        if bool_key in raw_value and not isinstance(raw_value[bool_key], bool):
            raise RegistryError(
                f"Entry '{name}' field '{bool_key}' must be a boolean."
            )

    norm_target: str | None = None
    norm_script: str | None = None
    if target:
        norm_target = target[1:] if target.startswith("@//") else target
        _validate_target_label(norm_target)
    if script:
        norm_script = script[1:] if script.startswith("@//") else script
        _validate_script_path(norm_script)

    return RegistryEntry(
        name=name,
        category=category,
        summary=summary,
        target=norm_target,
        script=norm_script,
        inject_prebuilt_paths=bool(
            raw_value.get("inject_prebuilt_paths", False)
        ),
        contrib=bool(raw_value.get("contrib", False)),
        deprecated=bool(raw_value.get("deprecated", False)),
    )


def load_registry_file(
    path: Path, source_label: str = "core"
) -> dict[str, RegistryEntry]:
    """Load and normalize all entries from a single fx_host_tools.json file."""
    if not path.is_file():
        return {}
    raw = _load_raw_registry(path)
    entries: dict[str, RegistryEntry] = {}
    for name in sorted(raw.keys()):
        entry = _normalize_entry(name, raw[name])
        entry.source = str(path)
        entry.source_label = source_label
        entries[name] = entry
    return entries


def load_all_registry_entries(
    fuchsia_dir: Path,
    vendor: str | None = None,
    registry_file: str | None = None,
) -> list[RegistryEntry]:
    registry_files = discover_registry_files(fuchsia_dir, vendor, registry_file)
    entries: list[RegistryEntry] = []
    for source_label, path in registry_files:
        file_entries = load_registry_file(path, source_label=source_label)
        entries.extend(file_entries.values())
    return entries


def load_merged_registry(
    fuchsia_dir: Path,
    vendor: str | None = None,
    registry_file: str | None = None,
) -> dict[str, RegistryEntry]:
    registry_files = discover_registry_files(fuchsia_dir, vendor, registry_file)
    merged: dict[str, RegistryEntry] = {}
    for source_label, path in registry_files:
        file_entries = load_registry_file(path, source_label=source_label)
        for name, entry in file_entries.items():
            if name not in merged:
                merged[name] = entry
    return merged


def _write_registry(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    serialized = json.dumps(data, indent=4, sort_keys=True) + "\n"
    tmp_path = path.with_name(f".{path.name}.tmp.{os.getpid()}")
    tmp_path.write_text(serialized, encoding="utf-8")
    tmp_path.replace(path)


def _cmd_list(args: argparse.Namespace, fuchsia_dir: Path) -> int:
    registry_file = args.registry_file
    vendor = args.vendor
    fmt_choice = args.format
    if args.json:
        fmt_choice = "json"

    if fmt_choice == "help-tsv":
        all_entries = load_all_registry_entries(
            fuchsia_dir, vendor=vendor, registry_file=registry_file
        )
        for e in all_entries:
            name = e.name
            source_label = e.source_label or "core"
            if source_label.startswith("vendor/"):
                v_name = source_label[len("vendor/") :]
                display_name = f"vendor {v_name} {name}"
            else:
                display_name = name
            contrib = "true" if e.contrib else "false"
            deprecated = "true" if e.deprecated else "false"
            inject = "1" if e.inject_prebuilt_paths else "0"
            print(
                f"{display_name}\t{e.category}\t{contrib}\t{deprecated}\t{e.summary}\t{e.mode}\t{e.spec}\t{name}\t{source_label}\t{inject}"
            )
        return 0

    merged = load_merged_registry(
        fuchsia_dir, vendor=vendor, registry_file=registry_file
    )
    entries = [merged[name] for name in sorted(merged.keys())]

    pattern_str = args.pattern_opt or args.pattern
    if pattern_str:
        flags = re.IGNORECASE if args.ignore_case else 0
        try:
            compiled = re.compile(pattern_str, flags)
        except re.error as exc:
            print(
                f"ERROR: Invalid regular expression '{pattern_str}': {exc}",
                file=sys.stderr,
            )
            return 1
        entries = [e for e in entries if compiled.search(e.name)]

    if args.category:
        want_cat = args.category.lower()
        entries = [e for e in entries if e.category.lower() == want_cat]

    if args.hide_contrib:
        entries = [e for e in entries if not e.contrib]

    if fmt_choice == "json":
        print(json.dumps([e.to_dict() for e in entries], indent=2))
        return 0

    if fmt_choice == "names":
        for e in entries:
            print(e.name)
        return 0

    if fmt_choice == "zsh":
        for e in entries:
            summary = e.summary.replace(":", r"\:")
            print(f"{e.name}:{summary}")
        return 0

    # table format
    if not entries:
        return 0

    headers = ("NAME", "CATEGORY", "TARGET", "SUMMARY")
    rows = [
        (
            e.name,
            e.category,
            e.spec,
            e.summary,
        )
        for e in entries
    ]
    widths = [
        max(len(headers[i]), *(len(r[i]) for r in rows)) for i in range(3)
    ]
    fmt = f"{{:<{widths[0]}}}  {{:<{widths[1]}}}  {{:<{widths[2]}}}  {{}}"
    print(fmt.format(*headers).rstrip())
    for row in rows:
        print(fmt.format(*row).rstrip())
    return 0


def _cmd_add(args: argparse.Namespace, fuchsia_dir: Path) -> int:
    reg_path = resolve_registry_path(
        fuchsia_dir, vendor=args.vendor, registry_file=args.registry_file
    )
    name = args.name.strip()
    try:
        _validate_cmd_name(name)
    except RegistryError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    raw_target = args.target.strip() if args.target else None
    raw_script = args.script.strip() if args.script else None
    if not (bool(raw_target) ^ bool(raw_script)):
        print(
            "ERROR: Pass exactly one of positional <target> or --script <path>.",
            file=sys.stderr,
        )
        return 1

    entry: dict[str, Any] = {}
    if raw_target is not None:
        target = raw_target[1:] if raw_target.startswith("@//") else raw_target
        try:
            _validate_target_label(target)
        except RegistryError as exc:
            print(f"ERROR: {exc}", file=sys.stderr)
            return 1
        entry["target"] = target
    else:
        assert raw_script is not None
        script = raw_script[1:] if raw_script.startswith("@//") else raw_script
        try:
            _validate_script_path(script)
        except RegistryError as exc:
            print(f"ERROR: {exc}", file=sys.stderr)
            return 1
        entry["script"] = script

    raw = _load_raw_registry(reg_path)
    for existing_name, existing_val in raw.items():
        if existing_name == name and args.force:
            continue
        _normalize_entry(existing_name, existing_val)

    if name in raw and not args.force:
        print(
            f"ERROR: Tool '{name}' is already registered in {reg_path} "
            "(pass --force to overwrite).",
            file=sys.stderr,
        )
        return 1

    if args.category is not None:
        entry["category"] = args.category
    if args.summary is not None:
        entry["summary"] = args.summary
    if args.inject_prebuilt_paths:
        entry["inject_prebuilt_paths"] = True
    if args.contrib:
        entry["contrib"] = True
    if args.deprecated:
        entry["deprecated"] = True

    try:
        _normalize_entry(name, entry)
    except RegistryError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    raw[name] = entry
    _write_registry(reg_path, raw)
    return 0


def _cmd_remove(args: argparse.Namespace, fuchsia_dir: Path) -> int:
    reg_path = resolve_registry_path(
        fuchsia_dir, vendor=args.vendor, registry_file=args.registry_file
    )
    raw = _load_raw_registry(reg_path)
    if args.name not in raw:
        print(
            f"ERROR: Tool '{args.name}' is not registered in {reg_path}.",
            file=sys.stderr,
        )
        return 1
    del raw[args.name]
    _write_registry(reg_path, raw)
    return 0


def validate_registry_file(
    reg_path: Path, fuchsia_dir: Path, vendor_name: str | None = None
) -> list[str]:
    errors: list[str] = []
    if not reg_path.is_file():
        errors.append(f"{reg_path}: Registry file does not exist.")
        return errors

    try:
        raw_text = reg_path.read_text(encoding="utf-8")
        if not raw_text.strip():
            errors.append(f"{reg_path}: Registry file is empty.")
            return errors
        data = json.loads(raw_text)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        errors.append(f"{reg_path}: Failed to parse JSON: {exc}")
        return errors

    if not isinstance(data, dict):
        errors.append(
            f"{reg_path}: Expected top-level JSON object in fx_host_tools.json."
        )
        return errors

    keys = list(data.keys())
    if keys != sorted(keys):
        errors.append(
            f"{reg_path}: Top-level keys in fx_host_tools.json must be sorted alphabetically."
        )

    for cmd, entry in data.items():
        try:
            norm = _normalize_entry(cmd, entry)
        except RegistryError as exc:
            errors.append(f"{reg_path}: {exc}")
            continue

        if isinstance(entry, dict):
            entry_keys = list(entry.keys())
            if entry_keys != sorted(entry_keys):
                errors.append(
                    f"{reg_path}: Entry '{cmd}' keys must be sorted alphabetically."
                )

        target = norm.get("target")
        script = norm.get("script")

        if vendor_name:
            vendor_prefix = f"//vendor/{vendor_name}/"
            spec = target or script or ""
            if not spec.startswith(vendor_prefix):
                errors.append(
                    f"{reg_path}: Entry '{cmd}' in vendor/{vendor_name} must have target or script starting with '{vendor_prefix}', got '{spec}'."
                )

        if target:
            pkg_dir = target[2:].split(":", 1)[0]
            build_file = fuchsia_dir / pkg_dir / "BUILD.bazel"
            if not build_file.is_file():
                errors.append(
                    f"{reg_path}: Entry '{cmd}' references missing Bazel package '{pkg_dir}/BUILD.bazel'."
                )

        if script:
            rel_script = script[2:]
            script_file = fuchsia_dir / rel_script
            if not script_file.is_file():
                errors.append(
                    f"{reg_path}: Entry '{cmd}' references missing script '{rel_script}'."
                )

    return errors


def _infer_vendor_from_path(path: Path, fuchsia_dir: Path) -> str | None:
    try:
        rel = path.resolve().relative_to(fuchsia_dir)
        parts = rel.parts
        if len(parts) >= 2 and parts[0] == "vendor":
            return parts[1]
    except ValueError:
        pass
    return None


def _cmd_check(args: argparse.Namespace, fuchsia_dir: Path) -> int:
    files_to_check: list[tuple[str | None, Path]] = []
    if args.files:
        for f in args.files:
            p = Path(f).resolve()
            files_to_check.append((_infer_vendor_from_path(p, fuchsia_dir), p))
    else:
        discovered = discover_registry_files(
            fuchsia_dir, vendor=args.vendor, registry_file=args.registry_file
        )
        for label, path in discovered:
            vendor = (
                label[len("vendor/") :]
                if label.startswith("vendor/")
                else _infer_vendor_from_path(path, fuchsia_dir)
            )
            files_to_check.append((vendor, path))

    all_errors: list[str] = []
    for vendor, p in files_to_check:
        errs = validate_registry_file(p, fuchsia_dir, vendor_name=vendor)
        all_errors.extend(errs)

    if all_errors:
        for err in all_errors:
            print(f"ERROR: {err}", file=sys.stderr)
        return 1
    return 0


def _build_parser() -> argparse.ArgumentParser:
    common_parent = argparse.ArgumentParser(add_help=False)
    common_parent.add_argument(
        "--fuchsia-dir",
        default=argparse.SUPPRESS,
        help="Path to Fuchsia source checkout root.",
    )
    common_parent.add_argument(
        "--vendor",
        default=argparse.SUPPRESS,
        help="Operate on vendor/<VENDOR>/scripts/devshell/fx_host_tools.json.",
    )
    common_parent.add_argument(
        "--registry-file",
        default=argparse.SUPPRESS,
        help="Explicit path to an fx_host_tools.json registry file.",
    )

    parser = argparse.ArgumentParser(
        prog="fx registry",
        description="List, validate, and manage registered fx host tools.",
        parents=[common_parent],
    )

    subparsers = parser.add_subparsers(dest="subcommand")

    list_parser = subparsers.add_parser(
        "list",
        parents=[common_parent],
        help="List registered fx host tools, optionally filtered by pattern or category.",
    )
    list_parser.add_argument(
        "pattern",
        nargs="?",
        help="Regular expression matched against tool names (substring match).",
    )
    list_parser.add_argument(
        "-p",
        "--pattern",
        dest="pattern_opt",
        help="Regular expression matched against tool names (substring match).",
    )
    list_parser.add_argument(
        "-i",
        "--ignore-case",
        action="store_true",
        help="Perform case-insensitive regular expression matching.",
    )
    list_parser.add_argument(
        "-c",
        "--category",
        help="Filter tools by category (case-insensitive).",
    )
    list_parser.add_argument(
        "--format",
        choices=["table", "json", "names", "zsh", "help-tsv"],
        default="table",
        help="Output format: table (default), json, names, zsh, help-tsv.",
    )
    list_parser.add_argument(
        "--json",
        action="store_true",
        help="Emit matching entries as JSON (shortcut for --format=json).",
    )
    list_parser.add_argument(
        "--hide-contrib",
        action="store_true",
        help="Exclude contrib commands from the output.",
    )

    add_parser = subparsers.add_parser(
        "add",
        parents=[common_parent],
        help="Register a Bazel fx_host_tool target or direct checkout script.",
    )
    add_parser.add_argument("name", help="Subcommand name exposed by `fx`.")
    add_parser.add_argument(
        "target",
        nargs="?",
        help="Bazel target label (e.g. //path/to:tool). Mutually exclusive with --script.",
    )
    add_parser.add_argument(
        "--script",
        help="Checkout-relative script path executed directly without Bazel (e.g. //tools/devshell/set).",
    )
    add_parser.add_argument(
        "-c",
        "--category",
        help="Help category (e.g. 'Build', 'Source tree', 'Testing').",
    )
    add_parser.add_argument(
        "-s",
        "--summary",
        help="One-line summary displayed in `fx help --full` and `fx registry list`.",
    )
    add_parser.add_argument(
        "--inject-prebuilt-paths",
        action="store_true",
        help="Opt into legacy PREBUILT_ALL_PATHS injection during migration.",
    )
    add_parser.add_argument(
        "--contrib",
        action="store_true",
        help="Mark command as a contrib subcommand.",
    )
    add_parser.add_argument(
        "--deprecated",
        action="store_true",
        help="Mark command as deprecated.",
    )
    add_parser.add_argument(
        "-f",
        "--force",
        action="store_true",
        help="Overwrite an existing registry entry with the same name.",
    )

    remove_parser = subparsers.add_parser(
        "remove",
        parents=[common_parent],
        aliases=["rm"],
        help="Remove a registered fx host tool.",
    )
    remove_parser.add_argument("name", help="Registered subcommand name.")

    check_parser = subparsers.add_parser(
        "check",
        parents=[common_parent],
        help="Validate fx_host_tools.json schema, key sorting, and on-disk files.",
    )
    check_parser.add_argument(
        "files",
        nargs="*",
        help="Optional explicit registry files to validate (defaults to all discovered registries).",
    )

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    for attr in ("fuchsia_dir", "vendor", "registry_file"):
        if not hasattr(args, attr):
            setattr(args, attr, None)
    fuchsia_dir = (
        Path(args.fuchsia_dir).resolve()
        if args.fuchsia_dir
        else resolve_fuchsia_dir()
    )
    try:
        if args.subcommand in (None, "list"):
            if args.subcommand is None:
                args.pattern = None
                args.pattern_opt = None
                args.ignore_case = False
                args.category = None
                args.format = "table"
                args.json = False
                args.hide_contrib = False
            return _cmd_list(args, fuchsia_dir)
        if args.subcommand == "add":
            return _cmd_add(args, fuchsia_dir)
        if args.subcommand in ("remove", "rm"):
            return _cmd_remove(args, fuchsia_dir)
        if args.subcommand == "check":
            return _cmd_check(args, fuchsia_dir)
    except RegistryError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2
    except OSError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    parser.print_help()
    return 1


if __name__ == "__main__":
    sys.exit(main())
