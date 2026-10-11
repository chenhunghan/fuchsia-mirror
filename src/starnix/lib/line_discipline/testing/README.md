# Line Discipline Trace Generator

This directory contains `trace_generator.py`, a utility for generating golden
trace files on Linux to verifying Starnix line discipline behavior.

## Usage

The generator MUST be run on a Linux host (not Fuchsia/Starnix) as it captures
the behavior of the host's PTY implementation.

### Generating Traces

To generate all trace files:

```bash
python3 src/starnix/lib/line_discipline/testing/trace_generator.py
```

This will generate trace files in `src/starnix/lib/line_discipline/testing/generated/`.

> **Note:** You must run this script manually whenever you change or add a scenario.

## Adding New Tests

1.  Add a scenario JSON file to the `scenarios/` directory.
2.  Add the name of the scenario to `scenarios_list.json`.
3.  Run `trace_generator.py` to produce the golden JSON trace in `generated/`.

## Scenario Options and Actions

Top-level scenario fields:
*   `name`: Scenario identifier.
*   `initial_termios`: Initial `c_iflag`, `c_oflag`, `c_lflag`, and `c_cc` settings.
*   `capture_signals` (optional `bool`): When `true`, spawns a child session leader attached to the replica PTY as its controlling terminal and records generated `SIGINT`, `SIGQUIT`, and `SIGTSTP` signals on each `write_to_master` event.

Supported event actions:
*   `write_to_master` / `write_to_slave`: Writes `data` (string or byte array) to the PTY end (supports `"expect_block": true`, `"expect_signal": true`, `"expect_no_change": true`, and `EIO` capture).
*   `read_from_master` / `read_from_slave`: Waits for readability (failing with `TimeoutError` on timeout) and drains all available bytes from the PTY end.
*   `read_once_from_master` / `read_once_from_slave`: Performs a single `read` call with optional `"size"` (default `4096`), capturing single-packet/canonical-line boundaries, `EAGAIN` (`_blocked` when `"expect_block": true`), or `EIO` (`_eio`), and failing with `TimeoutError` if an expected read times out.
*   `set_termios`: Updates replica `termios`.
*   `set_packet_mode`: Enables or disables `TIOCPKT` packet mode on the master.
*   `flush`: Calls `tcflush` on `"side"` (`"main"` or `"replica"`) with `"queue_selector"` (`"TCIFLUSH"`, `"TCOFLUSH"`, or `"TCIOFLUSH"`).
*   `check_readable_size`: Queries `FIONREAD` on `"side"` (`"main"` or `"replica"`).
*   `check_poll`: Queries `poll` readiness (`POLLIN`, `POLLOUT`, `POLLPRI`, `POLLERR`, `POLLHUP`) on `"side"` (`"main"` or `"replica"`).
*   `close`: Closes `"side"` (`"main"` or `"replica"`) and waits for `POLLHUP` on the remaining end.
*   `wait_until_readable`: Waits until `"side"` becomes readable (failing with `TimeoutError` on timeout).
