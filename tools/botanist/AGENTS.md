# `botanist` Development Guide for AI Agents

`botanist` sets up the Swarming bot environment, provisions and boots Fuchsia
targets (devices, emulators, GCE), manages background host services (package
server, serial/syslog streaming, SSH ControlMaster, `ffx monitor`), and runs
`testrunner` (`//tools/testing/testrunner`) in-process.

Update this guide in the same change if you modify an invariant or workflow
documented here.

---

## 1. Concurrency, Subprocesses & Target Recovery

* **Return `error` values; never add `recover()` handlers**: Reserve `panic`
  for unreachable invariants. Unhandled Go panics and runtime fatal throws are
  caught post-hoc in `//tools/testing/tefmocheck/check-configs.yaml` via
  `go_panic` and `go_fatal_error`.
* **Join background processes before teardown**: Use `botanist.WaitForProcess`
  (and bounded exit timeouts for graceful shutdowns like `ffx monitor`) so
  background processes exit before `FUCHSIA_TEST_OUTDIR` is copied and deleted.
  Pass `subprocess.RunOptions{Setpgid: true}` to `subprocess.Runner` calls so
  timed-out or canceled commands kill their child process trees.
* **Context & teardown ordering**: Derive detached cleanup contexts from
  `botanist.GetLoggerCtx(ctx)` (not `context.Background()`) to preserve
  logging. Cancel `ctx` *before* `targets.StopTargets(...)` so `t.Wait(ctx)`
  and `CaptureSerialLog` treat shutdown as intentional (`ctx.Err() != nil`).
* **Target recovery**: Never re-run `targets.StartTargets` or tear down the
  serial server mid-run (closing the serial socket causes `CaptureSerialLog`
  to hit `io.EOF` and abort the `errgroup`, and `StartSerialServer` is a no-op
  once a socket path is set). Recover SSH via `rebootTarget` (`dm reboot` or
  `dmc set-power-state --state cycle`), and never call `dumpSyslogOverSerial`
  on retry paths (`serial.RunDiagnostics` sleeps >12m).
* **Strict CLI parsing in retries**: External tools (`dmc health-check`,
  `ffx forward`) can exit 0 with empty stdout or `[]`. Reject empty outputs
  explicitly and wrap terminal parse errors with `retry.Fatal(err)` inside
  `retry.Retry` loops.

---

## 2. Logging, Errors & `tefmocheck`

* **Keep returned errors deterministic**: Top-level errors are logged as
  `botanist ERROR: botanist failed: <err>` and matched by
  `//tools/testing/tefmocheck` and LUCI Analysis. Omit temp paths, unsorted
  flags, struct dumps, and nodenames; log verbose details via `logger.Debugf`.
* **Do not wrap `constants/constants.go` errors with prefixes**:
  `tefmocheck` rewrites `botanist ERROR: <ConstantMsg>` checks to match
  `botanist ERROR: botanist failed: <ConstantMsg>`. Inserting intermediate
  prefixes (e.g., `fmt.Errorf("failed to setup: %w", err)`) breaks this match
  and collapses the failure into the generic catch-all.
* **Stdio writers**: Route subprocess output through
  `botanist.NewStdioWriters(ctx, id)` and `defer flush()`, and do not write to
  the returned `LineWriter` concurrently without locking.

---

## 3. Build & Testing

* **`BUILD.bazel` is the source of truth**: Edit `tools/botanist/BUILD.bazel`
  for sources, tests, and deps, then run `fx bazel2gn -d tools/botanist` to
  update `tools/botanist/BUILD.gn` below `## BAZEL2GN SENTINEL` (never edit
  below it by hand). `main`, `botanist`, and `botanist_cmd_tests` are
  `# @bazel2gn:skip` (Bazel-only); add any new non-skipped `host_go_test` to
  `group("tests")` above the sentinel in `BUILD.gn`.
* **Updating `FuchsiaTarget`**: Add a stub to `MockTarget` in `cmd/run_test.go`
  whenever adding an interface method (it only builds in Bazel's
  `botanist_cmd_tests`).
* **Running tests**:
  ```bash
  fx bazel test --config=host //tools/botanist:all
  # Also run when touching constants/constants.go:
  fx bazel test --config=host //tools/testing/tefmocheck:all
  ```
