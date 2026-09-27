# Driver Debug CLI (`debug`)

`debug` is a command-line utility for interacting with drivers that implement the
[`fuchsia.driver.debug.Debug`](/sdk/fidl/fuchsia.driver.debug/debug.fidl) FIDL protocol.
It allows developers to discover supported debug commands on target drivers and execute them
with arbitrary arguments, streaming stdout and stderr back to the terminal.

## How it works

Drivers that implement the debug protocol serve `fuchsia.driver.debug.Debug` in their outgoing
service directory (e.g. `/out/svc/fuchsia.driver.debug.Debug`).

The `debug` binary connects to the debug protocol, binds stdout and stderr to Fuchsia sockets,
and sends execution requests to the driver via the `Execute` FIDL method. It can also query
the driver's list of registered commands via `ListCommands`.

### Default Path Resolution

When `debug` is run without specifying a custom path:
1. It first checks for the existence of `/out/svc/fuchsia.driver.debug.Debug`.
2. If not found or connection fails, it falls back to `/svc/fuchsia.driver.debug.Debug`.

This makes `debug` work seamlessly inside `ffx component explore` without needing to specify
any service paths.

## Usage with `ffx component explore`

The recommended and primary way to use `debug` is via `ffx component explore <driver_moniker>`.
In an explore container, the target driver component's outgoing directory is mounted directly
at `/out`. Because `debug` automatically checks `/out/svc/fuchsia.driver.debug.Debug`, no path
flags are necessary.

### 1. Interactive Exploration

Launch an interactive explore shell on the driver component:

```bash
ffx component explore bootstrap/boot-drivers:dev.sys.platform.00_00_2d
```

Inside the explore shell:

```bash
# List available debug commands supported by the driver:
$ debug --list-commands
# or
$ debug list-commands

# Execute a command:
$ debug ping
$ debug reset --hard
```

### 2. Non-interactive (One-off) Execution

You can run debug commands directly from your host workstation using `ffx component explore --command`:

```bash
# List supported debug commands:
ffx component explore <driver_moniker> --command "debug --list-commands"

# Execute a specific command:
ffx component explore <driver_moniker> --command "debug ping"
```

## Command Line Options

```text
Usage: debug [-s <service>] [--list-commands] [args...]

Debug CLI tool to interact with drivers via fuchsia.driver.debug.Debug. This
tool is typically invoked inside an explore shell on a driver component:   `ffx
component explore <driver_moniker>` or non-interactively via:   `ffx component
explore <driver_moniker> --command "debug ..."` Inside `component explore`, the
driver's outgoing directory is mounted at `/out`, allowing `debug` to connect to
`/out/svc/fuchsia.driver.debug.Debug` by default.

Options:
  -s, --service     path to the fuchsia.driver.debug.Debug service node.
                    Defaults to `/out/svc/fuchsia.driver.debug.Debug` or
                    `/svc/fuchsia.driver.debug.Debug`.
  --list-commands   list all available debug commands supported by the driver.
  --help, help      display usage information
```

### Custom Service Path

If the driver's debug protocol is exposed at a non-standard path or in a client component's incoming
namespace (`/svc/...`), pass `-s` or `--service`:

```bash
debug -s /custom/path/to/fuchsia.driver.debug.Debug --list-commands
debug -s /custom/path/to/fuchsia.driver.debug.Debug ping
```
