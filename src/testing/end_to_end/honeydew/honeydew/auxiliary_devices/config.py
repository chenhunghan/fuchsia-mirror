# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Helpers for resolving auxiliary device instances from testbed config."""

import importlib
import inspect
import typing
from collections.abc import Mapping

_DeviceT = typing.TypeVar("_DeviceT")


def create_device_from_config(
    *,
    device_config: Mapping[str, object] | None,
    device_key: str,
    expected_type: type[_DeviceT],
    error_cls: type[Exception],
) -> _DeviceT | None:
    """Returns an auxiliary device instance configured in `device_config`.

    Reads `device_config["auxiliary_devices"][device_key]` containing
    `hardware` (mapping of constructor keyword arguments, defaulting to `{}` if
    omitted) and `implementation` (`module` and `class`), validates that the
    resolved object is a concrete subclass of `expected_type`, and instantiates
    it with `**hardware`.

    Args:
        device_config: Honeydew or Mobly controller config dictionary for the
            device.
        device_key: Key under `device_config["auxiliary_devices"]` (for example,
            `"power_switch"` or `"usb_power_hub"`).
        expected_type: Expected base class or type of the instantiated device.
        error_cls: Exception type to raise on invalid configuration, import or
            lookup failures, or invalid implementation types.

    Returns:
        The configured instance, or `None` if no matching auxiliary device is
        configured.

    Raises:
        Exception: Instance of `error_cls` when configuration is invalid or the
            configured implementation cannot be loaded, validated, or
            instantiated.
    """
    if not isinstance(device_config, Mapping):
        return None
    aux_devices = device_config.get("auxiliary_devices")
    if not isinstance(aux_devices, Mapping):
        return None
    aux_config = aux_devices.get(device_key)
    if aux_config is None:
        return None
    if not isinstance(aux_config, Mapping):
        raise error_cls(
            f"Invalid 'auxiliary_devices.{device_key}' config: expected a "
            f"mapping, got {type(aux_config).__name__}."
        )
    if not aux_config or (
        aux_config.get("hardware") is None
        and aux_config.get("implementation") is None
    ):
        return None

    implementation = aux_config.get("implementation")
    if not isinstance(implementation, Mapping):
        raise error_cls(
            f"Invalid 'auxiliary_devices.{device_key}.implementation' config: "
            "expected a mapping with 'module' and 'class'."
        )
    module_name = implementation.get("module")
    class_name = implementation.get("class")
    if (
        not isinstance(module_name, str)
        or not module_name
        or not isinstance(class_name, str)
        or not class_name
    ):
        raise error_cls(
            f"Invalid 'auxiliary_devices.{device_key}.implementation' config: "
            "'module' and 'class' must be non-empty strings."
        )

    hardware = aux_config.get("hardware")
    if hardware is None:
        hardware = {}
    elif not isinstance(hardware, Mapping):
        raise error_cls(
            f"Invalid 'auxiliary_devices.{device_key}.hardware' config: "
            f"expected a mapping, got {type(hardware).__name__}."
        )

    try:
        module = importlib.import_module(module_name)
        resolved = getattr(module, class_name)
    except (ImportError, AttributeError) as err:
        raise error_cls(
            f"Failed to load {expected_type.__name__} implementation "
            f"'{module_name}.{class_name}': {err}"
        ) from err

    if (
        not isinstance(resolved, type)
        or not issubclass(resolved, expected_type)
        or inspect.isabstract(resolved)
    ):
        raise error_cls(
            f"Configured {expected_type.__name__} implementation "
            f"'{module_name}.{class_name}' must be a concrete subclass of "
            f"{expected_type.__name__}, got {resolved!r}."
        )

    try:
        return resolved(**typing.cast(Mapping[str, typing.Any], hardware))
    except TypeError as err:
        raise error_cls(
            f"Failed to instantiate {expected_type.__name__} implementation "
            f"'{module_name}.{class_name}': {err}"
        ) from err
