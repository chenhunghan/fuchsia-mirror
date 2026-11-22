// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"path"
	"strings"

	"go.starlark.net/syntax"
)

var (
	allowedResourceAttrs = map[string]bool{
		"name":                    true,
		"src":                     true,
		"dest":                    true,
		"sources":                 true,
		"srcs":                    true,
		"outputs":                 true,
		"allow_binary_output_dir": true,
		"deps":                    true,
		"data_deps":               true,
		"testonly":                true,
		"visibility":              true,
		"tags":                    true,
		"target_compatible_with":  true,
		generatedDepsAttr:         true,
	}

	allowedResourceGroupAttrs = map[string]bool{
		"name":                    true,
		"files":                   true,
		"srcs":                    true,
		"dest":                    true,
		"strip_prefix":            true,
		"basename_only":           true,
		"allow_binary_output_dir": true,
		"deps":                    true,
		"data_deps":               true,
		"testonly":                true,
		"visibility":              true,
		"tags":                    true,
		"target_compatible_with":  true,
		generatedDepsAttr:         true,
	}

	allowedResourceTreeAttrs = map[string]bool{
		"name":                    true,
		"sources":                 true,
		"srcs":                    true,
		"dest_dir":                true,
		"dest":                    true,
		"sources_root":            true,
		"allow_binary_output_dir": true,
		"deps":                    true,
		"data_deps":               true,
		"testonly":                true,
		"visibility":              true,
		"tags":                    true,
		"target_compatible_with":  true,
		generatedDepsAttr:         true,
	}

	allowedResourceCollectionAttrs = map[string]bool{
		"name":                   true,
		"resources":              true,
		"testonly":               true,
		"visibility":             true,
		"tags":                   true,
		"target_compatible_with": true,
	}
)

// isResourceRule reports whether `rule` is one of the resource rules handled by
// `prepareResourceCall()`.
func isResourceRule(rule string) bool {
	switch rule {
	case "resource",
		"fuchsia_package_resource",
		"resource_group",
		"fuchsia_package_resource_group",
		"resource_tree",
		"fuchsia_package_resource_collection":
		return true
	default:
		return false
	}
}

type compiledActionOutputRecord struct {
	actionName string
	gnPath     string
}

// resolveResourceTargets rewrites references to outputs of same-file
// `compiled_action()` and `dtb()` targets in `resource()`,
// `fuchsia_package_resource()`, `resource_group()`,
// `fuchsia_package_resource_group()`, and `resource_tree()` targets into
// `$target_gen_dir/...` or `$target_out_dir/...` paths and adds the generating
// targets to `deps`.
func resolveResourceTargets(f *syntax.File) error {
	actionOuts := map[string]compiledActionOutputRecord{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		actionName, ok := callStringAttr(call, "name")
		if !ok || actionName == "" {
			continue
		}
		switch fn.Name {
		case compiledActionRule:
			outs, err := compiledActionOuts(call)
			if err != nil || len(outs) == 0 {
				continue
			}
			bareRecs := map[string]compiledActionOutputRecord{}
			for key, gnPath := range outs {
				rec := compiledActionOutputRecord{
					actionName: actionName,
					gnPath:     gnPath,
				}
				actionOuts[key] = rec
				if !strings.HasPrefix(key, ":") {
					bareRecs[key] = rec
				}
			}
			if len(bareRecs) == 1 {
				for _, rec := range bareRecs {
					actionOuts[":"+actionName] = rec
				}
			}
		case "dtb":
			if gnPath, ok := dtbOutGNPath(call); ok {
				actionOuts[":"+actionName] = compiledActionOutputRecord{
					actionName: actionName,
					gnPath:     gnPath,
				}
			}
		}
	}

	if len(actionOuts) == 0 {
		return nil
	}

	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !isResourceRule(fn.Name) || fn.Name == "fuchsia_package_resource_collection" {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}

		var addedDeps []string
		seenDeps := map[string]bool{}
		addDep := func(actionName string) {
			dep := ":" + actionName
			if !seenDeps[dep] {
				seenDeps[dep] = true
				addedDeps = append(addedDeps, dep)
			}
		}

		for _, arg := range call.Args {
			if skip, err := hasSkipAnnotation(arg); err != nil || skip {
				continue
			}
			attr, hasName := attrName(arg)
			if !hasName {
				continue
			}
			kv := arg.(*syntax.BinaryExpr)
			switch attr {
			case "src", "sources", "srcs":
				rewriteResourceSourceExpr(kv.Y, actionOuts, addDep)
			case "files":
				rewriteResourceGroupFilesExpr(kv.Y, actionOuts, addDep)
			}
		}

		if len(addedDeps) > 0 {
			appendGeneratedDeps(call, addedDeps)
		}
	}
	return nil
}

func dtbOutGNPath(call *syntax.CallExpr) (string, bool) {
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		attr, hasName := attrName(arg)
		if !hasName || attr != "srcs" {
			continue
		}
		kv := arg.(*syntax.BinaryExpr)
		list, ok := unwrapParenExpr(kv.Y).(*syntax.ListExpr)
		if !ok || len(list.List) != 1 {
			return "", false
		}
		lit, ok := unwrapParenExpr(list.List[0]).(*syntax.Literal)
		if !ok {
			return "", false
		}
		src, ok := lit.Value.(string)
		if !ok {
			return "", false
		}
		if idx := strings.LastIndexAny(src, "/:"); idx != -1 {
			src = src[idx+1:]
		}
		switch {
		case strings.HasSuffix(src, ".dts.S"):
			return "$target_out_dir/" + strings.TrimSuffix(src, ".dts.S") + ".dtb", true
		case strings.HasSuffix(src, ".dts"):
			return "$target_out_dir/" + strings.TrimSuffix(src, ".dts") + ".dtb", true
		default:
			return "", false
		}
	}
	return "", false
}

func rewriteResourceSourceExpr(
	expr syntax.Expr,
	actionOuts map[string]compiledActionOutputRecord,
	addDep func(string),
) {
	switch v := unwrapParenExpr(expr).(type) {
	case *syntax.Literal:
		if _, hasOverwrite := overwrittenPath(v); hasOverwrite {
			return
		}
		s, ok := v.Value.(string)
		if !ok {
			return
		}
		if rec, found := actionOuts[s]; found {
			v.Value = rec.gnPath
			v.Raw = gnQuote(rec.gnPath)
			addDep(rec.actionName)
		}
	case *syntax.ListExpr:
		for _, elem := range v.List {
			if skip, err := hasSkipAnnotation(elem); err != nil || skip {
				continue
			}
			rewriteResourceSourceExpr(elem, actionOuts, addDep)
		}
	case *syntax.BinaryExpr:
		if v.Op == syntax.PLUS {
			rewriteResourceSourceExpr(v.X, actionOuts, addDep)
			rewriteResourceSourceExpr(v.Y, actionOuts, addDep)
		}
	case *syntax.CondExpr:
		rewriteResourceSourceExpr(v.True, actionOuts, addDep)
		rewriteResourceSourceExpr(v.False, actionOuts, addDep)
	case *syntax.CallExpr:
		if isSelectCall(v) && len(v.Args) == 1 {
			if dict, ok := unwrapParenExpr(v.Args[0]).(*syntax.DictExpr); ok {
				for _, entry := range dict.List {
					if kv, ok := entry.(*syntax.DictEntry); ok {
						rewriteResourceSourceExpr(kv.Value, actionOuts, addDep)
					}
				}
			}
		}
	}
}

func rewriteResourceGroupFilesExpr(
	expr syntax.Expr,
	actionOuts map[string]compiledActionOutputRecord,
	addDep func(string),
) {
	list, ok := unwrapParenExpr(expr).(*syntax.ListExpr)
	if !ok {
		return
	}
	for _, elem := range list.List {
		dict, ok := unwrapParenExpr(elem).(*syntax.DictExpr)
		if !ok {
			continue
		}
		for _, entry := range dict.List {
			kv, ok := entry.(*syntax.DictEntry)
			if !ok {
				continue
			}
			k, err := stringLiteralValue(kv.Key)
			if err == nil && k == "source" {
				rewriteResourceSourceExpr(kv.Value, actionOuts, addDep)
			}
		}
	}
}

// appendGeneratedDeps adds `deps` labels to `call` if they are not already
// present in an existing `deps` attribute on `call`.
func appendGeneratedDeps(call *syntax.CallExpr, deps []string) {
	existing := map[string]bool{}
	var depsList *syntax.ListExpr
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		attr, hasName := attrName(arg)
		if !hasName || attr != "deps" {
			continue
		}
		if list, ok := unwrapParenExpr(arg.(*syntax.BinaryExpr).Y).(*syntax.ListExpr); ok {
			depsList = list
			for _, elem := range list.List {
				if s, err := stringLiteralValue(elem); err == nil {
					existing[s] = true
				}
			}
		}
	}

	var toAdd []syntax.Expr
	for _, d := range deps {
		if !existing[d] {
			toAdd = append(toAdd, stringLiteral(d))
		}
	}
	if len(toAdd) == 0 {
		return
	}
	if depsList != nil {
		depsList.List = append(depsList.List, toAdd...)
		return
	}
	call.Args = append(call.Args, attrExpr(generatedDepsAttr, &syntax.ListExpr{List: toAdd}))
}

// prepareResourceCall validates a resource rule call and rewrites its arguments
// in place so the generic `callExprToGN()` loop can emit the corresponding GN
// template (`resource`, `resource_group`, `resource_tree`, or `group`).
func prepareResourceCall(call *syntax.CallExpr, gnTemplateName *string, pkgPath string, hasPkgPath bool) error {
	rule := call.Fn.(*syntax.Ident).Name
	switch rule {
	case "resource", "fuchsia_package_resource":
		return prepareResource(call, rule)
	case "resource_group", "fuchsia_package_resource_group":
		return prepareResourceGroup(call, rule, gnTemplateName, pkgPath, hasPkgPath)
	case "resource_tree":
		return prepareResourceTree(call)
	case "fuchsia_package_resource_collection":
		return validateResourceCollection(call)
	default:
		return fmt.Errorf("unexpected resource rule %q", rule)
	}
}

func prepareResource(call *syntax.CallExpr, rule string) error {
	var name string
	var srcArg, destArg, sourcesArg, outputsArg *syntax.BinaryExpr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("%s only accepts keyword arguments, got %T", rule, arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedResourceAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of %s; use # @bazel2gn:skip and write the GN copy by hand", ident.Name, rule)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("%s name: %v", rule, err)
			}
		case "src":
			srcArg = binaryExpr
		case "dest":
			destArg = binaryExpr
		case "sources", "srcs":
			if sourcesArg != nil {
				return fmt.Errorf("%s %q cannot specify both `sources` and `srcs`", rule, name)
			}
			sourcesArg = binaryExpr
		case "outputs":
			outputsArg = binaryExpr
		}
	}

	if name == "" {
		return fmt.Errorf("missing `name` attribute in %s", rule)
	}

	if srcArg != nil || destArg != nil {
		if sourcesArg != nil || outputsArg != nil {
			return fmt.Errorf("%s %q cannot specify both `src`/`dest` and `sources`/`outputs`", rule, name)
		}
		if srcArg == nil {
			return fmt.Errorf("missing `src` attribute in %s %q", rule, name)
		}
		if destArg == nil {
			return fmt.Errorf("missing `dest` attribute in %s %q", rule, name)
		}
		if err := validateResourceDestExpr(destArg.Y, rule, name, "dest"); err != nil {
			return err
		}
		srcList, err := scalarToListExpr(srcArg)
		if err != nil {
			return fmt.Errorf("%s %q src: %v", rule, name, err)
		}
		destList, err := scalarToListExpr(destArg)
		if err != nil {
			return fmt.Errorf("%s %q dest: %v", rule, name, err)
		}
		srcArg.X = &syntax.Ident{Name: "sources"}
		srcArg.Y = srcList
		destArg.X = &syntax.Ident{Name: "outputs"}
		destArg.Y = destList
		return nil
	}

	if sourcesArg == nil {
		return fmt.Errorf("missing `src` or `sources` attribute in %s %q", rule, name)
	}
	if outputsArg == nil {
		return fmt.Errorf("missing `outputs` attribute in %s %q", rule, name)
	}
	return validateResourceOutputsExpr(outputsArg.Y, rule, name)
}

// scalarToListExpr wraps a scalar string literal expression in a single-element
// `*syntax.ListExpr`, preserving suffix comments such as
// `# @bazel2gn:path_overwrite:` on the inner literal.
func scalarToListExpr(owner *syntax.BinaryExpr) (syntax.Expr, error) {
	if path, ok := overwrittenPathOnOwnerOnly(owner, owner.Y); ok {
		lit := unwrapParenExpr(owner.Y)
		lit.AllocComments()
		lit.Comments().Suffix = append(lit.Comments().Suffix, syntax.Comment{
			Text: pathOverwriteAnnotationPrefix + path,
		})
		clearPathOverwriteComments(owner)
	}
	lit, ok := unwrapParenExpr(owner.Y).(*syntax.Literal)
	if !ok {
		return nil, fmt.Errorf("expected string literal, got %T", owner.Y)
	}
	if _, ok := lit.Value.(string); !ok {
		return nil, fmt.Errorf("expected string literal, got %s", lit.Raw)
	}
	return &syntax.ListExpr{List: []syntax.Expr{lit}}, nil
}

func overwrittenPathOnOwnerOnly(owner syntax.Node, inner syntax.Expr) (string, bool) {
	if lit, ok := unwrapParenExpr(inner).(*syntax.Literal); ok {
		if _, has := overwrittenPath(lit); has {
			return "", false
		}
	}
	comments := owner.Comments()
	if comments == nil {
		return "", false
	}
	for _, c := range comments.Suffix {
		if after, ok := strings.CutPrefix(c.Text, pathOverwriteAnnotationPrefix); ok {
			return strings.TrimSpace(after), true
		}
	}
	return "", false
}

func clearPathOverwriteComments(node syntax.Node) {
	comments := node.Comments()
	if comments == nil {
		return
	}
	filtered := comments.Suffix[:0]
	for _, c := range comments.Suffix {
		if !strings.HasPrefix(c.Text, pathOverwriteAnnotationPrefix) {
			filtered = append(filtered, c)
		}
	}
	comments.Suffix = filtered
}

func validateResourceDestExpr(expr syntax.Expr, rule, name, attr string) error {
	dest, err := stringLiteralValue(expr)
	if err != nil {
		return fmt.Errorf("%s %q %s: %v", rule, name, attr, err)
	}
	if dest == "" {
		return fmt.Errorf("%s %q %s must not be empty", rule, name, attr)
	}
	if strings.HasPrefix(dest, "/") {
		return fmt.Errorf("%s %q %s %q must be a relative package path (must not start with \"/\")", rule, name, attr, dest)
	}
	return nil
}

func validateResourceOutputsExpr(expr syntax.Expr, rule, name string) error {
	list, ok := unwrapParenExpr(expr).(*syntax.ListExpr)
	if !ok {
		return fmt.Errorf("%s %q outputs must be a list of one string literal, got %T", rule, name, expr)
	}
	if len(list.List) != 1 {
		return fmt.Errorf("%s %q outputs must contain exactly 1 path, got %d", rule, name, len(list.List))
	}
	return validateResourceDestExpr(list.List[0], rule, name, "outputs")
}

func prepareResourceGroup(call *syntax.CallExpr, rule string, gnTemplateName *string, pkgPath string, hasPkgPath bool) error {
	var name string
	var filesArg, srcsArg, destArg, stripPrefixArg, basenameOnlyArg *syntax.BinaryExpr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("%s only accepts keyword arguments, got %T", rule, arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedResourceGroupAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of %s; use # @bazel2gn:skip and write the GN copy by hand", ident.Name, rule)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("%s name: %v", rule, err)
			}
		case "files":
			filesArg = binaryExpr
		case "srcs":
			srcsArg = binaryExpr
		case "dest":
			destArg = binaryExpr
		case "strip_prefix":
			stripPrefixArg = binaryExpr
		case "basename_only":
			basenameOnlyArg = binaryExpr
		}
	}

	if name == "" {
		return fmt.Errorf("missing `name` attribute in %s", rule)
	}

	if filesArg != nil {
		if srcsArg != nil || destArg != nil || stripPrefixArg != nil || basenameOnlyArg != nil {
			return fmt.Errorf("%s %q cannot specify both `files` and `srcs`/`dest`", rule, name)
		}
		return validateResourceGroupFilesExpr(filesArg.Y, rule, name)
	}

	if srcsArg == nil || destArg == nil {
		if srcsArg == nil && destArg == nil {
			return fmt.Errorf("missing `files` or (`srcs` and `dest`) in %s %q", rule, name)
		}
		if srcsArg == nil {
			return fmt.Errorf("missing `srcs` attribute in %s %q", rule, name)
		}
		return fmt.Errorf("missing `dest` attribute in %s %q", rule, name)
	}

	dest, err := stringLiteralValue(destArg.Y)
	if err != nil {
		return fmt.Errorf("%s %q dest: %v", rule, name, err)
	}
	if strings.HasPrefix(dest, "/") {
		return fmt.Errorf("%s %q dest %q must be a relative package path (must not start with \"/\")", rule, name, dest)
	}
	dest = strings.TrimSuffix(dest, "/")

	stripPrefix := ""
	if stripPrefixArg != nil {
		stripPrefix, err = stringLiteralValue(stripPrefixArg.Y)
		if err != nil {
			return fmt.Errorf("%s %q strip_prefix: %v", rule, name, err)
		}
	}

	basenameOnly := false
	if basenameOnlyArg != nil {
		ident, ok := unwrapParenExpr(basenameOnlyArg.Y).(*syntax.Ident)
		if !ok || (ident.Name != "True" && ident.Name != "False") {
			return fmt.Errorf("%s %q basename_only must be True or False, got %T", rule, name, basenameOnlyArg.Y)
		}
		basenameOnly = ident.Name == "True"
	}

	// When `basename_only = True`, every file in `srcs` is installed under
	// `<dest>/{{source_file_part}}`, which maps directly to a GN `resource()`
	// target.
	if basenameOnly {
		*gnTemplateName = "resource"
		outPattern := "{{source_file_part}}"
		if dest != "" {
			outPattern = dest + "/{{source_file_part}}"
		}
		newArgs := make([]syntax.Expr, 0, len(call.Args))
		for _, arg := range call.Args {
			attr, hasName := attrName(arg)
			if !hasName {
				newArgs = append(newArgs, arg)
				continue
			}
			switch attr {
			case "srcs":
				kv := *arg.(*syntax.BinaryExpr)
				kv.X = &syntax.Ident{Name: "sources"}
				newArgs = append(newArgs, &kv)
			case "dest":
				newArgs = append(newArgs, attrExpr("outputs", &syntax.ListExpr{
					List: []syntax.Expr{stringLiteral(outPattern)},
				}))
			case "strip_prefix", "basename_only":
				// Handled via `outputs = ["<dest>/{{source_file_part}}"]`.
			default:
				newArgs = append(newArgs, arg)
			}
		}
		call.Args = newArgs
		return nil
	}

	// Otherwise, expand `srcs` + `dest` + `strip_prefix` into `files` entries on
	// GN `resource_group()`.
	srcsList, ok := unwrapParenExpr(srcsArg.Y).(*syntax.ListExpr)
	if !ok {
		return fmt.Errorf("%s %q srcs must be a list literal when basename_only is not True, got %T", rule, name, srcsArg.Y)
	}
	var fileDicts []syntax.Expr
	for _, elem := range srcsList.List {
		skip, err := hasSkipAnnotation(elem)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		lit, ok := unwrapParenExpr(elem).(*syntax.Literal)
		if !ok {
			return fmt.Errorf("%s %q srcs must contain string literals, got %T", rule, name, elem)
		}
		rawLabel, ok := lit.Value.(string)
		if !ok {
			return fmt.Errorf("%s %q srcs must contain string literals, got %s", rule, name, lit.Raw)
		}
		relPath, err := resourceGroupDestRelPath(rawLabel, stripPrefix, pkgPath, hasPkgPath)
		if err != nil {
			return fmt.Errorf("%s %q srcs %q: %v", rule, name, rawLabel, err)
		}
		fullDest := relPath
		if dest != "" {
			fullDest = dest + "/" + relPath
		}
		if fullDest == "" || strings.HasPrefix(fullDest, "/") {
			return fmt.Errorf("%s %q computed invalid destination path %q for %q", rule, name, fullDest, rawLabel)
		}
		fileDicts = append(fileDicts, &syntax.DictExpr{
			List: []syntax.Expr{
				&syntax.DictEntry{
					Key:   stringLiteral("source"),
					Value: lit,
				},
				&syntax.DictEntry{
					Key:   stringLiteral("dest"),
					Value: stringLiteral(fullDest),
				},
			},
		})
	}

	newArgs := make([]syntax.Expr, 0, len(call.Args))
	for _, arg := range call.Args {
		attr, hasName := attrName(arg)
		if !hasName {
			newArgs = append(newArgs, arg)
			continue
		}
		switch attr {
		case "srcs":
			newArgs = append(newArgs, attrExpr("files", &syntax.ListExpr{List: fileDicts}))
		case "dest", "strip_prefix", "basename_only":
			// Expanded into `files`.
		default:
			newArgs = append(newArgs, arg)
		}
	}
	call.Args = newArgs
	return nil
}

// resourceGroupDestRelPath computes the package-relative path for a source
// label in `fuchsia_package_resource_group()`, matching
// `_fuchsia_package_resource_group_impl` in `resources.bzl`.
func resourceGroupDestRelPath(label, stripPrefix, pkgPath string, hasPkgPath bool) (string, error) {
	if strings.HasPrefix(label, "$target_gen_dir/") {
		rel := strings.TrimPrefix(label, "$target_gen_dir/")
		rel = strings.TrimPrefix(rel, stripPrefix)
		rel = strings.TrimPrefix(rel, "/")
		return rel, nil
	}
	var rel string
	if strings.HasPrefix(label, "//") {
		if !hasPkgPath {
			return "", fmt.Errorf("absolute label %q on non-basename_only resource_group requires package path context (use a relative path/label or # @bazel2gn:skip)", label)
		}
		pkg, file, ok := strings.Cut(label[2:], ":")
		if !ok {
			return "", fmt.Errorf("expected a label of a file in the form //path/to:file, got %q", label)
		}
		if pkg == "" {
			rel = file
		} else {
			rel = pkg + "/" + file
		}
		if pkgPath != "" {
			rel = strings.TrimPrefix(rel, pkgPath+"/")
		}
	} else {
		rel = strings.TrimPrefix(label, ":")
	}
	rel = strings.TrimPrefix(rel, stripPrefix)
	rel = strings.TrimPrefix(rel, "/")
	if rel == "" {
		rel = path.Base(label)
	}
	return rel, nil
}

func validateResourceGroupFilesExpr(expr syntax.Expr, rule, name string) error {
	switch v := unwrapParenExpr(expr).(type) {
	case *syntax.ListExpr:
		for _, elem := range v.List {
			skip, err := hasSkipAnnotation(elem)
			if err != nil {
				return err
			}
			if skip {
				continue
			}
			dict, ok := unwrapParenExpr(elem).(*syntax.DictExpr)
			if !ok {
				return fmt.Errorf("%s %q files entries must be dicts with \"source\" and \"dest\", got %T", rule, name, elem)
			}
			if err := validateResourceGroupFileDict(dict, rule, name); err != nil {
				return err
			}
		}
		return nil
	default:
		return fmt.Errorf("%s %q files must be a list of dicts, got %T", rule, name, expr)
	}
}

func validateResourceGroupFileDict(dict *syntax.DictExpr, rule, name string) error {
	var hasSource, hasDest bool
	for _, entry := range dict.List {
		kv, ok := entry.(*syntax.DictEntry)
		if !ok {
			return fmt.Errorf("unexpected entry in %s %q files dict: %T", rule, name, entry)
		}
		key, err := stringLiteralValue(kv.Key)
		if err != nil {
			return fmt.Errorf("%s %q files dict key: %v", rule, name, err)
		}
		switch key {
		case "source":
			hasSource = true
			if _, err := stringLiteralValue(kv.Value); err != nil {
				return fmt.Errorf("%s %q files \"source\": %v", rule, name, err)
			}
		case "dest":
			hasDest = true
			if err := validateResourceDestExpr(kv.Value, rule, name, "files.dest"); err != nil {
				return err
			}
		default:
			return fmt.Errorf("unsupported key %q in %s %q files dict (expected \"source\" and \"dest\")", key, rule, name)
		}
	}
	if !hasSource || !hasDest {
		return fmt.Errorf("each entry in %s %q files must have both \"source\" and \"dest\" keys", rule, name)
	}
	return nil
}

// bazelResourceGroupFilesToGN transforms the `source` entries of `files` dicts
// in `resource_group()` using `bazelFilePathsToGN`.
func bazelResourceGroupFilesToGN(expr syntax.Expr) (syntax.Expr, error) {
	dict, ok := expr.(*syntax.DictExpr)
	if !ok {
		return expr, nil
	}
	for _, entry := range dict.List {
		kv, ok := entry.(*syntax.DictEntry)
		if !ok {
			continue
		}
		key, err := stringLiteralValue(kv.Key)
		if err != nil || key != "source" {
			continue
		}
		transformed, err := bazelFilePathsToGN(kv.Value)
		if err != nil {
			return nil, err
		}
		kv.Value = transformed
	}
	return dict, nil
}

func prepareResourceTree(call *syntax.CallExpr) error {
	var name string
	var hasSources, hasDestDir bool

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("resource_tree only accepts keyword arguments, got %T", arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedResourceTreeAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of resource_tree; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("resource_tree name: %v", err)
			}
		case "sources", "srcs":
			if hasSources {
				return fmt.Errorf("resource_tree %q cannot specify both `sources` and `srcs`", name)
			}
			hasSources = true
			if list, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.ListExpr); ok {
				for _, elem := range list.List {
					if skip, err := hasSkipAnnotation(elem); err != nil || skip {
						continue
					}
					s, err := stringLiteralValue(elem)
					if err == nil && (strings.HasPrefix(s, ":") || strings.HasPrefix(s, "/") || strings.HasPrefix(s, "@") || strings.HasPrefix(s, "$")) {
						return fmt.Errorf("resource_tree %q sources must be paths relative to sources_root, got %q", name, s)
					}
				}
			}
		case "dest_dir", "dest":
			if hasDestDir {
				return fmt.Errorf("resource_tree %q cannot specify both `dest_dir` and `dest`", name)
			}
			hasDestDir = true
			ident.Name = "dest_dir"
			destDir, err := stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("resource_tree %q dest_dir: %v", name, err)
			}
			if strings.HasPrefix(destDir, "/") {
				return fmt.Errorf("resource_tree %q dest_dir %q cannot start with \"/\"", name, destDir)
			}
		case "sources_root":
			root, err := stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("resource_tree %q sources_root: %v", name, err)
			}
			if root == "" {
				return fmt.Errorf("resource_tree %q sources_root cannot be empty", name)
			}
		}
	}

	if name == "" {
		return errors.New("missing `name` attribute in resource_tree")
	}
	if !hasSources {
		return fmt.Errorf("missing `sources` attribute in resource_tree %q", name)
	}
	if !hasDestDir {
		return fmt.Errorf("missing `dest_dir` attribute in resource_tree %q", name)
	}
	return nil
}

func validateResourceCollection(call *syntax.CallExpr) error {
	var name string
	var hasResources bool

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("fuchsia_package_resource_collection only accepts keyword arguments, got %T", arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedResourceCollectionAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of fuchsia_package_resource_collection; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("fuchsia_package_resource_collection name: %v", err)
			}
		case "resources":
			hasResources = true
		}
	}

	if name == "" {
		return errors.New("missing `name` attribute in fuchsia_package_resource_collection")
	}
	if !hasResources {
		return fmt.Errorf("missing `resources` attribute in fuchsia_package_resource_collection %q", name)
	}
	return nil
}
