# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ABC with methods for Host-(Fuchsia)Target interactions via Serial port."""

import abc
from collections.abc import Sequence


class Serial(abc.ABC):
    """ABC with methods for Host-(Fuchsia)Target interactions via Serial port."""

    @abc.abstractmethod
    def read(
        self,
        size: int = 512,
    ) -> str:
        """Reads bytes directly from the serial port and decodes them as a utf-8 string.

        This method makes no guarantees around timing of the serial output being read.
        For example, the output of a serial command from `send` is not guaranteed to be
        immediately available in the output of `read`.

        Args:
            size: The number of bytes to read.

        Returns:
            The bytes decoded into a utf-8 string.

        Raises:
            SerialError: In case of failure.
        """

    @abc.abstractmethod
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

    @abc.abstractmethod
    def send_and_recv(
        self,
        cmd: str,
        stop_tokens: Sequence[str] = (),
        timeout_sec: float = 5.0,
        recv_size: int = 4096,
        log_output: bool = True,
    ) -> str:
        """Send command over serial port and read output on the same connection.

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
