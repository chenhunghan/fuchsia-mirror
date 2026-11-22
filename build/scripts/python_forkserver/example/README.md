# Run a script on a Python forkserver by hand

`toy_action.py` is a small stand-in for a Python build action: it parses its
arguments, hashes its inputs, and writes a JSON output and, optionally, a
depfile.

To run it, or any other script, on a server, start from your build directory:
the server warms up in the directory it starts in, and only serves requests
from there.

```sh
cd out/default
PY=../../prebuilt/third_party/python3/linux-x64/bin/python3
FS=../../build/scripts/python_forkserver

# 1. Start a server, and wait until it logs that it has warmed up. Any unique
#    name works: it's an abstract AF_UNIX socket, so there's no file to clean
#    up.
SOCK="fx-python-forkserver-$USER-manual"
"$PY" -S -W error::SyntaxWarning "$FS/server.py" --socket-name "$SOCK" \
  --stats /tmp/forkserver.stats.json 2> /tmp/forkserver.log &
SERVER_PID=$!
until grep -q imported /tmp/forkserver.log; do sleep 0.1; done

# 2. Run scripts through the launcher. Served scripts run with the server's
#    interpreter flags, whatever the client's are. PYTHONPATH is what
#    `fx build` gives actions; scripts that import `build.*` need it with or
#    without the server.
export FX_PYTHON_FORKSERVER_SOCKET="$SOCK"
export PYTHONPATH="$PWD/../.."
"$PY" -S "$FS/client.py" ../../build/rbe/cxx_remote_wrapper.py --help
"$PY" -S "$FS/client.py" "$FS/example/toy_action.py" \
  --output /tmp/toy_action.json "$FS/README.md" "$FS/client.py"

# 3. Stop the server (it stops its forkers and merges their stats), then read
#    the stats.
kill -TERM "$SERVER_PID"
wait "$SERVER_PID"
cat /tmp/forkserver.stats.json
```

Without `FX_PYTHON_FORKSERVER_SOCKET`, the same `client.py` command warns once
per directory and execs `"$PY" -S SCRIPT ARGS`.
