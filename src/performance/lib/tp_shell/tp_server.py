# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs trace_processor_shell as a server for a single trace file."""

import contextlib
import os
import selectors
import subprocess
import tempfile
import time
from types import TracebackType
from typing import IO, Self

from perfetto.common.exceptions import PerfettoException
from perfetto.trace_processor.process_tree import terminate_process_tree

# trace_processor_shell prints exactly one line starting with this prefix on
# stdout once the trace is loaded and the socket is accepting connections.
_READY_PREFIX = b"perfetto-session "

_SOCKET_NAME = "tp.sock"

# Linux limits Unix socket paths to 107 bytes plus the terminating NUL.
_MAX_SOCKET_PATH_BYTES = 107

# Last resort for the socket directory if neither $XDG_RUNTIME_DIR nor the
# temporary directory is usable.
_FALLBACK_SOCKET_PARENT_DIR = "/tmp"


class TraceProcessorStartupError(PerfettoException):
    """trace_processor_shell exited or misbehaved before it became ready.

    The message includes whatever the shell wrote to stdout and stderr.

    Attributes:
        exit_code: The shell's exit code, or None if it was still running and
            had to be killed.
    """

    def __init__(self, message: str, exit_code: int | None) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class TraceProcessorServer:
    """A trace_processor_shell process serving one trace over a Unix socket.

    The shell reads and parses `trace_path` itself, then serves queries on
    `socket_path` until the server is closed. Pass `socket_path` to
    `perfetto.trace_processor.TraceProcessor(remote=...)` to connect.

    Construction blocks until the trace is loaded and the shell is ready, and
    raises `TraceProcessorStartupError` if the shell exits or reports anything
    else first. Use the server as a context manager or call `close()`, which
    kills the process and removes the socket.

    Attributes:
        socket_path: Path of the Unix socket the shell is listening on.
    """

    socket_path: str

    def __init__(
        self,
        tp_shell_path: str,
        trace_path: str,
        startup_timeout_s: float = 120.0,
    ) -> None:
        """Starts trace_processor_shell and waits until it is ready.

        Args:
            tp_shell_path: Path to the trace_processor_shell binary.
            trace_path: Path to the trace file for the shell to load.
            startup_timeout_s: How long to wait for the shell to load the
                trace before giving up.

        Raises:
            TraceProcessorStartupError: The shell exited or failed to start.
            TimeoutError: The shell did not become ready in time.
            OSError: No directory was usable for the socket.
        """
        # The shell runs with the socket directory as its working directory,
        # so relative paths must be resolved against ours first.
        tp_shell_path = os.path.abspath(tp_shell_path)
        trace_path = os.path.abspath(trace_path)
        # Resources are released in reverse order: the process is killed
        # before its stderr file is closed and its socket directory removed.
        self._resources = contextlib.ExitStack()
        try:
            socket_dir = self._resources.enter_context(_make_socket_dir())
            self.socket_path = os.path.join(socket_dir, _SOCKET_NAME)
            stderr = self._resources.enter_context(tempfile.TemporaryFile())
            self._proc = subprocess.Popen(
                [
                    tp_shell_path,
                    "--quiet",
                    "--no-ftrace-raw",
                    "server",
                    "unix",
                    "--path",
                    self.socket_path,
                    "--idle-timeout",
                    "never",
                    trace_path,
                ],
                cwd=socket_dir,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=stderr,
                # Gives the shell its own process group so that
                # terminate_process_tree() can kill it and any children.
                start_new_session=True,
            )
            self._resources.callback(self._terminate)

            ready_line = _read_line(self._proc, startup_timeout_s)
            if not ready_line.startswith(_READY_PREFIX):
                self._terminate()
                stderr.seek(0)
                raise TraceProcessorStartupError(
                    f"trace_processor_shell failed to load '{trace_path}' "
                    f"(exit code {self._proc.returncode}).\n"
                    f"stdout: {_decode(ready_line)}\n"
                    f"stderr: {_decode(stderr.read())}",
                    exit_code=self._proc.returncode,
                )
        except BaseException:
            self._resources.close()
            raise

    @property
    def running(self) -> bool:
        """Whether the shell process is still running."""
        return self._proc.poll() is None

    def close(self) -> None:
        """Kills the shell and removes its socket. Safe to call repeatedly."""
        self._resources.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: TracebackType | None,
    ) -> None:
        self.close()

    def _terminate(self) -> None:
        # Once the process has been reaped its pid (and process group id) may
        # be reused, so only signal it if it has not been reaped yet.
        if self._proc.returncode is None:
            terminate_process_tree(self._proc)
        if self._proc.stdout is not None:
            self._proc.stdout.close()


def _socket_parent_dir_candidates() -> list[str]:
    """Returns the directories to try for the socket, most preferred first.

    $XDG_RUNTIME_DIR is per-user, short, and meant for sockets (Perfetto's own
    session sockets live there). The temporary directory honors $TMPDIR, which
    matters where /tmp is not writable. /tmp is the last resort.
    """
    candidates = [
        os.environ.get("XDG_RUNTIME_DIR", ""),
        tempfile.gettempdir(),
        _FALLBACK_SOCKET_PARENT_DIR,
    ]
    unique: list[str] = []
    for candidate in candidates:
        if candidate and os.path.abspath(candidate) not in unique:
            unique.append(os.path.abspath(candidate))
    return unique


def _make_socket_dir() -> tempfile.TemporaryDirectory[str]:
    """Creates a private directory to hold the server's socket.

    Uses the first candidate parent directory that is writable and leaves the
    socket path within the kernel's length limit.

    Raises:
        OSError: No candidate directory was usable.
    """
    rejected: list[str] = []
    for parent in _socket_parent_dir_candidates():
        try:
            socket_dir = tempfile.TemporaryDirectory(prefix="tp_", dir=parent)
        except OSError as e:
            rejected.append(f"{parent}: {e}")
            continue
        socket_path = os.path.join(socket_dir.name, _SOCKET_NAME)
        path_bytes = len(os.fsencode(socket_path))
        if path_bytes <= _MAX_SOCKET_PATH_BYTES:
            return socket_dir
        socket_dir.cleanup()
        rejected.append(
            f"{parent}: socket path would be {path_bytes} bytes "
            f"(limit {_MAX_SOCKET_PATH_BYTES})"
        )
    raise OSError(
        "No usable directory for the trace_processor_shell socket:\n  "
        + "\n  ".join(rejected)
    )


def _read_line(proc: subprocess.Popen[bytes], timeout_s: float) -> bytes:
    """Returns the first line `proc` writes to stdout, or what it wrote
    before exiting if it exits without writing a full line.

    Raises:
        TimeoutError: No full line arrived within `timeout_s`.
    """
    stdout: IO[bytes] | None = proc.stdout
    assert stdout is not None
    fd = stdout.fileno()
    data = bytearray()
    deadline = time.monotonic() + timeout_s
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        while (newline := data.find(b"\n")) == -1:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"trace_processor_shell was not ready after {timeout_s}s"
                )
            if not selector.select(timeout=remaining):
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                return bytes(data)
            data += chunk
    return bytes(data[: newline + 1])


def _decode(data: bytes) -> str:
    return data.decode("utf-8", errors="replace").strip()
