# print-input-report

A tool to inspect input devices and reports via the FIDL protocol
`fuchsia.input.report.InputDevice`.

## Subcommands

### `list`

Lists connected devices.

Options:
* `--output <table|csv|json>`: Output format (default: `table`).

### `get-descriptor`

Reads device descriptors.

Options:
* `--output <text|json>`: Output format (default: `text`).
* `--instance <instance>`: Instance name to filter by (e.g. `000`).
* `--vendor <0xhex>`: Vendor ID to filter by in hexadecimal format (e.g. `0x1234`).
* `--product <0xhex>`: Product ID to filter by in hexadecimal format (e.g. `0x5678`).

### `read`

Reads input reports via `InputReportsReaderV2`.

Options:
* `--output <text|json>`: Output format (default: `text`).
* `--instance <instance>`: Instance name to filter by (e.g. `000`).
* `--vendor <0xhex>`: Vendor ID to filter by in hexadecimal format (e.g. `0x1234`).
* `--product <0xhex>`: Product ID to filter by in hexadecimal format (e.g. `0x5678`).
* `--num-reads <num>`: Total number of reports to read from all devices. If omitted, reads
  continuously.

## Usage Examples

### List devices

List all connected devices in table format:
```posix-terminal
ffx target ssh -- print-input-report list
```

List devices in CSV format:
```posix-terminal
ffx target ssh -- print-input-report list --output csv
```

List devices in JSON format:
```posix-terminal
ffx target ssh -- print-input-report list --output json
```

### Get descriptors

Read descriptors for all connected devices:
```posix-terminal
ffx target ssh -- print-input-report get-descriptor
```

Read descriptor for a specific device instance:
```posix-terminal
ffx target ssh -- print-input-report get-descriptor --instance 000
```

Read descriptor in JSON format filtered by vendor and product ID:
```posix-terminal
ffx target ssh -- print-input-report get-descriptor --vendor 0x18d1 --product 0x5036 --output json
```

### Read input reports

Continuously stream input reports from all devices:
```posix-terminal
ffx target ssh -- print-input-report read
```

Read 5 reports from a specific device instance:
```posix-terminal
ffx target ssh -- print-input-report read --instance 000 --num-reads 5
```

Read reports in JSON format filtered by vendor and product ID:
```posix-terminal
ffx target ssh -- print-input-report read --vendor 0x18d1 --product 0x5036 --output json
```

## Testing

Run unit tests for `print-input-report`:
```posix-terminal
fx test print-input-report-tests
```
