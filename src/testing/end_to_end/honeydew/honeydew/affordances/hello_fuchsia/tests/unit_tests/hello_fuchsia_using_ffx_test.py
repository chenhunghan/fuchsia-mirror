# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for hello_fuchsia_using_ffx.py."""

import unittest
from collections.abc import Callable
from unittest import mock

from honeydew import errors
from honeydew.affordances.hello_fuchsia import errors as hello_fuchsia_errors
from honeydew.affordances.hello_fuchsia import hello_fuchsia_using_ffx
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx as ffx_transport
from parameterized import param, parameterized

_TARGET_NAME: str = "fuchsia-emulator"


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_obj: param
) -> str:
    """Custom test name function method."""
    test_func_name: str = testcase_func.__name__
    test_label: str = parameterized.to_safe_name(param_obj.kwargs["label"])
    return f"{test_func_name}_with_{test_label}"


class HelloFuchsiaUsingFfxTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for hello_fuchsia_using_ffx.py."""

    def setUp(self) -> None:
        super().setUp()

        self.ffx_obj = mock.MagicMock(spec=ffx_transport.FFX)
        self.hello_fuchsia_obj = hello_fuchsia_using_ffx.HelloFuchsiaUsingFfx(
            device_name=_TARGET_NAME, ffx=self.ffx_obj
        )

    def test_verify_supported_not_supported_error(self) -> None:
        """Test for HelloFuchsiaUsingFfx.verify_supported() raising NotSupportedError."""
        self.ffx_obj.check_connection.side_effect = (
            ffx_errors.FfxConnectionError("Connection failed")
        )
        with self.assertRaises(errors.NotSupportedError):
            hello_fuchsia_using_ffx.HelloFuchsiaUsingFfx(
                device_name=_TARGET_NAME, ffx=self.ffx_obj
            )

    @parameterized.expand(
        [
            param(
                label="name_arg_not_set",
                name=None,
                expected_greeting=f"Hello Fuchsia, {_TARGET_NAME}!",
            ),
            param(
                label="name_arg_set",
                name="starnix",
                expected_greeting="Hello Fuchsia, starnix!",
            ),
        ],
        name_func=_custom_test_name_func,
    )
    async def test_greeting(
        self,
        label: str,  # pylint: disable=unused-argument
        name: str | None,
        expected_greeting: str,
    ) -> None:
        """Test for HelloFuchsiaUsingFfx.greeting() method."""
        self.ffx_obj.get_target_name.return_value = _TARGET_NAME

        self.assertEqual(
            await self.hello_fuchsia_obj.greeting(name=name), expected_greeting
        )

    async def test_greeting_error(self) -> None:
        """Test for HelloFuchsiaUsingFfx.greeting() raising HelloFuchsiaAffordanceError."""
        self.ffx_obj.get_target_name.return_value = _TARGET_NAME
        with self.assertRaises(
            hello_fuchsia_errors.HelloFuchsiaAffordanceError
        ):
            await self.hello_fuchsia_obj.greeting(name=_TARGET_NAME)

    async def test_greeting_ffx_error(self) -> None:
        """Test for HelloFuchsiaUsingFfx.greeting() when FFX command fails."""
        self.ffx_obj.get_target_name.side_effect = ffx_errors.FfxCommandError(
            "FFX error"
        )
        with self.assertRaises(
            hello_fuchsia_errors.HelloFuchsiaAffordanceError
        ):
            await self.hello_fuchsia_obj.greeting()


if __name__ == "__main__":
    unittest.main()
