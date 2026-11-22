---
name: ffx-to-honeydew-affordance
description: >
  Guide for translating FFX CLI commands, subtools, and component exploration
  tools into robust Honeydew affordances following Lacewing architectural
  patterns.
---

# Translating FFX Commands into Honeydew Affordances

This skill provides a standardized methodology for translating `ffx` commands,
subtools, and target-side CLI exploration tools into robust, ergonomic Python
affordances in Honeydew
(`src/testing/end_to_end/honeydew/honeydew/affordances/`).

While [`fidl-to-honeydew-affordance`](../fidl-to-honeydew-affordance/SKILL.md)
focuses on direct asynchronous FIDL proxies via Fuchsia Controller, this guide
addresses **FFX-based affordances** that interact with host-side `ffx` tools or
target-side exploration tools.

---

## 1. When to Use an FFX-Based Affordance

Use an FFX-based affordance when:

* **No Direct FIDL Routing**: The service or test protocol is served inside an
  isolated driver or component namespace not routed to public service
  directories, requiring `ffx component explore --tools ...`.
* **Standard FFX Subtools**: Fuchsia already provides high-level `ffx` subtools
  with built-in orchestration (e.g. `ffx session`, `ffx screenshot`, `ffx
  target`).
* **Host-Side Operations**: The operation involves host file manipulation, data
  streaming, or host-orchestrated lifecycle events.

---

## 2. Directory & File Structure (Single vs. Multiple Implementations)

Honeydew uses a hybrid architecture to ensure IDE "Go to Definition" jumps
directly to concrete implementation methods while still enforcing API contracts
when multiple implementations exist:

* **Single-Implementation Affordance (Default)**: Place the concrete class
  directly in `<affordance_name>.py` inheriting from `affordance.Affordance`. Do
  **not** create an intermediate ABC or a separate
  `<affordance_name>_using_ffx.py` file when only a single implementation
  exists.
* **Multi-Implementation Affordance (2+ implementations)**: Only when an
  affordance has multiple concrete implementations (e.g.,
  `hello_fuchsia_using_ffx.py` and `hello_fuchsia_using_fc.py`, or
  `system_power_state_controller_using_starnix.py` and
  `system_power_state_controller_using_usb_cli.py`), define an Abstract Base
  Class named `<AffordanceName>` in `<affordance_name>.py` with
  `@abc.abstractmethod` methods (and an empty `__init__.py`), place each
  concrete implementation in `<affordance_name>_using_<impl>.py` inheriting from
  `<affordance_name>.<AffordanceName>`, define `class
  Implementation(enum.StrEnum)` in `<affordance_name>/types.py` when the
  implementation is selected via `honeydew_config`, and annotate
  `@properties.Affordance` on `FuchsiaDevice` as returning `->
  <affordance_name>.<AffordanceName>`.
* **Transitioning (1 ↔ 2 implementations)**:
  * **1 → 2**: Move the existing implementation from `<affordance_name>.py` to
    `<affordance_name>_using_<impl1>.py`, add
    `<affordance_name>_using_<impl2>.py`, replace `<affordance_name>.py` with
    the ABC `class <AffordanceName>` (keeping `__init__.py` empty), and if
    selectable via `honeydew_config`, add `class Implementation(enum.StrEnum)`
    in `types.py` and update `FuchsiaDevice.<affordance_name>` to read
    `config["affordances"]["<affordance_name>"]["implementation"]`. If the 1st
    implementation is in `honeydew` and the 2nd is in `ghoneydew`, also add
    `<affordance_name>.<AffordanceName>` to `_CROSS_REPO_ABCS` in `honeydew`'s
    `architecture_test.py`.
  * **2 → 1**: Delete the removed implementation file and its unit test, and
    consolidate the remaining concrete class `<AffordanceName>Using<Impl>` back
    into `<affordance_name>.py` as `class <AffordanceName>`.

A standard **single-implementation** FFX-based Honeydew affordance uses this
layout:

```
src/testing/end_to_end/honeydew/honeydew/affordances/<domain>/<affordance_name>/
├── __init__.py                # Empty (license header only)
├── <affordance_name>.py       # Concrete affordance class inheriting affordance.Affordance
├── errors.py                  # Domain-specific exceptions inheriting HoneydewError
├── types.py                   # (Optional) Data types, Enums, and Dataclasses
└── tests/
    ├── unit_tests/
    │   ├── BUILD.gn           # GN test target definition (python_host_test)
    │   └── <affordance_name>_test.py  # Isolated unit test suite mocking FFX transport
    └── functional_tests/
        ├── BUILD.gn           # GN Mobly E2E test target (python_mobly_test)
        └── test_<affordance_name>.py
```

A **multi-implementation** affordance (e.g., `hello_fuchsia`) uses this layout:

```
src/testing/end_to_end/honeydew/honeydew/affordances/<domain>/<affordance_name>/
├── __init__.py                         # Empty (license header only)
├── <affordance_name>.py                # ABC class <AffordanceName>(affordance.Affordance)
├── <affordance_name>_using_ffx.py      # Concrete class <AffordanceName>UsingFfx(<AffordanceName>)
├── <affordance_name>_using_fc.py       # Concrete class <AffordanceName>UsingFc(<AffordanceName>)
├── errors.py                           # Domain-specific exceptions inheriting HoneydewError
├── types.py                            # Includes Implementation(enum.StrEnum) for honeydew_config
└── tests/
    ├── unit_tests/
    │   ├── BUILD.gn                    # Defines python_host_test per implementation
    │   ├── <affordance_name>_using_ffx_test.py
    │   └── <affordance_name>_using_fc_test.py
    └── functional_tests/
        ├── BUILD.gn                    # Defines python_mobly_test per implementation via honeydew_config
        └── test_<affordance_name>.py
```

---

## 3. Step-by-Step Implementation Workflow

### Step 1: Define Custom Exceptions (`errors.py`)

Create a domain-specific exception hierarchy inheriting from
`honeydew.errors.HoneydewError`:

```python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Errors raised by MyFeature affordance."""

from honeydew import errors


class HoneydewMyFeatureError(errors.HoneydewError):
    """Base error raised by MyFeature affordance."""


class MyFeatureCommandError(HoneydewMyFeatureError):
    """Raised when an FFX command fails for MyFeature."""


class MyFeatureStateError(HoneydewMyFeatureError):
    """Raised when MyFeature is in an invalid state for the operation."""
```

---

### Step 2: Implement the FFX Affordance (`<name>.py`)

For a single-implementation affordance, define the concrete class directly in
`<name>.py` inheriting from `honeydew.affordances.affordance.Affordance`. (If
the affordance has 2+ implementations, define `class <Name>` with
`@abc.abstractmethod` in `<name>.py` and implement
`<Name>UsingFfx(<name>.<Name>)` in `<name>_using_ffx.py`.)

The implementation class must:

1.  Inherit from `affordance.Affordance` (or the affordance ABC if 2+
    implementations exist).
2.  Accept `device_name: str` and `ffx: ffx_transport.FFX` in `__init__()`.
3.  Accept optional lifecycle registries: `reboot_affordance` or
    `fuchsia_device_close` if cleanup is needed.
4.  Call `self.verify_supported()` inside `__init__()`.
5.  Catch `ffx_errors.FfxCommandError` and wrap it into your domain-specific
    exception.

#### Common FFX Execution Patterns:

#### Pattern A: Direct Subtool Commands (e.g. `ffx session start`)

```python
def start(self) -> None:
    try:
        self._ffx.run(
            ["session", "start"],
            machine=ffx_types.MachineFormat.RAW,
        )
    except ffx_errors.FfxCommandError as err:
        raise MyFeatureCommandError(f"Failed to start session: {err}") from err
```

#### Pattern B: Injected Tools via Component Explore (e.g. `fake-battery-cli`)

```python
_TOOL_URL: str = "fuchsia-pkg://fuchsia.com/my_tool_pkg"

def _run_tool_command(self, cmd_args: list[str]) -> str:
    command_str = " ".join(cmd_args)
    try:
        return self._ffx.run(
            [
                "component",
                "explore",
                self._moniker,
                "--tools",
                _TOOL_URL,
                "-c",
                command_str,
            ],
            machine=ffx_types.MachineFormat.RAW,
        )
    except ffx_errors.FfxCommandError as err:
        raise MyFeatureCommandError(f"Command '{command_str}' failed: {err}") from err
```

#### Pattern C: Structured JSON Output (`--machine json`)

```python
import json

def get_status_info(self) -> dict[str, Any]:
    try:
        raw_json = self._ffx.run(
            ["my_tool", "status"],
            machine=ffx_types.MachineFormat.JSON,
        )
        return json.loads(raw_json)
    except (ffx_errors.FfxCommandError, json.JSONDecodeError) as err:
        raise MyFeatureCommandError(f"Failed to query status: {err}") from err
```

---

### Step 3: Capability Verification & Moniker Resolution

#### Capability Verification in `verify_supported()`
Check if required capabilities exist on the DUT:

```python
_REQUIRED_CAPABILITY = "fuchsia.hardware.power.battery.Service"

def verify_supported(self) -> None:
    """Verifies affordance support on the DUT."""
    try:
        output = self._ffx.run(
            ["component", "capability", _REQUIRED_CAPABILITY],
            machine=ffx_types.MachineFormat.RAW,
        )
        if _REQUIRED_CAPABILITY not in output:
            raise errors.NotSupportedError(
                f"Capability '{_REQUIRED_CAPABILITY}' not supported on {self._device_name}"
            )
        self._moniker = self._parse_moniker_from_capability_output(output)
    except ffx_errors.FfxCommandError as err:
        raise errors.NotSupportedError(
            f"Affordance not supported on {self._device_name}"
        ) from err
```

#### Moniker Resolution from Capability Output
Fuchsia's `ffx component capability <name>` formats output as: `` `<moniker>`
declared capability `<capability>` `` or `` `<moniker>` exposed capability
`<capability>` from ... ``

Cache the resolved moniker during `__init__` rather than querying FFX repeatedly
on every method invocation:

```python
def _parse_moniker_from_capability_output(self, output: str) -> str:
    """Parses the active component moniker from capability output."""
    for line in output.splitlines():
        line = line.strip()
        if (
            f"declared capability `{_REQUIRED_CAPABILITY}`" in line
            and "devfs_driver" not in line
        ):
            parts = line.split("`")
            if len(parts) >= 2 and parts[1] != _REQUIRED_CAPABILITY:
                return parts[1]
        if (
            f"exposed capability `{_REQUIRED_CAPABILITY}`" in line
            or f"exposed `{_REQUIRED_CAPABILITY}`" in line
        ):
            parts = line.split("`")
            if len(parts) >= 2 and parts[1] != _REQUIRED_CAPABILITY:
                return parts[1]
    return _DEFAULT_MONIKER
```

---

### Step 4: Argument Serialization & Validation

When mapping Python parameters to CLI flags:

1.  **Validate early**: Check ranges, valid bounds, and required parameter
    combinations in Python before executing host processes.
2.  **Convert Enums cleanly**: Format Python enum instances using
    `.name.lower()` or `.value`.
3.  **Omit None values**: Only append `--flag <value>` if the argument is
    provided, letting tool defaults take effect.

```python
cmd_args: list[str] = ["my-cli", "set"]

if level is not None:
    if not (0.0 <= level <= 100.0):
        raise ValueError(f"Level must be between 0.0 and 100.0, got {level}")
    cmd_args.extend(["--level", str(level)])

if status is not None:
    status_str = status.name.lower() if isinstance(status, ChargeStatus) else str(status).lower()
    cmd_args.extend(["--status", status_str])

if current_ua is not None:
    cmd_args.extend(["--current-ua", str(current_ua)])
```

---

### Step 5: Lifecycle & Cleanup Hooks

If the affordance modifies device state, spawns sessions, or produces artifacts:

* Accept `fuchsia_device_close: affordances_capable.FuchsiaDeviceClose` and
  register a teardown hook:

  ```python
  self._fuchsia_device_close = fuchsia_device_close
  self._fuchsia_device_close.register_for_on_device_close(self.cleanup)
  ```

* Accept `reboot_affordance: affordances_capable.RebootCapableDevice` and
  register a post-boot reset handler:

  ```python
  self._reboot_affordance = reboot_affordance
  self._reboot_affordance.register_for_on_device_boot(self._on_reboot)
  ```

---

### Step 6: Unit Testing (`tests/unit_tests.py`)

Unit tests should test the affordance in isolation without requiring a live
Fuchsia device:

* Mock `ffx_transport.FFX` using `mock.MagicMock(spec=ffx_transport.FFX,
  autospec=True)`.
* Verify exact CLI commands using `assert_called_with`.
* Test capability verification, moniker parsing, error propagation, and argument
  validation.

#### Example `tests/BUILD.gn`:

```gn
import("//build/python/python_host_test.gni")

assert(is_host, "Python only supported in host toolchains.")

python_host_test("unit_tests") {
  main_source = "unit_tests.py"
  libraries = [ "//src/testing/end_to_end/honeydew" ]
  main_callable = "unittest.main"
  extra_args = [ "-v" ]
}
```

#### Example `tests/unit_tests.py`:

```python
import unittest
from unittest import mock
from honeydew import errors
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from honeydew.affordances.my_domain.my_feature import (
    errors as my_feature_errors,
)
from honeydew.affordances.my_domain.my_feature import (
    my_feature as my_feature_affordance,
)

_CAPABILITY_OUTPUT = (
    "`core/my-component` declared capability `fuchsia.example.Service`"
)

class MyFeatureTests(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        self.ffx_obj = mock.MagicMock(spec=ffx_transport.FFX, autospec=True)
        self.ffx_obj.run.return_value = _CAPABILITY_OUTPUT
        self.my_feature = my_feature_affordance.MyFeature(
            device_name="fuchsia-test-device",
            ffx=self.ffx_obj,
        )

    def test_verify_supported_failure(self) -> None:
        self.ffx_obj.run.return_value = ""
        with self.assertRaises(errors.NotSupportedError):
            my_feature_affordance.MyFeature(
                device_name="fuchsia-test-device",
                ffx=self.ffx_obj,
            )

    def test_trigger_action_success(self) -> None:
        self.ffx_obj.run.return_value = "Command success"
        res = self.my_feature.trigger_action(param_a="value1", param_b=42)
        self.assertEqual(res, "Command success")
        self.ffx_obj.run.assert_called_with(
            ["component", "explore", "core/my-component", "--tools", "fuchsia-pkg://...", "-c", "my-cli --param-a value1 --param-b 42"],
            machine=ffx_types.MachineFormat.RAW,
        )

    def test_trigger_action_error(self) -> None:
        self.ffx_obj.run.side_effect = ffx_errors.FfxCommandError("Command failed")
        with self.assertRaises(my_feature_errors.MyFeatureCommandError):
            self.my_feature.trigger_action(param_a="value1")
```

---

### Step 7: Honeydew Integration & Device Registration

1.  **`src/testing/end_to_end/honeydew/BUILD.gn`**:
    - Add all new `.py` source files (excluding `tests/`) to
      `python_library("honeydew_no_testonly")` `sources`.
    - Add unit test target to `group("unit_tests")`:

     ```gn
     "honeydew/affordances/<domain>/<name>/tests/unit_tests:tests",
     ```

    - Add functional test target to `group("functional_tests")`:

     ```gn
     "honeydew/affordances/<domain>/<name>/tests/functional_tests:tests",
     ```

2.  **`src/testing/end_to_end/honeydew/honeydew/fuchsia_device/fuchsia_device.py`**:
    - **Single-Implementation Affordance**: Import `<name>` and return the
      concrete `<name>_affordance.<Name>` directly:

     ```python
     from honeydew.affordances.<domain>.<name> import <name> as <name>_affordance

     @properties.Affordance
     def <name>(self) -> <name>_affordance.<Name>:
         """Returns a <Name> affordance object.

         Returns:
             <name>.<Name> object
         """
         return <name>_affordance.<Name>(
             device_name=self.device_name,
             ffx=self.ffx,
         )
     ```

    - **Multi-Implementation Affordance (Selected via `honeydew_config`)**:
      1.  Define `class Implementation(enum.StrEnum)` in `<name>/types.py`
          (e.g., `FUCHSIA_CONTROLLER = "fuchsia-controller"` and `FFX = "ffx"`).
      2.  Add a `_get_<name>_affordance_implementation()` helper on
          `FuchsiaDevice` that reads `common.read_from_dict(self._config,
          key_path=("affordances", "<name>", "implementation"),
          should_exist=should_exist)` and converts it to
          `<name>_types.Implementation` (raising `errors.ConfigError` on invalid
          values).
      3.  Annotate `@properties.Affordance def <name>(self) ->
          <name>_affordance.<Name>:` with the ABC and return the selected
          concrete implementation:

     ```python
     @properties.Affordance
     def <name>(self) -> <name>_affordance.<Name>:
         """Returns a <Name> affordance object.

         Returns:
             <name>_using_fc.<Name>UsingFc | <name>_using_ffx.<Name>UsingFfx object
         """
         impl = self._get_<name>_affordance_implementation()
         if impl == <name>_types.Implementation.FFX:
             return <name>_using_ffx.<Name>UsingFfx(
                 device_name=self.device_name,
                 ffx=self.ffx,
             )
         return <name>_using_fc.<Name>UsingFc(
             device_name=self.device_name,
             fuchsia_controller=self.fuchsia_controller,
         )
     ```

      4.  In `tests/functional_tests/BUILD.gn`, define a `python_mobly_test`
          target per implementation with `honeydew_config`:

     ```gn
     honeydew_config = {
       affordances = {
         <name> = {
           implementation = "ffx" # or "fuchsia-controller"
         }
       }
     }
     ```

3.  **`src/testing/end_to_end/honeydew/honeydew/fuchsia_device/tests/unit_tests/fuchsia_device_test.py`**:
    - Add `test_<name>` verifying `self.fd_fc_obj.<name>` returns the expected
      concrete class.
    - For multi-implementation affordances selected via `honeydew_config`, also
      add `test_<name>_implementation_selection` (testing `config=None`, missing
      key, `"ffx"`, and `"fuchsia-controller"`) and `test_<name>_invalid_config`
      (verifying `errors.ConfigError` is raised).

---

## 4. In-Tree Reference Implementations

Refer to the following in-tree implementations for concrete patterns:

* **Injected CLI Tool via `component explore`**:
  [`src/testing/end_to_end/honeydew/honeydew/affordances/drivers/fake_battery/`](//src/testing/end_to_end/honeydew/honeydew/affordances/drivers/fake_battery/)
  * Shows capability verification, dynamic moniker caching, argument
    serialization, and enum formatting.
* **Session Lifecycle & Cleanup**:
  [`src/testing/end_to_end/honeydew/honeydew/affordances/session/`](//src/testing/end_to_end/honeydew/honeydew/affordances/session/)
  * Shows subtool wrapper (`ffx session`), polling readiness, and
    `fuchsia_device_close` cleanup.
* **Screenshot & File Transfer**:
  [`src/testing/end_to_end/honeydew/honeydew/affordances/ui/screenshot/`](//src/testing/end_to_end/honeydew/honeydew/affordances/ui/screenshot/)
  * Shows `ffx target screenshot` host extraction.
* **Minimal Single-Implementation FFX Affordance**:
  [`src/testing/end_to_end/honeydew/honeydew/affordances/hello_world/`](//src/testing/end_to_end/honeydew/honeydew/affordances/hello_world/)
  * Canonical baseline for single-implementation Honeydew affordances.
* **Minimal Multi-Implementation Affordance (`FFX` + `Fuchsia-Controller`)**:
  [`src/testing/end_to_end/honeydew/honeydew/affordances/hello_fuchsia/`](//src/testing/end_to_end/honeydew/honeydew/affordances/hello_fuchsia/)
  * Canonical baseline for multi-implementation Honeydew affordances (`ABC` +
    multiple implementations selected via `honeydew_config`).
