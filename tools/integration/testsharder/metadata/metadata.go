// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package metadata resolves test ownership and issue tracker component metadata
// for test specs processed by testsharder.
package metadata

import (
	"fmt"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/owners"
	"go.fuchsia.dev/fuchsia/tools/metadata"
)

// TestMetadata holds the tier-1 owners and issue tracker component ID attached
// to a test entry for ResultDB upload.
type TestMetadata struct {
	// A list of tier-1 owners for this directory.
	Owners []string `json:"owners,omitempty"`

	// The Google Issue Tracker component ID.
	ComponentID int `json:"component_id,omitempty"`
}

// GetMetadata resolves tier-1 owners and Google Issue Tracker component IDs for
// testSpecs from OWNERS and METADATA.textproto files under checkoutDir.
func GetMetadata(testSpecs []build.TestSpec, checkoutDir string) (map[string]TestMetadata, error) {
	resultMap := make(map[string]TestMetadata, len(testSpecs))
	if len(testSpecs) == 0 {
		return resultMap, nil
	}
	ownersResolver, err := owners.NewResolver(checkoutDir)
	if err != nil {
		return nil, err
	}
	metadataResolver, err := metadata.NewResolver(checkoutDir)
	if err != nil {
		return nil, err
	}
	for _, testSpec := range testSpecs {
		testName := testSpec.Test.Name
		sourceLabel := testSpec.Test.SourceLabel
		if strings.TrimSpace(sourceLabel) == "" {
			return nil, fmt.Errorf("test %q has no source_label", testName)
		}
		relDir, inMainRepo, err := sourceLabelDir(sourceLabel)
		if err != nil {
			return nil, fmt.Errorf("test %q: %w", testName, err)
		}
		if !inMainRepo {
			resultMap[testName] = TestMetadata{}
			continue
		}
		entries, err := ownersResolver.Find(relDir)
		if err != nil {
			return nil, fmt.Errorf("resolving owners for test %q (%s): %w", testName, sourceLabel, err)
		}
		componentID, err := metadataResolver.FindComponentID(relDir)
		if err != nil {
			return nil, fmt.Errorf("resolving metadata for test %q (%s): %w", testName, sourceLabel, err)
		}
		resultMap[testName] = TestMetadata{
			Owners:      owners.Tier1Emails(entries),
			ComponentID: int(componentID),
		}
	}
	return resultMap, nil
}

// sourceLabelDir extracts the checkout-relative directory path from a GN or
// Bazel label, returning ("", false, nil) for external Bazel repository labels.
func sourceLabelDir(sourceLabel string) (string, bool, error) {
	label, _, _ := strings.Cut(strings.TrimSpace(sourceLabel), "(")
	label = strings.TrimSpace(label)
	repoPrefix, rest, hasSlashes := strings.Cut(label, "//")
	if strings.HasPrefix(label, "@") {
		if !hasSlashes || (repoPrefix != "@" && repoPrefix != "@@") {
			if strings.TrimLeft(repoPrefix, "@") == "" {
				return "", false, fmt.Errorf("invalid source_label %q: malformed repository prefix %q", sourceLabel, repoPrefix)
			}
			return "", false, nil
		}
	} else if !hasSlashes || repoPrefix != "" {
		return "", false, fmt.Errorf("invalid source_label %q: must start with //", sourceLabel)
	}
	dirPart, targetPart, hasColon := strings.Cut(rest, ":")
	dir := strings.Trim(dirPart, "/")
	if hasColon && targetPart == "" {
		return "", false, fmt.Errorf("invalid source_label %q: empty target name after ':'", sourceLabel)
	}
	if !hasColon && dir == "" {
		return "", false, fmt.Errorf("invalid source_label %q: missing package path and target name", sourceLabel)
	}
	return dir, true, nil
}
