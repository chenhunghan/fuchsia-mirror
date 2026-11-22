# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for power_switch_using_synaccess.py."""

import unittest
from unittest import mock

from honeydew import errors
from honeydew.auxiliary_devices.power_switch import (
    power_switch,
    power_switch_using_synaccess,
)
from honeydew.utils import http_utils

_MOCK_PDU_HOST: str = "192.168.42.185"
_EXPECTED_AUTH_HEADERS: dict[str, str] = {
    "Authorization": "Basic YWRtaW46YWRtaW4=",
}


class PowerSwitchUsingSynaccessTests(unittest.TestCase):
    """Unit tests for power_switch_using_synaccess.py."""

    def setUp(self) -> None:
        super().setUp()
        self.pdu_obj: power_switch_using_synaccess.PowerSwitchUsingSynaccess = (
            power_switch_using_synaccess.PowerSwitchUsingSynaccess(
                host=_MOCK_PDU_HOST,
                outlet=1,
            )
        )

    def test_power_switch_using_synaccess_is_a_power_switch(self) -> None:
        """Test case to make sure PowerSwitchUsingSynaccess is a PowerSwitch."""
        self.assertIsInstance(self.pdu_obj, power_switch.PowerSwitch)

    def test_status_power_on_and_power_off(self) -> None:
        """Test case for status(), power_on(), and power_off() HTTP commands."""
        pdu_outlet_2 = power_switch_using_synaccess.PowerSwitchUsingSynaccess(
            host=_MOCK_PDU_HOST,
            outlet=2,
        )
        with mock.patch.object(
            http_utils,
            "send_http_request",
            side_effect=[
                "$A0,10",
                "$A0,10",
                "$A0",
                "$A0,11",
                "$A0",
                "$A0,01",
            ],
            autospec=True,
        ) as mock_send_http_request:
            self.assertEqual(
                self.pdu_obj._status(),  # pylint: disable=protected-access
                {"1": False, "2": True},
            )
            self.pdu_obj.power_on()
            pdu_outlet_2.power_off()

            mock_send_http_request.assert_any_call(
                f"http://{_MOCK_PDU_HOST}/cmd.cgi?%24A5",
                headers=_EXPECTED_AUTH_HEADERS,
                parse_json=False,
            )
            mock_send_http_request.assert_any_call(
                f"http://{_MOCK_PDU_HOST}/cmd.cgi?%24A3+1+1",
                headers=_EXPECTED_AUTH_HEADERS,
                parse_json=False,
            )
            mock_send_http_request.assert_any_call(
                f"http://{_MOCK_PDU_HOST}/cmd.cgi?%24A3+2+0",
                headers=_EXPECTED_AUTH_HEADERS,
                parse_json=False,
            )

    def test_from_config_missing_outlet_raises_power_switch_error(self) -> None:
        """Test PowerSwitch.from_config() raises PowerSwitchError when outlet is missing."""
        with self.assertRaises(power_switch.PowerSwitchError):
            power_switch.PowerSwitch.from_config(
                device_config={
                    "auxiliary_devices": {
                        "power_switch": {
                            "hardware": {"host": _MOCK_PDU_HOST},
                            "implementation": {
                                "module": (
                                    "honeydew.auxiliary_devices.power_switch."
                                    "power_switch_using_synaccess"
                                ),
                                "class": "PowerSwitchUsingSynaccess",
                            },
                        },
                    },
                }
            )

    def test_invalid_port_and_error_response(self) -> None:
        """Test case to ensure invalid ports and error responses raise PowerSwitchSynaccessError."""
        with self.assertRaises(
            power_switch_using_synaccess.PowerSwitchSynaccessError
        ):
            power_switch_using_synaccess.PowerSwitchUsingSynaccess(
                host=_MOCK_PDU_HOST,
                outlet=0,
            )

        with self.assertRaises(
            power_switch_using_synaccess.PowerSwitchSynaccessError
        ):
            power_switch_using_synaccess.PowerSwitchUsingSynaccess(
                host=_MOCK_PDU_HOST,
                outlet=3,
            )

        with mock.patch.object(
            http_utils,
            "send_http_request",
            return_value="$AF",
            autospec=True,
        ):
            with self.assertRaises(
                power_switch_using_synaccess.PowerSwitchSynaccessError
            ):
                self.pdu_obj.power_on()

        with mock.patch.object(
            http_utils,
            "send_http_request",
            side_effect=errors.HttpRequestError("HTTP request failed"),
            autospec=True,
        ):
            with self.assertRaises(
                power_switch_using_synaccess.PowerSwitchSynaccessError
            ):
                self.pdu_obj.power_off()


if __name__ == "__main__":
    unittest.main()
