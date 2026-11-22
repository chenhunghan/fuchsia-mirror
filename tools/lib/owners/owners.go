// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package owners parses OWNERS files and resolves proximity-tiered directory
// ownership in a Fuchsia checkout.
package owners

import (
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"syscall"
)

// An OWNERS file can import another file using `include <path>` or `file: <path>`.
var includeRegex = regexp.MustCompile(`^(include\s+|file:\s*)(\S+)$`)

// OWNERS file lines matching this regex (excluding automated service accounts)
// are treated as owner email addresses.
var ownerRegex = regexp.MustCompile(`^[^\s=@]+@[^\s=@.]+(?:\.[^\s=@.]+)+$`)

// Entry describes one resolved owner for a directory annotated with its
// proximity rank and provenance.
type Entry struct {
	Email        string `json:"email"`
	Tier         int    `json:"tier"`
	OwnersStep   int    `json:"owners_step"`
	IncludeDepth int    `json:"include_depth"`
	DirDistance  int    `json:"dir_distance"`
	OwnersFile   string `json:"owners_file"`
}

// Tier1Emails returns the email addresses of all tier-1 (closest) owners in
// order, or nil if entries contains no tier-1 owners.
func Tier1Emails(entries []Entry) []string {
	var emails []string
	for _, entry := range entries {
		if entry.Tier == 1 {
			emails = append(emails, entry.Email)
		}
	}
	return emails
}

// Resolver resolves and caches OWNERS lookups within a Fuchsia checkout. A
// Resolver instance is not safe for concurrent use.
type Resolver struct {
	checkoutDir string
	fileCache   map[string][]byte
	ownersCache map[string]loadedOwners
	dirCache    map[string][]Entry
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
		fileCache:   make(map[string][]byte),
		ownersCache: make(map[string]loadedOwners),
		dirCache:    make(map[string][]Entry),
	}, nil
}

// SetKnownFiles restricts ancestor OWNERS and subrepository .git probes to the
// given checkout-relative paths discovered during a full tree walk. Passing nil
// restores direct filesystem probing.
func (r *Resolver) SetKnownFiles(knownFiles map[string]bool) {
	r.knownFiles = maps.Clone(knownFiles)
	clear(r.dirCache)
	clear(r.ownersCache)
}

// Find walks up from relDir toward the checkout root, stopping before root
// //OWNERS, and returns proximity-tiered owners for relDir.
func (r *Resolver) Find(relDir string) ([]Entry, error) {
	relDir, err := normalizeRelDir(relDir)
	if err != nil {
		return nil, err
	}
	if cached, ok := r.dirCache[relDir]; ok {
		return slices.Clone(cached), nil
	}

	var entries []Entry
	seenOwners := make(map[string]bool)
	ownersStep := 0
	tier := 0

	// Stop before the checkout root ("") because root //OWNERS lists global
	// repository reviewers rather than owners of individual directories.
	for loopDir, dirDistance := relDir, 0; loopDir != ""; loopDir, dirDistance = parentRelDir(loopDir), dirDistance+1 {
		ownersRel := joinRel(loopDir, "OWNERS")
		var loaded loadedOwners
		if r.knownFiles == nil || r.knownFiles[ownersRel] {
			loaded, err = r.loadOwners(ownersRel)
			if err != nil {
				return nil, err
			}
		}
		stepAdded := false
		prevDepth := -1
		for _, o := range loaded.owners {
			if seenOwners[o.email] {
				continue
			}
			seenOwners[o.email] = true
			if !stepAdded {
				ownersStep++
				stepAdded = true
			}
			if o.includeDepth != prevDepth {
				tier++
				prevDepth = o.includeDepth
			}
			entries = append(entries, Entry{
				Email:        o.email,
				Tier:         tier,
				OwnersStep:   ownersStep,
				IncludeDepth: o.includeDepth,
				DirDistance:  dirDistance,
				OwnersFile:   o.ownersFile,
			})
		}
		if loaded.noParent {
			break
		}
	}

	r.dirCache[relDir] = entries
	return slices.Clone(entries), nil
}

type rawOwner struct {
	email        string
	includeDepth int
	ownersFile   string
}

type loadedOwners struct {
	owners   []rawOwner
	noParent bool
}

func (r *Resolver) loadOwners(ownersFile string) (loadedOwners, error) {
	if cached, ok := r.ownersCache[ownersFile]; ok {
		return cached, nil
	}
	type queueItem struct {
		relFile    string
		depth      int
		viaInclude bool
	}
	const (
		visitFile = iota + 1
		visitInclude
	)
	queue := []queueItem{{relFile: ownersFile, depth: 0, viaInclude: true}}
	visitedFiles := make(map[string]int)
	seenOwners := make(map[string]bool)
	var result loadedOwners

	for len(queue) > 0 {
		curr := queue[0]
		queue = queue[1:]
		mode := visitFile
		if curr.viaInclude {
			mode = visitInclude
		}
		prev := visitedFiles[curr.relFile]
		// Revisit a file previously seen only via `file:` if later reached via
		// `include`, since `include` additionally propagates `set noparent`.
		if prev >= mode {
			continue
		}
		visitedFiles[curr.relFile] = mode

		b, err := r.readOwnersFile(curr.relFile)
		if err != nil {
			return loadedOwners{}, err
		}
		if len(b) == 0 {
			continue
		}
		for _, rawLine := range strings.Split(string(b), "\n") {
			commentStripped, _, _ := strings.Cut(rawLine, "#")
			line := strings.TrimSpace(commentStripped)
			if line == "" || strings.HasPrefix(line, "per-file ") || strings.HasPrefix(line, "per-file\t") {
				continue
			}
			if fields := strings.Fields(line); len(fields) == 2 && fields[0] == "set" && fields[1] == "noparent" {
				// `include` imports rules including `set noparent`; `file:` copies owners only.
				if curr.viaInclude {
					result.noParent = true
				}
				continue
			}
			if includeMatches := includeRegex.FindStringSubmatch(line); includeMatches != nil {
				viaInclude := curr.viaInclude && strings.HasPrefix(includeMatches[1], "include")
				includedRel, ok, err := r.resolveIncludeTarget(curr.relFile, includeMatches[2])
				if err != nil {
					return loadedOwners{}, err
				}
				if ok {
					queue = append(queue, queueItem{
						relFile:    includedRel,
						depth:      curr.depth + 1,
						viaInclude: viaInclude,
					})
				}
				continue
			}
			if prev == 0 && ownerRegex.MatchString(line) && !isServiceAccount(line) && !seenOwners[line] {
				seenOwners[line] = true
				result.owners = append(result.owners, rawOwner{
					email:        line,
					includeDepth: curr.depth,
					ownersFile:   curr.relFile,
				})
			}
		}
	}
	r.ownersCache[ownersFile] = result
	return result, nil
}

func (r *Resolver) readOwnersFile(relFile string) ([]byte, error) {
	if b, ok := r.fileCache[relFile]; ok {
		return b, nil
	}
	resolved, err := filepath.EvalSymlinks(filepath.Join(r.checkoutDir, filepath.FromSlash(relFile)))
	if err != nil {
		if isIgnorableReadErr(err) {
			r.fileCache[relFile] = nil
			return nil, nil
		}
		return nil, err
	}
	rel, err := filepath.Rel(r.checkoutDir, resolved)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return nil, fmt.Errorf("OWNERS file %q resolves outside checkout root", relFile)
	}
	b, err := os.ReadFile(resolved)
	if err != nil {
		if isIgnorableReadErr(err) {
			r.fileCache[relFile] = nil
			return nil, nil
		}
		return nil, err
	}
	r.fileCache[relFile] = b
	return b, nil
}

// resolveIncludeTarget resolves an `include` or `file:` target path referenced
// from currFile to a clean slash-separated checkout-relative path. Foreign
// Gerrit repository references return ("", false, nil).
func (r *Resolver) resolveIncludeTarget(currFile, target string) (string, bool, error) {
	base := r.repoRoot(parentRelDir(currFile))
	if !strings.HasPrefix(target, "/") && !strings.HasPrefix(target, ".") && strings.Contains(target, ":") {
		parts := strings.SplitN(target, ":", 3)
		proj, rest := parts[0], parts[len(parts)-1]
		if rest == "" {
			return "", false, nil
		}
		if !strings.HasPrefix(rest, "/") {
			rest = "/" + rest
		}
		switch {
		case proj == "":
		case proj == "fuchsia":
			base = ""
		case base != "" && (proj == base || strings.HasSuffix(proj, "/"+base)):
		default:
			return "", false, nil
		}
		target = rest
	}

	var joined string
	if strings.HasPrefix(target, "/") {
		trimmed := strings.TrimLeft(target, "/")
		if trimmed == "" {
			return "", false, nil
		}
		joined = path.Clean(joinRel(base, trimmed))
	} else {
		joined = path.Clean(joinRel(parentRelDir(currFile), target))
	}
	if joined == "." || joined == "" || joined == base {
		return "", false, nil
	}
	if joined == ".." || strings.HasPrefix(joined, "../") {
		return "", false, fmt.Errorf("OWNERS include %q in %q escapes checkout root", target, currFile)
	}
	if base != "" && !strings.HasPrefix(joined, base+"/") {
		return "", false, fmt.Errorf("OWNERS include %q in %q escapes subrepository %q", target, currFile, base)
	}
	return joined, true, nil
}

// repoRoot returns the enclosing vendor/ subrepository root for relDir, or ""
// for the main checkout (including third_party/ subrepositories, whose OWNERS
// files commonly write /src/... includes targeting the main checkout root).
func (r *Resolver) repoRoot(relDir string) string {
	for d := relDir; strings.HasPrefix(d, "vendor/"); d = parentRelDir(d) {
		gitRel := d + "/.git"
		if r.knownFiles != nil {
			if r.knownFiles[gitRel] {
				return d
			}
			continue
		}
		if _, err := os.Lstat(filepath.Join(r.checkoutDir, filepath.FromSlash(gitRel))); err == nil {
			return d
		}
	}
	return ""
}

func normalizeRelDir(relDir string) (string, error) {
	s := strings.Trim(strings.ReplaceAll(relDir, "\\", "/"), "/")
	if s == "" || s == "." {
		return "", nil
	}
	cleaned := path.Clean(s)
	if cleaned == "." {
		return "", nil
	}
	if cleaned == ".." || strings.HasPrefix(cleaned, "../") {
		return "", fmt.Errorf("directory %q escapes checkout root", relDir)
	}
	return cleaned, nil
}

func joinRel(dir, elem string) string {
	if dir == "" {
		return elem
	}
	return dir + "/" + elem
}

func parentRelDir(rel string) string {
	idx := strings.LastIndexByte(rel, '/')
	if idx < 0 {
		return ""
	}
	return rel[:idx]
}

func isServiceAccount(email string) bool {
	return strings.HasSuffix(strings.ToLower(email), ".gserviceaccount.com")
}

func isIgnorableReadErr(err error) bool {
	return errors.Is(err, fs.ErrNotExist) || errors.Is(err, syscall.EISDIR) || errors.Is(err, syscall.ENOTDIR)
}
