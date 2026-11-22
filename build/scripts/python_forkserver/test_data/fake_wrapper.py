# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Stand-in for the //build/rbe/*_remote_wrapper.py scripts in server_test.py.

server_test.py copies this file into its fake checkout's //build/rbe under
each RBE wrapper's name, and runs it both through the forkserver and with
plain `python3`, expecting the same results. What matters for the tests:

- Like the real wrappers, it imports `remote_action`, a sibling module in
  //build/rbe (see remote_action.py here), and its __main__ block calls
  remote_action.init_from_main_once() before main(argv, env). `echo` reports
  __name__, whether __main__ is this module, and the init count, which show
  whether each run gets its own __main__ and remote_action state.
- It imports standard library modules (asyncio, subprocess, signal) whose
  state a forked worker must not carry over from the server.
- Its first argument picks what to do: print its argv, cwd, umask, and
  environment; exit with a code; raise; get interrupted; report its signal
  handlers; run a subprocess or asyncio; write a file; kill itself; sleep;
  or wait for its stdin to close.
"""

import asyncio
import json
import os
import signal
import subprocess
import sys
import time
from collections.abc import Mapping

import remote_action


def main(argv: list[str], env: Mapping[str, str]) -> int:
    cmd = argv[0] if argv else "noop"
    if cmd == "noop":
        return 0
    if cmd == "sigs":
        print(
            [
                str(signal.getsignal(s))
                for s in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
            ]
        )
        return 0
    if cmd == "echo":
        um = os.umask(0o022)
        os.umask(um)
        payload = {
            "argv": sys.argv,
            "cwd": os.getcwd(),
            "umask": um,
            "main_name": __name__,
            "is_main_mod": sys.modules.get("__main__") is sys.modules[__name__],
            "init_count": remote_action.INIT_COUNT,
            "env_x": env.get("X"),
            "args": argv[1:],
        }
        print(json.dumps(payload, sort_keys=True))
        sys.stderr.write("err:" + ",".join(argv[1:]) + "\n")
        return 0
    if cmd == "exit":
        return int(argv[1])
    if cmd == "sysexit":
        sys.exit(int(argv[1]))
    if cmd == "raise":
        raise RuntimeError("boom-from-wrapper")
    if cmd == "interrupt":
        import atexit

        atexit.register(print, "atexit-ran")
        sys.stdout.write("buffered\n")
        raise KeyboardInterrupt
    if cmd == "subproc":
        r = subprocess.run(
            [
                sys.executable,
                "-S",
                "-c",
                "import sys; print('sub:' + sys.argv[1])",
                argv[1],
            ],
            capture_output=True,
            text=True,
            check=True,
        )
        sys.stdout.write(r.stdout)
        return 0
    if cmd == "asyncio":

        async def _go() -> str:
            await asyncio.sleep(0.005)
            return "async-ok:" + os.environ.get("X", "")

        print(asyncio.run(_go()))
        return 0
    if cmd == "write":
        target = argv[1]
        with open(target, "a") as f:
            f.write("written:" + os.environ.get("X", "") + "\n")
        return 0
    if cmd == "self_sigkill":
        os.kill(os.getpid(), 9)
        return 0
    if cmd == "sleep":
        time.sleep(float(argv[1]))
        return 0
    if cmd == "wait_stdin":
        sys.stdin.read()
        return 0
    if cmd == "ready_sleep":
        print("ready", flush=True)
        time.sleep(float(argv[1]))
        return 0
    return 2


if __name__ == "__main__":
    remote_action.init_from_main_once()
    sys.exit(main(sys.argv[1:], os.environ))
