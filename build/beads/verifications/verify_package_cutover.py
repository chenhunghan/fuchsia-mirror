#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that a Bazel package matches the GN package it replaces.

When a change cuts a Fuchsia package over from GN to Bazel, this tool builds
both packages and compares them:

  * The package content comparison compares the built packages: the blob
    paths, the decoded `.cm` files, structured config, the ABI revision,
    subpackages and ELF facts of each binary.
  * The build command comparison compares the compile and link commands of
    each binary in the package, using
    `//build/beads/scripts/compare_utils.py`, for x64 and arm64. The other
    CPU's commands come from an analysis-only out dir
    `<build dir>.cutover-<cpu>` (`gn gen` only, reused across runs).

The packages are built for the build directory's CPU, or for arm64 with
`--package-arch=arm64` (built in `<build dir>.cutover-arm64`). A package
that does not build is reported as `cutover_info_not_compared`.

Differences that `cutover_normalizations.json` lists as expected are dropped
and counted. The rest are reported as findings for Planter (`--json`) or as
text, each anchored at the BUILD.bazel target it is about. The severity of
each category comes from `cutover_categories.json` (stage 2 is WARNING,
otherwise INFO).

Cutover phases (see //build/beads/planter/machinery/prompts/coder.md):

  parallel  The GN package and the new Bazel package both exist. Both are
            built and compared, and a phase-1 record with the facts and
            build commands of both packages is written to
            `$PLANTER_CUTOVER_RECORD_DIR` (Planter keeps it per task) or
            else `$(git rev-parse --git-common-dir)/planter-cutover/`. When
            the Planter directory has no record for a package, a record
            this checkout left in the git common dir is used and copied
            there.
  final     The GN package was deleted. The Bazel package is built again and
            compared with the recorded GN package, and the differences found
            now are reported. The summary says whether the Bazel package
            changed since phase 1 and which files changed; differences
            that phase 1 did not report after such a change also get a
            `cutover_diff_guard`, which needs a verdict. A record from
            before version 2 has no GN facts: its findings are reported again
            and the Bazel `meta.far` merkle is compared with the recorded one
            (the guard, `cutover_diff_guard`).

Examples:

  # What Planter runs (detects cutovers in the change):
  verify_package_cutover.py --json

  # Replay a landed cutover: capture each side at its revision, then compare.
  verify_package_cutover.py --capture gn --label //src/foo:pkg --out gn.json
  verify_package_cutover.py --capture bazel --label //src/foo:pkg --out bazel.json
  verify_package_cutover.py --gn-facts gn.json --bazel-facts bazel.json
"""

import argparse
import fcntl
import itertools
import json
import os
import pathlib
import re
import shlex
import subprocess
import sys
import time
import typing as T

_FUCHSIA_DIR = pathlib.Path(__file__).resolve().parent.parent.parent.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.beads.scripts import cutover_utils as cu

SOURCE = "cutover_consistency"
NORMALIZATIONS_JSON = (
    pathlib.Path(__file__).parent / "cutover_normalizations.json"
)
BAZEL_CONFIG = "fuchsia_platform"
# normalize_rustc_args.py drops -Copt-level; the cutover check compares it.
_OPT_LEVEL_RE = re.compile(r"(?:-C\s*|--codegen[=\s])opt-level=(\S+)")
MANIFEST_RULES = ("fx_component_manifest", "fx_unittest_component")
BINARY_RULES = (
    "fx_cc_binary",
    "fx_cc_test_binary",
    "cc_binary",
    "rustc_binary",
    "rustc_test",
    "rust_binary",
    "fx_rust_binary",
)


def log(msg: str) -> None:
    print(f"[verify_package_cutover] {msg}", file=sys.stderr, flush=True)


class Checkout:
    """Builds and locates packages in one Fuchsia checkout.

    The build directory's CPU is the primary one. Commands for another CPU come
    from a reusable, analysis-only GN out dir next to it
    (`<build dir>.cutover-<cpu>`), which is never built unless the package
    content comparison is asked to run on that CPU (`package_arch`).
    """

    def __init__(
        self,
        workdir: str,
        build_dir: T.Optional[str] = None,
        package_arch: str = "",
    ):
        self.workdir = os.path.abspath(workdir)
        if not build_dir:
            rel = (
                cu.read_text(os.path.join(self.workdir, ".fx-build-dir")) or ""
            ).strip()
            build_dir = os.path.join(self.workdir, rel) if rel else ""
        self.build_dir = os.path.abspath(build_dir) if build_dir else ""
        self.fx = os.path.join(self.workdir, "scripts", "fx")
        self._lock: T.Optional[T.IO[str]] = None
        # Bazel package manifests name blobs relative to the execroot.
        self.execroot = ""
        self.build_cpu = "x64"
        if self.build_dir:
            try:
                with open(os.path.join(self.build_dir, "args.json")) as f:
                    self.build_cpu = str(json.load(f).get("target_cpu", "x64"))
            except (OSError, ValueError):
                pass
        self.package_cpu = package_arch or self.build_cpu
        self._fresh_cpu_dirs: set[str] = set()

    @property
    def other_cpu(self) -> str:
        return "arm64" if self.build_cpu != "arm64" else "x64"

    def lock(self) -> None:
        """Takes the workdir build lock that build_verification also takes."""
        if self._lock:
            return
        gdir = cu.git(self.workdir, ["rev-parse", "--absolute-git-dir"])
        if gdir and os.path.isdir(gdir):
            self._lock = open(
                os.path.join(gdir, "planter-workdir-build.lock"), "a+"
            )
            fcntl.flock(self._lock.fileno(), fcntl.LOCK_EX)

    def run(self, args: list[str], timeout: int = 5400) -> tuple[int, str]:
        log("running: " + shlex.join(["fx"] + args))
        try:
            p = subprocess.run(
                [self.fx] + args,
                cwd=self.workdir,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return 124, f"timed out after {timeout}s"
        return p.returncode, p.stdout

    def cpu_dir(self, cpu: str) -> str:
        """Returns a GN out dir for `cpu`, generating it when needed.

        The build directory serves its own CPU. For another CPU, the out dir
        `<build dir>.cutover-<cpu>` gets the build's args with the board for
        `cpu`, and is regenerated (`gn gen` only) when its inputs changed.
        """
        if cpu == self.build_cpu:
            return self.build_dir
        if cpu not in ("x64", "arm64", "riscv64"):
            raise ValueError(f"unknown CPU {cpu!r}")
        out = f"{self.build_dir}.cutover-{cpu}"
        if out in self._fresh_cpu_dirs:
            return out
        self.lock()
        args_path = os.path.join(out, "args.gn")
        want = cpu_args(
            cu.read_text(os.path.join(self.build_dir, "args.gn")) or "",
            self.build_cpu,
            cpu,
        )
        if (
            not os.path.exists(os.path.join(out, "build.ninja"))
            or cu.read_text(args_path) != want
        ):
            # A new out dir, or the build's args changed (e.g. `fx set`).
            os.makedirs(out, exist_ok=True)
            with open(args_path, "w") as f:
                f.write(want)
            rc, log_text = self.run(["--dir", out, "gen"])
        else:
            # Re-runs the regenerator only if a BUILD file changed.
            from build.bazel.scripts import build_utils

            ninja = build_utils.BazelPaths(
                pathlib.Path(self.workdir), pathlib.Path(self.build_dir)
            ).ninja_path
            log(f"running: ninja -C {out} build.ninja.stamp")
            p = subprocess.run(
                [str(ninja), "-C", out, "build.ninja.stamp"],
                cwd=self.workdir,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
            )
            rc, log_text = p.returncode, p.stdout
        if rc:
            raise RuntimeError(
                f"gn gen for {cpu} failed: "
                + "\n".join(log_text.splitlines()[-10:])
            )
        self._fresh_cpu_dirs.add(out)
        return out

    def bazel_cmd(self, verb: str) -> list[str]:
        """Returns `fx` args for a Bazel `verb` on the compared packages.

        Packages for a non-build CPU are built in that CPU's out dir, whose
        Bazel workspace has that CPU's sysroot and GN targets.
        """
        d = self.package_dir
        repo = os.path.join(d, "build/bazel/tests_json.gn_targets")
        extra = (
            [f"--override_repository=gn_targets+={repo}"]
            if os.path.isdir(repo)
            else []
        )
        dir_args = ["--dir", d] if d != self.build_dir else []
        return dir_args + ["bazel", verb, f"--config={BAZEL_CONFIG}"] + extra

    def build(self, side: str, label: str) -> str:
        """Builds a package. Returns "" or the tail of the failing log."""
        self.lock()
        try:
            package_dir = self.package_dir
        except RuntimeError as e:  # `gn gen` failed for the package CPU.
            return str(e)
        if side == "gn":
            dir_args = (
                ["--dir", package_dir] if package_dir != self.build_dir else []
            )
            rc, out = self.run(dir_args + ["build", "--no-status", label])
            if rc and "Unknown GN label" in out:
                # `fx build` resolves labels against the last `fx gen`; a
                # BUILD.gn edited since then needs a fresh graph.
                self.run(dir_args + ["gen"])
                rc, out = self.run(dir_args + ["build", "--no-status", label])
        else:
            rc, out = self.run(
                self.bazel_cmd("build") + ["--config=remote", label]
            )
        return "" if rc == 0 else "\n".join(out.splitlines()[-30:])

    @property
    def package_dir(self) -> str:
        """The GN out dir the compared packages are built in."""
        return self.cpu_dir(self.package_cpu)

    def manifest(self, side: str, label: str) -> str:
        """Returns the path of a built package's package_manifest.json."""
        d, name = cu.label_dir(label), label.split(":")[-1]
        if side == "gn":
            return os.path.join(
                self.package_dir, "obj", d, name, "package_manifest.json"
            )
        rc, out = self.run(self.bazel_cmd("cquery") + ["--output=files", label])
        rel = next(
            (
                l.strip()
                for l in out.splitlines()
                if l.strip().endswith(f"{name}_pkg/package_manifest.json")
            ),
            "",
        )
        rc, info = self.run(self.bazel_cmd("info") + ["execution_root"])
        self.execroot = (
            info.strip().splitlines()[-1] if rc == 0 and info.strip() else ""
        )
        return os.path.join(self.execroot, rel) if rel and self.execroot else ""


def cpu_args(args_gn: str, from_cpu: str, to_cpu: str) -> str:
    """Rewrites a build's args.gn for another CPU (board import and board name)."""
    board = re.compile(
        r'(import\("//boards/[^"]*?)\b'
        + re.escape(from_cpu)
        + r'\b([^"]*\.gni"\))'
    )
    out, n = board.subn(r"\g<1>" + to_cpu + r"\g<2>", args_gn)
    out = re.sub(
        r'^(build_info_board\s*=\s*")' + re.escape(from_cpu) + r'"',
        r"\g<1>" + to_cpu + '"',
        out,
        flags=re.M,
    )
    if not n:
        out = out.rstrip("\n") + f'\ntarget_cpu = "{to_cpu}"\n'
    return out


def swap_toolchain_cpu(label: str, from_cpu: str, to_cpu: str) -> str:
    """Maps `//a:b(//build/toolchain/fuchsia:x64-shared)` to another CPU."""
    return re.sub(
        r"(//build/toolchain/fuchsia:)" + re.escape(from_cpu) + r"\b",
        r"\g<1>" + to_cpu,
        label,
    )


# ---------------------------------------------------------------------------
# Build command comparison
# ---------------------------------------------------------------------------


def bazel_binaries(
    workdir: str, bazel_label: str, rev: T.Optional[str] = None
) -> dict[str, dict[str, T.Any]]:
    """Maps blob basenames to the Bazel binaries in a package's BUILD.bazel."""

    def targets(d: str) -> dict[str, dict[str, T.Any]]:
        path = f"{d}/BUILD.bazel"
        text = (
            cu.git_show(workdir, rev, path)
            if rev
            else cu.read_text(os.path.join(workdir, path))
        )
        return cu.bazel_targets(text)

    pkg_dir = cu.label_dir(bazel_label)
    local = targets(pkg_dir)
    out: dict[str, dict[str, T.Any]] = {}

    def add(blob_name: str, label: str) -> None:
        attrs = targets(cu.label_dir(label)).get(label.split(":")[-1])
        if not attrs or attrs["rule"] not in BINARY_RULES:
            return
        rust = "rust" in attrs["rule"]
        srcs = [s for s in attrs.get("srcs", []) if s.endswith((".cc", ".cpp"))]
        out[blob_name] = {
            "label": label,
            "actions": ["rustc"]
            if rust
            else ["cpp_link"] + (["cpp_compile"] if srcs else []),
            "source": srcs[0] if srcs and not rust else None,
        }

    for name, attrs in local.items():
        if attrs["rule"] == "fx_packaged_binary" and isinstance(
            attrs.get("binary"), str
        ):
            label = cu.resolve_label(attrs["binary"], pkg_dir)
            add(attrs.get("binary_name") or label.split(":")[-1], label)
    for name, attrs in local.items():
        if attrs["rule"] in BINARY_RULES:
            add(attrs.get("output_name") or name, f"//{pkg_dir}:{name}")
    return out


def capture_commands(
    side: str,
    checkout: Checkout,
    facts: dict[str, T.Any],
    binaries: dict[str, dict[str, T.Any]],
    cpu: str = "",
) -> tuple[dict[str, dict[str, T.Any]], list[str]]:
    """Returns ({"<blob> <action>[ <cpu>]": {"label", "args"}}, errors).

    Keys for the build directory's CPU have no suffix (stable verdict keys);
    other CPUs get ` <cpu>`.
    """
    from build.bazel.scripts import build_utils
    from build.beads.scripts import (
        compare_utils,
        normalize_clang_args,
        normalize_rustc_args,
        path_normalizer,
        shell_utils,
    )

    cpu = cpu or checkout.build_cpu
    suffix = "" if cpu == checkout.build_cpu else f" {cpu}"
    paths = build_utils.BazelPaths(
        pathlib.Path(checkout.workdir), pathlib.Path(checkout.build_dir)
    )
    queries: list[tuple[str, T.Any]] = []
    errors: list[str] = []
    if side == "gn":
        from build.api import gn_ninja_outputs

        # GN labels come from the out dir the package was built in; commands
        # from the out dir for `cpu` (analysis only for a non-build CPU).
        pkg_dir = pathlib.Path(checkout.package_dir)
        cmd_dir = pathlib.Path(checkout.cpu_dir(cpu))
        db = gn_ninja_outputs.load_from_build_dir(pkg_dir)
        normalizer: T.Any = path_normalizer.GnPathNormalizer(
            paths.fuchsia_dir, cmd_dir
        )
    else:
        normalizer = path_normalizer.BazelPathNormalizer(paths)
    for blob, entry in sorted(facts["blobs"].items()):
        if "elf" not in entry:
            continue
        b = binaries.get(os.path.basename(blob))
        if not b:
            continue
        if side == "gn":
            src = os.path.relpath(facts["sources"][blob], pkg_dir)
            label = (
                (db.path_to_gn_label(src) or db.path_to_gn_label(f"./{src}"))
                if db
                else ""
            )
            if not label:
                errors.append(f"{blob}: no GN label produces {src}")
                continue
            label = swap_toolchain_cpu(label, checkout.package_cpu, cpu)
        for action in b["actions"]:
            if side == "gn":
                q = compare_utils.CompareCommandsQuery(
                    gn=label, action_type=action, source=b["source"]
                )
            else:
                q = compare_utils.CompareCommandsQuery(
                    bazel=compare_utils.ConfiguredBazelLabel(
                        b["label"],
                        BAZEL_CONFIG,
                        cpu="" if cpu == checkout.build_cpu else cpu,
                    ),
                    action_type=action,
                    source=b["source"],
                )
            queries.append((f"{blob} {action}{suffix}", q))
    commands: dict[str, dict[str, T.Any]] = {}
    for key, q in queries:
        try:
            if side == "gn":
                runner = build_utils.NinjaRunner(paths.ninja_path, cmd_dir)
                raw = compare_utils.query_ninja_commands(runner, [q]).get(
                    q.gn, ""
                )
            else:
                raw = compare_utils.query_bazel_commands(
                    build_utils.BazelLauncher(paths.launcher),
                    paths.execroot,
                    [q],
                ).get(q.bazel, "")
        except Exception as e:  # compare_utils raises ValueError/RuntimeError
            errors.append(f"{key}: {e}")
            continue
        if not raw:
            errors.append(f"{key}: no {side} command found")
            continue
        action = key.split(" ")[1]
        cmd = shell_utils.ShellCommand(raw)
        tools = (
            ["rustc"]
            if action == "rustc"
            else ["clang++", "clang", "ld.lld", "lld"]
        )
        for tool in tools:
            found = shell_utils.find_command_with_tool(cmd.split(), tool)
            if found:
                cmd = found
                break
        if action == "rustc":
            args = list(
                normalize_rustc_args.normalize_rustc_cmd(str(cmd), normalizer)
            )
            levels = _OPT_LEVEL_RE.findall(str(cmd))
            if levels:
                args.append(f"-Copt-level={levels[-1]}")
        else:
            args = normalize_clang_args.normalize_clang_cmd(
                str(cmd), normalizer, action_type=action
            )
        args = pair_flag_values(list(args), str(cmd))
        commands[key] = {
            "label": q.gn if side == "gn" else q.bazel.label,
            "args": list(args),
        }
    return commands, errors


# Flags whose value can be a separate argument.
_VALUE_FLAGS = {
    "-L",
    "-l",
    "-I",
    "-isystem",
    "-iquote",
    "-include",
    "-o",
    "-x",
    "-target",
    "-C",
    "--cfg",
    "--extern",
    "--crate-name",
    "--crate-type",
    "--edition",
    "-Xlinker",
    "-Xclang",
    "-MF",
    "-MT",
    "-MQ",
}


def pair_flag_values(args: list[str], raw: str) -> list[str]:
    """Re-attaches flags to values that normalization left as bare tokens.

    The normalizers drop some flags (e.g. rustc `-L`) but can keep their
    separate value token; report such values as `<flag> <value>`.
    """
    try:
        tokens = shlex.split(raw)
    except ValueError:
        return args
    flag_of: dict[str, str] = {}
    for prev, tok in itertools.pairwise(tokens):
        if prev in _VALUE_FLAGS and not tok.startswith("-"):
            flag_of.setdefault(tok, prev)
    out = []
    for a in args:
        flag = "" if a.startswith(("-", "@")) else flag_of.get(a, "")
        if not flag:
            flag = (
                next(
                    (
                        f
                        for v, f in flag_of.items()
                        if a and v.endswith("/" + a)
                    ),
                    "",
                )
                if not a.startswith(("-", "@"))
                else ""
            )
        out.append(f"{flag} {a}" if flag else a)
    return out


def compare_commands(
    gn: dict[str, T.Any], bazel: dict[str, T.Any]
) -> list[cu.Diff]:
    diffs = []
    for key in sorted(set(gn) & set(bazel)):
        g, b = set(gn[key]["args"]), set(bazel[key]["args"])
        for flag in sorted(g - b):
            diffs.append(
                cu.Diff(
                    "cmd_flags",
                    key,
                    "present",
                    "absent",
                    item=flag,
                    comparison="build_commands",
                )
            )
        for flag in sorted(b - g):
            diffs.append(
                cu.Diff(
                    "cmd_flags",
                    key,
                    "absent",
                    "present",
                    item=flag,
                    comparison="build_commands",
                )
            )
    return diffs


def flag_category(d: cu.Diff) -> str:
    if d.kind != "cmd_flags":
        return ""
    try:
        from build.beads.scripts import flags_differences as fd
    except Exception:  # Optional: only labels the impact.
        return ""
    fn = (
        fd.categorize_rust_flag
        if " rustc" in d.key
        else fd.categorize_clang_flag
    )
    return f"flag category: {fn(d.item)}"


# ---------------------------------------------------------------------------
# Capture and compare
# ---------------------------------------------------------------------------


def capture(
    side: str,
    checkout: Checkout,
    label: str,
    bazel_label: str,
    build: bool,
    tier: str,
    hint_rev: T.Optional[str] = None,
) -> tuple[T.Optional[dict[str, T.Any]], list[dict[str, T.Any]]]:
    """Builds one side and returns (facts, info findings)."""
    infos: list[dict[str, T.Any]] = []
    file = f"{cu.label_dir(bazel_label)}/BUILD.bazel"
    if build:
        err = checkout.build(side, label)
        if err:
            infos.append(
                unverified_finding(
                    "cutover_info_not_compared",
                    file,
                    label,
                    f"the {side} package did not build",
                    f"[{label}] The {side} package did not build, so the packages were not compared.\n{err}",
                )
            )
            return None, infos
    manifest = checkout.manifest(side, label)
    if not manifest or not os.path.exists(manifest):
        infos.append(
            unverified_finding(
                "cutover_info_not_compared",
                file,
                label,
                f"no package manifest for the {side} package",
                f"[{label}] No package_manifest.json for the {side} package ({manifest or 'not found'}); "
                "build it first or drop PLANTER_SKIP_BUILD.",
            )
        )
        return None, infos
    tools = cu.Tools.find(checkout.workdir, checkout.build_dir)
    missing = [
        n
        for n, p in (("cmc", tools.cmc), ("llvm-readelf", tools.readelf))
        if not p
    ]
    if missing:
        infos.append(
            cu.new_finding(
                SOURCE,
                "cutover_info_tool_unavailable",
                file,
                f"[{', '.join(missing)}] Not found, so .cm files are compared by hash and ELF facts are skipped.",
            )
        )
    facts = cu.package_facts(
        manifest,
        tools,
        cwd=checkout.package_dir if side == "gn" else checkout.execroot,
    )
    facts["label"] = label
    facts["manifest"] = manifest
    if tier == "quick":
        binaries = bazel_binaries(checkout.workdir, bazel_label, hint_rev)
        facts["own_binaries"] = sorted(
            p
            for p, e in facts["blobs"].items()
            if "elf" in e and os.path.basename(p) in binaries
        )
        facts["commands"], facts["command_errors"] = {}, []
        facts["command_cpus"] = [checkout.build_cpu, checkout.other_cpu]
        for cpu in facts["command_cpus"]:
            started = time.monotonic()
            try:
                commands, errors = capture_commands(
                    side, checkout, facts, binaries, cpu
                )
            except Exception as e:  # gn gen, Bazel or Ninja failures.
                commands, errors = {}, [str(e)]
            log(f"{side} {cpu} commands: {time.monotonic() - started:.1f}s")
            facts["commands"].update(commands)
            facts["command_errors"] += [f"{cpu}: {e}" for e in errors]
    return facts, infos


def count_by_comparison(findings: list[dict[str, T.Any]]) -> dict[str, int]:
    """Counts cutover_diff_* findings per comparison (from the first line)."""
    counts = {"package_contents": 0, "build_commands": 0}
    for f in findings:
        if not f["category"].startswith("cutover_diff_"):
            continue
        first = f["message"].splitlines()[0]
        key = (
            "build_commands"
            if f"({cu.COMPARISONS['build_commands']})" in first
            else "package_contents"
        )
        counts[key] += 1
    return counts


def compare(
    gn: dict[str, T.Any],
    bazel: dict[str, T.Any],
    normalizations: list[dict[str, T.Any]],
    file: str,
    package_name: str,
    anchor: T.Optional[T.Callable[[cu.Diff], cu.Anchor]] = None,
) -> tuple[list[dict[str, T.Any]], dict[str, int], dict[str, T.Any]]:
    """Returns (findings, normalizations applied, {comparison: count or reason})."""
    diffs = cu.compare_facts(gn, bazel)
    diffs += compare_commands(gn.get("commands", {}), bazel.get("commands", {}))
    kept, applied = cu.apply_normalizations(diffs, normalizations)
    findings = cu.findings_from_diffs(
        kept, SOURCE, file, package_name, flag_category, anchor
    )
    counts: dict[str, T.Any] = dict(count_by_comparison(findings))
    common = set(gn.get("commands", {})) & set(bazel.get("commands", {}))
    if not common:
        if not (has_elf(gn) or has_elf(bazel)):
            counts["build_commands"] = "not applicable (no binaries)"
        elif "commands" not in gn and "commands" not in bazel:
            counts["build_commands"] = "not compared (--tier contents)"
        else:
            counts["build_commands"] = "not compared (no matching commands)"
    return findings, applied, counts


def make_locator(
    workdir: str, rev: T.Optional[str] = None
) -> T.Callable[[str], cu.Anchor]:
    """Returns a function mapping a label to (BUILD.bazel, line or 0, label)."""
    cache: dict[str, dict[str, dict[str, T.Any]]] = {}

    def locate(label: str) -> cu.Anchor:
        d, name = cu.label_dir(label), label.split(":")[-1]
        if d not in cache:
            path = f"{d}/BUILD.bazel"
            text = (
                cu.git_show(workdir, rev, path)
                if rev
                else cu.read_text(os.path.join(workdir, path))
            )
            cache[d] = cu.bazel_targets(text)
        return (
            f"{d}/BUILD.bazel",
            int(cache[d].get(name, {}).get("_line", 0)),
            label,
        )

    return locate


def make_anchor(
    workdir: str, c: cu.Cutover, rev: T.Optional[str] = None
) -> T.Callable[[cu.Diff], cu.Anchor]:
    """Maps a difference to the BUILD.bazel target it is about.

    Package contents anchor at the package rule (e.g. `fx_package(`), `.cm`
    files at their `fx_component_manifest(` and build commands at the binary
    target. Line 0 means the target was not found in the file.
    """
    at = make_locator(workdir, rev)
    pkg_dir = cu.label_dir(c.bazel_label)
    pkg_targets = cu.bazel_targets(
        cu.git_show(workdir, rev, f"{pkg_dir}/BUILD.bazel")
        if rev
        else cu.read_text(os.path.join(workdir, pkg_dir, "BUILD.bazel"))
    )

    package = at(c.bazel_label)

    def anchor(diff: cu.Diff) -> cu.Anchor:
        if diff.kind == "cm":
            stem = os.path.basename(diff.key).removesuffix(".cm")
            for name, attrs in pkg_targets.items():
                if attrs["rule"] in MANIFEST_RULES and stem in (
                    attrs.get("component_name"),
                    name,
                ):
                    return at(f"//{pkg_dir}:{name}")
            return package
        if diff.kind == "cmd_flags":
            blob = diff.key.split(" ")[0]
            b = bazel_binaries(workdir, c.bazel_label, rev).get(
                os.path.basename(blob)
            )
            if b:
                return at(b["label"])
            # Binary not found: anchor at the file, without a line or target.
            return (package[0], 0, "")
        return package

    return anchor


def has_elf(facts: T.Optional[dict[str, T.Any]]) -> bool:
    """Whether a package ships ELF files, so the build command comparison applies."""
    return bool(facts) and any(
        "elf" in e for e in (facts or {}).get("blobs", {}).values()
    )


def _uncovered_binaries(
    gn: dict[str, T.Any], bazel: dict[str, T.Any]
) -> dict[str, list[str]]:
    """{cpu: package binaries with no command compared on that CPU}."""
    cpus = bazel.get("command_cpus") or gn.get("command_cpus") or []
    covered = set()
    for key in set(gn.get("commands", {})) & set(bazel.get("commands", {})):
        parts = key.split(" ")
        covered.add(
            (parts[0], parts[2] if len(parts) > 2 else (cpus or [""])[0])
        )
    own = sorted(
        p
        for p in bazel.get("own_binaries", [])
        if "elf" in bazel.get("blobs", {}).get(p, {})
    )
    gaps = {cpu: [p for p in own if (p, cpu) not in covered] for cpu in cpus}
    return {cpu: blobs for cpu, blobs in gaps.items() if blobs}


def command_coverage_gaps(
    gn: T.Optional[dict[str, T.Any]], bazel: T.Optional[dict[str, T.Any]]
) -> list[str]:
    """Describes what the build command comparison did not cover."""
    if gn is None or bazel is None:
        return []
    notes = [
        f"{cpu} build commands for " + ", ".join(blobs)
        for cpu, blobs in sorted(_uncovered_binaries(gn, bazel).items())
    ]
    for side, f in (("GN", gn), ("Bazel", bazel)):
        for e in f.get("command_errors", [])[:5]:
            notes.append(f"{side}: {e}")
    return notes


def not_compared(reason: str) -> dict[str, T.Any]:
    """Counts for a cutover whose comparisons did not run."""
    return {
        "package_contents": f"not compared ({reason})",
        "build_commands": f"not compared ({reason})",
    }


def summary_finding(
    cutover: cu.Cutover,
    phase: str,
    counts: dict[str, T.Any],
    applied: dict[str, int],
    not_covered: list[str],
    guard: str,
    repro: str,
    file: str,
    package_arch: str = "",
) -> dict[str, T.Any]:
    """The cutover summary. A count that is not an int says why it is missing,
    so that a comparison that did not run is never reported as 0."""

    def shown(name: str) -> str:
        return str(counts.get(name, "not compared"))

    contents = shown("package_contents")
    head = f"**Package cutover:** `{cutover.package_name}` (GN `{cutover.gn_label or 'unknown'}` -> Bazel `{cutover.bazel_label or 'none paired'}`), phase `{phase}`."
    if contents.startswith("not compared"):
        # Nothing was compared: say so instead of listing empty results.
        reason = contents.removeprefix("not compared").strip() or "(unknown)"
        lines = [
            head,
            "",
            f"- Package contents: {contents}.",
            f"- Not covered: everything {reason}.",
            "- Normalizations: n/a.",
            f"- Guard: {guard}",
            f"- Reproduce: `{repro}`",
        ]
        return cu.new_finding(SOURCE, "cutover_summary", file, "\n".join(lines))
    arch = package_arch or "the build directory's CPU"
    lines = [
        head,
        "",
        f"- Differences: package contents {contents}, build commands {shown('build_commands')}.",
        f"- Package contents compared on {arch}"
        + (
            "."
            if package_arch == "arm64"
            else "; arm64 opt-in (`--package-arch=arm64`)."
        ),
        "- Normalizations applied: "
        + (
            ", ".join(f"`{k}` x{v}" for k, v in sorted(applied.items()))
            or "none"
        ),
        "- Not covered: " + ("; ".join(not_covered) or "nothing"),
        f"- Guard: {guard}",
        f"- Reproduce: `{repro}`",
    ]
    return cu.new_finding(SOURCE, "cutover_summary", file, "\n".join(lines))


def _info(category: str, file: str, msg: str) -> dict[str, T.Any]:
    return cu.new_finding(SOURCE, category, file, msg)


def unverified_finding(
    category: str, file: str, key: str, reason: str, msg: str
) -> dict[str, T.Any]:
    """A note that a cutover was not compared (`cutover_info_not_compared`
    or `cutover_info_phase1_missing`), fingerprinted by its key, category
    and reason so that its message's volatile parts (paths, build logs) do
    not count as a change."""
    return cu.new_finding(
        SOURCE,
        category,
        file,
        msg,
        fingerprint=cu.unverified_fingerprint(key, category, reason),
    )


def not_covered_findings(
    gn: dict[str, T.Any], bazel: dict[str, T.Any], file: str, tier: str
) -> list[dict[str, T.Any]]:
    """One `cutover_info_not_covered` per CPU whose commands were not compared."""
    if not (has_elf(gn) or has_elf(bazel)):
        return []
    if tier == "contents":
        return [
            _info(
                "cutover_info_not_covered",
                file,
                "[build_commands] Not covered: the build command comparison was "
                "not run (--tier contents).",
            )
        ]
    return [
        _info(
            "cutover_info_not_covered",
            file,
            f"[{cpu}] Not covered: the build command comparison found no matching "
            f"{cpu} commands for {', '.join(blobs)}, so {cpu}-only flag differences "
            "there are not detected.",
        )
        for cpu, blobs in sorted(_uncovered_binaries(gn, bazel).items())
    ]


def run_detected(
    args: argparse.Namespace, normalizations: list[dict[str, T.Any]]
) -> list[dict[str, T.Any]]:
    """The Planter flow: detect cutovers, then run the parallel or final phase."""
    checkout = Checkout(
        args.workdir, args.build_dir, getattr(args, "package_arch", "")
    )
    base = (
        args.base
        or os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or "HEAD"
    )
    raw_changed = os.environ.get("PLANTER_CHANGED_FILES", "")
    if getattr(args, "changed_files", None):
        raw_changed = cu.read_text(args.changed_files) or ""
    changed = [l.strip() for l in raw_changed.splitlines() if l.strip()]
    if not changed:
        changed = cu.git(
            checkout.workdir, ["diff", "--name-only", base]
        ).splitlines()
        changed += cu.git(
            checkout.workdir, ["ls-files", "--others", "--exclude-standard"]
        ).splitlines()
    cutovers = cu.detect_cutovers(checkout.workdir, base, changed)
    unpaired = cu.unpaired_gn_packages(
        checkout.workdir, base, changed, cutovers
    )
    findings: list[dict[str, T.Any]] = []
    build = not args.no_build and os.environ.get(
        "PLANTER_SKIP_BUILD", ""
    ).strip() in ("", "0")
    if cutovers:
        # The git common dir is shared by all worktrees, so its records
        # outlive their tasks; prune them even when Planter passes its own
        # directory, which it manages itself, so that find_record() never
        # falls back to an expired one.
        for path in cu.prune_records(cu.local_records_dir(checkout.workdir)):
            log(f"deleted stale phase-1 record {path}")
    dual_build: list[cu.Cutover] = []
    requested_cpu = checkout.package_cpu
    for c in cutovers:
        # A final phase switches to its record's CPU; don't carry that over.
        checkout.package_cpu = requested_cpu
        phase = select_phase(c, args)
        if phase == "dual_build":
            dual_build.append(c)
            continue
        if phase == "ambiguous":
            findings += ambiguous_findings(c, base)
            continue
        out: list[dict[str, T.Any]] = []
        try:
            _run_cutover(
                c, args, checkout, base, build, normalizations, out, phase
            )
        except Exception as e:  # Keep this cutover's partial results.
            log(f"internal error on {c.package_name}: {e!r}")
            out.append(
                _info(
                    "cutover_info_tool_unavailable",
                    c.bazel_build_file,
                    f"[{c.key}] The package cutover check failed: {e!r}. Rerun it by hand: "
                    f"build/beads/verifications/verify_package_cutover.py --base {base}",
                )
            )
            if not any(f["category"] == "cutover_summary" for f in out):
                # Count it as an unverified cutover.
                out.append(
                    unverified_finding(
                        "cutover_info_not_compared",
                        c.bazel_build_file,
                        c.key,
                        "the check failed",
                        f"[{c.key}] The packages were not compared: the check failed.",
                    )
                )
                out.append(
                    summary_finding(
                        c,
                        "unknown",
                        not_compared("internal error"),
                        {},
                        command_coverage_gaps(None, None),
                        f"not run (internal error: {e!r})",
                        f"build/beads/verifications/verify_package_cutover.py --base {base}",
                        c.bazel_build_file,
                        package_arch=checkout.package_cpu,
                    )
                )
        findings += out
    for u in unpaired:
        findings += unpaired_findings(u, base)
    if dual_build:
        findings.append(dual_build_finding(dual_build))
    return findings


def unpaired_findings(
    u: cu.UnpairedGnPackage, base: str
) -> list[dict[str, T.Any]]:
    """Findings for a GN package deleted or bridged without a detected Bazel
    package replacing it: its cutover was not compared."""
    file = u.gn_build_file
    how = (
        "deleted"
        if u.gn_state == "gone"
        else "replaced by a bridge to a Bazel package that this change does not add"
    )
    return [
        unverified_finding(
            "cutover_info_not_compared",
            file,
            u.gn_label,
            f"no paired Bazel package ({u.gn_state})",
            f"[{u.gn_label}] The GN package `{u.package_name}` is {how}, and no Bazel "
            "package added in this change pairs with it, so it was not compared. Add the "
            "Bazel package in the same directory with the same target or package name "
            "while the GN package still exists (phase 1), or explain the deletion.",
        ),
        summary_finding(
            cu.Cutover(
                package_name=u.package_name,
                gn_label=u.gn_label,
                bazel_label="",
                gn_state=u.gn_state,
            ),
            "unknown",
            not_compared("no paired Bazel package"),
            {},
            [],
            "not run (no paired Bazel package)",
            f"build/beads/verifications/verify_package_cutover.py --base {base}",
            file,
        ),
    ]


# Why a package that stays dual-built is not compared, by `gn_state`.
DUAL_BUILD_REASONS = {
    "hand": "held back: the hand-written GN package still exists",
    "mirror": "bazel2gn mirror: GN builds a generated copy of the Bazel package",
    "bridge": "bridge: GN re-exports the Bazel package",
}


def dual_build_finding(dual_build: list[cu.Cutover]) -> dict[str, T.Any]:
    """One note for the whole run listing the packages that stay dual-built."""
    items = "; ".join(
        f"`{c.package_name}` ({DUAL_BUILD_REASONS.get(c.gn_state, c.gn_state)})"
        for c in dual_build
    )
    return _info(
        "cutover_info_dual_build_only",
        dual_build[0].bazel_build_file,
        f"[dual_build] Dual-built, not cut over, so not compared: {items}. "
        "The assembly does not use their Bazel labels.",
    )


def ambiguous_findings(c: cu.Cutover, base: str) -> list[dict[str, T.Any]]:
    """Findings for a Bazel package whose GN package can't be told apart."""
    file = c.bazel_build_file
    if len(c.ambiguous) == 1:
        detail = (
            f"[{c.key}] Another Bazel package in this change already replaces "
            f"`{c.ambiguous[0]}` (package `{c.package_name}`), so "
            f"`{c.bazel_label}` could not be paired with a GN package."
        )
    else:
        candidates = ", ".join(f"`{l}`" for l in c.ambiguous)
        detail = (
            f"[{c.key}] Several GN packages named `{c.package_name}` could be the one "
            f"`{c.bazel_label}` replaces: {candidates}. Give the Bazel package the "
            "target name of the GN package it replaces, so the check can pair them."
        )
    return [
        _info("cutover_info_ambiguous", file, detail),
        unverified_finding(
            "cutover_info_not_compared",
            file,
            c.key,
            "ambiguous GN package: " + ", ".join(sorted(c.ambiguous)),
            f"[{c.key}] The packages were not compared: the GN package is ambiguous.",
        ),
        summary_finding(
            c,
            "unknown",
            not_compared("ambiguous GN package"),
            {},
            [],
            "not run (ambiguous GN package)",
            f"build/beads/verifications/verify_package_cutover.py --base {base}",
            file,
        ),
    ]


def select_phase(c: cu.Cutover, args: argparse.Namespace) -> str:
    """Returns "parallel", "final" or "dual_build" for a detected package.

    Planter sets PLANTER_CUTOVER_PHASE=parallel for the phase-1 run (the coder
    reported phase 1); cutover_consistency.sh passes it as `--phase`.
    Otherwise a package counts as cut over only when its GN package is gone
    or the assembly points at the Bazel label.
    """
    if c.gn_state == "ambiguous":
        return "ambiguous"
    if args.phase == "parallel" and c.gn_state == "mirror":
        return "dual_build"
    if args.phase != "auto":
        return str(args.phase)
    env_phase = os.environ.get("PLANTER_CUTOVER_PHASE", "").strip()
    if env_phase == "parallel" and c.gn_state == "mirror":
        # Comparing a Bazel package with its own bazel2gn copy is not a cutover.
        return "dual_build"
    if env_phase in ("parallel", "final"):
        return env_phase
    return c.state


def _run_cutover(
    c: cu.Cutover,
    args: argparse.Namespace,
    checkout: Checkout,
    base: str,
    build: bool,
    normalizations: list[dict[str, T.Any]],
    findings: list[dict[str, T.Any]],
    phase: str,
) -> None:
    """Runs the parallel or final phase for one cutover, appending findings."""
    file = c.bazel_build_file
    repro = f"PLANTER_CUTOVER_PHASE={phase} build/beads/verifications/verify_package_cutover.py --base {base}"
    rpath = cu.record_path(checkout.workdir, c.package_name, c.bazel_target)
    base_sha = cu.resolve_commit(checkout.workdir, base)
    log(
        f"cutover {c.package_name}: {c.gn_label} -> {c.bazel_label} ({phase}); record {rpath}"
    )
    if phase == "parallel" and not c.gn_exists:
        findings.append(
            unverified_finding(
                "cutover_info_phase1_missing",
                file,
                c.key,
                "the GN package no longer exists in phase 1",
                f"[{c.key}] PLANTER_CUTOVER_PHASE=parallel, but the GN package {c.gn_label} no longer exists; running the final phase.",
            )
        )
        phase = "final"
    if phase == "parallel":
        if build:
            drop_records(c, checkout.workdir, rpath)
        gn, infos = capture(
            "gn", checkout, c.gn_label, c.bazel_label, build, args.tier
        )
        findings += infos
        bazel, infos = capture(
            "bazel",
            checkout,
            c.bazel_package_label,
            c.bazel_label,
            build,
            args.tier,
        )
        findings += infos
        if gn is None or bazel is None:
            findings.append(
                summary_finding(
                    c,
                    phase,
                    not_compared("build failed"),
                    {},
                    command_coverage_gaps(None, None),
                    "not run (a package did not build)",
                    repro,
                    file,
                    package_arch=checkout.package_cpu,
                )
            )
            return
        diff_findings, applied, counts = compare(
            gn,
            bazel,
            normalizations,
            file,
            c.key,
            make_anchor(checkout.workdir, c),
        )
        findings += diff_findings
        findings += not_covered_findings(gn, bazel, file, args.tier)
        if build:
            cu.write_record(
                rpath,
                c,
                bazel,
                diff_findings,
                applied,
                base_sha,
                bazel.get("manifest"),
                checkout.workdir,
                checkout.package_cpu,
                {
                    "counts": counts,
                    "not_covered": command_coverage_gaps(gn, bazel),
                },
                gn_facts=gn,
                build_cpu=checkout.build_cpu,
            )
            guard = f"phase-1 record written (Bazel meta.far {bazel['meta_far_merkle'][:16]})"
        else:
            # Outputs may predate the change; only a building run records.
            guard = "phase-1 record not written (builds skipped)"
        findings.append(
            summary_finding(
                c,
                phase,
                counts,
                applied,
                command_coverage_gaps(gn, bazel),
                guard,
                repro,
                file,
                package_arch=checkout.package_cpu,
            )
        )
        return
    record = find_record(c, checkout.workdir, rpath)
    if record is None:
        findings.append(
            unverified_finding(
                "cutover_info_phase1_missing",
                file,
                c.key,
                "no phase-1 record",
                f"[{c.key}] No phase-1 record at {rpath}, so the Bazel package was not compared "
                f"with the GN package {c.gn_label}. Planter writes it when the coder stops after phase 1 "
                "(`cutover_phase: parallel`).",
            )
        )
        findings.append(
            summary_finding(
                c,
                phase,
                not_compared("no phase-1 record"),
                {},
                command_coverage_gaps(None, None),
                "unverified (no phase-1 record)",
                repro,
                file,
                package_arch=checkout.package_cpu,
            )
        )
        return
    # Rebuild the Bazel package for the same CPU as phase 1. The CPU names an
    # out dir; only accept the CPUs this tool knows.
    recorded_cpu = record.get("package_cpu")
    checkout.package_cpu = (
        recorded_cpu if recorded_cpu in ("x64", "arm64") else checkout.build_cpu
    )
    recorded_gn = record.get("gn_facts")
    if not isinstance(recorded_gn, dict):
        _final_from_v1_record(
            c, record, checkout, base_sha, build, findings, phase, repro
        )
        return
    try:
        bazel, infos = capture(
            "bazel",
            checkout,
            c.bazel_package_label,
            c.bazel_label,
            build,
            args.tier,
        )
    except Exception:
        # run_detected() reports the error; keep what phase 1 found.
        findings += replay_recorded_findings(record, checkout.workdir)
        raise
    findings += infos
    if bazel is None:
        # Nothing to compare with; show what phase 1 found.
        findings += replay_recorded_findings(record, checkout.workdir)
        summary = record.get("summary") or {}
        findings.append(
            summary_finding(
                c,
                phase,
                summary.get("counts")
                or count_by_comparison(record.get("findings", [])),
                record.get("normalizations_applied", {}),
                list(summary.get("not_covered", [])),
                "not recompared (the Bazel package did not build); the "
                "differences shown are from phase 1",
                repro,
                file,
                package_arch=checkout.package_cpu,
            )
        )
        return
    # Compare the Bazel package as it is now with the recorded GN package,
    # exactly as phase 1 compared them.
    gn = dict(
        recorded_gn,
        commands=cu.rekey_commands(
            recorded_gn.get("commands", {}),
            str(record.get("build_cpu") or checkout.build_cpu),
            checkout.build_cpu,
        ),
    )
    diff_findings, applied, counts = compare(
        gn,
        bazel,
        normalizations,
        file,
        c.key,
        make_anchor(checkout.workdir, c),
    )
    findings += diff_findings
    new_keys = new_difference_keys(
        rekey_recorded_findings(
            record.get("findings", []),
            str(record.get("build_cpu") or checkout.build_cpu),
            checkout.build_cpu,
        ),
        diff_findings,
    )
    changed = bazel["meta_far_merkle"] != record.get("bazel_meta_far_merkle")
    if changed and new_keys:
        findings.append(
            guard_finding(
                c, checkout, record, bazel, base_sha, new_keys, diff_findings
            )
        )
    findings += not_covered_findings(gn, bazel, file, args.tier)
    findings.append(
        summary_finding(
            c,
            phase,
            counts,
            applied,
            command_coverage_gaps(gn, bazel),
            recompare_note(record, bazel, base_sha, new_keys),
            repro,
            file,
            package_arch=checkout.package_cpu,
        )
    )


def _finding_item_key(f: dict[str, T.Any]) -> tuple[str, str, str]:
    """(category, `[...]` key, key after `<package>:`) of a finding."""
    m = re.match(r"\[([^\]]*)\]", f.get("message", ""))
    full = m.group(1) if m else ""
    return f.get("category", ""), full, full.split(":", 1)[-1]


def rekey_recorded_findings(
    findings: list[dict[str, T.Any]], from_cpu: str, to_cpu: str
) -> list[dict[str, T.Any]]:
    """Recorded findings with their build command keys renamed for the
    current build CPU, as `cu.rekey_commands` renames the recorded commands,
    so that phase-1 and final keys name the same CPU's commands."""
    if not from_cpu or from_cpu == to_cpu:
        return findings
    out = []
    for f in findings:
        m = re.match(r"\[([^\]:]*):([^\]]*)\]", f.get("message", ""))
        if f.get("category") == "cutover_diff_cmd_flags" and m:
            key = cu.rekey_command_key(m.group(2), from_cpu, to_cpu)
            f = dict(
                f, message=f"[{m.group(1)}:{key}]" + f["message"][m.end() :]
            )
        out.append(f)
    return out


def _finding_content(f: dict[str, T.Any]) -> str:
    """What a difference says, for comparing it across phases.

    The fingerprint when the finding has one; otherwise (records written
    before fingerprints) its message without the key and target lines,
    which can differ between phases for the same content.
    """
    if f.get("fingerprint"):
        return "fingerprint:" + str(f["fingerprint"])
    lines = str(f.get("message", "")).splitlines()[1:]
    return "message:" + "\n".join(
        l for l in lines if not l.startswith(("Target: ", "Device impact: "))
    )


def new_difference_keys(
    recorded: list[dict[str, T.Any]], current: list[dict[str, T.Any]]
) -> list[str]:
    """The `[...]` keys of current differences that phase 1 did not report.

    A difference is new when phase 1 reported nothing under its category
    and key (ignoring a `--<target>` prefix), or reported different details
    there (e.g. one more flag), so that a verdict on the phase-1 content does
    not cover the change.
    """
    old: dict[tuple[str, str], set[str]] = {}
    for f in recorded:
        c, _, item = _finding_item_key(f)
        old.setdefault((c, item), set()).add(_finding_content(f))
    new = set()
    for f in current:
        c, full, item = _finding_item_key(f)
        if not c.startswith("cutover_diff_"):
            continue
        before = old.get((c, item))
        if before is None:
            new.add(full)
            continue
        content = _finding_content(f)
        if content.startswith("fingerprint:") and not any(
            b.startswith("fingerprint:") for b in before
        ):
            # A record from before fingerprints: compare the messages.
            content = _finding_content(dict(f, fingerprint=""))
        if content not in before:
            new.add(full)
    return sorted(new)


def base_moved_note(record: dict[str, T.Any], base_sha: str) -> str:
    old_base = str(record.get("change_base") or base_sha)
    if old_base == base_sha:
        return ""
    return f"the change base moved {old_base[:12]} -> {base_sha[:12]}"


def guard_finding(
    c: cu.Cutover,
    checkout: Checkout,
    record: dict[str, T.Any],
    bazel: dict[str, T.Any],
    base_sha: str,
    new_keys: list[str],
    current: T.Sequence[dict[str, T.Any]],
) -> dict[str, T.Any]:
    """`cutover_diff_guard`: the Bazel package changed after phase 1 and
    the packages now differ in ways phase 1 did not report.

    Its fingerprint covers the new keys and the content of the `current`
    differences under them, so a verdict on it is asked again when they
    change."""
    where, line, label = make_anchor(checkout.workdir, c)(
        cu.Diff("guard", "meta.far", "", "")
    )
    moved = base_moved_note(record, base_sha)
    return cu.new_finding(
        SOURCE,
        "cutover_diff_guard",
        where,
        "\n".join(
            [
                f"[{c.key}:meta.far] The Bazel package changed after phase 1, "
                "and comparing it with the GN package again finds differences "
                "phase 1 did not report, or reported with other details.",
                f"Target: {label}",
                f"GN: (phase 1 Bazel meta.far) {record.get('bazel_meta_far_merkle', '')}",
                f"Bazel: (final) {bazel['meta_far_merkle']}",
                package_changes(record.get("bazel_facts", {}), bazel),
                "New or changed differences (reported above): "
                + ", ".join(f"[{k}]" for k in new_keys),
            ]
            + (
                [f"Note: {moved}; the new differences may come from it."]
                if moved
                else []
            )
            + [f"Device impact: {cu.device_impact('cutover_diff_guard')}"]
        ),
        f"Fix: {cu.DIFF_KINDS['guard'][1]}.",
        line=line,
        severity=cu.category_severity("cutover_diff_guard"),
        fingerprint=cu.fingerprint(
            [(k, "") for k in new_keys]
            + [
                (full, _finding_content(f))
                for f in current
                for _, full, _ in [_finding_item_key(f)]
                if full in new_keys
            ]
        ),
    )


def record_matches(
    record: T.Optional[dict[str, T.Any]],
    c: cu.Cutover,
    workdir: str,
    check_workdir: bool,
) -> bool:
    """Whether a phase-1 record was written for this cutover (and checkout).

    The git common dir is shared by fx worktrees, so a record there must come
    from this checkout (`check_workdir`). Records in
    `$PLANTER_CUTOVER_RECORD_DIR` belong to the Planter task and follow it to
    other checkouts.
    """
    if (
        record is None
        or record.get("bazel_label") != c.bazel_label
        or record.get("gn_label") != c.gn_label
        or record.get("package_name", c.package_name) != c.package_name
    ):
        return False
    return not check_workdir or record.get("workdir", workdir) == workdir


def record_candidates(
    c: cu.Cutover, workdir: str
) -> T.Sequence[tuple[str, bool]]:
    """Where a cutover's phase-1 record may be, as (path, check_workdir).

    `$PLANTER_CUTOVER_RECORD_DIR` first when set, then the git common dir;
    in each, the current `<package_name>--<target>.json` name, then the
    `<package_name>.json` name of older records.
    """
    names = ((c.package_name, c.bazel_target), (c.package_name, ""))
    harness = (
        tuple((cu.record_path(workdir, n, t), False) for n, t in names)
        if cu.harness_records_dir()
        else ()
    )
    return harness + tuple(
        (cu.record_path(workdir, n, t, local=True), True) for n, t in names
    )


def find_record(
    c: cu.Cutover, workdir: str, rpath: str
) -> T.Optional[dict[str, T.Any]]:
    """Returns the phase-1 record for a cutover, or None.

    Planter always sets `$PLANTER_CUTOVER_RECORD_DIR`, to an empty directory
    when it holds no records for the task. A record found elsewhere (in the
    git common dir for this checkout, or under its older file name) is used
    and copied to `rpath` when no file is there yet, so that Planter keeps it
    from now on. A file Planter stores is never replaced.
    """
    for path, check_workdir in record_candidates(c, workdir):
        record = cu.read_record(path)
        if not record_matches(record, c, workdir, check_workdir):
            continue
        assert record is not None
        if path != rpath and not os.path.lexists(rpath):
            cu.store_record(rpath, record)
            log(f"copied the phase-1 record {path} to {rpath}")
        return record
    return None


def drop_records(c: cu.Cutover, workdir: str, rpath: str) -> None:
    """Deletes older phase-1 records that a new phase 1 replaces.

    Never let an older record stand in for this run, even if it fails part
    way: neither the one at `rpath` nor any that `find_record()` would fall
    back to.
    """
    for path, check_workdir in record_candidates(c, workdir):
        if path == rpath or record_matches(
            cu.read_record(path), c, workdir, check_workdir
        ):
            if os.path.exists(path):
                os.remove(path)


def replay_recorded_findings(
    record: dict[str, T.Any], workdir: str
) -> list[dict[str, T.Any]]:
    """Phase 1's findings, re-anchored at today's BUILD.bazel lines."""
    out = []
    locate = make_locator(workdir)
    for f in record.get("findings", []):
        # BUILD.bazel lines move between phases: re-anchor on the recorded
        # target, and take the severity from today's category stage.
        f = dict(f, message=f["message"] + "\n(Recorded in phase 1.)")
        m = re.search(r"^Target: (\S+)$", f["message"], re.M)
        if m:
            f["file"], f["line"], _ = locate(m.group(1))
        f["severity"] = cu.category_severity(f["category"])
        out.append(f)
    return out


def package_changes(old: dict[str, T.Any], new: dict[str, T.Any]) -> str:
    """Says which files of the Bazel package changed since phase 1."""
    changed = cu.blob_changes(old, new)
    if changed:
        return "Changed files: " + "; ".join(changed)
    return "Changed files: none found (only the `meta.far` itself differs)"


def recompare_note(
    record: dict[str, T.Any],
    bazel: dict[str, T.Any],
    base_sha: str,
    new_keys: T.Sequence[str],
) -> str:
    """The summary's guard line for a final phase that compared again."""
    old, new = record.get("bazel_meta_far_merkle", ""), bazel["meta_far_merkle"]
    if old == new:
        return (
            "passed: the Bazel package is unchanged since phase 1 (meta.far "
            f"{new[:16]}); recompared with the phase-1 GN package"
        )
    moved = base_moved_note(record, base_sha)
    verdict = (
        f"FAILED: {len(new_keys)} difference(s) new or changed since phase 1 "
        "(see cutover_diff_guard)"
        if new_keys
        else "passed: no difference new or changed since phase 1"
    )
    return (
        f"{verdict}; recompared with the phase-1 GN package; the Bazel "
        f"package changed since phase 1 (meta.far {old[:16]} -> {new[:16]}"
        f"{', and ' + moved if moved else ''}). "
        f"{package_changes(record.get('bazel_facts', {}), bazel)}."
    )


def _final_from_v1_record(
    c: cu.Cutover,
    record: dict[str, T.Any],
    checkout: Checkout,
    base_sha: str,
    build: bool,
    findings: list[dict[str, T.Any]],
    phase: str,
    repro: str,
) -> None:
    """The final phase for a version 1 record, which has no GN facts.

    Replays the recorded findings and compares the Bazel meta.far merkle with
    the recorded one (the guard).
    """
    file = c.bazel_build_file
    v1 = "record predates recompare (v1); "
    recorded = record.get("findings", [])
    findings += replay_recorded_findings(record, checkout.workdir)
    # Phase 1's counts keep "not compared (...)" reasons; older records
    # only have the findings.
    summary = record.get("summary") or {}
    counts = summary.get("counts") or count_by_comparison(recorded)
    bazel, infos = capture(
        "bazel",
        checkout,
        c.bazel_package_label,
        c.bazel_label,
        build,
        "contents",
    )
    findings += infos
    if bazel is None:
        guard = v1 + "not run (the Bazel package did not build)"
    elif bazel["meta_far_merkle"] == record["bazel_meta_far_merkle"]:
        guard = (
            v1
            + f"passed (Bazel meta.far {bazel['meta_far_merkle'][:16]} matches phase 1)"
        )
    else:
        changes = cu.compare_facts(record["bazel_facts"], bazel)
        detail = package_changes(record["bazel_facts"], bazel) + (
            "\nChanged facts: "
            + "; ".join(
                f"{d.category} {d.key}" + (f" {d.item}" if d.item else "")
                for d in changes[:20]
            )
            if changes
            else ""
        )
        rebased = record.get("change_base", base_sha) != base_sha
        if rebased:
            # A rebase changes Bazel inputs outside this change; not a coder edit.
            guard = (
                v1
                + "skipped (rebased since phase 1; see cutover_info_guard_rebased)"
            )
            findings.append(
                _info(
                    "cutover_info_guard_rebased",
                    file,
                    "\n".join(
                        [
                            f"[{c.key}] The change was rebased since phase 1 "
                            f"({record.get('change_base', '')[:12]} -> {base_sha[:12]}) and the Bazel "
                            "meta.far changed. These differences may come from the rebase:",
                            detail,
                        ]
                    ),
                )
            )
        else:
            guard = v1 + "FAILED (the Bazel meta.far changed after phase 1)"
            where, line, label = make_anchor(checkout.workdir, c)(
                cu.Diff("guard", "meta.far", "", "")
            )
            findings.append(
                cu.new_finding(
                    SOURCE,
                    "cutover_diff_guard",
                    where,
                    "\n".join(
                        [
                            f"[{c.key}:meta.far] The Bazel package changed after phase 1 was verified.",
                            f"Target: {label}",
                            f"GN: (phase 1 Bazel meta.far) {record['bazel_meta_far_merkle']}",
                            f"Bazel: (final) {bazel['meta_far_merkle']}",
                            detail,
                            "The phase-1 record predates recompare (v1), so the "
                            "packages were not compared again; rerun phase 1 to "
                            "compare them.",
                            f"Device impact: {cu.device_impact('cutover_diff_guard')}",
                        ]
                    ),
                    f"Fix: {cu.DIFF_KINDS['guard'][1]}.",
                    line=line,
                    severity=cu.category_severity("cutover_diff_guard"),
                    fingerprint=cu.fingerprint(
                        [
                            (
                                "v1",
                                str(record["bazel_meta_far_merkle"]),
                                str(bazel["meta_far_merkle"]),
                            )
                        ]
                    ),
                )
            )
    findings.append(
        summary_finding(
            c,
            phase,
            counts,
            record.get("normalizations_applied", {}),
            list(summary.get("not_covered", [])),
            guard,
            repro,
            file,
            package_arch=checkout.package_cpu,
        )
    )


def _run_main(parser: argparse.ArgumentParser, args: argparse.Namespace) -> int:
    normalization_error = ""
    try:
        normalizations = cu.load_normalizations(str(NORMALIZATIONS_JSON))
    except Exception as e:
        # A bad registry entry must not turn this INFO-only check into a
        # blocking failure; compare without normalizations and say so.
        normalizations, normalization_error = [], repr(e)

    if args.capture:
        if not args.label or not args.out:
            parser.error("--capture needs --label and --out")
        checkout = Checkout(
            args.workdir, args.build_dir, getattr(args, "package_arch", "")
        )
        facts, infos = capture(
            args.capture,
            checkout,
            args.label,
            args.bazel_label or args.label,
            not args.no_build,
            args.tier,
            args.bazel_rev,
        )
        for info in infos:
            log(info["message"])
        if facts is None:
            return 1
        with open(args.out, "w") as out:
            json.dump(facts, out, indent=2, sort_keys=True)
        return 0

    if args.gn_facts or args.bazel_facts:
        if not (args.gn_facts and args.bazel_facts):
            parser.error("--gn-facts and --bazel-facts go together")
        with open(args.gn_facts) as gn_file:
            gn = json.load(gn_file)
        with open(args.bazel_facts) as bazel_file:
            bazel = json.load(bazel_file)
        label = bazel.get("label", "//unknown:pkg")
        file = f"{cu.label_dir(label)}/BUILD.bazel"
        c = cu.Cutover(
            bazel["package_name"], gn.get("label", ""), label, gn_state="hand"
        )
        findings, applied, counts = compare(
            gn,
            bazel,
            normalizations,
            file,
            c.package_name,
            make_anchor(args.workdir, c),
        )
        findings += not_covered_findings(gn, bazel, file, args.tier)
        guard = "not applicable (comparing captured facts)"
        findings.append(
            summary_finding(
                c,
                "replay",
                counts,
                applied,
                command_coverage_gaps(gn, bazel),
                guard,
                f"verify_package_cutover.py --gn-facts {args.gn_facts} --bazel-facts {args.bazel_facts}",
                file,
            )
        )
    else:
        try:
            findings = run_detected(args, normalizations)
        except Exception as e:  # An INFO-only check must never block Planter.
            if not args.json:
                raise
            log(f"internal error: {e!r}")
            findings = [
                _info(
                    "cutover_info_tool_unavailable",
                    "build/beads/verifications/verify_package_cutover.py",
                    f"[internal_error] The package cutover check failed: {e!r}. Rerun it by hand: "
                    "build/beads/verifications/verify_package_cutover.py",
                )
            ]

    if normalization_error and findings:
        findings.append(
            _info(
                "cutover_info_tool_unavailable",
                "build/beads/verifications/cutover_normalizations.json",
                f"[cutover_normalizations.json] Could not load the normalizations ({normalization_error}), "
                "so no difference was normalized.",
            )
        )

    if args.json:
        print(json.dumps(findings, indent=2))
    else:
        for f in findings:
            print(
                f"{f['category']} ({f['file']}):\n  "
                + f["message"].replace("\n", "\n  ")
                + "\n"
            )
        if not findings:
            print("No package cutover detected.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--workdir", default=os.environ.get("PLANTER_WORKDIR") or os.getcwd()
    )
    parser.add_argument(
        "--build-dir", help="Ninja build dir (default: from .fx-build-dir)"
    )
    parser.add_argument(
        "--base",
        help="change base (default: $PLANTER_RESOLVED_CHANGE_BASE or HEAD)",
    )
    parser.add_argument(
        "--phase", choices=("auto", "parallel", "final"), default="auto"
    )
    parser.add_argument(
        "--tier",
        choices=("quick", "contents"),
        default="quick",
        help="quick: package content and build command comparisons; contents: package contents only",
    )
    parser.add_argument(
        "--package-arch",
        choices=("", "arm64"),
        default="",
        help="also build the GN and Bazel packages for this CPU (in a reusable "
        "out dir) and compare their contents there (default: the build "
        "directory's CPU); set from $PLANTER_CUTOVER_PACKAGE_ARCH by the check",
    )
    parser.add_argument(
        "--changed-files",
        help="file listing the changed paths (default: $PLANTER_CHANGED_FILES or git diff)",
    )
    parser.add_argument(
        "--record-dir",
        help="directory for phase-1 records (default: $PLANTER_CUTOVER_RECORD_DIR "
        "or <git-common-dir>/planter-cutover); set from $PLANTER_CUTOVER_RECORD_DIR by the check",
    )
    parser.add_argument(
        "--no-build", action="store_true", help="use existing build outputs"
    )
    parser.add_argument(
        "--json", action="store_true", help="print Planter findings as JSON"
    )
    parser.add_argument("--list-normalizations", action="store_true")
    parser.add_argument(
        "--markdown",
        action="store_true",
        help="with --list-normalizations: print CUTOVER_NORMALIZATIONS.md",
    )
    parser.add_argument(
        "--capture",
        choices=("gn", "bazel"),
        help="build one side and write its facts to --out",
    )
    parser.add_argument("--label", help="with --capture: the package label")
    parser.add_argument(
        "--bazel-label",
        help="with --capture gn: the Bazel package, used to find binaries for the build command comparison",
    )
    parser.add_argument(
        "--bazel-rev",
        help="with --capture gn: read the Bazel package BUILD.bazel at this revision",
    )
    parser.add_argument("--out", help="with --capture: facts file to write")
    parser.add_argument(
        "--gn-facts", help="compare these captured GN facts ..."
    )
    parser.add_argument(
        "--bazel-facts", help="... with these captured Bazel facts"
    )
    args = parser.parse_args()

    if args.list_normalizations:
        normalizations = cu.load_normalizations(str(NORMALIZATIONS_JSON))
        if args.markdown:
            print(
                cu.registry_markdown(normalizations, cu.load_categories()),
                end="",
            )
        else:
            for e in normalizations:
                print(
                    f"{e['id']} [{e['comparison']}] {json.dumps(e['match'])}: {e['reason']}"
                )
        return 0

    record_dir = args.record_dir.strip() if args.record_dir is not None else ""
    old_record_dir = os.environ.get(cu.RECORD_DIR_ENV)
    if record_dir:
        os.environ[cu.RECORD_DIR_ENV] = os.path.abspath(record_dir)
    try:
        return _run_main(parser, args)
    finally:
        if record_dir:
            if old_record_dir is None:
                os.environ.pop(cu.RECORD_DIR_ENV, None)
            else:
                os.environ[cu.RECORD_DIR_ENV] = old_record_dir


if __name__ == "__main__":
    sys.exit(main())
