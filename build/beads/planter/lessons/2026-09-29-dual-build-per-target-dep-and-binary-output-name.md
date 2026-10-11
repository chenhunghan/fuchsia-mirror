# Dual build per target dep and binary output name parity

- **Learned from:** human review on [CL 1850658](https://fuchsia-review.googlesource.com/c/fuchsia/+/1850658), patchset 1 (task `pkg-go-leaves`)
- **Date:** 2026-09-29
- **Changed:** `checks/gn_dep_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

gn_dep_parity.sh previously only checked removed dependencies at the package
level (ignoring dependencies dropped from an individual target such as a
go_binary when a sibling go_library in the same package still depended on that
label, and downgrading non-link removals to INFO), did not check for newly added
dependencies in regenerated BUILD.gn targets (such as fine-grained
//third_party/golibs subpackage targets required by Bazel go_library when GN
only depended on the aggregate module target), and did not check that binary
output_name attributes in BUILD.gn were preserved across bazel2gn conversion.

## Why not a one-off fix

Fixing only the single Go package's BUILD.bazel and BUILD.gn leaves the entire
class of per-target dependency additions/removals and dropped binary output_name
attributes undetected across all future dual-build migrations; enforcing
per-target removed/added dependency parity and binary output_name parity in
gn_dep_parity.sh plus documenting the go_binary_host_tool and # @bazel2gn:skip
patterns in coder.md prevents these regressions across the entire repository.
