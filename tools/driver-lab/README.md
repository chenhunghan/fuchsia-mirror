# driver-lab host tools

This directory contains the host tools for `driver-lab`, enabling host-side
interaction with device nodes via a driver-like Python API at runtime via the
`driver-lab` proxy driver, for rapid driver development and debugging.

See the [specification](./SPEC.md) for more details.

**Note:** This code is experimental and has not been subject to the standard
level of human code review.

---

Host tooling for the driver-lab workflow (phase 1, read-only):
plan validation and digests, operator read grants with fail-closed
consent, hashed evidence bundles, and plan execution over the
`fuchsia.driver.lab` wire contract served by `//src/devices/driver-lab`.

Most behavior is covered by host tests (`fx test --host //tools/driver-lab`)
against a wire-contract-faithful fake, plus a real-FIDL round-trip suite.
The section below runs the same stack against the real driver.

## Conformance run against the real driver (emulator)

Prerequisites: an engineering build with
`//src/devices/driver-lab:pkg` and `//src/devices/driver-lab/testing:pkg`
in the universe, a running emulator, and a registered package repository
(`fx serve` or the `fx test` temporary server).

1. Register both drivers ephemerally and create a lab node. `lab_root`
   binds to the marked node and stands up a fake platform device with
   one seeded VMO-backed MMIO; the proxy binds to the child it creates:

   ```
   ffx driver register fuchsia-pkg://fuchsia.com/lab_root#meta/lab_root.cm
   ffx driver register fuchsia-pkg://fuchsia.com/lab_proxy#meta/lab_proxy.cm
   ffx driver test-node add lab-station fuchsia.driver.lab.LAB_ROOT=selected
   ffx driver dump   # expect [lab-station] -> [proxy-target] with lab_proxy.cm
   ```

2. Run a plan unattended. Without a grant this fails closed (exit
   category 2) but finalizes evidence, which records the target's
   identity and per-resource digests:

   ```
   cd $(fx get-build-dir)
   python3 host_x64/obj/tools/driver-lab/driver-lab.pyz run \
     --plan plan.json --evidence-dir evidence --grants grants.toml \
     --target-scope <scope> --node-id proxy-target \
     --moniker "bootstrap/full-drivers:dev.lab-station.proxy-target" \
     --target <addr>
   ```

   (Run from the build dir so the bundled fuchsia-controller native
   libraries resolve. `--target` takes the address from `ffx target
   list`; a plain name only resolves in the default ffx isolate.)

3. Grant the exact read using the digest from
   `evidence/<run>/target.description.json`, then rerun with a new
   `run_id`:

   ```
   python3 .../driver-lab.pyz permissions add --grants grants.toml \
     --target-scope <scope> --node-id proxy-target \
     --resource-digest sha256:... --resource mmio0 --offset 0x10 \
     --decision allow
   ```

   The rerun exits 0 and reads the seeded value (0xFEEDFACE at 0x10);
   the evidence bundle contains the drained target audit stamped with
   the instance's boot id and generation.

Plans may pin `target.expected_boot_id` and
`node.expected_resource_digest` from a previous run's description;
mismatches fail with exit category 3 before any session opens.
