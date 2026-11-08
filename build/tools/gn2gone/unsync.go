// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"regexp"
	"slices"
	"sort"
	"strings"

	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
	"go.starlark.net/syntax"
)

// BazelEdit describes mechanical GN-conversion leftovers to strip from an
// unsynced directory's BUILD.bazel.
type BazelEdit struct {
	// DropLines are 1-based line numbers to remove completely.
	DropLines map[int]bool
	// TrimComment maps a 1-based line number to a trailing `# @bazel2gn:...`
	// comment to strip from the end of that line.
	TrimComment map[int]string
}

// Plan is the set of file edits that unsyncs some READY directories.
type Plan struct {
	// Deletes are BUILD.gn files to delete, relative to the Fuchsia root.
	Deletes []string
	// Truncates maps a BUILD.gn file to the sentinel line: that line and
	// everything after it are removed.
	Truncates map[string]int
	// RemoveEntries maps a file to the list entries (string literals) to
	// remove from it.
	RemoveEntries map[string][]Reference
	// BazelEdits maps a BUILD.bazel file in an unsynced directory to the
	// `@bazel2gn:` annotations and GN-only macro attributes to strip from it.
	BazelEdits map[string]*BazelEdit
	// Dirs are the directories the plan unsyncs.
	Dirs []string
	// Skipped maps directories that can't be unsynced to the reason.
	Skipped map[string]string
	// FollowUps are leftovers the plan doesn't edit, for a human to review:
	// test groups whose every entry is removed (noting dynamic `:<name>`
	// callers), targets above a stripped sentinel that only the stripped
	// section used, `//dir:*` patterns naming a deleted BUILD.gn (with
	// generated-file notices), and BUILD.bazel comments mentioning GN/bazel2gn
	// or sitting above a stripped annotation/attribute. Line numbers are from
	// before Apply. Sorted.
	FollowUps []string
}

// PlanUnsync plans the edits for `reports`. Directories that are not READY
// are skipped.
func (idx *Index) PlanUnsync(reports []*DirReport) *Plan {
	p := &Plan{
		Truncates:     map[string]int{},
		RemoveEntries: map[string][]Reference{},
		BazelEdits:    map[string]*BazelEdit{},
		Skipped:       map[string]string{},
	}
	deleted := map[string]bool{}
	for _, rep := range reports {
		if rep.State == StateReady && rep.Mode == ModeDeleteBuildGN {
			deleted[path.Join(rep.Dir, "BUILD.gn")] = true
		}
	}
	var bazelFollowUps []string
	for _, rep := range reports {
		if rep.State != StateReady {
			p.Skipped[rep.Dir] = string(rep.State)
			continue
		}
		gnFile := path.Join(rep.Dir, "BUILD.gn")
		switch rep.Mode {
		case ModeDeleteBuildGN:
			p.Deletes = append(p.Deletes, gnFile)
		case ModeStripBelowSentinel:
			p.Truncates[gnFile] = idx.Sentinel[rep.Dir].SentinelLine
		}
		for _, c := range rep.Cleanup {
			if deleted[c.File] {
				continue
			}
			if sl, ok := p.Truncates[c.File]; ok && c.Line >= sl {
				continue
			}
			p.RemoveEntries[c.File] = append(p.RemoveEntries[c.File], c)
		}
		if ed, fus := idx.planBazelCleanup(rep.Dir); ed != nil || len(fus) > 0 {
			if ed != nil {
				p.BazelEdits[path.Join(rep.Dir, "BUILD.bazel")] = ed
			}
			bazelFollowUps = append(bazelFollowUps, fus...)
		}
		p.Dirs = append(p.Dirs, rep.Dir)
	}
	sort.Strings(p.Deletes)
	sort.Strings(p.Dirs)
	p.FollowUps = idx.followUps(p, deleted, bazelFollowUps)
	return p
}

// UnsyncWaves repeatedly scans, plans, and applies unsync passes starting from
// `idx` until no READY directories remain. The edits are already applied on
// disk when UnsyncWaves returns; the returned Plan aggregates Dirs, Files(),
// and surviving FollowUps across all passes, alongside the pass count.
func UnsyncWaves(ctx context.Context, idx *Index, opts LoadOptions) (*Plan, int, error) {
	combined := &Plan{
		Truncates:     map[string]int{},
		RemoveEntries: map[string][]Reference{},
		BazelEdits:    map[string]*BazelEdit{},
		Skipped:       map[string]string{},
	}
	allDeleted := map[string]bool{}
	var rawFollowUps []string
	waves := 0
	cur := idx
	for {
		var ready []*DirReport
		for _, r := range cur.ScanAll() {
			if r.State == StateReady {
				ready = append(ready, r)
			}
		}
		if len(ready) == 0 {
			break
		}
		p := cur.PlanUnsync(ready)
		if len(p.Dirs) == 0 {
			break
		}
		if err := p.Apply(ctx, opts.Root, opts.GNBin); err != nil {
			return nil, waves, err
		}
		waves++
		combined.Dirs = append(combined.Dirs, p.Dirs...)
		for _, f := range p.Deletes {
			allDeleted[f] = true
			combined.Deletes = append(combined.Deletes, f)
			delete(combined.Truncates, f)
			delete(combined.RemoveEntries, f)
		}
		for f, sl := range p.Truncates {
			if !allDeleted[f] {
				combined.Truncates[f] = sl
			}
		}
		for f, es := range p.RemoveEntries {
			if !allDeleted[f] {
				combined.RemoveEntries[f] = append(combined.RemoveEntries[f], es...)
			}
		}
		for f, be := range p.BazelEdits {
			combined.BazelEdits[f] = be
		}
		rawFollowUps = append(rawFollowUps, p.FollowUps...)

		nextIdx, stats, err := Load(ctx, opts)
		if err != nil {
			return nil, waves, err
		}
		if len(stats.ParseErrors) > 0 {
			return nil, waves, fmt.Errorf("after wave %d, %d GN files failed to parse: %v", waves, len(stats.ParseErrors), stats.ParseErrors[0])
		}
		cur = nextIdx
	}
	for _, fu := range rawFollowUps {
		file, _, _ := strings.Cut(fu, ":")
		if !allDeleted[file] {
			combined.FollowUps = append(combined.FollowUps, fu)
		}
	}
	sort.Strings(combined.Dirs)
	sort.Strings(combined.Deletes)
	combined.Deletes = slices.Compact(combined.Deletes)
	sort.Strings(combined.FollowUps)
	combined.FollowUps = slices.Compact(combined.FollowUps)
	return combined, waves, nil
}

// followUps finds what `p` leaves behind (see Plan.FollowUps).
func (idx *Index) followUps(p *Plan, deleted map[string]bool, bazelFollowUps []string) []string {
	out := append([]string(nil), bazelFollowUps...)
	byPath := map[string]*FileIndex{}
	for _, fi := range idx.Files {
		byPath[fi.Path] = fi
	}
	genCache := map[string]string{}
	withGen := func(file, msg string) string {
		hdr, ok := genCache[file]
		if !ok {
			hdr = generatedHeader(idx.Root, file)
			genCache[file] = hdr
		}
		if hdr != "" {
			return fmt.Sprintf("%s (generated file: %s)", msg, hdr)
		}
		return msg
	}

	// Test groups emptied by the removed entries.
	for f, entries := range p.RemoveEntries {
		fi := byPath[f]
		if fi == nil {
			continue
		}
		removed := map[[2]int]bool{}
		for _, e := range entries {
			removed[[2]int{e.Line, e.Column}] = true
		}
		for _, t := range fi.Targets {
			if !IsTestGroup(t) || t.OpaqueDeps || t.InCondition {
				continue
			}
			if sl, ok := p.Truncates[f]; ok && t.Line >= sl {
				continue
			}
			total, gone := 0, 0
			for _, l := range fi.Literals {
				if l.Target != t || (l.Attr != "deps" && l.Attr != "public_deps") {
					continue
				}
				total++
				if removed[[2]int{l.Line, l.Column}] {
					gone++
				}
			}
			if gone > 0 && gone == total {
				var msg string
				if callers := idx.bareDirCallers(fi.Dir(), f, p, deleted); len(callers) > 0 {
					msg = fmt.Sprintf("%s:%d %s is now empty; check dynamic %q callers (%s) before removing it", f, t.Line, enclosing(t), ":"+t.Name, strings.Join(callers, ", "))
				} else {
					msg = fmt.Sprintf("%s:%d %s is now empty; remove it and the entries (including any dynamic %q references) that point at it", f, t.Line, enclosing(t), ":"+t.Name)
				}
				out = append(out, withGen(f, msg))
			}
		}
	}
	// Private targets above a stripped sentinel that only the stripped
	// section used.
	for f := range p.Truncates {
		fi := byPath[f]
		if fi == nil {
			continue
		}
		dir := fi.Dir()
		for _, t := range fi.Targets {
			if t.Region != RegionAbove || t.Name == "" || !privateTarget(fi, t) {
				continue
			}
			if !idx.usedOutsideSynced(dir, fi, t) {
				out = append(out, withGen(f, fmt.Sprintf("%s:%d %s was only used by the stripped targets; remove it if nothing else needs it", f, t.Line, enclosing(t))))
			}
		}
	}
	// Visibility-style patterns naming a deleted BUILD.gn.
	patterns := map[string]bool{}
	for f := range deleted {
		patterns["//"+path.Dir(f)+":*"] = true
	}
	for _, fi := range idx.Files {
		if deleted[fi.Path] {
			continue
		}
		for _, l := range fi.Literals {
			if l.Region != RegionBelow && patterns[l.Value] {
				out = append(out, withGen(l.File, fmt.Sprintf("%s:%d %q names a deleted BUILD.gn", l.File, l.Line, l.Value)))
			}
		}
	}
	sort.Strings(out)
	return slices.Compact(out)
}

func isNonLabelAttr(attr string) bool {
	switch attr {
	case "visibility", "assert_no_deps", "sources", "inputs", "outputs", "public", "script", "depfile", "include_dirs", "configs", "public_configs", "all_dependent_configs", "output_name", "crate_name", "package_name", "component_name":
		return true
	}
	return false
}

// bareDirCallers returns `file:line` sites outside `groupFile` that list `dir`
// as a bare directory outside `deps`/`public_deps`/`data_deps` (e.g. a `_tools`
// list iterated into `"$tool:tests"`).
func (idx *Index) bareDirCallers(dir, groupFile string, p *Plan, deleted map[string]bool) []string {
	if dir == "." {
		return nil
	}
	var out []string
	for _, fi := range idx.Files {
		if deleted[fi.Path] || fi.Path == groupFile {
			continue
		}
		sl, truncated := p.Truncates[fi.Path]
		for _, l := range fi.Literals {
			if l.Region == RegionBelow || (truncated && l.Line >= sl) {
				continue
			}
			if IsBazelLabel(l) || (l.CallArgOf != "" && l.CallArgOf != "foreach") || isNonLabelAttr(l.Attr) {
				continue
			}
			raw := stripToolchain(l.Value)
			if strings.Contains(raw, ":") || path.Base(raw) == "." || path.Base(raw) == ".." || bareDirName(l, dir) != "" {
				continue
			}
			if d, _, ok := idx.plainLabel(l, path.Dir(l.File)); ok && d == dir {
				out = append(out, fmt.Sprintf("%s:%d", l.File, l.Line))
			}
		}
	}
	sort.Strings(out)
	return slices.Compact(out)
}

// generatedHeader returns the generated-file notice in `relPath`'s leading
// `#` comments (before any code or bazel2gn sentinel), or "" if hand-written.
func generatedHeader(root, relPath string) string {
	if root == "" {
		return ""
	}
	f, err := os.Open(filepath.Join(root, relPath))
	if err != nil {
		return ""
	}
	defer f.Close()
	s := bufio.NewScanner(f)
	for s.Scan() {
		line := strings.TrimSpace(s.Text())
		if line == "" {
			continue
		}
		if line == bazel2gn.SentinelComment || !strings.HasPrefix(line, "#") {
			break
		}
		comment := strings.TrimSpace(strings.TrimLeft(line, "#"))
		lower := strings.ToLower(comment)
		if strings.Contains(lower, "do not edit") || strings.Contains(lower, "@generated") {
			return comment
		}
	}
	return ""
}

var gnMentionRE = regexp.MustCompile(`(?:\bGN\b|BUILD\.gn|\bbazel2gn\b)`)

// isGNOnlyBazelAttr reports whether `attr` on top-level Bazel rule/macro
// `kind` exists solely for bazel2gn conversion (`doc = "Unused in Bazel, for
// GN conversion only."` or `Ignored in Bazel` in //build/bazel/rules).
func isGNOnlyBazelAttr(kind, attr string) bool {
	switch kind {
	case "fx_cc_binary", "fx_cc_test_binary", "cc_binary_host_tool", "idk_cc_binary_host_tool", "host_cc_test":
		return attr == "gn_output_name" || attr == "public_configs" || attr == "disable_syslog_backend"
	case "fx_cc_library", "cc_shared_library_zx", "cc_source_library_zx", "cc_static_library_zx":
		return attr == "configs" || attr == "public_configs"
	case "fx_cc_shared_library":
		return attr == "public_configs"
	case "fidl_library":
		return attr == "additional_cpp_configs" || attr == "non_fidl_deps"
	case "rustc_library", "rustc_dylib":
		return attr == "output_name"
	case "rustc_staticlib":
		return attr == "output_name" || attr == "gn_quiet_clippy" || attr == "gn_disable_rustdoc"
	}
	if strings.HasPrefix(kind, "idk_cc_shared_library") || strings.HasPrefix(kind, "idk_cc_static_library") {
		return attr == "friend" || attr == "public_configs" || attr == "configs" || attr == "ldflags" || attr == "inputs"
	}
	if strings.HasPrefix(kind, "idk_cc_source_library") {
		return attr == "build_as_static" || attr == "friend" || attr == "public_configs" || attr == "configs"
	}
	return false
}

func bazelTargetLabel(kind, name string) string {
	if name == "" {
		return kind + "(...)"
	}
	return fmt.Sprintf("%s(%q)", kind, name)
}

// planBazelCleanup inspects `<dir>/BUILD.bazel`, scheduling `# @bazel2gn:`
// annotations and GN-only macro attributes for removal and returning follow-ups
// for prose comments that mention GN/bazel2gn or sit above removed items.
func (idx *Index) planBazelCleanup(dir string) (*BazelEdit, []string) {
	if idx.Root == "" {
		return nil, nil
	}
	bazelRel := path.Join(dir, "BUILD.bazel")
	src, err := os.ReadFile(filepath.Join(idx.Root, bazelRel))
	if err != nil {
		return nil, nil
	}
	ast, err := bazel2gn.ParseBytes(bazelRel, src)
	if err != nil {
		return nil, nil
	}
	lines := strings.Split(string(src), "\n")
	commentsByLine := map[int]syntax.Comment{}
	syntax.Walk(ast, func(n syntax.Node) bool {
		if n == nil {
			return false
		}
		if cs := n.Comments(); cs != nil {
			for _, list := range [][]syntax.Comment{cs.Before, cs.Suffix, cs.After} {
				for _, c := range list {
					commentsByLine[int(c.Start.Line)] = c
				}
			}
		}
		return true
	})

	dropLines := map[int]bool{}
	trimComment := map[int]string{}
	removedHead := map[int]bool{}
	var followUps []string

	for ln, c := range commentsByLine {
		if ln < 1 || ln > len(lines) {
			continue
		}
		body := strings.TrimSpace(strings.TrimPrefix(c.Text, "#"))
		if !strings.HasPrefix(body, "@bazel2gn:") {
			continue
		}
		if strings.TrimSpace(lines[ln-1]) == c.Text {
			dropLines[ln] = true
			removedHead[ln] = true
		} else {
			trimComment[ln] = c.Text
		}
	}

	for _, stmt := range ast.Stmts {
		es, ok := stmt.(*syntax.ExprStmt)
		if !ok {
			continue
		}
		call, ok := es.X.(*syntax.CallExpr)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok {
			continue
		}
		ruleName := ""
		for _, arg := range call.Args {
			if bin, ok := arg.(*syntax.BinaryExpr); ok && bin.Op == syntax.EQ {
				if k, ok := bin.X.(*syntax.Ident); ok && k.Name == "name" {
					if lit, ok := bin.Y.(*syntax.Literal); ok && lit.Token == syntax.STRING {
						if s, ok := lit.Value.(string); ok {
							ruleName = s
						}
					}
				}
			}
		}
		for _, arg := range call.Args {
			bin, ok := arg.(*syntax.BinaryExpr)
			if !ok || bin.Op != syntax.EQ {
				continue
			}
			key, ok := bin.X.(*syntax.Ident)
			if !ok || !isGNOnlyBazelAttr(fn.Name, key.Name) {
				continue
			}
			start, end := bin.Span()
			sLine, eLine := int(start.Line), int(end.Line)
			ownLines := int(call.Lparen.Line) < sLine && int(call.Rparen.Line) > eLine && sLine >= 1 && eLine <= len(lines)
			if ownLines {
				for _, other := range call.Args {
					if other == arg {
						continue
					}
					os, oe := other.Span()
					if !(int(oe.Line) < sLine || int(os.Line) > eLine) {
						ownLines = false
						break
					}
				}
			}
			if ownLines {
				for ln := sLine; ln <= eLine; ln++ {
					dropLines[ln] = true
				}
				removedHead[sLine] = true
			} else {
				followUps = append(followUps, fmt.Sprintf("%s:%d %s attribute %q is for GN conversion only; remove it", bazelRel, sLine, bazelTargetLabel(fn.Name, ruleName), key.Name))
			}
		}
	}

	var remLines []int
	for ln := range commentsByLine {
		if ln >= 1 && ln <= len(lines) && !dropLines[ln] && trimComment[ln] == "" {
			remLines = append(remLines, ln)
		}
	}
	sort.Ints(remLines)
	fullLineComment := func(ln int) bool {
		return strings.TrimSpace(lines[ln-1]) == commentsByLine[ln].Text
	}
	for i := 0; i < len(remLines); {
		first := remLines[i]
		last := first
		j := i + 1
		if fullLineComment(first) {
			for j < len(remLines) && remLines[j] == last+1 && fullLineComment(remLines[j]) {
				last = remLines[j]
				j++
			}
		}
		i = j
		// Skip the file's top copyright/license header.
		if first == 1 && strings.Contains(commentsByLine[first].Text, "Copyright") {
			continue
		}
		mentionsGN := false
		for ln := first; ln <= last; ln++ {
			if gnMentionRE.MatchString(commentsByLine[ln].Text) {
				mentionsGN = true
				break
			}
		}
		switch {
		case mentionsGN:
			followUps = append(followUps, fmt.Sprintf("%s:%d %q mentions GN/bazel2gn; remove or update it", bazelRel, first, commentsByLine[first].Text))
		case fullLineComment(last) && removedHead[last+1]:
			followUps = append(followUps, fmt.Sprintf("%s:%d %q was above a removed bazel2gn item; check whether it still applies", bazelRel, first, commentsByLine[first].Text))
		}
	}

	var ed *BazelEdit
	if len(dropLines) > 0 || len(trimComment) > 0 {
		ed = &BazelEdit{DropLines: dropLines, TrimComment: trimComment}
	}
	return ed, followUps
}

// privateTarget reports whether `t` sets `visibility = [ ":*" ]`.
func privateTarget(fi *FileIndex, t *Target) bool {
	n := 0
	for _, l := range fi.Literals {
		if l.Target == t && l.Attr == "visibility" && l.InnerAttr == "" {
			if l.Value != ":*" {
				return false
			}
			n++
		}
	}
	return n > 0
}

// Files returns every file the plan touches, sorted.
func (p *Plan) Files() []string {
	set := map[string]bool{}
	for _, f := range p.Deletes {
		set[f] = true
	}
	for f := range p.Truncates {
		set[f] = true
	}
	for f := range p.RemoveEntries {
		set[f] = true
	}
	for f := range p.BazelEdits {
		set[f] = true
	}
	var out []string
	for f := range set {
		out = append(out, f)
	}
	sort.Strings(out)
	return out
}

// Apply performs the plan under `root`, then runs `gn format` on every
// edited (not deleted) GN file. Entries are removed by line: the literal must
// be the only list entry on its line (the `gn format` style for multi-entry
// lists), or the only entry of a single-line list. Apply checks every edit
// before changing any file.
func (p *Plan) Apply(ctx context.Context, root, gnBin string) error {
	edited := map[string][]string{}
	read := func(f string) ([]string, error) {
		if lines, ok := edited[f]; ok {
			return lines, nil
		}
		b, err := os.ReadFile(filepath.Join(root, f))
		if err != nil {
			return nil, err
		}
		lines := strings.Split(string(b), "\n")
		edited[f] = lines
		return lines, nil
	}
	// Truncate first: removed entries are above the sentinel, so their line
	// numbers are unaffected, but removing them would shift the sentinel.
	for f, sl := range p.Truncates {
		lines, err := read(f)
		if err != nil {
			return err
		}
		if sl < 1 || sl > len(lines) {
			return fmt.Errorf("%s: sentinel line %d out of range", f, sl)
		}
		edited[f] = append(lines[:sl-1:sl-1], "")
	}
	for f, entries := range p.RemoveEntries {
		lines, err := read(f)
		if err != nil {
			return err
		}
		// Edit bottom-up so earlier line numbers stay valid.
		sorted := append([]Reference(nil), entries...)
		sort.Slice(sorted, func(i, j int) bool { return sorted[i].Line > sorted[j].Line })
		for _, e := range sorted {
			if e.Line < 1 || e.Line > len(lines) {
				return fmt.Errorf("%s:%d: line out of range", f, e.Line)
			}
			newLine, drop, err := removeEntry(lines[e.Line-1], e.Literal)
			if err != nil {
				return fmt.Errorf("%s:%d: %v", f, e.Line, err)
			}
			if drop {
				lines = append(lines[:e.Line-1], lines[e.Line:]...)
			} else {
				lines[e.Line-1] = newLine
			}
		}
		edited[f] = lines
	}
	for _, f := range p.Deletes {
		delete(edited, f)
	}

	editedBazel := map[string]string{}
	for f, be := range p.BazelEdits {
		b, err := os.ReadFile(filepath.Join(root, f))
		if err != nil {
			return err
		}
		out, err := applyBazelEdit(f, strings.Split(string(b), "\n"), be)
		if err != nil {
			return err
		}
		editedBazel[f] = out
	}

	var toFormat []string
	for f, lines := range edited {
		if err := os.WriteFile(filepath.Join(root, f), []byte(strings.Join(lines, "\n")), 0o644); err != nil {
			return err
		}
		toFormat = append(toFormat, filepath.Join(root, f))
	}
	for f, content := range editedBazel {
		if err := os.WriteFile(filepath.Join(root, f), []byte(content), 0o644); err != nil {
			return err
		}
	}
	for _, f := range p.Deletes {
		if err := os.Remove(filepath.Join(root, f)); err != nil {
			return err
		}
	}
	if len(toFormat) == 0 || gnBin == "" {
		return nil
	}
	sort.Strings(toFormat)
	out, err := exec.CommandContext(ctx, gnBin, append([]string{"format"}, toFormat...)...).CombinedOutput()
	if err != nil {
		return fmt.Errorf("gn format failed (%v): %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

func applyBazelEdit(file string, lines []string, be *BazelEdit) (string, error) {
	var out []string
	droppedSinceText := false
	for i, line := range lines {
		ln := i + 1
		if be.DropLines[ln] {
			droppedSinceText = true
			continue
		}
		if suf, ok := be.TrimComment[ln]; ok {
			idx := strings.LastIndex(line, suf)
			if idx < 0 {
				return "", fmt.Errorf("%s:%d: comment %q not found", file, ln, suf)
			}
			line = strings.TrimRight(line[:idx], " \t")
		}
		isFinalNewline := i == len(lines)-1 && line == ""
		if line == "" && !isFinalNewline && droppedSinceText {
			if len(out) == 0 || out[len(out)-1] == "" {
				continue
			}
		}
		if droppedSinceText {
			trimmed := strings.TrimSpace(line)
			if (isFinalNewline || strings.HasPrefix(trimmed, ")") || strings.HasPrefix(trimmed, "]")) && len(out) > 0 && out[len(out)-1] == "" {
				out = out[:len(out)-1]
			}
		}
		if line != "" {
			droppedSinceText = false
		}
		out = append(out, line)
	}
	return strings.Join(out, "\n"), nil
}

// removeEntry removes the list entry `"literal"` from `line`. It returns
// drop=true if the whole line should be removed.
func removeEntry(line, literal string) (string, bool, error) {
	quoted := regexp.QuoteMeta(`"` + literal + `"`)
	if regexp.MustCompile(`^\s*` + quoted + `\s*,?\s*(#.*)?$`).MatchString(line) {
		return "", true, nil
	}
	single := regexp.MustCompile(`\[\s*` + quoted + `\s*,?\s*\]`)
	if single.MatchString(line) {
		return single.ReplaceAllString(line, "[]"), false, nil
	}
	return "", false, fmt.Errorf("can't remove %q automatically: it shares the line with other code", literal)
}
