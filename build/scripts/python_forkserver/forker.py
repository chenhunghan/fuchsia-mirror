# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""The forker processes of the Python forkserver.

The controller (server.py) forks --forkers of them after warm-up. Each one
accepts connections on the shared socket, checks that the client runs as the
same user, sends the ready message, and forks a worker per request
(worker.py), then sends the client the worker's pid and wait status. When
the controller shuts the socket down, it stops accepting, waits for its
workers, and writes its stats for the controller to merge (stats.py).
"""

import errno
import os
import resource
import select
import signal
import socket
import struct
import sys
import traceback
from typing import Any, NamedTuple, NoReturn

import client
import stats
import worker

_MAX_REQUEST_SIZE = 64 << 20  # 64 MiB
_CHUNK_SIZE = 64 << 10  # 64 KiB

# Signals a forker keeps blocked for its whole life: the controller owns them
# and stops the forkers via listener.shutdown(SHUT_RDWR). They keep their
# dispositions, so a worker forked mid-handshake queues an early one instead
# of discarding it, until worker.run() unblocks them.
_BLOCKED_SIGNALS = frozenset({signal.SIGTERM, signal.SIGHUP, signal.SIGINT})

# Linux 4.5+ (<sys/epoll.h>); the vendored Python's select module is built
# without the constant.
_EPOLLEXCLUSIVE = getattr(select, "EPOLLEXCLUSIVE", 1 << 28)

# SO_PEERCRED returns a struct ucred {pid_t pid; uid_t uid; gid_t gid;}: in
# struct module format, one int ("i") and two unsigned ints ("2I").
_UCRED_FORMAT = "i2I"


class _ReceivedRequest(NamedTuple):
    """A client request and the stdio fds that came with it."""

    request: client.Request
    # The client's stdin, stdout, and stderr.
    fds: list[int]


def _warn(message: str, role: str = "forker") -> None:
    """Writes `message` to the server's log (its stderr).

    Args:
      message: What to write.
      role: What this process is ("forker" or "worker"), for the log line.
    """
    print(
        f"python_forkserver: {role} {os.getpid()}: {message}",
        file=sys.stderr,
        flush=True,
    )


def _on_sigchld(signum: int, frame: Any) -> None:
    """Does nothing: SIGCHLD's C-level handler already wrote to the wakeup fd.

    Python only writes to the fd from signal.set_wakeup_fd() for signals that
    have a Python handler, so SIGCHLD needs one.
    """


def _msg(tag: bytes, value: int) -> bytes:
    """Packs a 5-byte server->client frame (1-byte tag + signed i32le value)."""
    return tag + value.to_bytes(4, "little", signed=True)


def _same_user(conn: socket.socket) -> bool:
    """Returns True if the client of `conn` runs as this user.

    Abstract socket names are visible to all users.
    """
    creds = conn.getsockopt(
        socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize(_UCRED_FORMAT)
    )
    _, uid, _ = struct.unpack(_UCRED_FORMAT, creds)
    return uid == os.getuid()


def _recv_request(conn: socket.socket) -> _ReceivedRequest:
    """Reads one framed client request and its SCM_RIGHTS stdio fds.

    Closes any received fds before raising.

    Raises:
      EOFError: The client hung up without sending anything.
      ValueError: The client didn't send a well-formed request.
      OSError: The connection failed.
    """
    try:
        data, fds, flags, _ = socket.recv_fds(conn, _CHUNK_SIZE, 3)
    except ConnectionResetError as e:
        # A client that closes with frames from us unread (e.g. the PID)
        # resets the connection rather than closing it cleanly.
        raise EOFError("the client hung up before sending a request") from e
    if not data and not fds:
        raise EOFError("the client hung up before sending a request")
    try:
        if flags & socket.MSG_CTRUNC or len(fds) != 3:
            raise ValueError("expected 3 stdio fds")
        buf = bytearray(data)
        while len(buf) < 4:
            chunk = conn.recv(_CHUNK_SIZE)
            if not chunk:
                raise ValueError("truncated request")
            buf += chunk
        payload_len = int.from_bytes(buf[:4], "little")
        if payload_len > _MAX_REQUEST_SIZE:
            raise ValueError("request too large")
        total = 4 + payload_len
        while len(buf) < total:
            chunk = conn.recv(min(_CHUNK_SIZE, total - len(buf)))
            if not chunk:
                raise ValueError("truncated request")
            buf += chunk
        return _ReceivedRequest(client.Request.decode(bytes(buf[4:total])), fds)
    except BaseException:
        for fd in fds:
            os.close(fd)
        raise


class _Forker:
    """Accepts connections and forks a worker for each, until shut down.

    serve() waits for its two kinds of events, a connection on the listener and
    a worker exiting, in one epoll_wait(). SIGCHLD's C-level handler writes to
    a wakeup pipe that epoll watches, so a worker that exits just before
    epoll_wait() blocks still wakes it. (A Python-level handler only runs once
    the blocking call returns, so a forker blocked in accept() could miss the
    signal and leave the worker's client waiting.)
    """

    def __init__(
        self,
        listener: socket.socket,
        warm: worker.WarmState,
        import_cwd: bytes,
        stats_path: str,
    ):
        self.listener = listener
        self.warm = warm
        self.import_cwd = import_cwd
        self.metrics = stats.Metrics(stats_path)
        self.children: dict[int, socket.socket] = {}
        self.wakeup_r, self.wakeup_w = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
        self.epoll = select.epoll()

    def serve(self) -> int:
        """Serves requests until the controller shuts the socket down.

        Returns:
          1 if accept() fails (for example, EMFILE), which makes the
          controller stop the server, else 0.
        """
        signal.pthread_sigmask(signal.SIG_SETMASK, _BLOCKED_SIGNALS)
        try:
            # All forkers share the listener, so it's non-blocking: another
            # forker may accept a connection first.
            self.listener.setblocking(False)
            self._watch_listener()
            self.epoll.register(self.wakeup_r, select.EPOLLIN)
            signal.signal(signal.SIGCHLD, _on_sigchld)
            signal.set_wakeup_fd(self.wakeup_w, warn_on_full_buffer=False)
            while True:
                for fd, events in self.epoll.poll():
                    if fd == self.wakeup_r:
                        self._on_wakeup()
                    elif events & select.EPOLLHUP:
                        # The controller shut the listener down (SHUT_RDWR).
                        # (A non-blocking accept() would keep failing with
                        # EAGAIN rather than EINVAL.)
                        return 0
                    elif not self._accept_and_handle():
                        return 0
        except OSError:
            traceback.print_exc()
            return 1
        finally:
            self._drain()
            ru = resource.getrusage(resource.RUSAGE_SELF)
            extras = {
                "pid": os.getpid(),
                "server_utime_s": ru.ru_utime,
                "server_stime_s": ru.ru_stime,
            }
            self.metrics.write(extras, pid_suffix=True)

    def _watch_listener(self) -> None:
        """Adds the listener to the epoll set."""
        try:
            # Wakes one idle forker per connection rather than all of them.
            self.epoll.register(self.listener, select.EPOLLIN | _EPOLLEXCLUSIVE)
        except OSError as e:
            if e.errno != errno.EINVAL:  # A kernel without EPOLLEXCLUSIVE.
                raise
            self.epoll.register(self.listener, select.EPOLLIN)

    def _accept_and_handle(self) -> bool:
        """Accepts a waiting connection, if any, and forks a worker for it.

        Returns:
          False once the controller has shut the socket down, else True.

        Raises:
          OSError: accept() failed, for example with EMFILE.
        """
        try:
            conn, _ = self.listener.accept()
        except (BlockingIOError, ConnectionAbortedError):
            return True  # Another forker took it, or the client gave up.
        except OSError as e:
            if e.errno == errno.EINVAL:
                return False  # The controller shut it down.
            raise
        conn.setblocking(True)
        self._handle(conn)
        return True

    def _on_wakeup(self) -> None:
        """Empties the wakeup pipe, then reaps the workers that exited."""
        try:
            while os.read(self.wakeup_r, 4096):
                pass
        except BlockingIOError:
            pass
        self._reap(block=False)

    def _reap(self, *, block: bool) -> None:
        """Reaps exited workers; with `block`, waits for all of them."""
        while self.children:
            pid, status, ru = os.wait4(-1, 0 if block else os.WNOHANG)
            if pid == 0:
                return
            self._finish(pid, status, ru)

    def _finish(self, pid: int, status: int, ru: Any) -> None:
        """Records a reaped worker and sends its wait status to the client."""
        conn = self.children.pop(pid)
        self.metrics.worker_done(status, ru)
        try:
            conn.sendall(_msg(client.TAG_STATUS, status))
        except OSError:
            # The client is gone (e.g. Ninja was interrupted and killed it), so
            # nobody is waiting for this status, and worker_done() above has
            # already counted it. No warning: an interrupted build would log
            # one for every action that was running.
            pass
        conn.close()

    def _handle(self, conn: socket.socket) -> None:
        """Forks a worker for `conn` and sends the client its pid."""
        try:
            if not _same_user(conn):
                _warn("refused a client that runs as another user")
                conn.close()
                return
            conn.sendall(_msg(client.TAG_READY, 0))
        except OSError:
            conn.close()
            return
        self.metrics.counters.requests += 1
        try:
            pid = os.fork()
        except OSError as err:
            _warn(f"couldn't fork a worker: {err}")
            conn.close()
            return
        if pid == 0:
            self._become_worker(conn)
        try:
            os.setpgid(pid, pid)
        except OSError:
            pass
        self.children[pid] = conn
        try:
            conn.sendall(_msg(client.TAG_PID, pid))
        except OSError:
            # The client is gone, so nobody will forward signals: stop the worker
            # and anything it started. ESRCH: it already exited.
            try:
                os.killpg(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

    def _become_worker(self, conn: socket.socket) -> NoReturn:
        """Turns this forked child into the worker for `conn`."""
        # Undo serve()'s SIGCHLD wakeup before closing its fds, and drop the
        # forker's other fds, so that only the forker holds them.
        signal.set_wakeup_fd(-1)
        signal.signal(signal.SIGCHLD, signal.SIG_DFL)
        self.epoll.close()
        os.close(self.wakeup_r)
        os.close(self.wakeup_w)
        self.listener.close()
        for other in self.children.values():
            other.close()
        try:
            received = _recv_request(conn)
        except EOFError:
            # The client is gone, e.g. because the build was interrupted:
            # nothing to run, and nothing worth a warning.
            os._exit(1)
        except (ValueError, OSError) as err:
            _warn(f"got a bad request: {err!r}", role="worker")
            os._exit(1)  # Not a request: drop the connection.
        worker.run(
            conn, received.request, received.fds, self.warm, self.import_cwd
        )

    def _drain(self) -> None:
        """Stops accepting, then waits for the remaining workers."""
        signal.set_wakeup_fd(-1)
        signal.signal(signal.SIGCHLD, signal.SIG_DFL)
        self.epoll.close()
        self.listener.close()
        self._reap(block=True)


def run(
    listener: socket.socket,
    warm: worker.WarmState,
    import_cwd: bytes,
    stats_path: str,
) -> NoReturn:
    """Serves requests in this forked forker until shut down, then exits.

    Never returns, so the forker can't run on into the controller's code.
    """
    code = 1
    try:
        code = _Forker(listener, warm, import_cwd, stats_path).serve()
    except BaseException:
        traceback.print_exc()
    finally:
        os._exit(code)
