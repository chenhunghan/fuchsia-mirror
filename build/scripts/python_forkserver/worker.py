# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Warms up the forkserver and runs one build action in each forked worker.

server.py calls warm_up() once, then forks a worker per request and calls
run() in it. The worker takes on the request's working directory check,
umask, environment, stdio, and argv, and runs SCRIPT the way
`python3 -S SCRIPT ARGS` would with the server's interpreter and startup
flags.
"""

import atexit
import fcntl
import gc
import importlib
import io
import os
import runpy
import signal
import socket
import sys
import traceback
import types
import warnings
from collections.abc import Mapping
from importlib.machinery import PathFinder, SourceFileLoader
from pathlib import Path
from typing import Any, NamedTuple, NoReturn

import client

_PREWARM_EXTRA_MODULES = (
    "select",
    "build.python.modules.serialization",
    "build.scripts.service_profile",
)
_PROJECT_ROOT = Path(__file__).resolve().parents[3]

# Interpreter startup variables that each worker applies from the request's
# environment, as `python3 -S SCRIPT` would read them at startup.
ENV_PYTHONPATH = "PYTHONPATH"
ENV_DONT_WRITE_BYTECODE = "PYTHONDONTWRITEBYTECODE"
ENV_PYCACHE_PREFIX = "PYTHONPYCACHEPREFIX"


def _abspath(entry: str) -> str:
    """Returns a $PYTHONPATH entry made absolute like the interpreter does."""
    path = os.path.normpath(entry)  # "" -> "."
    if path.startswith("/"):
        return path
    cwd = os.getcwd()
    return cwd if path == "." else os.path.join(cwd, path)


def python_path(environ: Mapping[bytes, bytes]) -> list[str]:
    """Returns the sys.path entries that $PYTHONPATH in `environ` adds."""
    value = environ.get(os.fsencode(ENV_PYTHONPATH))
    if not value or sys.flags.ignore_environment:
        return []
    return [_abspath(os.fsdecode(entry)) for entry in value.split(b":")]


# The interpreter's own entries (the standard library), after sys.path[0] and
# the server's $PYTHONPATH entries. Every worker's sys.path ends with these.
_INTERPRETER_PATH = tuple(sys.path[1 + len(python_path(os.environb)) :])


class WarmState(NamedTuple):
    """What the server imported at warm-up, for deciding what a script reuses."""

    # sys.path[1:] at warm-up: what a script whose $PYTHONPATH matches the
    # server's gets after its own directory.
    client_path: tuple[str, ...]
    # sys.path[0] during warm-up, and the top-level modules loaded from it.
    local_dir: str
    local_modules: frozenset[str]
    # Top-level modules found only through the checkout root that warm-up added.
    root_modules: frozenset[str]


def is_path_module(module: Any) -> bool:
    """Returns True if `module` was found on sys.path (file or namespace pkg).

    Built-in and frozen modules are found before sys.path is searched, so an
    entry on sys.path can't shadow them.
    """
    spec = getattr(module, "__spec__", None)
    if spec is None:
        return False
    origin = spec.origin
    return (isinstance(origin, str) and origin.startswith("/")) or (
        origin is None and spec.submodule_search_locations is not None
    )


def warm_up(cwd: bytes) -> WarmState:
    """Changes to `cwd` and pre-imports common modules."""
    os.chdir(cwd)
    # The server's module search path, plus the checkout root so
    # `from build... import ...` works even when invoked outside
    # `fx build` without PYTHONPATH.
    client_path = tuple(sys.path[1:])
    project_root = str(_PROJECT_ROOT)
    added_project_root = project_root not in sys.path
    if added_project_root:
        sys.path.append(project_root)
    # Served scripts get the server's warning filters, whatever imports set.
    filters = warnings.filters[:]
    for extra in _PREWARM_EXTRA_MODULES:
        importlib.import_module(extra)
    warnings.filters[:] = filters  # type: ignore[index]
    warnings._filters_mutated()  # type: ignore[attr-defined]

    local_mods: set[str] = set()
    root_mods: set[str] = set()
    for name, mod in sys.modules.items():
        if "." in name or not is_path_module(mod):
            continue
        origin = getattr(mod.__spec__, "origin", None)
        if isinstance(origin, str) and os.path.dirname(origin) == sys.path[0]:
            local_mods.add(name)
        elif (
            added_project_root
            and PathFinder.find_spec(name, client_path) is None
        ):
            root_mods.add(name)
    # Keep the imported objects out of the collector's way so that workers
    # don't copy-on-write the whole heap when a GC pass runs.
    gc.collect()
    gc.freeze()
    return WarmState(
        client_path, sys.path[0], frozenset(local_mods), frozenset(root_mods)
    )


def run(
    conn: socket.socket,
    request: client.Request,
    fds: list[int],
    warm: WarmState,
    import_cwd: bytes,
) -> NoReturn:
    """Runs `request` in this forked worker, then exits with its status.

    Args:
      conn: The client's connection. Closed once the script starts.
      request: What to run.
      fds: The client's stdin, stdout, and stderr.
      warm: What warm_up() imported.
      import_cwd: The directory warm_up() ran in.
    """
    started = False
    interrupted = False
    try:
        os.setpgid(0, 0)
        for sig in (signal.SIGCHLD, signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, signal.SIG_DFL)
        signal.signal(signal.SIGINT, signal.default_int_handler)
        signal.pthread_sigmask(signal.SIG_SETMASK, set())
        if request.cwd != import_cwd:
            # Warm-up imports computed paths from import_cwd, so another
            # directory (e.g. a nested build) can't be served.
            os.write(
                fds[2],
                b"python_forkserver: this server runs actions from %s, not %s; "
                b"unset FX_PYTHON_FORKSERVER_SOCKET to run them without it.\n"
                % (import_cwd, request.cwd),
            )
            os._exit(1)
        if _peer_closed(conn):
            os._exit(1)  # The client is gone, so nobody wants the result.
        # Before apply_request(): if conn landed on fd 0-2, dup2() would
        # replace it, and closing it afterwards would close the script's stdio.
        conn.close()
        apply_request(request, fds)
        started = True
        code = run_script(warm)
    except KeyboardInterrupt as e:
        if started:
            # Like the interpreter: no frames from this file.
            print_uncaught(e, __file__)
        interrupted = True
        code = 130  # In case dying from SIGINT below fails.
    except BaseException:
        traceback.print_exc()
        code = 1
    if started:
        try:
            atexit._run_exitfuncs()
        except BaseException:
            pass
        for stream in (sys.stdout, sys.stderr):
            try:
                stream.flush()
            except BaseException:
                pass
    if interrupted:
        # Like CPython after an uncaught KeyboardInterrupt: once cleanup is
        # done, die from SIGINT, so that callers (and Ninja) see the same.
        signal.signal(signal.SIGINT, signal.SIG_DFL)
        os.kill(os.getpid(), signal.SIGINT)
    os._exit(code & 0xFF)


def _peer_closed(conn: socket.socket) -> bool:
    """Returns True if the client end of `conn` has already closed."""
    try:
        return conn.recv(1, socket.MSG_PEEK | socket.MSG_DONTWAIT) == b""
    except BlockingIOError:
        return False
    except OSError:
        return True


def apply_request(request: client.Request, fds: list[int]) -> None:
    """Applies a request's umask, environment, stdio fds, and `sys.argv`.

    Takes ownership of `fds` (the client's stdin, stdout, and stderr).
    """
    os.umask(request.umask)
    environb = os.environb
    environb.clear()
    for entry in request.env:
        key, sep, value = entry.partition(b"=")
        if sep and key:
            environb[key] = value
    # Move any received fd that landed on 0-2 (if server stdio was closed)
    # above 2 first so dup2() onto 0, 1, 2 never clobbers another slot.
    for i, fd in enumerate(fds):
        if fd <= 2:
            fds[i] = fcntl.fcntl(fd, fcntl.F_DUPFD_CLOEXEC, 3)
    for target, fd in enumerate(fds):
        os.dup2(fd, target)
        os.close(fd)
    # sys.stdout chose line buffering for the server's own fd 1; choose again
    # for the client's, the way the interpreter does at startup (stderr is
    # always line-buffered, and under -u, stdout writes through instead).
    if (
        isinstance(sys.stdout, io.TextIOWrapper)
        and not sys.stdout.write_through
    ):
        sys.stdout.reconfigure(line_buffering=os.isatty(1))
    sys.argv = [os.fsdecode(a) for a in request.argv]


def run_script(warm: WarmState) -> int:
    """Runs `python3 -S SCRIPT ARGS`, with SCRIPT ARGS in sys.argv.

    Gives the script the module search path, bytecode settings, and fresh
    __main__ that the server's interpreter would give it with the request's
    environment. Startup flags and -W options stay the server's.

    Returns:
      The script's exit code.
    """
    script = script_path(sys.argv[0])
    if script.endswith(".pyz"):
        path0 = script  # A zip application's archive holds __main__.py.
    else:
        path0 = os.path.dirname(os.path.realpath(sys.argv[0]))
    # The interpreter's own rule under -S: the script's directory, then
    # $PYTHONPATH, then the standard library.
    rest = (*python_path(os.environb), *_INTERPRETER_PATH)
    forget_shadowed_modules(path0, rest, warm)
    sys.path[:] = [path0, *rest]
    if not sys.flags.ignore_environment:
        sys.dont_write_bytecode = bool(os.environ.get(ENV_DONT_WRITE_BYTECODE))
        sys.pycache_prefix = os.environ.get(ENV_PYCACHE_PREFIX) or None
    main_module = new_main_module(script)
    try:
        if script.endswith(".pyz"):
            # The same call `python3 app.pyz` makes (pymain_run_module() in
            # CPython's Modules/main.c). runpy._run_module_as_main() is
            # private, but the public runpy.run_path() differs: it runs the
            # archive in a temporary __main__ module and changes sys.argv[0],
            # then restores both.
            runpy._run_module_as_main("__main__", False)  # type: ignore[attr-defined]
        else:
            # Compile directly from source like `python3 -S SCRIPT` so
            # __main__ never writes source-tree __pycache__/*.pyc or skips
            # compile-time -W error::SyntaxWarning on cached bytecode.
            with open(script, "rb") as f:
                code = compile(f.read(), script, "exec", dont_inherit=True)
            exec(code, main_module.__dict__)
        result: Any = None
    except SystemExit as e:
        result = e.code
    except KeyboardInterrupt:
        raise
    except BaseException as e:
        print_uncaught(e, __file__)
        return 1
    return _exit_code(result)


def _exit_code(result: Any) -> int:
    """Converts a SystemExit code (or main()'s result) to an exit code."""
    if result is None:
        return 0
    if isinstance(result, int):
        return result
    print(result, file=sys.stderr)
    return 1


def script_path(script: str) -> str:
    """Returns `script` made absolute the way the interpreter does (no normpath)."""
    return script if script.startswith("/") else os.getcwd() + "/" + script


def new_main_module(script: str) -> types.ModuleType:
    """Installs and returns an empty __main__ module for running `script`.

    Args:
      script: Absolute path of the script, as from script_path().
    """
    main = types.ModuleType("__main__")
    main.__builtins__ = sys.modules["builtins"]  # type: ignore[attr-defined]
    if not script.endswith(".pyz"):
        main.__file__ = script
        main.__cached__ = None  # type: ignore[attr-defined]
        main.__loader__ = SourceFileLoader("__main__", script)
    sys.modules["__main__"] = main
    return main


def print_uncaught(exc: BaseException, skip_file: str) -> None:
    """Prints `exc` the way the interpreter prints an uncaught exception.

    Leaves out the leading frames from `skip_file` (the code that started the
    script), as the interpreter would.
    """
    tb = exc.__traceback__
    while tb is not None and tb.tb_frame.f_code.co_filename == skip_file:
        tb = tb.tb_next
    # sys.excepthook prints the exception's own __traceback__, if it has one.
    sys.excepthook(type(exc), exc.with_traceback(tb), tb)


def forget_shadowed_modules(
    path0: str, rest: tuple[str, ...], warm: WarmState
) -> None:
    """Drops pre-imported modules that a script would not import itself.

    A fresh interpreter resolves top-level imports on [path0, *rest].
    Modules warm-up loaded outside rest, or that a same-named module or
    package in path0 (directory or .pyz archive) would shadow, would
    resolve differently, so they're removed from sys.modules.
    """
    if rest == warm.client_path and os.path.isdir(path0):
        if path0 == warm.local_dir:
            stale = set(warm.root_modules)
        else:
            stale = set(warm.local_modules) | set(warm.root_modules)
            try:
                entries = os.listdir(path0)
            except OSError:
                entries = []
            for entry in entries:
                stem = entry.partition(".")[0]
                if is_path_module(sys.modules.get(stem)):
                    stale.add(stem)
    else:
        stale = set()
        for name, mod in list(sys.modules.items()):
            if "." in name or not is_path_module(mod):
                continue
            mod_spec = mod.__spec__
            if mod_spec is None:
                continue
            spec = PathFinder.find_spec(name, [path0, *rest])
            if (
                spec is None
                or spec.origin != mod_spec.origin
                or list(spec.submodule_search_locations or ())
                != list(mod_spec.submodule_search_locations or ())
            ):
                stale.add(name)
    if not stale:
        return
    for name in list(sys.modules):
        if name.partition(".")[0] in stale:
            del sys.modules[name]
