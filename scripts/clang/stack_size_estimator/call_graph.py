# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Call graph and stack size parsing module."""

import os
from collections import defaultdict
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from typing import Any

import graph

# Type aliases
# One file of llvm-readelf --elf-output-style=JSON output.
JsonData = dict[str, Any]
Address = int
FuncName = str
StackSizeMap = dict[Address, int]
FunctionInfoByAddr = dict[Address, dict[str, Any]]

# Bytes pushed onto the stack by a call instruction, which is not included in
# the .stack_sizes frame size. On other architectures the return address is
# passed in a register, and saving it is part of the callee's frame.
_CALL_FRAME_OVERHEAD = {
    "x86_64": 8,
    "i386": 4,
}

# Bytes below the stack pointer that the ABI lets a function use without
# moving the stack pointer. Only a function that makes no calls can use them
# (a call would overwrite them), and the compiler leaves them out of the
# frame size it reports in .stack_sizes for such a function.
_RED_ZONE = {
    "x86_64": 128,
}


def get_arch(data: JsonData) -> str:
    """Returns the architecture name that llvm-readelf printed, or ""."""
    return data.get("FileSummary", {}).get("Arch", "")


def default_call_frame_overhead(data: JsonData) -> int:
    """Returns the bytes a call pushes on the file's architecture."""
    return _CALL_FRAME_OVERHEAD.get(get_arch(data), 0)


# The shape of the llvm-readelf JSON that the analysis reads. A key may be
# absent, but a key that is present must have the type given here: a scalar
# type, a list of the element shape, or an object with further keys.
_SYMBOL_SHAPE: dict[str, Any] = {
    "Symbol": {
        "Name": {"Name": str},
        "Value": int,
        "Type": {"Name": str},
        "Section": {"Value": int},
    }
}
_FILE_SHAPE: dict[str, Any] = {
    "FileSummary": {"File": str, "Arch": str},
    "ElfHeader": {"Type": str, "Entry": int},
    "CallGraph": [
        {
            "Function": {
                "Names": [str],
                "Address": int,
                "IsIndirectTarget": bool,
                "TypeID": int,
                "DirectCallees": [{"Names": [str], "Address": int}],
                "IndirectTypeIDs": [int],
            }
        }
    ],
    "StackSizes": [
        {"Entry": {"Functions": [str], "Address": int, "Size": int}}
    ],
    "DynamicSymbols": [_SYMBOL_SHAPE],
    "Symbols": [_SYMBOL_SHAPE],
    "NoteSections": [
        {"NoteSection": {"Notes": [{"Owner": str, "Build ID": str}]}}
    ],
    "ProgramHeaders": [
        {
            "ProgramHeader": {
                "Type": {"Name": str},
                "VirtualAddress": int,
                "MemSize": int,
                "Flags": {"Flags": [{"Name": str}]},
            }
        }
    ],
}


def check_shape(value: Any, shape: Any, path: str) -> None:
    """
    Raises ValueError naming the first part of value that is not shape, in
    the terms of _FILE_SHAPE: a key may be absent, but a key that is present
    must have the given type, which is a scalar type, a list of the element
    shape, or an object with further keys. path names value in the message.
    """
    if isinstance(shape, dict):
        if not isinstance(value, dict):
            raise ValueError(f"{path}: expected an object")
        for key, key_shape in shape.items():
            if key in value:
                check_shape(value[key], key_shape, f"{path}.{key}")
    elif isinstance(shape, list):
        if not isinstance(value, list):
            raise ValueError(f"{path}: expected a list")
        for index, item in enumerate(value):
            check_shape(item, shape[0], f"{path}[{index}]")
    elif not isinstance(value, shape) or (
        shape is int and isinstance(value, bool)
    ):
        raise ValueError(f"{path}: expected {shape.__name__}")


def single_file(readelf_output: Any) -> JsonData:
    """
    Returns the one file in llvm-readelf JSON output, which is a list with an
    object per input file, checked to have the shape the analysis reads. The
    analysis is of one linked file, whose addresses mean nothing in another
    file, so any other number of files, like a malformed file, is a
    ValueError that names the problem.
    """
    if not isinstance(readelf_output, list) or len(readelf_output) != 1:
        raise ValueError(
            "expected llvm-readelf JSON output for exactly one file"
        )
    check_shape(readelf_output[0], _FILE_SHAPE, "llvm-readelf output")
    return readelf_output[0]


@dataclass(frozen=True)
class FunctionSet:
    """
    Functions named by a configuration, each by name or by address, which
    identifies the function exactly, so that a local function whose name is
    not unique can be named.
    """

    names: frozenset[FuncName] = frozenset()
    addresses: frozenset[Address] = frozenset()
    # Every function, for a setting that applies to the whole file.
    all_functions: bool = False

    def contains(self, addr: Address, names: Iterable[FuncName]) -> bool:
        return (
            self.all_functions
            or addr in self.addresses
            or not self.names.isdisjoint(names)
        )


def function_set(items: Any) -> FunctionSet:
    """
    Parses a configuration value that names functions: a list of function
    names and dictionaries with an address (and a name, for the reader), or
    true for every function.
    """
    if items is True:
        return FunctionSet(all_functions=True)
    names: set[FuncName] = set()
    addresses: set[Address] = set()
    for item in items or []:
        if isinstance(item, str):
            names.add(item)
        elif isinstance(item, dict) and "address" in item:
            addresses.add(item["address"])
        elif isinstance(item, dict) and "name" in item:
            names.add(item["name"])
    return FunctionSet(frozenset(names), frozenset(addresses))


def get_exclusions(config: dict[str, Any] | None) -> FunctionSet:
    """Parses exclude_functions, the functions left out of the analysis."""
    return function_set((config or {}).get("exclude_functions"))


def get_dead_unmatched_indirect_calls(
    config: dict[str, Any] | None,
) -> FunctionSet:
    """
    Parses dead_unmatched_indirect_calls: the functions whose indirect calls
    of a type that no function in the file has are dead code, so that such a
    call counts as no call rather than as a call to unknown targets. It is
    true for every function, or a list of functions.
    """
    return function_set((config or {}).get("dead_unmatched_indirect_calls"))


def extract_type_id_mapping(
    data: JsonData, excluded: FunctionSet = FunctionSet()
) -> dict[int, list[Address]]:
    """
    Extracts a mapping from TypeID to a list of function addresses.
    """
    type_id_to_targets: dict[int, list[Address]] = defaultdict(list)
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        addr = func.get("Address")
        if addr is None:
            continue
        if excluded.contains(addr, func.get("Names", [])):
            continue
        type_id = func.get("TypeID")
        if type_id:
            type_id_to_targets[type_id].append(addr)
    return type_id_to_targets


def find_unknown_type_targets(
    data: JsonData, excluded: FunctionSet = FunctionSet()
) -> list[Address]:
    """
    Returns the potential indirect call targets whose type ID is unknown (0).
    Any indirect call might call these functions.
    """
    targets = []
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        addr = func.get("Address")
        if addr is None or not func.get("IsIndirectTarget"):
            continue
        if excluded.contains(addr, func.get("Names", [])):
            continue
        if not func.get("TypeID"):
            targets.append(addr)
    return targets


def get_elf_entry_point(data: JsonData) -> Address | None:
    """
    Returns the ELF header entry point address if it is present and non-zero.
    """
    return data.get("ElfHeader", {}).get("Entry") or None


def function_symbols(symbols: list[JsonData]) -> dict[Address, set[str]]:
    """
    Returns the names of the defined function symbols at each address, from
    a symbol table (DynamicSymbols or Symbols) in llvm-readelf JSON format.
    """
    functions: dict[Address, set[str]] = {}
    for entry in symbols:
        symbol = entry.get("Symbol", {})
        if symbol.get("Type", {}).get("Name") != "Function":
            continue
        # SHN_UNDEF: the symbol is a reference, not a definition.
        if symbol.get("Section", {}).get("Value") == 0:
            continue
        name = symbol.get("Name", {}).get("Name")
        addr = symbol.get("Value")
        if not name or addr is None:
            continue
        functions.setdefault(addr, set()).add(name)
    return functions


def get_dynamic_function_symbols(data: JsonData) -> set[Address]:
    """
    Returns the addresses of the defined function symbols in the dynamic
    symbol table, e.g. the functions that a shared library exports.
    """
    return set(function_symbols(data.get("DynamicSymbols", [])))


def markup(element: str) -> str:
    """Wraps an element in symbolizer markup, e.g. markup("pc:0x1000")."""
    return "{{{" + element + "}}}"


def markup_pc(addr: Address) -> str:
    """Returns the symbolizer markup for a code address."""
    return markup(f"pc:{addr:#x}")


def symbolizer_markup_context(data: JsonData) -> list[str]:
    """
    Returns the symbolizer markup lines that let a markup filter (such as
    `ffx debug symbolize` or `llvm-symbolizer --filter-markup`) resolve the
    markup_pc() elements of a report: the module, by build ID, and its load
    segments at their link-time addresses.

    This needs the output of llvm-readelf --notes --program-headers in the
    data; without it the list is empty, and the elements stay unresolved.
    """
    build_id = None
    for section in data.get("NoteSections", []):
        for note in section.get("NoteSection", {}).get("Notes", []):
            if note.get("Owner") == "GNU" and "Build ID" in note:
                build_id = note["Build ID"]
    segments = [
        header["ProgramHeader"]
        for header in data.get("ProgramHeaders", [])
        if header.get("ProgramHeader", {}).get("Type", {}).get("Name")
        == "PT_LOAD"
    ]
    if build_id is None or not segments:
        return []
    name = os.path.basename(data.get("FileSummary", {}).get("File", ""))
    lines = [markup("reset"), markup(f"module:0:{name}:elf:{build_id}")]
    for segment in segments:
        flags = {
            flag["Name"] for flag in segment.get("Flags", {}).get("Flags", [])
        }
        perms = "".join(
            letter
            for flag, letter in (
                ("PF_R", "r"),
                ("PF_W", "w"),
                ("PF_X", "x"),
            )
            if flag in flags
        )
        vaddr = segment["VirtualAddress"]
        size = segment["MemSize"]
        lines.append(
            markup(f"mmap:{vaddr:#x}:{size:#x}:load:0:{perms}:{vaddr:#x}")
        )
    return lines


def get_display_names(data: JsonData) -> dict[Address, list[str]]:
    """
    Returns the symbol names that llvm-readelf printed for each function and
    direct callee address. This is only for presenting results to the user.
    """
    names_by_addr: dict[Address, list[str]] = {}
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        for item in [func] + func.get("DirectCallees", []):
            addr = item.get("Address")
            if addr is not None and item.get("Names"):
                names_by_addr.setdefault(addr, item["Names"])
    sym_names: dict[Address, set[str]] = defaultdict(set)
    for table in ("DynamicSymbols", "Symbols"):
        for addr, names in function_symbols(data.get(table, [])).items():
            sym_names[addr].update(names)
    for addr, names in sym_names.items():
        names_by_addr.setdefault(addr, sorted(names))
    return names_by_addr


def callees(adj: dict[str, Any]) -> Iterator[Address]:
    """
    Yields the functions in the call graph that an adjacency list entry
    calls, directly or as the targets of its indirect calls.
    """
    yield from adj.get("direct", [])
    for indirect in adj.get("indirect", []):
        yield from indirect.get("targets", [])


def _register_vertices(
    data: JsonData,
    excluded: FunctionSet,
    stack_sizes: StackSizeMap,
    call_graph: graph.Graph,
    fn_name_to_addr: dict[FuncName, Address],
    function_info_map: FunctionInfoByAddr,
    excluded_addrs: set[Address],
) -> None:
    """Registers function vertices and their stack sizes."""
    red_zone = _RED_ZONE.get(get_arch(data), 0)
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        addr = func.get("Address")
        names = func.get("Names", [])

        if addr is None:
            continue

        primary_name = names[0] if names else f"<unknown_@{hex(addr)}>"

        if excluded.contains(addr, names):
            excluded_addrs.add(addr)
            continue

        fn_name_to_addr.update(dict.fromkeys(names, addr))
        # None if the function has no stack size entry. For example,
        # Clang does not emit one for functions with dynamically sized
        # stack objects.
        stack_usage = stack_sizes.get(addr)
        # A function that makes no calls at all may keep its frame in
        # the red zone, below the stack pointer, where .stack_sizes
        # doesn't count it. There is no way to tell from the file
        # whether it does, so assume the worst case.
        is_leaf = not func.get("DirectCallees") and not func.get(
            "IndirectTypeIDs"
        )
        if stack_usage is not None and is_leaf:
            stack_usage += red_zone

        call_graph.add_vertex(addr)
        function_info_map[addr] = {
            "stack_usage": stack_usage,
            "label": primary_name,
            "names": names,
        }


def _add_edges(
    data: JsonData,
    type_id_to_targets: dict[int, list[Address]],
    call_graph: graph.Graph,
    function_info_map: FunctionInfoByAddr,
    adj_list: dict[Address, dict[str, Any]],
    excluded_addrs: set[Address],
    dead_unmatched: FunctionSet,
) -> None:
    """Resolves direct/indirect calls into graph edges and adjacency list."""
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        caller_addr = func.get("Address")
        names = func.get("Names", [])

        if caller_addr is None or caller_addr in excluded_addrs:
            continue

        if caller_addr not in adj_list:
            adj_list[caller_addr] = {
                "direct": [],
                "indirect": [],
                "uninstrumented": [],
                "unmatched_indirect": [],
                "untyped_indirect": 0,
            }

        for callee in func.get("DirectCallees", []):
            callee_addr = callee.get("Address")
            # Calls to excluded functions are ignored.
            if callee_addr is None or callee_addr in excluded_addrs:
                continue
            if callee_addr in function_info_map:
                call_graph.add_edge(caller_addr, callee_addr)
                adj_list[caller_addr]["direct"].append(callee_addr)
            elif callee_addr not in adj_list[caller_addr]["uninstrumented"]:
                # The callee has no call graph information. In linked
                # files, calls to functions in other modules have callee
                # address 0.
                adj_list[caller_addr]["uninstrumented"].append(callee_addr)

        for indirect_type_id in func.get("IndirectTypeIDs", []):
            if not indirect_type_id:
                # Type ID 0 is unknown, so the call may reach any function.
                adj_list[caller_addr]["untyped_indirect"] += 1
                continue
            targets = []
            for target_addr in type_id_to_targets.get(indirect_type_id, []):
                if target_addr in function_info_map:
                    call_graph.add_edge(caller_addr, target_addr)
                    targets.append(target_addr)
            if targets:
                adj_list[caller_addr]["indirect"].append(
                    {"typeId": indirect_type_id, "targets": targets}
                )
            elif not dead_unmatched.contains(caller_addr, names):
                # No function in the file has the type of the call. Unless
                # the call graph is incomplete, the call is dead code that
                # the compiler could not fold away, such as a call through a
                # function pointer that is always null behind a runtime
                # check; the configuration says which.
                adj_list[caller_addr]["unmatched_indirect"].append(
                    indirect_type_id
                )


# The e_type of a relocatable object file.
_ET_REL = 1


def elf_type(data: JsonData) -> int | None:
    """
    Returns the e_type of the file from the ELF header, which llvm-readelf
    prints with --file-headers as "Executable (0x2)" and so on, or None if
    the header is not in the data.
    """
    type_field = data.get("ElfHeader", {}).get("Type")
    if not isinstance(type_field, str):
        return None
    _, _, value = type_field.rpartition("(")
    try:
        return int(value.rstrip(")"), 0)
    except ValueError:
        return None


def _check_linked(data: JsonData) -> None:
    """
    Raises ValueError for the call graph of a relocatable object file, whose
    addresses are not final and so mean nothing to the analysis. The ELF
    header says what the file is when the data has it. Otherwise the call
    graph gives it away: in a relocatable object file the address fields of
    the call graph section are placeholders, so llvm-readelf identifies a
    function by the relocation that applies to the field instead, and prints
    a Relocation object (a Name in older versions) rather than an Address.
    """
    message = (
        "relocatable object files are not supported, the input must be a "
        "linked ELF file"
    )
    if elf_type(data) == _ET_REL:
        raise ValueError(message)
    for entry in data.get("CallGraph", []):
        func = entry.get("Function", {})
        for item in [func] + func.get("DirectCallees", []):
            if "Address" not in item:
                raise ValueError(
                    f"{message} (a function is identified by a relocation "
                    "rather than an address)"
                )


def build_call_graph(
    data: JsonData,
    stack_sizes: StackSizeMap,
    config: dict[str, Any] | None = None,
) -> tuple[
    graph.Graph,
    dict[FuncName, Address],
    FunctionInfoByAddr,
    dict[Address, dict[str, Any]],
]:
    """
    Constructs a directed graph from call graph metadata and stack sizes.

    Algorithm overview:
    1. Parses the excluded functions from the configuration so we can ignore
       undesired nodes early.
    2. Passes over the JSON data twice:
       - First pass: Registers all function vertices, resolves primary names,
         and maps their associated stack usages.
       - Second pass: Resolves call edges. Direct calls are mapped directly via
         addresses. Indirect calls are resolved by matching IndirectTypeIDs
         to target addresses.

    Invariants:
    - Nodes (vertices) are strictly unique integer addresses.
    - An edge is added only if both the caller and the callee exist in the
      first pass of vertex registration. Other direct callees are recorded
      as "uninstrumented", indirect calls of a type that no function has as
      "unmatched_indirect" unless the configuration declares them dead, and
      indirect calls of unknown type are counted in "untyped_indirect".
    - Excluded functions are skipped entirely, generating no vertices or edges.
      Calls to them are ignored.
    """
    _check_linked(data)

    call_graph = graph.Graph()
    fn_name_to_addr: dict[FuncName, Address] = {}
    function_info_map: FunctionInfoByAddr = {}

    excluded = get_exclusions(config)
    # The functions excluded by address need no record to be excluded.
    excluded_addrs: set[Address] = set(excluded.addresses)

    # Map TypeID -> List[Address]
    type_id_to_targets = extract_type_id_mapping(data, excluded)

    _register_vertices(
        data,
        excluded,
        stack_sizes,
        call_graph,
        fn_name_to_addr,
        function_info_map,
        excluded_addrs,
    )

    adj_list: dict[Address, dict[str, Any]] = {}

    _add_edges(
        data,
        type_id_to_targets,
        call_graph,
        function_info_map,
        adj_list,
        excluded_addrs,
        get_dead_unmatched_indirect_calls(config),
    )

    return call_graph, fn_name_to_addr, function_info_map, adj_list


def parse_stack_sizes(data: JsonData) -> StackSizeMap:
    """Parses the stack size entries of llvm-readelf JSON output by address."""
    sizes: StackSizeMap = {}
    # llvm-readelf --stack-sizes --elf-output-style=JSON output has a
    # "StackSizes" list in the file's object:
    # { "StackSizes": [ { "Entry": { "Functions": [...], "Address": 4096,
    #                                 "Size": 10 } } ] }
    for entry_item in data.get("StackSizes", []):
        entry = entry_item.get("Entry", {})
        addr = entry.get("Address")
        if addr is None:
            raise ValueError(
                f"stack size entry of {entry.get('Functions')} has no "
                "address, which only a linked file has"
            )
        # Identical code folding can leave several entries at one
        # address; they describe identical functions, but take the
        # largest size to be safe.
        sizes[addr] = max(entry.get("Size", 0), sizes.get(addr, 0))
    return sizes
