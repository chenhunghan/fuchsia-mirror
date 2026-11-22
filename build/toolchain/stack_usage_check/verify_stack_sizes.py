#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Checks the stack usage of the entry points of a linked ELF file.

See the --help text for the inputs and outputs, and
//scripts/clang/stack_size_estimator for the analysis.
"""

import argparse
import fnmatch
import json
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# The estimator is a prototype of tooling that is meant to move upstream, so
# it lives apart from the build integration.
_STACK_ESTIMATOR_DIR = (
    Path(__file__).resolve().parents[3]
    / "scripts"
    / "clang"
    / "stack_size_estimator"
)
if str(_STACK_ESTIMATOR_DIR) not in sys.path:
    sys.path.insert(0, str(_STACK_ESTIMATOR_DIR))

import call_graph  # pylint: disable=wrong-import-position
import stack_estimator  # pylint: disable=wrong-import-position

_DESCRIPTION = """\
Estimate the stack usage of the entry points of a linked ELF file, and
optionally check that none can use more stack than a budget.

The file must be compiled with -fexperimental-call-graph-section and
-fstack-size-section. Its entry points are the ELF header entry point
(if any), the defined function symbols in its dynamic symbol table, e.g.
the exported functions of a shared library, plus the functions in its
symbol table whose names match an entry_points pattern of the policy,
e.g. alternate entry points that the kernel patches in. The maximum
stack usage of each is estimated by //scripts/clang/stack_size_estimator
from the call graph section.
"""

_EPILOG = """\
The policy is a JSON file, since the script is meant to be driven by the
build:

  {
    "max_stack_usage": 512,
    "entry_points": ["CODE_*"],
    "limits": {"zx_channel_call": 256},
    "leaves": ["zx_ticks_get"],
    "exclude_functions": ["__zx_panic"]
  }

max_stack_usage is the budget for every entry point. entry_points are
fnmatch patterns over the symbol table that add entry points. limits and
leaves express intent for particular entry points, named by any of their
symbol names: a tighter (or looser) budget than max_stack_usage, and that
the function must call nothing at all. Naming a function that is not an
entry point is an error. exclude_functions are functions asserted to be
unreachable in this file, such as a panic path that never returns, which
the estimate leaves out along with the calls to them, in the form of the
estimator's own configuration: names or objects with an address. Every
key is optional, and so is the policy.

With a budget (max_stack_usage or a limit), every entry point must have
call graph information, and its estimate must not be a lower bound: it
is an error if the entry point reaches recursion, a function without a
stack size entry, an indirect call whose targets are unknown, or a call
to a function without call graph information. Without one, nothing is
enforced: the report just states what is known.

The report written with --output is JSON:

  {
    "elf_file": "...",
    "max_stack_usage": 512,
    "call_frame_overhead": 8,
    "entry_functions": [
      {
        "entry_function": "_zx_channel_call",
        "address": 49152,
        "names": ["_zx_channel_call", "zx_channel_call"],
        "limit": 512,
        "max_stack_usage": 48,
        "stack_usage_expression": "48",
        "max_stack_path": [{"name": "...", "address": 0, "stack": 0}],
        "is_lower_bound": true,
        "lower_bound_reasons": ["..."]
      }
    ],
    "error_report": "..."
  }

max_stack_usage and limit are present when there is a budget. The stack
of each frame of max_stack_path excludes the call_frame_overhead, which
max_stack_usage counts once per frame, and is null when it is unknown.
Without --output, the script prints the report as text to stdout, for
use by hand.

A violation fails the script, and the report then has an error_report:
the text that the script prints, which lists every violation. For an
exceeded budget it gives the frames on the call path that reaches the
maximum, so that it is clear whether to fix the code or to raise the
budget. The functions in it are followed by their addresses as
symbolizer markup, so piping the text through `ffx debug symbolize`
adds source locations. The script is meant to run as a GN validation of
the linked target, which the verify_stack_sizes() template next to it
defines: a violation fails the build, but nothing else waits for the
check.
"""

Json = dict[str, Any]

# The shape of a policy file, in the terms of call_graph.check_shape().
_POLICY_SHAPE: dict[str, Any] = {
    "max_stack_usage": int,
    "entry_points": [str],
    "limits": {},
    "leaves": [str],
    "exclude_functions": list,
}


@dataclass
class Policy:
    """What the check requires of the entry points."""

    max_stack_usage: int | None = None
    entry_point_patterns: list[str] = field(default_factory=list)
    limits: dict[str, int] = field(default_factory=dict)
    leaves: set[str] = field(default_factory=set)
    exclude_functions: list[Any] = field(default_factory=list)

    @classmethod
    def from_config(cls, config: Any) -> "Policy":
        """Parses a policy file; ValueError names what is wrong with it."""
        call_graph.check_shape(config, _POLICY_SHAPE, "policy")
        limits = config.get("limits", {})
        for name, limit in limits.items():
            call_graph.check_shape(limit, int, f"policy.limits.{name}")
        return cls(
            max_stack_usage=config.get("max_stack_usage"),
            entry_point_patterns=config.get("entry_points", []),
            limits=dict(limits),
            leaves=set(config.get("leaves", [])),
            exclude_functions=list(config.get("exclude_functions", [])),
        )

    def is_entry_point(self, name: str) -> bool:
        return any(
            fnmatch.fnmatchcase(name, pattern)
            for pattern in self.entry_point_patterns
        )


def read_elf(readelf: Path, elf_file: Path) -> Json:
    """Runs llvm-readelf and returns its parsed JSON output for the file."""
    cmd = [
        str(readelf),
        "--elf-output-style=JSON",
        "--file-headers",
        "--call-graph-info",
        "--stack-sizes",
        "--dyn-symbols",
        "--symbols",
        # For the symbolizer markup context of the messages.
        "--notes",
        "--program-headers",
        str(elf_file),
    ]
    with subprocess.Popen(cmd, stdout=subprocess.PIPE) as proc:
        assert proc.stdout is not None
        output = json.load(proc.stdout)
        if proc.wait() != 0:
            raise subprocess.CalledProcessError(proc.returncode, cmd)
    try:
        return call_graph.single_file(output)
    except ValueError as e:
        raise ValueError(f"{elf_file}: {e}") from e


def linked_file(rspfile: Path) -> Path:
    """
    Returns the linked file that a response file names, which must be
    exactly one; ValueError says how many it names otherwise.
    """
    names = rspfile.read_text().split()
    if len(names) != 1:
        raise ValueError(
            f"{rspfile} must name exactly one linked file, instead found "
            f"{len(names)}"
        )
    return Path(names[0])


def _unnamed(address: int) -> str:
    """Names a function that has no symbol, which is external if it is 0."""
    return "<external>" if address == 0 else hex(address)


def _lower_bound_reasons(
    estimate: Json,
    result: Json,
    symbols: dict[int, set[str]],
) -> list[str]:
    """Explains why an entry point's estimate is only a lower bound."""
    nodes = estimate["nodes"]

    def name(address: int, known: str | None = None) -> str:
        if known is None:
            known = nodes.get(address, {}).get("name")
        if known is None and address in symbols:
            known = min(symbols[address])
        return known if known is not None else _unnamed(address)

    reasons = []
    cycles = {cycle["id"]: cycle for cycle in estimate["cycles"]}
    for scc_id in result["cycles"]:
        members = ", ".join(sorted(name(a) for a in cycles[scc_id]["nodes"]))
        reasons.append(f"recursion among {members}")
    for address in result["unknown_stack_sizes"]:
        reasons.append(f"{name(address)} has no stack size entry")
    for callee in result["uninstrumented_callees"]:
        callers = ", ".join(sorted(name(a) for a in callee["callers"]))
        callee_name = name(callee["address"], callee.get("name"))
        reasons.append(
            f"{callee_name} has no call graph information "
            f"(called by {callers})"
        )
    for caller in sorted(
        {call["caller"] for call in result["unmatched_indirect_calls"]}
    ):
        reasons.append(
            f"an indirect call in {name(caller)} has a type that no function "
            "has (dead code unless the call graph is incomplete)"
        )
    for caller in result["untyped_indirect_calls"]:
        reasons.append(f"an indirect call in {name(caller)} has unknown type")
    if result.get("unknown_type_indirect_targets"):
        targets = ", ".join(
            sorted(name(a) for a in result["unknown_type_indirect_targets"])
        )
        reasons.append(f"indirect call targets of unknown type: {targets}")
    return reasons


def _callees(estimate: Json, address: int) -> list[str]:
    """Returns the names of everything a function calls."""
    nodes = estimate["nodes"]
    node = nodes[address]
    callees = [nodes[a]["name"] for a in node["direct"]]
    for group in node["indirect"]:
        callees += [nodes[a]["name"] for a in group["targets"]]
    callees += [_unnamed(a) for a in node["uninstrumented"]]
    if node["unmatched_indirect"] or node["untyped_indirect"]:
        callees.append("an indirect call with unknown targets")
    return callees


def _format_frame(frame: Json) -> str:
    """
    Formats a frame of a max stack path: its stack size, the function's
    name, and its address as symbolizer markup.
    """
    stack = "unknown" if frame["stack"] is None else frame["stack"]
    return f"{stack!s:>6} {frame['name']} " + call_graph.markup_pc(
        frame["address"]
    )


def check_stack_sizes(data: Json, policy: Policy) -> tuple[list[str], Json]:
    """
    Checks the entry points of the file described by the llvm-readelf JSON
    output data against the policy.

    Returns the errors, and the report: the call frame overhead that the
    estimates count per frame, and an entry for each entry point.
    """
    exported = call_graph.function_symbols(data.get("DynamicSymbols", []))
    defined = call_graph.function_symbols(data.get("Symbols", []))
    for address, exported_names in exported.items():
        defined.setdefault(address, set()).update(exported_names)

    # The entry points are the ELF header entry point (if any), the
    # exported functions, and the functions that the policy names.
    roots = {address: set(names) for address, names in exported.items()}
    elf_entry = call_graph.get_elf_entry_point(data)
    if elf_entry is not None:
        defined.setdefault(elf_entry, {hex(elf_entry)})
        roots.setdefault(elf_entry, set()).update(defined[elf_entry])
    for address, defined_names in defined.items():
        if any(policy.is_entry_point(name) for name in defined_names):
            roots.setdefault(address, set()).update(defined_names)

    errors = []
    by_name = {
        name: address for address, names in roots.items() for name in names
    }
    for name in sorted(set(policy.limits) | policy.leaves):
        if name not in by_name:
            errors.append(f"{name} is not an entry point")
    excluded = call_graph.function_set(policy.exclude_functions)
    for address in sorted(roots):
        if excluded.contains(address, roots[address]):
            errors.append(
                f"{min(roots[address])} is an entry point, so it cannot be "
                "excluded"
            )
            del roots[address]

    estimate = stack_estimator.estimate(
        {
            "entry_functions": [
                {"name": min(roots[address]), "address": address}
                for address in sorted(roots)
            ],
            "exclude_functions": policy.exclude_functions,
        },
        data,
    )

    entries = []
    for result in estimate["entry_functions"]:
        address = result["address"]
        names = sorted(defined[address])
        # An exported function is named by its exported name.
        entry_function = min(exported.get(address, names))
        entry: Json = {
            "entry_function": entry_function,
            "address": address,
            "names": names,
        }
        limits = {
            name: policy.limits[name] for name in names if name in policy.limits
        }
        if len(set(limits.values())) > 1:
            errors.append(
                f"{entry_function} has conflicting limits: "
                + ", ".join(f"{n}={l}" for n, l in sorted(limits.items()))
            )
        limit = min(limits.values(), default=policy.max_stack_usage)
        if limit is not None:
            entry["limit"] = limit
        leaf = bool(policy.leaves.intersection(names))

        if "error" in result:
            # The function has no call graph information, e.g. it is
            # written in assembly that doesn't emit the section.
            if limit is not None or leaf:
                errors.append(f"{entry_function} has no call graph information")
            entries.append(entry)
            continue

        usage = result["max_stack_usage"]
        entry["max_stack_usage"] = usage
        entry["stack_usage_expression"] = result["stack_usage_expression"]
        entry["max_stack_path"] = result["max_stack_path"]
        entry["is_lower_bound"] = result["is_lower_bound"]
        reasons = _lower_bound_reasons(estimate, result, defined)
        entry["lower_bound_reasons"] = reasons
        entries.append(entry)

        if limit is not None and usage > limit:
            overhead = estimate["call_frame_overhead"]
            detail = f" plus {overhead} bytes per call" if overhead else ""
            errors.append(
                "\n".join(
                    [
                        f"{entry_function}: max stack usage {usage} exceeds "
                        f"{limit} (frames{detail}):"
                    ]
                    + [
                        "    " + _format_frame(frame)
                        for frame in result["max_stack_path"]
                    ]
                )
            )
        if limit is not None and reasons:
            errors.append(
                f"{entry_function}: the max stack usage {usage} is only a "
                "lower bound: " + "; ".join(reasons)
            )
        if leaf:
            callees = _callees(estimate, address)
            if callees:
                errors.append(
                    f"{entry_function} must be a leaf, but calls "
                    + ", ".join(callees)
                )
    report = {
        "call_frame_overhead": estimate["call_frame_overhead"],
        "entry_functions": entries,
    }
    return errors, report


def format_text_report(report: Json, markup_context: list[str]) -> str:
    """
    Formats the report for a reader, preceded by markup_context, which is
    call_graph.symbolizer_markup_context(data), so that a markup filter can
    resolve the functions on the max stack paths to source locations.
    """
    lines = list(markup_context)
    overhead = report["call_frame_overhead"]
    overhead_note = f" (+ {overhead} bytes per call)" if overhead else ""
    for entry in report["entry_functions"]:
        name = f"{entry['entry_function']} ({entry['address']:#x})"
        if "max_stack_usage" not in entry:
            lines.append(f"{name}: no call graph information")
            continue
        lines.append(f"{name}:")
        lines.append(f"  stack usage: {entry['stack_usage_expression']}")
        if entry["is_lower_bound"]:
            lines.append(
                f"  known stack usage: {entry['max_stack_usage']} bytes "
                "(lower bound)"
            )
            lines.extend(
                f"  reason: {reason}" for reason in entry["lower_bound_reasons"]
            )
        path = entry["max_stack_path"]
        bound_note = (
            " (at least; a frame size is unknown)"
            if any(frame["stack"] is None for frame in path)
            else ""
        )
        lines.append(f"  max stack path{overhead_note}{bound_note}:")
        lines.extend("    " + _format_frame(frame) for frame in path)
    if not report["entry_functions"]:
        lines.append("no entry points")
    return "\n".join(lines)


def format_error_report(
    elf_file: Path,
    errors: list[str],
    policy: Policy,
    markup_context: list[str],
    output: Path | None,
) -> str:
    """
    Formats the errors for a reader, preceded by markup_context, which is
    call_graph.symbolizer_markup_context(data), so that a markup filter can
    resolve the functions in the errors to source locations.
    """
    lines = list(markup_context)
    detail = f" (see {output})" if output is not None else ""
    lines.append(
        f"{elf_file}: {len(errors)} stack size check error(s){detail}:"
    )
    lines.extend(f"  {error}" for error in errors)
    if policy.max_stack_usage is not None:
        lines.append(
            f"""\
The budget is {policy.max_stack_usage} bytes per entry point. Either reduce
the stack usage, or raise the budget in the policy that the build passes to
this script, if the new usage is acceptable to the callers that the budget
documents."""
        )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=_DESCRIPTION,
        epilog=_EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    input_group = parser.add_mutually_exclusive_group(required=True)
    input_group.add_argument(
        "--rspfile",
        type=Path,
        help="Response file naming the linked ELF file to check",
    )
    input_group.add_argument(
        "--elf",
        type=Path,
        help="Linked ELF file to check",
    )
    parser.add_argument(
        "--config",
        type=Path,
        help="Policy JSON file (see below)",
    )
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        help="JSON report to write (see below); "
        "without it, the report is printed as text",
    )
    parser.add_argument(
        "--depfile",
        type=Path,
        help="Depfile to write for --output",
    )
    parser.add_argument(
        "readelf",
        type=Path,
        nargs=1,
        help="llvm-readelf binary",
    )
    args = parser.parse_args()
    [readelf] = args.readelf
    if args.depfile is not None and args.output is None:
        parser.error("--depfile requires --output")

    try:
        if args.elf is not None:
            elf_file = args.elf
        else:
            elf_file = linked_file(args.rspfile)
        if args.depfile is not None:
            inputs = [readelf, elf_file]
            inputs += [f for f in (args.rspfile, args.config) if f is not None]
            deps = " ".join(str(f) for f in inputs)
            args.depfile.write_text(f"{args.output}: {deps}\n")
        policy = Policy()
        if args.config is not None:
            with open(args.config) as f:
                policy = Policy.from_config(json.load(f))
        data = read_elf(readelf, elf_file)
        errors, report = check_stack_sizes(data, policy)
    except ValueError as e:
        print(f"{sys.argv[0]}: {e}", file=sys.stderr)
        return 1
    markup_context = call_graph.symbolizer_markup_context(data)
    error_report = None
    if errors:
        error_report = format_error_report(
            elf_file, errors, policy, markup_context, args.output
        )
    if args.output is None:
        print(format_text_report(report, markup_context))
    else:
        output: Json = {"elf_file": str(elf_file)}
        if policy.max_stack_usage is not None:
            output["max_stack_usage"] = policy.max_stack_usage
        output.update(report)
        if error_report is not None:
            output["error_report"] = error_report
        with open(args.output, "w") as f:
            json.dump(output, f, indent=2)
            f.write("\n")
    if error_report is not None:
        print(error_report, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
