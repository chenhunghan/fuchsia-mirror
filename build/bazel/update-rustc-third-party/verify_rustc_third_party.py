#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Verifies that //third_party/rust_crates generated files are in sync.

Checks that Cargo.toml, Cargo.lock, BUILD.gn, crate_annotations.bzl,
crate_annotation_overwrites.bzl, and the Bazel files under
//third_party/rust_crates are consistent with `fx update-rustc-third-party`
without requiring network access or mutating the checkout.
"""

import argparse
import concurrent.futures
import dataclasses
import difflib
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import tomllib
from typing import Any

sys.path.insert(0, os.path.dirname(__file__))

import generate_crate_annotations
import postprocess_crates_vendor

SUPPORTED_BAZEL_TRIPLES: tuple[str, ...] = (
    "aarch64-unknown-fuchsia",
    "aarch64-unknown-linux-gnu",
    "riscv64gc-unknown-fuchsia",
    "x86_64-unknown-fuchsia",
    "x86_64-unknown-linux-gnu",
)


@dataclasses.dataclass(frozen=True)
class Finding:
    """A single out-of-sync finding."""

    filepath: str
    message: str


def _diff_excerpt(expected: str, actual: str, filepath: str) -> str:
    diff_lines = list(
        difflib.unified_diff(
            actual.splitlines(),
            expected.splitlines(),
            fromfile=f"a/{filepath}",
            tofile=f"b/{filepath}",
            lineterm="",
        )
    )
    if len(diff_lines) > 40:
        diff_lines = diff_lines[:40] + ["... (diff truncated)"]
    return "\n".join(diff_lines)


def verify_crate_annotations(
    fuchsia_dir: pathlib.Path,
    buildifier_bin: pathlib.Path | None = None,
) -> list[Finding]:
    """Verifies crate_annotations.bzl and crate_annotation_overwrites.bzl.

    Checks two invariants:
    1. Every entry in `crate_annotation_overwrites.bzl` corresponds to a crate
       and version present in `third_party/rust_crates/Cargo.toml` (via
       `generate_crate_annotations.validate_annotation_overwrites`).
    2. Regenerating the Starlark crate annotations in memory from `[gn.package]`
       sections in `Cargo.toml` merged with `crate_annotation_overwrites.bzl`
       (and formatting with `buildifier` when `buildifier_bin` is provided)
       matches the checked-in `crate_annotations.bzl` byte-for-byte.
    """
    cargo_toml_rel = "third_party/rust_crates/Cargo.toml"
    overwrites_rel = (
        "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl"
    )
    annotations_rel = (
        "build/bazel/update-rustc-third-party/crate_annotations.bzl"
    )

    cargo_toml_path = fuchsia_dir / cargo_toml_rel
    overwrites_path = fuchsia_dir / overwrites_rel
    actual_annotations = (fuchsia_dir / annotations_rel).read_text()

    cargo_data = generate_crate_annotations.parse_cargo_toml(
        str(cargo_toml_path)
    )
    annotation_overwrites = (
        generate_crate_annotations.parse_annotation_overwrites(
            str(overwrites_path)
        )
    )
    validation_errors = (
        generate_crate_annotations.validate_annotation_overwrites(
            annotation_overwrites, cargo_data
        )
    )
    if validation_errors:
        return [
            Finding(
                filepath=overwrites_rel,
                message=(
                    f"{overwrites_rel} is out of sync with {cargo_toml_rel}: "
                    f"{err}\nRun `fx update-rustc-third-party` to update."
                ),
            )
            for err in validation_errors
        ]

    gn_annotations = generate_crate_annotations.build_crate_annotations(
        cargo_data
    )
    merged = generate_crate_annotations.merge_annotations(
        gn_annotations, annotation_overwrites
    )
    expected_annotations = generate_crate_annotations.render_starlark(merged)

    if buildifier_bin is not None:
        if not buildifier_bin.is_file():
            raise FileNotFoundError(
                f"buildifier binary not found at {buildifier_bin}"
            )
        res = subprocess.run(
            [str(buildifier_bin), "-type=bzl"],
            input=expected_annotations,
            capture_output=True,
            text=True,
            check=True,
        )
        expected_annotations = res.stdout

    if expected_annotations != actual_annotations:
        diff = _diff_excerpt(
            expected_annotations, actual_annotations, annotations_rel
        )
        return [
            Finding(
                filepath=annotations_rel,
                message=(
                    f"{annotations_rel} is out of sync with {cargo_toml_rel} "
                    f"and {overwrites_rel}.\n"
                    f"Run `fx update-rustc-third-party` to regenerate.\n{diff}"
                ),
            )
        ]
    return []


def _check_crate_license_rules(
    build_file: pathlib.Path,
    fuchsia_dir: pathlib.Path,
    content: str,
) -> list[Finding]:
    """Checks `license(...)` rules in a crate `BUILD.bazel`."""
    crate_dir = build_file.parent
    rel_pkg = str(crate_dir.relative_to(fuchsia_dir))
    rel_build = str(build_file.relative_to(fuchsia_dir))
    findings: list[Finding] = []
    if 'license_text = "license"' in content and '":license"' in content:
        findings.append(
            Finding(
                filepath=rel_build,
                message=(
                    f"{rel_build} has an unrenamed ':license' target "
                    "colliding with a lowercase 'license' file. "
                    "Run `fx update-rustc-third-party`."
                ),
            )
        )
    if (
        fallback := postprocess_crates_vendor.license_needs_explicit_text(
            content, crate_dir
        )
    ) is not None:
        findings.append(
            Finding(
                filepath=rel_build,
                message=(
                    f"{rel_build} defines license() without license_text "
                    f"and {rel_pkg}/LICENSE does not exist (found "
                    f"{fallback!r}). Run `fx update-rustc-third-party`."
                ),
            )
        )
    return findings


def verify_postprocess_crates_vendor(
    fuchsia_dir: pathlib.Path,
    cargo_toml: dict[str, Any],
) -> list[Finding]:
    """Verifies post-processing invariants from postprocess_crates_vendor.py.

    Checks that:
    1. Every `group_visibility` variable declared under `[gn.package]` in
       `Cargo.toml` has a matching `package_group` in
       `third_party/rust_crates/BUILD.bazel`.
    2. Restricted crates set `default_visibility` to
       `//third_party/rust_crates:__subpackages__` in their `BUILD.bazel`, while
       unrestricted crates under `vendor/` keep `//visibility:public`.
    3. Crates under `vendor/` with a lowercase `license` file do not have a
       colliding `:license` target name.
    4. Every `alias()` in `third_party/rust_crates/vendor/BUILD.bazel` points to
       an existing crate directory and carries the expected `visibility`
       restriction from `Cargo.toml`.
    """
    findings: list[Finding] = []
    path_overrides = postprocess_crates_vendor.get_rust_package_paths(
        fuchsia_dir, cargo_toml
    )
    visibilities = postprocess_crates_vendor.get_rust_package_visibilities(
        fuchsia_dir, cargo_toml
    )
    fuchsia_license_crates = (
        postprocess_crates_vendor.get_rust_fuchsia_license_crates(
            fuchsia_dir, cargo_toml
        )
    )

    try:
        postprocess_crates_vendor.check_package_groups_exist(
            fuchsia_dir, visibilities
        )
    except ValueError as err:
        findings.append(
            Finding(
                filepath="third_party/rust_crates/BUILD.bazel",
                message=f"{err}\nRun `fx update-rustc-third-party` to sync.",
            )
        )
        return findings

    try:
        pkg_path_to_vis = (
            postprocess_crates_vendor.get_restricted_crate_packages(
                fuchsia_dir, path_overrides, visibilities
            )
        )
        fuchsia_license_pkgs = (
            postprocess_crates_vendor.get_fuchsia_license_crate_packages(
                fuchsia_dir, path_overrides, fuchsia_license_crates
            )
        )
    except ValueError as err:
        findings.append(
            Finding(
                filepath="third_party/rust_crates/Cargo.toml",
                message=f"{err}\nRun `fx update-rustc-third-party` to sync.",
            )
        )
        return findings

    # Verify restricted vs public default_visibility on crate BUILD.bazel files.
    vendor_dir = fuchsia_dir / "third_party/rust_crates/vendor"
    for crate_dir in sorted(vendor_dir.iterdir()):
        if not crate_dir.is_dir():
            continue
        build_file = crate_dir / "BUILD.bazel"
        if not build_file.is_file():
            continue
        rel_pkg = str(crate_dir.relative_to(fuchsia_dir))
        rel_build = str(build_file.relative_to(fuchsia_dir))
        content = build_file.read_text()
        if rel_pkg in pkg_path_to_vis:
            if (
                postprocess_crates_vendor._RESTRICTED_DEFAULT_VISIBILITY
                not in content
            ):
                findings.append(
                    Finding(
                        filepath=rel_build,
                        message=(
                            f"Restricted crate {rel_pkg} must have "
                            f"{postprocess_crates_vendor._RESTRICTED_DEFAULT_VISIBILITY}. "
                            "Run `fx update-rustc-third-party`."
                        ),
                    )
                )
        else:
            if (
                postprocess_crates_vendor._PUBLIC_DEFAULT_VISIBILITY
                not in content
            ):
                findings.append(
                    Finding(
                        filepath=rel_build,
                        message=(
                            f"Unrestricted crate {rel_pkg} has restricted "
                            "default_visibility, but no group_visibility in "
                            "Cargo.toml. Run `fx update-rustc-third-party`."
                        ),
                    )
                )
        findings.extend(
            _check_crate_license_rules(build_file, fuchsia_dir, content)
        )

    for sub in ("forks", "intree"):
        sub_dir = fuchsia_dir / "third_party/rust_crates" / sub
        if not sub_dir.is_dir():
            continue
        for build_file in sorted(
            dirpath / "BUILD.bazel"
            for dirpath, _, filenames in sub_dir.walk(follow_symlinks=True)
            if "BUILD.bazel" in filenames
        ):
            findings.extend(
                _check_crate_license_rules(
                    build_file, fuchsia_dir, build_file.read_text()
                )
            )

    for pkg_path in sorted(fuchsia_license_pkgs):
        build_file = fuchsia_dir / pkg_path / "BUILD.bazel"
        if not build_file.is_file():
            continue
        rel_build = str(build_file.relative_to(fuchsia_dir))
        content = build_file.read_text()
        if not postprocess_crates_vendor.has_fuchsia_license(content):
            findings.append(
                Finding(
                    filepath=rel_build,
                    message=(
                        f"Fuchsia-licensed crate {pkg_path} must include "
                        '"//:license" in package default_package_metadata or '
                        "default_applicable_licenses. Run "
                        "`fx update-rustc-third-party`."
                    ),
                )
            )

    for pkg_path in sorted(pkg_path_to_vis):
        build_file = fuchsia_dir / pkg_path / "BUILD.bazel"
        if not build_file.is_file():
            continue
        rel_build = str(build_file.relative_to(fuchsia_dir))
        content = build_file.read_text()
        if (
            postprocess_crates_vendor._RESTRICTED_DEFAULT_VISIBILITY
            not in content
        ):
            findings.append(
                Finding(
                    filepath=rel_build,
                    message=(
                        f"Restricted crate {pkg_path} must have "
                        f"{postprocess_crates_vendor._RESTRICTED_DEFAULT_VISIBILITY}. "
                        "Run `fx update-rustc-third-party`."
                    ),
                )
            )

    # Verify alias targets and visibilities in vendor/BUILD.bazel.
    vendor_build_rel = "third_party/rust_crates/vendor/BUILD.bazel"
    vendor_build_content = (fuchsia_dir / vendor_build_rel).read_text()
    alias_pattern = re.compile(
        r'(alias\(\s*name\s*=\s*"([^"]+)",\s*actual\s*=\s*"([^"]+)"\s*,?.*?\)\s*)',
        re.DOTALL,
    )
    for match in alias_pattern.finditer(vendor_build_content):
        full_block, alias_name, actual_target = (
            match.group(1),
            match.group(2),
            match.group(3),
        )
        if not actual_target.startswith("//"):
            continue
        parts = actual_target[2:].split(":")
        if len(parts) != 2:
            continue
        pkg_path, _ = parts
        if not (fuchsia_dir / pkg_path).is_dir():
            findings.append(
                Finding(
                    filepath=vendor_build_rel,
                    message=(
                        f"Alias '{alias_name}' in {vendor_build_rel} points to "
                        f"non-existent package '{actual_target}'. "
                        "Run `fx update-rustc-third-party`."
                    ),
                )
            )
            continue
        expected_vis = pkg_path_to_vis.get(pkg_path)
        vis_match = re.search(r'\bvisibility\s*=\s*\["([^"]+)"\]', full_block)
        actual_vis = vis_match.group(1) if vis_match else None
        if actual_vis != expected_vis:
            findings.append(
                Finding(
                    filepath=vendor_build_rel,
                    message=(
                        f"Alias '{alias_name}' in {vendor_build_rel} has "
                        f"visibility={actual_vis!r}, expected {expected_vis!r}. "
                        "Run `fx update-rustc-third-party`."
                    ),
                )
            )

    return findings


def verify_vendor_dir(fuchsia_dir: pathlib.Path) -> list[Finding]:
    """Verifies .cargo-checksum.json and license files in vendor/*.

    Iterates over every crate directory in `third_party/rust_crates/vendor/` to
    check that:
    1. A license file (`LICENSE*`, `LICENCE*`, or `license*`) is present.
    2. `.cargo-checksum.json` exists, parses as valid JSON, and the SHA-256
       digests of key manifest and entrypoint files (`Cargo.toml`,
       `Cargo.toml.orig`, `build.rs`, `src/lib.rs`, `src/main.rs`) match the
       recorded checksums so in-place edits to vendored crates are caught.
    """
    findings: list[Finding] = []
    vendor_dir = fuchsia_dir / "third_party/rust_crates/vendor"
    for crate_dir in sorted(vendor_dir.iterdir()):
        if crate_dir.name.startswith(".") or not crate_dir.is_dir():
            continue
        rel_dir = str(crate_dir.relative_to(fuchsia_dir))

        # Check license file (mirrors //scripts/rust/check_rust_licenses.py).
        has_license = any(
            name.startswith(("LICENSE", "LICENCE", "license"))
            for name in os.listdir(crate_dir)
        )
        if not has_license:
            findings.append(
                Finding(
                    filepath=rel_dir,
                    message=f"Missing LICENSE file in {rel_dir}.",
                )
            )

        checksum_path = crate_dir / ".cargo-checksum.json"
        rel_checksum = str(checksum_path.relative_to(fuchsia_dir))
        if not checksum_path.is_file():
            findings.append(
                Finding(
                    filepath=rel_dir,
                    message=(
                        f"Missing {rel_checksum}. Vendored crates must be "
                        "generated by `fx update-rustc-third-party`."
                    ),
                )
            )
            continue

        try:
            checksum_data = json.loads(checksum_path.read_text())
        except json.JSONDecodeError as err:
            findings.append(
                Finding(
                    filepath=rel_checksum,
                    message=f"Invalid JSON in {rel_checksum}: {err}",
                )
            )
            continue

        files_map = checksum_data.get("files", {})
        for key_file in (
            "Cargo.toml",
            "Cargo.toml.orig",
            "build.rs",
            "src/lib.rs",
            "src/main.rs",
        ):
            if key_file in files_map:
                file_path = crate_dir / key_file
                rel_file = str(file_path.relative_to(fuchsia_dir))
                if not file_path.is_file():
                    findings.append(
                        Finding(
                            filepath=rel_file,
                            message=(
                                f"Vendored file {rel_file} listed in "
                                f"{rel_checksum} is missing."
                            ),
                        )
                    )
                    continue
                digest = hashlib.sha256(file_path.read_bytes()).hexdigest()
                if digest != files_map[key_file]:
                    findings.append(
                        Finding(
                            filepath=rel_file,
                            message=(
                                f"Vendored file {rel_file} was modified "
                                f"(SHA-256 mismatch with {rel_checksum}). "
                                "Do not edit vendored crates in-place; move to "
                                "//third_party/rust_crates/forks/ or re-run "
                                "`fx update-rustc-third-party`."
                            ),
                        )
                    )

    return findings


def _run_cargo_queries(
    fuchsia_dir: pathlib.Path,
    cargo_bin: pathlib.Path,
    rustc_bin: pathlib.Path,
) -> tuple[dict[str, Any] | None, list[str], list[Finding]]:
    """Runs `cargo metadata --frozen --offline` and `cargo tree --frozen --offline`."""
    manifest_rel = "third_party/rust_crates/Cargo.toml"
    manifest_path = fuchsia_dir / manifest_rel
    with tempfile.TemporaryDirectory() as cargo_home:
        env = {
            "RUSTC": str(rustc_bin),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": cargo_home,
            "CARGO_HOME": cargo_home,
        }

        def run_cargo(args: list[str]) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                [str(cargo_bin)] + args,
                cwd=fuchsia_dir / "third_party/rust_crates",
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )

        with concurrent.futures.ThreadPoolExecutor(
            max_workers=1 + len(SUPPORTED_BAZEL_TRIPLES)
        ) as executor:
            meta_fut = executor.submit(
                run_cargo,
                [
                    "metadata",
                    "--format-version",
                    "1",
                    "--frozen",
                    "--offline",
                    "--manifest-path",
                    str(manifest_path),
                ],
            )
            tree_futs = [
                executor.submit(
                    run_cargo,
                    [
                        "tree",
                        "--frozen",
                        "--offline",
                        "--manifest-path",
                        str(manifest_path),
                        "--edges",
                        "normal,build,dev",
                        "--prefix=depth",
                        "--format=;{p};{f};",
                        "--color=never",
                        "--charset=ascii",
                        "--workspace",
                        "--target",
                        triple,
                    ],
                )
                for triple in SUPPORTED_BAZEL_TRIPLES
            ]

            meta_res = meta_fut.result()
            if meta_res.returncode != 0:
                return (
                    None,
                    [],
                    [
                        Finding(
                            filepath="third_party/rust_crates/Cargo.lock",
                            message=(
                                "`cargo metadata --frozen --offline` failed; "
                                "Cargo.lock or vendor/ is out of sync with "
                                "Cargo.toml.\nRun `fx update-rustc-third-party`.\n"
                                f"{meta_res.stderr.strip()}"
                            ),
                        )
                    ],
                )

            tree_outs: list[str] = []
            for fut in tree_futs:
                tree_res = fut.result()
                if tree_res.returncode != 0:
                    return (
                        None,
                        [],
                        [
                            Finding(
                                filepath="third_party/rust_crates/Cargo.lock",
                                message=(
                                    "`cargo tree --frozen --offline` failed.\n"
                                    "Run `fx update-rustc-third-party`.\n"
                                    f"{tree_res.stderr.strip()}"
                                ),
                            )
                        ],
                    )
                tree_outs.append(tree_res.stdout)

        return json.loads(meta_res.stdout), tree_outs, []


def _pkg_gn_target_name(name: str, version: str) -> str:
    return f"{name}-v{version.replace('.', '_')}"


def verify_build_gn(
    fuchsia_dir: pathlib.Path,
    cargo_toml: dict[str, Any],
    meta: dict[str, Any],
) -> list[Finding]:
    """Verifies //third_party/rust_crates/BUILD.gn against Cargo.toml and metadata.

    Compares the cargo-gnaw generated `BUILD.gn` against `Cargo.toml` and the
    resolved `cargo metadata` graph by checking that:
    1. Every `[gn.package.<name>.<version>]` block in `Cargo.toml` matches a
       crate version present in the resolved Cargo graph.
    2. Top-level `group("<dep>")` rules exist in `BUILD.gn` for all workspace
       dependencies declared in `Cargo.toml` (honoring `group_name` renames).
    3. Every normal-dependency library/proc-macro crate reachable from the
       workspace root (unless redirected via `existing_gn_target`) has a
       corresponding `rustc_library`, `rustc_macro`, or `icu_rustc_library`
       target in `BUILD.gn`, and no stale crate targets remain.
    4. Each target's enabled `--cfg=feature=...` flags, crate `deps`, and
       `[gn.package]` overrides (`rustflags`, `env_vars`, `configs`, `deps`)
       match the resolved graph and `Cargo.toml` configuration.
    """
    findings: list[Finding] = []
    build_gn_rel = "third_party/rust_crates/BUILD.gn"
    cargo_toml_rel = "third_party/rust_crates/Cargo.toml"
    build_gn = (fuchsia_dir / build_gn_rel).read_text()

    pkgs_by_id: dict[str, dict[str, Any]] = {
        p["id"]: p for p in meta["packages"]
    }
    nodes_by_id: dict[str, dict[str, Any]] = {
        n["id"]: n for n in meta["resolve"]["nodes"]
    }
    resolved_name_vers: set[tuple[str, str]] = {
        (pkgs_by_id[nid]["name"], pkgs_by_id[nid]["version"])
        for nid in nodes_by_id
        if pkgs_by_id[nid]["name"] != "fuchsia-third-party"
    }

    gn_pkg_cfgs: dict[str, dict[str, Any]] = cargo_toml.get("gn", {}).get(
        "package", {}
    )

    # 1. Check for unused [gn.package] configs in Cargo.toml.
    for pkg_name, versions in sorted(gn_pkg_cfgs.items()):
        if not isinstance(versions, dict):
            continue
        for version in sorted(versions):
            if (pkg_name, version) not in resolved_name_vers:
                findings.append(
                    Finding(
                        filepath=cargo_toml_rel,
                        message=(
                            f"[gn.package.{pkg_name}.{version!r}] in "
                            f"{cargo_toml_rel} does not match any crate in "
                            "the resolved Cargo graph. "
                            "Run `fx update-rustc-third-party`."
                        ),
                    )
                )

    # 2. Check top-level group("<dep>") targets in BUILD.gn vs Cargo.toml.
    gn_groups = set(
        re.findall(r'^\s*group\("([^"]+)"\)\s*\{', build_gn, re.MULTILINE)
    )
    expected_top_groups: set[str] = set()
    for dep in nodes_by_id[meta["resolve"]["root"]].get("deps", []):
        if not any(dk.get("kind") is None for dk in dep.get("dep_kinds", [])):
            continue
        dep_pkg = pkgs_by_id[dep["pkg"]]
        pkg_cfg = gn_pkg_cfgs.get(dep_pkg["name"], {}).get(
            dep_pkg["version"], {}
        )
        if "groups" in pkg_cfg:
            for group_cfg in pkg_cfg["groups"]:
                expected_top_groups.add(group_cfg["name"])
        elif "group_name" in pkg_cfg:
            expected_top_groups.add(pkg_cfg["group_name"])
        else:
            expected_top_groups.add(dep_pkg["name"])

    missing_groups = sorted(expected_top_groups - gn_groups)
    if missing_groups:
        findings.append(
            Finding(
                filepath=build_gn_rel,
                message=(
                    f"{build_gn_rel} is missing top-level group targets for "
                    f"Cargo.toml dependencies: {missing_groups}. "
                    "Run `fx update-rustc-third-party`."
                ),
            )
        )

    # 3. Parse rustc_library / rustc_macro / icu_rustc_library targets in BUILD.gn.
    gn_target_re = re.compile(
        r'^(?:rustc_library|rustc_macro|icu_rustc_library)\("([^"]+)"\)\s*\{(.*?)^\}',
        re.MULTILINE | re.DOTALL,
    )
    gn_feature_re = re.compile(r'--cfg=feature=\\"([^\\"]+)\\"')
    gn_dep_re = re.compile(r'deps\s*\+=\s*\[\s*":([^"($]+)')

    gn_targets: dict[str, dict[str, Any]] = {}
    for match in gn_target_re.finditer(build_gn):
        target_name, body = match.group(1), match.group(2)
        gn_targets[target_name] = {
            "features": set(gn_feature_re.findall(body)),
            "deps": set(gn_dep_re.findall(body)),
            "body": body,
        }

    # cargo-gnaw only emits GN targets for crates reachable via normal
    # (non-build-script) dependencies from the workspace root.
    root_id = meta["resolve"]["root"]
    reachable_normal_ids: set[str] = set()
    queue = [root_id]
    while queue:
        curr_id = queue.pop()
        if curr_id in reachable_normal_ids:
            continue
        reachable_normal_ids.add(curr_id)
        for dep in nodes_by_id[curr_id].get("deps", []):
            if any(dk.get("kind") is None for dk in dep.get("dep_kinds", [])):
                queue.append(dep["pkg"])

    expected_gn_targets: set[str] = set()
    for pkg_id in sorted(reachable_normal_ids):
        node = nodes_by_id[pkg_id]
        pkg = pkgs_by_id[pkg_id]
        name, version = pkg["name"], pkg["version"]
        if name == "fuchsia-third-party":
            continue
        pkg_cfg = gn_pkg_cfgs.get(name, {}).get(version, {})
        if "existing_gn_target" in pkg_cfg:
            continue
        # Only crates with a lib / rlib / proc-macro target get a library rule.
        has_lib = any(
            any(k in t.get("kind", []) for k in ("lib", "rlib", "proc-macro"))
            for t in pkg.get("targets", [])
        )
        if not has_lib:
            continue

        gn_name = _pkg_gn_target_name(name, version)
        expected_gn_targets.add(gn_name)
        if gn_name not in gn_targets:
            findings.append(
                Finding(
                    filepath=build_gn_rel,
                    message=(
                        f"{build_gn_rel} is missing target '{gn_name}' for "
                        f"resolved crate {name} {version}. "
                        "Run `fx update-rustc-third-party`."
                    ),
                )
            )
            continue

        # Check features.
        expected_features = set(node.get("features", []))
        if "features" in pkg_cfg:
            expected_features = set(pkg_cfg["features"])
        for plat_cfg in pkg_cfg.get("platform", {}).values():
            if "features" in plat_cfg:
                expected_features.update(plat_cfg["features"])
            for rf in plat_cfg.get("rustflags", []):
                for fm in re.finditer(r'--cfg=feature=\\"([^\\"]+)\\"', rf):
                    expected_features.add(fm.group(1))
        for rf in pkg_cfg.get("rustflags", []):
            for fm in re.finditer(r'--cfg=feature=\\"([^\\"]+)\\"', rf):
                expected_features.add(fm.group(1))

        actual_features = gn_targets[gn_name]["features"]
        if expected_features != actual_features:
            findings.append(
                Finding(
                    filepath=build_gn_rel,
                    message=(
                        f"Target '{gn_name}' in {build_gn_rel} has features "
                        f"{sorted(actual_features)}, expected "
                        f"{sorted(expected_features)} from Cargo.lock. "
                        "Run `fx update-rustc-third-party`."
                    ),
                )
            )

        # Check crate deps.
        expected_deps: set[str] = set()
        for dep in node.get("deps", []):
            if any(dk.get("kind") is None for dk in dep.get("dep_kinds", [])):
                dep_pkg = pkgs_by_id[dep["pkg"]]
                expected_deps.add(
                    _pkg_gn_target_name(dep_pkg["name"], dep_pkg["version"])
                )
        for d_str in pkg_cfg.get("deps", []):
            if d_str.startswith(":"):
                expected_deps.add(d_str[1:].split("(")[0])
        for plat_cfg in pkg_cfg.get("platform", {}).values():
            for d_str in plat_cfg.get("deps", []):
                if d_str.startswith(":"):
                    expected_deps.add(d_str[1:].split("(")[0])

        actual_deps = gn_targets[gn_name]["deps"]
        if expected_deps != actual_deps:
            findings.append(
                Finding(
                    filepath=build_gn_rel,
                    message=(
                        f"Target '{gn_name}' in {build_gn_rel} has crate deps "
                        f"{sorted(actual_deps)}, expected "
                        f"{sorted(expected_deps)} from Cargo.lock. "
                        "Run `fx update-rustc-third-party`."
                    ),
                )
            )

        # Check [gn.package] rustflags, env_vars, configs, and non-crate deps.
        body = gn_targets[gn_name]["body"]
        all_gn_cfgs = [pkg_cfg] + list(pkg_cfg.get("platform", {}).values())
        for cfg_block in all_gn_cfgs:
            if not isinstance(cfg_block, dict):
                continue
            for attr_key in ("rustflags", "env_vars", "configs", "deps"):
                for item in cfg_block.get(attr_key, []):
                    if item not in body:
                        findings.append(
                            Finding(
                                filepath=build_gn_rel,
                                message=(
                                    f"Target '{gn_name}' in {build_gn_rel} is "
                                    f"missing {attr_key} entry {item!r} from "
                                    f"[gn.package.{name}.{version!r}]. "
                                    "Run `fx update-rustc-third-party`."
                                ),
                            )
                        )

    extra_gn_targets = sorted(set(gn_targets) - expected_gn_targets)
    if extra_gn_targets:
        findings.append(
            Finding(
                filepath=build_gn_rel,
                message=(
                    f"{build_gn_rel} contains stale targets not in Cargo.lock: "
                    f"{extra_gn_targets}. Run `fx update-rustc-third-party`."
                ),
            )
        )

    return findings


def verify_bazel_files(
    fuchsia_dir: pathlib.Path,
    cargo_toml: dict[str, Any],
    meta: dict[str, Any],
    tree_outs: list[str],
) -> list[Finding]:
    """Verifies vendor/BUILD.bazel, vendor/crates.bzl, and per-crate BUILD.bazel files.

    Cross-checks the `crates_vendor` generated Bazel rules against `Cargo.toml`,
    `cargo metadata`, and `cargo tree` outputs across `SUPPORTED_BAZEL_TRIPLES`
    by verifying that:
    1. `_NORMAL_DEPENDENCIES` and `_PROC_MACRO_DEPENDENCIES` in
       `vendor/crates.bzl` and `alias()` rules in `vendor/BUILD.bazel` match the
       non-kernel workspace dependencies in `Cargo.toml`.
    2. The set of `vendor/<name>-<version>` directories matches the crates.io
       packages in `Cargo.lock`.
    3. Crate annotations from `crate_annotations.bzl` (`gen_build_script`,
       `deps`, `rustc_flags`, `build_script_env`, etc.) are reflected in the
       corresponding `vendor/<name>-<version>/BUILD.bazel` files.
    4. Each crate's `rust_library` / `rust_proc_macro` rule has `crate_features`
       and `deps` / `proc_macro_deps` matching the union of features and normal
       dependencies resolved by `cargo tree` across all supported target triples
       (unless explicitly overridden in `crate_annotation_overwrites.bzl`).
    """
    findings: list[Finding] = []
    vendor_build_rel = "third_party/rust_crates/vendor/BUILD.bazel"
    crates_bzl_rel = "third_party/rust_crates/vendor/crates.bzl"
    overwrites_rel = (
        "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl"
    )

    pkgs_by_id: dict[str, dict[str, Any]] = {
        p["id"]: p for p in meta["packages"]
    }
    nodes_by_id: dict[str, dict[str, Any]] = {
        n["id"]: n for n in meta["resolve"]["nodes"]
    }

    # 1. Verify top-level dependencies in Cargo.toml vs vendor/crates.bzl & vendor/BUILD.bazel.
    path_overrides = postprocess_crates_vendor.get_rust_package_paths(
        fuchsia_dir, cargo_toml
    )
    pkg_name_to_lib: dict[str, str] = {}
    for pkg in pkgs_by_id.values():
        for t in pkg.get("targets", []):
            if any(
                k in t.get("kind", []) for k in ("lib", "rlib", "proc-macro")
            ):
                pkg_name_to_lib[pkg["name"]] = t["name"]

    expected_crates_bzl_keys: set[str] = set()
    expected_vendor_aliases: set[str] = set()

    def _add_top_dep(dep_key: str, dep_spec: Any) -> None:
        if isinstance(dep_spec, dict) and "path" in dep_spec:
            return
        is_renamed = isinstance(dep_spec, dict) and "package" in dep_spec
        bazel_dep_name = dep_key.replace("-", "_") if is_renamed else dep_key
        expected_crates_bzl_keys.add(bazel_dep_name)
        pkg_name = (
            dep_spec.get("package", dep_key)
            if isinstance(dep_spec, dict)
            else dep_key
        )
        # If a patched crate's library target name differs from its package name
        # and is not in path_overrides (e.g. prettytable-rs -> prettytable),
        # postprocess_crates_vendor.py removes its dangling alias.
        if pkg_name in path_overrides:
            lib_name = pkg_name_to_lib.get(pkg_name, pkg_name)
            if (
                lib_name not in path_overrides
                and lib_name.replace("_", "-") not in path_overrides
            ):
                return
        expected_vendor_aliases.add(bazel_dep_name)

    for k, v in cargo_toml.get("dependencies", {}).items():
        _add_top_dep(k, v)
    for target_cfg, tbl in cargo_toml.get("target", {}).items():
        if "kernel" in target_cfg:
            continue
        for k, v in tbl.get("dependencies", {}).items():
            _add_top_dep(k, v)

    crates_bzl = (fuchsia_dir / crates_bzl_rel).read_text()
    actual_crates_bzl_keys: set[str] = set()
    for map_name in ("_NORMAL_DEPENDENCIES", "_PROC_MACRO_DEPENDENCIES"):
        m = re.search(
            rf'{map_name}\s*=\s*\{{\s*"third_party/rust_crates":\s*\{{(.*?)\n    \}},',
            crates_bzl,
            re.DOTALL,
        )
        if m:
            actual_crates_bzl_keys.update(
                re.findall(r'"([^"]+)":\s*Label\(', m.group(1))
            )

    if expected_crates_bzl_keys != actual_crates_bzl_keys:
        missing = sorted(expected_crates_bzl_keys - actual_crates_bzl_keys)
        extra = sorted(actual_crates_bzl_keys - expected_crates_bzl_keys)
        findings.append(
            Finding(
                filepath=crates_bzl_rel,
                message=(
                    f"{crates_bzl_rel} is out of sync with Cargo.toml "
                    f"(missing={missing}, extra={extra}). "
                    "Run `fx update-rustc-third-party`."
                ),
            )
        )

    vendor_build_bazel = (fuchsia_dir / vendor_build_rel).read_text()
    actual_alias_names = set(
        re.findall(r'alias\(\s*name\s*=\s*"([^"]+)"', vendor_build_bazel)
    )
    missing_aliases = sorted(expected_vendor_aliases - actual_alias_names)
    if missing_aliases:
        findings.append(
            Finding(
                filepath=vendor_build_rel,
                message=(
                    f"{vendor_build_rel} is missing aliases for Cargo.toml "
                    f"dependencies: {missing_aliases}. "
                    "Run `fx update-rustc-third-party`."
                ),
            )
        )

    # 2. Check that every crates.io package in Cargo.lock matches vendor/<name>-<ver>.
    expected_vendor_dirs: set[str] = set()
    for pkg_id in nodes_by_id:
        pkg = pkgs_by_id[pkg_id]
        name, version = pkg["name"], pkg["version"]
        if name == "fuchsia-third-party":
            continue
        if pkg.get("source") is None:
            continue
        expected_vendor_dirs.add(f"{name}-{version.replace('+', '-')}")

    vendor_dir = fuchsia_dir / "third_party/rust_crates/vendor"
    actual_vendor_dirs = {
        d.name
        for d in vendor_dir.iterdir()
        if d.is_dir() and not d.name.startswith(".")
    }
    missing_dirs = sorted(expected_vendor_dirs - actual_vendor_dirs)
    extra_dirs = sorted(actual_vendor_dirs - expected_vendor_dirs)
    if missing_dirs or extra_dirs:
        findings.append(
            Finding(
                filepath="third_party/rust_crates/vendor",
                message=(
                    "Vendor directory packages do not match Cargo.lock "
                    f"(missing={missing_dirs}, extra={extra_dirs}). "
                    "Run `fx update-rustc-third-party`."
                ),
            )
        )

    # 3. Parse cargo tree outputs for per-crate features and dependency edges.
    normal_dep_targets: dict[tuple[str, str], set[str]] = {}
    pkg_lib_name: dict[tuple[str, str], str] = {}
    for pkg_id, node in nodes_by_id.items():
        pkg = pkgs_by_id[pkg_id]
        key = (pkg["name"], pkg["version"])
        for t in pkg.get("targets", []):
            if any(
                k in t.get("kind", []) for k in ("lib", "rlib", "proc-macro")
            ):
                pkg_lib_name[key] = t["name"].replace("-", "_")
        deps_set: set[str] = set()
        for dep in node.get("deps", []):
            if any(dk.get("kind") is None for dk in dep.get("dep_kinds", [])):
                dep_pkg = pkgs_by_id[dep["pkg"]]
                for t in dep_pkg.get("targets", []):
                    if any(
                        k in t.get("kind", [])
                        for k in ("lib", "rlib", "proc-macro")
                    ):
                        deps_set.add(t["name"].replace("-", "_"))
        normal_dep_targets[key] = deps_set

    tree_features: dict[tuple[str, str], set[str]] = {}
    tree_deps: dict[tuple[str, str], set[str]] = {}
    for out in tree_outs:
        stack: list[tuple[int, tuple[str, str]]] = []
        for line in out.splitlines():
            parts = line.split(";")
            if len(parts) < 3:
                continue
            depth_str, pkg_part, feat_part = (
                parts[0].strip(),
                parts[1],
                parts[2],
            )
            if not depth_str.isdigit():
                continue
            depth = int(depth_str)
            m = re.match(r"^([a-zA-Z0-9_-]+)\s+v([0-9][^\s]*)", pkg_part)
            if not m:
                continue
            node_key = (m.group(1), m.group(2))
            feats = {f for f in feat_part.split(",") if f}
            tree_features.setdefault(node_key, set()).update(feats)
            tree_deps.setdefault(node_key, set())
            while stack and stack[-1][0] >= depth:
                stack.pop()
            if stack:
                parent_key = stack[-1][1]
                child_lib = pkg_lib_name.get(
                    node_key, node_key[0].replace("-", "_")
                )
                if child_lib in normal_dep_targets.get(parent_key, set()):
                    tree_deps[parent_key].add(child_lib)
            stack.append((depth, node_key))

    # Include features enabled by proc-macro crates (cargo-bazel splices all
    # transitive proc-macros as direct workspace dependencies).
    for pkg_id, pkg in pkgs_by_id.items():
        is_proc_macro = any(
            "proc-macro" in t.get("kind", []) for t in pkg.get("targets", [])
        )
        if not is_proc_macro or pkg_id not in nodes_by_id:
            continue
        dep_reqs = {d["name"]: d for d in pkg.get("dependencies", [])}
        for dep in nodes_by_id[pkg_id].get("deps", []):
            dep_pkg = pkgs_by_id[dep["pkg"]]
            req = dep_reqs.get(dep_pkg["name"])
            if req:
                tree_features.setdefault(
                    (dep_pkg["name"], dep_pkg["version"]), set()
                ).update(req.get("features", []))

    overwrites = generate_crate_annotations.parse_annotation_overwrites(
        str(fuchsia_dir / overwrites_rel)
    )
    cargo_ann_data = generate_crate_annotations.parse_cargo_toml(
        str(fuchsia_dir / "third_party/rust_crates/Cargo.toml")
    )
    merged_annotations = generate_crate_annotations.merge_annotations(
        generate_crate_annotations.build_crate_annotations(cargo_ann_data),
        overwrites,
    )
    for ann_pkg, anns in sorted(merged_annotations.items()):
        for ann in anns:
            ver_dir = ann.version.replace("+", "-")
            bf = vendor_dir / f"{ann_pkg}-{ver_dir}" / "BUILD.bazel"
            if not bf.is_file():
                continue
            rel_bf = str(bf.relative_to(fuchsia_dir))
            text = bf.read_text()
            for attr, val in ann.attributes.items():
                if attr == "gen_build_script" and val is False:
                    if "cargo_build_script(" in text:
                        findings.append(
                            Finding(
                                filepath=rel_bf,
                                message=(
                                    f"{rel_bf} contains cargo_build_script(), "
                                    "but gen_build_script = False in "
                                    "crate_annotations.bzl. "
                                    "Run `fx update-rustc-third-party`."
                                ),
                            )
                        )
                elif isinstance(val, list):
                    for item in val:
                        escaped = item.replace('"', '\\"')
                        if item not in text and escaped not in text:
                            findings.append(
                                Finding(
                                    filepath=rel_bf,
                                    message=(
                                        f"{rel_bf} is missing {attr} entry "
                                        f"{item!r} from crate_annotations.bzl. "
                                        "Run `fx update-rustc-third-party`."
                                    ),
                                )
                            )
                elif isinstance(val, dict):
                    for k, v in val.items():
                        v_items = [v] if isinstance(v, str) else v
                        if k not in text or any(
                            vi not in text for vi in v_items
                        ):
                            findings.append(
                                Finding(
                                    filepath=rel_bf,
                                    message=(
                                        f"{rel_bf} is missing {attr} entry "
                                        f"{k!r}: {v!r} from crate_annotations.bzl. "
                                        "Run `fx update-rustc-third-party`."
                                    ),
                                )
                            )
                elif isinstance(val, generate_crate_annotations.CrateSelect):
                    for item in val.common:
                        escaped = item.replace('"', '\\"')
                        if item not in text and escaped not in text:
                            findings.append(
                                Finding(
                                    filepath=rel_bf,
                                    message=(
                                        f"{rel_bf} is missing {attr} entry "
                                        f"{item!r} from crate_annotations.bzl. "
                                        "Run `fx update-rustc-third-party`."
                                    ),
                                )
                            )
                    for items in val.selects.values():
                        for item in items:
                            escaped = item.replace('"', '\\"')
                            if item not in text and escaped not in text:
                                findings.append(
                                    Finding(
                                        filepath=rel_bf,
                                        message=(
                                            f"{rel_bf} is missing {attr} entry "
                                            f"{item!r} from crate_annotations.bzl. "
                                            "Run `fx update-rustc-third-party`."
                                        ),
                                    )
                                )
    overwritten_feature_crates = {
        name
        for name, anns in overwrites.items()
        if any("crate_features" in a.attributes for a in anns)
    }
    overwritten_dep_crates = {
        name
        for name, anns in overwrites.items()
        if any("deps" in a.attributes for a in anns)
    }

    bazel_files = list(vendor_dir.glob("*/BUILD.bazel"))
    for sub in ("ask2patch", "empty", "forks", "intree"):
        sub_dir = fuchsia_dir / "third_party/rust_crates" / sub
        if sub_dir.is_dir():
            bazel_files.extend(sub_dir.rglob("BUILD.bazel"))

    bazel_rule_re = re.compile(
        r'^(rust_library|rust_proc_macro)\(\s*name\s*=\s*"([^"]+)",(.*?)^\)$',
        re.MULTILINE | re.DOTALL,
    )

    for bf in sorted(bazel_files):
        rel_bf = str(bf.relative_to(fuchsia_dir))
        text = bf.read_text()
        rule_m = bazel_rule_re.search(text)
        if not rule_m:
            continue
        body = rule_m.group(3)
        pkg_info_m = re.search(
            r'package_info\(\s*name\s*=\s*"package_info",\s*package_name\s*=\s*"([^"]+)",.*?package_version\s*=\s*"([^"]+)"',
            text,
            re.DOTALL,
        )
        if not pkg_info_m:
            continue
        pkg_name, version = pkg_info_m.group(1), pkg_info_m.group(2)
        key = (pkg_name, version)
        if key not in tree_features:
            continue

        if pkg_name not in overwritten_feature_crates:
            feat_block_m = re.search(
                r"\bcrate_features\s*=\s*(.*?)(?:\n    [a-z_]+\s*=|\Z)",
                body,
                re.DOTALL,
            )
            bazel_features: set[str] = set()
            if feat_block_m:
                for fm in re.finditer(r'"([^"]+)"', feat_block_m.group(1)):
                    val = fm.group(1)
                    if not val.startswith(("@", "//")):
                        bazel_features.add(val)
            expected_feats = tree_features[key]
            if bazel_features != expected_feats:
                findings.append(
                    Finding(
                        filepath=rel_bf,
                        message=(
                            f"{rel_bf} has crate_features "
                            f"{sorted(bazel_features)}, expected "
                            f"{sorted(expected_feats)} from Cargo graph. "
                            "Run `fx update-rustc-third-party`."
                        ),
                    )
                )

        if pkg_name not in overwritten_dep_crates and not rel_bf.startswith(
            "third_party/rust_crates/empty/"
        ):
            bazel_deps: set[str] = set()
            for attr in ("deps", "proc_macro_deps"):
                attr_m = re.search(
                    rf"\b{attr}\s*=\s*(.*?)(?:\n    [a-z_]+\s*=|\Z)",
                    body,
                    re.DOTALL,
                )
                if attr_m:
                    for lm in re.finditer(
                        r'"(//third_party/rust_crates/[^"]+)"', attr_m.group(1)
                    ):
                        label = lm.group(1)
                        target = (
                            label.split(":")[1]
                            if ":" in label
                            else label.rsplit("/", 1)[1]
                        )
                        bazel_deps.add(target.replace("-", "_"))
            expected_deps = tree_deps.get(key, set())
            if bazel_deps != expected_deps:
                findings.append(
                    Finding(
                        filepath=rel_bf,
                        message=(
                            f"{rel_bf} has crate deps {sorted(bazel_deps)}, "
                            f"expected {sorted(expected_deps)} from Cargo "
                            "graph. Run `fx update-rustc-third-party`."
                        ),
                    )
                )

    return findings


def verify_checkout(
    fuchsia_dir: pathlib.Path,
    cargo_bin: pathlib.Path,
    rustc_bin: pathlib.Path,
    buildifier_bin: pathlib.Path | None = None,
) -> list[Finding]:
    """Runs all offline consistency checks for //third_party/rust_crates.

    Parses `third_party/rust_crates/Cargo.toml`, verifies crate annotations,
    post-processed visibility/alias rules, and vendored checksums/licenses, then
    queries `cargo metadata --frozen --offline` and `cargo tree --frozen --offline`
    to verify `BUILD.gn` and `BUILD.bazel` files against the Cargo dependency
    graph.
    """
    findings: list[Finding] = []
    cargo_toml_rel = "third_party/rust_crates/Cargo.toml"
    try:
        with open(fuchsia_dir / cargo_toml_rel, "rb") as f:
            cargo_toml = tomllib.load(f)
    except tomllib.TOMLDecodeError as err:
        return [
            Finding(
                filepath=cargo_toml_rel,
                message=f"Invalid TOML in {cargo_toml_rel}: {err}",
            )
        ]

    findings.extend(verify_crate_annotations(fuchsia_dir, buildifier_bin))
    findings.extend(verify_postprocess_crates_vendor(fuchsia_dir, cargo_toml))
    findings.extend(verify_vendor_dir(fuchsia_dir))

    meta, tree_outs, cargo_findings = _run_cargo_queries(
        fuchsia_dir, cargo_bin, rustc_bin
    )
    findings.extend(cargo_findings)
    if meta is not None:
        findings.extend(verify_build_gn(fuchsia_dir, cargo_toml, meta))
        findings.extend(
            verify_bazel_files(fuchsia_dir, cargo_toml, meta, tree_outs)
        )

    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Verify //third_party/rust_crates files are in sync with fx update-rustc-third-party."
    )
    parser.add_argument(
        "--fuchsia-dir",
        type=pathlib.Path,
        required=True,
        help="Path to the Fuchsia source root.",
    )
    parser.add_argument(
        "--cargo",
        type=pathlib.Path,
        required=True,
        help="Path to the prebuilt cargo binary.",
    )
    parser.add_argument(
        "--rustc",
        type=pathlib.Path,
        required=True,
        help="Path to the prebuilt rustc binary.",
    )
    parser.add_argument(
        "--buildifier",
        type=pathlib.Path,
        required=True,
        help="Path to the prebuilt buildifier binary.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print findings as JSON to stdout.",
    )
    args = parser.parse_args(argv)

    findings = verify_checkout(
        fuchsia_dir=args.fuchsia_dir.resolve(),
        cargo_bin=args.cargo.resolve(),
        rustc_bin=args.rustc.resolve(),
        buildifier_bin=args.buildifier.resolve(),
    )

    if args.json:
        print(
            json.dumps(
                [dataclasses.asdict(f) for f in findings],
                indent=2,
            )
        )
        return 1 if findings else 0

    if findings:
        for finding in findings:
            print(
                f"ERROR: {finding.filepath}: {finding.message}\n",
                file=sys.stderr,
            )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
