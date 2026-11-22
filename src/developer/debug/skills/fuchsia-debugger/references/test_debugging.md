# Target test workflow

In agent debug mode, `fx test` starts `zxdb` and the background daemon
automatically, attaches to target test processes, and stops the session on exit.
`--agent-debugging-mode`, `--break-on-failure`, and `--breakpoint` support
target tests only and do not work with `--host`. For subcommand flags and JSON
schemas, see [Command and JSON protocol reference](command_reference.md).

## Step 1: Start the test in the background

Run `fx test` with `--agent-debugging-mode` in the background so that you can
poll events while you monitor test output:

```posix-terminal
fx test <test_suite_name> --agent-debugging-mode
```

Note: By default, `fx test` runs `fx build` before it starts the debugger
daemon. Wait for compilation to finish (or build first and pass `--no-build`)
before you poll `wait-for-event`.

## Step 2: Poll for target events

After compilation finishes, poll the daemon for state transitions with
`wait-for-event`. Start with `--last-seen-seq 0`. On later calls, set
`--last-seen-seq` to the highest `"seq"` returned:

```posix-terminal
fx debug cli wait-for-event --last-seen-seq 0 --timeout 10
```

Note: After `fx test` finishes compilation, the daemon can take 1 to 2 seconds
to bind its socket.

If the first call after compilation fails with `Daemon socket not found`, wait
briefly. Then run `wait-for-event` again.

When a test crashes or hits a breakpoint, `wait-for-event` returns a `"stopped"`
event:

```json
{
  "success": true,
  "events": [
    {
      "seq": 1,
      "event": "stopped",
      "body": {
        "thread_id": 1,
        "reason": "breakpoint"
      }
    }
  ]
}
```

Caution: Monitor the background `fx test` task while you poll `wait-for-event`.

## Step 3: Handle execution outcomes

Match the daemon response and `fx test` status to one of these four outcomes:

### Scenario A: Test failure or breakpoint hit

When `wait-for-event` returns a `"stopped"` event, follow these steps to inspect
the paused target:

1.  **Query session state.** List active threads and attached processes:

    ```posix-terminal
    fx debug cli get-state
    ```

    Example response:

    ```json
    {
      "success": true,
      "body": {
        "threads": [
          { "thread_id": 1, "name": "initial-thread" }
        ],
        "processes": { "12345": "my_test_binary" }
      }
    }
    ```

2.  **Inspect stack traces.** Retrieve frames for one thread (`-t`) or across
    all threads in a process (`-p`):

    ```posix-terminal
    # Single thread stack trace
    fx debug cli stack-trace -t 1

    # Process-wide stack traces across all threads in pid 12345
    fx debug cli stack-trace -p 12345
    ```

    Example thread response:

    ```json
    {
      "success": true,
      "body": {
        "thread_id": 1,
        "stack_frames": [
          {
            "frame_index": 0,
            "name": "my_test_function",
            "source": {
              "name": "main_test.cc",
              "path": "/src/main_test.cc"
            },
            "line": 42,
            "column": 1
          }
        ],
        "total_frames": 1
      }
    }
    ```

    Example process response:

    ```json
    {
      "success": true,
      "body": {
        "process_id": 12345,
        "stacks": [
          {
            "thread_id": 1,
            "stack_frames": [
              {
                "frame_index": 0,
                "name": "my_test_function",
                "source": {
                  "name": "main_test.cc",
                  "path": "/src/main_test.cc"
                },
                "line": 42,
                "column": 1
              }
            ],
            "total_frames": 1
          },
          {
            "thread_id": 2,
            "stack_frames": [
              {
                "frame_index": 0,
                "name": "worker_thread_entry",
                "source": {
                  "name": "worker.cc",
                  "path": "/src/worker.cc"
                },
                "line": 105,
                "column": 5
              }
            ],
            "total_frames": 1
          }
        ]
      }
    }
    ```

3.  **Inspect local variables.** Read arguments and locals in a stack frame:

    ```posix-terminal
    fx debug cli variables -t 1 --frame-index 0
    ```

4.  **Evaluate expressions or run console commands.** Pass an expression
    directly to `evaluate` without a `print` prefix:

    ```posix-terminal
    fx debug cli evaluate -t 1 --frame-index 0 "my_var"
    ```

    Example response:

    ```json
    {
      "success": true,
      "body": {
        "result": "42",
        "type": "int"
      }
    }
    ```

    If the target thread is active, `variables`, `evaluate`, and `console -t`
    pause it automatically and change stack frame indices. Before you pass
    `--frame-index`, run `stack-trace` on the paused thread.

    For `zxdb` commands without a dedicated subcommand (such as symbol lookup or
    disassembly), run `console`:

    ```posix-terminal
    fx debug cli console -t 1 --frame-index 0 sym-info my_var
    ```

5.  **Stop the session.** After you finish diagnostics, stop the daemon. The
    daemon detaches from targets so that `fx test` exits cleanly:

    ```posix-terminal
    fx debug cli stop
    ```

### Scenario B: Clean pass

When all tests pass, the background `fx test` task exits with code 0 and reports
`FAILED: 0`.

- **Connection error edge case:** On a clean exit, `fx test` stops the
  background daemon automatically. Any active `wait-for-event` call terminates
  with a socket connection error (such as connection refused or closed socket).
- **Required action:** If `wait-for-event` returns a connection error, inspect
  the exit code of the background `fx test` task. If `fx test` exited with code
  0, treat the run as a clean pass. Do not run `fx debug cli stop`.

### Scenario C: Proactive breakpoints on tests that pass

Without `--breakpoint`, `fx test` attaches weakly (`attach --weak --recursive`).
Weak attach defers symbol loading until an exception occurs, so dynamic `break`
requests stay pending and unverified until a crash.

To debug a code path that passes or pause before a failure, follow these steps:

1.  Pass `--breakpoint <source_file>:<line>` (relative to the workspace root) or
    `--breakpoint <function_symbol>` to force a strong attach and load symbols
    immediately:

    ```posix-terminal
    fx test <test_name> --agent-debugging-mode --breakpoint <source_file>:<line>
    ```

    Note: `fx test` passes `--breakpoint` directly to `zxdb`, so those startup
    breakpoints do not appear in `get-state` output. Because `--breakpoint`
    forces a strong attach, later `fx debug cli break` commands also verify
    immediately.

2.  After compilation finishes, poll `wait-for-event` until the `"stopped"`
    event arrives.
3.  Inspect the target state. Then resume execution with
    `fx debug cli continue <tid>`.
4.  Poll `wait-for-event` again until `fx test` exits cleanly (Scenario B), or
    run `fx debug cli stop` to end the session early.

### Scenario D: Unresponsive test

If a test stalls without a crash and `wait-for-event` times out, follow these
steps:

1.  Run `fx debug cli get-state` to find the process KOID in `"processes"`.
2.  Run `fx debug cli diagnose-hang -p <pid>` to classify blocked threads, open
    handles, and async task trees (see [Live processes and
    drivers](live_processes_and_drivers.md)), or run
    `fx debug cli stack-trace -p <pid>`.
3.  When you finish diagnostics, run `fx debug cli stop`.
