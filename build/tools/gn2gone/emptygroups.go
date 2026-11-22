// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"fmt"
	"path"
	"slices"
	"sort"
	"strings"
)

// EmptyGroup is a hand-written GN test group with no `deps` or `public_deps`
// entries, together with its static and dynamic callers across the checkout.
type EmptyGroup struct {
	File   string `json:"file"`
	Dir    string `json:"dir"`
	Line   int    `json:"line"`
	Name   string `json:"name"`
	Label  string `json:"label"`
	Region Region `json:"region"`
	// Callers are static GN references naming this group.
	Callers []Reference `json:"callers,omitempty"`
	// DynamicCallers are `file:line` sites for bare-directory lists or dynamic
	// prefix/local expressions that may reference this group's directory.
	DynamicCallers []string `json:"dynamic_callers,omitempty"`
	// GeneratedHeader is the leading DO NOT EDIT / @generated notice from
	// `File`, if any.
	GeneratedHeader string `json:"generated_header,omitempty"`
}

func normDir(dir string) string {
	if dir == "." {
		return ""
	}
	return dir
}

func groupLabel(dir, name string) string {
	if dir == "" {
		return "//:" + name
	}
	return "//" + dir + ":" + name
}

func dirsUnderPrefix(sortedDirs []string, prefix string) []string {
	if strings.Trim(prefix, "/") == "" {
		return nil
	}
	var out []string
	for i := sort.SearchStrings(sortedDirs, prefix); i < len(sortedDirs); i++ {
		d := sortedDirs[i]
		if !strings.HasPrefix(d, prefix) {
			break
		}
		out = append(out, d)
	}
	return out
}

// EmptyGroups discovers hand-written, unconditional, non-opaque `IsTestGroup`
// targets outside `RegionBelow` across all `BUILD.gn` files that have zero
// `deps` or `public_deps` entries, and collects their static and dynamic
// callers across the checkout.
func (idx *Index) EmptyGroups() []*EmptyGroup {
	var groups []*EmptyGroup
	byDirName := map[string]map[string]*EmptyGroup{}
	byDir := map[string][]*EmptyGroup{}
	egTarget := map[*EmptyGroup]*Target{}
	var allBuildDirs []string

	for _, fi := range idx.Files {
		if path.Base(fi.Path) != "BUILD.gn" {
			continue
		}
		dir := normDir(fi.Dir())
		allBuildDirs = append(allBuildDirs, dir)

		depCount := map[*Target]int{}
		for _, l := range fi.Literals {
			if l.Target != nil && l.InnerAttr == "" && (l.Attr == "deps" || l.Attr == "public_deps") {
				depCount[l.Target]++
			}
		}
		hdrLoaded := false
		hdr := ""
		for _, t := range fi.Targets {
			if t.Region != RegionExternal && t.Region != RegionAbove {
				continue
			}
			if !IsTestGroup(t) || t.Name == "" || strings.Contains(t.Name, "$") {
				continue
			}
			if t.InCondition || t.InLoop || t.InTemplate || t.OpaqueDeps || t.HasInnerControlFlow {
				continue
			}
			if depCount[t] > 0 {
				continue
			}
			if !hdrLoaded {
				hdr = generatedHeader(idx.Root, fi.Path)
				hdrLoaded = true
			}
			eg := &EmptyGroup{
				File:            fi.Path,
				Dir:             dir,
				Line:            t.Line,
				Name:            t.Name,
				Label:           groupLabel(dir, t.Name),
				Region:          t.Region,
				GeneratedHeader: hdr,
			}
			groups = append(groups, eg)
			if byDirName[dir] == nil {
				byDirName[dir] = map[string]*EmptyGroup{}
			}
			byDirName[dir][t.Name] = eg
			byDir[dir] = append(byDir[dir], eg)
			egTarget[eg] = t
		}
	}
	if len(groups) == 0 {
		return nil
	}

	sort.Strings(allBuildDirs)
	allBuildDirs = slices.Compact(allBuildDirs)

	addDynamic := func(d string, l *Literal) {
		site := fmt.Sprintf("%s:%d", l.File, l.Line)
		for _, eg := range byDir[d] {
			if l.Target == egTarget[eg] {
				continue
			}
			eg.DynamicCallers = append(eg.DynamicCallers, site)
		}
	}
	matchDynamicPrefix := func(static string, l *Literal) {
		if d, _, hasColon := strings.Cut(static, ":"); hasColon {
			d = normDir(strings.TrimSuffix(d, "/"))
			addDynamic(d, l)
			return
		}
		dirs := dirsUnderPrefix(allBuildDirs, static)
		if len(dirs) > maxDynamicDirs {
			return
		}
		for _, d := range dirs {
			addDynamic(d, l)
		}
	}

	for _, fi := range idx.Files {
		srcDir := normDir(fi.Dir())
		for _, l := range fi.Literals {
			if IsBazelLabel(l) || isNonLabelAttr(l.Attr) || (l.CallArgOf != "" && l.CallArgOf != "foreach") {
				continue
			}
			if l.Value == "" || strings.ContainsAny(l.Value, " \t") {
				continue
			}
			if l.Suffix && !strings.HasPrefix(l.Value, "//") {
				continue
			}

			raw := stripToolchain(l.Value)
			s := raw
			how := How("")
			if m := leadingVarRE.FindStringSubmatch(s); m != nil {
				if v, ok := idx.Resolver.vars[m[1]]; ok {
					v = strings.TrimSuffix(v, "/")
					rest := strings.TrimPrefix(m[2], "/")
					if v == "" || v == "/" {
						s = "//" + rest
					} else {
						s = v + "/" + rest
					}
					how = HowVarSubstituted
				}
			}

			// Dynamic prefix or dynamic local label.
			if dyn := strings.IndexByte(s, '$'); dyn >= 0 || l.InConcat {
				static := s
				if dyn >= 0 {
					static = s[:dyn]
				}
				if !labelChars.MatchString(static) {
					continue
				}
				switch {
				case strings.HasPrefix(static, "//"):
					matchDynamicPrefix(static[2:], l)
				case l.Region == RegionDotGN || strings.HasPrefix(static, "/"):
				case strings.HasPrefix(static, ":"):
					addDynamic(srcDir, l)
				default:
					matchDynamicPrefix(relativeStatic(fi.Dir(), static), l)
				}
				continue
			}

			// Plain label: static caller or bare-directory dynamic caller.
			d, name, ok := idx.plainLabel(l, fi.Dir())
			if !ok {
				continue
			}
			hasColon := strings.Contains(raw, ":")
			if hasColon || bareDirName(l, d) != "" {
				eg := byDirName[d][name]
				if eg == nil || l.Target == egTarget[eg] {
					continue
				}
				if how == "" {
					switch {
					case strings.HasPrefix(raw, "//"):
						how = HowAbsolute
					case strings.HasPrefix(raw, ":"):
						how = HowLocal
					default:
						how = HowRelative
					}
				}
				eg.Callers = append(eg.Callers, idx.toReference(Ref{
					Lit:     l,
					Dir:     d,
					How:     how,
					RefName: eg.Name,
					Tier:    TierExact,
					Target:  eg.Name,
				}))
				continue
			}

			// Bare directory outside deps/public_deps/data_deps (e.g. `_tools = [ "//dir" ]`).
			if d == "" {
				continue
			}
			if path.Base(raw) == "." || path.Base(raw) == ".." {
				continue
			}
			for _, eg := range byDir[d] {
				if l.File == eg.File {
					continue
				}
				eg.DynamicCallers = append(eg.DynamicCallers, fmt.Sprintf("%s:%d", l.File, l.Line))
			}
		}
	}

	sort.Slice(groups, func(i, j int) bool {
		if groups[i].File != groups[j].File {
			return groups[i].File < groups[j].File
		}
		if groups[i].Line != groups[j].Line {
			return groups[i].Line < groups[j].Line
		}
		return groups[i].Name < groups[j].Name
	})
	for _, eg := range groups {
		sortRefs(eg.Callers)
		sort.Strings(eg.DynamicCallers)
		eg.DynamicCallers = slices.Compact(eg.DynamicCallers)
	}
	return groups
}
