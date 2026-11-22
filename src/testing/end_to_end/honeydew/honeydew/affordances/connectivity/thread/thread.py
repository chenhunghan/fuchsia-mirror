# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Thread affordance implementation using Fuchsia Controller."""

from __future__ import annotations

import asyncio
import codecs

import fidl_fuchsia_factory_lowpan as f_factory_lowpan
import fidl_fuchsia_lowpan_device as f_lowpan_device
import fidl_fuchsia_lowpan_thread as f_lowpan_thread
from fidl import AsyncSocket
from fuchsia_controller_py import FcTransportStatus, ZxStatus
from honeydew import affordances_capable, errors
from honeydew.affordances.affordance import (
    Affordance,
    AsyncLazyReady,
    ensure_ready,
)
from honeydew.affordances.connectivity.thread.errors import (
    HoneydewThreadError,
    ThreadDeviceNotFoundError,
)
from honeydew.affordances.connectivity.thread.types import ThreadRole
from honeydew.affordances.connectivity.thread.utils import ot_cli
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing.custom_types import FidlEndpoint

_LOWPAN_MONIKER = "core/lowpanservice"
_FACTORY_LOOKUP_CAPABILITY = "fuchsia.factory.lowpan.FactoryLookup"
_DEVICE_CONNECTOR_CAPABILITY = "fuchsia.lowpan.device.DeviceConnector"
_DATASET_CONNECTOR_CAPABILITY = "fuchsia.lowpan.thread.DatasetConnector"

_REQUIRED_CAPABILITIES = [_FACTORY_LOOKUP_CAPABILITY]
_DEFAULT_INTERFACE_NAME = "lowpan0"
_DEFAULT_NETWORK_NAME = "OpenThread"
_DEFAULT_CLI_TIMEOUT = 30.0
_MAKE_READY_TIMEOUT = 15.0
_MAKE_READY_RETRY_DELAY = 0.5

_FACTORY_LOOKUP_ENDPOINT = FidlEndpoint(
    _LOWPAN_MONIKER, _FACTORY_LOOKUP_CAPABILITY
)
_DEVICE_CONNECTOR_ENDPOINT = FidlEndpoint(
    _LOWPAN_MONIKER, _DEVICE_CONNECTOR_CAPABILITY
)
_DATASET_CONNECTOR_ENDPOINT = FidlEndpoint(
    _LOWPAN_MONIKER, _DATASET_CONNECTOR_CAPABILITY
)


class Thread(Affordance, AsyncLazyReady):
    """Thread affordance implemented with Fuchsia Controller via LoWPAN FIDLs and `SetupOtCli`."""

    # TODO(https://fxbug.dev/570921520): Replace `SetupOtCli` completely after
    # the missing LoWPAN FIDLs are supported.

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        fuchsia_controller: fc_transport.FuchsiaController,
        reboot_affordance: affordances_capable.RebootCapableDevice,
        fuchsia_device_close: affordances_capable.FuchsiaDeviceClose,
        interface_name: str = _DEFAULT_INTERFACE_NAME,
    ) -> None:
        AsyncLazyReady.__init__(self)
        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx
        self._fc_transport: fc_transport.FuchsiaController = fuchsia_controller
        self._reboot_affordance = reboot_affordance
        self._fuchsia_device_close = fuchsia_device_close
        self._interface_name: str = interface_name
        self._factory_device_proxy: f_factory_lowpan.FactoryDeviceClient | None = (
            None
        )
        self._device_proxy: f_lowpan_device.DeviceClient | None = None
        self._dataset_proxy: f_lowpan_thread.DatasetClient | None = None
        self._ot_cli_socket: AsyncSocket | None = None
        self._rx_buffer: str = ""
        # Use an incremental UTF-8 decoder so multi-byte UTF-8 sequences split
        # across consecutive `AsyncSocket.read()` chunks decode cleanly.
        self._utf8_decoder = codecs.getincrementaldecoder("utf-8")(
            errors="replace"
        )
        self._cli_lock = asyncio.Lock()

        self.verify_supported()

        self._reboot_affordance.register_for_on_device_boot(self.make_ready)
        self._fuchsia_device_close.register_for_on_device_close(self._close)

    def verify_supported(self) -> None:
        """Verifies that the Thread affordance is supported on the target device."""
        for capability in _REQUIRED_CAPABILITIES:
            output = self._ffx.run(
                ["component", "capability", capability],
                machine=ffx_types.MachineFormat.RAW,
            )
            if capability not in output:
                raise errors.NotSupportedError(
                    f"Capability '{capability}' not supported on {self._device_name}"
                )

    async def make_ready(self) -> None:
        """Establishes `FactoryDevice` connection and persistent `SetupOtCli` socket."""
        await super().make_ready()
        last_err: Exception | None = None
        try:
            async with asyncio.timeout(_MAKE_READY_TIMEOUT):
                while True:
                    client_socket = None
                    try:
                        lookup_channel = (
                            self._fc_transport.connect_device_proxy(
                                _FACTORY_LOOKUP_ENDPOINT
                            )
                        )
                        lookup_proxy = f_factory_lowpan.FactoryLookupClient(
                            lookup_channel
                        )
                        client, server = self._fc_transport.channel_create()
                        lookup_proxy.lookup(
                            name=self._interface_name,
                            device_factory=server.take(),
                        )
                        self._factory_device_proxy = (
                            f_factory_lowpan.FactoryDeviceClient(client.take())
                        )
                        (
                            client_socket,
                            server_socket,
                        ) = self._fc_transport.socket_create()
                        await self._factory_device_proxy.setup_ot_cli(
                            server_socket=server_socket.take()
                        )
                        self._ot_cli_socket = AsyncSocket(client_socket)
                        self._rx_buffer = ""
                        self._utf8_decoder.reset()
                        return
                    except (FcTransportStatus, ZxStatus) as err:
                        last_err = err
                        if client_socket is not None:
                            client_socket.close()
                        await asyncio.sleep(_MAKE_READY_RETRY_DELAY)
        except Exception as err:
            self._close()
            cause = (
                last_err if isinstance(err, TimeoutError) and last_err else err
            )
            raise ThreadDeviceNotFoundError(
                f"Failed to connect to FactoryDevice '{self._interface_name}' via {_LOWPAN_MONIKER}"
            ) from cause

    def _close(self) -> None:
        """Releases all FIDL client connections and the OT CLI socket."""
        if self._ot_cli_socket is not None:
            self._ot_cli_socket.socket.close()
            self._ot_cli_socket = None
        self._factory_device_proxy = None
        self._device_proxy = None
        self._dataset_proxy = None
        self._rx_buffer = ""
        self._utf8_decoder.reset()
        # `_cli_lock` does not need to be released explicitly here: closing
        # `_ot_cli_socket` wakes any in-flight `read()`, and the `async with
        # self._cli_lock:` block releases the lock on exit.
        self._ready = False

    def _connect_device_proxy(self) -> f_lowpan_device.DeviceClient:
        """Creates a fresh `fuchsia.lowpan.device.Device` channel for `self._interface_name`."""
        connector_ch = self._fc_transport.connect_device_proxy(
            _DEVICE_CONNECTOR_ENDPOINT
        )
        connector = f_lowpan_device.DeviceConnectorClient(connector_ch)
        client, server = self._fc_transport.channel_create()
        connector.connect(
            name=self._interface_name,
            server_end=server.take(),
        )
        return f_lowpan_device.DeviceClient(client.take())

    def _get_device_proxy(self) -> f_lowpan_device.DeviceClient:
        """Returns a `fuchsia.lowpan.device.Device` client proxy."""
        if self._device_proxy is not None:
            return self._device_proxy
        self._device_proxy = self._connect_device_proxy()
        return self._device_proxy

    def _get_dataset_proxy(self) -> f_lowpan_thread.DatasetClient:
        """Returns a `fuchsia.lowpan.thread.Dataset` client proxy."""
        if self._dataset_proxy is not None:
            return self._dataset_proxy
        connector_ch = self._fc_transport.connect_device_proxy(
            _DATASET_CONNECTOR_ENDPOINT
        )
        connector = f_lowpan_thread.DatasetConnectorClient(connector_ch)
        client, server = self._fc_transport.channel_create()
        connector.connect(
            name=self._interface_name,
            server_end=server.take(),
        )
        self._dataset_proxy = f_lowpan_thread.DatasetClient(client.take())
        return self._dataset_proxy

    def _drain_pending_socket(self) -> None:
        """Discards any unread bytes already sitting in the socket before a new command.

        The `SetupOtCli` stream socket may contain unsolicited asynchronous
        OpenThread CLI output or trailing prompt bytes that arrived between
        commands. Performing non-blocking reads until `FC_ERR_SHOULD_WAIT` and
        resetting `_rx_buffer` ensures stale bytes do not contaminate the next
        command's response.

        TODO(https://fxbug.dev/570921520): When `wait_for_cli_event()` is
        implemented, buffer drained unsolicited lines into an event queue
        instead of discarding them so asynchronous CLI events are not lost
        across commands.
        """
        if self._ot_cli_socket is None:
            return
        while True:
            try:
                chunk = self._ot_cli_socket.socket.read()
            except FcTransportStatus as status:
                if (
                    status.args
                    and status.args[0] == FcTransportStatus.FC_ERR_SHOULD_WAIT
                ):
                    break
                raise
            if not isinstance(chunk, (bytes, bytearray)) or not chunk:
                break
        self._rx_buffer = ""
        self._utf8_decoder.reset()

    async def _read_next_line(self) -> str:
        """Reads from `_ot_cli_socket` until a full newline-terminated line is available."""
        assert self._ot_cli_socket is not None
        while "\n" not in self._rx_buffer:
            chunk = await self._ot_cli_socket.read()
            if not chunk:
                raise HoneydewThreadError(
                    "OT CLI socket closed unexpectedly (EOF)"
                )
            self._rx_buffer += self._utf8_decoder.decode(chunk)
        raw_line, self._rx_buffer = self._rx_buffer.split("\n", 1)
        return raw_line.rstrip("\r")

    @ensure_ready
    async def _send_raw_command(
        self, command: str, timeout: float = _DEFAULT_CLI_TIMEOUT
    ) -> str:
        """Sends a command over the `SetupOtCli` socket and reads until `Done` or `Error`.

        Args:
            command: Raw OpenThread CLI command string to write to the socket
                (e.g. `"state"`, `"ipaddr"`, or `"br omrprefix local"`).
                Leading/trailing whitespace is stripped and a trailing newline
                (`"\\n"`) is appended before sending.
            timeout: Maximum time in seconds to wait for a terminal `"Done"` or
                `"Error ..."` line before raising `HoneydewThreadError`.
                Defaults to `_DEFAULT_CLI_TIMEOUT` (30.0s).

        Returns:
            Raw CRLF-joined response lines read from the socket up to and
            including the terminal `"Done"` or `"Error ..."` line.

        Raises:
            HoneydewThreadError: If the command times out, the socket hits EOF,
                or a Zircon/Fuchsia Controller transport error occurs.
        """
        assert self._ot_cli_socket is not None
        async with self._cli_lock:
            try:
                self._drain_pending_socket()
                cmd_bytes = f"{command.strip()}\n".encode("utf-8")
                self._ot_cli_socket.write(cmd_bytes)

                collected_lines: list[str] = []
                async with asyncio.timeout(timeout):
                    while True:
                        line = await self._read_next_line()
                        collected_lines.append(line)
                        stripped = line.strip()
                        if stripped.startswith("> "):
                            stripped = stripped[2:].strip()
                        if stripped == "Done" or stripped.startswith("Error "):
                            break
                return "\r\n".join(collected_lines) + "\r\n"
            except TimeoutError as err:
                raise HoneydewThreadError(
                    f"Timed out after {timeout}s waiting for OT CLI response to '{command}'"
                ) from err
            except (FcTransportStatus, ZxStatus) as err:
                self._close()
                raise HoneydewThreadError(
                    f"Transport error on OT CLI socket during '{command}'"
                ) from err

    async def run_ot_cli(self, command: str) -> str:
        """Executes a raw OpenThread CLI command and returns the cleaned output.

        Args:
            command: OpenThread CLI command string to send (e.g. `"state"` or `"ipaddr"`).

        Returns:
            Newline-joined output lines with the echoed command, prompt (`>`), and
            trailing `Done` marker stripped.

        Raises:
            HoneydewThreadError: If the CLI returns an `Error` line or the socket times out.
        """
        raw_output = await self._send_raw_command(command)
        lines = ot_cli.parse_response(command, raw_output)
        return "\n".join(lines)

    async def wait_for_cli_event(
        self, pattern: str, timeout: float = _DEFAULT_CLI_TIMEOUT
    ) -> str:
        """Waits for an unsolicited OpenThread CLI event line matching `pattern`.

        Why this function is needed:
        OpenThread CLI commands such as `udp` (incoming UDP packet
        notifications: `<N> bytes from <ip> <port> <payload>`) and
        `srp client callback` (`SRP client callback - error:OK`) emit
        unsolicited asynchronous output lines directly onto the `SetupOtCli`
        Zircon socket without a `Done` terminator. Because `lowpan-ot-driver`
        forwards all `otCliOutputCallback` bytes exclusively to the CLI socket
        and does not expose CLI-originated UDP or SRP client events via FIDL
        callbacks, E2E test cases (such as bidirectional UDP Tx/Rx and SRP
        client callback verification) need this method to capture asynchronous
        CLI events.

        Args:
            pattern: Substring or regular expression pattern to match against
                unsolicited OpenThread CLI output lines.
            timeout: Maximum time in seconds to wait for a matching event line.

        Returns:
            The matched unsolicited CLI output line.
        """
        # TODO(https://fxbug.dev/570921520): Implement this method (together
        # with buffering unsolicited lines in `_drain_pending_socket()`) when
        # the first E2E test case calls `wait_for_cli_event()`. Currently kept
        # as an empty placeholder because no test case invokes it yet.
        del pattern, timeout
        return ""

    @ensure_ready
    async def get_role(self) -> ThreadRole:
        """Returns the current OpenThread device role via `fuchsia.lowpan.device.Device`."""
        try:
            # WatchDeviceState is a hanging-get; the first call on a fresh Device
            # channel always returns the full current state snapshot immediately.
            device_proxy = self._connect_device_proxy()
            res = await device_proxy.watch_device_state()
            return ThreadRole.from_fidl(res.device_combined_state)
        except (FcTransportStatus, ZxStatus) as err:
            raise HoneydewThreadError(
                f"Device.WatchDeviceState FIDL Error on '{self._interface_name}'"
            ) from err

    @ensure_ready
    async def get_active_dataset_tlvs(self) -> str:
        """Returns the hex-encoded Active Operational Dataset TLVs via `fuchsia.lowpan.thread.Dataset`."""
        try:
            res = await self._get_dataset_proxy().get_active_tlvs()
            if not res.dataset:
                return ""
            return bytes(res.dataset).hex()
        except (FcTransportStatus, ZxStatus) as err:
            self._dataset_proxy = None
            raise HoneydewThreadError(
                f"Dataset.GetActiveTlvs FIDL Error on '{self._interface_name}'"
            ) from err

    @ensure_ready
    async def set_active_dataset_tlvs(self, dataset_tlvs: str) -> None:
        """Sets the Active Operational Dataset from a hex-encoded TLV string via `fuchsia.lowpan.thread.Dataset`.

        Args:
            dataset_tlvs: Hex-encoded Operational Dataset TLV string.

        Raises:
            HoneydewThreadError: If `dataset_tlvs` is not valid hex or if the FIDL call fails.
        """
        try:
            raw_tlvs = list(bytes.fromhex(dataset_tlvs.strip()))
        except ValueError as err:
            raise HoneydewThreadError(
                f"Invalid hex dataset TLVs: {dataset_tlvs!r}"
            ) from err
        try:
            await self._get_dataset_proxy().set_active_tlvs(dataset=raw_tlvs)
        except (FcTransportStatus, ZxStatus) as err:
            self._dataset_proxy = None
            raise HoneydewThreadError(
                f"Dataset.SetActiveTlvs FIDL Error on '{self._interface_name}'"
            ) from err

    @ensure_ready
    async def attach_all_nodes_to(self, dataset_tlvs: str) -> int:
        """Requests all nodes on the current network transition to the given dataset via `fuchsia.lowpan.thread.Dataset`.

        Args:
            dataset_tlvs: Hex-encoded Operational Dataset TLV string to transition the mesh to.

        Returns:
            Delay timer in milliseconds (`delay_ms`) before nodes switch to the new dataset.

        Raises:
            HoneydewThreadError: If `dataset_tlvs` is not valid hex or if the FIDL call fails.
        """
        try:
            raw_tlvs = list(bytes.fromhex(dataset_tlvs.strip()))
        except ValueError as err:
            raise HoneydewThreadError(
                f"Invalid hex dataset TLVs: {dataset_tlvs!r}"
            ) from err
        try:
            res = await self._get_dataset_proxy().attach_all_nodes_to(
                dataset=raw_tlvs
            )
            return int(res.delay_ms)
        except (FcTransportStatus, ZxStatus) as err:
            self._dataset_proxy = None
            raise HoneydewThreadError(
                f"Dataset.AttachAllNodesTo FIDL Error on '{self._interface_name}'"
            ) from err

    @ensure_ready
    async def form_network(
        self,
        *,
        channel: int | None = None,
        network_name: str | None = None,
        pan_id: int | None = None,
        ext_pan_id: str | None = None,
        network_key: str | None = None,
    ) -> str:
        """Forms a new Thread network and returns its active dataset hex TLVs via `fuchsia.lowpan.device.Device`.

        Args:
            channel: Optional IEEE 802.15.4 channel number (e.g. 11-26 for 2.4 GHz).
                If omitted, `lowpanservice` selects a default channel.
            network_name: Optional Thread network name (UTF-8 string, up to 16 bytes).
                Defaults to `_DEFAULT_NETWORK_NAME` ("OpenThread") when `None`.
            pan_id: Optional 16-bit IEEE 802.15.4 PAN ID (0x0000-0xFFFE).
                If omitted, `lowpanservice` generates a random PAN ID.
            ext_pan_id: Optional 8-byte Extended PAN ID as a 16-character hex string.
                If omitted, `lowpanservice` generates a random Extended PAN ID.
            network_key: Optional 16-byte Thread network key as a 32-character hex string.
                If omitted, `lowpanservice` generates a random network key.

        Returns:
            Hex-encoded Active Operational Dataset TLV string of the newly formed network.

        Raises:
            HoneydewThreadError: If `ext_pan_id` or `network_key` is not valid hex of the
                expected length, or if the underlying LoWPAN FIDL call fails.
        """
        effective_name = (
            network_name if network_name is not None else _DEFAULT_NETWORK_NAME
        )
        xpanid_bytes: list[int] | None = None
        if ext_pan_id is not None:
            try:
                parsed_xpanid = bytes.fromhex(ext_pan_id.strip())
            except ValueError as err:
                raise HoneydewThreadError(
                    f"Invalid hex ext_pan_id: {ext_pan_id!r}"
                ) from err
            if len(parsed_xpanid) != 8:
                raise HoneydewThreadError(
                    f"ext_pan_id must be 8 bytes (16 hex chars), got {ext_pan_id!r}"
                )
            xpanid_bytes = list(parsed_xpanid)

        credential: f_lowpan_device.Credential | None = None
        if network_key is not None:
            try:
                parsed_key = bytes.fromhex(network_key.strip())
            except ValueError as err:
                raise HoneydewThreadError(
                    f"Invalid hex network_key: {network_key!r}"
                ) from err
            if len(parsed_key) != 16:
                raise HoneydewThreadError(
                    f"network_key must be 16 bytes (32 hex chars), got {network_key!r}"
                )
            credential = f_lowpan_device.Credential(
                network_key=list(parsed_key)
            )

        identity = f_lowpan_device.Identity(
            raw_name=list(effective_name.encode("utf-8")),
            channel=channel,
            panid=pan_id,
            xpanid=xpanid_bytes,
        )
        params = f_lowpan_device.ProvisioningParams(
            identity=identity,
            credential=credential,
        )
        try:
            device_proxy = self._get_device_proxy()
            await device_proxy.provision_network(params=params)
            await device_proxy.set_active(active=True)
        except (FcTransportStatus, ZxStatus) as err:
            self._device_proxy = None
            raise HoneydewThreadError(
                f"Device.ProvisionNetwork/SetActive FIDL Error on '{self._interface_name}'"
            ) from err
        return await self.get_active_dataset_tlvs()

    @ensure_ready
    async def leave_network(self) -> None:
        """Stops Thread, brings down the interface, and clears the dataset via `fuchsia.lowpan.device.Device`."""
        try:
            device_proxy = self._get_device_proxy()
            await device_proxy.leave_network()
            await device_proxy.set_active(active=False)
        except (FcTransportStatus, ZxStatus) as err:
            self._device_proxy = None
            raise HoneydewThreadError(
                f"Device.LeaveNetwork/SetActive FIDL Error on '{self._interface_name}'"
            ) from err
