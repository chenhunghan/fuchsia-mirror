// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme_fuchsia

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const docURL = "https://fuchsia.dev/fuchsia-src/development/source_code/third-party-metadata"

// Validate checks if the README.fuchsia file structures contain all required fields
// and no unknown fields. It also verifies that referenced paths exist on disk.
func Validate(projectRoot string, readmes []*Readme) []error {
	var errs []error
	var baseDir = projectRoot

	for i, r := range readmes {
		filePath := r.FilePath
		defaultSpan := r.BlockSpan
		if defaultSpan.StartLine == 0 {
			defaultSpan = LineSpan{StartLine: 1, EndLine: 1}
		}

		getSpan := func(key string) LineSpan {
			if r.Spans != nil {
				if s, ok := r.Spans[key]; ok && s.StartLine > 0 {
					return s
				}
			}
			return defaultSpan
		}

		addFinding := func(msg string, span LineSpan, replacements ...string) {
			if span.StartLine == 0 {
				span = defaultSpan
			}
			errs = append(errs, Finding{
				FilePath:     filePath,
				Line:         span.StartLine,
				EndLine:      span.EndLine,
				Level:        "error",
				Message:      msg,
				Replacements: replacements,
			})
		}

		blockBaseDir := ResolveProjectRoot(baseDir, filePath)
		var currentDir string
		if i == 0 {
			currentDir = blockBaseDir
		} else {
			if r.Location != "" {
				currentDir = filepath.Join(blockBaseDir, r.Location)
				if _, err := os.Stat(currentDir); os.IsNotExist(err) {
					addFinding(fmt.Sprintf("[%d]: 'Location' directory does not exist: %s (%s)", i+1, r.Location, docURL), getSpan("Location"))
				}
			} else {
				currentDir = blockBaseDir // Fallback
			}
		}

		// Check 1: Unknown fields
		for _, uf := range r.UnknownFields {
			addFinding(fmt.Sprintf("[%d]: Found unknown/invalid fields: [{Key:%s Value:%s}] (%s#syntax)", i+1, uf.Key, uf.Value, docURL), uf.Span)
		}

		// Check 2: Required Fields
		if r.Name == "" {
			addFinding(fmt.Sprintf("[%d]: Missing required field 'Name' (%s#name)", i+1, docURL), defaultSpan)
		}

		if r.FirstParty != "" && r.FirstParty != "yes" && r.FirstParty != "no" {
			var repl []string
			if strings.EqualFold(r.FirstParty, "true") {
				repl = []string{"First Party: yes"}
			} else if strings.EqualFold(r.FirstParty, "false") {
				repl = []string{"First Party: no"}
			}
			addFinding(fmt.Sprintf("[%d]: Field 'First Party' has an unknown value. Required 'yes' or 'no', got %q (%s)", i+1, r.FirstParty, docURL), getSpan("First Party"), repl...)
		}

		projPath := formatProjectPath(currentDir, filePath)

		if r.FirstParty != "yes" {
			hasUrlAndRev := r.URL != "" && r.Revision != ""
			hasCpeAndVer := r.CPEPrefix != "" && r.Version != ""
			if !hasUrlAndRev && !hasCpeAndVer {
				span := defaultSpan
				if r.URL != "" {
					span = getSpan("URL")
				}
				addFinding(fmt.Sprintf("[%d]: Missing required fields. Must specify either ('URL' AND 'Revision') OR ('CPEPrefix' AND 'Version') (%s#url)", i+1, docURL), span)
			}

			if r.SecurityCritical == "" {
				addFinding(fmt.Sprintf("[%d]: Missing required field 'Security Critical' (%s#security-critical)", i+1, docURL), defaultSpan)
			} else if r.SecurityCritical != "yes" && r.SecurityCritical != "no" {
				var repl []string
				if strings.EqualFold(r.SecurityCritical, "true") {
					repl = []string{"Security Critical: yes"}
				} else if strings.EqualFold(r.SecurityCritical, "false") {
					repl = []string{"Security Critical: no"}
				}
				addFinding(fmt.Sprintf("[%d]: Field 'Security Critical' has an unknown value. Required 'yes' or 'no', got %q (%s#security-critical)", i+1, r.SecurityCritical, docURL), getSpan("Security Critical"), repl...)
			}
			if !hasMissingLicensePolicyException(projPath, currentDir, filePath) {
				if len(r.Licenses) == 0 {
					addFinding(fmt.Sprintf(
						"[%d]: Missing required field 'License' (%s#license)\n"+
							"  Remediation: Add 'License: <SPDX-ID>', or if this project legitimately has no license file, omit 'License' and 'License File' and run:\n"+
							"    fx check-licenses policy add -bug BUG_ID AllProjectsMustHaveALicense %s",
						i+1, docURL, projPath), defaultSpan)
				}
				if len(r.LicenseFiles) == 0 {
					addFinding(fmt.Sprintf(
						"[%d]: Missing required field 'License File'. At least one must be specified. (%s#license-file)\n"+
							"  Remediation: Add 'License File: <path>', or if this project legitimately has no license file, omit 'License' and 'License File' and run:\n"+
							"    fx check-licenses policy add -bug BUG_ID AllProjectsMustHaveALicense %s",
						i+1, docURL, projPath), defaultSpan)
				}
			}
		}

		if i > 0 && r.Location == "" {
			addFinding(fmt.Sprintf("[%d]: Missing required field 'Location' for sub-project defined after a DEPENDENCY DIVIDER (%s)", i+1, docURL), defaultSpan)
		}

		for _, lf := range r.LicenseFiles {
			filePathOnDisk := filepath.Join(currentDir, lf)
			if _, err := os.Stat(filePathOnDisk); os.IsNotExist(err) {
				addFinding(fmt.Sprintf(
					"[%d]: License File does not exist: %s (%s#license-file)\n"+
						"  Remediation: Verify the path relative to %s, or if this project legitimately has no license file, remove 'License' and 'License File' from README.fuchsia and run:\n"+
						"    fx check-licenses policy add -bug BUG_ID AllProjectsMustHaveALicense %s",
					i+1, filePathOnDisk, docURL, projPath, projPath), getSpan(lf))
			}
		}

		for _, nlf := range r.NonLicenseFiles {
			filePathOnDisk := filepath.Join(currentDir, nlf)
			if _, err := os.Stat(filePathOnDisk); os.IsNotExist(err) {
				addFinding(fmt.Sprintf("[%d]: Non-License File does not exist: %s (%s)", i+1, filePathOnDisk, docURL), getSpan(nlf))
			}
		}

		for _, gnf := range r.GeneratedNoticeFiles {
			noticeDir := currentDir
			if r.FilePath != "" {
				noticeDir = filepath.Dir(r.FilePath)
				if loc := filepath.Clean(r.Location); loc != "" && loc != "." {
					subDir := filepath.Join(noticeDir, loc)
					if stat, err := os.Stat(subDir); err == nil && stat.IsDir() {
						noticeDir = subDir
					}
				}
			}
			filePathOnDisk := filepath.Join(noticeDir, gnf)
			if _, err := os.Stat(filePathOnDisk); os.IsNotExist(err) {
				addFinding(fmt.Sprintf("[%d]: Generated Notice File does not exist: %s (%s#license-file)", i+1, filePathOnDisk, docURL), getSpan(gnf))
			}
		}
	}

	return errs
}

// ResolveProjectRoot returns the physical directory for the project governed by readmePath,
// translating virtual README paths under tools/check-licenses/assets/readmes/ to their logical
// repository directory when projectRoot is empty.
func ResolveProjectRoot(projectRoot, readmePath string) string {
	if projectRoot != "" {
		return projectRoot
	}
	if readmePath == "" {
		return ""
	}
	root := filepath.Dir(readmePath)
	dir := filepath.ToSlash(root)
	const virtualMarker = "tools/check-licenses/assets/readmes/"
	if idx := strings.Index(dir, virtualMarker); idx != -1 {
		root = filepath.FromSlash(dir[idx+len(virtualMarker):])
		if fuchsiaDir := FindFuchsiaDir(readmePath); fuchsiaDir != "" {
			root = filepath.Join(fuchsiaDir, root)
		}
	}
	return root
}

// FindFuchsiaDir locates the Fuchsia workspace root from FUCHSIA_DIR or by walking up from candidate paths.
func FindFuchsiaDir(paths ...string) string {
	if env := os.Getenv("FUCHSIA_DIR"); env != "" {
		if abs, err := filepath.Abs(env); err == nil {
			return abs
		}
	}
	candidates := append([]string{}, paths...)
	if cwd, err := os.Getwd(); err == nil {
		candidates = append(candidates, cwd)
	}
	for _, p := range candidates {
		if p == "" {
			continue
		}
		abs, err := filepath.Abs(p)
		if err != nil {
			continue
		}
		for dir := abs; dir != "/" && dir != "."; dir = filepath.Dir(dir) {
			if _, err := os.Stat(filepath.Join(dir, "tools/check-licenses/config.json")); err == nil {
				return dir
			}
		}
	}
	return ""
}

func formatProjectPath(currentDir, readmeFilePath string) string {
	candidate := currentDir
	if candidate == "" || candidate == "." {
		if readmeFilePath != "" {
			candidate = filepath.Dir(readmeFilePath)
		}
	}
	slash := filepath.ToSlash(filepath.Clean(candidate))
	const virtualMarker = "tools/check-licenses/assets/readmes/"
	if idx := strings.Index(slash, virtualMarker); idx != -1 {
		slash = slash[idx+len(virtualMarker):]
	}
	if filepath.IsAbs(slash) {
		if fuchsiaDir := FindFuchsiaDir(slash, readmeFilePath); fuchsiaDir != "" {
			if rel, err := filepath.Rel(fuchsiaDir, slash); err == nil && !strings.HasPrefix(rel, "..") {
				slash = filepath.ToSlash(rel)
			}
		}
	}
	slash = strings.TrimPrefix(slash, "./")
	if slash == "" || slash == "." {
		return "<project_path>"
	}
	return slash
}

func hasMissingLicensePolicyException(relProj, currentDir, readmePath string) bool {
	if relProj == "" || relProj == "<project_path>" {
		return false
	}
	fuchsiaDir := FindFuchsiaDir(readmePath, currentDir)
	if fuchsiaDir == "" {
		return false
	}

	var exceptionDirs []string
	pubDir := filepath.Join(fuchsiaDir, "tools/check-licenses/assets/configs/policy_exceptions/AllProjectsMustHaveALicense")
	if info, err := os.Stat(pubDir); err == nil && info.IsDir() {
		exceptionDirs = append(exceptionDirs, pubDir)
	}
	if matches, err := filepath.Glob(filepath.Join(fuchsiaDir, "vendor/*/tools/check-licenses/assets/configs/policy_exceptions/AllProjectsMustHaveALicense")); err == nil {
		exceptionDirs = append(exceptionDirs, matches...)
	}

	type policyConfig struct {
		PolicyExceptions map[string][]struct {
			Paths []string `json:"paths"`
		} `json:"policy_exceptions"`
	}

	for _, dir := range exceptionDirs {
		entries, err := os.ReadDir(dir)
		if err != nil {
			continue
		}
		for _, entry := range entries {
			if entry.IsDir() || filepath.Ext(entry.Name()) != ".json" {
				continue
			}
			data, err := os.ReadFile(filepath.Join(dir, entry.Name()))
			if err != nil {
				continue
			}
			var cfg policyConfig
			if err := json.Unmarshal(data, &cfg); err != nil {
				continue
			}
			for _, item := range cfg.PolicyExceptions["AllProjectsMustHaveALicense"] {
				for _, p := range item.Paths {
					cleanP := filepath.ToSlash(filepath.Clean(strings.TrimPrefix(p, "//")))
					if cleanP == relProj {
						return true
					}
				}
			}
		}
	}
	return false
}
