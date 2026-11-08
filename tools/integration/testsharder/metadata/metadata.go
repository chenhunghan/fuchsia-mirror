// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package metadata

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"google.golang.org/protobuf/encoding/prototext"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/metadata/proto"
)

// An OWNERS file can import another OWNERS file using a line of the form
// `include /path/to/other/OWNERS`.
var includeRegex = regexp.MustCompile(`^include (\S+)$`)

// OWNERS file lines that match this regex (a very crude email matcher) will be
// considered to represent owners.
var ownerRegex = regexp.MustCompile(`^\S+@\S+$`)

type TestMetadata struct {
	// A list of owners for this directory.
	Owners []string `json:"owners,omitempty"`

	// The Google Issue Tracker component ID.
	ComponentID int `json:"component_id,omitempty"`
}

func GetMetadata(testSpecs []build.TestSpec, checkoutDir string) (map[string]TestMetadata, error) {
	resultMap := make(map[string]TestMetadata)
	for _, testSpec := range testSpecs {
		testName := testSpec.Test.Name
		sourceLabel := testSpec.Test.SourceLabel
		if sourceLabel == "" {
			return nil, fmt.Errorf("test %q has no source_label", testName)
		}
		if checkoutDir == "" {
			return nil, fmt.Errorf("checkout directory path cannot be empty")
		}
		owners, componentId, err := findOwnersAndComponentID(checkoutDir, sourceLabel)
		if err != nil {
			return nil, err
		}
		resultMap[testName] = TestMetadata{owners, componentId}
	}
	return resultMap, nil
}

func findOwnersAndComponentID(checkoutDir string, sourceLabel string) ([]string, int, error) {
	// Strip off the toolchain, target name, and leading dashes
	relativeTestDir := strings.Trim(strings.Split(strings.Split(sourceLabel, "(")[0], ":")[0], "/")

	var owners []string
	var componentId int
	var err error
	pathToRelativeDestDir := filepath.Join(strings.Split(relativeTestDir, "/")...)
	currentDir := filepath.Join(checkoutDir, pathToRelativeDestDir)

	// Give up if we reach the checkout root before finding an OWNERS file. We
	// want to avoid falling back to global owners since global owners are
	// for large-scale change reviews and are not responsible for arbitrary tests.
	for loopDir := currentDir; loopDir != checkoutDir && (len(owners) == 0 || componentId == 0); loopDir = filepath.Dir(loopDir) {
		metadataFilePath := filepath.Join(loopDir, "METADATA.textproto")
		if componentId == 0 {
			componentId, err = loadMetadata(metadataFilePath)
			if err != nil {
				return nil, 0, err
			}
		}
		ownersFilePath := filepath.Join(loopDir, "OWNERS")
		if len(owners) == 0 {
			owners, err = loadOwners(ownersFilePath, checkoutDir)
			if err != nil {
				return nil, 0, err
			}
		}
	}
	return owners, componentId, nil
}

func loadMetadata(path string) (int, error) {
	bytes, err := os.ReadFile(path)
	if err != nil {
		// Ignore missing METADATA.textproto files
		if errors.Is(err, fs.ErrNotExist) {
			return 0, nil
		}
		return 0, err
	}
	var metadata proto.Metadata
	// Ignore invalid METADATA.textproto files
	if err := prototext.Unmarshal(bytes, &metadata); err != nil {
		return 0, nil
	}
	// only the first component ID is used if there are multiple
	// Buganizer does not allow a bug to have multiple components
	for _, tracker := range metadata.GetTrackers() {
		for _, issueTracker := range tracker.GetIssueTracker() {
			return int(issueTracker.GetComponentId()), nil
		}
	}
	return 0, nil
}

func loadOwners(ownersFile string, checkoutDir string) ([]string, error) {
	queue := []string{ownersFile}
	visitedFiles := make(map[string]bool)
	seenOwners := make(map[string]bool)
	owners := make([]string, 0)

	for len(queue) > 0 {
		currFile := queue[0]
		queue = queue[1:]
		cleanPath := filepath.Clean(currFile)
		if visitedFiles[cleanPath] {
			continue
		}
		visitedFiles[cleanPath] = true

		b, err := os.ReadFile(currFile)
		if err != nil {
			// Ignore missing/invalid OWNERS files
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			return owners, err
		}
		for _, line := range strings.Split(string(b), "\n") {
			includeMatches := includeRegex.FindStringSubmatch(line)
			ownerMatch := ownerRegex.FindString(line)
			if includeMatches != nil {
				includedFileParts := strings.Split(strings.TrimLeft(includeMatches[1], "/"), "/")
				var path string
				var err error
				// Included paths can be relative
				if strings.HasPrefix(includedFileParts[0], ".") {
					path, err = filepath.Abs(filepath.Join(filepath.Dir(currFile), filepath.Join(includedFileParts...)))
					if err != nil {
						return nil, fmt.Errorf("Error reading included lines in OWNERS file: %w", err)
					}
				} else {
					path = filepath.Join(checkoutDir, filepath.Join(includedFileParts...))
				}
				queue = append(queue, path)
			}
			if ownerMatch != "" && !seenOwners[line] {
				seenOwners[line] = true
				owners = append(owners, line)
			}
		}
	}
	return owners, nil
}
