# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities to record and report on GN and Bazel flag differences."""

import collections
import dataclasses
import difflib
import typing as T

# A callable that takes a single flag as input, and returns a category name
# when creating a differences report.
FlagCategorizer: T.TypeAlias = T.Callable[[str], str]


@dataclasses.dataclass
class FlagsDifferences:
    """Lists of flags that differ between GN and Bazel commands.

    Usage is:
       1) Use new_from_lists() to create new instance from given sequences
          of GN and Bazel flags corresponding to the same targets.

       2) Call generate_sunnary() or generate_markdown_report() to generate
          a textual representation of the flag differences.

    """

    gn_flags: list[str] = dataclasses.field(default_factory=list)
    bazel_flags: list[str] = dataclasses.field(default_factory=list)

    # These subsets correspond to different flags from the lists above.
    gn_only: set[str] = dataclasses.field(default_factory=set)
    bazel_only: set[str] = dataclasses.field(default_factory=set)
    common: set[str] = dataclasses.field(default_factory=set)

    @staticmethod
    def new_from_lists(
        gn_flags: T.Iterable[str], bazel_flags: T.Iterable[str]
    ) -> "FlagsDifferences":
        """Create new instance from two sequences of GN and Bazel flags."""
        gn_flags_set = set(gn_flags)
        bazel_flags_set = set(bazel_flags)

        return FlagsDifferences(
            gn_flags=list(gn_flags),
            bazel_flags=list(bazel_flags),
            gn_only=gn_flags_set - bazel_flags_set,
            bazel_only=bazel_flags_set - gn_flags_set,
            common=gn_flags_set & bazel_flags_set,
        )

    @property
    def has_differences(self) -> bool:
        """Return True if this instance has differences."""
        return bool(self.gn_only) or bool(self.bazel_only)

    def get_categories_map(
        self, flag_categorizer: FlagCategorizer
    ) -> dict[str, "FlagsDifferences"]:
        """Map an input FlagsDifferences value to categories.

        Args:
          flag_categorizer: A FlagCategorizer instance, which
              will be used on each flag in |differences|.
        Returns:
          A { category -> FlagsDifferences } map, whose values only contain
          'gn_only' and 'bazel_only' entries.
        """
        result: dict[str, FlagsDifferences] = collections.defaultdict(
            FlagsDifferences
        )
        for flag in self.gn_only:
            category = flag_categorizer(flag)
            result[category].gn_only.add(flag)
        for flag in self.bazel_only:
            category = flag_categorizer(flag)
            result[category].bazel_only.add(flag)

        return result

    def _get_unified_diff(self) -> list[str]:
        return list(
            difflib.unified_diff(
                self.gn_flags,
                self.bazel_flags,
                fromfile="gn_flags",
                tofile="bazel_flags",
                lineterm="",
            )
        )

    def generate_summary(
        self,
        title: str = "",
        flag_categorizer: FlagCategorizer | None = None,
    ) -> str:
        """Generate a human-readable summary of differences.

        Args:
          flag_categorizer: An optional FlagCategorizer applied to all flags in
            this instance. If not provided, default_categorizer will be used.
        Returns:
          A summary as a string.
        """
        lines = [
            "=" * 60,
            title or "FLAG COMPARISON SUMMARY",
            "=" * 60,
            f"Matching flags:   {len(self.common)}",
            f"GN-only flags:    {len(self.gn_only)}",
            f"Bazel-only flags: {len(self.bazel_only)}",
            "-" * 60,
        ]

        # Map each flag to a category, then create a { category -> FlagsDifferences } map
        # from the result for the final report.

        categories_map = self.get_categories_map(
            flag_categorizer or default_categorizer
        )
        for category, flags in sorted(categories_map.items()):
            lines.append(f"\n[{category}]")
            if flags.gn_only:
                lines.append("  In GN only (missing in Bazel):")
                lines.extend(f"    {f}" for f in flags.gn_only)
            if flags.bazel_only:
                lines.append("  In Bazel only (extra in Bazel):")
                lines.extend(f"    {f}" for f in flags.bazel_only)

        lines += [
            "",
            "=" * 60,
            "Unified diff",
            "=" * 60,
        ] + self._get_unified_diff()

        return "\n".join(lines)

    def generate_markdown_report(
        self,
        title: str = "",
        flag_categorizer: FlagCategorizer | None = None,
    ) -> str:
        """Generate a Markdown report of differences.

        Args:
          title: Optional title.
          flag_categorizer: An optional FlagCategorizer applied to all flags in
            this instance.
        Returns:
          A Markdown report as a string.
        """
        title = title or "Flag Comparison Report"
        lines = [
            f"### {title}",
            "",
            f"| Metric | Count |",
            "| :--- | :--- |",
            f"| Matching Flags | {len(self.common)} |",
            f"| GN Only (Missing in Bazel) | {len(self.gn_only)} |",
            f"| Bazel Only (Extra in Bazel) | {len(self.bazel_only)} |",
            "",
        ]

        categories_map = self.get_categories_map(
            flag_categorizer or default_categorizer
        )
        for category, flags in sorted(categories_map.items()):
            lines += [
                f"#### {category}",
                "| Status | Flag |",
                "| :--- | :--- |",
            ]
            for f in flags.gn_only:
                lines.append(f"| GN only | `{f}` |")
            for f in flags.bazel_only:
                lines.append(f"| Bazel only | `{f}` |")
            lines.append("")

        lines += [
            f"### Unified Diff",
        ] + self._get_unified_diff()

        return "\n".join(lines)


def default_categorizer(flag: str) -> str:
    """The default FlagCategorizer, always returns 'Other'"""
    return "Other"


def categorize_clang_flag(flag: str) -> str:
    """A FlagCategorizer function for clang flags."""
    if (
        flag.startswith("-L")
        or flag.startswith("-l")
        or flag.startswith("-Wl,")
    ):
        return "Linker Options"
    if flag.startswith("-W"):
        return "Warnings (-W)"
    if flag.startswith("-D"):
        return "Defines (-D)"
    if (
        flag.startswith("-I")
        or flag.startswith("-isystem")
        or flag.startswith("-iquote")
    ):
        return "Includes (-I)"
    if flag.startswith("-O"):
        return "Optimization (-O)"
    if (
        flag.startswith("-ffile-prefix-map=")
        or flag.startswith("-fdebug-prefix-map=")
        or flag.startswith("-fmacro-prefix-map=")
        or flag.startswith("-fcoverage-prefix-map=")
        or flag.startswith("-fprofile-prefix-map=")
        or flag.startswith("-ffile-compilation-dir=")
        or flag.startswith("--remap-path-prefix=")
    ):
        return "Prefix Mappings / Determinism"
    if flag.startswith("-f"):
        return "Compiler Features (-f)"
    if flag.startswith("-m"):
        return "Machine Options (-m)"
    if flag.startswith("-g"):
        return "Debug Info (-g)"
    if flag.startswith("-std="):
        return "Language Standard (-std)"
    if flag.startswith("--target=") or flag.startswith("--sysroot="):
        return "Target / Sysroot"
    return "Other"


def categorize_rust_flag(flag: str) -> str:
    """A FlagCategorizer function for rustc flags."""
    # Linker and library search options (checked before general -C and -L/-l)
    if (
        flag.startswith("-Clinker=")
        or flag.startswith("--codegen=linker=")
        or flag.startswith("-Clink-arg=")
        or flag.startswith("--codegen=link-arg=")
        or flag.startswith("-Clink-args=")
        or flag.startswith("--codegen=link-args=")
        or flag.startswith("-L")
        or flag.startswith("-l")
    ):
        return "Linker Options"

    # Optimization options
    if (
        flag.startswith("-Copt-level=")
        or flag.startswith("--codegen=opt-level=")
        or flag == "-O"
        or flag.startswith("-O")
    ):
        return "Optimization (-O)"

    # Debug info and debug assertions
    if (
        flag.startswith("-Cdebuginfo=")
        or flag.startswith("--codegen=debuginfo=")
        or flag.startswith("-Cdebug-assertions=")
        or flag.startswith("--codegen=debug-assertions=")
        or flag == "-g"
        or flag.startswith("-g")
    ):
        return "Debug Info (-g)"

    # Path remapping and determinism / metadata flags
    if (
        flag.startswith("--remap-path-prefix=")
        or flag.startswith("-Cmetadata=")
        or flag.startswith("--codegen=metadata=")
        or flag.startswith("-Cextra-filename=")
        or flag.startswith("--codegen=extra-filename=")
    ):
        return "Prefix Mappings / Determinism"

    # Lints / Warnings: -A (--allow), -W (--warn), -D (--deny), -F (--forbid)
    if (
        flag.startswith("-A")
        or flag.startswith("--allow")
        or flag.startswith("-W")
        or flag.startswith("--warn")
        or flag.startswith("-D")
        or flag.startswith("--deny")
        or flag.startswith("-F")
        or flag.startswith("--forbid")
        or flag.startswith("--cap-lints")
    ):
        return "Lints / Warnings"

    # Conditional compilation (--cfg, --check-cfg)
    if flag.startswith("--cfg") or flag.startswith("--check-cfg"):
        return "Conditional Compilation (--cfg)"

    # External crate dependencies
    if flag.startswith("--extern"):
        return "Dependencies (--extern)"

    # Remaining Codegen options (-C / --codegen)
    if flag.startswith("-C") or flag.startswith("--codegen"):
        return "Codegen Options (-C)"

    # Rust edition / language version
    if flag.startswith("--edition"):
        return "Language Edition (--edition)"

    # Target architecture and sysroot
    if flag.startswith("--target=") or flag.startswith("--sysroot="):
        return "Target / Sysroot"

    # Crate outputs, artifact emission, and formatting
    if (
        flag.startswith("--crate-name=")
        or flag.startswith("--crate-type=")
        or flag.startswith("--emit=")
        or flag.startswith("--out-dir=")
        or flag.startswith("-o=")
        or flag.startswith("-o")
        or flag.startswith("--error-format=")
    ):
        return "Crate Outputs & Formats"

    # Unstable rustc flags (-Z)
    if flag.startswith("-Z"):
        return "Unstable Options (-Z)"

    return "Other"
