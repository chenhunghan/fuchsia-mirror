# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.auxiliary_devices.config."""

import abc
import types
import unittest
from collections.abc import Callable, Mapping
from typing import Any
from unittest import mock

from honeydew.auxiliary_devices import config as aux_config
from parameterized import param, parameterized


class _CustomAuxError(Exception):
    """Custom error class for testing aux_config error wrapping."""


class _AbstractFakeDevice(abc.ABC):
    """Abstract base class for fake devices."""

    @classmethod
    def from_config(
        cls, device_config: Mapping[str, object] | None = None
    ) -> "_AbstractFakeDevice | None":
        """Resolves a configured `_AbstractFakeDevice` via config helper."""
        return aux_config.create_device_from_config(
            device_config=device_config,
            device_key="fake_device",
            expected_type=cls,
            error_cls=_CustomAuxError,
        )

    @abc.abstractmethod
    def action(self) -> None:
        """Abstract method to ensure subclasses implement the contract."""


class _StillAbstractFakeDevice(_AbstractFakeDevice):
    """Subclass that fails to implement abstract method."""


class _FakeDevice(_AbstractFakeDevice):
    """Concrete fake device taking explicit hardware arguments."""

    def __init__(self, host: str, port: int) -> None:
        self.host = host
        self.port = port

    def action(self) -> None:
        pass


class _NoArgsDevice(_AbstractFakeDevice):
    """Concrete fake device taking no constructor arguments."""

    def __init__(self) -> None:
        self.initialized = True

    def action(self) -> None:
        pass


class _UnrelatedDevice:
    """Class that does not inherit from _AbstractFakeDevice."""


def _not_a_class() -> None:
    """Function used to verify non-type targets are rejected."""


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_arg: param
) -> str:
    """Custom test name function for parameterized tests."""
    test_func_name: str = testcase_func.__name__
    params_dict: dict[str, object] = param_arg.args[0]
    test_label: str = parameterized.to_safe_name(str(params_dict["label"]))
    return f"{test_func_name}_with_{test_label}"


class AuxDeviceConfigTests(unittest.TestCase):
    """Unit tests for honeydew.auxiliary_devices.config."""

    def test_create_device_from_config_success(self) -> None:
        """Verifies create_device_from_config instantiates the configured class."""
        fake_module = types.SimpleNamespace(_FakeDevice=_FakeDevice)
        with mock.patch("importlib.import_module", return_value=fake_module):
            device = _AbstractFakeDevice.from_config(
                device_config={
                    "auxiliary_devices": {
                        "fake_device": {
                            "hardware": {"host": "10.0.0.1", "port": 2},
                            "implementation": {
                                "module": "fake.module",
                                "class": "_FakeDevice",
                            },
                        },
                    },
                },
            )

        self.assertIsInstance(device, _FakeDevice)
        assert isinstance(device, _FakeDevice)
        self.assertEqual(device.host, "10.0.0.1")
        self.assertEqual(device.port, 2)

    @parameterized.expand(
        [
            ({"label": "empty_hardware_dict", "hw_entry": {"hardware": {}}},),
            ({"label": "none_hardware", "hw_entry": {"hardware": None}},),
            ({"label": "omitted_hardware", "hw_entry": {}},),
        ],
        name_func=_custom_test_name_func,
    )
    def test_create_device_from_config_with_empty_or_omitted_hardware(
        self, parameterized_dict: dict[str, Any]
    ) -> None:
        """Verifies empty, None, or omitted `hardware` passes zero kwargs."""
        fake_module = types.SimpleNamespace(_NoArgsDevice=_NoArgsDevice)
        aux_entry: dict[str, Any] = {
            **parameterized_dict["hw_entry"],
            "implementation": {
                "module": "fake.module",
                "class": "_NoArgsDevice",
            },
        }
        with mock.patch("importlib.import_module", return_value=fake_module):
            device = _AbstractFakeDevice.from_config(
                device_config={"auxiliary_devices": {"fake_device": aux_entry}},
            )

        self.assertIsInstance(device, _NoArgsDevice)
        assert isinstance(device, _NoArgsDevice)
        self.assertTrue(device.initialized)

    def test_create_device_from_config_no_top_level_key_collision(self) -> None:
        """Verifies top-level keys are ignored when auxiliary_devices.<key> is absent."""
        with mock.patch("importlib.import_module") as mock_import:
            device = _AbstractFakeDevice.from_config(
                device_config={
                    "hardware": {"host": "10.0.0.1", "port": 2},
                    "implementation": {
                        "module": "fake.module",
                        "class": "_FakeDevice",
                    },
                    "outlet": 1,
                    "port": 2,
                    "auxiliary_devices": {"other_device": {}},
                },
            )

        self.assertIsNone(device)
        mock_import.assert_not_called()

    @parameterized.expand(
        [
            ({"label": "none_config", "device_config": None},),
            ({"label": "empty_config", "device_config": {}},),
            (
                {
                    "label": "none_auxiliary_devices",
                    "device_config": {"auxiliary_devices": None},
                },
            ),
            (
                {
                    "label": "missing_device_key",
                    "device_config": {"auxiliary_devices": {}},
                },
            ),
            (
                {
                    "label": "none_device_entry",
                    "device_config": {
                        "auxiliary_devices": {"fake_device": None}
                    },
                },
            ),
            (
                {
                    "label": "empty_device_entry",
                    "device_config": {"auxiliary_devices": {"fake_device": {}}},
                },
            ),
            (
                {
                    "label": "legacy_flat_keys_ignored",
                    "device_config": {
                        "power_switch_hw": {"host": "10.0.0.1", "port": 2},
                        "power_switch_impl": {
                            "module": "fake.module",
                            "class": "_FakeDevice",
                        },
                    },
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    def test_create_device_from_config_returns_none(
        self, parameterized_dict: dict[str, Any]
    ) -> None:
        """Verifies unconfigured or non-matching configs return None."""
        device_config = parameterized_dict["device_config"]
        with mock.patch("importlib.import_module") as mock_import:
            device = _AbstractFakeDevice.from_config(
                device_config=device_config,
            )

        self.assertIsNone(device)
        mock_import.assert_not_called()

    @parameterized.expand(
        [
            (
                {
                    "label": "non_mapping_aux_entry",
                    "aux_entry": "invalid",
                },
            ),
            (
                {
                    "label": "missing_implementation",
                    "aux_entry": {"hardware": {}},
                },
            ),
            (
                {
                    "label": "non_mapping_implementation",
                    "aux_entry": {"hardware": {}, "implementation": "invalid"},
                },
            ),
            (
                {
                    "label": "empty_implementation",
                    "aux_entry": {"hardware": {}, "implementation": {}},
                },
            ),
            (
                {
                    "label": "missing_module_key",
                    "aux_entry": {
                        "hardware": {},
                        "implementation": {"class": "_NoArgsDevice"},
                    },
                },
            ),
            (
                {
                    "label": "missing_class_key",
                    "aux_entry": {
                        "hardware": {},
                        "implementation": {"module": "fake.module"},
                    },
                },
            ),
            (
                {
                    "label": "empty_module_string",
                    "aux_entry": {
                        "hardware": {},
                        "implementation": {
                            "module": "",
                            "class": "_NoArgsDevice",
                        },
                    },
                },
            ),
            (
                {
                    "label": "non_mapping_hardware",
                    "aux_entry": {
                        "hardware": ["invalid"],
                        "implementation": {
                            "module": "fake.module",
                            "class": "_NoArgsDevice",
                        },
                    },
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    def test_create_device_from_config_invalid_config_raises_error_cls(
        self, parameterized_dict: dict[str, Any]
    ) -> None:
        """Verifies malformed auxiliary device configs raise error_cls."""
        with self.assertRaises(_CustomAuxError):
            _AbstractFakeDevice.from_config(
                device_config={
                    "auxiliary_devices": {
                        "fake_device": parameterized_dict["aux_entry"]
                    }
                },
            )

    def test_create_device_from_config_wraps_import_error(self) -> None:
        """Verifies ImportError during module load is wrapped in error_cls."""
        with mock.patch(
            "importlib.import_module",
            side_effect=ImportError("no such module"),
        ):
            with self.assertRaisesRegex(_CustomAuxError, "no such module"):
                _AbstractFakeDevice.from_config(
                    device_config={
                        "auxiliary_devices": {
                            "fake_device": {
                                "hardware": {},
                                "implementation": {
                                    "module": "missing.module",
                                    "class": "_NoArgsDevice",
                                },
                            },
                        },
                    },
                )

    def test_create_device_from_config_wraps_attribute_error(self) -> None:
        """Verifies missing class AttributeError is wrapped in error_cls."""
        fake_module = types.SimpleNamespace()
        with mock.patch("importlib.import_module", return_value=fake_module):
            with self.assertRaises(_CustomAuxError):
                _AbstractFakeDevice.from_config(
                    device_config={
                        "auxiliary_devices": {
                            "fake_device": {
                                "hardware": {},
                                "implementation": {
                                    "module": "fake.module",
                                    "class": "MissingClass",
                                },
                            },
                        },
                    },
                )

    @parameterized.expand(
        [
            (
                {
                    "label": "not_a_type",
                    "attr_name": "_not_a_class",
                    "attr_value": _not_a_class,
                },
            ),
            (
                {
                    "label": "unrelated_class",
                    "attr_name": "_UnrelatedDevice",
                    "attr_value": _UnrelatedDevice,
                },
            ),
            (
                {
                    "label": "abstract_base_class",
                    "attr_name": "_AbstractFakeDevice",
                    "attr_value": _AbstractFakeDevice,
                },
            ),
            (
                {
                    "label": "incomplete_abstract_subclass",
                    "attr_name": "_StillAbstractFakeDevice",
                    "attr_value": _StillAbstractFakeDevice,
                },
            ),
        ],
        name_func=_custom_test_name_func,
    )
    def test_create_device_from_config_rejects_invalid_implementation_type(
        self, parameterized_dict: dict[str, Any]
    ) -> None:
        """Verifies non-types, non-subclasses, and abstract classes raise error_cls."""
        attr_name = parameterized_dict["attr_name"]
        fake_module = types.SimpleNamespace(
            **{attr_name: parameterized_dict["attr_value"]}
        )
        with mock.patch("importlib.import_module", return_value=fake_module):
            with self.assertRaisesRegex(_CustomAuxError, "concrete subclass"):
                _AbstractFakeDevice.from_config(
                    device_config={
                        "auxiliary_devices": {
                            "fake_device": {
                                "hardware": {},
                                "implementation": {
                                    "module": "fake.module",
                                    "class": attr_name,
                                },
                            },
                        },
                    },
                )

    def test_create_device_from_config_wraps_instantiation_type_error(
        self,
    ) -> None:
        """Verifies missing/invalid constructor kwargs raise error_cls."""
        fake_module = types.SimpleNamespace(_FakeDevice=_FakeDevice)
        with mock.patch("importlib.import_module", return_value=fake_module):
            with self.assertRaises(_CustomAuxError):
                _AbstractFakeDevice.from_config(
                    device_config={
                        "auxiliary_devices": {
                            "fake_device": {
                                "hardware": {"host": "10.0.0.1"},
                                "implementation": {
                                    "module": "fake.module",
                                    "class": "_FakeDevice",
                                },
                            },
                        },
                    },
                )
