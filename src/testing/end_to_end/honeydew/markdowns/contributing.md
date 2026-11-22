# Creating Honeydew Affordances, Transports, and Auxiliary Devices

[TOC]

This guide covers how Honeydew structures **affordances**, **transports**, and
**auxiliary devices** across both `honeydew` (`//src/testing/end_to_end/honeydew`)
and `ghoneydew` (`//vendor/google/testing/lacewing/ghoneydew`), including:

* How the three categories compare
* How single-implementation vs. multi-implementation modules are organized so
  IDE "Go to Definition" navigates directly to concrete implementations while
  preserving `abc.ABC` contract checks
* Step-by-step instructions for creating a new affordance, transport, or
  auxiliary device

## Overview: Affordances vs. Transports vs. Auxiliary Devices

Honeydew organizes host-side interaction code into three categories:

* **Affordances** ([honeydew/affordances/](../honeydew/affordances/)) provide
  high-level, domain-specific APIs for interacting with target device subsystems
  (such as `wlan`, `bluetooth`, `rtc`, or `tracing`) without exposing the
  underlying transport mechanics.
* **Transports** ([honeydew/transports/](../honeydew/transports/)) encapsulate
  the low-level host-to-target communication channels that affordances and
  device classes build upon, such as [FFX](../honeydew/transports/ffx/),
  [Fuchsia-Controller](../honeydew/transports/fuchsia_controller/),
  [Fastboot](../honeydew/transports/fastboot/),
  [Serial](../honeydew/transports/serial/), and
  [ADB](../honeydew/transports/adb/).
* **Auxiliary devices**
  ([honeydew/auxiliary_devices/](../honeydew/auxiliary_devices/)) encapsulate
  external host-connected lab hardware owned 1:1 by a target or peer device
  (such as `FuchsiaDevice` or `AccessPoint`), invoked by Honeydew methods,
  transports, or affordances, and controlled via DMC in Fuchsia infra using the
  parent device's nodename, such as
  [PowerSwitch](../honeydew/auxiliary_devices/power_switch/) and
  [UsbPowerHub](../honeydew/auxiliary_devices/usb_power_hub/).
  * *(Independent peer devices orchestrated directly by tests and advertised as
    standalone Botanist targets - such as `AccessPoint` - belong in
    [mobly_controller/](../../mobly_controller/) rather than
    `honeydew/auxiliary_devices/`.)*
  * Because each auxiliary device Python instance is owned 1:1 by a single
    parent device, per-device connection details (such as `outlet` or `port`)
    are passed to `__init__()` via `hardware`, while operational
    methods (such as `power_on()` and `power_off()`) take zero port/outlet
    arguments.

All three follow the exact same package layout, single- vs. multi-implementation
ABC rules, test layout, and `BUILD.gn` wiring. The only differences between them
are summarized below:

| Aspect | Affordance | Transport | Auxiliary Device |
| :--- | :--- | :--- | :--- |
| **Directory** | `honeydew/affordances/<name>/` | `honeydew/transports/<name>/` | `honeydew/auxiliary_devices/<name>/` |
| **Base class** | [Affordance](../honeydew/affordances/affordance.py) (or `AsyncLazyReady` for async FIDL affordances) | [Transport](../honeydew/transports/transport.py) | `abc.ABC` (when `<Name>` ABC exists) or standalone class |
| **`verify_supported()`** | Required (called in `__init__()`) | Required (called in `__init__()`) | Not applicable (external lab hardware) |
| **How `FuchsiaDevice` exposes it** | `@properties.Affordance` property on [FuchsiaDevice](../honeydew/fuchsia_device/fuchsia_device.py) or [device_classes](../honeydew/device_classes/) | `@properties.Transport` property on [FuchsiaDevice](../honeydew/fuchsia_device/fuchsia_device.py) or [device_classes](../honeydew/device_classes/) | `@property def <name>` and `set_<name>()` on [FuchsiaDevice](../honeydew/fuchsia_device/fuchsia_device.py) (or [USB](../honeydew/affordances/usb/usb.py) for `UsbPowerHub`), initialized automatically via `<Name>.from_config()` (`set_<name>()` is only needed for runtime overrides or dynamic fallback injection) |
| **Multi-impl selection** | `honeydew_config` (`config["affordances"]["<name>"]["implementation"]`) on `FuchsiaDevice`, or overridden in [device_classes](../honeydew/device_classes/) | Selected on `FuchsiaDevice` or overridden in [device_classes](../honeydew/device_classes/) | Strictly config-driven via `<Name>.from_config(device_config)` reading `config["auxiliary_devices"]["<name>"]` (populated by [mobly_driver/driver/infra.py](../../mobly_driver/mobly_driver/driver/infra.py) when `DMC_PATH` is set in infra, or forwarded from Mobly testbed `auxiliary_devices.<name>` by [mobly_controller/fuchsia_device.py](../../mobly_controller/fuchsia_device.py)) |
| **1-impl examples** | [hello_world](../honeydew/affordances/hello_world/), [rtc](../honeydew/affordances/rtc/), [tracing](../honeydew/affordances/tracing/) | [serial](../honeydew/transports/serial/), [adb](../honeydew/transports/adb/) | Single-implementation lab hardware module `<name>.py` |
| **Multi-impl / cross-repo examples** | [hello_fuchsia](../honeydew/affordances/hello_fuchsia/), [system_power_state_controller](../honeydew/affordances/power/system_power_state_controller/) | [ffx](../honeydew/transports/ffx/) (`FfxImpl`, `FFXWithMonitor`) | [power_switch](../honeydew/auxiliary_devices/power_switch/), [usb_power_hub](../honeydew/auxiliary_devices/usb_power_hub/) |

## Single vs. Multiple Implementations

Across all three categories (`affordances/`, `transports/`, and
`auxiliary_devices/`), Honeydew uses a clean design (enforced automatically by
[architecture_test.py](../tests/unit_tests/architecture_test.py)) that preserves direct IDE "Go
to Definition" navigation for single-implementation domains while enforcing API
contracts via an ABC whenever multiple implementations exist:

* **Single implementation (default)**: Implement the class directly as a
  concrete class `<Name>` in `<name>.py` with an empty `__init__.py` marking the
  package (license header only), and annotate the device property as returning
  `-> <name>.<Name>`. Do **not** create an abstract base class or a separate
  `<name>_using_<impl>.py` / `<name>_impl.py` module when only one
  implementation exists.
* **Multiple implementations (2+ implementations, or 1 in `honeydew` + 1 in
  `ghoneydew`)**:
  1. Define the ABC contract in `<name>.py` named `<Name>` (inheriting from
     [Affordance](../honeydew/affordances/affordance.py),
     [Transport](../honeydew/transports/transport.py), or `abc.ABC` for
     auxiliary devices) with `@abc.abstractmethod` methods to enforce that all
     implementations share the exact same public API contract. Keep
     `<name>/__init__.py` empty (license header only).
  2. Implement each concrete class in `<name>_using_<impl>.py` (or
     `<name>_impl.py`), inheriting from `<name>.<Name>`.
  3. **For affordances and transports**:
     * If callers or tests can choose which implementation to use at runtime via
       `honeydew_config`, define `class Implementation(enum.StrEnum)` in
       `<name>/types.py` (see
       [hello_fuchsia/types.py](../honeydew/affordances/hello_fuchsia/types.py)).
     * Annotate the device property on `FuchsiaDevice` and caller type hints
       with `<name>.<Name>` (the ABC), and select the concrete implementation
       either via `self._config` (`honeydew_config`) or by overriding the
       property in a device subclass under
       [device_classes](../honeydew/device_classes/).
  4. **For auxiliary devices**:
     * Auxiliary devices can have implementations across `honeydew`, `ghoneydew`,
       or external test packages (one DMC-based implementation for Fuchsia infra
       and multiple hardware implementations for local testbeds), so instead of
       a closed `types.Implementation` enum, they define a strictly config-driven
       `<Name>.from_config(device_config) -> <Name> | None` factory classmethod
       on the auxiliary device ABC (invoked in `FuchsiaDevice` or the owning
       affordance, such as `USB` for `UsbPowerHub`).
     * `from_config(device_config)` delegates to
       `create_device_from_config()` in
       [config.py](../honeydew/auxiliary_devices/config.py), reading
       `device_config["auxiliary_devices"]["<name>"]` (`implementation`
       `{"module": ..., "class": ...}` and optional `hardware` keyword
       arguments, defaulting to `{}` when omitted or `None`), dynamically
       importing and validating that the class is a concrete subclass of
       `<Name>`, and instantiating it with `**hardware`. It returns `None` when
       the auxiliary device entry is absent or empty, and raises the domain
       error class (`<Name>Error`) on malformed configs, import/lookup failures,
       invalid subclass types, or constructor `TypeError`s.
     * `config["auxiliary_devices"]["<name>"]` is populated by
       [mobly_driver/driver/infra.py](../../mobly_driver/mobly_driver/driver/infra.py)
       in Fuchsia infra when `DMC_PATH` is set (selecting `<Name>UsingDmc` with
       `hardware: {"device_name": nodename}`), or forwarded from top-level
       `auxiliary_devices.<name>` (`hardware` + `implementation`) in the Mobly
       testbed YAML by
       [mobly_controller/fuchsia_device.py](../../mobly_controller/fuchsia_device.py)
       for local runs.

### Transitioning Between Single and Multiple Implementations

* **Adding a 2nd implementation (`1 -> 2`)**:
  1. Move the existing concrete class from `<name>.py` into
     `<name>_using_<impl1>.py` (named `<Name>Using<Impl1>`, or `<name>_impl.py`
     with `<Name>Impl`) and rename its unit test to
     `<name>_using_<impl1>_test.py`.
  2. Add the new implementation in `<name>_using_<impl2>.py` (named
     `<Name>Using<Impl2>`) with its unit test `<name>_using_<impl2>_test.py`.
  3. Replace `<name>.py` with the abstract base class `class <Name>(...)` with
     `@abc.abstractmethod` methods (and have both concrete classes inherit from
     `<name>.<Name>`). Because `<name>.<Name>` keeps the exact same module and
     class name, all external callers, type annotations, and mocks remain
     completely unchanged.
  4. Wire implementation selection:
     * **For an affordance or transport** selected via `honeydew_config`, add
       `class Implementation(enum.StrEnum)` in `<name>/types.py` and update
       `FuchsiaDevice.<name>` (and `_get_<name>_affordance_implementation()`) to
       read `config["affordances"]["<name>"]["implementation"]` and return the
       selected concrete class (or override the property in
       [device_classes](../honeydew/device_classes/)).
     * **For an auxiliary device**, define
       `<Name>.from_config(device_config) -> <Name> | None` on the ABC in
       `<name>.py` delegating to `create_device_from_config()` in
       [config.py](../honeydew/auxiliary_devices/config.py) to resolve
       `implementation` (`module` + `class`) and optional `hardware` (default
       `{}`) from `device_config["auxiliary_devices"]["<name>"]`
       (forwarded from Mobly testbed YAML `auxiliary_devices` by
       [mobly_controller/fuchsia_device.py](../../mobly_controller/fuchsia_device.py)),
       and (if a DMC implementation `<Name>UsingDmc` exists for Fuchsia infra)
       populate it in
       [mobly_driver/driver/infra.py](../../mobly_driver/mobly_driver/driver/infra.py) when
       `DMC_PATH` is set. When adding a new local testbed implementation that
       users select via `auxiliary_devices.<name>` (`implementation` +
       `hardware`) in their Mobly testbed YAML (or
       `config["auxiliary_devices"]["<name>"]`), no changes to `FuchsiaDevice`,
       `from_config()`, `mobly_controller`, or `mobly_driver` are needed.
  5. **Cross-repo 2nd implementation (`1` in `honeydew` + `1` in `ghoneydew`)**:
     * In `honeydew`: define the ABC `class <Name>` in `<name>.py`, implement
       the `honeydew` concrete class in `<name>_using_<impl>.py`, and add
       `<name>.<Name>` to `_CROSS_REPO_ABCS` in
       [architecture_test.py](../tests/unit_tests/architecture_test.py) so `honeydew`'s unit
       test allows that ABC to have 1 implementation in `honeydew` without
       referencing any `ghoneydew` names.
     * In `ghoneydew`: implement the `ghoneydew` concrete class in
       `<name>_using_<impl>.py` inheriting from `honeydew`'s `<name>.<Name>`.
       `ghoneydew`'s `init_test.py` automatically verifies that every ABC across
       both repos has `>= 2` concrete implementations in total.
* **Removing an implementation leaving only 1 (`2 -> 1`)**:
  1. Delete the deprecated `<name>_using_<old_impl>.py` file and its unit test.
  2. Consolidate the remaining concrete class `<Name>Using<RemainingImpl>` back
     into `<name>.py` as `class <Name>(...)`, delete
     `<name>_using_<remaining_impl>.py`, and rename its unit test to
     `<name>_test.py` (and remove `<name>.<Name>` from `_CROSS_REPO_ABCS` if it
     was a cross-repo ABC).

## Step-by-Step Creation Guide

The steps below walk through creating a new affordance, transport, or auxiliary
device under `$FUCHSIA_DIR/src/testing/end_to_end/honeydew/honeydew/<category>/<name>/`
(where `<category>` is `affordances`, `transports`, or `auxiliary_devices`; see
[hello_world](../honeydew/affordances/hello_world/),
[hello_fuchsia](../honeydew/affordances/hello_fuchsia/),
[serial](../honeydew/transports/serial/), and
[usb_power_hub](../honeydew/auxiliary_devices/usb_power_hub/) for complete
examples):

1. Create the package directory at
   `$FUCHSIA_DIR/src/testing/end_to_end/honeydew/honeydew/<category>/<name>/`.

2. Create `OWNERS` and `METADATA.textproto` files if the module is owned by a
   specific team (for example,
   [adb/OWNERS](../honeydew/transports/adb/OWNERS)).

3. Create `__init__.py` and the module file(s):
   * `__init__.py` must be empty (license header only) to mark the directory as
     a Python package.
   * For a **single implementation**, implement the concrete `class <Name>`
     directly in `<name>.py`.
   * For **multiple implementations** (or cross-repo `honeydew` + `ghoneydew`
     implementations), define the abstract base class `class <Name>` in
     `<name>.py` and implement each concrete class `class <Name>Using<Impl>` in
     `<name>_using_<impl>.py`.

4. Create optional helper modules if needed:
   * `errors.py` (for example,
     [hello_world/errors.py](../honeydew/affordances/hello_world/errors.py) or
     [serial/errors.py](../honeydew/transports/serial/errors.py)) to define
     domain-specific exceptions inheriting from `errors.HoneydewError`.
   * `types.py` (for example,
     [hello_fuchsia/types.py](../honeydew/affordances/hello_fuchsia/types.py) or
     [fastboot/types.py](../honeydew/transports/fastboot/types.py)) to define
     custom data types for the module, including `class Implementation(enum.StrEnum)`
     when a multi-implementation affordance allows selecting the implementation
     via `honeydew_config`.

5. Inherit from the appropriate base class:
   * **Affordance**: inherit from
     [Affordance](../honeydew/affordances/affordance.py) (or `AsyncLazyReady`
     for asynchronous FIDL affordances).
   * **Transport**: inherit from
     [Transport](../honeydew/transports/transport.py).
   * **Auxiliary device**: inherit from `<Name>(abc.ABC)` when an ABC contract
     exists, or define `class <Name>` directly for a standalone single
     implementation.

6. For **affordances** and **transports**, implement `verify_supported(self) -> None`
   and call `self.verify_supported()` inside `__init__()`:
   * If only specific devices or configurations support it (such as
     [Adb.verify_supported()](../honeydew/transports/adb/adb.py)), check device
     support and raise `errors.NotSupportedError` when unsupported.
   * If all Fuchsia devices support it, provide an empty implementation with a
     short comment noting universal support.
   * *(Auxiliary devices skip this step because they represent host-side lab
     hardware rather than target device capabilities.)*

7. Create a `tests/` directory under
   `$FUCHSIA_DIR/src/testing/end_to_end/honeydew/honeydew/<category>/<name>/tests/`
   containing:
   * `unit_tests/` with `<name>_test.py` for a single-implementation module (or
     one `<name>_using_<impl>_test.py` per concrete implementation for a
     multi-implementation module, such as
     [hello_fuchsia_using_ffx_test.py](../honeydew/affordances/hello_fuchsia/tests/unit_tests/hello_fuchsia_using_ffx_test.py)
     and
     [hello_fuchsia_using_fc_test.py](../honeydew/affordances/hello_fuchsia/tests/unit_tests/hello_fuchsia_using_fc_test.py))
     and a `BUILD.gn` defining `group("tests")`.
   * `functional_tests/` (when applicable) exercising every public method
     against real hardware or an emulator, with a `BUILD.gn` defining
     `group("tests")`. For multi-implementation affordances selectable via
     `honeydew_config`, define a `python_mobly_test` target per implementation
     in `functional_tests/BUILD.gn` passing
     `honeydew_config = { affordances = { <name> = { implementation = "<impl>" } } }`
     (see
     [hello_fuchsia/tests/functional_tests/BUILD.gn](../honeydew/affordances/hello_fuchsia/tests/functional_tests/BUILD.gn)).

8. Wire unit tests into Honeydew's root [BUILD.gn](../BUILD.gn) by adding
   `"honeydew/<category>/<name>/tests/unit_tests:tests"` to
   `group("unit_tests")`, and verify locally:
   ```
   $ fx set core.x64 --with-host //src/testing/end_to_end/honeydew:unit_tests
   $ fx test //src/testing/end_to_end/honeydew --host --output
   ```

9. If functional tests were added, wire
   `"honeydew/<category>/<name>/tests/functional_tests:tests"` into
   `group("functional_tests")` in Honeydew's [BUILD.gn](../BUILD.gn), run them
   locally with `fx test ... --e2e --output`, and add them to the appropriate
   platform test groups in [end_to_end/BUILD.gn](../../BUILD.gn) (and in
   `//v/g/bundles/buildbot/<PRODUCT>/<BOARD>/BUILD.gn` in a follow-up `tqr/` CL
   for internal products).

10. Add all non-test `.py` files (excluding `tests/`) to
    `python_library("honeydew_no_testonly")` in Honeydew's
    [BUILD.gn](../BUILD.gn).

11. Expose the new module on [FuchsiaDevice](../honeydew/fuchsia_device/fuchsia_device.py)
    (or on the relevant subclass in [device_classes](../honeydew/device_classes/))
    and add unit test coverage in
    [fuchsia_device_test.py](../honeydew/fuchsia_device/tests/unit_tests/fuchsia_device_test.py)
    when applicable:
    * **Affordance**: expose via `@properties.Affordance` annotated with
      `-> <name>.<Name>`:
      * For a **single implementation**, instantiate and return `<name>.<Name>`
        directly (see `FuchsiaDevice.hello_world`).
      * For **multiple implementations**, annotate the property with the ABC
        `-> <name>.<Name>` and either:
        1. Read `self._config` (`config["affordances"]["<name>"]["implementation"]`)
           via a `_get_<name>_affordance_implementation()` helper that parses
           `<name>_types.Implementation` (raising `errors.ConfigError` on
           invalid values) and returns the selected `<Name>Using<Impl>` class
           (see `FuchsiaDevice.hello_fuchsia`), or
        2. Return the default implementation on `FuchsiaDevice` and override the
           property on a device subclass in
           [device_classes](../honeydew/device_classes/) (see
           `system_power_state_controller` on `FuchsiaDevice` vs. `Workbench`).
    * **Transport**: expose via `@properties.Transport` annotated with
      `-> <name>.<Name>`.
    * **Auxiliary device**:
      * If adding a **new implementation of an existing auxiliary device** (such
        as a new `PowerSwitch` or `UsbPowerHub` implementation), accept any
        per-device connection details (such as `outlet` or `port`) in
        `__init__()` so operational methods remain zero-argument. No changes to
        `FuchsiaDevice`, `<Name>.from_config()`, or `mobly_controller` are
        needed because `<Name>.from_config(device_config)` delegates via
        [config.py](../honeydew/auxiliary_devices/config.py) to resolve
        `implementation["module"]` / `implementation["class"]` and `**hardware`
        (optional, defaulting to `{}`) directly from
        `config["auxiliary_devices"]["<name>"]` (forwarded from
        `auxiliary_devices.<name>` in the Mobly testbed YAML by
        [mobly_controller/fuchsia_device.py](../../mobly_controller/fuchsia_device.py),
        or populated by
        [mobly_driver/driver/infra.py](../../mobly_driver/mobly_driver/driver/infra.py) for
        infra DMC implementations when `DMC_PATH` is set).
      * If creating a **new category of auxiliary device**, define
        `<Name>.from_config(device_config) -> <Name> | None` on `<name>.<Name>`
        delegating to `create_device_from_config()` in
        [config.py](../honeydew/auxiliary_devices/config.py) to read
        `device_config["auxiliary_devices"]["<name>"]` (`implementation`
        `module` + `class`, optional `hardware` kwargs, and `error_cls`),
        initialize `self._<name>` via
        `<Name>.from_config(device_config=self._config)` in
        `FuchsiaDevice.__init__()` (or the owning affordance, such as `USB` for
        `UsbPowerHub`) along with `@property def <name>` and `set_<name>()`
        (only needed when callers override the auto-configured instance or
        inject a dynamic fallback at runtime), and (if controlled via DMC in
        Fuchsia infra) populate `<Name>UsingDmc` in
        [mobly_driver/driver/infra.py](../../mobly_driver/mobly_driver/driver/infra.py) when
        `DMC_PATH` is set (top-level `auxiliary_devices.<name>` entries in the
        Mobly testbed YAML are automatically forwarded by
        [mobly_controller/fuchsia_device.py](../../mobly_controller/fuchsia_device.py)).

You can also refer to:
* [hello_world](../honeydew/affordances/hello_world/) as a complete end-to-end
  reference example for a **single-implementation** module and its tests.
* [hello_fuchsia](../honeydew/affordances/hello_fuchsia/) as a complete
  end-to-end reference example for a **multi-implementation** module (`ABC` +
  multiple implementations selected via config) and its tests.
