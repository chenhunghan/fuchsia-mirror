# GN-only target attributes (toolchain-variant exclusion via exclude toolchain tags, default-config removal via configs -=, disable syslog backend) silently dropped when a GN target is converted to a bazel2gn-generated target

- **Learned from:** cq failure on CL I8a72d619c70f2f7790c6c233a5712af117663649, patchset 4 (task `starnix-wave-next`)
- **Date:** 2026-09-30
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

gn_dep_parity only compares dependency labels/edges and gn_target_type_parity
only compares templates; no check compared non-dependency GN attributes that
bazel2gn cannot emit. Worse, migration_sanity forced every convertible
library/binary out of the hand-written section (rejecting @bazel2gn:skip and
targets above the sentinel), so the coder converted targets whose
exclude_toolchain_tags/configs -=/disable_syslog_backend then vanished, breaking
coverage links and running asan-excluded tests.

## Why not a one-off fix

Restoring the attributes in the two affected packages would not stop the next
wave: migration_sanity would keep demanding conversion of any target carrying
these attributes, and nothing would detect the drop until instrumented or
sanitizer CQ builders fail. A deterministic base-vs-current attribute comparison
plus an exemption in migration_sanity prevents the whole class in every package.
