// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The shac_test_runner command runs `shac test` on Starlark test files and reports the
// results of individual test cases to Fuchsia test runners.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"syscall"

	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"go.fuchsia.dev/fuchsia/tools/testing/testrunner/constants"
)

func usage() {
	fmt.Fprintf(flag.CommandLine.Output(), `shac_test_runner [flags] [--] [shac test args...]

Runs "shac test" in a checkout root and, if $%s is set, writes a summary of
the individual test case results to that path.

All positional arguments are passed through to "shac test".

Flags:
`, constants.TestOutputSummaryPathEnvKey)
	flag.PrintDefaults()
}

// resolveRoot returns the checkout root in which to run `shac test`.
func resolveRoot(rootFlag string) (string, error) {
	if rootFlag != "" {
		return filepath.Abs(rootFlag)
	}
	// host_test() stages all data dependencies at their workspace-relative
	// paths under the main repository's runfiles directory, so that directory
	// is a self-contained mini-checkout.
	for _, env := range []string{"RUNFILES_DIR", "TEST_SRCDIR"} {
		if dir := os.Getenv(env); dir != "" {
			return filepath.Abs(filepath.Join(dir, "_main"))
		}
	}
	return "", errors.New("could not determine checkout root: --root, $RUNFILES_DIR and $TEST_SRCDIR are all unset")
}

func mainImpl() (int, error) {
	// Set up signal handling here rather than in main() so that cancel()
	// runs before main() calls os.Exit, which skips deferred calls.
	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer cancel()

	flag.Usage = usage
	rootFlag := flag.String("root", "", "Checkout root in which to run shac. Defaults to $RUNFILES_DIR/_main.")
	shacFlag := flag.String("shac", "prebuilt/tools/shac/shac", "Path to the shac binary, relative to the checkout root.")
	flag.Parse()

	root, err := resolveRoot(*rootFlag)
	if err != nil {
		return 1, err
	}

	jsonOut, err := os.CreateTemp("", "shac-test-*.json")
	if err != nil {
		return 1, err
	}
	if err := jsonOut.Close(); err != nil {
		return 1, err
	}
	defer os.Remove(jsonOut.Name())

	shac := *shacFlag
	if !filepath.IsAbs(shac) {
		shac = filepath.Join(root, shac)
	}
	args := append([]string{"test", "-C", root, "--json-output", jsonOut.Name()}, flag.Args()...)
	cmd := exec.CommandContext(ctx, shac, args...)
	// shac resolves relative test file paths against its working directory
	// rather than the -C root.
	cmd.Dir = root
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	retCode := 0
	var runErr error
	if err := cmd.Run(); err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			// shac has already reported the failure details on stderr.
			retCode = exitErr.ExitCode()
			// ExitCode() is -1 if shac was killed by a signal.
			if retCode < 0 {
				retCode = 1
			}
		} else {
			retCode = 1
			runErr = fmt.Errorf("failed to run shac: %w", err)
		}
	}

	if summaryPath := os.Getenv(constants.TestOutputSummaryPathEnvKey); summaryPath != "" {
		if err := writeSummary(jsonOut.Name(), summaryPath); err != nil {
			return max(retCode, 1), errors.Join(runErr, err)
		}
	}
	return retCode, runErr
}

// writeSummary converts the `shac test --json-output` results at resultsPath
// into a Fuchsia test summary at summaryPath.
func writeSummary(resultsPath, summaryPath string) error {
	data, err := os.ReadFile(resultsPath)
	if err != nil {
		return err
	}
	// shac only writes the results file once all tests have run, so if shac
	// failed before then (e.g. due to a syntax error), the file is empty and
	// the test's overall exit code is all there is to report.
	if len(data) == 0 {
		return nil
	}
	cases, err := parseResults(data)
	if err != nil {
		return err
	}
	if err := jsonutil.WriteToFile(summaryPath, runtests.TestResult{Cases: cases}); err != nil {
		return fmt.Errorf("failed to write test summary: %w", err)
	}
	return nil
}

func main() {
	retCode, err := mainImpl()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
	os.Exit(retCode)
}
