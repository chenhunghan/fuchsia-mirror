# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for verify_stack_sizes."""

import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any

_SCRIPT_DIR = Path(__file__).resolve().parents[1]
if str(_SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPT_DIR))

import verify_stack_sizes
from verify_stack_sizes import Policy


def _symbol(
    name: str,
    value: int,
    binding: str = "Global",
    sym_type: str = "Function",
    section: int = 9,
) -> dict[str, Any]:
    """Returns a symbol in llvm-readelf JSON format."""
    return {
        "Symbol": {
            "Name": {"Name": name, "Value": 1},
            "Value": value,
            "Size": 16,
            "Binding": {"Name": binding, "Value": 1},
            "Type": {"Name": sym_type, "Value": 2},
            "Other": {"Value": 0, "Flags": []},
            "Section": {
                "Name": "Undefined" if section == 0 else ".text",
                "Value": section,
            },
        }
    }


def _func(
    names: list[str], address: int, callees: list[int] = []
) -> dict[str, Any]:
    """Returns a CallGraph entry in llvm-readelf JSON format."""
    return {
        "Function": {
            "Names": names,
            "Address": address,
            "Version": 0,
            "IsIndirectTarget": True,
            "TypeID": 0x1234,
            "DirectCallees": [{"Address": c} for c in callees],
            "IndirectTypeIDs": [],
        }
    }


def _stack_size(names: list[str], addr: int, size: int) -> dict[str, Any]:
    """Returns a StackSizes entry in llvm-readelf JSON format."""
    return {"Entry": {"Functions": names, "Address": addr, "Size": size}}


# A shared library like the vDSO: exported C++ functions with two names
# each, a hidden C++ alternate for one of them, and assembly entry points
# and helpers, which declare their call graph information and (empty)
# frames like the vDSO's syscall stubs do.
#
#   _zx_wait (0x1000, 32) -> SYSCALL_zx_wait (0x3000, assembly, 0)
#   _zx_time (0x1010, 16) -> helper (0x2000, 48)
#   CODE_time_via_kernel (0x1020, 24) -> SYSCALL_zx_wait
#   _zx_leaf (0x1030, 8)
#   _zx_create (0x3010, assembly, 0, also CODE_SYSCALL_zx_create)
_ZX_CREATE_NAMES = [
    "SYSCALL_zx_create",
    "CODE_SYSCALL_zx_create",
    "_zx_create",
    "zx_create",
]

READELF_DATA: dict[str, Any] = {
    "FileSummary": {"File": "libzircon.so", "Arch": "aarch64"},
    "DynamicSymbols": [
        _symbol("", 0, "Local", "None", 0),
        _symbol("_zx_wait", 0x1000),
        _symbol("zx_wait", 0x1000, "Weak"),
        _symbol("_zx_time", 0x1010),
        _symbol("zx_time", 0x1010, "Weak"),
        _symbol("_zx_leaf", 0x1030),
        _symbol("zx_leaf", 0x1030, "Weak"),
        _symbol("_zx_create", 0x3010),
        _symbol("zx_create", 0x3010, "Weak"),
        _symbol("external", 0, section=0),
        _symbol("data_object", 0x5000, sym_type="Object"),
    ],
    "Symbols": [
        _symbol("", 0, "Local", "None", 0),
        _symbol("_zx_wait", 0x1000),
        _symbol("zx_wait", 0x1000, "Weak"),
        _symbol("VDSO_zx_wait", 0x1000, "Local"),
        _symbol("_zx_time", 0x1010),
        _symbol("zx_time", 0x1010, "Weak"),
        _symbol("CODE_time_via_kernel", 0x1020, "Local"),
        _symbol("_zx_leaf", 0x1030),
        _symbol("zx_leaf", 0x1030, "Weak"),
        _symbol("helper", 0x2000, "Local"),
        _symbol("SYSCALL_zx_wait", 0x3000, "Local"),
        _symbol("_zx_create", 0x3010),
        _symbol("zx_create", 0x3010, "Weak"),
        _symbol("SYSCALL_zx_create", 0x3010, "Local"),
        _symbol("CODE_SYSCALL_zx_create", 0x3010, "Local"),
        # Not a function: never an entry point.
        _symbol("CODE_SYSRET_label", 0x3020, "Local", "None"),
    ],
    "StackSizes": [
        _stack_size(["VDSO_zx_wait", "_zx_wait", "zx_wait"], 0x1000, 32),
        _stack_size(["_zx_time", "zx_time"], 0x1010, 16),
        _stack_size(["CODE_time_via_kernel"], 0x1020, 24),
        _stack_size(["_zx_leaf", "zx_leaf"], 0x1030, 8),
        _stack_size(["helper"], 0x2000, 48),
        _stack_size(["SYSCALL_zx_wait"], 0x3000, 0),
        _stack_size(_ZX_CREATE_NAMES, 0x3010, 0),
    ],
    "CallGraph": [
        _func(["VDSO_zx_wait", "_zx_wait", "zx_wait"], 0x1000, [0x3000]),
        _func(["_zx_time", "zx_time"], 0x1010, [0x2000]),
        _func(["CODE_time_via_kernel"], 0x1020, [0x3000]),
        _func(["_zx_leaf", "zx_leaf"], 0x1030),
        _func(["helper"], 0x2000),
        _func(["SYSCALL_zx_wait"], 0x3000),
        _func(_ZX_CREATE_NAMES, 0x3010),
    ],
}

# The same library with assembly that doesn't emit call graph information
# or stack sizes.
READELF_DATA_WITHOUT_ASSEMBLY_INFO: dict[str, Any] = dict(
    READELF_DATA,
    StackSizes=READELF_DATA["StackSizes"][:-2],
    CallGraph=READELF_DATA["CallGraph"][:-2],
)


def _policy(max_stack_usage: int = 64, **kwargs: Any) -> Policy:
    """A policy like the vDSO's: hidden CODE_* alternates are entry points."""
    kwargs.setdefault("entry_point_patterns", ["CODE_*"])
    return Policy(max_stack_usage, **kwargs)


def _check(
    policy: Policy, data: dict[str, Any] = READELF_DATA
) -> tuple[list[str], dict[str, dict[str, Any]]]:
    """Checks data against policy; the entries are keyed by entry point."""
    errors, report = verify_stack_sizes.check_stack_sizes(data, policy)
    return errors, {
        entry["entry_function"]: entry for entry in report["entry_functions"]
    }


class TestCheckStackSizes(unittest.TestCase):
    def test_pass(self) -> None:
        errors, report = _check(_policy())
        self.assertEqual(errors, [])
        # Each exported address is one entry point, under its exported name,
        # however many names it has. CODE_* functions are entry points too.
        self.assertEqual(
            sorted(report),
            [
                "CODE_time_via_kernel",
                "_zx_create",
                "_zx_leaf",
                "_zx_time",
                "_zx_wait",
            ],
        )
        wait = report["_zx_wait"]
        self.assertEqual(wait["address"], 0x1000)
        self.assertEqual(wait["names"], ["VDSO_zx_wait", "_zx_wait", "zx_wait"])
        self.assertEqual(wait["limit"], 64)
        self.assertEqual(wait["max_stack_usage"], 32)
        self.assertEqual(wait["stack_usage_expression"], "32")
        self.assertEqual(
            wait["max_stack_path"],
            [
                {"name": "VDSO_zx_wait", "address": 0x1000, "stack": 32},
                {"name": "SYSCALL_zx_wait", "address": 0x3000, "stack": 0},
            ],
        )
        self.assertFalse(wait["is_lower_bound"])
        self.assertEqual(wait["lower_bound_reasons"], [])
        time = report["_zx_time"]
        self.assertEqual(time["max_stack_usage"], 64)
        self.assertEqual(time["stack_usage_expression"], "64")
        self.assertFalse(time["is_lower_bound"])
        self.assertEqual(time["lower_bound_reasons"], [])
        self.assertEqual(report["CODE_time_via_kernel"]["max_stack_usage"], 24)
        # An exported function is named by an exported name even if it also
        # matches an --entry-point pattern.
        create = report["_zx_create"]
        self.assertEqual(create["address"], 0x3010)
        self.assertEqual(create["names"], sorted(_ZX_CREATE_NAMES))
        self.assertEqual(create["limit"], 64)
        self.assertEqual(create["max_stack_usage"], 0)
        self.assertFalse(create["is_lower_bound"])

    def test_exceeds(self) -> None:
        errors, report = _check(_policy(63))
        self.assertEqual(len(errors), 1)
        self.assertIn("_zx_time: max stack usage 64 exceeds 63", errors[0])
        self.assertIn("_zx_time", errors[0])
        self.assertIn("helper", errors[0])
        self.assertEqual(report["_zx_time"]["limit"], 63)

    def test_call_frame_overhead(self) -> None:
        data = dict(READELF_DATA, FileSummary={"Arch": "x86_64"})
        errors, report = _check(_policy(64), data)
        # 8 bytes of return address per frame on x86-64, and the 128-byte
        # red zone for the functions that make no calls: helper, _zx_leaf
        # and the assembly stubs.
        self.assertEqual(report["_zx_wait"]["max_stack_usage"], 40 + 136)
        self.assertEqual(report["_zx_time"]["max_stack_usage"], 24 + 184)
        self.assertEqual(report["_zx_leaf"]["max_stack_usage"], 144)
        self.assertEqual(report["_zx_create"]["max_stack_usage"], 136)
        self.assertEqual(len(errors), 5)
        for error in errors:
            self.assertIn("per call", error)

    def test_limit(self) -> None:
        # A limit applies under any of the function's names.
        errors, report = _check(_policy(limits={"zx_time": 63}))
        self.assertEqual(len(errors), 1)
        self.assertIn("_zx_time: max stack usage 64 exceeds 63", errors[0])
        self.assertEqual(report["_zx_time"]["limit"], 63)
        self.assertEqual(report["_zx_wait"]["limit"], 64)
        # A looser limit is allowed too.
        errors, report = _check(_policy(63, limits={"_zx_time": 64}))
        self.assertEqual(errors, [])
        self.assertEqual(report["_zx_time"]["limit"], 64)
        # Limits under two names of one function must agree.
        errors, _ = _check(_policy(limits={"_zx_time": 63, "zx_time": 64}))
        self.assertIn(
            "_zx_time has conflicting limits: _zx_time=63, zx_time=64", errors
        )

    def test_leaf(self) -> None:
        errors, _ = _check(_policy(leaves={"zx_leaf"}))
        self.assertEqual(errors, [])
        errors, _ = _check(_policy(leaves={"zx_time", "_zx_wait"}))
        self.assertEqual(
            errors,
            [
                "_zx_wait must be a leaf, but calls SYSCALL_zx_wait",
                "_zx_time must be a leaf, but calls helper",
            ],
        )

    def test_intent_for_unknown_function(self) -> None:
        errors, _ = _check(_policy(limits={"nothing": 8}, leaves={"helper"}))
        self.assertEqual(
            errors,
            ["helper is not an entry point", "nothing is not an entry point"],
        )

    def test_no_call_graph_info(self) -> None:
        # Missing call graph information is an error, for an entry point
        # and for a callee, whose caller's estimate is then a lower bound.
        errors, report = _check(
            _policy(limits={"_zx_create": 0}),
            READELF_DATA_WITHOUT_ASSEMBLY_INFO,
        )
        self.assertEqual(
            errors,
            [
                "_zx_wait: the max stack usage 32 is only a lower bound: "
                "SYSCALL_zx_wait has no call graph information "
                "(called by VDSO_zx_wait)",
                "CODE_time_via_kernel: the max stack usage 24 is only a "
                "lower bound: SYSCALL_zx_wait has no call graph information "
                "(called by CODE_time_via_kernel)",
                "_zx_create has no call graph information",
            ],
        )
        self.assertTrue(report["_zx_wait"]["is_lower_bound"])
        self.assertNotIn("max_stack_usage", report["_zx_create"])

    def test_external_callee(self) -> None:
        # A call to a function outside the file (callee address 0) is a
        # call to a function without call graph information.
        call_graph = list(READELF_DATA["CallGraph"])
        call_graph[3] = _func(["_zx_leaf", "zx_leaf"], 0x1030, [0])
        errors, report = _check(
            _policy(leaves={"zx_leaf"}),
            dict(READELF_DATA, CallGraph=call_graph),
        )
        self.assertEqual(
            errors,
            [
                "_zx_leaf: the max stack usage 8 is only a lower bound: "
                "<external> has no call graph information (called by _zx_leaf)",
                "_zx_leaf must be a leaf, but calls <external>",
            ],
        )
        self.assertTrue(report["_zx_leaf"]["is_lower_bound"])

    def test_exclude_functions(self) -> None:
        # An excluded function is asserted unreachable: calls to it are
        # ignored, so its caller's estimate shrinks and is not a lower
        # bound. By name or by address.
        for excluded in ("helper", {"address": 0x2000}):
            errors, report = _check(_policy(63, exclude_functions=[excluded]))
            self.assertEqual(errors, [])
            time = report["_zx_time"]
            self.assertEqual(time["max_stack_usage"], 16)
            self.assertFalse(time["is_lower_bound"])
            self.assertEqual(
                time["max_stack_path"],
                [{"name": "_zx_time", "address": 0x1010, "stack": 16}],
            )
        # An entry point cannot be excluded: that is an error, and the entry
        # point is left out of the report rather than analyzed anyway.
        errors, report = _check(_policy(exclude_functions=["zx_time"]))
        self.assertEqual(
            errors, ["_zx_time is an entry point, so it cannot be excluded"]
        )
        self.assertNotIn("_zx_time", report)

    def test_policy_from_config(self) -> None:
        policy = Policy.from_config(
            {
                "max_stack_usage": 512,
                "entry_points": ["CODE_*"],
                "limits": {"zx_time": 64},
                "leaves": ["zx_leaf"],
                "exclude_functions": ["panic", {"address": 0x2000}],
            }
        )
        self.assertEqual(policy.max_stack_usage, 512)
        self.assertEqual(policy.entry_point_patterns, ["CODE_*"])
        self.assertEqual(policy.limits, {"zx_time": 64})
        self.assertEqual(policy.leaves, {"zx_leaf"})
        self.assertEqual(
            policy.exclude_functions, ["panic", {"address": 0x2000}]
        )
        self.assertEqual(Policy.from_config({"max_stack_usage": 8}), Policy(8))
        # Everything is optional: an empty policy enforces nothing.
        self.assertEqual(Policy.from_config({}), Policy())
        self.assertIsNone(Policy().max_stack_usage)
        # A malformed policy is an error that names the problem.
        for config, problem in (
            (
                {"max_stack_usage": "512"},
                "policy.max_stack_usage: expected int",
            ),
            (
                {"max_stack_usage": 8, "entry_points": "CODE_*"},
                "policy.entry_points: expected a list",
            ),
            (
                {"max_stack_usage": 8, "limits": {"f": "8"}},
                "policy.limits.f: expected int",
            ),
            (
                {"max_stack_usage": 8, "exclude_functions": "panic"},
                "policy.exclude_functions: expected list",
            ),
            ([], "policy: expected an object"),
        ):
            with self.assertRaises(ValueError) as raised:
                Policy.from_config(config)
            self.assertEqual(str(raised.exception), problem)

    def test_linked_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            rspfile = Path(tmp) / "link.rsp"
            rspfile.write_text("obj/libzircon.so\n")
            self.assertEqual(
                verify_stack_sizes.linked_file(rspfile),
                Path("obj/libzircon.so"),
            )
            # Naming no file or several is an error that says how many.
            for contents, count in (("", 0), ("a.so\nb.so\n", 2)):
                rspfile.write_text(contents)
                with self.assertRaises(ValueError) as raised:
                    verify_stack_sizes.linked_file(rspfile)
                self.assertEqual(
                    str(raised.exception),
                    f"{rspfile} must name exactly one linked file, "
                    f"instead found {count}",
                )

    def test_error_report(self) -> None:
        errors, _ = _check(_policy(63))
        text = verify_stack_sizes.format_error_report(
            Path("libzircon.so"),
            errors,
            _policy(63),
            ["{{{reset}}}", "{{{module:0:libzircon.so:elf:abcd}}}"],
            Path("report.json"),
        )
        lines = text.split("\n")
        # The markup context comes first, then one line per error, with
        # the frames of the path that exceeds the budget, then the advice.
        self.assertEqual(lines[0], "{{{reset}}}")
        self.assertEqual(lines[1], "{{{module:0:libzircon.so:elf:abcd}}}")
        self.assertEqual(
            lines[2],
            "libzircon.so: 1 stack size check error(s) (see report.json):",
        )
        self.assertEqual(
            lines[3], "  _zx_time: max stack usage 64 exceeds 63 (frames):"
        )
        self.assertEqual(lines[4], "        16 _zx_time {{{pc:0x1010}}}")
        self.assertEqual(lines[5], "        48 helper {{{pc:0x2000}}}")
        self.assertTrue(lines[6].startswith("The budget is 63 bytes"))
        # By hand, without a report file, and without a budget, e.g. for a
        # leaf violation: no report to see, no budget to raise.
        errors, _ = _check(Policy(leaves={"zx_time"}))
        text = verify_stack_sizes.format_error_report(
            Path("libzircon.so"), errors, Policy(leaves={"zx_time"}), [], None
        )
        self.assertEqual(
            text.split("\n"),
            [
                "libzircon.so: 1 stack size check error(s):",
                "  _zx_time must be a leaf, but calls helper",
            ],
        )

    def test_without_budget(self) -> None:
        # Without a budget nothing is enforced, not even call graph
        # information for every entry point: the report just states what
        # is known, and the entries have no limit.
        errors, report = _check(Policy(), READELF_DATA_WITHOUT_ASSEMBLY_INFO)
        self.assertEqual(errors, [])
        wait = report["_zx_wait"]
        self.assertNotIn("limit", wait)
        self.assertTrue(wait["is_lower_bound"])
        self.assertEqual(
            wait["lower_bound_reasons"],
            [
                "SYSCALL_zx_wait has no call graph information "
                "(called by VDSO_zx_wait)"
            ],
        )
        self.assertNotIn("max_stack_usage", report["_zx_create"])
        # A limit for one entry point is a budget for it alone.
        errors, report = _check(
            Policy(limits={"zx_wait": 8}), READELF_DATA_WITHOUT_ASSEMBLY_INFO
        )
        self.assertEqual(len(errors), 2)
        self.assertIn("_zx_wait: max stack usage 32 exceeds 8", errors[0])
        self.assertIn("_zx_wait: the max stack usage 32 is only", errors[1])
        self.assertEqual(report["_zx_wait"]["limit"], 8)
        self.assertNotIn("limit", report["_zx_time"])
        # A leaf requirement cannot be checked without call graph
        # information, so that is an error even without a budget.
        errors, _ = _check(
            Policy(leaves={"zx_create", "zx_leaf"}),
            READELF_DATA_WITHOUT_ASSEMBLY_INFO,
        )
        self.assertEqual(errors, ["_zx_create has no call graph information"])

    def test_elf_entry_point(self) -> None:
        # Like the startup dynamic linker: no exported functions, entered
        # through the ELF header entry point (_start), calling external
        # vDSO functions (callee address 0).
        data = {
            "FileSummary": {"File": "libld-startup.so", "Arch": "x86_64"},
            "ElfHeader": {"Entry": 0x7000},
            "DynamicSymbols": [_symbol("zx_vmo_read", 0, section=0)],
            "Symbols": [
                _symbol("_start", 0x7000, "Local"),
                _symbol("StartLd", 0x7100, "Local"),
            ],
            "StackSizes": [
                _stack_size(["_start"], 0x7000, 24),
                _stack_size(["StartLd"], 0x7100, 600),
            ],
            "CallGraph": [
                _func(["_start"], 0x7000, [0x7100]),
                _func(["StartLd"], 0x7100, [0]),
            ],
        }
        errors, report = verify_stack_sizes.check_stack_sizes(data, Policy())
        self.assertEqual(errors, [])
        self.assertEqual(report["call_frame_overhead"], 8)
        [entry] = report["entry_functions"]
        self.assertEqual(entry["entry_function"], "_start")
        self.assertEqual(entry["address"], 0x7000)
        self.assertEqual(entry["names"], ["_start"])
        self.assertNotIn("limit", entry)
        # _start (24+8) + StartLd (600+8), and whatever the external
        # function uses; the expression counts the return address of that
        # call too.
        self.assertEqual(entry["max_stack_usage"], 640)
        self.assertEqual(
            entry["stack_usage_expression"], "648 + calls(<external>)"
        )
        self.assertTrue(entry["is_lower_bound"])
        self.assertEqual(
            entry["lower_bound_reasons"],
            ["<external> has no call graph information (called by StartLd)"],
        )
        # With a budget, the entry point is checked like any other.
        errors, _ = verify_stack_sizes.check_stack_sizes(data, Policy(1024))
        self.assertEqual(
            errors,
            [
                "_start: the max stack usage 640 is only a lower bound: "
                "<external> has no call graph information (called by StartLd)"
            ],
        )
        # The entry point is named by its address if it has no symbol.
        data = dict(data, Symbols=[])
        errors, report = verify_stack_sizes.check_stack_sizes(data, Policy())
        self.assertEqual(errors, [])
        [entry] = report["entry_functions"]
        self.assertEqual(entry["entry_function"], "0x7000")
        self.assertEqual(entry["address"], 0x7000)

    def test_text_report(self) -> None:
        errors, report = verify_stack_sizes.check_stack_sizes(
            dict(READELF_DATA, FileSummary={"Arch": "x86_64"}),
            Policy(limits={"zx_wait": 8}),
        )
        text = verify_stack_sizes.format_text_report(report, ["{{{reset}}}"])
        lines = text.split("\n")
        # The markup context comes first, then each entry point with its
        # stack usage and the frames of its max stack path.
        self.assertEqual(
            lines[:6],
            [
                "{{{reset}}}",
                "_zx_wait (0x1000):",
                "  stack usage: 176",
                "  max stack path (+ 8 bytes per call):",
                "        32 VDSO_zx_wait {{{pc:0x1000}}}",
                "       128 SYSCALL_zx_wait {{{pc:0x3000}}}",
            ],
        )
        self.assertIn("_zx_time (0x1010):", lines)
        # The errors are separate: the text report states what is known.
        self.assertEqual(len(errors), 1)
        self.assertNotIn("exceeds", text)
        # Without call graph information or with a lower bound, it says so.
        _, report = verify_stack_sizes.check_stack_sizes(
            READELF_DATA_WITHOUT_ASSEMBLY_INFO, Policy()
        )
        text = verify_stack_sizes.format_text_report(report, [])
        self.assertIn("_zx_create (0x3010): no call graph information", text)
        self.assertIn(
            "_zx_wait (0x1000):\n"
            "  stack usage: 32 + calls(SYSCALL_zx_wait)\n"
            "  known stack usage: 32 bytes (lower bound)\n"
            "  reason: SYSCALL_zx_wait has no call graph information "
            "(called by VDSO_zx_wait)\n"
            "  max stack path:\n"
            "        32 VDSO_zx_wait {{{pc:0x1000}}}",
            text,
        )

    def test_entry_point_patterns(self) -> None:
        errors, report = _check(_policy(entry_point_patterns=[]))
        self.assertEqual(errors, [])
        self.assertNotIn("CODE_time_via_kernel", report)
        # Only functions match.
        errors, report = _check(_policy(entry_point_patterns=["*SYSRET*"]))
        self.assertEqual(errors, [])
        self.assertNotIn("CODE_SYSRET_label", report)


if __name__ == "__main__":
    unittest.main()
