# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import io
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import typing as T
import unittest
from unittest import mock

from build.beads.scripts import cutover_utils as cu
from build.beads.verifications import verify_package_cutover as vpc

_DIR = os.path.dirname(os.path.abspath(__file__))

ELF = {
    "arch": "Advanced Micro Devices X86-64",
    "needed": ["libc.so"],
    "soname": "",
    "interp": "ld.so.1",
    "relro": True,
    "stack": "RW",
    "sections": [".text"],
    "exports": [],
    "text_size": 1000,
    "stripped": True,
}


def facts(
    merkle: str = "m", opt: str = "-O2", **overrides: T.Any
) -> dict[str, T.Any]:
    f: dict[str, T.Any] = {
        "label": "//src/snap:pkg",
        "package_name": "snapshot",
        "meta_far_merkle": merkle,
        "abi_revision": "07",
        "subpackages": [],
        "blobs": {"bin/snapshot": {"merkle": "a", "size": 1, "elf": dict(ELF)}},
        "meta": {},
        "cm": {},
        "cvf": {},
        "commands": {
            "bin/snapshot cpp_compile": {
                "label": "x",
                "args": ["clang++", opt, "-c"],
            }
        },
        # Only x64 commands were found, so arm64 is reported as not covered.
        "command_cpus": ["x64", "arm64"],
        "own_binaries": ["bin/snapshot"],
    }
    f.update(overrides)
    return f


class NormalizationRegistryTest(unittest.TestCase):
    def setUp(self) -> None:
        self.entries = cu.load_normalizations(
            os.path.join(_DIR, "cutover_normalizations.json")
        )

    def test_ids_are_unique(self) -> None:
        ids = [e["id"] for e in self.entries]
        self.assertEqual(len(ids), len(set(ids)))

    def test_fixtures_match_their_entry(self) -> None:
        for e in self.entries:
            with self.subTest(e["id"]):
                self.assertIn(e["comparison"], cu.COMPARISONS)
                if "code" in e["match"]:
                    self.assertIsNone(e["fixture"])
                    continue
                fx = e["fixture"]
                d = cu.Diff(
                    fx["category"].removeprefix("cutover_diff_"),
                    fx.get("key", ""),
                    fx.get("gn", ""),
                    fx.get("bazel", ""),
                    item=fx.get("item", ""),
                    comparison=e["comparison"],
                )
                self.assertIn(d.kind, cu.DIFF_KINDS)
                self.assertTrue(cu.normalization_matches(e, d))

    def test_generated_manifest_path_normalization(self) -> None:
        g = 'C {\n    sources: [\n        "obj/a/x_component_generated_manifest.cml",\n    ],\n}'
        b = 'C {\n    sources: [\n        "bazel-out/k8/bin/a/x.component.generated_manifest.cml",\n    ],\n}'
        diffs = cu.compare_facts(
            facts(cm={"meta/x.cm": g}), facts(cm={"meta/x.cm": b})
        )
        kept, applied = cu.apply_normalizations(diffs, self.entries)
        self.assertEqual(
            (kept, applied), ([], {"pkg_cm_generated_manifest_path": 2})
        )

    def test_rustc_search_paths_are_not_hidden_by_link_spelling(self) -> None:
        def norm(item: str, key: str) -> dict[str, int]:
            d = cu.Diff(
                "cmd_flags",
                key,
                "present",
                "absent",
                item=item,
                comparison="build_commands",
            )
            return cu.apply_normalizations([d], self.entries)[1]

        self.assertEqual(
            norm("-L gen/zircon/public/sysroot/lib", "bin/x rustc"),
            {"cmd_rust_sysroot_search_path": 1},
        )
        self.assertEqual(norm("-l zircon", "bin/x rustc"), {})
        self.assertEqual(
            norm("-lfdio", "bin/x cpp_link"), {"cmd_link_library_spelling": 1}
        )
        self.assertEqual(
            norm("-lfdio", "bin/x cpp_link arm64"),
            {"cmd_link_library_spelling": 1},
        )
        self.assertEqual(norm("-lfdio", "bin/x rustc arm64"), {})

    def test_fuchsia_link_pthread_normalization_only_matches_cpp_link(
        self,
    ) -> None:
        def norm(item: str, key: str) -> dict[str, int]:
            d = cu.Diff(
                "cmd_flags",
                key,
                "absent",
                "present",
                item=item,
                comparison="build_commands",
            )
            return cu.apply_normalizations([d], self.entries)[1]

        self.assertEqual(
            norm("-pthread", "bin/unit_tests cpp_link"),
            {"cmd_fuchsia_link_pthread": 1},
        )
        self.assertEqual(
            norm("-pthread", "bin/unit_tests cpp_link arm64"),
            {"cmd_fuchsia_link_pthread": 1},
        )
        self.assertEqual(norm("-pthread", "bin/unit_tests cpp_compile"), {})
        self.assertEqual(
            norm("-pthread", "bin/unit_tests cpp_compile arm64"), {}
        )
        self.assertEqual(norm("-pthread", "bin/unit_tests rustc"), {})

    def test_markdown_is_up_to_date(self) -> None:
        with open(os.path.join(_DIR, "CUTOVER_NORMALIZATIONS.md")) as f:
            current = f.read()
        self.assertEqual(
            current,
            cu.registry_markdown(self.entries, cu.load_categories()),
            "CUTOVER_NORMALIZATIONS.md is stale; regenerate it with "
            "`verify_package_cutover.py --list-normalizations --markdown > CUTOVER_NORMALIZATIONS.md`",
        )


class CompareTest(unittest.TestCase):
    def test_clean(self) -> None:
        findings, applied, counts = vpc.compare(
            facts(),
            facts(merkle="other"),
            [],
            "src/snap/BUILD.bazel",
            "snapshot",
        )
        self.assertEqual(
            (findings, applied, counts),
            ([], {}, {"package_contents": 0, "build_commands": 0}),
        )

    def test_opt_level_is_a_b_finding(self) -> None:
        findings, _, counts = vpc.compare(
            facts(), facts(opt="-O0"), [], "src/snap/BUILD.bazel", "snapshot"
        )
        self.assertEqual(counts, {"package_contents": 0, "build_commands": 1})
        (f,) = findings
        self.assertEqual(f["category"], "cutover_diff_cmd_flags")
        lines = f["message"].splitlines()
        self.assertEqual(
            lines[0],
            "[snapshot:bin/snapshot cpp_compile] The compile or link flags differ (build command comparison).",
        )
        self.assertEqual(lines[1:3], ["GN: -O2", "Bazel: -O0"])
        self.assertIn("flag category:", lines[3])

    def test_normalization_drops_and_counts(self) -> None:
        norm = {
            "id": "n",
            "comparison": "build_commands",
            "match": {"category": "cutover_diff_cmd_flags", "item": "-O[02]"},
            "reason": "r",
            "source": "s",
        }
        findings, applied, _ = vpc.compare(
            facts(), facts(opt="-O0"), [norm], "f", "snapshot"
        )
        self.assertEqual((findings, applied), ([], {"n": 2}))

    def test_coverage_lists_unmatched_binaries_per_cpu(self) -> None:
        bazel = facts(commands={})
        self.assertEqual(
            vpc.command_coverage_gaps(facts(), bazel),
            [
                "arm64 build commands for bin/snapshot",
                "x64 build commands for bin/snapshot",
            ],
        )

    def test_both_cpus_covered_means_no_not_covered_finding(self) -> None:
        both = {
            "bin/snapshot cpp_compile": {"label": "x", "args": ["-O2"]},
            "bin/snapshot cpp_compile arm64": {"label": "x", "args": ["-O2"]},
        }
        f = facts(commands=both)
        self.assertEqual(vpc.not_covered_findings(f, f, "f", "quick"), [])
        self.assertEqual(vpc.command_coverage_gaps(f, f), [])
        gn = facts(commands=both)
        bazel = facts(
            commands=dict(
                both,
                **{
                    "bin/snapshot cpp_compile arm64": {
                        "label": "x",
                        "args": ["-O0"],
                    }
                },
            )
        )
        findings, _, counts = vpc.compare(gn, bazel, [], "f", "snapshot")
        self.assertEqual(
            findings[0]["message"].splitlines()[0],
            "[snapshot:bin/snapshot cpp_compile arm64] The compile or link flags differ (build command comparison).",
        )
        self.assertEqual(counts["build_commands"], 1)

    def test_build_commands_not_run_is_not_zero(self) -> None:
        data = facts(blobs={"data/x": {"merkle": "d", "size": 1}}, commands={})
        self.assertEqual(
            vpc.compare(data, data, [], "f", "p")[2]["build_commands"],
            "not applicable (no binaries)",
        )
        contents = facts()
        del contents["commands"]
        self.assertEqual(
            vpc.compare(contents, contents, [], "f", "p")[2]["build_commands"],
            "not compared (--tier contents)",
        )


BAZEL_BUILD = """fx_cc_binary(name = "main", srcs = ["main.cc"])
fx_packaged_binary(name = "main_packaged", binary = ":main", binary_name = "snapshot")
rustc_binary(name = "bin", output_name = "tool")
fx_package(name = "pkg", package_name = "snapshot")
"""


ANCHOR_BUILD = """fx_cc_binary(name = "main", srcs = ["main.cc"])

fx_packaged_binary(name = "main_packaged", binary = ":main", binary_name = "snapshot")

fx_component_manifest(
    name = "manifest",
    component_name = "snapshot",
    manifest = "meta/snapshot.cml",
)

fx_package(
    name = "pkg",
    package_name = "snapshot",
)
"""


class AnchorTest(unittest.TestCase):
    def test_targets(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "src/snap"))
            with open(os.path.join(tmp, "src/snap/BUILD.bazel"), "w") as f:
                f.write(ANCHOR_BUILD)
            c = cu.Cutover(
                "snapshot", "//src/snap:pkg", "//src/snap:pkg", gn_state="gone"
            )
            anchor = vpc.make_anchor(tmp, c)
            file = "src/snap/BUILD.bazel"
            self.assertEqual(
                anchor(cu.Diff("missing_blob", "data/x", "", "")),
                (file, 11, "//src/snap:pkg"),
            )
            self.assertEqual(
                anchor(cu.Diff("cm", "meta/snapshot.cm", "", "")),
                (file, 5, "//src/snap:manifest"),
            )
            self.assertEqual(
                anchor(
                    cu.Diff(
                        "cmd_flags",
                        "bin/snapshot cpp_link",
                        "",
                        "",
                        comparison="build_commands",
                    )
                ),
                (file, 1, "//src/snap:main"),
            )
            # Unknown binary: keep the file, no line.
            self.assertEqual(
                anchor(
                    cu.Diff(
                        "cmd_flags",
                        "bin/other rustc",
                        "",
                        "",
                        comparison="build_commands",
                    )
                ),
                (file, 0, ""),
            )


class ReplayMainTest(unittest.TestCase):
    def test_compare_captured_facts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            for name, f in (
                ("gn.json", facts()),
                ("bazel.json", facts(opt="-O0")),
            ):
                with open(os.path.join(tmp, name), "w") as facts_file:
                    json.dump(f, facts_file)
            out = io.StringIO()
            argv = [
                "vpc",
                "--json",
                "--workdir",
                tmp,
                "--gn-facts",
                os.path.join(tmp, "gn.json"),
                "--bazel-facts",
                os.path.join(tmp, "bazel.json"),
            ]
            with mock.patch.object(sys, "argv", argv), mock.patch(
                "sys.stdout", out
            ):
                self.assertEqual(vpc.main(), 0)
        findings = json.loads(out.getvalue())
        self.assertEqual(
            [f["category"] for f in findings],
            [
                "cutover_diff_cmd_flags",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        self.assertIn(
            "Differences: package contents 0, build commands 1.",
            findings[-1]["message"],
        )
        for f in findings:
            self.assertNotIn("approach", f["message"].lower())


class CpuTest(unittest.TestCase):
    def test_cpu_dir_regenerates_when_args_change(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            build = os.path.join(tmp, "out", "x")
            os.makedirs(build)
            with open(os.path.join(build, "args.gn"), "w") as f:
                f.write('import("//boards/x64.gni")\nfoo = true\n')
            with open(os.path.join(build, "args.json"), "w") as f:
                json.dump({"target_cpu": "x64"}, f)
            other = build + ".cutover-arm64"
            os.makedirs(other)
            for name, text in (
                ("build.ninja", ""),
                ("args.gn", 'import("//boards/arm64.gni")\n'),
            ):
                with open(os.path.join(other, name), "w") as f:
                    f.write(text)
            checkout = vpc.Checkout(tmp, build)
            with mock.patch.object(checkout, "lock"), mock.patch.object(
                checkout, "run", return_value=(0, "")
            ) as run:
                self.assertEqual(checkout.cpu_dir("arm64"), other)
            run.assert_called_once_with(["--dir", other, "gen"])
            self.assertEqual(
                cu.read_text(os.path.join(other, "args.gn")),
                'import("//boards/arm64.gni")\nfoo = true\n',
            )

    def test_cpu_args_swaps_the_board(self) -> None:
        args = 'import("//boards/x64.gni")\nimport("//products/minimal.gni")\nbuild_info_board = "x64"\n'
        self.assertEqual(
            vpc.cpu_args(args, "x64", "arm64"),
            'import("//boards/arm64.gni")\nimport("//products/minimal.gni")\nbuild_info_board = "arm64"\n',
        )

    def test_cpu_args_without_a_board_sets_target_cpu(self) -> None:
        self.assertEqual(
            vpc.cpu_args("a = 1\n", "x64", "arm64"),
            'a = 1\ntarget_cpu = "arm64"\n',
        )

    def test_swap_toolchain_cpu(self) -> None:
        self.assertEqual(
            vpc.swap_toolchain_cpu(
                "//sdk/lib/fdio:fdio(//build/toolchain/fuchsia:x64-shared)",
                "x64",
                "arm64",
            ),
            "//sdk/lib/fdio:fdio(//build/toolchain/fuchsia:arm64-shared)",
        )
        self.assertEqual(
            vpc.swap_toolchain_cpu("//src/x:bin", "x64", "arm64"), "//src/x:bin"
        )


class PairFlagValuesTest(unittest.TestCase):
    def test_pairs_bare_values_with_their_flag(self) -> None:
        raw = "rustc -L gen/zircon/public/sysroot/lib -o out main.rs"
        self.assertEqual(
            vpc.pair_flag_values(
                ["gen/zircon/public/sysroot/lib", "main.rs", "-Copt-level=z"],
                raw,
            ),
            ["-L gen/zircon/public/sysroot/lib", "main.rs", "-Copt-level=z"],
        )


class BazelBinariesTest(unittest.TestCase):
    def test_mapping(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "src/snap"))
            with open(os.path.join(tmp, "src/snap/BUILD.bazel"), "w") as f:
                f.write(BAZEL_BUILD)
            bins = vpc.bazel_binaries(tmp, "//src/snap:pkg")
        self.assertEqual(
            bins,
            {
                "snapshot": {
                    "label": "//src/snap:main",
                    "actions": ["cpp_link", "cpp_compile"],
                    "source": "main.cc",
                },
                "main": {
                    "label": "//src/snap:main",
                    "actions": ["cpp_link", "cpp_compile"],
                    "source": "main.cc",
                },
                "tool": {
                    "label": "//src/snap:bin",
                    "actions": ["rustc"],
                    "source": None,
                },
            },
        )


GN_BUILD = 'fuchsia_package("pkg") {\n  package_name = "snapshot"\n}\n'
BAZEL_PKG = 'fx_package(name = "pkg", package_name = "snapshot")\n'


class RunDetectedTest(unittest.TestCase):
    """Runs the Planter flow on a tiny git repo with `capture` mocked."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name
        self.git("init", "-q")
        self.write("src/snap/BUILD.gn", GN_BUILD)
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "base",
        )
        self.args = argparse.Namespace(
            workdir=self.dir,
            build_dir=self.dir,
            base="HEAD",
            phase="auto",
            tier="quick",
            no_build=False,
        )
        env = {
            k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")
        }
        self.env = mock.patch.dict(os.environ, env, clear=True)
        self.env.start()

    def tearDown(self) -> None:
        self.env.stop()
        self.tmp.cleanup()

    def git(self, *args: str) -> None:
        subprocess.check_call(["git", "-C", self.dir] + list(args))

    def write(self, path: str, text: str) -> None:
        full = os.path.join(self.dir, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)

    def run_with(self, gn: T.Any, bazel: T.Any) -> list[dict[str, T.Any]]:
        """Runs the flow; passing GN facts means Planter's phase-1 run."""

        def fake_capture(side: str, *args: T.Any, **kwargs: T.Any) -> T.Any:
            return (gn if side == "gn" else bazel), []

        env = {"PLANTER_CUTOVER_PHASE": "parallel"} if gn is not None else {}
        with mock.patch.object(
            vpc, "capture", side_effect=fake_capture
        ), mock.patch.dict(os.environ, env):
            return vpc.run_detected(self.args, [])

    def categories(self, findings: list[dict[str, T.Any]]) -> list[str]:
        return [f["category"] for f in findings]

    def test_no_cutover_prints_nothing(self) -> None:
        self.write("src/snap/README.md", "x")
        self.assertEqual(self.run_with(facts(), facts()), [])

    def test_parallel_then_final(self) -> None:
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "pkg", package_name = "snapshot")\n',
        )
        findings = self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        self.assertTrue(all(f["severity"] == "INFO" for f in findings))
        self.assertIn("phase-1 record written", findings[-1]["message"])
        self.assertTrue(
            os.path.exists(cu.record_path(self.dir, "snapshot", "pkg"))
        )

        # Final: GN package deleted, Bazel package unchanged. The packages are
        # compared again with the same result.
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts(opt="-O0", merkle="p1"))
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        self.assertNotIn("(Recorded in phase 1.)", findings[0]["message"])
        self.assertIn(
            "Guard: passed: the Bazel package is unchanged since phase 1 "
            "(meta.far p1)",
            findings[-1]["message"],
        )
        self.assertIn(
            "Not covered: arm64 build commands for bin/snapshot",
            findings[-1]["message"],
        )

        # Final with a changed Bazel package: the recompare reports the real
        # difference, the summary names the changed file, and the guard
        # flags the difference that phase 1 did not report.
        bazel = facts(merkle="p2", blobs={})
        findings = self.run_with(None, bazel)
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_missing_blob",
                "cutover_diff_guard",
                "cutover_summary",
            ],
        )
        self.assertTrue(
            findings[0]["message"].startswith("[snapshot:bin/snapshot] ")
        )
        summary = findings[-1]["message"]
        self.assertIn("the Bazel package changed since phase 1", summary)
        self.assertIn("Changed files: bin/snapshot removed", summary)
        self.assertIn("Guard: FAILED: 1 difference(s)", summary)

    def record(self) -> dict[str, T.Any]:
        record = cu.read_record(cu.record_path(self.dir, "snapshot", "pkg"))
        assert record is not None
        return record

    def rewrite_record(self, record: dict[str, T.Any]) -> None:
        with open(cu.record_path(self.dir, "snapshot", "pkg"), "w") as f:
            json.dump(record, f)

    def make_v1_record(self) -> None:
        record = self.record()
        del record["gn_facts"], record["build_cpu"]
        record["schema_version"] = 1
        self.rewrite_record(record)

    def test_record_keeps_gn_facts_and_commands(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        gn = facts(sources={"bin/snapshot": "/out/exe.unstripped/snapshot"})
        self.run_with(gn, facts(merkle="p1"))
        record = self.record()
        self.assertEqual(record["schema_version"], 2)
        self.assertEqual(record["build_cpu"], "x64")
        expected = dict(gn)
        del expected["sources"]
        self.assertEqual(record["gn_facts"], expected)

    def test_final_recompares_changed_commands(self) -> None:
        # Live-run bug (fxr/1971475): `crate_name` was removed after phase 1
        # and the final phase did not report the `--crate-name` difference.
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        rustc = {
            "bin/snapshot rustc": {
                "label": "x",
                "args": ["--crate-name reachability", "-Copt-level=z"],
            },
        }
        self.run_with(facts(commands=rustc), facts(merkle="p1", commands=rustc))
        self.write("src/snap/BUILD.gn", "")
        changed_elf = dict(ELF, needed=[])
        changed = facts(
            merkle="p2",
            blobs={
                "bin/snapshot": {"merkle": "b", "size": 1, "elf": changed_elf}
            },
            commands={
                "bin/snapshot rustc": {
                    "label": "x",
                    "args": ["--crate-name bin", "-Copt-level=z"],
                },
            },
        )
        findings = self.run_with(None, changed)
        # New differences after a Bazel change: the guard asks for a verdict.
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_diff_elf_needed",
                "cutover_diff_guard",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        guard = findings[2]["message"]
        self.assertTrue(guard.startswith("[snapshot:meta.far] "))
        self.assertIn(
            "New or changed differences (reported above): [snapshot:bin/snapshot], "
            "[snapshot:bin/snapshot rustc]",
            guard,
        )
        self.assertEqual(findings[2]["severity"], "INFO")
        self.assertIn("Guard: FAILED: 2 difference(s)", findings[-1]["message"])
        self.assertIn("GN: --crate-name reachability", findings[0]["message"])
        self.assertIn("Bazel: --crate-name bin", findings[0]["message"])
        self.assertIn("GN: libc.so", findings[1]["message"])
        self.assertIn(
            "Changed files: bin/snapshot a -> b", findings[-1]["message"]
        )
        self.assertIn(
            "Differences: package contents 1, build commands 1.",
            findings[-1]["message"],
        )

    def test_unchanged_package_gives_phase_one_findings(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        bazel = facts(opt="-O0", merkle="p1", blobs={})
        phase1 = self.run_with(facts(), bazel)
        self.write("src/snap/BUILD.gn", "")
        final = self.run_with(None, bazel)
        self.assertEqual(final[:-1], phase1[:-1])
        self.assertNotIn("cutover_diff_guard", self.categories(final))
        self.assertIn("unchanged since phase 1", final[-1]["message"])

    def test_rebase_is_noted_not_failed(self) -> None:
        bazel_build = 'fx_package(name = "pkg", package_name = "snapshot")\n'
        self.write("src/snap/BUILD.bazel", bazel_build)
        self.run_with(facts(), facts(merkle="p1"))
        self.rebase(bazel_build)
        findings = self.run_with(None, facts(merkle="p2"))
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_not_covered", "cutover_summary"],
        )
        self.assertIn("the change base moved", findings[-1]["message"])
        self.assertIn("Guard: passed: no difference", findings[-1]["message"])
        self.assertIn("Changed files: none found", findings[-1]["message"])

    def rebase(self, bazel_build: str) -> None:
        """Moves the change onto a new base, then finishes the cutover."""
        os.remove(os.path.join(self.dir, "src/snap/BUILD.bazel"))
        self.write("other.txt", "x")
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "upstream",
        )
        self.write("src/snap/BUILD.bazel", bazel_build)
        self.write("src/snap/BUILD.gn", "")

    def test_v1_record_replays_and_guards(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        self.make_v1_record()
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts(opt="-O0", merkle="p1"))
        self.assertEqual(
            self.categories(findings),
            ["cutover_diff_cmd_flags", "cutover_summary"],
        )
        self.assertIn("(Recorded in phase 1.)", findings[0]["message"])
        self.assertIn(
            "Guard: record predates recompare (v1); passed",
            findings[-1]["message"],
        )

        findings = self.run_with(None, facts(merkle="p2", blobs={}))
        self.assertEqual(
            self.categories(findings),
            ["cutover_diff_cmd_flags", "cutover_diff_guard", "cutover_summary"],
        )
        guard = findings[1]["message"]
        self.assertTrue(guard.startswith("[snapshot:meta.far] "))
        self.assertIn("Changed files: bin/snapshot removed", guard)
        self.assertIn(
            "Changed facts: cutover_diff_missing_blob bin/snapshot", guard
        )
        self.assertNotIn("non-deterministic", guard)
        self.assertIn(
            "Guard: record predates recompare (v1); FAILED",
            findings[-1]["message"],
        )

    def test_v1_record_rebase_turns_guard_into_info(self) -> None:
        bazel_build = 'fx_package(name = "pkg", package_name = "snapshot")\n'
        self.write("src/snap/BUILD.bazel", bazel_build)
        self.run_with(facts(), facts(merkle="p1"))
        self.make_v1_record()
        self.rebase(bazel_build)
        findings = self.run_with(None, facts(merkle="p2", blobs={}))
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_guard_rebased", "cutover_summary"],
        )
        self.assertIn(
            "cutover_diff_missing_blob bin/snapshot", findings[0]["message"]
        )
        self.assertIn("skipped (rebased", findings[-1]["message"])

    def test_harness_record_dir(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        with tempfile.TemporaryDirectory() as records:
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                stale = os.path.join(records, "stale.json")
                with open(stale, "w") as f:
                    json.dump(
                        {"schema_version": 2, "timestamp": 0, "workdir": "/x"},
                        f,
                    )
                self.run_with(facts(), facts(opt="-O0", merkle="p1"))
                path = os.path.join(records, "snapshot--pkg.json")
                self.assertEqual(
                    cu.record_path(self.dir, "snapshot", "pkg"), path
                )
                self.assertTrue(os.path.exists(path))
                # Planter owns the directory: nothing is pruned there.
                self.assertTrue(os.path.exists(stale))
                # A task resumed in another checkout still finds its record.
                record = self.record()
                record["workdir"] = "/some/other/worktree"
                self.rewrite_record(record)
                self.write("src/snap/BUILD.gn", "")
                findings = self.run_with(None, facts(opt="-O0", merkle="p1"))
                self.assertEqual(
                    self.categories(findings),
                    [
                        "cutover_diff_cmd_flags",
                        "cutover_info_not_covered",
                        "cutover_summary",
                    ],
                )
                # Another cutover's record is still ignored.
                record["gn_label"] = "//src/other:pkg"
                self.rewrite_record(record)
                findings = self.run_with(None, facts(opt="-O0", merkle="p1"))
                self.assertEqual(
                    self.categories(findings),
                    ["cutover_info_phase1_missing", "cutover_summary"],
                )
        # Nothing was written to the git common dir.
        self.assertFalse(
            os.path.exists(os.path.join(self.dir, ".git", "planter-cutover"))
        )

    def test_harness_record_dir_adopts_a_local_record(self) -> None:
        # A task that was mid-cutover when Planter started passing the
        # directory: its record is in the git common dir.
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        local = cu.record_path(self.dir, "snapshot", "pkg")
        self.write("src/snap/BUILD.gn", "")
        with tempfile.TemporaryDirectory() as records:
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                findings = self.run_with(None, facts(opt="-O0", merkle="p1"))
                self.assertEqual(
                    self.categories(findings),
                    [
                        "cutover_diff_cmd_flags",
                        "cutover_info_not_covered",
                        "cutover_summary",
                    ],
                )
                # Copied, so Planter stores it in the ledger.
                copied = cu.read_record(
                    os.path.join(records, "snapshot--pkg.json")
                )
                self.assertEqual(copied, cu.read_record(local))
                self.assertEqual(os.listdir(records), ["snapshot--pkg.json"])

    def test_harness_record_dir_never_adopts_an_expired_record(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        record = self.record()
        record["timestamp"] = 0
        self.rewrite_record(record)
        local = cu.record_path(self.dir, "snapshot", "pkg")
        self.write("src/snap/BUILD.gn", "")
        with tempfile.TemporaryDirectory() as records:
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                findings = self.run_with(None, facts(merkle="p1"))
                self.assertEqual(os.listdir(records), [])
        self.assertFalse(os.path.exists(local))
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )

    def test_harness_record_dir_keeps_another_cutovers_record(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        self.write("src/snap/BUILD.gn", "")
        with tempfile.TemporaryDirectory() as records:
            # An old-style record of another cutover with the same name.
            other = os.path.join(records, "snapshot.json")
            with open(other, "w") as f:
                json.dump({"schema_version": 2, "gn_label": "//x:y"}, f)
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                findings = self.run_with(None, facts(merkle="p1"))
            with open(other) as f:
                self.assertEqual(json.load(f)["gn_label"], "//x:y")
            self.assertEqual(
                sorted(os.listdir(records)),
                ["snapshot--pkg.json", "snapshot.json"],
            )
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_not_covered", "cutover_summary"],
        )

    def test_changed_details_under_a_reported_key_trip_the_guard(self) -> None:
        # Phase 1 reported a flag difference under `bin/snapshot cpp_compile`
        # and the uploader may have called it benign; after phase 1 another
        # flag differs under the same key. That is new content: the guard
        # asks again instead of letting the old verdict cover it (G3).
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        phase1 = self.record()["findings"]
        self.assertTrue(all(f.get("fingerprint") for f in phase1))
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts(opt="-O3", merkle="p2"))
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_diff_guard",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        self.assertIn(
            "[snapshot:bin/snapshot cpp_compile]", findings[1]["message"]
        )
        self.assertNotEqual(
            findings[0]["fingerprint"], phase1[0]["fingerprint"]
        )
        self.assertIn("Guard: FAILED: 1 difference(s)", findings[-1]["message"])

    def test_record_from_another_build_cpu_keeps_guard_keys(self) -> None:
        # Phase 1 ran in an x64 checkout; the final phase runs in an arm64
        # one (records move with PLANTER_CUTOVER_RECORD_DIR). The recorded
        # x64 commands are now keyed `... x64`, and so must the recorded
        # findings be, or an unchanged flag difference reads as new.
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        self.write("src/snap/BUILD.gn", "")
        with open(os.path.join(self.dir, "args.json"), "w") as f:
            json.dump({"target_cpu": "arm64"}, f)
        bazel = facts(
            opt="-O0",
            merkle="p2",
            commands={
                "bin/snapshot cpp_compile x64": {
                    "label": "x",
                    "args": ["clang++", "-O0", "-c"],
                }
            },
        )
        findings = self.run_with(None, bazel)
        self.assertNotIn("cutover_diff_guard", self.categories(findings))
        self.assertTrue(
            findings[0]["message"].startswith(
                "[snapshot:bin/snapshot cpp_compile x64] "
            )
        )
        self.assertIn("Guard: passed: no difference", findings[-1]["message"])

    def test_new_difference_keys_compares_content(self) -> None:
        def f(key: str, fp: str = "", body: str = "GN: a") -> dict[str, T.Any]:
            out = {
                "category": "cutover_diff_cmd_flags",
                "message": f"[snapshot:{key}] x\nTarget: //t\n{body}",
            }
            if fp:
                out["fingerprint"] = fp
            return out

        self.assertEqual(
            vpc.new_difference_keys([f("k", "1")], [f("k", "1")]), []
        )
        self.assertEqual(
            vpc.new_difference_keys([f("k", "1")], [f("k", "2")]),
            ["snapshot:k"],
        )
        self.assertEqual(
            vpc.new_difference_keys([f("k", "1")], [f("j", "1")]),
            ["snapshot:j"],
        )
        # A record from before fingerprints compares the messages, without
        # the key and target lines.
        old = f("k")
        self.assertEqual(vpc.new_difference_keys([old], [f("k", "9")]), [])
        self.assertEqual(
            vpc.new_difference_keys([old], [f("k", "9", body="GN: b")]),
            ["snapshot:k"],
        )

    def test_unpaired_gn_package_deletion_is_not_compared(self) -> None:
        # D1: the GN package is deleted, but the Bazel package that replaces
        # it landed earlier, so no cutover is detected; it was not compared.
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "bazel",
        )
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts())
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_not_compared", "cutover_summary"],
        )
        note, summary = findings
        self.assertEqual(note["file"], "src/snap/BUILD.gn")
        self.assertTrue(note["message"].startswith("[//src/snap:pkg] "))
        self.assertIn("is deleted", note["message"])
        self.assertRegex(note["fingerprint"], r"^[0-9a-f]{16}$")
        self.assertIn(
            "(GN `//src/snap:pkg` -> Bazel `none paired`)", summary["message"]
        )
        self.assertIn(
            "Package contents: not compared (no paired Bazel package)",
            summary["message"],
        )

    def test_unverified_notes_have_stable_fingerprints(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.write("src/snap/BUILD.gn", "")
        first = self.run_with(None, facts())
        self.assertEqual(
            self.categories(first),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )
        # The message names the record path, which differs per checkout;
        # the fingerprint does not.
        with tempfile.TemporaryDirectory() as records:
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                second = self.run_with(None, facts())
        self.assertNotEqual(first[0]["message"], second[0]["message"])
        self.assertEqual(first[0]["fingerprint"], second[0]["fingerprint"])

    def test_jiri_sync_shape_has_no_guard(self) -> None:
        # fxr/1972034 shape: the Bazel package changed after phase 1 (e.g.
        # a jiri sync), but no difference is new.
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        self.write("src/snap/BUILD.gn", "")
        resynced = facts(
            opt="-O0",
            merkle="p2",
            blobs={
                "bin/snapshot": {"merkle": "z", "size": 1, "elf": dict(ELF)}
            },
        )
        findings = self.run_with(None, resynced)
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        summary = findings[-1]["message"]
        self.assertIn("Guard: passed: no difference", summary)
        self.assertIn("Changed files: bin/snapshot a -> z", summary)

    def test_shared_package_names_get_their_own_records_and_keys(self) -> None:
        gn = (
            'fuchsia_package("package-debug") {\n'
            '  package_name = "snapshot"\n}\n'
            'fuchsia_package("package-debug-less-logs") {\n'
            '  package_name = "snapshot"\n}\n'
        )
        self.write("src/snap/BUILD.gn", gn)
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "two",
        )
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "package-debug", package_name = "snapshot")\n'
            'fx_package(name = "package-debug-less-logs", package_name = "snapshot")\n',
        )
        findings = self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        flags = [
            f["message"].splitlines()[0]
            for f in findings
            if f["category"] == "cutover_diff_cmd_flags"
        ]
        self.assertEqual(
            [m.split("]")[0] for m in flags],
            [
                "[snapshot--package-debug:bin/snapshot cpp_compile",
                "[snapshot--package-debug-less-logs:bin/snapshot cpp_compile",
            ],
        )
        for target in ("package-debug", "package-debug-less-logs"):
            self.assertTrue(
                os.path.exists(cu.record_path(self.dir, "snapshot", target))
            )

    def test_ambiguous_gn_package_is_not_compared(self) -> None:
        gn = (
            'fuchsia_package("a") {\n  package_name = "snapshot"\n}\n'
            'fuchsia_package("b") {\n  package_name = "snapshot"\n}\n'
        )
        self.write("src/snap/BUILD.gn", gn)
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "two",
        )
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        findings = self.run_with(facts(), facts())
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_info_ambiguous",
                "cutover_info_not_compared",
                "cutover_summary",
            ],
        )
        self.assertIn("`//src/snap:a`, `//src/snap:b`", findings[0]["message"])
        self.assertIn("Not covered: everything", findings[-1]["message"])

    def test_bridge_in_place_is_a_cutover(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.write(
            "src/snap/BUILD.gn",
            'bazel_fuchsia_package("pkg") {\n  package_name = "snapshot"\n}\n',
        )
        findings = self.run_with(None, facts())
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )
        self.assertIn("phase `final`", findings[-1]["message"])

    def test_dual_build_reported_once_with_reason(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        findings = self.run_with(None, facts())
        (f,) = findings
        self.assertEqual(f["category"], "cutover_info_dual_build_only")
        self.assertIn(
            "`snapshot` (held back: the hand-written GN package still exists)",
            f["message"],
        )

    def test_old_record_name_is_read_and_migrated(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        new = cu.record_path(self.dir, "snapshot", "pkg")
        old = cu.record_path(self.dir, "snapshot")
        os.rename(new, old)
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts(opt="-O0", merkle="p1"))
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_info_not_covered",
                "cutover_summary",
            ],
        )
        self.assertTrue(os.path.exists(new))
        # An old unsuffixed `[snapshot:...]` finding key matches a suffixed
        # `[snapshot--pkg:...]` key when another cutover shares the name.
        self.assertEqual(
            vpc.new_difference_keys(
                [
                    {
                        "category": "cutover_diff_cmd_flags",
                        "message": "[snapshot:bin/snapshot cpp_compile] x",
                    }
                ],
                [
                    {
                        "category": "cutover_diff_cmd_flags",
                        "message": "[snapshot--pkg:bin/snapshot cpp_compile] x",
                    }
                ],
            ),
            [],
        )

    def test_harness_record_dir_ignores_other_checkouts_local_record(
        self,
    ) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        record = self.record()
        record["workdir"] = "/some/other/worktree"
        self.rewrite_record(record)
        self.write("src/snap/BUILD.gn", "")
        with tempfile.TemporaryDirectory() as records:
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                findings = self.run_with(None, facts(merkle="p1"))
                self.assertEqual(os.listdir(records), [])
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )

    def test_harness_phase_one_replaces_the_local_record(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        local = cu.record_path(self.dir, "snapshot", "pkg")
        with tempfile.TemporaryDirectory() as records:
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                # A phase 1 that fails must not leave the old record to be
                # adopted by the final phase.
                self.args.phase = "parallel"
                self.run_with(None, None)
                self.assertFalse(os.path.exists(local))
                self.assertEqual(os.listdir(records), [])
                self.run_with(facts(), facts(merkle="p2"))
                # Only the compact record: no manifest copy.
                self.assertEqual(os.listdir(records), ["snapshot--pkg.json"])
                with open(os.path.join(records, "snapshot--pkg.json")) as f:
                    self.assertNotIn("\n", f.read())

    def test_error_in_one_cutover_keeps_recorded_findings(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(opt="-O0", merkle="p1"))
        self.write("src/snap/BUILD.gn", "")
        with mock.patch.object(vpc, "capture", side_effect=ValueError("boom")):
            findings = vpc.run_detected(self.args, [])
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_diff_cmd_flags",
                "cutover_info_tool_unavailable",
                "cutover_info_not_compared",
                "cutover_summary",
            ],
        )
        self.assertIn("internal error", findings[-1]["message"])

    def test_record_from_another_checkout_is_ignored(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        path = cu.record_path(self.dir, "snapshot", "pkg")
        record = cu.read_record(path)
        assert record is not None
        record["workdir"] = "/some/other/worktree"
        with open(path, "w") as f:
            json.dump(record, f)
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts(merkle="p1"))
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )

    def test_guard_uses_the_recorded_package_cpu(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        path = cu.record_path(self.dir, "snapshot", "pkg")
        record = cu.read_record(path)
        assert record is not None
        self.assertEqual(record["package_cpu"], "x64")
        record["package_cpu"] = "arm64"
        with open(path, "w") as f:
            json.dump(record, f)
        self.write("src/snap/BUILD.gn", "")
        cpus = []

        def fake_capture(side: str, checkout: T.Any, *a: T.Any) -> T.Any:
            cpus.append(checkout.package_cpu)
            return facts(merkle="p1"), []

        with mock.patch.object(vpc, "capture", side_effect=fake_capture):
            findings = vpc.run_detected(self.args, [])
        self.assertEqual(cpus, ["arm64"])
        self.assertIn("compared on arm64", findings[-1]["message"])

        # A record naming an unknown CPU (it names an out dir) is not trusted.
        record["package_cpu"] = "../evil"
        with open(path, "w") as f:
            json.dump(record, f)
        cpus.clear()
        with mock.patch.object(vpc, "capture", side_effect=fake_capture):
            vpc.run_detected(self.args, [])
        self.assertEqual(cpus, ["x64"])

    def test_package_cpu_does_not_leak_between_cutovers(self) -> None:
        seen = []

        def fake_run(c: T.Any, args: T.Any, checkout: T.Any, *a: T.Any) -> None:
            seen.append(checkout.package_cpu)
            checkout.package_cpu = "arm64"  # As a final phase may do.

        with mock.patch.object(
            cu, "detect_cutovers", return_value=[mock.Mock(), mock.Mock()]
        ), mock.patch.object(
            cu, "unpaired_gn_packages", return_value=[]
        ), mock.patch.object(
            vpc, "select_phase", return_value="final"
        ), mock.patch.object(
            vpc, "_run_cutover", side_effect=fake_run
        ):
            vpc.run_detected(self.args, [])
        self.assertEqual(seen, ["x64", "x64"])

    def test_failed_parallel_build_drops_stale_record(self) -> None:
        self.args.phase = "parallel"
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        self.assertTrue(
            os.path.exists(cu.record_path(self.dir, "snapshot", "pkg"))
        )
        self.run_with(None, None)
        self.assertFalse(
            os.path.exists(cu.record_path(self.dir, "snapshot", "pkg"))
        )

    def test_bad_normalizations_do_not_block(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "bad.json")
            with open(bad, "w") as f:
                f.write('[{"id": "x"}]')
            out = io.StringIO()
            with mock.patch.object(
                vpc, "NORMALIZATIONS_JSON", bad
            ), mock.patch.object(
                sys, "argv", ["vpc", "--json", "--workdir", self.dir]
            ), mock.patch(
                "sys.stdout", out
            ):
                self.assertEqual(vpc.main(), 0)
        self.assertEqual(json.loads(out.getvalue()), [])

    def test_skipped_builds_write_no_record(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.args.no_build = True
        findings = self.run_with(facts(), facts())
        self.assertIn("record not written", findings[-1]["message"])
        self.assertFalse(
            os.path.exists(cu.record_path(self.dir, "snapshot", "pkg"))
        )

    def test_exception_in_parallel_drops_stale_record(self) -> None:
        self.args.phase = "parallel"
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        with mock.patch.object(vpc, "capture", side_effect=ValueError("boom")):
            vpc.run_detected(self.args, [])
        self.assertFalse(
            os.path.exists(cu.record_path(self.dir, "snapshot", "pkg"))
        )

    def test_unittest_package_builds_inner_target(self) -> None:
        self.args.phase = "parallel"
        self.write("src/snap/BUILD.gn", 'fuchsia_unittest_package("t") {\n}\n')
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qam",
            "t",
        )
        self.write("src/snap/BUILD.bazel", 'fx_unittest_package(name = "t")\n')
        labels = []

        def fake_capture(
            side: str, checkout: T.Any, label: str, *a: T.Any
        ) -> T.Any:
            labels.append(label)
            return facts(package_name="t"), []

        with mock.patch.object(vpc, "capture", side_effect=fake_capture):
            vpc.run_detected(self.args, [])
        self.assertEqual(labels, ["//src/snap:t", "//src/snap:t.package"])

    def test_bad_normalizations_are_reported_on_a_cutover(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "bad.json")
            with open(bad, "w") as f:
                f.write("not json")
            out = io.StringIO()
            summary = cu.new_finding(
                "cutover_consistency", "cutover_summary", "f", "s"
            )
            with mock.patch.object(
                vpc, "NORMALIZATIONS_JSON", bad
            ), mock.patch.object(
                vpc, "run_detected", return_value=[summary]
            ), mock.patch.object(
                sys, "argv", ["vpc", "--json", "--workdir", self.dir]
            ), mock.patch(
                "sys.stdout", out
            ):
                self.assertEqual(vpc.main(), 0)
        cats = [f["category"] for f in json.loads(out.getvalue())]
        self.assertEqual(
            cats, ["cutover_summary", "cutover_info_tool_unavailable"]
        )

    def test_skipped_builds_keep_planters_record(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.run_with(facts(), facts(merkle="p1"))
        self.args.no_build = True
        self.run_with(facts(), facts(merkle="p1"))
        self.assertTrue(
            os.path.exists(cu.record_path(self.dir, "snapshot", "pkg"))
        )

    def test_malformed_registry_does_not_block(self) -> None:
        for content in ('["oops"]', '{"id": "x"}'):
            with tempfile.TemporaryDirectory() as tmp:
                bad = os.path.join(tmp, "bad.json")
                with open(bad, "w") as f:
                    f.write(content)
                out = io.StringIO()
                with mock.patch.object(
                    vpc, "NORMALIZATIONS_JSON", bad
                ), mock.patch.object(
                    sys, "argv", ["vpc", "--json", "--workdir", self.dir]
                ), mock.patch(
                    "sys.stdout", out
                ):
                    self.assertEqual(vpc.main(), 0)
                self.assertEqual(json.loads(out.getvalue()), [])

    def test_dual_built_package_is_not_compared(self) -> None:
        # A migrated test package next to its bazel2gn mirror is not a cutover.
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.write(
            "src/snap/BUILD.gn",
            "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
            'fuchsia_package("pkg") {\n  package_name = "snapshot-gn"\n}\n',
        )
        findings = self.run_with(None, facts())
        self.assertEqual(
            self.categories(findings), ["cutover_info_dual_build_only"]
        )
        self.assertIn("`snapshot`", findings[0]["message"])

    def test_mirror_is_not_compared_in_the_parallel_phase(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        self.write(
            "src/snap/BUILD.gn",
            "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
            'fuchsia_package("pkg") {\n  package_name = "snapshot-gn"\n}\n',
        )
        findings = self.run_with(facts(), facts())
        self.assertEqual(
            self.categories(findings), ["cutover_info_dual_build_only"]
        )

    def test_data_only_package_has_no_arm64_note(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        data = facts(blobs={"data/x": {"merkle": "d", "size": 1}}, commands={})
        findings = self.run_with(data, data)
        self.assertEqual(self.categories(findings), ["cutover_summary"])
        self.assertIn("Not covered: nothing", findings[0]["message"])

    def test_final_reanchors_recorded_findings(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        data = facts(blobs={"data/x": {"merkle": "d", "size": 1}}, commands={})
        missing = facts(blobs={}, commands={})
        self.run_with(data, missing)  # Records a missing_blob at line 1.
        # Phase 2 moves the package rule down two lines.
        self.write("src/snap/BUILD.bazel", "# a\n# b\n" + BAZEL_PKG)
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, missing)
        (f,) = [
            f for f in findings if f["category"] == "cutover_diff_missing_blob"
        ]
        self.assertEqual((f["file"], f["line"]), ("src/snap/BUILD.bazel", 3))

    def test_final_without_record(self) -> None:
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "pkg", package_name = "snapshot")\n',
        )
        self.write("src/snap/BUILD.gn", "")
        findings = self.run_with(None, facts())
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )
        self.assertIn("unverified", findings[-1]["message"])

    def test_phase_env_override(self) -> None:
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "pkg", package_name = "snapshot")\n',
        )
        with mock.patch.dict(os.environ, {"PLANTER_CUTOVER_PHASE": "final"}):
            findings = self.run_with(None, facts())
        self.assertEqual(
            self.categories(findings),
            ["cutover_info_phase1_missing", "cutover_summary"],
        )

    def test_build_failure_is_not_compared(self) -> None:
        # Live-run bug: a failed build was summarized as "package contents 0".
        self.args.phase = "parallel"
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        tail = "ERROR: no such package '@gn_targets//src/lib/icu/tzdata'"
        with mock.patch.object(vpc.Checkout, "build", return_value=tail):
            findings = vpc.run_detected(self.args, [])
        self.assertEqual(
            self.categories(findings),
            [
                "cutover_info_not_compared",
                "cutover_info_not_compared",
                "cutover_summary",
            ],
        )
        self.assertIn(tail, findings[0]["message"])
        summary = findings[-1]["message"]
        self.assertIn(
            "- Package contents: not compared (build failed).", summary
        )
        self.assertIn("- Not covered: everything (build failed).", summary)
        self.assertIn("- Normalizations: n/a.", summary)
        self.assertNotIn("compared on", summary)
        self.assertNotIn(" 0,", summary)

    def _stage_check_workdir(self) -> str:
        """Populates `self.dir` with the cutover CLI and an `env -i` python wrapper."""
        verif_dir = os.path.join(self.dir, "build/beads/verifications")
        scripts_dir = os.path.join(self.dir, "build/beads/scripts")
        os.makedirs(verif_dir, exist_ok=True)
        os.makedirs(scripts_dir, exist_ok=True)
        for init_dir in (
            os.path.join(self.dir, "build"),
            os.path.join(self.dir, "build/beads"),
            verif_dir,
            scripts_dir,
        ):
            with open(os.path.join(init_dir, "__init__.py"), "w"):
                pass
        assert vpc.__file__ is not None
        assert cu.__file__ is not None
        shutil.copyfile(
            os.path.abspath(vpc.__file__),
            os.path.join(verif_dir, "verify_package_cutover.py"),
        )
        for name in ("cutover_normalizations.json", "cutover_categories.json"):
            shutil.copyfile(
                os.path.join(_DIR, name), os.path.join(verif_dir, name)
            )
        shutil.copyfile(
            os.path.abspath(cu.__file__),
            os.path.join(scripts_dir, "cutover_utils.py"),
        )
        wrapper = os.path.join(self.dir, "scripts/fuchsia-vendored-python")
        os.makedirs(os.path.dirname(wrapper), exist_ok=True)
        with open(wrapper, "w") as f:
            f.write(
                "#!/usr/bin/env bash\n"
                'exec /usr/bin/env -i PATH="$PATH" HOME="${HOME:-/tmp}" '
                f'{shlex.quote(sys.executable)} "$@"\n'
            )
        os.chmod(wrapper, 0o755)
        fx = os.path.join(self.dir, "scripts/fx")
        with open(fx, "w") as f:
            f.write("#!/usr/bin/env bash\nexit 1\n")
        os.chmod(fx, 0o755)
        return os.path.abspath(
            os.path.join(
                _DIR, "../planter/machinery/checks/cutover_consistency.sh"
            )
        )

    @mock.patch.object(vpc, "capture", autospec=True, spec_set=True)
    def test_main_record_dir_flag_sets_and_restores_env(
        self, mock_capture: mock.MagicMock
    ) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        data = facts(blobs={"data/x": {"merkle": "d", "size": 1}}, commands={})
        mock_capture.return_value = (data, [])
        with tempfile.TemporaryDirectory() as records:
            out = io.StringIO()
            with mock.patch.dict(
                os.environ, {cu.RECORD_DIR_ENV: "/prior/records"}
            ), mock.patch.object(
                sys,
                "argv",
                [
                    "vpc",
                    "--workdir",
                    self.dir,
                    "--build-dir",
                    self.dir,
                    "--phase",
                    "parallel",
                    "--record-dir",
                    records,
                    "--json",
                ],
            ), mock.patch.object(
                sys, "stdout", out
            ):
                self.assertEqual(vpc.main(), 0)
                self.assertEqual(
                    os.environ.get(cu.RECORD_DIR_ENV), "/prior/records"
                )
            self.assertTrue(
                os.path.exists(os.path.join(records, "snapshot--pkg.json"))
            )
            self.assertFalse(
                os.path.exists(
                    cu.record_path(self.dir, "snapshot", "pkg", local=True)
                )
            )

    def test_cutover_consistency_script_forwards_record_dir_across_hermetic_env(
        self,
    ) -> None:
        check_script = self._stage_check_workdir()
        self.write("src/snap/BUILD.bazel", BAZEL_PKG)
        data = facts(blobs={"data/x": {"merkle": "d", "size": 1}}, commands={})
        with tempfile.TemporaryDirectory() as caller_cwd:
            records = os.path.join(caller_cwd, "rel-records")
            with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
                self.run_with(data, facts(merkle="p1", blobs={}, commands={}))
                rec = self.record()
                rec["workdir"] = "/some/other/worktree"
                self.rewrite_record(rec)
            self.write("src/snap/BUILD.gn", "")
            env = dict(
                os.environ,
                PLANTER_WORKDIR=self.dir,
                PLANTER_CUTOVER_PHASE="final",
                PLANTER_SKIP_BUILD="1",
                PLANTER_CUTOVER_RECORD_DIR="rel-records",
            )
            proc = subprocess.run(
                ("bash", check_script),
                cwd=caller_cwd,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            findings = json.loads(proc.stdout)
            cats = self.categories(findings)
            self.assertNotIn("cutover_info_phase1_missing", cats)
            self.assertIn("cutover_diff_missing_blob", cats)
            self.assertIn(
                "the differences shown are from phase 1",
                findings[-1]["message"],
            )

    def test_cutover_consistency_script_omits_record_dir_for_legacy_cli(
        self,
    ) -> None:
        check_script = self._stage_check_workdir()
        self.write(
            "build/beads/verifications/verify_package_cutover.py",
            "import argparse\n"
            "p = argparse.ArgumentParser()\n"
            "p.add_argument('--workdir')\n"
            "p.add_argument('--json', action='store_true')\n"
            "p.add_argument('--base')\n"
            "p.add_argument('--phase')\n"
            "p.add_argument('--no-build', action='store_true')\n"
            "p.parse_args()\n"
            "print('[]')\n",
        )
        with tempfile.TemporaryDirectory() as records:
            env = dict(
                os.environ,
                PLANTER_WORKDIR=self.dir,
                PLANTER_CUTOVER_PHASE="final",
                PLANTER_SKIP_BUILD="1",
                PLANTER_CUTOVER_RECORD_DIR=records,
            )
            proc = subprocess.run(
                ("bash", check_script),
                cwd=self.dir,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertEqual(json.loads(proc.stdout), [])


if __name__ == "__main__":
    unittest.main()
