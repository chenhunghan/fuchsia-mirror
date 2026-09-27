# Validating driver changes in QEMU

## High-level process

Use the steps below to validate a code change in QEMU. Each of the steps below
references one of the procedures defined in the following section.

1. Start a QEMU emulator running a Fuchsia build.

2. Extract the virtual display resolution from the emulator configuration.

3. Launch the Fuchsia tool `display-tool info`. Check that the output includes
   one display whose resolution matches the virtual display resolution.

4. Take a screenshot and check that it looks as expected. `workbench_eng`
   products show a Virtcon console with a Fuchsia logo ASCII art made out of `f`
   letters.

5. Launch the Fuchsia tool `display-tool squares`.

6. Take a screenshot. Check that it contains four colored squares, which may
   overlap.

7. Check for errors and inconsistencies in the driver's own output. The driver
   reports its activity below the default severity threshold, so this requires
   the procedure for collecting the display driver's logs.

8. Check for ERROR entries (software implementation errors) in the serial logs.

9. Check for ERROR entries (software implementation errors) in the Fuchsia
   system logs.

## Procedures for interacting with the emulator

### Launching a QEMU instance running a Fuchsia build

Steps:

1. Ensure the emulator is stopped: `ffx emu stop --all`

2. Build the project (may take a few minutes): `fx build`

3. Create the logs storage directory: `mkdir -p local`

4. Delete any old logs: `rm -f local/logs.qemu.*`

5. Start the emulator, saving QEMU tool logs:
   `ffx emu start --engine qemu --headless --log local/logs.qemu`

6. Wait for `ffx` to exit. The last output line should be: `Emulator is ready.`

7. Wait for the emulator ffx connection to stabilize: `ffx target wait`

`ffx emu start` command arguments breakdown:

* `--engine qemu` required, as the default engine is FEMU (Fuchsia Emulator)

* `--log local/logs.qemu` sets the QEMU log path; the serial log path is
  computed by appending `.serial` to this value, obtaining
  `local/logs.qemu.serial`

* `--headless` optional in a graphical environment

### Launching a Fuchsia shell tool in the emulator

<!-- TODO(https://fxbug.dev/518978889): Add instructions to cover tools not
     included in the package cache by default. -->

Use the step below to run driver development tools in the emulator, such as
`display-tool` and `display-tweak`.

1. Run the command: `ffx target ssh -- {COMMAND} [{ARGUMENTS...}]`.
   Example: `ffx target ssh -- display-tool info`


### Obtaining emulator configuration details

Options:

* `ffx emu show --cmd` produces a compact description of the emulated hardware
* `ffx emu show` outputs all available information

### Collecting logs from the Fuchsia build

Example commands for analyzing serial logs:

* `grep --context=3 "virtio-gpu-display" local/logs.qemu.serial`
* `grep --context=3 "ERROR" local/logs.qemu.serial`

Example sequence of commands for saving and analyzing system logs:

1. `ffx log dump > local/logs.qemu.fuchsia`
2. `grep --context=3 "ERROR" local/logs.qemu.fuchsia`

Tool dependencies:

* `ffx emu start` configures QEMU to save serial logs at a predetermined
  location. The example `ffx emu start --engine qemu --log local/logs.qemu`
  saves serial logs at `local/logs.qemu.serial`. These logs are available even
  if the ffx connection to the emulator does not work.

* `ffx log dump` requires a working ffx connection.

### Collecting the display driver's logs

The driver traces its activity, such as the FIDL calls it serves, at the DEBUG
level. DEBUG is below the default severity threshold, so these entries are
missing from the serial logs and from `ffx log dump` until the driver's minimum
severity is lowered.

Steps:

1. Start streaming the driver's logs:
   `ffx log --set-severity "bootstrap/**#DEBUG" --force-set-severity --tag
   virtio-gpu-display-rust --severity debug`

2. Reproduce the behavior under investigation while the command above runs.

`ffx log` command arguments breakdown:

* `--tag virtio-gpu-display-rust` selects the entries logged by the Rust
  driver. The C++ driver uses the `virtio-gpu-display` tag.

* `--set-severity` lowers the minimum severity emitted by the components
  matching a selector; `--force-set-severity` passes the selector through
  without filtering

* `--severity debug` lowers the severity threshold applied to the entries that
  `ffx log` displays

Lowering the driver's minimum severity also routes the DEBUG entries to the
serial log, so `grep --context=3 "virtio-gpu-display-rust"
local/logs.qemu.serial` reports them after the steps above.

Tool dependencies:

* The `--set-severity` selector must cover the driver's component moniker. The
  moniker names the PCI device that the driver binds to, and therefore depends
  on the emulated PCI topology. `ffx component list | grep virtio-gpu-display`
  prints it.

* The driver's moniker is easily confused with the Display Coordinator's
  moniker. The Display Coordinator binds to the node published by the display
  driver, so its moniker extends the display driver's moniker with a
  `virtio-gpu-display` segment. For example, a display driver at
  `bootstrap/boot-drivers:PCI0.bus.00_04_0.00_04_0` is paired with a Display
  Coordinator at
  `bootstrap/base-drivers:PCI0.bus.00_04_0.00_04_0.virtio-gpu-display`.

### Obtaining a screenshot from the emulator

Sequence of steps for looking at the emulator screen.

1. `ffx emu screenshot --output local/screenshot.qemu.png`
2.  Read the binary file `local/screenshot.qemu.png` with a tool that allows you
    to interpret its visual content.

Tool dependencies:

* `ffx emu screenshot` takes a screenshot using emulator infrastructure, and
  does not rely on any software running inside the emulator.
