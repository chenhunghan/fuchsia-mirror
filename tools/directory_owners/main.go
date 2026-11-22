// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The directory_owners command collects OWNERS and METADATA.textproto files from
// a Fuchsia checkout and writes an aggregated directory_owners.json file.
package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/lib/owners"
	"go.fuchsia.dev/fuchsia/tools/metadata"
)

// schemaVersion is the format version emitted in directory_owners.json.
const schemaVersion = 1

type directoryMetadata struct {
	ComponentID int64          `json:"component_id,omitempty"`
	Owners      []owners.Entry `json:"owners,omitempty"`
}

type snapshot struct {
	Version     int                          `json:"version"`
	Directories map[string]directoryMetadata `json:"directories"`
}

func main() {
	if err := run(os.Args[1:], os.Stderr); err != nil {
		fmt.Fprintf(os.Stderr, "directory_owners: %v\n", err)
		os.Exit(1)
	}
}

func run(args []string, stderr io.Writer) error {
	flags := flag.NewFlagSet("directory_owners", flag.ContinueOnError)
	flags.SetOutput(stderr)
	fuchsiaDir := flags.String("fuchsia-dir", "", "path to the Fuchsia checkout root directory")
	outputPath := flags.String("output", "", "path to write the output directory_owners.json file")
	if err := flags.Parse(args); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return nil
		}
		return err
	}
	if flags.NArg() > 0 {
		return fmt.Errorf("unexpected positional arguments: %v", flags.Args())
	}
	if *fuchsiaDir == "" {
		return errors.New("-fuchsia-dir is required")
	}
	if *outputPath == "" {
		return errors.New("-output is required")
	}

	snap, err := collect(*fuchsiaDir)
	if err != nil {
		return err
	}
	encoded, err := json.MarshalIndent(snap, "", "  ")
	if err != nil {
		return fmt.Errorf("marshaling directory owners JSON: %w", err)
	}
	encoded = append(encoded, '\n')

	if err := os.MkdirAll(filepath.Dir(*outputPath), 0o755); err != nil {
		return fmt.Errorf("creating output directory: %w", err)
	}
	if err := os.WriteFile(*outputPath, encoded, 0o644); err != nil {
		return fmt.Errorf("writing output: %w", err)
	}
	return nil
}

// collect walks checkoutDir and resolves ownership and component metadata for
// all eligible directories. It skips hidden dot-directories and top-level
// integration, out, and prebuilt trees, and filters third_party/,
// vendor/third_party/, and vendor/<partner>/third_party/ trees to directories
// containing a BUILD.gn, BUILD.bazel, or BUILD file.
func collect(checkoutDir string) (*snapshot, error) {
	ownersResolver, err := owners.NewResolver(checkoutDir)
	if err != nil {
		return nil, err
	}
	metadataResolver, err := metadata.NewResolver(checkoutDir)
	if err != nil {
		return nil, err
	}
	absCheckout, err := filepath.Abs(checkoutDir)
	if err != nil {
		return nil, err
	}
	resolvedRoot, err := filepath.EvalSymlinks(absCheckout)
	if err != nil {
		return nil, err
	}

	knownFiles := make(map[string]bool)
	var dirs []string
	tpBuildDirs := make(map[string]bool)

	err = filepath.WalkDir(resolvedRoot, func(fullPath string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		relPath, err := filepath.Rel(resolvedRoot, fullPath)
		if err != nil {
			return err
		}
		rel := filepath.ToSlash(relPath)
		if rel == "." {
			rel = ""
		}
		name := d.Name()
		if rel != "" && name == ".git" {
			knownFiles[rel] = true
			if d.IsDir() {
				return fs.SkipDir
			}
			return nil
		}
		if d.IsDir() {
			if rel != "" && (strings.HasPrefix(name, ".") || isSkippedTopDir(rel)) {
				return fs.SkipDir
			}
			if !inThirdParty(rel) {
				dirs = append(dirs, rel)
			}
			return nil
		}
		if name == "OWNERS" || name == "METADATA.textproto" {
			knownFiles[rel] = true
		}
		if name == "BUILD.gn" || name == "BUILD.bazel" || name == "BUILD" {
			parent := parentRelDir(rel)
			if inThirdParty(parent) && isRegularOrFileSymlink(fullPath, d) {
				tpBuildDirs[parent] = true
			}
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	for dir := range tpBuildDirs {
		dirs = append(dirs, dir)
	}

	ownersResolver.SetKnownFiles(knownFiles)
	metadataResolver.SetKnownFiles(knownFiles)

	directories := make(map[string]directoryMetadata, len(dirs))
	for _, dir := range dirs {
		entries, err := ownersResolver.Find(dir)
		if err != nil {
			return nil, err
		}
		componentID, err := metadataResolver.FindComponentID(dir)
		if err != nil {
			return nil, err
		}
		directories[dir] = directoryMetadata{
			ComponentID: componentID,
			Owners:      entries,
		}
	}
	return &snapshot{
		Version:     schemaVersion,
		Directories: directories,
	}, nil
}

func parentRelDir(rel string) string {
	idx := strings.LastIndexByte(rel, '/')
	if idx < 0 {
		return ""
	}
	return rel[:idx]
}

func isSkippedTopDir(rel string) bool {
	return rel == "integration" || rel == "out" || rel == "prebuilt"
}

func inThirdParty(rel string) bool {
	isRootThirdParty := rel == "third_party" || strings.HasPrefix(rel, "third_party/")
	isVendorThirdParty := rel == "vendor/third_party" || strings.HasPrefix(rel, "vendor/third_party/")
	if isRootThirdParty || isVendorThirdParty {
		return true
	}
	if rest, ok := strings.CutPrefix(rel, "vendor/"); ok {
		if _, after, ok := strings.Cut(rest, "/"); ok {
			return after == "third_party" || strings.HasPrefix(after, "third_party/")
		}
	}
	return false
}

func isRegularOrFileSymlink(fullPath string, d fs.DirEntry) bool {
	if d.Type().IsRegular() {
		return true
	}
	if d.Type()&fs.ModeSymlink != 0 {
		info, err := os.Stat(fullPath)
		return err == nil && info.Mode().IsRegular()
	}
	return false
}
