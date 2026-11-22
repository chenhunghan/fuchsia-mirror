# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Runs a Python script on the build's forkserver.

Build actions opt in by prefixing the Python script invocation with this file,
so the script itself needs no changes:

  python3 -S client.py SCRIPT [ARGS...]

The client is a messenger: it sends SCRIPT, ARGS, the environment, working
directory, umask, and stdin/stdout/stderr to the forkserver named by
$FX_PYTHON_FORKSERVER_SOCKET, which runs SCRIPT in a forked worker with the
server's own interpreter and startup flags. SIGINT/SIGTERM/SIGHUP are
forwarded to the worker, and this process exits with the worker's exit status
(or dies from the same signal).

If $FX_PYTHON_FORKSERVER_SOCKET is unset or empty (for example, Ninja run
outside `fx build`), the client warns once per build directory and execs
`python3 [OPTIONS] SCRIPT [ARGS...]` with this command line's own interpreter
and options.

Anything else fails the action, with a message saying how to turn the
forkserver off: the server isn't running, closes the connection, or doesn't
answer within _HANDSHAKE_TIMEOUT_S, or client stdio is closed.

Platform support: Linux only (abstract AF_UNIX sockets, /proc/<pid>/stat),
matching the Fuchsia build host requirement (//build/config/BUILDCONFIG.gn).
"""

# Every Python build action runs this file, so it only imports modules built
# into the interpreter: sys, posix, and _signal are already loaded at startup,
# and _socket loads no Python files. Each import of a Python module adds
# milliseconds to every action (typing alone adds ~6 ms).
import posix
import sys

import _signal
import _socket

# Not typing.TYPE_CHECKING, to skip that import; mypy treats this as True.
TYPE_CHECKING = False
if TYPE_CHECKING:
    from typing import NoReturn

# ---- Wire protocol, shared with server.py ----------------------------------
# server.py imports these, so the client and server can't disagree. README.md
# describes the protocol.

MAGIC = b"fx-python-forkserver-v2"
# Bytes, like the keys of posix.environ, so nothing needs decoding.
SOCKET_ENV = b"FX_PYTHON_FORKSERVER_SOCKET"
# Server -> client messages: a tag byte and an i32le value.
MESSAGE_SIZE = 5
TAG_READY = b"R"  # Accepted; the client may send its request.
TAG_PID = b"P"  # The worker's pid (and process group).
TAG_STATUS = b"S"  # The worker's wait status.


class Request:
    """What a worker needs to run a script the way this process would.

    The client's stdin, stdout, and stderr travel next to the request, as
    SCM_RIGHTS.
    """

    def __init__(
        self, cwd: bytes, umask: int, argv: list[bytes], env: list[bytes]
    ) -> None:
        self.cwd = cwd
        self.umask = umask
        self.argv = argv  # SCRIPT and its arguments.
        self.env = env  # KEY=VALUE entries.

    @classmethod
    def current(cls, argv: list[str]) -> "Request":
        """Returns the request that runs `argv` in this process's context."""
        enc = sys.getfilesystemencoding()
        errors = sys.getfilesystemencodeerrors()
        umask = posix.umask(0o022)
        posix.umask(umask)
        return cls(
            posix.getcwdb(),
            umask,
            [arg.encode(enc, errors) for arg in argv],
            [k + b"=" + v for k, v in posix.environ.items()],
        )

    def encode(self) -> bytes:
        """Returns the wire form: a u32le payload length, then the payload.

        The payload is these fields, separated by NUL bytes (which can't occur
        in any of them): MAGIC, cwd, umask and argc in decimal, the argc argv
        entries, then the KEY=VALUE environment entries.
        """
        fields = [MAGIC, self.cwd, b"%d" % self.umask, b"%d" % len(self.argv)]
        payload = b"\0".join(fields + self.argv + self.env)
        return len(payload).to_bytes(4, "little") + payload

    @classmethod
    def decode(cls, payload: bytes) -> "Request":
        """Parses encode()'s payload, without the length.

        Raises:
          ValueError: `payload` isn't a request.
        """
        fields = payload.split(b"\0")
        if len(fields) < 5 or fields[0] != MAGIC:
            raise ValueError("not a Python forkserver request")
        umask = int(fields[2])
        argc = int(fields[3])
        argv = fields[4 : 4 + argc]
        if not 0 <= umask <= 0o777 or argc < 1 or len(argv) != argc:
            raise ValueError("malformed Python forkserver request")
        return cls(fields[1], umask, argv, fields[4 + argc :])


# ---- Client ------------------------------------------------------------------

_SLOW_PATH_MARKER = ".python_forkserver_slow_path"
_HANDSHAKE_TIMEOUT_S = 2.0
_FORWARDED_SIGNALS = (_signal.SIGINT, _signal.SIGTERM, _signal.SIGHUP)
# How to turn the forkserver off. setting.py owns this setting, but importing
# it (and pathlib) here would slow down every action.
_TURN_OFF = (
    "echo FX_BUILD_INTERNAL_PYTHON_FORKSERVER=0 > .fx/config/python_forkserver"
)


def _worker_alive(pid: int) -> bool:
    """Returns True if `pid` exists and has not exited into a zombie."""
    try:
        fd = posix.open(f"/proc/{pid}/stat", posix.O_RDONLY)
    except OSError:
        return False
    try:
        stat = posix.read(fd, 128)
    finally:
        posix.close(fd)
    # "PID (COMM) STATE ...": COMM may contain anything, even ") Z ", so look
    # after its last ")". State Z is a zombie.
    return stat.rpartition(b")")[2][1:2] != b"Z"


def _die_by_signal(signum: int) -> "NoReturn":
    """Terminates this client process with `signum` (or exit code 128 + signum)."""
    try:
        _signal.signal(signum, _signal.SIG_DFL)
    except OSError:
        pass
    posix.kill(posix.getpid(), signum)
    posix._exit(128 + signum)


def _exit_like(status: int) -> "NoReturn":
    """Exits with the same exit code or terminating signal as `status`.

    Args:
      status: Raw `wait4()` status integer reported by `server.py`.
    """
    code = posix.waitstatus_to_exitcode(status)
    if code < 0:
        # The worker died from a signal: die from the same one, so that the
        # caller (Ninja) sees the same thing it would have seen without us.
        _die_by_signal(-code)
    posix._exit(code)


def _run_without_server() -> None:
    """Warns once per build directory, then execs the script without us."""
    try:  # O_EXCL: exactly one action wins, even with hundreds running at once.
        posix.close(
            posix.open(
                _SLOW_PATH_MARKER,
                posix.O_WRONLY | posix.O_CREAT | posix.O_EXCL,
                0o644,
            )
        )
        print(
            "client.py: warning: no Python forkserver is running, so Python "
            "actions run without it (slower). Use `fx build` to get it.",
            file=sys.stderr,
            flush=True,
        )
    except OSError:
        pass  # Already warned since the last `fx build`, or not writable.
    # python3 [OPTIONS] client.py SCRIPT ARGS -> python3 [OPTIONS] SCRIPT ARGS
    i = len(sys.orig_argv) - len(sys.argv)
    posix.execv(sys.executable, sys.orig_argv[:i] + sys.orig_argv[i + 1 :])


class _Messenger:
    """Runs one action on the forkserver, then exits like its worker."""

    def __init__(self) -> None:
        self.worker_pid = 0  # 0 until the server sends it.
        self.pending_signal = 0  # The last signal received, if any.

    def run(self, name: bytes, argv: list[str]) -> "NoReturn":
        """Runs `argv` on the forkserver at `name`, then exits like the worker.

        1. Connects and waits for the server's ready message (fails if it
           doesn't come within 2 s).
        2. Sends the request (cwd, umask, argv, environment) along with this
           process's stdin/stdout/stderr, so the worker writes straight to our
           caller's pipes. SIGINT/SIGTERM/SIGHUP are forwarded to the worker
           from here on.
        3. Waits for the worker's pid, then its wait status, and exits the same
           way (same exit code or same signal).

        Any failure to reach or hear from the server fails the action.
        """
        sock = self._connect(name)
        self._send_request(sock, Request.current(argv))
        self._wait_for_worker(sock)

    def _fail(self, problem: str) -> "NoReturn":
        """Fails this action because the forkserver could not run it."""
        if self.pending_signal:
            _die_by_signal(self.pending_signal)
        print(
            f"client.py: the Python forkserver {problem}. To build without it, "
            f"run `{_TURN_OFF}` in your Fuchsia checkout.",
            file=sys.stderr,
            flush=True,
        )
        posix._exit(1)

    def _forward_signal(self, signum: int, frame: object) -> None:
        """Relays a signal to the worker's process group.

        The server puts the worker in its own process group before it sends
        the pid, so this reaches the worker and anything it started.
        """
        self.pending_signal = signum
        if self.worker_pid > 0:
            try:
                posix.killpg(self.worker_pid, signum)
            except OSError:
                pass  # ESRCH: the worker and its children already exited.

    def _connect(self, name: bytes) -> _socket.socket:
        """Connects to the server and waits for its ready message."""
        sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
        if sock.fileno() <= 2:
            # One of stdin/stdout/stderr (0-2) is closed, so `sock` took its
            # slot, and the worker can't be given this process's stdio.
            self._fail(
                "can't run an action whose stdin, stdout, or stderr is closed"
            )
        sock.settimeout(_HANDSHAKE_TIMEOUT_S)
        try:
            sock.connect(b"\0" + name)
            ready = b""
            while len(ready) < MESSAGE_SIZE:
                chunk = sock.recv(MESSAGE_SIZE - len(ready))
                if not chunk:
                    self._fail("closed the connection before answering")
                ready += chunk
            if ready[:1] != TAG_READY:
                self._fail("sent an unexpected handshake byte")
        except TimeoutError:
            self._fail(f"didn't answer within {_HANDSHAKE_TIMEOUT_S:g} s")
        except OSError as err:
            self._fail(
                f"isn't running ({err.strerror or err.__class__.__name__})"
            )
        return sock

    def _send_request(self, sock: _socket.socket, request: Request) -> None:
        """Sends `request` with our stdio, and starts forwarding signals."""
        data = request.encode()
        fds = b"".join(fd.to_bytes(4, sys.byteorder) for fd in (0, 1, 2))
        for sig in _FORWARDED_SIGNALS:
            _signal.signal(sig, self._forward_signal)
        try:
            sent = sock.sendmsg(
                [data], [(_socket.SOL_SOCKET, _socket.SCM_RIGHTS, fds)]
            )
            while sent < len(data):
                sent += sock.send(data[sent:])
        except TimeoutError:
            self._fail(f"didn't answer within {_HANDSHAKE_TIMEOUT_S:g} s")
        except OSError as err:
            self._fail(
                "closed the connection "
                f"({err.strerror or err.__class__.__name__})"
            )

    def _wait_for_worker(self, sock: _socket.socket) -> "NoReturn":
        """Waits for the worker's pid, then exits with its wait status.

        The pid must arrive within the handshake timeout. After that, the
        action may run as long as it needs: the timeout only wakes this loop so
        that, once a signal has been forwarded, the client still exits if the
        worker is gone but the server never reports it (for example, because
        the server is stopped).
        """
        buf = b""
        while True:
            try:
                chunk = sock.recv(64)
            except TimeoutError:
                if self.worker_pid == 0:
                    # The server accepted the request but never started a
                    # worker.
                    self._fail(
                        f"didn't answer within {_HANDSHAKE_TIMEOUT_S:g} s"
                    )
                if self.pending_signal and not _worker_alive(self.worker_pid):
                    _die_by_signal(self.pending_signal)
                continue
            except OSError:
                chunk = b""
            if not chunk:
                if self.worker_pid == 0:
                    self._fail(
                        "closed the connection before starting the action"
                    )
                self._fail("closed the connection while the action was running")
            buf += chunk
            while len(buf) >= MESSAGE_SIZE:
                tag = buf[:1]
                value = int.from_bytes(
                    buf[1:MESSAGE_SIZE], "little", signed=True
                )
                buf = buf[MESSAGE_SIZE:]
                if tag == TAG_PID:
                    self.worker_pid = value
                    if self.pending_signal:
                        self._forward_signal(self.pending_signal, None)
                elif tag == TAG_STATUS:
                    _exit_like(value)


def main() -> None:
    if len(sys.argv) < 2:
        print("usage: client.py SCRIPT [ARGS...]", file=sys.stderr, flush=True)
        posix._exit(2)
    name = posix.environ.get(SOCKET_ENV)
    if not name:
        _run_without_server()
    else:
        _Messenger().run(name, sys.argv[1:])


if __name__ == "__main__":
    main()
