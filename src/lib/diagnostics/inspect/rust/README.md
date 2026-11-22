# `fuchsia_inspect`

`fuchsia_inspect` is a library for writing and reading Inspect-formatted
VMOs. Docs are available [here](/docs/reference/diagnostics/inspect/vmo-format.md).

## Building

This project should be automatically included in builds.

## Using

`fuchsia_inspect` can be used by depending on the
`//src/lib/diagnostics/inspect/rust` GN target and then using
the `fuchsia_inspect` crate in a Rust project.

`fuchsia_inspect` is not available in the SDK.

## Testing

Unit tests for `fuchsia_inspect` are available in the
`fuchsia_inspect_tests` package:

```
$ fx test fuchsia_inspect_tests
```

You'll need to include `//src/lib/diagnostics/inspect/rust:tests` in your
build, either by using `fx args` to put it under `universe_package_labels`, or
by `fx set [....] --with //src/lib/diagnostics/inspect/rust:tests`.

## Benchmarking

Benchmarks for `fuchsia_inspect` are available in the `rust-inspect-benchmarks`
package (`//src/lib/diagnostics/inspect/rust:benchmarks`).

Include the benchmarks in your build (for example,
`fx set terminal.x64 --with //src/lib/diagnostics/inspect/rust:benchmarks`),
then run any of the host-driven performance test targets:

```
$ fx test --e2e --outdir="out/test_out" --timestamp-artifacts rust-inspect-reader-benchmark
$ fx test --e2e --outdir="out/test_out" --timestamp-artifacts rust-inspect-snapshot-filter-benchmark
$ fx test --e2e --outdir="out/test_out" --timestamp-artifacts rust-inspect-writer-benchmark
```

To run a benchmark component in fast unit-test mode without collecting
performance results:

```
$ fx test fuchsia-pkg://fuchsia.com/rust-inspect-benchmarks#meta/writer.cm
```
