# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Linter check to require //build/bazel/BAZEL_OWNERS review of # @bazel2gn:*_overwrite comment annotations."""

import os
import re
from pathlib import Path

from framework import BazelCheck, FileContext, FileKind, register_check

from .bazel2gn_comment_utils import (
    Bazel2gnCommentAnnotation,
    find_bazel2gn_comment_annotations,
)

_OVERWRITE_REGEX = re.compile(r"@bazel2gn:([a-zA-Z0-9_]*_overwrite)(?::|$)")

# Check whether a path_overwrite directive maps an fx_packaged_binary wrapper
# label back to its underlying executable/rustc_binary target label in GN.
# TODO(https://fxbug.dev/584478592): Remove once fx_packaged_binary target overwrites are resolved.
_FX_PACKAGED_BIN_CACHE: dict[str, str] = {}


def _block_at(text: str, start: int) -> str:
    depth = 0
    for i in range(start, len(text)):
        c = text[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
    return text[start:]


def _is_fx_packaged_binary_overwrite(root: Path, d: str, line: str) -> bool:
    m_dir = re.search(r"#\s*@bazel2gn:path_overwrite:([^\s#]+)", line)
    if not m_dir:
        return False
    code_part = line.split("#", 1)[0]
    m_lbl = re.search(r'"([^"]+)"', code_part)
    if not m_lbl:
        return False
    bz_lbl = m_lbl.group(1).strip()
    raw_clean = bz_lbl[1:] if bz_lbl.startswith("@") else bz_lbl
    if raw_clean.startswith("//"):
        body = raw_clean[2:]
        if ":" in body:
            dep_pkg, dep_name = body.split(":", 1)
        else:
            dep_pkg = body.strip("/")
            dep_name = os.path.basename(dep_pkg)
    elif raw_clean.startswith(":"):
        dep_pkg, dep_name = d, raw_clean[1:]
    else:
        return False
    dep_pkg = dep_pkg.strip("/")
    dep_bz = root / dep_pkg / "BUILD.bazel"
    dep_bz_str = str(dep_bz)
    if dep_bz_str not in _FX_PACKAGED_BIN_CACHE:
        try:
            _FX_PACKAGED_BIN_CACHE[dep_bz_str] = dep_bz.read_text(
                encoding="utf-8", errors="replace"
            )
        except OSError:
            _FX_PACKAGED_BIN_CACHE[dep_bz_str] = ""
    dep_src = _FX_PACKAGED_BIN_CACHE[dep_bz_str]
    if "fx_packaged_binary" not in dep_src:
        return False
    for cm in re.finditer(r"fx_packaged_binary\s*\(", dep_src):
        blk = _block_at(dep_src, cm.end() - 1)
        nm = re.search(r'name\s*=\s*"([^"]+)"', blk)
        if nm and nm.group(1) == dep_name:
            return True
    return False


# Check whether a path_overwrite directive matches a known unresolved pattern
# tracked under children of https://fxbug.dev/584376026.
def _is_known_unresolved_path_overwrite(line: str) -> bool:
    m_dir = re.search(r"#\s*@bazel2gn:path_overwrite:([^\s#]+)", line)
    if not m_dir:
        return False
    gn_val = m_dir.group(1).strip()
    code_part = line.split("#", 1)[0]
    m_lbl = re.search(r'"([^"]+)"', code_part)
    if not m_lbl:
        return False
    bz_lbl = m_lbl.group(1).strip()

    # TODO(https://fxbug.dev/584478312): Remove once //src/lib/diagnostics/inspect/rust:fuchsia-inspect
    # path overwrites are resolved.
    if (
        bz_lbl == "//src/lib/diagnostics/inspect/rust:fuchsia-inspect"
        and gn_val
        in (
            "//src/lib/diagnostics/inspect/rust",
            "//src/lib/diagnostics/inspect/rust:fuchsia-inspect",
        )
    ):
        return True

    # TODO(https://fxbug.dev/584477958): Remove once diagnostics-hierarchy and block-client
    # target group label path overwrites are resolved.
    if (
        bz_lbl == "//src/lib/diagnostics/hierarchy/rust:diagnostics-hierarchy"
        and gn_val == "//src/lib/diagnostics/hierarchy/rust"
    ) or (
        bz_lbl == "//src/storage/lib/block_client/rust:block-client"
        and gn_val == "//src/storage/lib/block_client/rust"
    ):
        return True

    # TODO(https://fxbug.dev/584478340): Remove once GoogleTest and GMock dependency mappings
    # are resolved.
    if bz_lbl in ("@com_google_googletest//:gtest", ":gmock") and gn_val in (
        "//third_party/googletest:gmock",
        "//third_party/googletest:gtest",
    ):
        return True

    # TODO(https://fxbug.dev/584478304): Remove once in-tree Rust crate fork mappings
    # under //third_party/rust_crates/forks/ are resolved.
    if bz_lbl.startswith("//third_party/rust_crates/forks/") and (
        gn_val.startswith("//third_party/rust_crates:")
        or gn_val == "//third_party/rust_crates"
    ):
        return True

    # TODO(https://fxbug.dev/584478436): Remove once //src/zircon/lib/zircon:headers
    # path overwrites are resolved.
    if bz_lbl == "//src/zircon/lib/zircon:headers" and gn_val in (
        "//zircon/system/public",
        "//zircon/system/public:public",
    ):
        return True

    # TODO(https://fxbug.dev/584478342): Remove once external repository @fuchsia_perfetto_protos
    # mapping is resolved.
    if bz_lbl.startswith("@fuchsia_perfetto_protos//:") and gn_val.startswith(
        "//third_party/perfetto/"
    ):
        return True

    return False


# Check whether an overwrite directive matches a known unresolved pattern
# tracked under children of https://fxbug.dev/584376026.
def _is_known_unresolved_overwrite(
    fc: FileContext, ann: Bazel2gnCommentAnnotation, pkg_dir: str, line: str
) -> bool:
    directive = ann.directive
    comment = ann.comment_text

    # 1. Visibility overwrites in idk_*() and fidl_library()
    # TODO(https://fxbug.dev/584854632): Remove once idk and fidl_library visibility is resolved.
    if directive == "raw_overwrite" and (
        '[ "//*" ]' in comment
        or '[ "//zircon/system/ulib/ldmsg/*" ]' in comment
    ):
        call_name = ""
        for call in fc.calls:
            if (call.target_name and call.target_name in ann.targets) or (
                getattr(call.node, "lineno", 0)
                <= ann.lineno
                <= getattr(call.node, "end_lineno", 0)
            ):
                call_name = call.func_name
                break
        if call_name.startswith("idk_") or call_name == "fidl_library":
            return True

    # 2. Generated sources, headers, and crate_root in $target_gen_dir
    # TODO(https://fxbug.dev/584478079): Remove once generated source path overwrites are resolved.
    if directive == "path_overwrite":
        m_dir = re.search(r"#\s*@bazel2gn:path_overwrite:([^\s#]+)", line)
        if m_dir:
            gn_val = m_dir.group(1).strip()
            if gn_val.startswith(
                ("$target_gen_dir/", "${target_gen_dir}/", "${gwp_asan}/")
            ):
                return True
    if (
        directive == "raw_overwrite"
        and "crate_root" in line
        and ("$target_gen_dir" in comment or "${target_gen_dir}" in comment)
    ):
        return True

    # 3. Missing compiler configs in Bazel
    # TODO(https://fxbug.dev/584854533): Remove once missing build_flags() are added in Bazel.
    if directive == "raw_overwrite":
        for cfg in (
            "//build/config:no-shadow-call-stack",
            "//build/config:Wno-conversion",
            "//build/config:Wno-extra-semi",
            ":leaf-frame-pointer",
            ":stack_config",
            ":vdso_config",
            ":rust-wrapper.config",
        ):
            if cfg in comment:
                return True

    # 4. Rust environment variables and rebased paths
    # TODO(https://fxbug.dev/584854675): Remove once bazel2gn supports rustc_env path rebasing.
    if directive == "value_overwrite":
        for var in (
            "rebased_config_path",
            "relative_root_out_dir",
            "_domains_test_data_path",
            "_test_data_path",
            "_subtool_search_host_tools",
            "_subtool_search_test_data",
            "fxfs_blob",
        ):
            if var in comment:
                return True

    # 5. Go third-party subpackage dependencies
    # TODO(https://fxbug.dev/584854220): Remove once bazel2gn strips golibs subpackages.
    if directive == "path_overwrite" and "//third_party/golibs:" in line:
        m_dir = re.search(r"#\s*@bazel2gn:path_overwrite:([^\s#]+)", line)
        code_part = line.split("#", 1)[0]
        m_lbl = re.search(r'"([^"]+)"', code_part)
        if m_dir and m_lbl:
            bz_lbl = m_lbl.group(1).strip()
            gn_val = m_dir.group(1).strip()
            if bz_lbl.startswith("//third_party/golibs:") and gn_val.startswith(
                "//third_party/golibs:"
            ):
                return True

    # 6. Go test non_go_deps
    # TODO(https://fxbug.dev/584853822): Remove once non_go_deps handling is supported in bazel2gn.
    if directive == "raw_overwrite" and "non_go_deps" in comment:
        return True

    # 7. Policy exclusion: //:integration/MILESTONE
    # Excluded by policy (integration repository has no BUILD.bazel file).
    if directive == "path_overwrite" and "//:integration/MILESTONE" in line:
        return True

    # 8. compile_data / data labels vs file paths
    # TODO(https://fxbug.dev/584854141): Remove once compile_data label conversion is resolved.
    if any(
        k in comment
        for k in (
            "${dispatcher_cm}",
            "${elf_runner_cm}",
            "${config_path}",
            "icu_git_info.json",
            "base_epoch_file",
        )
    ):
        return True

    return False


@register_check
class BazelMigrationBazel2gnOverwritesCheck(BazelCheck):
    """Requires //build/bazel/BAZEL_OWNERS review of # @bazel2gn:*_overwrite comments."""

    name = "bazel_migration_bazel2gn_overwrites"
    file_kinds = frozenset({FileKind.BUILD, FileKind.BZL})
    supports_allowlist = True
    supports_targets_allowlist = True

    def check_module(self, fc: FileContext) -> None:
        annotations = find_bazel2gn_comment_annotations(fc, _OVERWRITE_REGEX)
        pkg_dir = os.path.dirname(fc.path)
        for ann in annotations:
            line_text = (
                fc.lines[ann.lineno - 1]
                if 1 <= ann.lineno <= len(fc.lines)
                else ""
            )
            if _is_known_unresolved_overwrite(fc, ann, pkg_dir, line_text) or (
                ann.directive == "path_overwrite"
                and (
                    _is_fx_packaged_binary_overwrite(
                        fc.root, pkg_dir, line_text
                    )
                    or _is_known_unresolved_path_overwrite(line_text)
                )
            ):
                continue

            fc.report(
                f"bazel2gn overwrite comment is an exception requiring further review: `{ann.comment_text}`. "
                "Use of bazel2gn overwrites requires approval by "
                "//build/bazel/BAZEL_OWNERS. Add an entry to "
                "//scripts/shac/bazel_lint/allowlists/bazel_migration_bazel2gn_overwrites.json "
                "with a tracking bug. When GN targets are removed and bazel2gn annotations "
                "deleted, the allowlist entry must be removed so //build/bazel/BAZEL_OWNERS can review the "
                "deleted comment to ensure the annotation was not hiding a bug.",
                ann.span_node,
                level="error",
                url=None,
                targets=ann.targets,
            )
