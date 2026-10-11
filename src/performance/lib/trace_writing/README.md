# Fuchsia Performance Trace Writing Utilities

This directory contains host-side libraries and developer utilities for generating and converting trace files into Fuchsia FXT format.

## Packages

### `trace_writing`

A standalone Python library for constructing Fuchsia binary FXT traces programmatically:
- `trace_writing.fxt.Builder`: In-memory FXT builder supporting records for initialization, processes, threads, durations, instants, counters, async events, flows, and context switches.

## Scripts

### `json2fxt.py`

A Python utility to convert legacy Fuchsia JSON trace formats (e.g. Google Chrome trace-viewer format containing standard thread/process metadata and flow events) into standard Fuchsia FXT binary traces.

This is extremely useful for:
1. **Test Data Migration**: Migrating any remaining legacy JSON test trace assets to direct binary FXT format.
2. **Format Verification**: Validating the correctness of direct FXT loaders by comparing parsed outputs against legacy outputs from the same baseline.

#### Usage

1. Include the host tool in your build configuration:
   `fx set ... --with-host //src/performance/lib/trace_writing:json2fxt`
2. Build the tool:
   `fx build`
3. Run the tool:
   `fx json2fxt <input_legacy_trace.json> <output_binary_trace.fxt>`

#### References

*   [Fuchsia FXT Record Format Specification](https://fuchsia.dev/fuchsia-src/reference/tracing/format/records)
