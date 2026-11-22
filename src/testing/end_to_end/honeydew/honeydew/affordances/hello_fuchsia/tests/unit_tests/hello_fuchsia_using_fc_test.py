# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for hello_fuchsia_using_fc.py."""

import unittest
from collections.abc import Callable
from unittest import mock

import fidl_fuchsia_developer_remotecontrol as fd_remotecontrol
from honeydew import errors
from honeydew.affordances.hello_fuchsia import errors as hello_fuchsia_errors
from honeydew.affordances.hello_fuchsia import hello_fuchsia_using_fc
from honeydew.transports.fuchsia_controller import errors as fc_errors
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from parameterized import param, parameterized

_TARGET_NAME: str = "fuchsia-emulator"


def _custom_test_name_func(
    testcase_func: Callable[..., None], _: str, param_obj: param
) -> str:
    """Custom test name function method."""
    test_func_name: str = testcase_func.__name__
    test_label: str = parameterized.to_safe_name(param_obj.kwargs["label"])
    return f"{test_func_name}_with_{test_label}"


class HelloFuchsiaUsingFcTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for hello_fuchsia_using_fc.py."""

    def setUp(self) -> None:
        super().setUp()

        self.fc_obj = mock.MagicMock(spec=fc_transport.FuchsiaController)
        self.hello_fuchsia_obj = hello_fuchsia_using_fc.HelloFuchsiaUsingFc(
            device_name=_TARGET_NAME, fuchsia_controller=self.fc_obj
        )

    def test_verify_supported_not_supported_error(self) -> None:
        """Test for HelloFuchsiaUsingFc.verify_supported() raising NotSupportedError."""
        self.fc_obj.check_connection.side_effect = (
            fc_errors.FuchsiaControllerConnectionError("Connection failed")
        )
        with self.assertRaises(errors.NotSupportedError):
            hello_fuchsia_using_fc.HelloFuchsiaUsingFc(
                device_name=_TARGET_NAME, fuchsia_controller=self.fc_obj
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
    @mock.patch.object(
        fd_remotecontrol,
        "RemoteControlClient",
        autospec=True,
    )
    async def test_greeting(
        self,
        mock_rcs_client: mock.Mock,
        label: str,  # pylint: disable=unused-argument
        name: str | None,
        expected_greeting: str,
    ) -> None:
        """Test for HelloFuchsiaUsingFc.greeting() method."""
        mock_rcs = mock_rcs_client.return_value
        mock_resp = mock.MagicMock()
        mock_resp.unwrap.return_value = (
            fd_remotecontrol.RemoteControlIdentifyHostResponse(
                response=fd_remotecontrol.IdentifyHostResponse(
                    nodename=_TARGET_NAME
                )
            )
        )
        mock_rcs.identify_host = mock.AsyncMock(return_value=mock_resp)

        self.assertEqual(
            await self.hello_fuchsia_obj.greeting(name=name), expected_greeting
        )
        self.fc_obj.connect_device_proxy.assert_called_once()

    @mock.patch.object(
        fd_remotecontrol,
        "RemoteControlClient",
        autospec=True,
    )
    async def test_greeting_error(
        self,
        mock_rcs_client: mock.Mock,
    ) -> None:
        """Test for HelloFuchsiaUsingFc.greeting() raising HelloFuchsiaAffordanceError."""
        mock_rcs = mock_rcs_client.return_value
        mock_resp = mock.MagicMock()
        mock_resp.unwrap.return_value = (
            fd_remotecontrol.RemoteControlIdentifyHostResponse(
                response=fd_remotecontrol.IdentifyHostResponse(
                    nodename=_TARGET_NAME
                )
            )
        )
        mock_rcs.identify_host = mock.AsyncMock(return_value=mock_resp)

        with self.assertRaises(
            hello_fuchsia_errors.HelloFuchsiaAffordanceError
        ):
            await self.hello_fuchsia_obj.greeting(name=_TARGET_NAME)

    @mock.patch.object(
        fd_remotecontrol,
        "RemoteControlClient",
        autospec=True,
    )
    async def test_greeting_missing_nodename_error(
        self,
        mock_rcs_client: mock.Mock,
    ) -> None:
        """Test for HelloFuchsiaUsingFc.greeting() when nodename is missing."""
        mock_rcs = mock_rcs_client.return_value
        mock_resp = mock.MagicMock()
        mock_resp.unwrap.return_value = (
            fd_remotecontrol.RemoteControlIdentifyHostResponse(
                response=fd_remotecontrol.IdentifyHostResponse(nodename=None)
            )
        )
        mock_rcs.identify_host = mock.AsyncMock(return_value=mock_resp)

        with self.assertRaisesRegex(
            hello_fuchsia_errors.HelloFuchsiaAffordanceError,
            "Nodename not populated in IdentifyHost response",
        ):
            await self.hello_fuchsia_obj.greeting()

    async def test_greeting_fidl_error(self) -> None:
        """Test for HelloFuchsiaUsingFc.greeting() when FIDL connection fails."""
        self.fc_obj.connect_device_proxy.side_effect = (
            fc_errors.FuchsiaControllerError("FIDL error")
        )
        with self.assertRaises(
            hello_fuchsia_errors.HelloFuchsiaAffordanceError
        ):
            await self.hello_fuchsia_obj.greeting()


if __name__ == "__main__":
    unittest.main()
