# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Estimates maximum stack usage by combining call graph and stack sizes."""

import argparse
import json
import sys
from collections import deque
from pathlib import Path
from typing import Any

import call_graph
from graph import Graph

# Type aliases
SccResults = dict[int, dict[str, Any]]
FunctionInfoByAddr = dict[int, dict[str, Any]]


def _build_condensation_graph(
    cg: Graph,
    sccs: list[list[int]],
) -> tuple[Graph, dict[int, int]]:
    node_to_scc_id = {node: i for i, scc in enumerate(sccs) for node in scc}
    scc_dag = Graph()
    for u in cg.vertices:
        scc_id_u = node_to_scc_id.get(u)
        if scc_id_u is None:
            continue
        scc_dag.add_vertex(scc_id_u)

        if u in cg.graph:
            for v in cg.graph[u]:
                scc_id_v = node_to_scc_id.get(v)
                if scc_id_v is not None and scc_id_u != scc_id_v:
                    scc_dag.add_edge(scc_id_u, scc_id_v)
    return scc_dag, node_to_scc_id


def _compute_local_scc_stack_usage(
    function_info_map: FunctionInfoByAddr,
    sccs: list[list[int]],
    call_frame_overhead: int = 0,
) -> dict[int, int]:
    # Functions with unknown stack usage (None) count as 0, which makes the
    # result a lower bound. _generate_output_json reports them.
    return {
        i: sum(
            (function_info_map.get(func_addr, {}).get("stack_usage") or 0)
            + call_frame_overhead
            for func_addr in scc
        )
        for i, scc in enumerate(sccs)
    }


def _calculate_max_paths(
    scc_dag: Graph,
    scc_stack_usage: dict[int, int],
    external_sccs: set[int],
    external_stack_usage: int | None,
) -> tuple[dict[int, int], dict[int, int | None], set[int]]:
    """
    Returns the maximum stack usage from each SCC, the successor SCC on the
    path that reaches it, and the SCCs whose maximum path ends with a call
    out of the file instead, which costs external_stack_usage. external_sccs
    are the SCCs with such a call; without a budget for it, the call is not
    counted here and the result is a lower bound.
    """
    max_stack_per_scc: dict[int, int] = {}
    scc_next_hop: dict[int, int | None] = {}
    ends_with_external_call: set[int] = set()

    sorted_sccs = scc_dag.topological_sort()
    for scc_id in reversed(sorted_sccs):
        successors = scc_dag.graph.get(scc_id, [])
        max_downstream_usage = 0
        best_next_scc = None

        if successors:
            best_next_scc = max(
                successors, key=lambda succ: max_stack_per_scc.get(succ, 0)
            )
            max_downstream_usage = max_stack_per_scc.get(best_next_scc, 0)
        if (
            external_stack_usage is not None
            and scc_id in external_sccs
            and external_stack_usage > max_downstream_usage
        ):
            best_next_scc = None
            max_downstream_usage = external_stack_usage
            ends_with_external_call.add(scc_id)

        max_stack_per_scc[scc_id] = (
            scc_stack_usage.get(scc_id, 0) + max_downstream_usage
        )
        scc_next_hop[scc_id] = best_next_scc

    return max_stack_per_scc, scc_next_hop, ends_with_external_call


def _format_scc_results(
    sccs: list[list[int]],
    function_info_map: FunctionInfoByAddr,
    max_stack_per_scc: dict[int, int],
    scc_next_hop: dict[int, int | None],
    ends_with_external_call: set[int],
) -> SccResults:
    results: SccResults = {}
    for scc_id, scc_nodes in enumerate(sccs):
        functions_in_scc = [
            {
                "name": function_info_map.get(addr, {}).get(
                    "label", f"unknown @ {hex(addr)}"
                ),
                "address": addr,
                "stack_usage": function_info_map.get(addr, {}).get(
                    "stack_usage"
                ),
            }
            for addr in scc_nodes
        ]
        results[scc_id] = {
            "max_stack_usage": max_stack_per_scc.get(scc_id, 0),
            "functions": functions_in_scc,
            "next_scc_on_max_path": scc_next_hop.get(scc_id),
            "ends_with_external_call": scc_id in ends_with_external_call,
        }
    return results


def calculate_max_stack_usage(
    cg: Graph,
    function_info_map: FunctionInfoByAddr,
    sccs: list[list[int]],
    call_frame_overhead: int = 0,
    external_callers: set[int] = set(),
    external_stack_usage: int | None = None,
) -> SccResults:
    """
    Calculates max stack usage and tracks the path for each Strongly Connected Component (SCC).

    Algorithm:
    1. Builds a Condensation Graph (a DAG) by treating each SCC as a single node.
    2. Computes the conservative local stack usage for each SCC by summing up the
       stack sizes of all functions within it, plus call_frame_overhead bytes
       for each function. For a recursive SCC this is only a lower bound.
    3. Performs a bottom-up traversal (reverse topological sort) of the Condensation
       DAG to calculate the maximum cumulative stack usage:
       `MaxStack(SCC) = LocalStack(SCC) + Max(MaxStack(SuccessorSCCs))`
       A call out of the file, from one of external_callers, is a further
       successor that uses external_stack_usage when that is known.
    """
    scc_dag, _ = _build_condensation_graph(cg, sccs)
    scc_local_usage = _compute_local_scc_stack_usage(
        function_info_map, sccs, call_frame_overhead
    )
    external_sccs = {
        scc_id
        for scc_id, scc in enumerate(sccs)
        if not external_callers.isdisjoint(scc)
    }
    (
        max_stack_per_scc,
        scc_next_hop,
        ends_with_external_call,
    ) = _calculate_max_paths(
        scc_dag, scc_local_usage, external_sccs, external_stack_usage
    )
    return _format_scc_results(
        sccs,
        function_info_map,
        max_stack_per_scc,
        scc_next_hop,
        ends_with_external_call,
    )


def _parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Estimate max stack usage from call graph and stack sizes."
    )
    parser.add_argument(
        "--text",
        action="store_true",
        help="Print a human-readable summary instead of JSON",
    )
    parser.add_argument(
        "config",
        type=Path,
        nargs="?",
        help="Path to configuration JSON file (optional; without it, the "
        "entry points are the ELF entry point and the dynamic function "
        "symbols)",
    )
    parser.add_argument(
        "callgraph",
        type=Path,
        help="Path to call graph JSON file generated from llvm-readelf",
    )
    return parser.parse_args()


def _load_json_file(path: Path) -> Any:
    """Loads a JSON file from path."""
    try:
        with open(path, "r") as f:
            return json.load(f)
    except ValueError as e:
        raise ValueError(f"{path}: {e}") from e


def _uninstrumented_callee_name(
    addr: int, display_names: dict[int, list[str]]
) -> str:
    if addr in display_names and display_names[addr]:
        return display_names[addr][0]
    if addr == 0:
        return "<external>"
    return hex(addr)


def _indirect_call_site(addr: int, func_info: FunctionInfoByAddr) -> str:
    """
    Names the function that makes an indirect call with unknown targets. The
    call graph
    section has no call site information, only target type IDs, which mean
    nothing to a reader, so the function is the most useful identification;
    the text report gives its address as symbolizer markup for the location.
    """
    return func_info.get(addr, {}).get("label", hex(addr))


def _calculate_algebraic_stack_usage(
    cg: Graph,
    sccs: list[list[int]],
    func_info: FunctionInfoByAddr,
    adj_list: dict[int, dict[str, Any]],
    cyclic_scc_ids: set[int],
    display_names: dict[int, list[str]],
    has_unknown_type_targets: bool,
    call_frame_overhead: int,
    external_stack_usage: int | None = None,
) -> dict[int, str]:
    """
    Computes an algebraic stack usage expression for each SCC.

    Each call path from an SCC contributes a linear symbolic term (c, U), where
    c is the sum of known stack frame sizes and call_frame_overhead bytes along
    the path, and U is the set of symbolic unknowns encountered:
      - frame(<name>): a function in the call graph with no .stack_sizes entry
        (e.g. a function with a VLA or alloca).
      - calls(<name>): a call to an uninstrumented callee (with address 0
        represented as <external>). A call out of the file is instead the
        constant external_stack_usage when that is known.
      - cycle(<members>): additional recursive invocations of a cyclic SCC.
      - indirect(<function>): an indirect call from <function> whose targets
        are not all known, because no function has its type, its type is
        unknown, or potential indirect targets of unknown type exist.

    Because all stack usages are non-negative, term (c1, U1) dominates (c2, U2)
    for all non-negative assignments iff c1 >= c2 and U2 is a subset of U1.
    Dominated terms are pruned bottom-up on the condensation DAG.
    """
    scc_dag, _ = _build_condensation_graph(cg, sccs)
    scc_local_usage = _compute_local_scc_stack_usage(
        func_info, sccs, call_frame_overhead
    )
    scc_terms: dict[int, list[tuple[int, frozenset[str]]]] = {}

    for scc_id in reversed(scc_dag.topological_sort()):
        scc_nodes = sccs[scc_id]
        local_const = scc_local_usage.get(scc_id, 0)
        local_unknowns: set[str] = set()
        for addr in scc_nodes:
            info = func_info.get(addr, {})
            if info.get("stack_usage") is None:
                name = info.get("label", hex(addr))
                local_unknowns.add(f"frame({name})")
        if scc_id in cyclic_scc_ids:
            members = ", ".join(
                sorted(
                    func_info.get(addr, {}).get("label", hex(addr))
                    for addr in scc_nodes
                )
            )
            local_unknowns.add(f"cycle({members})")

        continuations: list[tuple[int, frozenset[str]]] = [(0, frozenset())]
        for succ in scc_dag.graph.get(scc_id, []):
            continuations.extend(scc_terms[succ])
        for addr in scc_nodes:
            adj = adj_list.get(addr, {})
            for callee_addr in adj.get("uninstrumented", []):
                if callee_addr == 0 and external_stack_usage is not None:
                    continuations.append((external_stack_usage, frozenset()))
                    continue
                callee_name = _uninstrumented_callee_name(
                    callee_addr, display_names
                )
                continuations.append(
                    (
                        call_frame_overhead,
                        frozenset([f"calls({callee_name})"]),
                    )
                )
            # All the indirect calls of a function with unknown targets are
            # one unknown: the most stack that any of them reaches.
            if (
                adj.get("unmatched_indirect")
                or adj.get("untyped_indirect")
                or (has_unknown_type_targets and adj.get("indirect"))
            ):
                call_site = _indirect_call_site(addr, func_info)
                continuations.append(
                    (
                        call_frame_overhead,
                        frozenset([f"indirect({call_site})"]),
                    )
                )

        by_unknowns: dict[frozenset[str], int] = {}
        for cont_const, cont_unknowns in continuations:
            total_const = local_const + cont_const
            total_unknowns = frozenset(local_unknowns | cont_unknowns)
            if (
                total_unknowns not in by_unknowns
                or total_const > by_unknowns[total_unknowns]
            ):
                by_unknowns[total_unknowns] = total_const

        items = list(by_unknowns.items())
        undominated: list[tuple[int, frozenset[str]]] = []
        for i, (u_i, c_i) in enumerate(items):
            if not any(
                i != j and c_j >= c_i and u_i.issubset(u_j)
                for j, (u_j, c_j) in enumerate(items)
            ):
                undominated.append((c_i, u_i))
        undominated.sort(key=lambda t: (-t[0], sorted(t[1])))
        scc_terms[scc_id] = undominated

    expressions: dict[int, str] = {}
    for scc_id, terms in scc_terms.items():
        formatted = []
        for const_val, unk_set in terms:
            parts = [str(const_val)] if const_val or not unk_set else []
            parts.extend(sorted(unk_set))
            formatted.append(" + ".join(parts))
        if len(formatted) == 1:
            expressions[scc_id] = formatted[0]
        else:
            expressions[scc_id] = f"max({', '.join(formatted)})"
    return expressions


def _max_stack_path(
    results: SccResults,
    scc_id: int | None,
    external_stack_usage: int | None = None,
) -> list[dict[str, Any]]:
    """
    Returns the functions on the path with the maximum stack usage from the
    given SCC, in call order, each with its stack size. All functions of a
    recursive SCC on the path are listed, since all of their stack sizes are
    counted. The condensation graph is acyclic, so the path is finite. A
    path that ends with a call out of the file ends with an <external> frame
    of external_stack_usage, which is the whole cost of that call.
    """
    # TODO(https://fxbug.dev/581031215): Mark the frames that make the
    # estimate a lower bound on the path itself, where a call enters a
    # recursive cycle and where a frame has a dynamically sized object.
    path = []
    while scc_id is not None:
        result = results[scc_id]
        for function in result["functions"]:
            path.append(
                {
                    "address": function["address"],
                    "name": function["name"],
                    "stack": function["stack_usage"],
                }
            )
        if result["ends_with_external_call"]:
            path.append(
                {
                    "address": 0,
                    "name": "<external>",
                    "stack": external_stack_usage,
                }
            )
        scc_id = result["next_scc_on_max_path"]
    return path


def _generate_output_json(
    func_info: FunctionInfoByAddr,
    results: SccResults,
    algebraic_by_scc: dict[int, str],
    addr_to_scc_id: dict[int, int],
    adj_list: dict[int, dict[str, Any]],
    entry_functions: list[str | dict[str, Any]],
    cyclic_scc_ids: set[int],
    display_names: dict[int, list[str]],
    unknown_type_targets: list[int],
    call_frame_overhead: int,
    external_stack_usage: int | None = None,
) -> dict[str, Any]:
    all_nodes = {}

    for u, info in func_info.items():
        cumulative_stack = 0
        scc_id = addr_to_scc_id.get(u)
        if scc_id is not None and scc_id in results:
            cumulative_stack = results[scc_id]["max_stack_usage"]

        node_data = {
            "name": info.get("label", f"@{hex(u)}"),
            "stack": info.get("stack_usage"),
            "max_cumulative_stack": cumulative_stack,
            "sccId": scc_id,
            "direct": [],
            "indirect": [],
            "uninstrumented": [],
            "unmatched_indirect": [],
            "untyped_indirect": 0,
        }

        adj = adj_list.get(u, {})
        for v in adj.get("direct", []):
            node_data["direct"].append(v)
        for ind_group in adj.get("indirect", []):
            node_data["indirect"].append(ind_group)
        for uninst in adj.get("uninstrumented", []):
            node_data["uninstrumented"].append(uninst)
        for type_id in adj.get("unmatched_indirect", []):
            node_data["unmatched_indirect"].append(type_id)
        node_data["untyped_indirect"] = adj.get("untyped_indirect", 0)

        all_nodes[u] = node_data

    # Recursive SCCs: those with more than one function, or a function that
    # calls itself.
    all_cycles = []
    for scc_id, res in results.items():
        if scc_id in cyclic_scc_ids:
            all_cycles.append(
                {
                    "id": scc_id,
                    "max_stack": res["max_stack_usage"],
                    "nodes": [f["address"] for f in res["functions"]],
                }
            )

    entry_funcs_output: list[dict[str, Any]] = []

    for entry_item in entry_functions:
        if isinstance(entry_item, dict):
            entry_function = entry_item.get("name")
            specified_addr = entry_item.get("address")
        else:
            entry_function = entry_item
            specified_addr = None

        entry_addr: int | None = None
        error = "Entry function not found"
        if specified_addr is not None:
            # An address identifies the function exactly, so it is not
            # second-guessed by looking up the name.
            if specified_addr in addr_to_scc_id:
                entry_addr = specified_addr
        elif isinstance(entry_function, str):
            # Local functions in different translation units can have the
            # same name, so a name can be ambiguous.
            candidates = [
                addr
                for addr, info in func_info.items()
                if entry_function in info.get("names", [])
            ]
            if len(candidates) == 1:
                entry_addr = candidates[0]
            elif candidates:
                error = "Ambiguous entry function name; specify an address"

        if entry_addr is None:
            if entry_function is None and specified_addr in display_names:
                entry_function = display_names[specified_addr][0]
            entry_funcs_output.append(
                {
                    "entry_function": entry_function,
                    "address": specified_addr,
                    "error": error,
                }
            )
            continue

        if entry_function is None:
            entry_function = func_info[entry_addr]["label"]

        entry_scc_id = addr_to_scc_id.get(entry_addr)
        max_stack = 0
        if entry_scc_id is not None and entry_scc_id in results:
            max_stack = results[entry_scc_id]["max_stack_usage"]

        # Find everything reachable from this entry point that makes
        # max_stack only a lower bound.
        reachable = set()
        reachable_cycles: set[int] = set()
        unknown_stack_sizes = []
        uninstrumented_by_addr: dict[int, dict[str, Any]] = {}
        unmatched_indirect_calls = []
        untyped_indirect_calls = []
        has_indirect_calls = False
        queue = deque([entry_addr])
        while queue:
            curr = queue.popleft()
            if curr in reachable:
                continue
            reachable.add(curr)
            if addr_to_scc_id.get(curr) in cyclic_scc_ids:
                reachable_cycles.add(addr_to_scc_id[curr])
            if func_info.get(curr, {}).get("stack_usage") is None:
                unknown_stack_sizes.append(curr)
            curr_adj = adj_list.get(curr, {})
            for uninst_addr in curr_adj.get("uninstrumented", []):
                if uninst_addr not in uninstrumented_by_addr:
                    uninstrumented_by_addr[uninst_addr] = {
                        "address": uninst_addr,
                        "callers": [],
                    }
                    # Names are only for display.
                    if uninst_addr in display_names:
                        uninstrumented_by_addr[uninst_addr][
                            "name"
                        ] = display_names[uninst_addr][0]
                uninstrumented_by_addr[uninst_addr]["callers"].append(curr)
            for type_id in curr_adj.get("unmatched_indirect", []):
                unmatched_indirect_calls.append(
                    {"caller": curr, "type_id": type_id}
                )
            if curr_adj.get("untyped_indirect"):
                untyped_indirect_calls.append(curr)
            if (
                curr_adj.get("indirect")
                or curr_adj.get("unmatched_indirect")
                or curr_adj.get("untyped_indirect")
            ):
                has_indirect_calls = True

            queue.extend(call_graph.callees(curr_adj))

        # With a budget for calls out of the file, those calls are counted
        # rather than left unknown, so they do not make the estimate a lower
        # bound. The functions that make them are still reported.
        external_callers = []
        if external_stack_usage is not None and 0 in uninstrumented_by_addr:
            external_callers = uninstrumented_by_addr.pop(0)["callers"]
        uninstrumented_list = list(uninstrumented_by_addr.values())
        expr = (
            algebraic_by_scc.get(entry_scc_id, str(max_stack))
            if entry_scc_id is not None
            else str(max_stack)
        )
        entry_output: dict[str, Any] = {
            "entry_function": entry_function,
            "address": entry_addr,
            "scc_id": entry_scc_id,
            "max_stack_usage": max_stack,
            "stack_usage_expression": expr,
            "max_stack_path": _max_stack_path(
                results, entry_scc_id, external_stack_usage
            ),
            "is_lower_bound": bool(
                reachable_cycles
                or unknown_stack_sizes
                or uninstrumented_list
                or unmatched_indirect_calls
                or untyped_indirect_calls
            ),
            "has_cycles": bool(reachable_cycles),
            "cycles": sorted(reachable_cycles),
            "unknown_stack_sizes": unknown_stack_sizes,
            "uninstrumented_callees": uninstrumented_list,
            "unmatched_indirect_calls": unmatched_indirect_calls,
            "untyped_indirect_calls": untyped_indirect_calls,
        }
        if external_callers:
            entry_output["external_callers"] = external_callers
        # Functions whose type is unknown could be called by any indirect call.
        if has_indirect_calls and unknown_type_targets:
            entry_output["unknown_type_indirect_targets"] = unknown_type_targets
            entry_output["is_lower_bound"] = True
        entry_funcs_output.append(entry_output)

    output = {
        "nodes": all_nodes,
        "cycles": all_cycles,
        "entry_functions": entry_funcs_output,
        "call_frame_overhead": call_frame_overhead,
    }
    if external_stack_usage is not None:
        output["external_stack_usage"] = external_stack_usage
    return output


def estimate(
    config: dict[str, Any] | None, data: call_graph.JsonData
) -> dict[str, Any]:
    """
    Estimates max stack usage and returns the result as a dictionary.

    config is the parsed configuration JSON (or None for defaults), and data
    the file in the parsed output of llvm-readelf --call-graph-info
    --stack-sizes --elf-output-style=JSON for a single linked ELF file, as
    call_graph.single_file() returns it. The call graph section is described
    at https://llvm.org/docs/CallGraphSection.html, and the configuration and
    the output in README.md. Raises ValueError for unsupported input.
    """
    if config is None:
        config = {}
    entry_functions = config.get("entry_functions")
    if entry_functions is None:
        # The entry points that the linked file declares itself: the ELF
        # header entry point, and the functions in its dynamic symbol table.
        addresses = call_graph.get_dynamic_function_symbols(data)
        elf_entry = call_graph.get_elf_entry_point(data)
        if elf_entry is not None:
            addresses.add(elf_entry)
        if not addresses:
            raise ValueError(
                "no entry points: the ELF header entry point is zero or "
                "absent and the dynamic symbol table defines no functions; "
                "configure entry_functions"
            )
        entry_functions = [{"address": addr} for addr in sorted(addresses)]

    stack_sizes = call_graph.parse_stack_sizes(data)
    cg, _, func_info, adj_list = call_graph.build_call_graph(
        data, stack_sizes, config
    )

    call_frame_overhead = config.get("call_frame_overhead")
    if call_frame_overhead is None:
        call_frame_overhead = call_graph.default_call_frame_overhead(data)
    external_stack_usage = config.get("external_stack_usage")
    if external_stack_usage is not None:
        call_graph.check_shape(
            external_stack_usage, int, "external_stack_usage"
        )
    # The functions that call out of the file, where the callee address is 0.
    external_callers = {
        addr
        for addr, adj in adj_list.items()
        if 0 in adj.get("uninstrumented", [])
    }

    # --- Analysis Logic ---
    addr_to_scc_id = {}

    sccs = cg.get_sccs()
    for i, scc in enumerate(sccs):
        for addr in scc:
            addr_to_scc_id[addr] = i
    cyclic_scc_ids = {i for i, scc in enumerate(sccs) if cg.is_cycle(scc)}

    results = calculate_max_stack_usage(
        cg,
        func_info,
        sccs,
        call_frame_overhead,
        external_callers,
        external_stack_usage,
    )

    excluded = call_graph.get_exclusions(config)
    display_names = call_graph.get_display_names(data)
    unknown_type_targets = call_graph.find_unknown_type_targets(data, excluded)
    algebraic_by_scc = _calculate_algebraic_stack_usage(
        cg,
        sccs,
        func_info,
        adj_list,
        cyclic_scc_ids,
        display_names,
        bool(unknown_type_targets),
        call_frame_overhead,
        external_stack_usage,
    )
    return _generate_output_json(
        func_info,
        results,
        algebraic_by_scc,
        addr_to_scc_id,
        adj_list,
        entry_functions,
        cyclic_scc_ids,
        display_names,
        unknown_type_targets,
        call_frame_overhead,
        external_stack_usage,
    )


def format_text_report(
    output: dict[str, Any], markup_context: list[str] | None = None
) -> str:
    """
    Formats the stack estimation output as human-readable text.

    Functions are given by name and, as symbolizer markup, by address, so
    that a markup filter can add source locations with full detail.
    markup_context is call_graph.symbolizer_markup_context(data), which the
    filter needs to resolve them; it is printed first.
    """
    nodes = output.get("nodes", {})
    cycles = {c["id"]: c for c in output.get("cycles", [])}
    overhead = output.get("call_frame_overhead", 0)
    lines: list[str] = list(markup_context or [])

    def node_name(addr: int) -> str:
        node = nodes.get(addr) or nodes.get(str(addr)) or {}
        return node.get("name", hex(addr))

    for entry in output.get("entry_functions", []):
        name = entry.get("entry_function")
        addr = entry.get("address")
        addr_str = f" ({addr:#x})" if isinstance(addr, int) else ""
        if "error" in entry:
            lines.append(f"{name}{addr_str}: {entry['error']}")
            continue

        lines.append(f"{name}{addr_str}:")
        lines.append(f"  stack usage: {entry['stack_usage_expression']}")
        if entry.get("is_lower_bound"):
            lines.append(
                f"  known stack usage: {entry['max_stack_usage']} bytes "
                "(lower bound)"
            )
        if entry.get("unknown_stack_sizes"):
            unknown_names = ", ".join(
                node_name(a) for a in entry["unknown_stack_sizes"]
            )
            lines.append(f"  unknown frame sizes: {unknown_names}")
        if entry.get("cycles"):
            for scc_id in entry["cycles"]:
                members = ", ".join(
                    sorted(node_name(a) for a in cycles[scc_id]["nodes"])
                )
                lines.append(f"  recursion: {members}")
        if entry.get("external_callers"):
            lines.append(
                "  calls out of the file from "
                f"{len(entry['external_callers'])} function(s), "
                f"{output['external_stack_usage']} bytes each as configured"
            )
        for callee in entry.get("uninstrumented_callees", []):
            callee_addr = callee["address"]
            callee_label = callee.get(
                "name", "<external>" if callee_addr == 0 else hex(callee_addr)
            )
            callers = [node_name(a) for a in callee.get("callers", [])]
            lines.append(
                f"  uninstrumented callee: {callee_label} "
                f"({len(callers)} caller(s))"
            )
        # The type IDs of unmatched indirect calls mean nothing to a reader;
        # the calling function and its location are what's useful.
        for caller in sorted(
            {
                call["caller"]
                for call in entry.get("unmatched_indirect_calls", [])
            }
        ):
            lines.append(
                "  indirect call of a type that no function has, from: "
                f"{node_name(caller)} {call_graph.markup_pc(caller)} "
                "(dead code unless the call graph is incomplete)"
            )
        for caller in entry.get("untyped_indirect_calls", []):
            lines.append(
                "  indirect call of unknown type from: "
                f"{node_name(caller)} {call_graph.markup_pc(caller)}"
            )
        if entry.get("unknown_type_indirect_targets"):
            targets = ", ".join(
                sorted(
                    node_name(a) for a in entry["unknown_type_indirect_targets"]
                )
            )
            lines.append(f"  indirect targets of unknown type: {targets}")

        # A frame without a size, such as one with a dynamically sized array,
        # counts as 0, so the path is at least this deep.
        path = entry.get("max_stack_path", [])
        overhead_note = f" (+ {overhead} bytes per call)" if overhead else ""
        bound_note = (
            " (at least; a frame size is unknown)"
            if any(frame["stack"] is None for frame in path)
            else ""
        )
        lines.append(f"  max stack path{overhead_note}{bound_note}:")
        for frame in path:
            # The <external> frame of a call out of the file has no address.
            pc = (
                call_graph.markup_pc(frame["address"])
                if frame["address"] != 0
                else ""
            )
            size = "unknown" if frame["stack"] is None else frame["stack"]
            lines.append(f"    {size!s:>7}  {frame['name']} {pc}".rstrip())
    return "\n".join(lines)


def main() -> int:
    """Main entry point for stack estimator tool."""
    args = _parse_arguments()
    try:
        config = _load_json_file(args.config) if args.config is not None else {}
        data = call_graph.single_file(_load_json_file(args.callgraph))
        output = estimate(config, data)
    except (OSError, ValueError) as e:
        print(f"Error: {e}", file=sys.stderr)
        return 1
    if args.text:
        print(
            format_text_report(
                output, call_graph.symbolizer_markup_context(data)
            )
        )
    else:
        print(json.dumps(output, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
