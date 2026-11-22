# Copyright 2026 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import os
import re
import shutil
import subprocess
import tempfile
import threading
import time
from types import TracebackType
from typing import IO, Self

from libs.net import wait_for_port
from libs.proc import job
from libs.proc.runner import (
    CalledProcessError,
    CalledProcessTransportError,
    Runner,
)
from libs.ssh import formatter
from libs.ssh import settings as ssh_settings

_LOGGER = logging.getLogger(__name__)


# TODO(https://fxbug.dev/567167756): Merge SshConnection and SSHProvider into a
# single SSH library.
class SshConnection(Runner):
    """Provides a connection to a remote machine through ssh.

    Provides the ability to connect to a remote machine and execute a command
    on it. The connection will try to establish a persistent connection When
    a command is run. If the persistent connection fails it will attempt
    to connect normally.
    """

    @property
    def socket_path(self) -> str:
        """Returns: The os path to the main socket file."""
        if self._main_ssh_tempdir is None:
            raise AttributeError(
                "socket_path is not available yet; run setup_main_ssh() first"
            )
        return os.path.join(self._main_ssh_tempdir, "socket")

    def __init__(self, settings: ssh_settings.SshSettings) -> None:
        """
        Args:
            settings: The ssh settings to use for this connection.
            formatter: The object that will handle formatting ssh command
                       for use with the background job.
        """
        self._settings = settings
        self._formatter = formatter.SshFormatter()
        self._lock = threading.Lock()
        self._main_ssh_proc: subprocess.Popen[bytes] | None = None
        self._main_ssh_tempdir: str | None = None

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: TracebackType | None,
    ) -> None:
        self.close()

    def __del__(self) -> None:
        self.close()

    def setup_main_ssh(self, timeout_sec: float = 5) -> None:
        """Sets up the main ssh connection.

        Sets up the initial main ssh connection if it has not already been
        started.

        Args:
            timeout_sec: The time to wait for the main ssh connection to
                be made.

        Raises:
            Error: When setting up the main ssh connection fails.
        """
        with self._lock:
            if self._main_ssh_proc is not None:
                socket_path = self.socket_path
                if (
                    not os.path.exists(socket_path)
                    or self._main_ssh_proc.poll() is not None
                ):
                    _LOGGER.debug(
                        "[SshConnection | %s] main ssh connection is down.",
                        self._settings.hostname,
                    )
                    self._cleanup_main_ssh()

            if self._main_ssh_proc is None:
                # Create a shared socket in a temp location.
                self._main_ssh_tempdir = tempfile.mkdtemp(prefix="ssh-main")

                # Setup flags and options for running the main ssh
                # -N: Do not execute a remote command.
                # ControlMaster: Spawn a main connection.
                # ControlPath: The main connection socket path.
                extra_flags: dict[str, str | int | None] = {"-N": None}
                extra_options: dict[str, str | int | bool] = {
                    "ControlMaster": True,
                    "ControlPath": self.socket_path,
                    "BatchMode": True,
                }

                # Construct the command and start it.
                main_cmd = self._formatter.format_ssh_local_command(
                    self._settings,
                    extra_flags=extra_flags,
                    extra_options=extra_options,
                )
                _LOGGER.info(
                    "[SshConnection | %s] Starting main ssh connection.",
                    self._settings.hostname,
                )
                self._main_ssh_proc = job.run_async(main_cmd)

                end_time = time.time() + timeout_sec

                while time.time() < end_time:
                    if (
                        os.path.exists(self.socket_path)
                        or self._main_ssh_proc.poll() is not None
                    ):
                        break
                    time.sleep(0.2)
                if not os.path.exists(self.socket_path):
                    self._cleanup_main_ssh()
                    raise CalledProcessTransportError(
                        "main ssh connection timed out."
                    )

    def run(
        self,
        command: str | list[str],
        stdin: bytes | None = None,
        timeout_sec: float | None = 60.0,
        log_output: bool = True,
        ignore_status: bool = False,
        attempts: int = 2,
    ) -> subprocess.CompletedProcess[bytes]:
        """Runs a remote command over ssh.

        Will ssh to a remote host and run a command. This method will
        block until the remote command is finished.

        Args:
            command: The command to execute over ssh.
            stdin: Standard input to command.
            timeout_sec: seconds to wait for command to finish.
            log_output: If true, print stdout and stderr to the debug log.
            ignore_status: True to ignore the exit code of the remote
                           subprocess.  Note that if you do ignore status codes,
                           you should handle non-zero exit codes explicitly.
            attempts: Number of attempts before giving up on command failures.

        Returns:
            Results of the ssh command.

        Raises:
            CalledProcessError: when the process exits with a non-zero status
                and ignore_status is False.
            subprocess.TimeoutExpired: When the remote command took to long to
                execute.
            CalledProcessTransportError: when the underlying transport fails
        """
        if attempts < 1:
            raise TypeError("attempts must be a positive, non-zero integer")

        deadline = (
            time.time() + timeout_sec if timeout_sec is not None else None
        )
        setup_timeout: float = self._settings.connect_timeout
        if deadline is not None:
            setup_timeout = min(setup_timeout, max(0.0, deadline - time.time()))

        try:
            self.setup_main_ssh(setup_timeout)
        except CalledProcessTransportError:
            _LOGGER.warning(
                "[SshConnection | %s] Failed to create main ssh connection, "
                "using normal ssh connection.",
                self._settings.hostname,
            )

        extra_options: dict[str, str | int | bool] = {"BatchMode": True}
        if self._main_ssh_proc:
            extra_options["ControlPath"] = self.socket_path

        if isinstance(command, list):
            full_command = " ".join(command)
        else:
            full_command = command

        terminal_command = self._formatter.format_command(
            full_command, self._settings, extra_options=extra_options
        )

        dns_retry_count = 2
        while True:
            try:
                run_timeout = timeout_sec
                if deadline is not None:
                    run_timeout = deadline - time.time()
                    if run_timeout <= 0:
                        raise subprocess.TimeoutExpired(
                            terminal_command, timeout_sec or 0.0
                        )
                result = job.run(
                    terminal_command,
                    stdin=stdin,
                    log_output=log_output,
                    timeout_sec=run_timeout,
                )

                return subprocess.CompletedProcess(
                    terminal_command,
                    result.returncode,
                    result.stdout,
                    result.stderr,
                )
            except CalledProcessError as e:
                # Check for SSH errors.
                if e.returncode == 255:
                    stderr = e.stderr.decode("utf-8", errors="replace")

                    had_dns_failure = re.search(
                        r"^ssh: .*: Name or service not known",
                        stderr,
                        flags=re.MULTILINE,
                    )
                    if had_dns_failure:
                        dns_retry_count -= 1
                        if not dns_retry_count:
                            raise CalledProcessTransportError(
                                "DNS failed to find host"
                            ) from e
                        _LOGGER.debug(
                            "[SshConnection | %s] Failed to connect to host, "
                            "retrying...",
                            self._settings.hostname,
                        )
                        continue

                    had_timeout = re.search(
                        r"^ssh: connect to host .* port .*: "
                        r"Connection timed out\r$",
                        stderr,
                        flags=re.MULTILINE,
                    )
                    if had_timeout:
                        raise CalledProcessTransportError(
                            "Ssh timed out"
                        ) from e

                    permission_denied = "Permission denied" in stderr
                    if permission_denied:
                        raise CalledProcessTransportError(
                            "Permission denied"
                        ) from e

                    unknown_host = re.search(
                        r"ssh: Could not resolve hostname .*: "
                        r"Name or service not known",
                        stderr,
                        flags=re.MULTILINE,
                    )
                    if unknown_host:
                        raise CalledProcessTransportError("Unknown host") from e

                    # Retry unknown SSH errors.
                    _LOGGER.error(
                        "[SshConnection | %s] An unknown error has occurred. "
                        "Job result: %s",
                        self._settings.hostname,
                        e,
                    )
                    ping_output = job.run(
                        ["ping", self._settings.hostname, "-c", "3", "-w", "1"],
                        ignore_status=True,
                    )
                    _LOGGER.error(
                        "[SshConnection | %s] Ping result: %s",
                        self._settings.hostname,
                        ping_output,
                    )
                    if attempts > 1:
                        self._cleanup_main_ssh()
                        return self.run(
                            command,
                            stdin,
                            timeout_sec,
                            log_output,
                            ignore_status,
                            attempts - 1,
                        )
                    raise CalledProcessTransportError(
                        "The job failed for unknown reasons"
                    ) from e
                if ignore_status:
                    return subprocess.CompletedProcess(
                        terminal_command,
                        e.returncode,
                        e.stdout,
                        e.stderr,
                    )
                raise e

    def run_async(self, command: str) -> subprocess.CompletedProcess[bytes]:
        """Starts up a background command over ssh.

        Will ssh to a remote host and startup a command. This method will
        block until there is confirmation that the remote command has started.

        Args:
            command: The command to execute over ssh. Can be either a string
                     or a list.

        Returns:
            The result of the command to launch the background job.

        Raises:
            CalledProcessError: when the process fails to start
            subprocess.TimeoutExpired: when the timeout expires while waiting
                for a child process
            CalledProcessTransportError: when the underlying transport fails
        """
        return self.run(
            f"({command}) < /dev/null > /dev/null 2>&1 & echo -n $!"
        )

    def start(
        self,
        command: list[str],
        stdout: IO[bytes] | int = subprocess.PIPE,
        stdin: IO[bytes] | int = subprocess.PIPE,
        stderr: IO[bytes] | int = subprocess.PIPE,
    ) -> subprocess.Popen[bytes]:
        """Execute a child program in a new process."""
        extra_options: dict[str, str | int | bool] = {"BatchMode": True}
        if self._main_ssh_proc:
            extra_options["ControlPath"] = self.socket_path

        terminal_command = self._formatter.format_command(
            " ".join(command),
            self._settings,
            extra_options=extra_options,
        )
        return subprocess.Popen(
            terminal_command, stdout=stdout, stdin=stdin, stderr=stderr
        )

    def wait_until_reachable(self, timeout_sec: int | None = None) -> None:
        """Wait for the device to become reachable via SSH.

        Args:
            timeout_sec: Seconds to wait until raising TimeoutError.
        """
        if timeout_sec is None:
            timeout_sec = self._settings.connect_timeout
        timeout = time.monotonic() + timeout_sec
        while True:
            try:
                remaining_sec = max(1, int(timeout - time.monotonic()))
                wait_for_port(
                    self._settings.hostname,
                    self._settings.port,
                    timeout_sec=min(5, remaining_sec),
                )
                break
            except TimeoutError as e:
                if time.monotonic() >= timeout:
                    raise TimeoutError(
                        f"Waited over {timeout_sec}s for the service to start "
                        f"accepting connections at "
                        f"{self._settings.hostname}:{self._settings.port}"
                    ) from e

        while True:
            try:
                self.close()
                remaining = max(1.0, timeout - time.monotonic())
                self.run(
                    ["echo"],
                    stdin=None,
                    timeout_sec=min(10.0, remaining),
                    log_output=True,
                    attempts=1,
                )
                return
            except (
                CalledProcessTransportError,
                CalledProcessError,
                subprocess.TimeoutExpired,
            ) as e:
                if time.monotonic() < timeout:
                    time.sleep(1.0)
                    continue
                raise TimeoutError(
                    f"Waited over {timeout_sec}s for SSH to become reachable "
                    f"at {self._settings.hostname}:{self._settings.port}"
                ) from e

    def wait_until_unreachable(
        self,
        interval_sec: int = 1,
        timeout_sec: int | None = None,
    ) -> None:
        """Wait for the device to become unreachable via SSH.

        Args:
            interval_sec: Seconds to wait between unreachability attempts.
            timeout_sec: Seconds to wait until raising TimeoutError.
        """
        self.close()
        if timeout_sec is None:
            timeout_sec = self._settings.connect_timeout
        timeout = time.monotonic() + timeout_sec

        while True:
            try:
                wait_for_port(
                    self._settings.hostname,
                    self._settings.port,
                    timeout_sec=interval_sec,
                )
            except TimeoutError:
                return

            if time.monotonic() >= timeout:
                raise TimeoutError(
                    f"Connection to {self._settings.hostname} is still reachable "
                    f"after {timeout_sec}s"
                )
            time.sleep(0.2)

    def close(self) -> None:
        """Clean up open connections to remote host."""
        self._cleanup_main_ssh()

    def _cleanup_main_ssh(self) -> None:
        """
        Release all resources (process, temporary directory) used by an active
        main SSH connection.
        """
        # If a main SSH connection is running, kill it.
        if self._main_ssh_proc is not None:
            _LOGGER.debug(
                "[SshConnection | %s] Nuking main_ssh_job.",
                self._settings.hostname,
            )
            self._main_ssh_proc.kill()
            self._main_ssh_proc.wait()
            self._main_ssh_proc = None

        # Remove the temporary directory for the main SSH socket.
        if self._main_ssh_tempdir is not None:
            _LOGGER.debug(
                "[SshConnection | %s] Cleaning main_ssh_tempdir.",
                self._settings.hostname,
            )
            shutil.rmtree(self._main_ssh_tempdir)
            self._main_ssh_tempdir = None
