# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for stack_estimator."""

import argparse
import io
import json
import pathlib
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import Any
from unittest.mock import patch

import call_graph
import graph
import stack_estimator

# This will be set by main() from the command line arguments.
TESTDATA_DIR: pathlib.Path


class TestGraphHelper(unittest.TestCase):
    """Tests for graph.Graph."""

    def test_scc(self) -> None:
        g = graph.Graph()
        # 0 -> 1 -> 2 -> 0 (Cycle)
        # 2 -> 3
        g.add_edge(0, 1)
        g.add_edge(1, 2)
        g.add_edge(2, 0)
        g.add_edge(2, 3)

        sccs = g.get_sccs()
        # Should have {0, 1, 2} and {3}
        self.assertEqual(len(sccs), 2)
        scc_sets = [set(scc) for scc in sccs]
        self.assertIn({0, 1, 2}, scc_sets)
        self.assertIn({3}, scc_sets)

    def test_topological_sort(self) -> None:
        g = graph.Graph()
        # 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3
        g.add_edge(0, 1)
        g.add_edge(0, 2)
        g.add_edge(1, 3)
        g.add_edge(2, 3)

        order = g.topological_sort()
        self.assertEqual(len(order), 4)
        self.assertEqual(order[0], 0)
        self.assertEqual(order[3], 3)
        # 1 and 2 can be in any order in between
        self.assertSetEqual(set(order[1:3]), {1, 2})

    def test_self_loop_is_cycle(self) -> None:
        g = graph.Graph()
        g.add_edge(0, 0)
        g.add_edge(0, 1)
        self.assertTrue(g.is_cycle([0]))
        self.assertFalse(g.is_cycle([1]))

    def test_deep_chain(self) -> None:
        # Deeper than Python's default recursion limit.
        g = graph.Graph()
        n = 20000
        for i in range(n):
            g.add_edge(i, i + 1)
        self.assertEqual(len(g.get_sccs()), n + 1)
        self.assertEqual(g.topological_sort(), list(range(n + 1)))
        g.add_edge(n, 0)
        self.assertEqual(len(g.get_sccs()), 1)


class TestCallGraph(unittest.TestCase):
    """Tests for call_graph.py."""

    def test_parse_stack_sizes(self) -> None:
        with open(TESTDATA_DIR / "test_stack_sizes.json", "r") as f:
            data = call_graph.single_file(json.load(f))
        sizes = call_graph.parse_stack_sizes(data)
        self.assertEqual(sizes, {6032: 16, 6048: 32, 6064: 8, 6080: 64})

    def test_stack_size_without_address(self) -> None:
        # Only a linked file has an address for each entry.
        data = {"StackSizes": [{"Entry": {"Functions": ["f"], "Size": 8}}]}
        with self.assertRaisesRegex(ValueError, "no address"):
            call_graph.parse_stack_sizes(data)

    def test_extract_type_id_mapping(self) -> None:
        with open(TESTDATA_DIR / "test_indirect.json", "r") as f:
            data = call_graph.single_file(json.load(f))
        mapping = call_graph.extract_type_id_mapping(data)
        self.assertIn(12345, mapping)
        self.assertCountEqual(mapping[12345], [2000, 3000])

    def test_build_call_graph(self) -> None:
        with open(TESTDATA_DIR / "test_call_graph.json", "r") as f:
            cg_data = json.load(f)
        with open(TESTDATA_DIR / "test_stack_sizes.json", "r") as f:
            ss_data = call_graph.single_file(json.load(f))

        stack_sizes = call_graph.parse_stack_sizes(ss_data)
        cg, _, func_info, _ = call_graph.build_call_graph(cg_data, stack_sizes)

        self.assertIn(6080, cg.vertices)  # main
        self.assertCountEqual(cg.graph[6080], [6032, 6048, 6064])
        self.assertEqual(func_info[6080]["stack_usage"], 64)

    def test_get_display_names(self) -> None:
        data = {
            "CallGraph": [
                {
                    "Function": {
                        "Names": ["sym_c"],
                        "Address": 0x3000,
                        "DirectCallees": [
                            {"Names": ["uninst_d"], "Address": 0x4000},
                            {"Address": 0},
                        ],
                    }
                }
            ],
        }
        names = call_graph.get_display_names(data)
        self.assertEqual(names, {0x3000: ["sym_c"], 0x4000: ["uninst_d"]})

    def test_get_elf_entry_point(self) -> None:
        self.assertIsNone(call_graph.get_elf_entry_point({}))
        self.assertIsNone(
            call_graph.get_elf_entry_point({"ElfHeader": {"Entry": 0}})
        )
        self.assertEqual(
            call_graph.get_elf_entry_point({"ElfHeader": {"Entry": 0x7000}}),
            0x7000,
        )

    def test_get_dynamic_function_symbols(self) -> None:
        def symbol(name: str, value: int, type_name: str, shndx: int) -> Any:
            return {
                "Symbol": {
                    "Name": {"Name": name},
                    "Value": value,
                    "Type": {"Name": type_name},
                    "Section": {"Value": shndx},
                }
            }

        data = {
            "DynamicSymbols": [
                symbol("exported", 0x1000, "Function", 12),
                symbol("alias", 0x1000, "Function", 12),
                symbol("weak_export", 0x2000, "Function", 12),
                # Undefined: a reference to another module.
                symbol("imported", 0, "Function", 0),
                symbol("variable", 0x3000, "Object", 14),
            ],
            # Only the dynamic symbol table counts.
            "Symbols": [symbol("local_func", 0x4000, "Function", 12)],
        }
        self.assertEqual(
            call_graph.function_symbols(data["DynamicSymbols"]),
            {0x1000: {"exported", "alias"}, 0x2000: {"weak_export"}},
        )
        self.assertEqual(
            call_graph.get_dynamic_function_symbols(data), {0x1000, 0x2000}
        )
        self.assertEqual(call_graph.get_dynamic_function_symbols({}), set())

    def test_uninstrumented_callees_in_adj_list(self) -> None:
        data = {
            "CallGraph": [
                {
                    "Function": {
                        "Names": ["caller"],
                        "Address": 0x1000,
                        "DirectCallees": [
                            {"Names": ["ext_func"], "Address": 0x9000}
                        ],
                    }
                }
            ]
        }
        _, _, _, adj_list = call_graph.build_call_graph(data, {0x1000: 32})
        self.assertEqual(adj_list[0x1000]["uninstrumented"], [0x9000])

    def test_stack_sizes_by_address(self) -> None:
        # The address of an entry tells functions with the same name apart.
        data = {
            "CallGraph": [
                _func(0x10, "helper"),
                _func(0x20, "helper"),
                # Clang omits the stack size of functions with
                # dynamically sized stack objects.
                _func(0x30, "vla"),
            ],
            "StackSizes": [
                _stack_size("helper", 72, addr=0x10),
                _stack_size("helper", 24, addr=0x20),
            ],
        }
        _, _, func_info, _ = call_graph.build_call_graph(
            data, call_graph.parse_stack_sizes(data)
        )
        self.assertEqual(func_info[0x10]["stack_usage"], 72)
        self.assertEqual(func_info[0x20]["stack_usage"], 24)
        self.assertIsNone(func_info[0x30]["stack_usage"])

    def test_stack_sizes_by_address_ignores_names(self) -> None:
        data = {
            "CallGraph": [_func(0x10, "f"), _func(0x20, "g")],
            "StackSizes": [
                _stack_size("f", 8, addr=0x10),
                # An entry named for g, but at another address: the
                # address is authoritative.
                _stack_size("g", 64, addr=0x30),
            ],
        }
        _, _, func_info, _ = call_graph.build_call_graph(
            data, call_graph.parse_stack_sizes(data)
        )
        self.assertEqual(func_info[0x10]["stack_usage"], 8)
        self.assertIsNone(func_info[0x20]["stack_usage"])

    def test_stack_sizes_folded_functions(self) -> None:
        # Identical code folding leaves the entries of the folded functions
        # at one address. They describe identical code, so the sizes should
        # agree; the largest is used in any case.
        data = {
            "CallGraph": [_func(0x10, "f")],
            "StackSizes": [
                _stack_size("f", 16, addr=0x10),
                _stack_size("f_copy", 16, addr=0x10),
            ],
        }
        _, _, func_info, _ = call_graph.build_call_graph(
            data, call_graph.parse_stack_sizes(data)
        )
        self.assertEqual(func_info[0x10]["stack_usage"], 16)

    def test_excluded_callee_is_not_uninstrumented(self) -> None:
        data = {"CallGraph": [_func(0x10, "f", [0x20]), _func(0x20, "panic")]}
        cg, _, _, adj_list = call_graph.build_call_graph(
            data, {}, {"exclude_functions": ["panic"]}
        )
        self.assertNotIn(0x20, cg.vertices)
        self.assertEqual(adj_list[0x10]["direct"], [])
        self.assertEqual(adj_list[0x10]["uninstrumented"], [])

    def test_excluded_by_address(self) -> None:
        # Two local functions share a name; an address excludes one of them,
        # whether or not the name is given too. An address also excludes a
        # callee that has no call graph record.
        data = {
            "CallGraph": [
                _func(0x10, "f", [0x20, 0x30, 0x40]),
                _func(0x20, "helper"),
                _func(0x30, "helper"),
            ]
        }
        for excluded in (
            {"address": 0x30},
            {"name": "helper", "address": 0x30},
        ):
            cg, _, _, adj_list = call_graph.build_call_graph(
                data, {}, {"exclude_functions": [excluded, {"address": 0x40}]}
            )
            self.assertIn(0x20, cg.vertices)
            self.assertNotIn(0x30, cg.vertices)
            self.assertEqual(adj_list[0x10]["direct"], [0x20])
            self.assertEqual(adj_list[0x10]["uninstrumented"], [])

    def test_single_file(self) -> None:
        file_entry = {"CallGraph": [_func(0x10, "f")]}
        self.assertIs(call_graph.single_file([file_entry]), file_entry)
        # The addresses of different files are unrelated.
        for output in ([file_entry] * 2, [], file_entry):
            with self.assertRaises(ValueError):
                call_graph.single_file(output)

    def test_single_file_checks_the_shape(self) -> None:
        # Parseable JSON that is not shaped like llvm-readelf output is an
        # error that names the problem, not a traceback. A key may be absent.
        self.assertEqual(call_graph.single_file([{}]), {})
        for file_entry, problem in (
            ([], "llvm-readelf output: expected an object"),
            (
                {"CallGraph": {}},
                "llvm-readelf output.CallGraph: expected a list",
            ),
            (
                {"CallGraph": [{"Function": {"Address": "0x10"}}]},
                "llvm-readelf output.CallGraph[0].Function.Address: "
                "expected int",
            ),
            (
                {"CallGraph": [{"Function": {"Names": "f"}}]},
                "llvm-readelf output.CallGraph[0].Function.Names: "
                "expected a list",
            ),
            (
                {"CallGraph": [{"Function": {"TypeID": True}}]},
                "llvm-readelf output.CallGraph[0].Function.TypeID: "
                "expected int",
            ),
            (
                {"StackSizes": [{"Entry": {"Size": None}}]},
                "llvm-readelf output.StackSizes[0].Entry.Size: expected int",
            ),
            (
                {"ElfHeader": {"Entry": "main"}},
                "llvm-readelf output.ElfHeader.Entry: expected int",
            ),
        ):
            with self.assertRaises(ValueError) as raised:
                call_graph.single_file([file_entry])
            self.assertEqual(str(raised.exception), problem)

    def test_unmatched_and_untyped_indirect_calls(self) -> None:
        data = {
            "CallGraph": [
                _func(0x10, "f", indirect=[0x1111, 0]),
                _func(0x20, "g", indirect=[0x2222], type_id=0x2222),
            ]
        }
        _, _, _, adj_list = call_graph.build_call_graph(data, {})
        # No function has type 0x1111, and type ID 0 is unknown.
        self.assertEqual(adj_list[0x10]["unmatched_indirect"], [0x1111])
        self.assertEqual(adj_list[0x10]["untyped_indirect"], 1)
        self.assertEqual(adj_list[0x20]["unmatched_indirect"], [])
        self.assertEqual(adj_list[0x20]["untyped_indirect"], 0)
        self.assertEqual(
            adj_list[0x20]["indirect"], [{"typeId": 0x2222, "targets": [0x20]}]
        )

    def test_dead_unmatched_indirect_calls(self) -> None:
        # An unmatched indirect call that the configuration declares dead
        # counts as no call; an untyped one is unknown regardless.
        data = {
            "CallGraph": [
                _func(0x10, "f", indirect=[0x1111, 0]),
                _func(0x20, "g", indirect=[0x1111]),
            ]
        }
        for config, dead in (
            ({"dead_unmatched_indirect_calls": True}, {0x10, 0x20}),
            ({"dead_unmatched_indirect_calls": ["g"]}, {0x20}),
            ({"dead_unmatched_indirect_calls": [{"address": 0x10}]}, {0x10}),
            ({"dead_unmatched_indirect_calls": []}, set()),
            ({}, set()),
        ):
            _, _, _, adj_list = call_graph.build_call_graph(data, {}, config)
            for addr in (0x10, 0x20):
                self.assertEqual(
                    adj_list[addr]["unmatched_indirect"],
                    [] if addr in dead else [0x1111],
                    config,
                )
            self.assertEqual(adj_list[0x10]["untyped_indirect"], 1)

    def test_find_unknown_type_targets(self) -> None:
        data = {
            "CallGraph": [
                _func(0x10, "typed", type_id=0x1111, indirect_target=True),
                _func(0x20, "untyped", indirect_target=True),
                _func(0x30, "local"),
            ]
        }
        self.assertEqual(call_graph.find_unknown_type_targets(data), [0x20])

    def test_relocatable_object_file(self) -> None:
        # The ELF header says what the file is, when it is in the data.
        for header, linked in (
            ({"Type": "Relocatable (0x1)"}, False),
            ({"Type": "Executable (0x2)"}, True),
            ({"Type": "SharedObject (0x3)"}, True),
            ({}, True),
        ):
            data = {"ElfHeader": header, "CallGraph": [_func(0x10, "f")]}
            if linked:
                call_graph.build_call_graph(data, {})
            else:
                with self.assertRaisesRegex(ValueError, "relocatable"):
                    call_graph.build_call_graph(data, {})
        self.assertEqual(
            call_graph.elf_type({"ElfHeader": {"Type": "Core (0x4)"}}), 4
        )
        self.assertIsNone(call_graph.elf_type({}))

        # Without the header: in a relocatable object file, llvm-readelf
        # identifies a function by the relocation applied to its address
        # field rather than an address.
        relocation = {"SymbolIndex": 2, "SymbolName": "f"}
        data = {
            "CallGraph": [
                {
                    "Function": {
                        "Names": ["f"],
                        "Relocation": relocation,
                        "Version": 0,
                        "IsIndirectTarget": True,
                        "TypeID": 0x1111,
                        "DirectCallees": [],
                        "IndirectTypeIDs": [],
                    }
                }
            ]
        }
        with self.assertRaisesRegex(ValueError, "relocatable"):
            call_graph.build_call_graph(data, {})
        # A callee can be identified by a relocation while the function
        # itself has an address, and older llvm-readelf printed a Name.
        data = {
            "CallGraph": [
                _func(0x10, "f"),
                {
                    "Function": {
                        "Names": ["g"],
                        "Address": 0x20,
                        "DirectCallees": [{"Name": "f"}],
                    }
                },
            ]
        }
        with self.assertRaisesRegex(ValueError, "relocatable"):
            call_graph.build_call_graph(data, {})


class TestStackEstimator(unittest.TestCase):
    """Tests for stack_estimator.py."""

    def test_calculate_max_stack_usage_dag(self) -> None:
        """Tests calculation of max stack usage for a Directed Acyclic Graph."""
        # A -> B (10), A -> C (20)
        # B -> D (5), C -> D (5)
        # A: 100, B: 50, C: 60, D: 10
        cg = graph.Graph()
        cg.add_edge(1, 2)
        cg.add_edge(1, 3)
        cg.add_edge(2, 4)
        cg.add_edge(3, 4)

        func_info = {
            1: {"stack_usage": 100, "label": "A"},
            2: {"stack_usage": 50, "label": "B"},
            3: {"stack_usage": 60, "label": "C"},
            4: {"stack_usage": 10, "label": "D"},
        }
        sccs = [[1], [2], [3], [4]]

        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        # Max path: A -> C -> D = 100 + 60 + 10 = 170
        # SCC IDs might vary, find SCC for 1
        a_scc_id = next(i for i, scc in enumerate(sccs) if 1 in scc)
        self.assertEqual(results[a_scc_id]["max_stack_usage"], 170)

    def test_calculate_max_stack_usage_cycle(self) -> None:
        """Tests calculation of max stack usage when there are cycles (SCCs)."""
        # A -> B -> A (Cycle)
        # B -> C
        # A: 10, B: 20, C: 30
        cg = graph.Graph()
        cg.add_edge(1, 2)
        cg.add_edge(2, 1)
        cg.add_edge(2, 3)

        func_info = {
            1: {"stack_usage": 10, "label": "A"},
            2: {"stack_usage": 20, "label": "B"},
            3: {"stack_usage": 30, "label": "C"},
        }
        # Tarjan should give [[3], [1, 2]] or similar
        sccs = cg.get_sccs()

        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        # SCC {A, B} total = 10 + 20 = 30
        # SCC {C} total = 30
        # Max path from {A, B}: 30 + 30 = 60
        ab_scc_id = next(i for i, scc in enumerate(sccs) if 1 in scc)
        self.assertEqual(results[ab_scc_id]["max_stack_usage"], 60)

    def test_indirect_calls(self) -> None:
        with open(TESTDATA_DIR / "test_indirect.json", "r") as f:
            data = call_graph.single_file(json.load(f))

        stack_sizes = call_graph.parse_stack_sizes(data)
        cg, name_to_addr, func_info, _ = call_graph.build_call_graph(
            data, stack_sizes
        )

        sccs = cg.get_sccs()
        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        # caller (10) calls target1 (20) and target2 (30) indirectly
        # Max stack should be 10 + max(20, 30) = 40
        caller_addr = name_to_addr["caller"]
        caller_scc_id = next(
            i for i, scc in enumerate(sccs) if caller_addr in scc
        )
        self.assertEqual(results[caller_scc_id]["max_stack_usage"], 40)

    def test_end_to_end_combined(self) -> None:
        with open(TESTDATA_DIR / "test_combined.json", "r") as f:
            data = call_graph.single_file(json.load(f))

        stack_sizes = call_graph.parse_stack_sizes(data)
        cg, name_to_addr, func_info, _ = call_graph.build_call_graph(
            data, stack_sizes
        )

        self.assertIn("main", name_to_addr)
        entry_addr = name_to_addr["main"]

        sccs = cg.get_sccs()
        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        main_scc_id = next(i for i, scc in enumerate(sccs) if entry_addr in scc)
        max_stack = results[main_scc_id]["max_stack_usage"]

        # main (64) calls foo (16), bar (32), baz (8)
        # Total = 64 + 32 = 96
        self.assertEqual(max_stack, 96)


class TestStackEstimatorEndToEnd(unittest.TestCase):
    """End-to-End tests simulating CLI usage."""

    def test_multiple_entry_functions(self) -> None:
        """Tests that the script handles multiple entry functions."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            config_path = Path(tmp_dir) / "test_config.json"
            # Create a temporary config file referencing the existing test data
            config_data = {"entry_functions": ["main", "foo"]}
            with open(config_path, "w") as f:
                json.dump(config_data, f)

            callgraph_path = TESTDATA_DIR / "test_combined.json"

            # Capture stdout from a mock run
            # Using patch to redirect stdout and simulate sys.argv
            with patch.object(sys, "stdout", new=io.StringIO()) as fake_stdout:
                with patch.object(
                    sys,
                    "argv",
                    [
                        "stack_estimator.py",
                        str(config_path),
                        str(callgraph_path),
                    ],
                ):
                    stack_estimator.main()

            output_str = fake_stdout.getvalue()
            output_json = json.loads(output_str)

            entry_funcs = output_json["entry_functions"]
            self.assertEqual(len(entry_funcs), 2)

            main_result = next(
                r for r in entry_funcs if r["entry_function"] == "main"
            )
            self.assertEqual(main_result["max_stack_usage"], 96)

            foo_result = next(
                r for r in entry_funcs if r["entry_function"] == "foo"
            )
            self.assertEqual(foo_result["max_stack_usage"], 16)
            self.assertEqual(main_result["address"], 6080)

    def test_main_reports_errors(self) -> None:
        """Tests that invalid input issues a diagnostic instead of an exception."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            config_path = Path(tmp_dir) / "config.json"
            config_path.write_text("{}")
            bad_json = Path(tmp_dir) / "bad.json"
            bad_json.write_text("{")
            for callgraph in [bad_json, Path(tmp_dir) / "missing.json"]:
                with patch.object(sys, "stderr", new=io.StringIO()) as err:
                    with patch.object(
                        sys,
                        "argv",
                        [
                            "stack_estimator.py",
                            str(config_path),
                            str(callgraph),
                        ],
                    ):
                        self.assertEqual(stack_estimator.main(), 1)
                self.assertIn(callgraph.name, err.getvalue())

    def test_dict_entry_functions_and_uninstrumented(self) -> None:
        """Tests dictionary entry_functions and uninstrumented callee reporting."""
        config_data = {
            "entry_functions": [{"name": "caller", "address": 0x1000}]
        }
        cg_data = {
            "StackSizes": [_stack_size("caller", 48, addr=0x1000)],
            "CallGraph": [
                {
                    "Function": {
                        "Names": ["caller"],
                        "Address": 0x1000,
                        "DirectCallees": [
                            {"Names": ["ext_callee"], "Address": 0x9000}
                        ],
                    }
                }
            ],
        }

        res = stack_estimator.estimate(config_data, cg_data)
        entry_res = res["entry_functions"][0]
        self.assertEqual(entry_res["entry_function"], "caller")
        self.assertEqual(entry_res["address"], 0x1000)
        self.assertEqual(entry_res["max_stack_usage"], 48)
        self.assertTrue(entry_res["is_lower_bound"])
        self.assertEqual(
            entry_res["uninstrumented_callees"],
            [
                {
                    "name": "ext_callee",
                    "address": 0x9000,
                    "callers": [0x1000],
                }
            ],
        )
        self.assertEqual(res["nodes"][0x1000]["uninstrumented"], [0x9000])

    def test_external_stack_usage(self) -> None:
        """Tests the budget for calls out of the file (callee address 0)."""
        # entry (16) calls either f (32), which calls out of the file, or
        # g (40), a leaf. Without a budget the external call is an unknown;
        # with one it is a frame of that size, and the deeper path wins or
        # loses on the number.
        records = [
            _func(0x1000, "entry", callees=[0x1010, 0x1020]),
            _func(0x1010, "f", callees=[0]),
            _func(0x1020, "g"),
        ]
        sizes = [("entry", 16), ("f", 32), ("g", 40)]
        config = {"entry_functions": ["entry"]}

        res = self.estimate(config, records, sizes)
        entry = res["entry_functions"][0]
        self.assertEqual(entry["max_stack_usage"], 56)
        self.assertEqual(
            entry["stack_usage_expression"], "max(56, 48 + calls(<external>))"
        )
        self.assertTrue(entry["is_lower_bound"])
        self.assertNotIn("external_callers", entry)
        self.assertNotIn("external_stack_usage", res)

        res = self.estimate(
            dict(config, external_stack_usage=512), records, sizes
        )
        entry = res["entry_functions"][0]
        self.assertEqual(entry["max_stack_usage"], 16 + 32 + 512)
        self.assertEqual(entry["stack_usage_expression"], "560")
        self.assertFalse(entry["is_lower_bound"])
        self.assertEqual(entry["uninstrumented_callees"], [])
        self.assertEqual(entry["external_callers"], [0x1010])
        self.assertEqual(res["external_stack_usage"], 512)
        # The path ends with the call out of the file, as a frame of the
        # budget, which has no address.
        self.assertEqual(
            entry["max_stack_path"],
            [
                {"address": 0x1000, "name": "entry", "stack": 16},
                {"address": 0x1010, "name": "f", "stack": 32},
                {"address": 0, "name": "<external>", "stack": 512},
            ],
        )
        text = stack_estimator.format_text_report(res)
        self.assertIn(
            "  calls out of the file from 1 function(s), 512 bytes each as "
            "configured",
            text,
        )
        self.assertIn("\n        512  <external>\n", text + "\n")

        # A small budget leaves the leaf path as the maximum, and the budget
        # counts the return address the call pushes, so none is added.
        res = self.estimate(
            dict(config, external_stack_usage=4), records, sizes, "x86_64"
        )
        entry = res["entry_functions"][0]
        # entry 16+8, g 40+8+128 (red zone) = 200, versus f 32+8 + 4 = 68.
        self.assertEqual(entry["max_stack_usage"], 200)
        self.assertEqual(entry["stack_usage_expression"], "200")
        self.assertEqual(entry["max_stack_path"][-1]["name"], "g")
        self.assertFalse(entry["is_lower_bound"])

        with self.assertRaisesRegex(ValueError, "external_stack_usage"):
            self.estimate(
                dict(config, external_stack_usage="512"), records, sizes
            )

    def estimate(
        self,
        config: dict[str, Any],
        records: list[Any],
        stack_sizes: Any,
        arch: str = "",
    ) -> dict[str, Any]:
        return stack_estimator.estimate(
            config,
            {
                "FileSummary": {"Arch": arch},
                "CallGraph": records,
                "StackSizes": _stack_sizes(records, stack_sizes),
            },
        )

    def test_recursion_below_entry(self) -> None:
        # main -> a <-> b -> c, and main -> rec -> rec.
        res = self.estimate(
            {"entry_functions": ["main", "c"]},
            [
                _func(1, "main", [2, 5]),
                _func(2, "a", [3]),
                _func(3, "b", [2, 4]),
                _func(4, "c"),
                _func(5, "rec", [5]),
            ],
            [("main", 5), ("a", 10), ("b", 20), ("c", 30), ("rec", 8)],
        )
        main, c = res["entry_functions"]
        self.assertEqual(main["max_stack_usage"], 65)
        self.assertTrue(main["has_cycles"])
        self.assertTrue(main["is_lower_bound"])
        self.assertEqual(len(main["cycles"]), 2)
        self.assertFalse(c["has_cycles"])
        self.assertFalse(c["is_lower_bound"])
        cycle_nodes = sorted(sorted(c["nodes"]) for c in res["cycles"])
        self.assertEqual(cycle_nodes, [[2, 3], [5]])
        # main (5) -> {a, b} (10 + 20) -> c (30). Both functions of the
        # recursive SCC are on the path.
        path = main["max_stack_path"]
        self.assertEqual([f["address"] for f in path[:1]], [1])
        self.assertEqual(sorted(f["address"] for f in path[1:3]), [2, 3])
        self.assertEqual(path[3:], [{"address": 4, "name": "c", "stack": 30}])
        self.assertEqual(sum(f["stack"] for f in path), 65)
        self.assertEqual(
            c["max_stack_path"], [{"address": 4, "name": "c", "stack": 30}]
        )

    def test_unknown_stack_size_is_lower_bound(self) -> None:
        res = self.estimate(
            {"entry_functions": ["main"]},
            [_func(1, "main", [2]), _func(2, "vla")],
            [("main", 16)],
        )
        (main,) = res["entry_functions"]
        self.assertEqual(main["max_stack_usage"], 16)
        self.assertTrue(main["is_lower_bound"])
        self.assertEqual(main["unknown_stack_sizes"], [2])
        self.assertIsNone(res["nodes"][2]["stack"])

    def test_unmatched_indirect_call_is_lower_bound(self) -> None:
        res = self.estimate(
            {"entry_functions": ["main"]},
            [_func(1, "main", indirect=[0x1111])],
            [("main", 16)],
        )
        (main,) = res["entry_functions"]
        self.assertTrue(main["is_lower_bound"])
        self.assertEqual(
            main["unmatched_indirect_calls"], [{"caller": 1, "type_id": 0x1111}]
        )
        self.assertEqual(main["untyped_indirect_calls"], [])

        # Declared dead, the call makes no difference.
        res = self.estimate(
            {
                "entry_functions": ["main"],
                "dead_unmatched_indirect_calls": True,
            },
            [_func(1, "main", indirect=[0x1111])],
            [("main", 16)],
        )
        (main,) = res["entry_functions"]
        self.assertFalse(main["is_lower_bound"])
        self.assertEqual(main["unmatched_indirect_calls"], [])

    def test_untyped_indirect_call_is_lower_bound(self) -> None:
        res = self.estimate(
            {
                "entry_functions": ["main"],
                "dead_unmatched_indirect_calls": True,
            },
            [_func(1, "main", [2]), _func(2, "f", indirect=[0])],
            [("main", 16), ("f", 8)],
        )
        (main,) = res["entry_functions"]
        self.assertTrue(main["is_lower_bound"])
        self.assertEqual(main["unmatched_indirect_calls"], [])
        self.assertEqual(main["untyped_indirect_calls"], [2])
        self.assertEqual(res["nodes"][2]["untyped_indirect"], 1)

    def test_unknown_type_targets_are_lower_bound(self) -> None:
        res = self.estimate(
            {"entry_functions": ["main"]},
            [
                _func(1, "main", indirect=[0x1111]),
                _func(2, "t", type_id=0x1111, indirect_target=True),
                _func(3, "untyped", indirect_target=True),
            ],
            [("main", 8), ("t", 8), ("untyped", 8)],
        )
        (main,) = res["entry_functions"]
        self.assertEqual(main["max_stack_usage"], 16)
        self.assertTrue(main["is_lower_bound"])
        self.assertEqual(main["unknown_type_indirect_targets"], [3])

    def test_call_frame_overhead(self) -> None:
        records = [_func(1, "main", [2]), _func(2, "f")]
        sizes = [("main", 16), ("f", 32)]
        res = self.estimate(
            {"entry_functions": ["main"]}, records, sizes, "x86_64"
        )
        self.assertEqual(res["call_frame_overhead"], 8)
        # main 16 + 8, f 32 + 8 + 128 for the red zone, as f makes no calls.
        self.assertEqual(res["entry_functions"][0]["max_stack_usage"], 192)
        res = self.estimate(
            {"entry_functions": ["main"], "call_frame_overhead": 0},
            records,
            sizes,
            "x86_64",
        )
        self.assertEqual(res["entry_functions"][0]["max_stack_usage"], 176)

    def test_red_zone(self) -> None:
        # On x86_64, a function that makes no calls may use the 128-byte red
        # zone, which .stack_sizes leaves out. A function that calls anything,
        # even an uninstrumented function (address 0) or through a pointer,
        # can't, and other architectures have no red zone.
        records = [
            _func(1, "main", [2, 3, 4]),
            _func(2, "leaf"),
            _func(3, "calls_external", [0]),
            _func(4, "calls_indirect", indirect=[0x1234]),
        ]
        sizes = [
            ("main", 16),
            ("leaf", 32),
            ("calls_external", 32),
            ("calls_indirect", 32),
        ]
        config = {"entry_functions": ["main"], "call_frame_overhead": 0}
        res = self.estimate(config, records, sizes, "x86_64")
        self.assertEqual(res["nodes"][2]["stack"], 160)
        self.assertEqual(res["nodes"][3]["stack"], 32)
        self.assertEqual(res["nodes"][4]["stack"], 32)
        self.assertEqual(res["entry_functions"][0]["max_stack_usage"], 176)
        res = self.estimate(config, records, sizes, "aarch64")
        self.assertEqual(res["nodes"][2]["stack"], 32)
        self.assertEqual(res["entry_functions"][0]["max_stack_usage"], 48)

    def test_ambiguous_entry_function(self) -> None:
        res = self.estimate(
            {"entry_functions": ["helper", {"name": "helper", "address": 2}]},
            [_func(1, "helper"), _func(2, "helper")],
            [("helper", 8), ("helper", 16)],
        )
        ambiguous, by_address = res["entry_functions"]
        self.assertIn("Ambiguous", ambiguous["error"])
        self.assertEqual(by_address["address"], 2)
        self.assertEqual(by_address["max_stack_usage"], 16)

    def test_unknown_entry_address(self) -> None:
        # An address is authoritative: it is not resolved by name instead,
        # even if the name is unique.
        res = self.estimate(
            {"entry_functions": [{"name": "main", "address": 0x999}]},
            [_func(1, "main")],
            [("main", 8)],
        )
        (main,) = res["entry_functions"]
        self.assertEqual(main["error"], "Entry function not found")
        self.assertEqual(main["address"], 0x999)

    def test_elf_header_entry_point_default(self) -> None:
        data = {
            "FileSummary": {"Arch": "aarch64"},
            "ElfHeader": {"Entry": 0x7000},
            "CallGraph": [
                _func(0x7000, "_start", [0x7100]),
                _func(0x7100, "StartLd"),
            ],
            "StackSizes": [
                _stack_size("_start", 16, addr=0x7000),
                _stack_size("StartLd", 64, addr=0x7100),
            ],
        }
        res = stack_estimator.estimate(None, data)
        (entry,) = res["entry_functions"]
        self.assertEqual(entry["entry_function"], "_start")
        self.assertEqual(entry["address"], 0x7000)
        self.assertEqual(entry["max_stack_usage"], 80)

    def test_dynamic_symbols_default(self) -> None:
        # Without configured entry_functions, the exported functions are the
        # entry points, once per address: helper_alias is an alias of helper.
        with open(TESTDATA_DIR / "test_readelf_output.json", "r") as f:
            data = call_graph.single_file(json.load(f))
        res = stack_estimator.estimate(None, data)
        by_name = {e["entry_function"]: e for e in res["entry_functions"]}
        self.assertEqual(
            sorted(by_name),
            [
                "add",
                "call_through",
                "entry",
                "helper",
                "leaf",
                "mul",
                "rec_a",
                "rec_b",
                "vla",
            ],
        )
        self.assertEqual(by_name["helper"]["address"], 0x1650)
        self.assertEqual(by_name["helper"]["max_stack_usage"], 208)
        self.assertEqual(by_name["entry"]["max_stack_usage"], 432)

    def test_no_entry_points_error(self) -> None:
        # Neither an ELF header entry point nor a dynamic symbol table: there
        # is nothing robust to fall back on, so entry_functions is required.
        data = {
            "ElfHeader": {"Entry": 0},
            "CallGraph": [_func(0x1000, "main")],
            "StackSizes": [_stack_size("main", 16, addr=0x1000)],
        }
        with self.assertRaisesRegex(ValueError, "no entry points"):
            stack_estimator.estimate(None, data)
        self.assertEqual(
            stack_estimator.estimate({"entry_functions": ["main"]}, data)[
                "entry_functions"
            ][0]["max_stack_usage"],
            16,
        )

    def test_cli_without_config_file(self) -> None:
        # test_combined.json has no ELF header or dynamic symbol table, so
        # it can't be analyzed without a configuration file.
        callgraph_path = TESTDATA_DIR / "test_combined.json"
        with patch.object(sys, "stderr", new=io.StringIO()) as err:
            with patch.object(
                sys,
                "argv",
                ["stack_estimator.py", str(callgraph_path)],
            ):
                self.assertEqual(stack_estimator.main(), 1)
        self.assertIn("no entry points", err.getvalue())

        callgraph_path = TESTDATA_DIR / "test_readelf_output.json"
        with patch.object(sys, "stdout", new=io.StringIO()) as fake_stdout:
            with patch.object(
                sys,
                "argv",
                ["stack_estimator.py", str(callgraph_path)],
            ):
                self.assertEqual(stack_estimator.main(), 0)
        output_json = json.loads(fake_stdout.getvalue())
        self.assertEqual(len(output_json["entry_functions"]), 9)

    def test_readelf_output(self) -> None:
        # Actual llvm-readelf output for the x86-64 file built from
        # test_readelf_output.c: see the comments there.
        with open(TESTDATA_DIR / "test_readelf_output.json", "r") as f:
            data = call_graph.single_file(json.load(f))
        res = stack_estimator.estimate(
            {"entry_functions": ["entry", "helper_alias"]}, data
        )
        leaf, helper, rec_a, rec_b, vla, add, mul, call_through, entry = (
            0x1600,
            0x1650,
            0x16A0,
            0x16E0,
            0x1720,
            0x1760,
            0x17B0,
            0x1800,
            0x1860,
        )
        self.assertEqual(res["call_frame_overhead"], 8)
        nodes = res["nodes"]
        # Frame sizes from .stack_sizes; vla has none. leaf, add and mul make
        # no calls, so the 128-byte x86-64 red zone is added to theirs.
        self.assertEqual(
            {addr: nodes[addr]["stack"] for addr in nodes},
            {
                leaf: 24 + 128,
                helper: 40,
                rec_a: 24,
                rec_b: 40,
                vla: None,
                add: 40 + 128,
                mul: 72 + 128,
                call_through: 56,
                entry: 152,
            },
        )
        self.assertEqual(nodes[helper]["name"], "helper")
        self.assertEqual(nodes[vla]["uninstrumented"], [0])
        self.assertEqual(
            nodes[call_through]["indirect"],
            [{"typeId": 0x6CF58E448911DFD5, "targets": [add, mul]}],
        )

        entry_result, alias_result = res["entry_functions"]
        # entry 160 -> call_through 64 -> mul 208, with 8 bytes of return
        # address per frame.
        self.assertEqual(entry_result["address"], entry)
        self.assertEqual(entry_result["max_stack_usage"], 432)
        self.assertEqual(
            entry_result["stack_usage_expression"],
            "max(432, 400 + cycle(rec_a, rec_b), "
            "176 + calls(<external>) + frame(vla))",
        )
        self.assertEqual(
            entry_result["max_stack_path"],
            [
                {"address": entry, "name": "entry", "stack": 152},
                {"address": call_through, "name": "call_through", "stack": 56},
                {"address": mul, "name": "mul", "stack": 72 + 128},
            ],
        )
        self.assertTrue(entry_result["is_lower_bound"])
        self.assertTrue(entry_result["has_cycles"])
        self.assertEqual(entry_result["unknown_stack_sizes"], [vla])
        self.assertEqual(
            entry_result["uninstrumented_callees"],
            [{"address": 0, "callers": [vla]}],
        )
        self.assertEqual(entry_result["unmatched_indirect_calls"], [])
        self.assertEqual(entry_result["untyped_indirect_calls"], [])
        self.assertNotIn("unknown_type_indirect_targets", entry_result)
        (cycle,) = res["cycles"]
        self.assertEqual(sorted(cycle["nodes"]), [rec_a, rec_b])
        self.assertEqual(entry_result["cycles"], [cycle["id"]])
        # An alias resolves to the same function: helper 48 -> leaf 160.
        self.assertEqual(alias_result["address"], helper)
        self.assertEqual(alias_result["max_stack_usage"], 208)
        self.assertEqual(alias_result["stack_usage_expression"], "208")
        self.assertFalse(alias_result["is_lower_bound"])

    def test_algebraic_dominance_and_text_report(self) -> None:
        # When the deepest known path also ends in an uninstrumented leaf,
        # the constant path is dominated by the algebraic term. (aarch64: no
        # call frame overhead, and no red zone to add to zero_leaf.)
        data = {
            "FileSummary": {"Arch": "aarch64"},
            "ElfHeader": {"Entry": 0x1000},
            "CallGraph": [
                _func(0x1000, "_start", [0x1100, 0x1200]),
                _func(0x1100, "deep", [0x1300, 0]),
                _func(0x1200, "shallow", [0]),
                _func(0x1300, "zero_leaf"),
            ],
            "StackSizes": [
                _stack_size("_start", 24, addr=0x1000),
                _stack_size("deep", 64, addr=0x1100),
                _stack_size("shallow", 16, addr=0x1200),
                _stack_size("zero_leaf", 0, addr=0x1300),
            ],
        }
        res = stack_estimator.estimate(None, data)
        (entry,) = res["entry_functions"]
        # _start 24 -> deep 64 -> zero_leaf 0 is 88.
        # _start 24 -> deep 64 -> <external> is 88 + calls(<external>), which
        # dominates both 88 and 40 + calls(<external>).
        self.assertEqual(entry["max_stack_usage"], 88)
        self.assertEqual(
            entry["stack_usage_expression"], "88 + calls(<external>)"
        )
        text = stack_estimator.format_text_report(res)
        self.assertIn("_start (0x1000):", text)
        self.assertIn("stack usage: 88 + calls(<external>)", text)
        self.assertIn("uninstrumented callee: <external> (2 caller(s))", text)
        # The path gives each function's address as symbolizer markup.
        self.assertIn("deep {{{pc:0x1100}}}", text)

    def test_indirect_call_expression(self) -> None:
        # The targets of an indirect call are unknown when no function has
        # its type ID. The call graph section has no call site information,
        # so the term names the calling function; all such calls of a
        # function are one unknown. The text report locates the function
        # with symbolizer markup.
        data: dict[str, Any] = {
            "FileSummary": {"Arch": "aarch64"},
            "ElfHeader": {"Entry": 0x1000},
            "CallGraph": [
                _func(0x1000, "_start", [0x1100]),
                _func(0x1100, "dispatch", indirect=[0x1234, 0x5678]),
            ],
            "StackSizes": [
                _stack_size("_start", 16, addr=0x1000),
                _stack_size("dispatch", 32, addr=0x1100),
            ],
        }
        res = stack_estimator.estimate(None, data)
        (entry,) = res["entry_functions"]
        self.assertEqual(entry["max_stack_usage"], 48)
        self.assertEqual(
            entry["stack_usage_expression"], "48 + indirect(dispatch)"
        )
        text = stack_estimator.format_text_report(res)
        self.assertIn(
            "indirect call of a type that no function has, from: dispatch "
            "{{{pc:0x1100}}} (dead code unless the call graph is incomplete)",
            text,
        )
        self.assertEqual(text.count("from: dispatch"), 1)

        # A call of unknown type is reported apart, and a frame without a
        # size is marked in the path.
        data["CallGraph"][1] = _func(0x1100, "dispatch", indirect=[0])
        del data["StackSizes"][1]
        res = stack_estimator.estimate(None, data)
        (entry,) = res["entry_functions"]
        self.assertEqual(
            entry["stack_usage_expression"],
            "16 + frame(dispatch) + indirect(dispatch)",
        )
        text = stack_estimator.format_text_report(res)
        self.assertIn(
            "indirect call of unknown type from: dispatch {{{pc:0x1100}}}",
            text,
        )
        self.assertIn(
            "max stack path (at least; a frame size is unknown):", text
        )
        self.assertIn("unknown  dispatch {{{pc:0x1100}}}", text)

    def test_symbolizer_markup_context(self) -> None:
        # With the build ID and program headers in the readelf output, the
        # text report starts with the markup that lets a filter resolve the
        # pc elements: the module and its load segments, unrelocated.
        def segment(vaddr: int, size: int, flags: list[str]) -> Any:
            return {
                "ProgramHeader": {
                    "Type": {"Name": "PT_LOAD"},
                    "VirtualAddress": vaddr,
                    "MemSize": size,
                    "Flags": {"Flags": [{"Name": f} for f in flags]},
                }
            }

        data = {
            "FileSummary": {"File": "out/libfoo.so", "Arch": "aarch64"},
            "NoteSections": [
                {
                    "NoteSection": {
                        "Name": ".note.gnu.build-id",
                        "Notes": [{"Owner": "GNU", "Build ID": "0123abcd"}],
                    }
                }
            ],
            "ProgramHeaders": [
                {"ProgramHeader": {"Type": {"Name": "PT_PHDR"}}},
                segment(0x0, 0x1000, ["PF_R"]),
                segment(0x1000, 0x2000, ["PF_X", "PF_R"]),
                segment(0x3000, 0x100, ["PF_W", "PF_R"]),
            ],
        }
        self.assertEqual(
            call_graph.symbolizer_markup_context(data),
            [
                "{{{reset}}}",
                "{{{module:0:libfoo.so:elf:0123abcd}}}",
                "{{{mmap:0x0:0x1000:load:0:r:0x0}}}",
                "{{{mmap:0x1000:0x2000:load:0:rx:0x1000}}}",
                "{{{mmap:0x3000:0x100:load:0:rw:0x3000}}}",
            ],
        )
        # Without them there is no context, and the report has none.
        self.assertEqual(call_graph.symbolizer_markup_context({}), [])
        text = stack_estimator.format_text_report(
            {"entry_functions": []}, call_graph.symbolizer_markup_context(data)
        )
        self.assertTrue(text.startswith("{{{reset}}}\n{{{module:0:libfoo.so"))


def _func(
    addr: int,
    name: str,
    callees: Sequence[int] = (),
    indirect: Sequence[int] = (),
    type_id: int = 0,
    indirect_target: bool = False,
) -> dict[str, Any]:
    """Returns a CallGraph entry in llvm-readelf JSON format."""
    return {
        "Function": {
            "Names": [name],
            "Address": addr,
            "Version": 0,
            "IsIndirectTarget": indirect_target,
            "TypeID": type_id,
            "DirectCallees": [{"Address": c} for c in callees],
            "IndirectTypeIDs": list(indirect),
        }
    }


def _stack_size(name: str, size: int, addr: int) -> dict[str, Any]:
    """Returns a StackSizes entry in llvm-readelf JSON format."""
    return {"Entry": {"Functions": [name], "Address": addr, "Size": size}}


def _stack_sizes(
    records: list[Any], sizes: Sequence[tuple[str, int]]
) -> list[dict[str, Any]]:
    """
    Returns StackSizes entries for the given (name, size) pairs, one at the
    address of each function in records with that name.
    """
    return [
        _stack_size(name, size, record["Function"]["Address"])
        for name, size in sizes
        for record in records
        if name in record["Function"]["Names"]
    ]


def main() -> None:
    # unittest.main() can't parse arguments which are not its own,
    # so we need to parse our arguments first.
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--testdata-dir",
        required=True,
        type=pathlib.Path,
        help="Path to testdata",
    )
    # unittest.main() will see the leftover args and complain.
    # We use parse_known_args to separate them out.
    args, remaining_argv = parser.parse_known_args()

    # Make TESTDATA_DIR available to the tests.
    global TESTDATA_DIR
    TESTDATA_DIR = args.testdata_dir

    # Now, run unittest with its own arguments, plus sys.argv[0]
    unittest.main(argv=[sys.argv[0]] + remaining_argv)


if __name__ == "__main__":
    main()
