# process-resolver

This component implements the `fuchsia.process.Resolver` protocol. This protocol
is used by the `#!resolve` trampoline implemented in `fdio` that can launch a
binary from any Fuchsia package.

`process-resolver` uses `fuchsia.pkg.PackageResolver-boot` from its namespace to
resolve `fuchsia-boot://` URLs and an optional `fuchsia.pkg.PackageResolver-pkg`
capability to resolve `fuchsia-pkg://` URLs. Product configuration determines
whether `fuchsia.pkg.PackageResolver-pkg` is provided by `base-resolver` (so
only base shell packages can be resolved), `full-resolver` (so base, cache, and
universe shell packages can be resolved), or routed from `void` on
bootstrap-only builds (so only `fuchsia-boot://` packages can be resolved).
