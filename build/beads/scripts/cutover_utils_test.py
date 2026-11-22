# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import shutil
import struct
import subprocess
import tempfile
import typing as T
import unittest
from unittest import mock

from build.beads.scripts import cutover_utils as cu


def write_far(path: str, entries: dict[str, bytes]) -> None:
    """Writes a minimal FAR archive with `entries`."""
    names = sorted(entries)
    name_blob = b"".join(n.encode() for n in names)
    name_blob += b"\0" * (-len(name_blob) % 8)
    index_len = 2 * 24
    dir_off = 16 + index_len
    dir_len = 32 * len(names)
    names_off = dir_off + dir_len
    data_off = names_off + len(name_blob)
    data_off += -data_off % 4096
    out = bytearray(cu.FAR_MAGIC + struct.pack("<Q", index_len))
    out += struct.pack("<8sQQ", b"DIR-----", dir_off, dir_len)
    out += struct.pack("<8sQQ", b"DIRNAMES", names_off, len(name_blob))
    noff, doff, data = 0, data_off, b""
    for n in names:
        content = entries[n]
        out += struct.pack("<IHHQQQ", noff, len(n), 0, doff, len(content), 0)
        noff += len(n)
        data += content + b"\0" * (-len(content) % 4096)
        doff += len(content) + (-len(content) % 4096)
    out += name_blob
    out += b"\0" * (data_off - len(out))
    out += data
    with open(path, "wb") as f:
        f.write(out)


def write_package(
    root: str,
    name: str = "snapshot",
    blobs: dict[str, bytes] | None = None,
    meta: dict[str, bytes] | None = None,
) -> str:
    """Writes a fake built package and returns its package_manifest.json."""
    os.makedirs(root, exist_ok=True)
    blobs = (
        blobs
        if blobs is not None
        else {"bin/snapshot": b"\x7fELFbinary", "data/a.json": b"{}"}
    )
    far = {
        "meta/package": json.dumps({"name": name, "version": "0"}).encode(),
        "meta/contents": b"",
        "meta/fuchsia.abi/abi-revision": struct.pack("<Q", 7),
    }
    far.update(meta or {})
    write_far(os.path.join(root, "meta.far"), far)
    entries = [
        {
            "source_path": "meta.far",
            "path": "meta/",
            "merkle": "m" * 64,
            "size": 1,
        }
    ]
    for path, content in blobs.items():
        src = path.replace("/", "_")
        with open(os.path.join(root, src), "wb") as f:
            f.write(content)
        entries.append(
            {
                "source_path": src,
                "path": path,
                "merkle": cu.hashlib.sha256(content).hexdigest(),
                "size": len(content),
            }
        )
    manifest = os.path.join(root, "package_manifest.json")
    with open(manifest, "w") as f:
        json.dump(
            {
                "version": "1",
                "package": {"name": name},
                "blobs": entries,
                "blob_sources_relative": "file",
            },
            f,
        )
    return manifest


ELF: dict[str, T.Any] = {
    "arch": "Advanced Micro Devices X86-64",
    "needed": ["libc.so", "libzircon.so"],
    "soname": "",
    "interp": "ld.so.1",
    "relro": True,
    "stack": "RW",
    "sections": [".dynsym", ".rodata", ".text"],
    "exports": [],
    "text_size": 100000,
    "stripped": True,
    "has_sections": True,
}


class FarAndFactsTest(unittest.TestCase):
    def test_read_far_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "meta.far")
            write_far(path, {"meta/package": b"{}", "meta/x.cm": b"abc"})
            self.assertEqual(
                cu.read_far(path), {"meta/package": b"{}", "meta/x.cm": b"abc"}
            )

    def test_read_far_rejects_non_far(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "x")
            with open(path, "wb") as f:
                f.write(b"not a far archive")
            with self.assertRaises(ValueError):
                cu.read_far(path)

    def test_package_facts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = write_package(
                tmp,
                meta={"meta/snapshot.cm": b"cm", "meta/snapshot.cvf": b"cvf"},
            )
            facts = cu.package_facts(manifest, cu.Tools())
        self.assertEqual(facts["package_name"], "snapshot")
        self.assertEqual(facts["abi_revision"], struct.pack("<Q", 7).hex())
        self.assertEqual(facts["meta_far_merkle"], "m" * 64)
        self.assertEqual(
            sorted(facts["blobs"]), ["bin/snapshot", "data/a.json"]
        )
        self.assertIn("elf", facts["blobs"]["bin/snapshot"])
        self.assertNotIn("elf", facts["blobs"]["data/a.json"])
        # Without cmc, .cm files are compared by hash.
        self.assertTrue(facts["cm"]["meta/snapshot.cm"].startswith("sha256:"))
        self.assertIn("meta/snapshot.cvf", facts["cvf"])


def facts(**overrides: object) -> dict[str, object]:
    base: dict[str, object] = {
        "package_name": "snapshot",
        "meta_far_merkle": "m",
        "abi_revision": "07",
        "subpackages": [],
        "blobs": {
            "bin/snapshot": {"merkle": "a", "size": 1, "elf": dict(ELF)},
            "data/x": {"merkle": "d", "size": 1},
        },
        "meta": {},
        "cm": {"meta/snapshot.cm": "Component {\n  uses: [a, b]\n}"},
        "cvf": {},
    }
    base.update(overrides)
    return base


class CompareTest(unittest.TestCase):
    def kinds(
        self, gn: dict[str, object], bazel: dict[str, object]
    ) -> list[tuple[str, str, str]]:
        return [(d.kind, d.key, d.item) for d in cu.compare_facts(gn, bazel)]

    def test_identical_packages_have_no_diffs(self) -> None:
        self.assertEqual(cu.compare_facts(facts(), facts()), [])

    def test_elf_merkle_differences_are_ignored(self) -> None:
        bazel = facts()
        bazel["blobs"]["bin/snapshot"]["merkle"] = "other"  # type: ignore[index]
        self.assertEqual(cu.compare_facts(facts(), bazel), [])

    def test_missing_and_extra_blobs(self) -> None:
        bazel = facts(
            blobs={
                "bin/snapshot": {"merkle": "a", "size": 1, "elf": dict(ELF)},
                "data/y": {"merkle": "e", "size": 1},
            }
        )
        self.assertEqual(
            self.kinds(facts(), bazel),
            [("missing_blob", "data/x", ""), ("extra_blob", "data/y", "")],
        )

    def test_data_blob_content(self) -> None:
        bazel = facts(
            blobs={
                "bin/snapshot": {"merkle": "a", "size": 1, "elf": dict(ELF)},
                "data/x": {"merkle": "z", "size": 1},
            }
        )
        self.assertEqual(
            self.kinds(facts(), bazel), [("blob_content", "data/x", "")]
        )

    def test_package_name_abi_and_config(self) -> None:
        bazel = facts(
            package_name="snapshot2", abi_revision="08", cvf={"meta/c.cvf": "x"}
        )
        gn = facts(cvf={"meta/c.cvf": "y"})
        self.assertEqual(
            self.kinds(gn, bazel),
            [
                ("package_name", "snapshot", ""),
                ("abi_revision", "meta/fuchsia.abi/abi-revision", ""),
                ("config", "meta/c.cvf", ""),
            ],
        )

    def test_cm_line_diff(self) -> None:
        bazel = facts(cm={"meta/snapshot.cm": "Component {\n  uses: [a]\n}"})
        diffs = cu.compare_facts(facts(), bazel)
        self.assertEqual(
            [(d.kind, d.item) for d in diffs],
            [("cm", "uses: [a, b]"), ("cm", "uses: [a]")],
        )

    def test_elf_facts_differences(self) -> None:
        elf = dict(
            ELF,
            needed=["libc.so"],
            stripped=False,
            text_size=120000,
            sections=ELF["sections"] + [".symtab"],
        )
        bazel = facts(
            blobs={
                "bin/snapshot": {"merkle": "b", "size": 1, "elf": elf},
                "data/x": {"merkle": "d", "size": 1},
            }
        )
        self.assertEqual(
            self.kinds(facts(), bazel),
            [
                ("elf_stripped", "bin/snapshot", ""),
                ("elf_needed", "bin/snapshot", "libzircon.so"),
                ("elf_sections", "bin/snapshot", ".symtab"),
                ("elf_text_size", "bin/snapshot", ""),
            ],
        )
        self.assertEqual(
            cu.compare_facts(facts(), bazel)[-1].bazel, "120000 (+20.0%)"
        )

    def test_section_stripped_binary(self) -> None:
        # GN strips section headers from executables; only compare what is left.
        gn_elf = dict(ELF, sections=[], has_sections=False, stripped=True)
        bazel_elf = dict(
            ELF, sections=ELF["sections"] + [".symtab"], stripped=False
        )
        gn = facts(
            blobs={
                "bin/snapshot": {"merkle": "a", "size": 1, "elf": gn_elf},
                "data/x": {"merkle": "d", "size": 1},
            }
        )
        bazel = facts(
            blobs={
                "bin/snapshot": {"merkle": "b", "size": 1, "elf": bazel_elf},
                "data/x": {"merkle": "d", "size": 1},
            }
        )
        self.assertEqual(cu.compare_facts(gn, bazel), [])

    def test_small_text_size_change_is_ignored(self) -> None:
        small = dict(ELF, text_size=46)
        gn = facts(
            blobs={
                "bin/snapshot": {"merkle": "a", "size": 1, "elf": small},
                "data/x": {"merkle": "d", "size": 1},
            }
        )
        bazel = facts(
            blobs={
                "bin/snapshot": {
                    "merkle": "b",
                    "size": 1,
                    "elf": dict(small, text_size=40),
                },
                "data/x": {"merkle": "d", "size": 1},
            }
        )
        self.assertEqual(cu.compare_facts(gn, bazel), [])

    def test_text_size_within_tolerance(self) -> None:
        elf = dict(ELF, text_size=105000)
        bazel = facts(
            blobs={
                "bin/snapshot": {"merkle": "b", "size": 1, "elf": elf},
                "data/x": {"merkle": "d", "size": 1},
            }
        )
        self.assertEqual(cu.compare_facts(facts(), bazel), [])


class PilotFixesTest(unittest.TestCase):
    def test_gn_package_state(self) -> None:
        hand = GN_BUILD
        mirror = '## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\nfuchsia_package("pkg") {\n  package_name = "snapshot-gn"\n}\n'
        self.assertEqual(cu.gn_package_state(hand, "pkg", "snapshot"), "hand")
        self.assertEqual(
            cu.gn_package_state(mirror, "pkg", "snapshot"), "mirror"
        )
        self.assertEqual(cu.gn_package_state("", "pkg", "snapshot"), "gone")
        bridge = (
            'bazel_fuchsia_package("pkg") {\n  package_name = "snapshot"\n}\n'
        )
        self.assertEqual(
            cu.gn_package_state(bridge, "pkg", "snapshot"), "bridge"
        )

    def test_gn_packages_from_a_local_template(self) -> None:
        text = (
            'template("netstack_package") {\n'
            "  fuchsia_package(target_name) {\n"
            '    forward_variables_from(invoker, [ "package_name" ])\n'
            "  }\n"
            "}\n"
            'template("standalone_test") {\n'
            '  _test_name = "src_lib_fuchsia_${target_name}"\n'
            '  _package_name = _test_name + "_package"\n'
            "  fuchsia_unittest_package(_package_name) {\n"
            "    package_name = _test_name\n"
            "  }\n"
            "  group(target_name) {\n"
            '    deps = [ ":${_package_name}" ]\n'
            "  }\n"
            "}\n"
            'template("conditional_package") {\n'
            "  if (defined(invoker.ctf) && invoker.ctf) {\n"
            "    fuchsia_test_package(target_name) {\n"
            "    }\n"
            "  }\n"
            "}\n"
            'template("other") {\n  group(target_name) {\n  }\n}\n'
            'netstack_package("package-debug") {\n'
            '  package_name = "netstack-debug"\n}\n'
            'netstack_package("package-debug-less-logs") {\n'
            '  package_name = "netstack-debug"\n}\n'
            'standalone_test("empty_test") {\n  type = "test"\n}\n'
            'conditional_package("maybe") {\n  ctf = false\n}\n'
            'other("x") {\n}\n'
        )
        self.assertEqual(
            cu.gn_packages(text),
            {
                "netstack-debug": ["package-debug", "package-debug-less-logs"],
                "src_lib_fuchsia_empty_test": [
                    "src_lib_fuchsia_empty_test_package"
                ],
            },
        )

    def test_gn_bridges(self) -> None:
        text = (
            'bazel_fuchsia_package("a") {\n}\n'
            'bazel_fuchsia_package("b") {\n  bazel_target = ":c"\n}\n'
        )
        self.assertEqual(cu.gn_bridges(text), {"a": "a", "b": "c"})

    def test_pair_gn_package(self) -> None:
        pair = cu.pair_gn_package
        self.assertEqual(pair("x", ["x", "y"], {}), ("x", []))
        self.assertEqual(pair("p", ["x", "y"], {"y": "p"}), ("y", []))
        self.assertEqual(pair("p", ["x"], {}), ("x", []))
        self.assertEqual(pair("p", ["x"], {}, {"x"}), ("", ["x"]))
        self.assertEqual(pair("p", ["x", "y"], {}, {"x"}), ("", ["x", "y"]))
        self.assertEqual(pair("p", ["x", "y"], {}), ("", ["x", "y"]))

    def test_gn_mirror_package_name_is_not_a_diff(self) -> None:
        self.assertEqual(
            cu.compare_facts(facts(package_name="snapshot-gn"), facts()), []
        )

    def test_dylib_rename_is_one_finding(self) -> None:
        gn_elf = dict(ELF, needed=["libc.so", "libvfs_rust.so"])
        bz_elf = dict(ELF, needed=["libc.so", "libvfs-1069634238.so"])
        gn = facts(
            blobs={
                "bin/x": {"merkle": "a", "size": 1, "elf": gn_elf},
                "lib/libvfs_rust.so": {
                    "merkle": "v",
                    "size": 1,
                    "elf": dict(ELF),
                },
            }
        )
        bz = facts(
            blobs={
                "bin/x": {"merkle": "b", "size": 1, "elf": bz_elf},
                "lib/libvfs-1069634238.so": {
                    "merkle": "w",
                    "size": 1,
                    "elf": dict(ELF),
                },
            }
        )
        diffs = cu.compare_facts(gn, bz)
        self.assertEqual(
            [(d.kind, d.key, d.bazel) for d in diffs],
            [
                (
                    "dylib_renamed",
                    "lib/libvfs_rust.so",
                    "lib/libvfs-1069634238.so",
                )
            ],
        )

    def test_dropped_library_is_still_reported_next_to_a_rename(self) -> None:
        gn_a = dict(ELF, needed=["libc.so", "libvfs_rust.so"])
        gn_b = dict(ELF, needed=["libc.so", "libvfs_rust.so"])
        bz_a = dict(ELF, needed=["libc.so"])  # bin/a dropped the library.
        bz_b = dict(ELF, needed=["libc.so", "libvfs-1069634238.so"])
        lib = {"merkle": "v", "size": 1, "elf": dict(ELF)}
        gn = facts(
            blobs={
                "bin/a": {"merkle": "a", "size": 1, "elf": gn_a},
                "bin/b": {"merkle": "b", "size": 1, "elf": gn_b},
                "lib/libvfs_rust.so": lib,
            }
        )
        bz = facts(
            blobs={
                "bin/a": {"merkle": "c", "size": 1, "elf": bz_a},
                "bin/b": {"merkle": "d", "size": 1, "elf": bz_b},
                "lib/libvfs-1069634238.so": lib,
            }
        )
        self.assertEqual(
            [(d.kind, d.key, d.item) for d in cu.compare_facts(gn, bz)],
            [
                ("elf_needed", "bin/a", "libvfs_rust.so"),
                ("dylib_renamed", "lib/libvfs_rust.so", ""),
            ],
        )

    def test_versioned_libraries_are_not_a_rename(self) -> None:
        self.assertNotEqual(
            cu._lib_stem("lib/libfoo_1.so"), cu._lib_stem("lib/libfoo_2.so")
        )

    def test_cm_diff_is_a_unified_diff_by_section(self) -> None:
        g = "Component {\n    uses: [\n        Protocol(a),\n        Protocol(b),\n    ],\n}"
        b = "Component {\n    uses: [\n        Protocol(a),\n    ],\n}"
        diffs = cu.compare_facts(
            facts(cm={"meta/x.cm": g}), facts(cm={"meta/x.cm": b})
        )
        (f,) = cu.findings_from_diffs(diffs, "s", "f", "p")
        lines = f["message"].splitlines()
        self.assertEqual(
            lines[3:7],
            ["```diff", "@@ uses @@", "-        Protocol(b),", "```"],
        )

    def test_prune_records(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            now = 1_000_000_000.0
            for name, ts, wd in (
                ("fresh", now, tmp),
                ("old", now - 15 * 86400, tmp),
                ("gone", now, tmp + "/nope"),
            ):
                with open(os.path.join(tmp, f"{name}.json"), "w") as f:
                    json.dump(
                        {
                            "schema_version": cu.RECORD_SCHEMA_VERSION,
                            "timestamp": ts,
                            "workdir": wd,
                        },
                        f,
                    )
            deleted = cu.prune_records(tmp, now)
            self.assertEqual(
                sorted(os.path.basename(p) for p in deleted),
                ["gone.json", "old.json"],
            )
            self.assertEqual(os.listdir(tmp), ["fresh.json"])

    def test_tools_from_tool_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            for name in ("cmc", "readelf"):
                path = os.path.join(tmp, name)
                with open(path, "w") as f:
                    f.write("")
                os.chmod(path, 0o755)
            cpu = (
                "arm64"
                if cu.platform.machine() in ("aarch64", "arm64")
                else "x64"
            )
            with open(os.path.join(tmp, "tool_paths.json"), "w") as f:
                json.dump(
                    [
                        {
                            "name": "cmc",
                            "os": "linux",
                            "cpu": cpu,
                            "path": "cmc",
                        },
                        {
                            "name": "llvm-readelf",
                            "os": "linux",
                            "cpu": cpu,
                            "path": "readelf",
                        },
                    ],
                    f,
                )
            tools = cu.Tools.find(tmp, tmp)
        self.assertEqual(
            (tools.cmc, tools.readelf),
            (os.path.join(tmp, "cmc"), os.path.join(tmp, "readelf")),
        )


class RecordHelpersTest(unittest.TestCase):
    def test_rekey_commands(self) -> None:
        commands = {"bin/x rustc": 1, "bin/x rustc arm64": 2}
        self.assertEqual(
            cu.rekey_commands(commands, "x64", "arm64"),
            {"bin/x rustc x64": 1, "bin/x rustc": 2},
        )
        self.assertEqual(cu.rekey_commands(commands, "x64", "x64"), commands)
        self.assertEqual(cu.rekey_commands(commands, "", "arm64"), commands)

    def test_blob_changes(self) -> None:
        old = facts(
            blobs={
                "bin/snapshot": {"merkle": "a" * 64, "size": 1},
                "data/x": {"merkle": "d", "size": 1},
            },
            meta={"meta/x": "0"},
        )
        new = facts(
            blobs={
                "bin/snapshot": {"merkle": "b" * 64, "size": 1},
                "data/new": {"merkle": "c", "size": 1},
            },
            meta={"meta/x": "1"},
            abi_revision="08",
        )
        self.assertEqual(
            cu.blob_changes(old, new),
            [
                "meta/fuchsia.abi/abi-revision changed",
                f"bin/snapshot {'a' * 16} -> {'b' * 16}",
                "data/new added",
                "data/x removed",
                "meta/x changed",
            ],
        )
        self.assertEqual(cu.blob_changes(old, old), [])
        self.assertEqual(
            cu.blob_changes(old, new, limit=1),
            ["meta/fuchsia.abi/abi-revision changed", "and 4 more"],
        )


class NormalizationTest(unittest.TestCase):
    ENTRY = {
        "id": "ignore_comment",
        "comparison": "package_contents",
        "match": {
            "category": "cutover_diff_elf_sections",
            "item": r"\.comment",
        },
        "reason": "r",
        "source": "s",
    }

    def test_apply(self) -> None:
        diffs = [
            cu.Diff(
                "elf_sections", "bin/x", "absent", "present", item=".comment"
            ),
            cu.Diff(
                "elf_sections", "bin/x", "absent", "present", item=".symtab"
            ),
        ]
        kept, applied = cu.apply_normalizations(diffs, [self.ENTRY])
        self.assertEqual([d.item for d in kept], [".symtab"])
        self.assertEqual(applied, {"ignore_comment": 1})

    def test_comparison_and_code_entries_do_not_match(self) -> None:
        d = cu.Diff(
            "elf_sections",
            "bin/x",
            "absent",
            "present",
            item=".comment",
            comparison="build_commands",
        )
        self.assertEqual(cu.apply_normalizations([d], [self.ENTRY])[1], {})
        code = dict(self.ENTRY, match={"code": "x.py"})
        self.assertFalse(
            cu.normalization_matches(
                code, cu.Diff("elf_sections", "k", "", "", item=".comment")
            )
        )

    def test_invalid_regex_never_matches(self) -> None:
        e = dict(
            self.ENTRY,
            match={"category": "cutover_diff_elf_sections", "item": "("},
        )
        d = cu.Diff("elf_sections", "k", "", "", item="(")
        self.assertEqual(cu.apply_normalizations([d], [e]), ([d], {}))

    def test_gn_and_bazel_match_anywhere(self) -> None:
        e = dict(
            self.ENTRY,
            match={"category": "cutover_diff_elf_interp", "bazel": "ld"},
        )
        self.assertTrue(
            cu.normalization_matches(
                e, cu.Diff("elf_interp", "k", "x", "lib/ld.so.1")
            )
        )


class FindingsTest(unittest.TestCase):
    def test_grouping_and_format(self) -> None:
        diffs = [
            cu.Diff(
                "elf_needed", "bin/x", "present", "absent", item="libfoo.so"
            ),
            cu.Diff(
                "elf_needed", "bin/x", "absent", "present", item="libbar.so"
            ),
            cu.Diff("missing_blob", "data/y", "present", "absent"),
        ]
        findings = cu.findings_from_diffs(
            diffs, "cutover_consistency", "src/x/BUILD.bazel", "x"
        )
        self.assertEqual(
            [f["category"] for f in findings],
            ["cutover_diff_elf_needed", "cutover_diff_missing_blob"],
        )
        msg = findings[0]["message"].splitlines()
        self.assertTrue(msg[0].startswith("[x:bin/x] "))
        self.assertEqual(msg[1], "GN: libfoo.so")
        self.assertEqual(msg[2], "Bazel: libbar.so")
        self.assertTrue(msg[3].startswith("Device impact: functional"))
        for f in findings:
            self.assertEqual(f["severity"], "INFO")
            self.assertEqual(f["file"], "src/x/BUILD.bazel")

    def test_every_kind_has_a_category(self) -> None:
        categories = cu.load_categories()
        diffs = {n: c for n, c in categories.items() if "_diff_" in n}
        self.assertEqual(
            sorted(diffs),
            sorted(f"cutover_diff_{k}" for k in cu.DIFF_KINDS),
        )
        for name, c in diffs.items():
            # Only the guard ships escalated: it needs a verdict.
            self.assertEqual(
                c["stage"], 1 if name == "cutover_diff_guard" else 0, name
            )
            self.assertTrue(
                c["device_impact"].startswith(("functional", "performance")),
                name,
            )
        others = {n: c for n, c in categories.items() if n not in diffs}
        self.assertIn("cutover_summary", others)
        for name, c in others.items():
            self.assertTrue(
                name.startswith("cutover_info_") or name == "cutover_summary",
                name,
            )
            self.assertEqual(c["stage"], 0, name)
            self.assertTrue(c["device_impact"].startswith("none:"), name)
        for kind, (summary, fix) in cu.DIFF_KINDS.items():
            self.assertTrue(summary and fix, kind)

    def test_stage_two_is_a_warning(self) -> None:
        staged = {
            "cutover_diff_missing_blob": {
                "stage": 2,
                "device_impact": "functional: x",
            }
        }
        with mock.patch.object(cu, "load_categories", return_value=staged):
            (f,) = cu.findings_from_diffs(
                [cu.Diff("missing_blob", "data/y", "present", "absent")],
                "s",
                "f",
                "p",
            )
        self.assertEqual(f["severity"], "WARNING")
        self.assertIn("Device impact: functional: x", f["message"])

    def test_anchor_sets_file_line_and_target(self) -> None:
        (f,) = cu.findings_from_diffs(
            [cu.Diff("missing_blob", "data/y", "present", "absent")],
            "s",
            "src/x/BUILD.bazel",
            "p",
            anchor=lambda d: ("src/x/BUILD.bazel", 12, "//src/x:pkg"),
        )
        self.assertEqual((f["file"], f["line"]), ("src/x/BUILD.bazel", 12))
        self.assertEqual(f["message"].splitlines()[1], "Target: //src/x:pkg")
        self.assertIn(
            "(package content comparison)", f["message"].splitlines()[0]
        )


class FingerprintTest(unittest.TestCase):
    def finding(self, *diffs: cu.Diff) -> dict[str, T.Any]:
        (f,) = cu.findings_from_diffs(list(diffs), "s", "f", "p")
        return f

    def test_diff_findings_have_a_content_fingerprint(self) -> None:
        a = cu.Diff("cmd_flags", "bin/x rustc", "present", "absent", item="-Cx")
        b = cu.Diff("cmd_flags", "bin/x rustc", "absent", "present", item="-Cy")
        fp = self.finding(a, b)["fingerprint"]
        self.assertRegex(fp, r"^[0-9a-f]{16}$")
        # Stable across runs and diff order.
        self.assertEqual(self.finding(b, a)["fingerprint"], fp)
        # A changed detail under the same key changes it.
        c = cu.Diff("cmd_flags", "bin/x rustc", "absent", "present", item="-Cz")
        changed = self.finding(a, b, c)
        self.assertEqual(
            changed["message"].splitlines()[0],
            self.finding(a, b)["message"].splitlines()[0],
        )
        self.assertNotEqual(changed["fingerprint"], fp)

    def test_out_dir_paths_are_not_part_of_the_fingerprint(self) -> None:
        def flag(out: str) -> cu.Diff:
            return cu.Diff(
                "cmd_flags",
                "bin/x cpp_link",
                "present",
                "absent",
                item=f"-L/work/fuchsia/{out}/x64-shared",
            )

        self.assertEqual(
            self.finding(flag("out/default"))["fingerprint"],
            self.finding(flag("out/fuchsia.x64-balanced"))["fingerprint"],
        )
        self.assertNotEqual(
            self.finding(flag("out/default"))["fingerprint"],
            self.finding(
                cu.Diff(
                    "cmd_flags",
                    "bin/x cpp_link",
                    "present",
                    "absent",
                    item="-L/work/fuchsia/out/default/arm64-shared",
                )
            )["fingerprint"],
        )

    def test_fingerprint_covers_values_the_message_truncates(self) -> None:
        def flags(n: int) -> list[cu.Diff]:
            return [
                cu.Diff(
                    "cmd_flags",
                    "bin/x rustc",
                    "absent",
                    "present",
                    item=f"-C{i}",
                )
                for i in range(n)
            ]

        a, b = self.finding(*flags(25)), self.finding(*flags(26))
        # The message shows 20 values per side, so it is the same...
        self.assertEqual(
            a["message"].split("(+")[0], b["message"].split("(+")[0]
        )
        # ...but the fingerprint covers every diff of the group.
        self.assertNotEqual(a["fingerprint"], b["fingerprint"])

    def test_cm_fingerprint_keeps_the_section(self) -> None:
        def moved(section: str) -> str:
            return self.finding(
                cu.Diff(
                    "cm",
                    "meta/x.cm",
                    "",
                    "protocol: fuchsia.Foo",
                    item="protocol: fuchsia.Foo",
                    detail="+    protocol: fuchsia.Foo",
                    section=section,
                )
            )["fingerprint"]

        self.assertNotEqual(moved("expose"), moved("offer"))
        self.assertEqual(moved("expose"), moved("expose"))

    def test_text_size_fingerprint_keeps_only_the_direction(self) -> None:
        def size(gn: int, bazel: int) -> str:
            return self.finding(
                cu.Diff("elf_text_size", "bin/x", str(gn), f"{bazel} (+x%)")
            )["fingerprint"]

        self.assertEqual(size(100000, 150000), size(100000, 160000))
        self.assertNotEqual(size(100000, 150000), size(100000, 50000))

    def test_unverified_fingerprint(self) -> None:
        fp = cu.unverified_fingerprint("snap", "cutover_info_not_compared", "x")
        self.assertEqual(
            fp,
            cu.unverified_fingerprint("snap", "cutover_info_not_compared", "x"),
        )
        self.assertNotEqual(
            fp,
            cu.unverified_fingerprint("snap", "cutover_info_not_compared", "y"),
        )


GN_BUILD = """import("//build/components.gni")
fuchsia_package("pkg") {
  package_name = "snapshot"  # comment
  deps = [ ":main" ]
}
executable("main") {}
"""

BAZEL_BUILD = """load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")
fx_package(
    name = "pkg",
    package_name = "snapshot",
    resources = [":main_packaged"],
)
"""


class DetectorTest(unittest.TestCase):
    def setUp(self) -> None:
        # Records must go to this test's repo, not to a Planter directory.
        self.env = mock.patch.dict(os.environ)
        self.env.start()
        os.environ.pop(cu.RECORD_DIR_ENV, None)
        self.addCleanup(self.env.stop)
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name
        self.git("init", "-q")
        self.write("src/snap/BUILD.gn", GN_BUILD)
        self.write(
            "bundles/assembly/BUILD.bazel",
            'x(\n    pkgs = [\n        "//bundles/assembly/bazel_inputs/src/snap:pkg",\n    ],\n)\n',
        )
        self.write(
            "bundles/assembly/bazel_inputs/src/snap/BUILD.gn",
            'export_fuchsia_package_to_bazel("pkg") {\n  package = "//src/snap:pkg"\n}\n',
        )
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

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def git(self, *args: str) -> None:
        subprocess.check_call(["git", "-C", self.dir] + list(args))

    def write(self, path: str, text: str) -> None:
        full = os.path.join(self.dir, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w") as f:
            f.write(text)

    def detect(self) -> list[cu.Cutover]:
        changed = cu.git(self.dir, ["diff", "--name-only", "HEAD"]).splitlines()
        changed += cu.git(
            self.dir, ["ls-files", "--others", "--exclude-standard"]
        ).splitlines()
        return cu.detect_cutovers(self.dir, "HEAD", changed)

    def test_no_cutover(self) -> None:
        self.write("src/other/BUILD.bazel", 'fx_package(name = "other")\n')
        self.assertEqual(self.detect(), [])

    def test_parallel_state(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        (c,) = self.detect()
        self.assertEqual(
            (c.package_name, c.gn_label, c.bazel_label, c.state),
            ("snapshot", "//src/snap:pkg", "//src/snap:pkg", "dual_build"),
        )
        self.assertEqual(c.triggers, ["bazel_package_added"])

    def test_final_state_with_assembly_switch(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        self.write("src/snap/BUILD.gn", 'executable("main") {}\n')
        self.write(
            "bundles/assembly/BUILD.bazel",
            'x(\n    pkgs = [\n        "//src/snap:pkg",\n    ],\n)\n',
        )
        (c,) = self.detect()
        self.assertEqual(c.state, "final")
        self.assertEqual(
            c.triggers, ["bazel_package_added", "assembly_label_replaced"]
        )

    def test_assembly_switch_to_existing_bazel_package(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "add bazel",
        )
        self.write(
            "bundles/assembly/BUILD.bazel",
            'x(\n    pkgs = [\n        "//src/snap:pkg",\n    ],\n)\n',
        )
        (c,) = self.detect()
        self.assertEqual(
            (c.gn_label, c.triggers, c.state),
            ("//src/snap:pkg", ["assembly_label_replaced"], "final"),
        )

    def test_unittest_package(self) -> None:
        self.write(
            "src/snap/BUILD.gn",
            GN_BUILD + 'fuchsia_unittest_package("t") {\n}\n',
        )
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
        (c,) = self.detect()
        self.assertEqual(
            (c.bazel_label, c.bazel_package_label),
            ("//src/snap:t", "//src/snap:t.package"),
        )

    def commit(self, msg: str) -> None:
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            msg,
        )

    def unpaired(self) -> list[tuple[str, str, str]]:
        changed = cu.git(self.dir, ["diff", "--name-only", "HEAD"]).splitlines()
        return [
            (u.package_name, u.gn_label, u.gn_state)
            for u in cu.unpaired_gn_packages(
                self.dir, "HEAD", changed, self.detect()
            )
        ]

    def test_driver_package_cutover(self) -> None:
        # Driver packages were invisible to the check (D2).
        self.write(
            "src/drv/BUILD.gn",
            'fuchsia_driver_package("pkg") {\n'
            '  package_name = "drv"\n'
            '  driver_components = [ ":c" ]\n}\n',
        )
        self.commit("drv")
        self.write(
            "src/drv/BUILD.bazel",
            'fx_package(name = "pkg", package_name = "drv")\n',
        )
        (c,) = self.detect()
        self.assertEqual((c.gn_label, c.state), ("//src/drv:pkg", "dual_build"))
        # Phase 2: the driver package becomes a bazel_driver_package bridge.
        self.write(
            "src/drv/BUILD.gn",
            'bazel_driver_package("pkg") {\n'
            '  bazel_target = ":pkg"\n'
            '  package_name = "drv"\n}\n',
        )
        (c,) = self.detect()
        self.assertEqual((c.gn_state, c.state), ("bridge", "final"))

    def test_rules_fuchsia_test_package_builds_its_inner_target(self) -> None:
        self.write(
            "src/t/BUILD.gn", 'fuchsia_test_package("t") {\n  deps = []\n}\n'
        )
        self.commit("t")
        self.write(
            "src/t/BUILD.bazel", 'fuchsia_unittest_package(name = "t")\n'
        )
        (c,) = self.detect()
        self.assertEqual(
            (c.bazel_label, c.bazel_package_label),
            ("//src/t:t", "//src/t:t_fuchsia_package"),
        )

    def test_rules_fuchsia_package_builds_its_inner_target(self) -> None:
        self.write(
            "src/snap/BUILD.bazel",
            'fuchsia_package(name = "pkg", package_name = "snapshot")\n',
        )
        (c,) = self.detect()
        self.assertEqual(
            c.bazel_package_label, "//src/snap:pkg_fuchsia_package"
        )

    def test_deleted_gn_package_without_a_pair_is_unpaired(self) -> None:
        # D1: a GN package deleted with no Bazel package detected for it.
        self.write("src/snap/BUILD.gn", 'executable("main") {}\n')
        self.assertEqual(self.detect(), [])
        self.assertEqual(
            self.unpaired(), [("snapshot", "//src/snap:pkg", "gone")]
        )

    def test_bridge_to_an_earlier_bazel_package_is_unpaired(self) -> None:
        # D1: the Bazel package landed earlier, so it is not "added".
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        self.commit("bazel")
        self.write(
            "src/snap/BUILD.gn",
            'bazel_fuchsia_package("pkg") {\n  package_name = "snapshot"\n}\n',
        )
        self.assertEqual(self.detect(), [])
        self.assertEqual(
            self.unpaired(), [("snapshot", "//src/snap:pkg", "bridge")]
        )

    def test_deleted_bazel2gn_mirror_is_not_unpaired(self) -> None:
        self.write(
            "src/snap/BUILD.gn",
            'executable("main") {}\n## BAZEL2GN SENTINEL\n' + GN_BUILD,
        )
        self.commit("mirror")
        self.write("src/snap/BUILD.gn", 'executable("main") {}\n')
        self.assertEqual(self.unpaired(), [])

    def test_paired_and_kept_gn_packages_are_not_unpaired(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        self.assertEqual(self.unpaired(), [])  # Parallel: the GN package stays.
        self.write("src/snap/BUILD.gn", 'executable("main") {}\n')
        (c,) = self.detect()
        self.assertEqual(c.gn_state, "gone")
        self.assertEqual(
            self.unpaired(), []
        )  # Final: paired with the Bazel one.

    def test_package_name_mismatch_is_not_a_cutover(self) -> None:
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "pkg", package_name = "other")\n',
        )
        self.assertEqual(self.detect(), [])

    def test_record_round_trip(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        (c,) = self.detect()
        path = cu.record_path(self.dir, c.package_name)
        self.assertTrue(
            path.endswith(
                os.path.join(".git", "planter-cutover", "snapshot.json")
            )
        )
        cu.write_record(
            path,
            c,
            facts(),
            [{"category": "cutover_diff_x"}],
            {"n": 1},
            "abc",
            package_cpu="arm64",
        )
        record = cu.read_record(path)
        assert record is not None
        self.assertEqual(record["bazel_meta_far_merkle"], "m")
        self.assertEqual(record["normalizations_applied"], {"n": 1})
        self.assertEqual(record["change_base"], "abc")
        self.assertEqual(record["package_cpu"], "arm64")
        self.assertNotIn("gn_facts", record)
        self.assertIsNone(cu.read_record(path + ".missing"))

    def test_record_v2_keeps_gn_facts(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        (c,) = self.detect()
        path = cu.record_path(self.dir, c.package_name)
        gn = dict(facts(), sources={"bin/x": "/out/x"}, commands={"k": {}})
        cu.write_record(
            path, c, facts(), [], {}, "abc", gn_facts=gn, build_cpu="x64"
        )
        record = cu.read_record(path)
        assert record is not None
        self.assertEqual(record["schema_version"], 2)
        self.assertEqual(record["build_cpu"], "x64")
        self.assertEqual(record["gn_facts"]["commands"], {"k": {}})
        self.assertNotIn("sources", record["gn_facts"])
        self.assertEqual(record["gn_facts"]["blobs"], facts()["blobs"])
        # The Bazel blobs keep only what says which files changed.
        self.assertEqual(
            record["bazel_facts"]["blobs"]["bin/snapshot"],
            {"merkle": "a", "size": 1},
        )

    def test_read_record_versions(self) -> None:
        path = os.path.join(self.dir, "r.json")
        for version, readable in ((1, True), (2, True), (3, False)):
            with open(path, "w") as f:
                json.dump({"schema_version": version}, f)
            with self.subTest(version=version):
                self.assertEqual(cu.read_record(path) is not None, readable)
        with open(path, "w") as f:
            f.write("[]")
        self.assertIsNone(cu.read_record(path))

    def test_harness_records_dir(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        (c,) = self.detect()
        records = os.path.join(self.dir, "ledger", "cutover")
        with mock.patch.dict(os.environ, {cu.RECORD_DIR_ENV: records}):
            path = cu.record_path(self.dir, "snapshot")
            self.assertEqual(path, os.path.join(records, "snapshot.json"))
            self.assertTrue(
                cu.record_path(self.dir, "snapshot", local=True).endswith(
                    os.path.join(".git", "planter-cutover", "snapshot.json")
                )
            )
            cu.write_record(path, c, facts(), [], {}, "abc")
            self.assertIsNotNone(cu.read_record(path))
            # Writes outside the configured directory are still refused.
            other = os.path.join(self.dir, "elsewhere", "snapshot.json")
            with self.assertRaises(ValueError):
                cu.write_record(other, c, facts(), [], {}, "abc")
        self.assertFalse(os.path.exists(os.path.join(self.dir, "elsewhere")))

    def test_record_path_keys_by_target(self) -> None:
        self.assertTrue(
            cu.record_path(
                self.dir, "netstack-debug", "package-debug"
            ).endswith(
                os.path.join(
                    "planter-cutover", "netstack-debug--package-debug.json"
                )
            )
        )
        for target in ("../x", "a/b", ".", ".."):
            with self.subTest(target=target), self.assertRaises(ValueError):
                cu.record_path(self.dir, "netstack-debug", target)

    def test_shared_package_name_pairs_by_target(self) -> None:
        # netstack3: two GN packages build `netstack-debug`.
        gn = (
            'fuchsia_package("package-debug") {\n'
            '  package_name = "netstack-debug"\n}\n'
            'fuchsia_package("package-debug-less-logs") {\n'
            '  package_name = "netstack-debug"\n}\n'
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
            'fx_package(name = "package-debug", package_name = "netstack-debug")\n'
            'fx_package(name = "package-debug-less-logs", package_name = "netstack-debug")\n',
        )
        a, b = self.detect()
        self.assertEqual(
            [(c.gn_label, c.bazel_label, c.key) for c in (a, b)],
            [
                (
                    "//src/snap:package-debug",
                    "//src/snap:package-debug",
                    "netstack-debug--package-debug",
                ),
                (
                    "//src/snap:package-debug-less-logs",
                    "//src/snap:package-debug-less-logs",
                    "netstack-debug--package-debug-less-logs",
                ),
            ],
        )
        # A Bazel package with another target name can't be paired, even when
        # a sibling Bazel package claims one of the GN candidates.
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "package-debug", package_name = "netstack-debug")\n'
            'fx_package(name = "pkg", package_name = "netstack-debug")\n',
        )
        matched, unpaired = self.detect()
        self.assertEqual(matched.gn_label, "//src/snap:package-debug")
        self.assertEqual(
            (unpaired.gn_state, unpaired.state, unpaired.gn_label),
            ("ambiguous", "ambiguous", ""),
        )
        self.assertSequenceEqual(
            unpaired.ambiguous,
            ("//src/snap:package-debug", "//src/snap:package-debug-less-logs"),
        )
        # A bridge replacing one of them in place pairs it.
        self.write(
            "src/snap/BUILD.bazel",
            'fx_package(name = "pkg", package_name = "netstack-debug")\n',
        )
        self.write(
            "src/snap/BUILD.gn",
            'bazel_fuchsia_package("package-debug") {\n  bazel_target = ":pkg"\n}\n'
            'fuchsia_package("package-debug-less-logs") {\n'
            '  package_name = "netstack-debug"\n}\n',
        )
        (c,) = self.detect()
        self.assertEqual(
            (c.gn_label, c.gn_state, c.state),
            ("//src/snap:package-debug", "bridge", "final"),
        )

    def test_unrolled_standalone_test_template_is_a_cutover(self) -> None:
        self.write(
            "src/snap/BUILD.gn",
            'template("standalone_test") {\n'
            '  _test_name = "src_lib_fuchsia_${target_name}"\n'
            '  _package_name = _test_name + "_package"\n'
            "  fuchsia_unittest_package(_package_name) {\n"
            "    package_name = _test_name\n"
            "  }\n"
            "  group(target_name) {\n"
            '    deps = [ ":${_package_name}" ]\n'
            "  }\n"
            "}\n"
            'standalone_test("empty_test") {\n  type = "test"\n}\n',
        )
        self.git("add", "-A")
        self.git(
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "tmpl",
        )
        self.write(
            "src/snap/BUILD.bazel",
            'fx_unittest_package(name = "src_lib_fuchsia_empty_test")\n',
        )
        (parallel,) = self.detect()
        self.assertEqual(
            (parallel.gn_label, parallel.gn_state),
            ("//src/snap:src_lib_fuchsia_empty_test_package", "hand"),
        )
        self.write("src/snap/BUILD.gn", "")
        (final,) = self.detect()
        self.assertEqual(
            (final.gn_label, final.gn_state, final.state),
            ("//src/snap:src_lib_fuchsia_empty_test_package", "gone", "final"),
        )

    def test_record_path_rejects_unsafe_package_names(self) -> None:
        for name in (
            "/tmp/evil",
            "../evil",
            "a/b",
            ".",
            "..",
            "",
            "A",
            "x" * 256,
        ):
            with self.subTest(name=name), self.assertRaises(ValueError):
                cu.record_path(self.dir, name)
        self.assertTrue(
            cu.record_path(self.dir, "my-pkg_1.0").endswith("my-pkg_1.0.json")
        )

    def test_write_record_stays_in_the_records_dir(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        (c,) = self.detect()
        records = os.path.dirname(cu.record_path(self.dir, "x"))
        outside = os.path.join(self.dir, "outside.json")
        for path in (outside, os.path.join(records, "..", "evil.json")):
            with self.subTest(path=path), self.assertRaises(ValueError):
                cu.write_record(path, c, facts(), [], {}, "abc")
        self.assertFalse(os.path.exists(outside))
        # A record path that is a symlink out of the records dir is refused.
        os.makedirs(records, exist_ok=True)
        os.symlink(outside, os.path.join(records, "snapshot.json"))
        with self.assertRaises(ValueError):
            cu.record_path(self.dir, "snapshot")
        self.assertFalse(os.path.exists(outside))

    def test_write_record_refuses_planted_symlinks(self) -> None:
        self.write("src/snap/BUILD.bazel", BAZEL_BUILD)
        (c,) = self.detect()
        path = cu.record_path(self.dir, "snapshot")
        records = os.path.dirname(path)
        manifest = os.path.join(self.dir, "package_manifest.json")
        with open(manifest, "w") as f:
            f.write("{}")
        outside = os.path.join(self.dir, "outside.txt")
        bazel_dir = os.path.join(records, "snapshot.bazel")

        def fresh_bazel_dir() -> None:
            shutil.rmtree(bazel_dir, ignore_errors=True)
            os.makedirs(bazel_dir)

        for planted in (
            path + ".tmp",
            os.path.join(bazel_dir, "meta_far_listing.txt"),
        ):
            with self.subTest(planted=planted):
                fresh_bazel_dir()
                os.symlink(outside, planted)
                with self.assertRaises(ValueError):
                    cu.write_record(
                        path, c, facts(), [], {}, "abc", bazel_manifest=manifest
                    )
                self.assertFalse(os.path.exists(outside))
                os.unlink(planted)
        # A symlink that stays inside is replaced, not followed.
        fresh_bazel_dir()
        os.symlink(
            os.path.join(records, "other.txt"),
            os.path.join(records, "snapshot.bazel", "package_manifest.json"),
        )
        cu.write_record(
            path, c, facts(), [], {}, "abc", bazel_manifest=manifest
        )
        self.assertFalse(os.path.exists(os.path.join(records, "other.txt")))
        self.assertIsNotNone(cu.read_record(path))


class GnParsingTest(unittest.TestCase):
    def test_gn_packages(self) -> None:
        text = (
            GN_BUILD + 'fuchsia_unittest_package("tests") {\n  deps = []\n}\n'
        )
        self.assertEqual(
            cu.gn_packages(text), {"snapshot": ["pkg"], "tests": ["tests"]}
        )

    def test_driver_fuzzer_and_ctf_packages(self) -> None:
        text = (
            'fuchsia_driver_package("d") {\n  package_name = "drv"\n}\n'
            'fuchsia_fuzzer_package("f") {\n  cpp_fuzzers = []\n}\n'
            'ctf_fuchsia_package("c") {\n  package_name = "ctf"\n}\n'
        )
        self.assertEqual(
            cu.gn_packages(text), {"drv": ["d"], "f": ["f"], "ctf": ["c"]}
        )

    def test_gn_bridge_templates(self) -> None:
        text = (
            'bazel_driver_package("d") {\n  bazel_target = "//x:drv"\n}\n'
            'bazel_fuchsia_test_package("t") {\n  test_package_name = "t"\n}\n'
        )
        self.assertEqual(cu.gn_bridges(text), {"d": "drv", "t": "t"})

    def test_resolve_label(self) -> None:
        self.assertEqual(cu.resolve_label(":x", "a/b"), "//a/b:x")
        self.assertEqual(cu.resolve_label("//a/b", "c"), "//a/b:b")
        self.assertEqual(cu.resolve_label("@//a:y", "c"), "//a:y")


if __name__ == "__main__":
    unittest.main()
