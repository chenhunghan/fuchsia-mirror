# Breakpoints, watchpoints, and tracepoints

If the target process can pause, use breakpoints (`break`) or hardware
watchpoints (`watch`). Otherwise, use non-stopping `tracepoint` probes. For full
CLI and JSON schemas, see [Command and JSON protocol
reference](command_reference.md).

## Source and function breakpoints

Use `break` (aliases `b`, `breakpoint`) to pause execution at a source line or
function symbol. Pass `-c` (`--condition`) to filter hits or `-d` (`--delete`)
to remove a breakpoint:

```posix-terminal
# Set a conditional function breakpoint
fx debug cli break --function usb_xhci::UsbXhci::Init --condition "status != 0"

# Set and delete a source-line breakpoint (workspace-relative path in CLI mode)
fx debug cli break src/devices/usb/drivers/xhci/usb-xhci.cc:142
fx debug cli break src/devices/usb/drivers/xhci/usb-xhci.cc:142 -d
```

Note: CLI `break` and `tracepoint` resolve workspace-relative and CWD-relative
paths to absolute host paths automatically, whereas `--json` `"file"` requires
an absolute host path. On a weak attach (`--weak`), breakpoints stay pending
(`verified: false`) until an exception occurs or `modules` switches the process
to a strong attach.

## Hardware watchpoints

Use `watch` (aliases `watchpoint`, `data-breakpoint`) to pause execution when
hardware detects a memory write (`-a write`) or read/write (`-a readWrite`).
`zxdb` does not support read-only watchpoints.

When you set a watchpoint, pass `-t <tid>` and `-a <write|readWrite>` (with
optional `--frame-index <idx>`, `-c <condition>`, and `--hit-condition <expr>`).
If the thread is active, the daemon pauses it to evaluate the target expression.
To watch a raw address, pass a typed pointer dereference expression:

```posix-terminal
# Watch writes to a variable in frame 0 of thread 1
fx debug cli watch -t 1 -a write my_struct.counter

# Watch reads and writes to a fixed 32-bit address
fx debug cli watch -t 1 -a readWrite "*(uint32_t*)0x7ffee1234000"

# List active watchpoints (data_id and expression) via get-state, then delete
fx debug cli get-state
fx debug cli watch -d <data_id>
fx debug cli watch --clear
```

Caution: Before you step out of a function, delete watchpoints on its local
stack variables. `zxdb` evaluates a watchpoint expression once and watches that
fixed address for the life of the process. After the function returns, later
calls reuse that stack slot and trigger false hits.

## Non-stopping tracepoints

A tracepoint (`tracepoint`, `tp`, or `trace`) captures state when a thread hits
a source line, function symbol, or hardware watchpoint (`-w`), and then resumes
the thread immediately. The daemon emits a `zxdb.Tracepoint` event to
`wait-for-event` and disables the tracepoint automatically after `--max-hits`
hits (20 by default).

### Configure and manage tracepoints

Select one target (`<file>:<line>`, `--file <f> --line <n>`, `<sym>`,
`-f <sym>`, or `-w <expr_or_hex_addr>`) and combine it with capture flags
(`-e <expr>`, `--locals`, `--registers`, `--backtrace`, `-r` for raw frames,
`--max-frames <n>` default 10, and `--max-hits <n>` default 20):

```posix-terminal
# Capture an expression and locals on a function (aliases: tp, trace)
fx debug cli tracepoint usb_xhci::UsbXhci::HandleEvent \
  -e status --locals --max-hits 10

# Capture a backtrace on a source line when a condition holds
fx debug cli tp src/devices/usb/drivers/xhci/usb-xhci.cc:142 \
  -c "ring != nullptr" --backtrace

# Set a non-pausing watchpoint tracepoint on a raw hex address
fx debug cli tp -w 0x7ffee1234000 -a readWrite --size 4

# Set an expression watchpoint tracepoint (pauses thread 1 during setup)
fx debug cli tp -w my_var -t 1 -a readWrite --size 4

# List, update, disable, re-enable (resets hit_count), delete, or clear
fx debug cli tp --list
fx debug cli tp usb_xhci::UsbXhci::HandleEvent --id 1 -e new_expr
fx debug cli tp 1 --disable
fx debug cli tp 1 --enable
fx debug cli tp 1 -d
fx debug cli tp --clear
```

Caution: Setting a watchpoint tracepoint on an expression
(`tp -w <expr> -t <tid>`) pauses the target thread during setup and leaves it
paused. On transport-sensitive driver hosts, use code-location tracepoints or
raw hex-address watchpoints (`tp -w 0x...` without `-t`).

### Read tracepoint hit events

Poll `wait-for-event` to consume `zxdb.Tracepoint` events:

```json
{
  "success": true,
  "events": [
    {
      "seq": 2,
      "event": "zxdb.Tracepoint",
      "body": {
        "tracepointId": 1,
        "location": "usb_xhci::UsbXhci::HandleEvent",
        "hitCount": 1,
        "maxHits": 10,
        "autoDisabled": false,
        "truncated": false,
        "threadKoid": 34567,
        "dwellMs": 1.4,
        "capture": {
          "matched": true,
          "frame": {
            "name": "usb_xhci::UsbXhci::HandleEvent()",
            "file": "src/devices/usb/drivers/xhci/usb-xhci.cc",
            "line": 98,
            "ip": 4398046515200,
            "sp": 140732920758272,
            "trust": "CFI"
          },
          "locals": {
            "ring": { "value": "0x20004000", "type": "usb_xhci::EventRing*" }
          },
          "evaluations": {
            "status": { "result": "0", "type": "zx_status_t", "error": null }
          },
          "errors": []
        }
      }
    }
  ]
}
```
