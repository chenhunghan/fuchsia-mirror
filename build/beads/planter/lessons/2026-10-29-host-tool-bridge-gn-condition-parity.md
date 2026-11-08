# Host tool bridge gn condition parity

- **Learned from:** inner loop friction on a local task (task `src-developer-ffx-tools-usb`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

After find_planter_bin was restored in gn_attr_parity.sh,
check_gn_conditions_with_host_tools only recognized bazel_host_tool() targets
when allowing newly added if (is_host) guards above the BAZEL2GN SENTINEL,
missing bazel_ffx_tool() (which expands to bazel_host_tool() and asserts
is_host) and companion forwarding groups that depend on its generated
sub-targets when BUILD.gn also defines an unconditional group("tests") evaluated
in the default toolchain.

## Why not a one-off fix

Every dual-build package that bridges a host tool via bazel_ffx_tool() or
bazel_host_tool() while retaining an unconditional group("tests") for
bazel2gn-generated unit tests must guard the host-tool bridge and its forwarding
groups with if (is_host) so default-toolchain evaluation does not trip
bazel_host_tool's assertion; recognizing bazel_ffx_tool and its forwarding
groups in gn_attr_parity.sh prevents false-positive unrequested_gn_condition
errors across all such migrations.
