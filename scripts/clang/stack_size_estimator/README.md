# Stack Size Estimator Documentation

## Overview

The `stack_estimator.py` tool estimates the maximum stack usage of a program by analyzing its call graph and the stack frame size of individual functions.

## Usage

The script accepts an optional `--text` flag (to print a human-readable summary instead of JSON), an optional path to the configuration JSON, and the path to the call graph JSON. The call graph JSON is the output of `llvm-readelf --file-headers --dyn-symbols --notes --program-headers --call-graph-info --stack-sizes --elf-output-style=JSON` for a single linked ELF file (`--file-headers` and `--dyn-symbols` are only needed when the configuration doesn't list `entry_functions`; `--notes` and `--program-headers` only for the symbolizer markup of `--text`). A relocatable object file is currently not supported by the analysis.

```bash
python3 scripts/clang/stack_size_estimator/stack_estimator.py [--text] [config.json] callgraph.json
```

The `--text` report names functions and gives their addresses as [symbolizer markup](https://llvm.org/docs/SymbolizerMarkupFormat.html) (`{{{pc:0x...}}}`), preceded by the module's build ID and load segments. Pipe it through a markup filter, such as `ffx debug symbolize` in a Fuchsia checkout or `llvm-symbolizer --filter-markup`, to get source locations with full detail, including inlined frames.

Other tools can run the analysis without writing files: `stack_estimator.estimate(config, data)` takes the parsed configuration (or `None` for defaults) and `llvm-readelf` JSON, and returns the same dictionary that the script outputs as JSON. It raises `ValueError` for unsupported input, such as the call graph of a relocatable object file.

## Configuration

The `stack_estimator.py` accepts an optional configuration JSON file. The schema of `config.json` is as follows:

```json
{
  "entry_functions": ["main", {"name": "task_entry", "address": 4096}],
  "exclude_functions": ["panic"],
  "call_frame_overhead": 8,
  "external_stack_usage": 512
}
```

*   `entry_functions`: (Optional) A list of entry point functions to analyze. Each is a function name, or a dictionary with an `address` (and optional `name`). Local functions in different translation units can have the same name; such functions must be given by address. If omitted, the entry points are the ones the linked file declares itself: the ELF header entry point (`ElfHeader.Entry`) if it is non-zero, and the defined function symbols of the dynamic symbol table (`DynamicSymbols`), e.g. the functions that a shared library exports. It is an error if that set is empty.
*   `exclude_functions`: (Optional) A list of functions to leave out of the analysis, each a function name, or a dictionary with an `address` (and a `name`, for the reader), as for `entry_functions`; a local function whose name is not unique is given by address. An excluded function is treated as using no stack, and calls to it are ignored, so excluding a function asserts that it is never reached in this configuration, for example a panic handler that never returns.
*   `call_frame_overhead`: (Optional) The number of bytes that a call instruction pushes onto the stack, which is not included in the `.stack_sizes` frame size. Defaults to 8 for x86_64, 4 for i386, and 0 for other architectures, which pass the return address in a register.
*   `external_stack_usage`: (Optional) The most stack, in bytes, that a call to a function outside the file uses, including the return address that the call pushes, such as the stack usage contract of the shared library that provides the function. With it, a call out of the file (callee address 0) counts as that much, on the path as an `<external>` frame, instead of making the estimate a lower bound; the functions that make such calls are listed in `external_callers`.

## Output

The standard output will be a JSON object containing the global graph nodes, detected recursive cycles, and a summary for each defined entry function:

```json
{
  // entry_functions is the main output of the analysis which provides the stack usage
  // data for each entry point provided by the config.json entry_functions list.
  "entry_functions": [
    {
      "entry_function": "main",
      "scc_id": 0,
      "address": 12345,
      "max_stack_usage": 1024,
      "stack_usage_expression": "1024",
      "max_stack_path": [
        {"address": 12345, "name": "main", "stack": 64},
        {"address": 12400, "name": "process", "stack": 944}
      ],
      "is_lower_bound": false,
      "has_cycles": false,
      "cycles": [],
      "unknown_stack_sizes": [],
      "uninstrumented_callees": [],
      "unmatched_indirect_calls": [],
      "untyped_indirect_calls": []
    }
  ],
  "nodes": {
    "12345": {
      "name": "main",
      "stack": 64,
      "max_cumulative_stack": 1024,
      "sccId": 0,
      "direct": [12350, 12400],
      "indirect": [],
      "uninstrumented": [],
      "unmatched_indirect": [],
      "untyped_indirect": 0
    }
  },
  "cycles": [
    {
      "id": 1,
      "max_stack": 128,
      "nodes": [12350, 12360]
    }
  ],
  "call_frame_overhead": 8
}
```

`max_stack_usage` includes `call_frame_overhead` for each function on the path. On x86_64, the stack size of a function that makes no calls includes the 128-byte red zone below the stack pointer, which such a function may use without moving the stack pointer, and which `.stack_sizes` leaves out; the file doesn't say whether the function does, so the worst case is assumed. `stack_usage_expression` gives the maximum stack usage as an algebraic expression over the condensation DAG, keeping symbolic terms (`frame(<name>)` for functions with unknown frame size such as VLAs, `calls(<name>)` for uninstrumented callees, `cycle(<members>)` for recursive cycles, and `indirect(<function>)` for the indirect calls of a function whose targets are not all known) and pruning dominated path terms. `max_stack_path` lists the functions on the path with the maximum stack usage, in call order, with their stack sizes; all functions of a recursive cycle on the path are listed, since all of their stack sizes are counted. `is_lower_bound` is true if the stack usage could be higher than `max_stack_usage` because a function reachable from the entry function:

*   is recursive (`cycles`, `has_cycles`). Each function of a recursive cycle is counted once.
*   has no stack size entry (`unknown_stack_sizes`), and is counted as 0. Clang omits stack sizes for functions with dynamically sized stack objects. A function's entry is the one at its address.
*   calls a function that has no call graph information (`uninstrumented_callees`). Calls to functions in other modules have callee address 0, unless `external_stack_usage` gives them a cost.
*   makes an indirect call of a type that no function in the file has (`unmatched_indirect_calls`, each with the `caller` and the `type_id`). Unless the call graph is incomplete, such a call is dead code that the compiler could not fold away, for example a call through a function pointer that is always null behind a runtime check. `dead_unmatched_indirect_calls` in the configuration declares such calls dead, so that they count as no call: `true` for the whole file, or a list of functions in the form of `exclude_functions` for the unmatched indirect calls of those functions only.
*   makes an indirect call of unknown type (`untyped_indirect_calls`, the callers), or makes an indirect call while there are potential indirect call targets of unknown type (`unknown_type_indirect_targets`), which any indirect call might reach.

## Architecture & Core Concepts

*   **Call Graph (CG)**: A directed graph where nodes represent functions and edges represent function calls (direct or indirect).
*   **Stack Usage**: The amount of stack memory (in bytes) reserved by a function's stack frame.
*   **Strongly Connected Component (SCC)**: A subgraph where every vertex is reachable from every other vertex. In a call graph, SCCs represent recursive cycles.
*   **Condensation Graph**: A Directed Acyclic Graph (DAG) derived by contracting each SCC into a single node.

The tool operates in three main phases:

1.  **Input Parsing & Graph Construction**: Reads JSON input containing call graph info and stack sizes to build an in-memory graph.
2.  **Analysis**: Calculates the maximum cumulative stack usage starting from configured entry points.
3.  **Reporting**: Outputs the results as a structured JSON object to stdout.

### 1. Input Parsing
*   **Input Format**: Expects a configuration JSON and a call graph JSON output from `llvm-readelf --call-graph-info --stack-sizes --elf-output-style=JSON`.
*   **Graph Construction**:
    *   Nodes are created for each function address.
    *   Edges are added for direct calls and resolved indirect calls (using type ID mappings).
    *   Stack sizes are associated with each node from the `llvm-readelf` data. On x86_64, a function that makes no calls gets the 128-byte red zone added, since `.stack_sizes` doesn't count it.

### 2. Analysis

The estimator calculates stack usage by handling recursion conservatively via Strongly Connected Components.

1.  **SCC Identification**: Uses Tarjan's algorithm to identify all Strongly Connected Components (SCCs) in the call graph.
2.  **Condensation Graph**: Builds a DAG where each node is an SCC.
3.  **Local Max**: For each SCC, calculates the sum of stack usages of all functions within it (conservative maximum, except if SCC has cycles i.e. recurses, then the worst-case usage can't be bounded and the result is a lower bound).
4.  **Path Calculation**: Performs a bottom-up traversal (reverse topological sort) of the Condensation DAG.
    *   `MaxStack(SCC) = LocalStack(SCC) + Max(MaxStack(SuccessorSCCs))`
5.  **Result**: The path logic dictates the max stack usage reachable from the entry point's SCC.

### 3. Reporting

*   **JSON Output**: Generates a structured JSON object containing a consolidated map of all analyzed nodes, a list of identified cycles, and summarized stack metrics for each configured entry function. This output is printed to `stdout`.

## Data Structures

### Internal Graph
The `Graph` class (in `graph.py`) uses an adjacency list representation, which keeps the vertices in insertion order so that the results are reproducible:
```python
class Graph:
    graph: Dict[Address, List[Address]]
```

### Output Structure
The JSON output contains the following three main sections that provide the results of the analysis:

1.  **`entry_functions`**: An array containing max stack usage metrics and cycle detection flags for each entry point defined in `config.json`. This is the primary result of the tool to consult for a program's overall stack usage.
2.  **`cycles`**: An array of objects detailing any detected strongly connected components (recursive cycles) reachable in the parsed call graph.
3.  **`nodes`**: A dictionary containing structural call graph info directly (direct and indirect calls) along with the calculated max cumulative stack metric per function in the graph. Functions are identified by address and name; a consumer that wants source locations can symbolize the addresses.

## Key Algorithms

*   **Tarjan's Algorithm**: For finding SCCs in $O(V+E)$. The depth-first search is iterative, so arbitrarily deep call chains are supported.
*   **Topological Sort**: For processing the Condensation DAG in dependency order.
