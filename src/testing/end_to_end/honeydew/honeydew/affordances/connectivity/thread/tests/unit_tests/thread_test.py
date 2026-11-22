# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for Thread affordance."""

from __future__ import annotations

import unittest
from unittest import mock

import fidl_fuchsia_lowpan_device as f_lowpan_device
import fidl_fuchsia_lowpan_thread as f_lowpan_thread
from fidl import GlobalHandleWaker
from fuchsia_controller_py import FcTransportStatus
from honeydew import affordances_capable, errors
from honeydew.affordances.connectivity.thread import thread as thread_affordance
from honeydew.affordances.connectivity.thread.errors import (
    HoneydewThreadError,
    ThreadDeviceNotFoundError,
)
from honeydew.affordances.connectivity.thread.types import ThreadRole
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)


class ThreadTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for Thread affordance."""

    async def asyncSetUp(self) -> None:
        await super().asyncSetUp()
        GlobalHandleWaker()._reset_for_testing()

        self.ffx_obj = mock.MagicMock(spec=ffx_transport.FFX, autospec=True)
        self.ffx_obj.run.return_value = "fuchsia.factory.lowpan.FactoryLookup"

        self.reboot_affordance_obj = mock.MagicMock(
            spec=affordances_capable.RebootCapableDevice,
            autospec=True,
        )
        self.fuchsia_device_close_obj = mock.MagicMock(
            spec=affordances_capable.FuchsiaDeviceClose,
            autospec=True,
        )
        self.fc_transport_obj = mock.MagicMock(
            spec=fc_transport.FuchsiaController,
            autospec=True,
        )

        self.thread = thread_affordance.Thread(
            device_name="fuchsia-test-device",
            ffx=self.ffx_obj,
            fuchsia_controller=self.fc_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
            fuchsia_device_close=self.fuchsia_device_close_obj,
        )

    async def test_verify_supported_failure(self) -> None:
        self.ffx_obj.run.return_value = ""
        with self.assertRaises(errors.NotSupportedError):
            thread_affordance.Thread(
                device_name="fuchsia-test-device",
                ffx=self.ffx_obj,
                fuchsia_controller=self.fc_transport_obj,
                reboot_affordance=self.reboot_affordance_obj,
                fuchsia_device_close=self.fuchsia_device_close_obj,
            )

    @mock.patch(
        "honeydew.affordances.connectivity.thread.thread.f_factory_lowpan",
        autospec=True,
    )
    async def test_make_ready_success(
        self, mock_f_factory_lowpan: mock.MagicMock
    ) -> None:
        mock_client_channel = mock.MagicMock()
        mock_server_channel = mock.MagicMock()
        self.fc_transport_obj.channel_create.return_value = (
            mock_client_channel,
            mock_server_channel,
        )
        mock_client_socket = mock.MagicMock()
        mock_server_socket = mock.MagicMock()
        self.fc_transport_obj.socket_create.return_value = (
            mock_client_socket,
            mock_server_socket,
        )
        mock_factory_device = mock.AsyncMock()
        mock_f_factory_lowpan.FactoryDeviceClient.return_value = (
            mock_factory_device
        )

        await self.thread.make_ready()

        self.fc_transport_obj.connect_device_proxy.assert_called_once()
        mock_f_factory_lowpan.FactoryLookupClient.return_value.lookup.assert_called_once_with(
            name="lowpan0",
            device_factory=mock_server_channel.take.return_value,
        )
        mock_factory_device.setup_ot_cli.assert_awaited_once_with(
            server_socket=mock_server_socket.take.return_value
        )
        self.assertIsNotNone(self.thread._factory_device_proxy)
        self.assertIsNotNone(self.thread._ot_cli_socket)

    async def test_make_ready_failure(self) -> None:
        self.fc_transport_obj.connect_device_proxy.side_effect = RuntimeError(
            "connection failed"
        )
        with self.assertRaises(ThreadDeviceNotFoundError):
            await self.thread.make_ready()

    @mock.patch(
        "honeydew.affordances.connectivity.thread.thread.asyncio.sleep",
        new_callable=mock.AsyncMock,
    )
    @mock.patch(
        "honeydew.affordances.connectivity.thread.thread.f_factory_lowpan",
        autospec=True,
    )
    async def test_make_ready_retries_on_transient_peer_closed(
        self,
        mock_f_factory_lowpan: mock.MagicMock,
        mock_sleep: mock.AsyncMock,
    ) -> None:
        self.fc_transport_obj.channel_create.return_value = (
            mock.MagicMock(),
            mock.MagicMock(),
        )
        self.fc_transport_obj.socket_create.return_value = (
            mock.MagicMock(),
            mock.MagicMock(),
        )
        mock_factory_device = mock.AsyncMock()
        mock_factory_device.setup_ot_cli.side_effect = [
            FcTransportStatus(FcTransportStatus.FC_ERR_CHANNEL_WRITE),
            None,
        ]
        mock_f_factory_lowpan.FactoryDeviceClient.return_value = (
            mock_factory_device
        )

        await self.thread.make_ready()
        self.assertEqual(mock_factory_device.setup_ot_cli.await_count, 2)
        mock_sleep.assert_awaited_once()

    async def test_get_role_via_device_fidl(self) -> None:
        self.thread._ready = True
        mock_device_proxy = mock.AsyncMock()
        mock_device_proxy.watch_device_state.side_effect = [
            f_lowpan_device.DeviceWatchDeviceStateResponse(
                device_combined_state=f_lowpan_device.DeviceState(
                    role=f_lowpan_device.Role.LEADER,
                    connectivity_state=f_lowpan_device.ConnectivityState.ATTACHED,
                )
            ),
            f_lowpan_device.DeviceWatchDeviceStateResponse(
                device_combined_state=f_lowpan_device.DeviceState(
                    role=f_lowpan_device.Role.DETACHED,
                    connectivity_state=f_lowpan_device.ConnectivityState.INACTIVE,
                )
            ),
        ]
        with mock.patch.object(
            self.thread, "_connect_device_proxy", return_value=mock_device_proxy
        ):
            self.assertEqual(await self.thread.get_role(), ThreadRole.LEADER)
            self.assertEqual(await self.thread.get_role(), ThreadRole.DISABLED)

    async def test_form_and_leave_network_via_fidl(self) -> None:
        self.thread._ready = True
        mock_device_proxy = mock.AsyncMock()
        mock_dataset_proxy = mock.AsyncMock()
        mock_dataset_proxy.get_active_tlvs.return_value = (
            f_lowpan_thread.DatasetGetActiveTlvsResponse(
                dataset=list(bytes.fromhex("0e080000000000010000"))
            )
        )
        self.thread._device_proxy = mock_device_proxy
        self.thread._dataset_proxy = mock_dataset_proxy

        tlvs = await self.thread.form_network(
            channel=15,
            network_name="TestMesh",
            pan_id=0x1234,
            ext_pan_id="0011223344556677",
            network_key="00112233445566778899aabbccddeeff",
        )
        self.assertEqual(tlvs, "0e080000000000010000")
        mock_device_proxy.provision_network.assert_awaited_once()
        mock_device_proxy.set_active.assert_awaited_once_with(active=True)

        await self.thread.leave_network()
        mock_device_proxy.leave_network.assert_awaited_once()
        mock_device_proxy.set_active.assert_awaited_with(active=False)

    async def test_dataset_fidl_methods(self) -> None:
        self.thread._ready = True
        mock_dataset_proxy = mock.AsyncMock()
        mock_dataset_proxy.attach_all_nodes_to.return_value = (
            f_lowpan_thread.DatasetAttachAllNodesToResponse(delay_ms=30000)
        )
        self.thread._dataset_proxy = mock_dataset_proxy

        await self.thread.set_active_dataset_tlvs("0e080000000000010000")
        mock_dataset_proxy.set_active_tlvs.assert_awaited_once_with(
            dataset=list(bytes.fromhex("0e080000000000010000"))
        )

        delay = await self.thread.attach_all_nodes_to("0e080000000000010000")
        self.assertEqual(delay, 30000)

    async def test_long_multi_chunk_response_exceeding_2000_bytes(self) -> None:
        mock_socket = mock.MagicMock()
        chunk_1 = ("fd00:1234::/64 paros med 1800\r\n" * 50).encode("utf-8")
        chunk_2 = ("fd00:5678::/64 paros med 1800\r\n" * 50).encode("utf-8")
        chunk_3 = b"Done\r\n"
        mock_socket.read = mock.AsyncMock(
            side_effect=[chunk_1, chunk_2, chunk_3]
        )
        self.thread._ot_cli_socket = mock_socket
        self.thread._ready = True

        output = await self.thread.run_ot_cli("netdata show")
        lines = output.splitlines()
        self.assertEqual(len(lines), 100)
        self.assertGreater(len(output), 2500)

    async def test_transport_error_raises_honeydew_thread_error(self) -> None:
        self.thread._ready = True
        mock_device_proxy = mock.AsyncMock()
        mock_device_proxy.watch_device_state.side_effect = FcTransportStatus(
            FcTransportStatus.FC_ERR_CHANNEL_WRITE
        )
        with mock.patch.object(
            self.thread, "_connect_device_proxy", return_value=mock_device_proxy
        ):
            with self.assertRaises(HoneydewThreadError):
                await self.thread.get_role()

        mock_socket = mock.MagicMock()
        mock_socket.write.side_effect = FcTransportStatus(
            FcTransportStatus.FC_ERR_CHANNEL_WRITE
        )
        self.thread._ot_cli_socket = mock_socket
        self.thread._ready = True
        with self.assertRaises(HoneydewThreadError):
            await self.thread.run_ot_cli("state")


if __name__ == "__main__":
    unittest.main()
