# Python forkserver

A per-build server that starts the Python interpreter and imports common modules
once, then runs each Python build action in a `fork()`ed copy of itself. Each
action still gets its own process; it just skips interpreter startup and
the imports the server already did.

Design doc: [go/fuchsia-python-forkserver](http://go/fuchsia-python-forkserver).

* `server.py`: the forkserver's controller: warms up, starts the forkers, and
  merges their stats when it stops.
* `forker.py`: a forker process: accepts requests and forks a worker for each.
* `stats.py`: the counters, per forker and merged.
* `worker.py`: warms the server up, and turns a forked worker into the process
  that runs the script.
* `client.py`: the launcher. `python3 -S client.py SCRIPT ARGS` runs `SCRIPT` on
  the server when one is advertised, or execs `python3 -S SCRIPT ARGS`
  otherwise.
* `setting.py`: reads the `//.fx/config/python_forkserver` opt-in.
* `example/`: a toy build action, and how to run it on a server by hand.

Linux only.

## Cheat sheet

| Want to... | Do |
|---|---|
| Run a script on the server | `FX_PYTHON_FORKSERVER_SOCKET=NAME python3 -S client.py SCRIPT ARGS` |
| Run it without the server | Unset `FX_PYTHON_FORKSERVER_SOCKET` (same command) |
| Change parallelism | `server.py --forkers N` (default: `min(8, cpu_count)`) |
| Get totals on exit | `server.py --stats FILE` (one JSON line) |
| Stop the server | `SIGTERM` (or `SIGINT`) to the controller (`server.py`) |

## What runs where

The server is a controller process (`server.py`) and `--forkers` forker
processes (`forker.py`). The controller binds the socket, warms up, forks the
forkers, and waits for `SIGTERM` (or `SIGHUP` or `SIGINT`). Each forker accepts
connections and forks a worker per request, which runs `SCRIPT` (`worker.py`).
On `SIGTERM`, the controller shuts the socket down, each forker waits for its
workers and writes its stats to `<stats>.<pid>`, and the controller merges them
into `<stats>` (`stats.py`).

```
server.py (controller)
  bind → warm up → ready byte → fork N forkers → sigwait() until SIGTERM/HUP/INT
  └─ forker × N  loop: accept → check peer uid → send READY → fork worker
      │                → send PID
      │          on SIGCHLD (via a wakeup fd): reap worker → send STATUS → close
      └─ worker (one per action)
           receive request + client's stdio fds
           → set argv/env/cwd/umask/sys.path as `python3 -S SCRIPT` would
           → run SCRIPT in-process → exit with its status

client.py (one per action)
  connect → wait READY → send request + stdio fds → wait PID
  → forward SIGINT/SIGTERM/SIGHUP to the worker → wait STATUS
  → exit with the same status (or die from the same signal)
```

The client is a messenger: it sends `SCRIPT`, its arguments, the environment,
working directory, umask, and stdio to the server, and nothing about its own
interpreter. Served scripts run with the server's interpreter and startup flags
(`-S -W error::SyntaxWarning` in the build), and `PYTHON*` startup variables
such as `PYTHONHASHSEED` and `PYTHONUTF8` are read once, when the server starts,
not from each request (except for those listed below). The server needs its own
directory on `sys.path`, so don't start it with `-P` or `PYTHONSAFEPATH`. Each
worker gives `SCRIPT` the module search path that `python3 -S SCRIPT` would
compute from the request's `PYTHONPATH`, plus its environment, working
directory, umask, stdio, bytecode settings (`PYTHONDONTWRITEBYTECODE`,
`PYTHONPYCACHEPREFIX`), and a fresh `__main__`. `SIGINT`, `SIGTERM`, and
`SIGHUP` sent to the client reach the worker's process group, and the client
exits with the worker's status (or dies from the same signal).

So in GN, only put `client.py` in `python_exe_src -S` commands, right after
the interpreter flags. A served script gets the server's flags, including
`-W error::SyntaxWarning`, even if its command line only has `-S`.

When `FX_PYTHON_FORKSERVER_SOCKET` is unset or empty, the client warns once per
directory (it creates `.python_forkserver_slow_path` there) and execs `SCRIPT`
with the command line's own interpreter and flags.

Everything else fails the action, with a message saying what to do (usually,
how to turn the forkserver off):

* the server isn't running, closes the connection, or doesn't answer within
  2 s,
* the client's stdin, stdout, or stderr is closed, or
* the request comes from another working directory than the one the server
  warmed up in (for example, a nested build).

## Protocol

One request per connection on an abstract `AF_UNIX` stream socket. The
constants and `Request`, the request encoding, live in `client.py`, which
`server.py` imports.

Server-to-client messages are 5 bytes: a tag byte and an i32le value.

1. Server to client: `R` (value 0) once the connection is accepted from a
   client running as the same user. The client only sends its stdio fds after
   this, so a hung server's queue of unaccepted connections never holds pipe
   write ends open.
2. Client to server: a u32le length, then NUL-separated fields: the magic
   string, cwd, umask, argc, `argv[0..argc)`, and `KEY=VALUE` environment
   entries. File descriptors 0, 1, and 2 ride along as `SCM_RIGHTS` on the
   first `sendmsg()`. The server closes the connection on a malformed request.
3. Server to client:
   * `P`: the worker's pid, which is also its process group.
   * `S`: the worker's wait status.

## Testing

The tests run during the build, the same way CQ runs them:

```sh
fx build //build/scripts/python_forkserver:tests
```

To iterate on one test file without a build, run it the way the build does:
with `python3 -S`, `PYTHONPATH` set to the checkout root, and no `PATH`.

```sh
cd build/scripts/python_forkserver
env -i HOME="$HOME" PYTHONPATH="$PWD/../../.." \
  ../../../prebuilt/third_party/python3/linux-x64/bin/python3 -S client_test.py
```

Build actions may have no `PATH` and may set `PYTHONPYCACHEPREFIX`, so tests
start helper processes with `sys.executable` rather than host tools like
`sleep`, and don't assume where bytecode caches go.

## Debugging

* Server log: whatever you redirect the server's stderr to. It prints one line
  at warm-up and a stats line on exit.
* If one of the server's forker processes exits early (for example, on
  EMFILE), the server logs which one, stops, and exits 1; actions after that
  fail with "isn't running".
* To compare against a plain run, unset `FX_PYTHON_FORKSERVER_SOCKET` and rerun
  the same command.
