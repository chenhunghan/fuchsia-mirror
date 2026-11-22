# cargo-osrb

`cargo-osrb` is a Fuchsia developer host tool that automates the audit and
review ticket preparation for introducing new third-party Rust dependencies to
Fuchsia.

It inspects a target Cargo crate or workspace to find all the reachable
dependencies against the external crates already approved in
`//third_party/rust_crates/Cargo.lock`, discovers license metadata and license
files (`LICENSE*`, `LICENCE*`, `COPYING*`, `COPYRIGHT*`, `UNLICENSE*`) from
unpacked local cargo sources or upstream Git checkout repositories, and outputs
Fuchsia Open Source Review Board (OSRB) dependency review blocks.

## Usage via `fx`

`cargo-osrb` is included in the default Fuchsia build configuration and can be
built and run directly through `fx host-tool`:

```sh
# Inspect the latest version of a crate published on crates.io:
fx host-tool cargo-osrb --crate serde_yaml

# Inspect a specific version of a crate from crates.io:
fx host-tool cargo-osrb --crate zerocopy@0.8.63

# Inspect multiple crates and manifests simultaneously:
fx host-tool cargo-osrb \
  -m ~/rust/charon/charon/Cargo.toml \
  -m ~/rust/charon/charon-ml/Cargo.toml \
  -c serde_yaml \
  -c zerocopy@0.8.63

# Generate OSRB review blocks for a local Cargo project/workspace:
fx host-tool cargo-osrb --manifest-path ~/path/to/Cargo.toml

# Include dev and build dependencies (by default only normal dependencies are
# traversed):
fx host-tool cargo-osrb \
  --manifest-path ~/rust/charon/charon/Cargo.toml \
  --include-dev-dependencies \
  --include-build-dependencies

# Filter dependencies by platform triple:
fx host-tool cargo-osrb \
  --manifest-path ~/rust/charon/charon/Cargo.toml \
  --filter-platform x86_64-unknown-linux-gnu

# Emit a Warning section instead of failing when unapproved crates are missing
# license/URL metadata or local source files:
fx host-tool cargo-osrb \
  --manifest-path ~/rust/charon/charon/Cargo.toml \
  --ignore-errors

# Write output to a file:
fx host-tool cargo-osrb \
  --manifest-path ~/rust/charon/charon/Cargo.toml \
  -o osrb-ticket.txt
```

## Output Structure

The output produced by `cargo-osrb` is structured as follows (empty optional
sections are omitted):

1. **Summary Header**: Displays inspected manifest paths, target crates, Fuchsia approved lockfile
   path, total reachable external/candidate crates, already approved crates in Fuchsia, the count of
   new dependencies needing OSRB approval broken down by direct and indirect dependencies and
   skippable candidates, and any unapproved crates with metadata errors when `--ignore-errors` is
   enabled.

2. **Warnings (when `--ignore-errors` is passed and errors occur)**: Lists unapproved crates that
   failed validation (missing license/URL metadata or missing local target source files) instead of
   failing the command.

3. **Skippable / Forked / Stubbed Crates Callout (omitted if none detected)**: Highlights
   dependencies that map to empty stub crates (e.g. Windows/macOS-specific crates like
   `windows-sys`, `core-foundation`), forked packages, or `ask2patch` crates that may be skippable or
   already customized in Fuchsia.

4. **New direct dependencies that require OSRB approval (omitted if none)**: Lists top-level
   dependencies (the root package(s) or specified `--crate` targets) requiring OSRB review.

5. **New indirect dependencies that require OSRB approval (omitted if none)**: Lists transitive
   dependencies requiring OSRB review.

Each OSRB entry contains:
- `Name`: Crate name and semver version
- `URL`: Upstream project URLs (`docs.rs` and repository)
- `LICENSE`: Declared SPDX license expression and hyperlinks to license files
- `Usage Description`: Proposed usage scope in Fuchsia

See `fx host-tool cargo-osrb --help` for all the command line arguments.

## Running Unit Tests

Unit tests can be executed via `fx test` (adding the test target if not already
in your build graph):

```sh
fx add-test //tools/cargo-osrb:tests
fx test --host cargo_osrb_bin_test
```
