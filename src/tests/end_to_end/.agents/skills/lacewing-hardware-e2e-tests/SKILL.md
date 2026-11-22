---
name: lacewing-hardware-e2e-tests
description: >
  Guidelines and code review rubric for authoring and reviewing Fuchsia
  Lacewing hardware and driver end-to-end (E2E) tests in Python and GN. Covers
  test layering, reboot budgeting, device-agnostic FIDL usage, capability
  discovery, repository boundaries, GN environment gating, hardware safety
  preconditions, sticky debug override cleanup, and telemetry drift
  assertions. Use when authoring or reviewing files under
  src/tests/end_to_end/, src/testing/end_to_end/, or vendor E2E test
  directories. Don't use for non-E2E unit tests or general timeout/deadline
  plumbing (use lacewing-timeouts-deadlines-cancellations).
---

# Lacewing hardware end-to-end tests

Apply these rules when authoring or reviewing host-driven Lacewing (Mobly +
Honeydew) end-to-end tests and `BUILD.gn` files that exercise hardware
subsystems, power, or drivers.

## Layer tests and minimize device reboots

Choose the lowest test layer that can exercise an invariant, and minimize total
device reboots across E2E test suites:

- **Keep fault injection in unit or component tests:** Hardware fault-injection
  and recovery paths (such as co-processor hangs, bus lockups, or corrupt
  non-volatile memory recovery) cannot be triggered deterministically by a
  normal reboot. Cover them in driver unit tests or fake-hardware component
  tests rather than host-driven reboot tests.
- **Run single-boot hardware checks on target:** Single-boot register,
  interrupt, EEPROM, and driver state checks that do not span a full device
  reboot belong in on-target Rust or C++ device tests rather than host-driven
  Mobly tests that scrape CLI output.
- **Consolidate cross-reboot checks to budget reboots:** Full device reboots
  (`await self.dut.reboot()`) are expensive and increase transport flake
  surface. Reserve Lacewing reboot tests for true cross-reboot persistence, fold
  related pre/post-reboot checks into a single reboot, and expose a boolean test
  parameter (such as `skip_reboot_test = true`) when a dedicated bench suite
  already covers the cross-reboot checks in another target.

## Use device-agnostic FIDL interfaces and respect repo boundaries

Keep `fuchsia.git` E2E tests (`//src/tests/end_to_end/`) device-agnostic and
vendor-neutral:

- **Prefer typed FIDL over Inspect or log scraping:** Query Honeydew FIDL
  affordances (such as `await self.dut.battery.get_status()` and `await
  self.dut.battery.get_spec()`) or typed FIDL proxies rather than scraping `ffx
  inspect` trees or scanning `ffx log dump` output for ad-hoc error strings.
- **Never guess driver schemas or log strings:** Always verify FIDL contracts,
  Inspect property names, storage locations, and debug subcommands against the
  actual driver source code before asserting on them.
- **Discover component monikers dynamically by capability:** Never hardcode
  chip-specific driver names or wildcard Inspect selectors. Query `ffx --machine
  json component capability` (`ffx_types.MachineFormat.JSON`) and match entries
  that declare or expose the target FIDL service from `self` to `parent`
  (excluding `devfs_driver`).
- **Keep vendor- and chip-specific helpers out of `fuchsia.git`:** Driver `ffx
  component explore -c "debug ..."` wrappers, chip register-dump parsers, and
  board-specific test cases belong in the vendor repository, never under
  `//src/tests/end_to_end/` in `fuchsia.git`.

**Good:**

```python
import json
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.ffx import types as ffx_types


def try_discover_service_moniker(
    dut: fuchsia_device.FuchsiaDevice, service_capability: str
) -> str | None:
    """Finds the component moniker declaring or exposing service_capability."""
    output = dut.ffx.run(
        ["component", "capability", service_capability],
        machine=ffx_types.MachineFormat.JSON,
    )
    entries = json.loads(output)
    if isinstance(entries, list):
        for entry in entries:
            if not isinstance(entry, dict):
                continue
            if entry.get("capability") != service_capability:
                continue
            entry_type = entry.get("type")
            is_provider = entry_type == "declare_by" or (
                entry_type == "expose_by"
                and entry.get("source") == "self"
                and entry.get("target") == "parent"
            )
            moniker = entry.get("moniker")
            if (
                is_provider
                and isinstance(moniker, str)
                and moniker
                and "devfs_driver" not in moniker
            ):
                return moniker
    return None
```

**Avoid:**

```python
# Hardcodes a chip name in fuchsia.git and scrapes raw text or Inspect trees.
_CHIP_SELECTOR = "bootstrap/*specific_pmic*:root"
_FATAL_LOG_PATTERNS = ("corrupt model", "unhandled hang")
```

## Separate and gate GN targets by hardware setup

Hardware E2E tests often run across environments with different physical
capabilities. Separate `python_mobly_test` targets and GN groups by the setup
each target expects:

1.  **Emulator (`emu_env`):** Includes fake driver or tool packages in
    `package_deps` (for example,
    `//src/power/testing/fake-battery/tool:package`).
2.  **Standard CI/CQ hardware infra (`has_real_battery = false`):** Targets
    standard board environments where bots use placeholder peripherals or
    battery eliminators without physical cells or pack EEPROMs.
3.  **Real-peripheral test bench and physical devices (`has_real_battery =
    true`):** Targets dedicated benches and local devices equipped with real
    physical peripherals. Place these in a dedicated GN group and **exclude**
    them from `group("tests")` and default infra groups so standard
    placeholder-hardware builders do not schedule them automatically.

Parse boolean Mobly `user_params` defensively across YAML boolean and CLI string
representations:

```python
from collections.abc import Mapping


def parse_bool_param(
    user_params: Mapping[str, object], key: str, *, default: bool = False
) -> bool:
    """Parses a boolean Mobly user parameter safely across YAML and CLI types."""
    if key not in user_params:
        return default
    val = user_params[key]
    if isinstance(val, bool):
        return val
    return str(val).strip().lower() in ("true", "1")
```

**Good (`BUILD.gn`):**

```gn
if (is_host) {
  # (1) Emulator with fake-battery package
  python_mobly_test("battery_emu_test") {
    main_source = "battery_test.py"
    libraries = _common_libraries
    package_deps = [ "//src/power/testing/fake-battery/tool:package" ]
    environments = [ emu_env ]
  }

  # (2) Standard hardware infra (battery eliminator / placeholder peripheral)
  python_mobly_test("battery_test_fc") {
    main_source = "battery_test.py"
    libraries = _common_libraries
    params = {
      has_real_battery = false
    }
    environments = [ board_env ]
  }

  # (3) Real-peripheral bench and local physical devices
  python_mobly_test("battery_real_battery_test") {
    main_source = "battery_test.py"
    libraries = _common_libraries
    params = {
      has_real_battery = true
      skip_reboot_test = true
    }
    environments = [ board_env ]
  }
}

group("e2e_battery_real_battery_tests") {
  testonly = true
  public_deps = [ ":battery_real_battery_test($host_toolchain)" ]
}

# Exclude real-peripheral bench targets from default group("tests").
group("tests") {
  testonly = true
  public_deps = [
    ":e2e_battery_emu_tests",
    ":e2e_battery_infra_tests",
  ]
}
```

## Enforce hardware safety preconditions and three-point override cleanup

Mutating hardware power or peripheral state (for example, disabling a buck
converter or forcing charger discharge via
`fuchsia.hardware.power.charger/Debug.SetControl`) can brown out system power if
run on a battery eliminator or a depleted cell. Moreover, debug FIDL overrides
are frequently sticky across channel closure and unrouted on non-eng builds:

- **Skip when physical hardware or debug routing is absent:** Skip with
  `asserts.skip(...)` when `has_real_battery` is `False` or when
  `try_discover_service_moniker(self.dut, debug_service)` returns `None` (such
  as on non-eng builds).
- **Check live safety floors before mutating power state:** Query current
  telemetry before applying an override, assert that `status.present is True`,
  and call `asserts.skip(...)` if voltage or state-of-charge is below a safe
  floor.
- **Use three-point cleanup for sticky debug overrides:**
    1.  **`setup_class()`:** Initialize the override state flag (for example,
        `self._charger_override_active = False`) and perform a best-effort clear
        (`check=False`) to remove any stale sticky override left behind by an
        interrupted prior test run.
    2.  **`try ... finally` around the mutation:** Perform a checked clear
        (`check=True`) in `finally` so failure to restore hardware state fails
        the test immediately.
    3.  **`teardown_test()`:** Perform a best-effort clear (`check=False`) if an
        override flag is still marked active.

**Good:**

```python
async def setup_class(self):
    await super().setup_class()
    self.dut.fuchsia_controller.create_context()
    self._charger_override_active: bool = False
    await utils.clear_charger_debug_control(self.dut, check=False)


async def teardown_test(self):
    if self._charger_override_active:
        await utils.clear_charger_debug_control(self.dut, check=False)
        self._charger_override_active = False
    await super().teardown_test()


async def test_set_discharging_status(self):
    if not self._has_real_battery:
        asserts.skip("Skipping discharge test on placeholder-battery setup.")
    if (
        utils.try_discover_service_moniker(
            self.dut, utils.CHARGER_DEBUG_SERVICE_CAPABILITY
        )
        is None
    ):
        asserts.skip("DebugService is not routed on this build.")

    pre_status = await self.battery.get_status()
    asserts.assert_true(
        pre_status.present is True,
        f"Expected battery present=True before forcing DISCHARGING, got {pre_status}",
    )
    asserts.assert_is_not_none(
        pre_status.voltage_uv,
        "Expected voltage_uv telemetry before forcing DISCHARGING.",
    )
    assert pre_status.voltage_uv is not None
    if pre_status.voltage_uv < _DISCHARGE_MIN_VOLTAGE_UV:
        asserts.skip("Battery voltage is below safe discharge floor.")

    self._charger_override_active = True
    try:
        await utils.set_charger_discharging(self.dut)
        status = await control_flows.retry_until_deadline(
            task=self._poll_discharging_status,
            deadline=Deadline.from_timeout(_DISCHARGE_POLL_TIMEOUT),
            retry_delay=_DISCHARGE_POLL_INTERVAL,
        )
        asserts.assert_less(status.current_ua, 0)
    finally:
        await utils.clear_charger_debug_control(self.dut, check=True)
        self._charger_override_active = False
```

## Guard optional FIDL fields and assert bounded before/after drift

Telemetry assertions must account for optional FIDL table fields,
placeholder-hardware boundary values, and warm-reboot state resets:

- **Guard optional FIDL fields on pre-action presence:** Fields in FIDL tables
  (such as `fuchsia.hardware.power.battery/Status.cycle_count`) may be optional
  and unset by fake drivers on boot. Only assert `status_after.cycle_count is
  not None` when `status_before.cycle_count is not None`.
- **Gate strict `> 0` assertions on real or fake peripherals:** Placeholder
  hardware or battery eliminators can report `0` for fields like
  `remaining_capacity_uah`. Gate `> 0` assertions on `self.fake_battery is not
  None or self._has_real_battery`, and use `>= 0` on placeholder-hardware
  setups.
- **Compare post-reboot telemetry against pre-reboot values within a bounded
  delta:** When testing persistence across a reboot or driver restart, checking
  only `status_after.full_charge_capacity_uah > 0` fails to catch a reboot that
  resets learned capacity or state-of-charge to a default value. Compare
  `status_after` against `status_before` within an explicit tolerance.

**Good:**

```python
if status_before.cycle_count is not None:
    asserts.assert_is_not_none(status_after.cycle_count)
    assert status_after.cycle_count is not None
    asserts.assert_greater_equal(
        status_after.cycle_count, status_before.cycle_count
    )

max_full_cap_drift = int(
    status_before.full_charge_capacity_uah * _MAX_FULL_CAP_REBOOT_DRIFT_RATIO
)
asserts.assert_less_equal(
    abs(
        status_after.full_charge_capacity_uah
        - status_before.full_charge_capacity_uah
    ),
    max_full_cap_drift,
)
```

## Fix transport and lifecycle bugs in Honeydew instead of test wrappers

Do not add test-local wrapper functions around `await self.dut.reboot()`,
`wait_for_online()`, or Honeydew affordances that catch and swallow transport
exceptions (such as USB IPv6 scope disappearance during reboot). Fix the
underlying bug in `//src/testing/end_to_end/honeydew/` so all Lacewing E2E tests
benefit, and call `await self.dut.reboot()` directly in tests.

## Review checklist

When authoring or reviewing a Lacewing hardware E2E test change, verify that:

- [ ] Fault-injection tests live in unit/component tests, single-boot hardware
  checks live in on-target device tests, and cross-reboot E2E tests consolidate
  reboots.
- [ ] `fuchsia.git` tests under `//src/tests/end_to_end/` use typed FIDL
  affordances and JSON capability discovery (`ffx --machine json component
  capability`) with no hardcoded chip names, log-scraping patterns, or
  vendor-specific `debug` CLI parsers.
- [ ] `BUILD.gn` separates emulator, placeholder-peripheral infra, and
  real-peripheral bench targets, and excludes real-peripheral bench targets from
  default `group("tests")`.
- [ ] Destructive or power-mutating tests check capability routing, verify
  `present is True` and safe voltage/level floors before mutating state, and
  clear sticky debug overrides in `setup_class()`, `finally`, and
  `teardown_test()`.
- [ ] Post-reboot assertions guard optional FIDL fields on pre-reboot presence,
  handle `0` boundary values on placeholder hardware, and check bounded drift
  against `status_before`.
- [ ] No test-local wrappers work around Honeydew reboot or transport bugs.
