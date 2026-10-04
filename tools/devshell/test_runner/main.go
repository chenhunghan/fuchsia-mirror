// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path"
	"path/filepath"
)

var testscript = flag.String("testscript", "", "test script to execute. Relative paths are relative to the location of the running script. Absolute paths are absolute.")
var testroot = flag.String("testroot", "", "Root directory of the files needed to execute the test.")

// This is a wrapper for running unit tests.
func main() {
	flag.Parse()

	var (
		theTest = *testscript
		theRoot = *testroot
	)
	dir, err := filepath.Abs(filepath.Dir(os.Args[0]))
	if err != nil {
		fmt.Fprintf(os.Stderr, "Could not determine execution path: %v\n", err)
		os.Exit(1)
	}
	if !filepath.IsAbs(theTest) {
		theTest = path.Join(dir, theTest)
	}
	if !filepath.IsAbs(theRoot) {
		theRoot = path.Join(dir, theRoot)
	}
	// Make sure the test is executable.
	if err := os.Chmod(theTest, 0755); err != nil {
		fmt.Fprintf(os.Stderr, "Chmod %v failed: %v\n", theTest, err)
		os.Exit(1)
	}
	cmd := exec.Command(theTest)
	cmd.Dir = theRoot
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	cmd.Stdin = os.Stdin
	if err := cmd.Run(); err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			os.Exit(exitErr.ExitCode())
		}
		fmt.Fprintf(os.Stderr, "%v failed: %v\n", theTest, err)
		os.Exit(1)
	}
}
