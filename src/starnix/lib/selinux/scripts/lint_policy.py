#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Lints SEStarnix userspace test SELinux policy fragments.

Invoked by `//src/starnix/lib/selinux/scripts/selinux_policy.star` (registered
from `//src/starnix/shac.star`) to enforce naming conventions, capability/domain
invariants, and `*` / `~{ ... }` / `neverallow` guardrails.
"""

import argparse
import dataclasses
import itertools
import json
import pathlib
import re
import sys
import types
from collections.abc import Mapping, Sequence, Set

_BASE_CONF_FILES = (
    "lib/selinux/testdata/policy/base/filesystems.conf",
    "lib/selinux/testdata/policy/base/minimal_base.conf",
    "tests/selinux/userspace/policies/test_policy.conf",
)
_CAPABILITY_CLASSES = frozenset(
    {"capability", "capability2", "cap_userns", "cap2_userns"}
)
_PROCESS_CLASSES = frozenset({"process"})
_TEST_A_SOURCES = frozenset({"test_a"})
_NEGATIVE_DOMAIN_PATTERN = re.compile(r"_(?:no|deny|denied|without|none)_")
_NO_TRANSITION_PATTERN = re.compile(r"_no_trans(?:ition)?(?:_|$)")
_SUFFIX_RULES = types.MappingProxyType(
    {
        "type": (
            re.compile(r"_t$"),
            "must end with `_t` (refpolicy style; do not use `_s` for domains)",
        ),
        "attribute": (re.compile(r"_a$"), "must end with `_a`"),
        "role": (re.compile(r"_r\d*$"), "must end with `_r`"),
        "user": (re.compile(r"_u\d*$"), "must end with `_u`"),
    }
)

_TOKEN_OR_BRACE = r"(?:~?\s*\{[^}]+\}|~\s*\S+|\S+)"
_INLINE_IF_PATTERN = re.compile(r"^if\s*\(.+\)\s*\{(.+)\}$")
_TYPE_DECL_PATTERN = re.compile(
    r"^type\s+(\S+?)(?:\s+alias\s+(?:\{[^}]+\}|\S+))?(?:\s*,\s*(.+?))?\s*;$"
)
_TYPEATTR_PATTERN = re.compile(r"^typeattribute\s+(\S+)\s+(.+?)\s*;$")
_ATTR_DECL_PATTERN = re.compile(r"^attribute\s+(\S+)\s*;$")
_ROLE_DECL_PATTERN = re.compile(r"^role\s+(\S+)\s*;$")
_USER_DECL_PATTERN = re.compile(r"^user\s+(\S+)\s+roles\s+")
_AV_RULE_PATTERN = re.compile(
    rf"^(allow|neverallow|dontaudit|auditallow)\s+({_TOKEN_OR_BRACE})\s+"
    rf"({_TOKEN_OR_BRACE})\s*:\s*({_TOKEN_OR_BRACE})\s+(.+?)\s*;$"
)


@dataclasses.dataclass(frozen=True)
class LintFinding:
    """A single lint finding emitted as JSON to SHAC."""

    rule: str
    filepath: str
    line: int
    message: str

    def to_json_dict(self) -> Mapping[str, object]:
        return {
            "rule": self.rule,
            "filepath": self.filepath,
            "line": self.line,
            "message": self.message,
        }


@dataclasses.dataclass(frozen=True)
class _TokenSet:
    tokens: Sequence[str]
    is_wildcard: bool
    is_negated: bool

    @property
    def is_broad(self) -> bool:
        return self.is_wildcard or self.is_negated

    def matches_any(self, candidates: Set[str]) -> bool:
        if self.is_wildcard:
            return True
        if self.is_negated:
            return any(c not in self.tokens for c in candidates)
        return any(t in candidates for t in self.tokens)


@dataclasses.dataclass(frozen=True)
class _AvRule:
    line: int
    kind: str
    sources: _TokenSet
    targets: _TokenSet
    classes: _TokenSet
    perms: _TokenSet


def _parse_token_set(raw: str) -> _TokenSet:
    stripped = raw.strip()
    if stripped == "*":
        return _TokenSet((), True, False)
    is_negated = stripped.startswith("~")
    if is_negated:
        stripped = stripped[1:].strip()
    if stripped.startswith("{") and stripped.endswith("}"):
        return _TokenSet(tuple(stripped[1:-1].split()), False, is_negated)
    return _TokenSet((stripped,), False, is_negated)


def _split_csv(raw: str) -> Sequence[str]:
    return tuple(s for part in raw.split(",") if (s := part.strip()))


def _load_base_symbols(starnix_root: pathlib.Path) -> frozenset[str]:
    symbols: set[str] = set()
    for rel_path in _BASE_CONF_FILES:
        for line in (
            (starnix_root / rel_path).read_text(encoding="utf-8").splitlines()
        ):
            cleaned = line.split("#", 1)[0].strip()
            if (match := _TYPE_DECL_PATTERN.match(cleaned)) is not None:
                symbols.add(match.group(1))
            elif (match := _ATTR_DECL_PATTERN.match(cleaned)) is not None:
                symbols.add(match.group(1))
    return frozenset(symbols)


def lint_policy_text(
    rel_path: str,
    content: str,
    base_symbols: Set[str],
) -> Sequence[LintFinding]:
    """Lints a single userspace test policy fragment and returns findings."""
    filename = pathlib.PurePosixPath(rel_path).name
    stem = (
        filename.removesuffix("_policy.conf")
        if filename.endswith("_policy.conf")
        else filename.removesuffix(".conf")
    )
    expected_prefix = f"test_{stem}_"

    declarations: list[tuple[int, str, str]] = []
    type_lines: dict[str, int] = {}
    type_attrs: dict[str, set[str]] = {}
    test_a_line: dict[str, int] = {}
    domain_a_line: dict[str, int] = {}
    av_rules: list[_AvRule] = []

    def _record_attrs(line_no: int, type_name: str, attrs: Sequence[str]):
        type_attrs.setdefault(type_name, set()).update(attrs)
        if "test_a" in attrs:
            test_a_line.setdefault(type_name, line_no)
        if "domain_a" in attrs:
            domain_a_line.setdefault(type_name, line_no)

    for line_no, raw_line in enumerate(content.splitlines(), start=1):
        cleaned = raw_line.split("#", 1)[0].strip()
        if not cleaned:
            continue
        if (if_match := _INLINE_IF_PATTERN.match(cleaned)) is not None:
            cleaned = if_match.group(1).strip()

        if (match := _TYPE_DECL_PATTERN.match(cleaned)) is not None:
            type_name, attrs_raw = match.groups()
            declarations.append((line_no, "type", type_name))
            type_lines.setdefault(type_name, line_no)
            type_attrs.setdefault(type_name, set())
            if attrs_raw is not None:
                _record_attrs(line_no, type_name, _split_csv(attrs_raw))
        elif (match := _TYPEATTR_PATTERN.match(cleaned)) is not None:
            _record_attrs(line_no, match.group(1), _split_csv(match.group(2)))
        elif (match := _ATTR_DECL_PATTERN.match(cleaned)) is not None:
            declarations.append((line_no, "attribute", match.group(1)))
        elif (match := _ROLE_DECL_PATTERN.match(cleaned)) is not None:
            declarations.append((line_no, "role", match.group(1)))
        elif (match := _USER_DECL_PATTERN.match(cleaned)) is not None:
            declarations.append((line_no, "user", match.group(1)))
        elif (match := _AV_RULE_PATTERN.match(cleaned)) is not None:
            kind, src_raw, tgt_raw, cls_raw, perms_raw = match.groups()
            av_rules.append(
                _AvRule(
                    line=line_no,
                    kind=kind,
                    sources=_parse_token_set(src_raw),
                    targets=_parse_token_set(tgt_raw),
                    classes=_parse_token_set(cls_raw),
                    perms=_parse_token_set(perms_raw),
                )
            )

    findings: list[LintFinding] = []

    def emit(rule: str, line: int, message: str):
        findings.append(
            LintFinding(
                rule=rule, filepath=rel_path, line=line, message=message
            )
        )

    # 1. Strict `test_<stem>_` prefixes and refpolicy suffixes.
    for line_no, kind, name in declarations:
        if not name.startswith(expected_prefix):
            emit(
                "naming_prefix",
                line_no,
                f"{kind.capitalize()} `{name}` must start with `{expected_prefix}` to match `{filename}`.",
            )
        suffix_pattern, suffix_desc = _SUFFIX_RULES[kind]
        if suffix_pattern.search(name) is None:
            emit(
                "naming_suffix",
                line_no,
                f"{kind.capitalize()} `{name}` {suffix_desc}.",
            )

    # 2. Disallow `domain_a` on `test_a` domains (leaks `dir { search }` on all `file_like_a`).
    for type_name, attrs in type_attrs.items():
        if "test_a" in attrs and "domain_a" in attrs:
            emit(
                "no_domain_a_on_test_domain",
                max(test_a_line[type_name], domain_a_line[type_name]),
                f"Do not attach `domain_a` to `test_a` domain `{type_name}`; "
                "`domain_a` implicitly grants `dir { search }` on all `file_like_a` objects.",
            )

    # Precompute attribute membership and `neverallow` coverage.
    attr_members: dict[str, set[str]] = {}
    for type_name, attrs in type_attrs.items():
        for attr in attrs:
            attr_members.setdefault(attr, set()).add(type_name)

    neverallow_by_triple: dict[tuple[str, str, str], set[str]] = {}
    neverallow_wildcards: set[tuple[str, str, str]] = set()
    types_in_neverallow: set[str] = set()
    for rule in av_rules:
        if (
            rule.kind != "neverallow"
            or rule.sources.is_broad
            or rule.targets.is_broad
            or rule.classes.is_broad
        ):
            continue
        srcs = {
            s
            for src in rule.sources.tokens
            for s in (src, *attr_members.get(src, ()))
        }
        tgts = {
            t
            for tgt in rule.targets.tokens
            for t in (tgt, *attr_members.get(tgt, ()))
        }
        types_in_neverallow.update(srcs, tgts)
        for s, t, cls_name in itertools.product(
            srcs, tgts, rule.classes.tokens
        ):
            triples = [(s, t, cls_name)]
            if s == t:
                triples.append((s, "self", cls_name))
            elif t == "self" and s in type_lines:
                triples.append((s, s, cls_name))
            for triple in triples:
                if rule.perms.is_wildcard:
                    neverallow_wildcards.add(triple)
                elif not rule.perms.is_negated:
                    neverallow_by_triple.setdefault(triple, set()).update(
                        rule.perms.tokens
                    )

    # 3. Capability self-target, wildcard/negation guardrails, and paired `neverallow`.
    for rule in av_rules:
        classes_include_capability = rule.classes.matches_any(
            _CAPABILITY_CLASSES
        )
        targets_only_self = not rule.targets.is_broad and tuple(
            rule.targets.tokens
        ) == ("self",)
        if classes_include_capability and not targets_only_self:
            emit(
                "capability_target_self",
                rule.line,
                "Capability rules (`capability`, `capability2`, `cap_userns`, `cap2_userns`) "
                "must use `self` as the target.",
            )

        if rule.kind != "allow":
            continue

        if (
            rule.perms.is_broad or rule.classes.is_broad
        ) and classes_include_capability:
            emit(
                "no_wildcard_capability",
                rule.line,
                "Do not use `*` or `~{ ... }` on capability classes; complemented "
                "capability grants silently enable superseding capabilities such as "
                "`sys_admin` and `dac_override`.",
            )
        if (
            rule.perms.is_broad or rule.classes.is_broad
        ) and rule.classes.matches_any(_PROCESS_CLASSES):
            emit(
                "no_wildcard_process_or_base_target",
                rule.line,
                "Do not use `*` or `~{ ... }` on `process`; it silently grants "
                "side-effecting permissions such as `noatsecure`, `siginh`, and `share`.",
            )
        if (
            rule.perms.is_broad or rule.sources.is_broad
        ) and rule.sources.matches_any(_TEST_A_SOURCES):
            emit(
                "no_wildcard_process_or_base_target",
                rule.line,
                "Do not use `*` or `~{ ... }` in `allow` with broad or `test_a` sources.",
            )
        if (
            rule.perms.is_broad or rule.targets.is_broad
        ) and rule.targets.matches_any(base_symbols):
            emit(
                "no_wildcard_process_or_base_target",
                rule.line,
                "Do not use `*` or `~{ ... }` in `allow` targeting base symbols; "
                "scope wildcard grants to explicit suite-local types.",
            )

        if (
            rule.perms.is_negated
            and not rule.sources.is_broad
            and not rule.targets.is_broad
            and not rule.classes.is_broad
        ):
            for src, tgt, cls_name in itertools.product(
                rule.sources.tokens, rule.targets.tokens, rule.classes.tokens
            ):
                triple = (src, tgt, cls_name)
                if triple in neverallow_wildcards:
                    continue
                denied = neverallow_by_triple.get(triple, ())
                missing = tuple(p for p in rule.perms.tokens if p not in denied)
                if missing:
                    missing_str = " ".join(missing)
                    emit(
                        "paired_neverallow_for_negation",
                        rule.line,
                        f"`allow {src} {tgt}:{cls_name} ~{{ ... }}` must be paired with "
                        f"`neverallow {src} {tgt}:{cls_name} {{ {missing_str} }};`.",
                    )

    # 4. Negative-named `test_a` domains must appear in a `neverallow`.
    for type_name, line_no in type_lines.items():
        if "test_a" not in type_attrs.get(type_name, ()):
            continue
        if (
            _NEGATIVE_DOMAIN_PATTERN.search(type_name) is not None
            and _NO_TRANSITION_PATTERN.search(type_name) is None
            and type_name not in types_in_neverallow
        ):
            emit(
                "negative_domain_requires_neverallow",
                line_no,
                f"Negative-permission domain `{type_name}` has no `neverallow` rule; "
                "add an explicit `neverallow` to guard against accidental permission leaks.",
            )

    findings.sort(key=lambda f: (f.line, f.rule))
    return tuple(findings)


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Lint SEStarnix userspace test SELinux policy fragments."
    )
    parser.add_argument(
        "--root",
        required=True,
        type=pathlib.Path,
        help="Path to the //src/starnix root directory.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        help="Policy file paths relative to --root.",
    )
    args = parser.parse_args(argv)

    starnix_root = args.root.resolve()
    base_symbols = _load_base_symbols(starnix_root)
    all_findings: list[LintFinding] = []
    for rel_path in sorted(set(p.lstrip("/") for p in args.files)):
        full_path = starnix_root / rel_path
        if full_path.is_file():
            all_findings.extend(
                lint_policy_text(
                    rel_path,
                    full_path.read_text(encoding="utf-8"),
                    base_symbols,
                )
            )
    print(json.dumps([f.to_json_dict() for f in all_findings]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
