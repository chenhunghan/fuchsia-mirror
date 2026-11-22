// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package metadata resolves METADATA.textproto files in a Fuchsia checkout.
package metadata

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"strings"
	"syscall"

	"google.golang.org/protobuf/encoding/prototext"

	metadatapb "go.fuchsia.dev/fuchsia/tools/metadata/proto"
)

// FindComponentID resolves the inherited Google Issue Tracker component ID for
// relDir under checkoutDir, or 0 if none is configured. Callers resolving many
// directories should reuse a Resolver via NewResolver.
func FindComponentID(checkoutDir, relDir string) (int64, error) {
	r, err := NewResolver(checkoutDir)
	if err != nil {
		return 0, err
	}
	return r.FindComponentID(relDir)
}

// Resolver resolves and caches METADATA.textproto lookups within a Fuchsia
// checkout. A Resolver instance is not safe for concurrent use.
type Resolver struct {
	checkoutDir string
	fileCache   map[string]int64
	dirCache    map[string]int64
	knownFiles  map[string]bool
}

// NewResolver returns a Resolver rooted at checkoutDir.
func NewResolver(checkoutDir string) (*Resolver, error) {
	if checkoutDir == "" {
		return nil, errors.New("checkout directory path cannot be empty")
	}
	abs, err := filepath.Abs(checkoutDir)
	if err != nil {
		return nil, fmt.Errorf("resolving checkout path: %w", err)
	}
	resolved, err := filepath.EvalSymlinks(abs)
	if err != nil {
		return nil, fmt.Errorf("resolving checkout directory: %w", err)
	}
	info, err := os.Stat(resolved)
	if err != nil {
		return nil, fmt.Errorf("stat checkout directory: %w", err)
	}
	if !info.IsDir() {
		return nil, fmt.Errorf("checkout path %q is not a directory", checkoutDir)
	}
	return &Resolver{
		checkoutDir: resolved,
		fileCache:   make(map[string]int64),
		dirCache:    make(map[string]int64),
	}, nil
}

// SetKnownFiles restricts METADATA.textproto and vendor/ subrepository .git
// probes to the given checkout-relative paths discovered during a full tree
// walk (such as by directory_owners), avoiding negative filesystem probes
// across directories without a metadata file. Passing nil restores direct
// filesystem probing.
func (r *Resolver) SetKnownFiles(knownFiles map[string]bool) {
	if knownFiles == nil {
		r.knownFiles = nil
	} else {
		r.knownFiles = make(map[string]bool, len(knownFiles))
		for k, v := range knownFiles {
			if !v {
				continue
			}
			if cleaned, err := normalizeRelPath(k); err == nil && cleaned != "" {
				r.knownFiles[cleaned] = true
			}
		}
	}
	clear(r.dirCache)
}

// FindComponentID walks up from relDir to the enclosing repository root
// (stopping at a vendor/ Git subrepository root if inside one, or at the
// checkout root including //METADATA.textproto) and returns the nearest
// positive Google Issue Tracker component ID, or 0 if none is configured.
func (r *Resolver) FindComponentID(relDir string) (int64, error) {
	relDir, err := normalizeRelPath(relDir)
	if err != nil {
		return 0, err
	}

	var visited []string
	var resolved int64
	for loopDir := relDir; ; loopDir = parentRelDir(loopDir) {
		if cid, ok := r.dirCache[loopDir]; ok {
			resolved = cid
			break
		}
		visited = append(visited, loopDir)
		metaRel := path.Join(loopDir, "METADATA.textproto")
		// Skip probing the filesystem when knownFiles excludes this file.
		if r.knownFiles == nil || r.knownFiles[metaRel] {
			cid, err := r.loadComponentID(metaRel)
			if err != nil {
				return 0, err
			}
			if cid > 0 {
				resolved = cid
				break
			}
		}
		if loopDir == "" || r.isVendorSubrepoRoot(loopDir) {
			break
		}
	}
	for _, d := range visited {
		r.dirCache[d] = resolved
	}
	return resolved, nil
}

func (r *Resolver) loadComponentID(relPath string) (int64, error) {
	if cid, ok := r.fileCache[relPath]; ok {
		return cid, nil
	}
	resolved, err := filepath.EvalSymlinks(filepath.Join(r.checkoutDir, filepath.FromSlash(relPath)))
	if err != nil {
		if isIgnorableReadErr(err) {
			r.fileCache[relPath] = 0
			return 0, nil
		}
		return 0, err
	}
	rel, err := filepath.Rel(r.checkoutDir, resolved)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return 0, fmt.Errorf("METADATA.textproto file %q resolves outside checkout root", relPath)
	}
	b, err := os.ReadFile(resolved)
	if err != nil {
		if isIgnorableReadErr(err) {
			r.fileCache[relPath] = 0
			return 0, nil
		}
		return 0, err
	}
	var metadata metadatapb.Metadata
	// Ignore invalid METADATA.textproto files and unknown newer fields.
	if err := (prototext.UnmarshalOptions{DiscardUnknown: true}).Unmarshal(b, &metadata); err != nil {
		r.fileCache[relPath] = 0
		return 0, nil
	}
	// Only the first positive component ID is used if there are multiple; Google
	// Issue Tracker does not allow an issue to have multiple components.
	for _, tracker := range metadata.GetTrackers() {
		for _, issueTracker := range tracker.GetIssueTracker() {
			if cid := issueTracker.GetComponentId(); cid > 0 {
				r.fileCache[relPath] = cid
				return cid, nil
			}
		}
	}
	r.fileCache[relPath] = 0
	return 0, nil
}

// isVendorSubrepoRoot reports whether relDir is the root of a vendor/ Git
// subrepository so directory walks do not cross into the public checkout root.
func (r *Resolver) isVendorSubrepoRoot(relDir string) bool {
	if !strings.HasPrefix(relDir, "vendor/") {
		return false
	}
	gitRel := path.Join(relDir, ".git")
	if r.knownFiles != nil {
		return r.knownFiles[gitRel]
	}
	_, err := os.Lstat(filepath.Join(r.checkoutDir, filepath.FromSlash(gitRel)))
	return err == nil
}

func normalizeRelPath(relPath string) (string, error) {
	s := strings.Trim(filepath.ToSlash(relPath), "/")
	if s == "" || s == "." {
		return "", nil
	}
	cleaned := path.Clean(s)
	if cleaned == "." {
		return "", nil
	}
	if !filepath.IsLocal(filepath.FromSlash(cleaned)) {
		return "", fmt.Errorf("path %q escapes checkout root", relPath)
	}
	return cleaned, nil
}

func parentRelDir(rel string) string {
	idx := strings.LastIndexByte(rel, '/')
	if idx < 0 {
		return ""
	}
	return rel[:idx]
}

func isIgnorableReadErr(err error) bool {
	return errors.Is(err, fs.ErrNotExist) || errors.Is(err, syscall.EISDIR) || errors.Is(err, syscall.ENOTDIR)
}
