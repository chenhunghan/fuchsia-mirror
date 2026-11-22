#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Persistent Python build-action forkserver.

Every Python build action starts a fresh `python3 -S SCRIPT`, paying an import
floor before the script does any work. This server starts the interpreter and
imports common modules once, then runs each action in a fork()ed copy of
itself, so every script keeps its one implementation and each action still
gets a fresh process.

Platform support: Linux only (abstract AF_UNIX sockets, SO_PEERCRED,
SCM_RIGHTS), matching the Fuchsia build host requirement
(//build/config/BUILDCONFIG.gn).

Lifecycle: forkserver_wrap.py starts one server per build when the
//.fx/config/python_forkserver setting is on (see setting.py), advertises it
to the build through $FX_PYTHON_FORKSERVER_SOCKET, and stops it with SIGTERM
after the build. Its CPU time, including all workers, is therefore
accounted to the build. Build actions reach it by starting their script
through client.py (`python3 -S client.py SCRIPT ARGS`).

Served scripts run with this server's interpreter and startup flags (start it
the way the build starts Python actions, e.g. `python3 -S -W
error::SyntaxWarning`); `PYTHON*` startup variables are read once, when the
server starts. Each script runs from its file, with the module search path,
environment, and fresh __main__ that `python3 -S SCRIPT` would give it with
the request's environment; modules imported at warm-up stay loaded unless the
script would find a different file under that name.

The server warms up when it starts, from its working directory (some modules
compute paths relative to the cwd at import time), and only serves requests
from that same directory. Start it from the build directory.

Processes: main() is the controller. It binds the socket, warms up, writes the
ready byte, forks --forkers forker processes (forker.run()) that accept
connections and fork a worker per request, and then waits for SIGTERM, SIGHUP,
or SIGINT to stop them and merge their stats. If a forker exits before that, the
controller stops the others and exits 1, so the build's remaining actions fail
rather than queue.

The wire protocol is described in README.md; its constants and the request
encoding live in client.py. forker.py accepts connections and forks a worker
per request, worker.py warms up and turns each forked worker into the process
that runs the script, and stats.py counts requests and merges the forkers'
stats.
"""

import argparse
import os
import resource
import signal
import socket
import sys
import time
import traceback
from typing import Any

import forker
import stats
import worker

_DEFAULT_FORKERS = min(8, os.cpu_count() or 1)

# Signals the controller waits for with sigwait(). The forkers keep SIGTERM,
# SIGHUP, and SIGINT blocked, so any of them stops the server cleanly.
_CONTROLLER_SIGNALS = frozenset(
    {signal.SIGTERM, signal.SIGHUP, signal.SIGINT, signal.SIGCHLD}
)


def _parse_args(argv: list[str], description: str) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--socket-name", required=True)
    parser.add_argument(
        "--stats", required=True, help="write a JSON stats line here on exit"
    )
    parser.add_argument(
        "--forkers",
        type=int,
        default=_DEFAULT_FORKERS,
        help="number of processes that accept and fork workers in parallel",
    )
    # forkserver_wrap.py reads one byte from this pipe once clients can connect.
    parser.add_argument(
        "--ready-fd", type=int, default=-1, help=argparse.SUPPRESS
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    """Runs the controller: see the module docstring."""
    args = _parse_args(argv, description=(__doc__ or "").splitlines()[0])
    t_start = time.monotonic()
    # The controller takes these with sigwait() below; forkers install their
    # own handlers.
    signal.pthread_sigmask(signal.SIG_BLOCK, _CONTROLLER_SIGNALS)

    # Closed on every path, including a failed warm-up, so that clients
    # waiting in its backlog fail right away.
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(b"\0" + args.socket_name.encode())
        listener.listen(4096)

        import_cwd = os.getcwdb()
        t0 = time.monotonic()
        try:
            warm = worker.warm_up(import_cwd)
        except BaseException:
            # Before the ready byte, so forkserver_wrap.py reports the failure.
            traceback.print_exc()
            return 1
        warmup_s = time.monotonic() - t0
        warmup_ms = warmup_s * 1000
        print(
            f"python_forkserver: imported {len(sys.modules)} modules "
            f"from {os.fsdecode(import_cwd)} in {warmup_ms:.1f} ms",
            file=sys.stderr,
            flush=True,
        )
        if args.ready_fd >= 0:
            # Clients can connect now; the forkers forked below accept them.
            os.write(args.ready_fd, b"R")
            os.close(args.ready_fd)

        # Don't copy warm-up's buffered output into the forkers and their
        # workers.
        sys.stdout.flush()
        sys.stderr.flush()
        forkers = []
        for _ in range(args.forkers):
            pid = os.fork()
            if pid == 0:
                forker.run(listener, warm, import_cwd, args.stats)
            forkers.append(pid)

        code = _supervise(forkers, listener)
    _write_stats(args, forkers, import_cwd, warmup_s, t_start)
    return code


def _supervise(forkers: list[int], listener: socket.socket) -> int:
    """Waits for SIGTERM, SIGHUP, or SIGINT, then stops the forkers.

    Args:
      forkers: The pids of the forker processes main() forked.
      listener: The socket the forkers accept connections on. Shut down
        here, which makes the forkers stop.

    Returns:
      1 if a forker exited first (the server then stops early), else 0.
    """
    code = 0
    running = set(forkers)
    # SIGCHLD with nothing to reap (warm-up, forker stop/resume): keep waiting.
    while signal.sigwait(_CONTROLLER_SIGNALS) == signal.SIGCHLD:
        pid, status = os.waitpid(-1, os.WNOHANG)
        if pid:
            exit_code = os.waitstatus_to_exitcode(status)
            print(
                f"python_forkserver: forker {pid} exited "
                f"({exit_code}) before the build ended",
                file=sys.stderr,
                flush=True,
            )
            running.remove(pid)
            code = 1
            break
    # Wakes the forkers, whose epoll then reports EPOLLHUP on the listener,
    # and fails their accept() calls, now and later. Then closes the
    # controller's copy: clients still queued in the backlog are reset once
    # the last copy closes, which the forkers do as they stop, rather than
    # waiting for the handshake timeout.
    listener.shutdown(socket.SHUT_RDWR)
    listener.close()
    for pid in running:
        os.waitpid(pid, 0)
    return code


def _write_stats(
    args: argparse.Namespace,
    forkers: list[int],
    import_cwd: bytes,
    warmup_s: float,
    t_start: float,
) -> None:
    """Merges the forkers' stats into `args.stats` and logs the result."""
    metrics = stats.Metrics(args.stats)
    extras: dict[str, Any] = metrics.merge_forkers(forkers)._asdict()
    self_ru = resource.getrusage(resource.RUSAGE_SELF)
    child_ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    extras |= {
        "pid": os.getpid(),
        "forkers": args.forkers,
        "socket": args.socket_name,
        "import_cwd": import_cwd.decode(errors="replace"),
        "warmup_s": round(warmup_s, 4),
        "wall_s": round(time.monotonic() - t_start, 3),
        "server_utime_s": self_ru.ru_utime,
        "server_stime_s": self_ru.ru_stime,
        "children_utime_s": child_ru.ru_utime,
        "children_stime_s": child_ru.ru_stime,
        "children_maxrss_kb": child_ru.ru_maxrss,
        "server_maxrss_kb": self_ru.ru_maxrss,
    }
    line = metrics.write(extras, pid_suffix=False)
    print(f"python_forkserver: stats {line}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
