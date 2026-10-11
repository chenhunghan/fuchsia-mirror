# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Serial transport interface implementation using unix socket."""

import codecs
import logging
import socket
import time
from collections.abc import Sequence

from honeydew.transports.serial import errors as serial_errors
from honeydew.transports.serial import serial as serial_interface
from honeydew.utils import decorators

_LOGGER: logging.Logger = logging.getLogger(__name__)


_CARRIAGE_RETURN: bytes = b"\r\n\r\n"


class SerialUsingUnixSocket(serial_interface.Serial):
    """Serial transport interface implementation using unix socket.

    Serial communication with Fuchsia devices is done using Unix Socket in
    Fuchsia infra labs.

    Args:
        device_name: Fuchsia device name.
        socket_path: AF_UNIX socket path associated with the device.
    """

    def __init__(
        self,
        device_name: str,
        socket_path: str,
    ) -> None:
        self._device_name: str = device_name
        self._socket_path: str = socket_path

    @decorators.liveness_check
    def read(
        self,
        size: int = 512,
    ) -> str:
        """Read data from the serial port.

        Args:
            size: The number of bytes to read.

        Returns:
            The data read from the serial port.

        Raises:
            SerialError: In case of failure.
        """
        _LOGGER.debug(
            "Reading %d bytes from the serial port of '%s'",
            size,
            self._device_name,
        )

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1)
                s.connect(self._socket_path)
                return s.recv(size).decode("utf-8", errors="ignore")
        except socket.timeout as e:
            raise serial_errors.SerialError(
                f"Timed out while reading from the serial port of '{self._device_name}'"
            ) from e
        except socket.error as e:
            raise serial_errors.SerialError(
                f"Failed to read from the serial port of '{self._device_name}'"
            ) from e

    @decorators.liveness_check
    def send(
        self,
        cmd: str,
    ) -> None:
        """Send command over serial port and immediately returns without any
        further checking if it ran successfully or not.

        Args:
            cmd: Command to run over serial port.

        Raises:
            SerialError: In case of failure.
        """
        _LOGGER.info(
            "Sending '%s' command over the serial port of '%s'",
            cmd,
            self._device_name,
        )

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.connect(self._socket_path)
                s.sendall(
                    _CARRIAGE_RETURN + cmd.encode("utf-8") + _CARRIAGE_RETURN
                )
        except socket.error as e:
            raise serial_errors.SerialError(
                f"Failed to send '{cmd}' command over the serial port of '{self._device_name}'"
            ) from e

    @decorators.liveness_check
    def send_and_recv(
        self,
        cmd: str,
        stop_tokens: Sequence[str] = (),
        timeout_sec: float = 5.0,
        recv_size: int = 4096,
        log_output: bool = True,
    ) -> str:
        """Send command over serial port and read output on the same connection.

        Keeps the unix socket open only during the send and receive window so
        the serial server does not drop unread command bytes on disconnect or
        buffer unrelated console output outside the command window.

        Args:
            cmd: Command to run over serial port.
            stop_tokens: Optional tokens that terminate reading once any token
                appears in the accumulated output. Callers should choose tokens
                that do not appear in `cmd` itself, as the serial console echoes
                the command upon entry.
            timeout_sec: Maximum duration in seconds to wait for command output.
            recv_size: Maximum number of bytes to read per recv call.
            log_output: When True, logs the output in DEBUG level. Callers
                may set this to False when expecting particularly large
                or spammy output.

        Returns:
            The accumulated output read from the serial port.

        Raises:
            SerialError: In case of failure.
        """
        _LOGGER.info(
            "Sending '%s' command and reading output over the serial port of '%s'",
            cmd,
            self._device_name,
        )

        output = ""
        decoder = codecs.getincrementaldecoder("utf-8")(errors="ignore")
        deadline = time.monotonic() + timeout_sec

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(timeout_sec)
                s.connect(self._socket_path)
                s.sendall(
                    _CARRIAGE_RETURN + cmd.encode("utf-8") + _CARRIAGE_RETURN
                )

                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    s.settimeout(remaining)
                    try:
                        chunk_bytes = s.recv(recv_size)
                    except socket.timeout:
                        break

                    if not chunk_bytes:
                        break

                    output += decoder.decode(chunk_bytes)
                    if stop_tokens and any(
                        token in output for token in stop_tokens
                    ):
                        break
                output += decoder.decode(b"", final=True)
        except socket.error as e:
            raise serial_errors.SerialError(
                f"Failed to send '{cmd}' command over the serial port of '{self._device_name}'"
            ) from e

        if log_output:
            _LOGGER.debug("Received output from serial: %s", output)

        return output
