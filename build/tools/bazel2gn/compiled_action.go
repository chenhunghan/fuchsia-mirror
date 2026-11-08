// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"fmt"
	"regexp"
	"strings"

	"go.starlark.net/syntax"
)

// compiledActionRule is the name of the Bazel rule defined in
// //build/bazel/rules/compiled_action.bzl, which is converted to the GN
// `compiled_action()` template defined in //build/compiled_action.gni.
const compiledActionRule = "compiled_action"

// compiledActionRuleDir is the Bazel `$(RULEDIR)` variable supported in
// `compiled_action()` args.
const compiledActionRuleDir = "$(RULEDIR)"

// compiledActionGNRuleDir is the GN path of the directory containing the
// outputs of a `compiled_action()`, i.e. the GN equivalent of `$(RULEDIR)`.
const compiledActionGNRuleDir = "$target_gen_dir"

// compiledActionLocationRE matches the `$(location <label>)` and
// `$(execpath <label>)` substitutions supported in `compiled_action()` args.
var compiledActionLocationRE = regexp.MustCompile(`\$\((location|execpath)\s+([^()\s]+)\s*\)`)

var allowedCompiledActionAttrs = map[string]bool{
	"name":                   true,
	"tool":                   true,
	"tool_output_name":       true,
	"srcs":                   true,
	"deps":                   true,
	"outs":                   true,
	"depfile":                true,
	"args":                   true,
	"mnemonic":               true,
	"testonly":               true,
	"visibility":             true,
	"tags":                   true,
	"target_compatible_with": true,
	"applicable_licenses":    true,
	"exec_properties":        true,
}

// validateCompiledAction checks that all un-skipped attributes on a
// `compiled_action()` call are supported by bazel2gn.
func validateCompiledAction(call *syntax.CallExpr) error {
	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			continue
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return err
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			continue
		}
		if !allowedCompiledActionAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of %s; use # @bazel2gn:skip and write the GN copy by hand", ident.Name, compiledActionRule)
		}
	}
	return nil
}

// compiledActionOutToGN converts a single Bazel `compiled_action()` output
// (an element of `outs` or `depfile`) to its GN path.
//
// Bazel outputs are relative to the package's output directory, the GN
// equivalent of which is `$target_gen_dir`. A path overwrite annotation, if
// any, takes precedence.
func compiledActionOutToGN(lit *syntax.Literal) (string, error) {
	if path, ok := overwrittenPath(lit); ok {
		return path, nil
	}
	out, ok := lit.Value.(string)
	if !ok {
		return "", fmt.Errorf("expected string literal in compiled_action outs, got %s", lit.Raw)
	}
	return compiledActionGNRuleDir + "/" + out, nil
}

// compiledActionOutsTransformer is a transformer converting Bazel
// `compiled_action()` outputs (in `outs` or `depfile`) to GN paths, see
// `compiledActionOutToGN`.
func compiledActionOutsTransformer(expr syntax.Expr) (syntax.Expr, error) {
	lit, ok := expr.(*syntax.Literal)
	if !ok {
		return expr, nil
	}
	path, err := compiledActionOutToGN(lit)
	if err != nil {
		return nil, err
	}
	lit.Raw = gnQuote(path)
	return lit, nil
}

// compiledActionScalarOutTransformer converts a scalar output attribute (such
// as `depfile`) of a Bazel `compiled_action()` to a GN path, checking suffix
// comments on both the enclosing `attr = value` expression and the literal.
func compiledActionScalarOutTransformer(owner syntax.Node) transformer {
	return func(expr syntax.Expr) (syntax.Expr, error) {
		lit, ok := unwrapParenExpr(expr).(*syntax.Literal)
		if !ok {
			return expr, nil
		}
		if path, ok := overwrittenPathOnAny(owner, lit); ok {
			lit.Raw = gnQuote(path)
			return lit, nil
		}
		path, err := compiledActionOutToGN(lit)
		if err != nil {
			return nil, err
		}
		lit.Raw = gnQuote(path)
		return lit, nil
	}
}

// compiledActionToolTransformer validates and converts the `tool` attribute of
// a Bazel `compiled_action()` to GN, checking suffix comments on both the
// enclosing `attr = value` expression and the literal.
func compiledActionToolTransformer(owner syntax.Node) transformer {
	return func(expr syntax.Expr) (syntax.Expr, error) {
		unwrapped := unwrapParenExpr(expr)
		if isSelectCall(unwrapped) {
			return expr, nil
		}
		if _, ok := unwrapped.(*syntax.CondExpr); ok {
			return expr, nil
		}
		lit, ok := unwrapped.(*syntax.Literal)
		if !ok {
			return nil, fmt.Errorf("compiled_action tool must be a string literal, got %T", expr)
		}
		if raw, ok := overwrittenRawOnAny(owner, lit); ok {
			lit.Raw = raw
			return lit, nil
		}
		if path, ok := overwrittenPathOnAny(owner, lit); ok {
			lit.Raw = gnQuote(path)
			return lit, nil
		}
		val, ok := lit.Value.(string)
		if !ok || val == "" {
			return nil, fmt.Errorf("compiled_action tool must be a non-empty string literal, got %s", lit.Raw)
		}
		return bazelDepToGN(lit)
	}
}

// compiledActionStringAttrTransformer validates a non-empty string literal
// attribute (such as `tool_output_name` or `mnemonic`) on `compiled_action()`.
func compiledActionStringAttrTransformer(attr string) transformer {
	return func(expr syntax.Expr) (syntax.Expr, error) {
		unwrapped := unwrapParenExpr(expr)
		if isSelectCall(unwrapped) {
			return expr, nil
		}
		if _, ok := unwrapped.(*syntax.CondExpr); ok {
			return expr, nil
		}
		lit, ok := unwrapped.(*syntax.Literal)
		if !ok {
			return nil, fmt.Errorf("compiled_action %s must be a string literal, got %T", attr, expr)
		}
		if raw, ok := overwrittenRaw(lit); ok {
			lit.Raw = raw
			return lit, nil
		}
		val, ok := lit.Value.(string)
		if !ok || val == "" {
			return nil, fmt.Errorf("compiled_action %s must be a non-empty string literal, got %s", attr, lit.Raw)
		}
		return lit, nil
	}
}

// compiledActionPaths returns a map from the Bazel labels that can appear in
// `$(location <label>)` or `$(execpath <label>)` in `compiled_action()` `args`
// to their GN paths:
//   - `outs` and `depfile` (e.g. "foo.txt" and ":foo.txt") map to
//     "$target_gen_dir/<file>" (or their `@bazel2gn:path_overwrite` path).
//   - `srcs` and `deps` entries with `@bazel2gn:path_overwrite` map to their
//     overwritten GN path.
//   - `deps` entries without `@bazel2gn:path_overwrite` map to `""` so
//     `compiledActionArgToGN` can reject them with a descriptive error instead
//     of treating the target label as a source file path.
func compiledActionPaths(call *syntax.CallExpr) (map[string]string, error) {
	outs := map[string]string{}
	srcOverwrites := map[string]string{}
	unannotatedDeps := map[string]string{}
	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			continue
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return nil, err
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			continue
		}
		if _, ok := overwrittenRaw(binaryExpr); ok {
			// The GN value is set by hand, so its paths aren't inferred here.
			continue
		}
		switch ident.Name {
		case "outs":
			list, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.ListExpr)
			if !ok {
				return nil, fmt.Errorf("compiled_action outs must be a list literal, got %T", binaryExpr.Y)
			}
			for _, elm := range list.List {
				elmSkip, err := hasSkipAnnotation(elm)
				if err != nil {
					return nil, err
				}
				if elmSkip {
					continue
				}
				lit, ok := unwrapParenExpr(elm).(*syntax.Literal)
				if !ok {
					return nil, fmt.Errorf("compiled_action outs must only contain string literals, got %T", elm)
				}
				path, err := compiledActionOutToGN(lit)
				if err != nil {
					return nil, err
				}
				out := lit.Value.(string)
				outs[out] = path
				outs[":"+out] = path
			}
		case "depfile":
			lit, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.Literal)
			if !ok {
				return nil, fmt.Errorf("compiled_action depfile must be a string literal, got %T", binaryExpr.Y)
			}
			depfile, isStr := lit.Value.(string)
			if !isStr || depfile == "" {
				return nil, fmt.Errorf("compiled_action depfile must be a non-empty string literal, got %s", lit.Raw)
			}
			path, hasOverwrite := overwrittenPathOnAny(binaryExpr, lit)
			if !hasOverwrite {
				var err error
				path, err = compiledActionOutToGN(lit)
				if err != nil {
					return nil, err
				}
			}
			outs[depfile] = path
			outs[":"+depfile] = path
		case "srcs", "deps":
			isDeps := ident.Name == "deps"
			syntax.Walk(binaryExpr.Y, func(n syntax.Node) bool {
				if n == nil {
					return false
				}
				if skip, err := hasSkipAnnotation(n); err == nil && skip {
					return false
				}
				lit, ok := n.(*syntax.Literal)
				if !ok {
					return true
				}
				src, isStr := lit.Value.(string)
				if !isStr || src == "" {
					return true
				}
				overwritten, hasOverwrite := overwrittenPath(lit)
				if !hasOverwrite {
					if isDeps {
						recordCompiledActionLabel(unannotatedDeps, src, "")
					}
					return true
				}
				recordCompiledActionLabel(srcOverwrites, src, overwritten)
				return true
			})
		}
	}
	paths := unannotatedDeps
	for k, v := range srcOverwrites {
		paths[k] = v
	}
	for k, v := range outs {
		paths[k] = v
	}
	return paths, nil
}

func recordCompiledActionLabel(m map[string]string, label, val string) {
	m[label] = val
	if !strings.HasPrefix(label, "//") && !strings.HasPrefix(label, "@") {
		bare := strings.TrimPrefix(label, ":")
		m[bare] = val
		m[":"+bare] = val
	}
}

// compiledActionArgsTransformer returns a transformer that converts the `args`
// attribute of a Bazel `compiled_action()` to GN.
//
// `paths` maps from Bazel labels of the outputs (`outs`, `depfile`) and
// path-overwritten `srcs` or `deps` of the action to their GN paths (and maps
// un-overwritten `deps` labels to `""` so references to them are rejected),
// see `compiledActionPaths`.
//
// Each element in args must be a string literal, and supports the following
// substitutions:
//
//   - `$(location <label>)` and `$(execpath <label>)`, where `<label>` is in
//     `outs`, `depfile`, `srcs`, or `path_overwrite`-annotated `deps`, are
//     converted to `rebase_path("<path>", root_build_dir)`.
//   - `$(RULEDIR)` is converted to
//     `rebase_path(target_gen_dir, root_build_dir)`.
//
// Any other `$(...)` substitution (or a reference to an un-overwritten `deps`
// label) is an error.
func compiledActionArgsTransformer(paths map[string]string) transformer {
	return func(expr syntax.Expr) (syntax.Expr, error) {
		unwrapped := unwrapParenExpr(expr)
		switch v := unwrapped.(type) {
		case *syntax.ListExpr, *syntax.CondExpr:
			return expr, nil
		case *syntax.CallExpr:
			if isSelectCall(v) {
				return expr, nil
			}
		case *syntax.BinaryExpr:
			if v.Op == syntax.PLUS {
				return expr, nil
			}
		}
		lit, ok := unwrapped.(*syntax.Literal)
		if !ok {
			return nil, fmt.Errorf("compiled_action args must be a list of string literals, got %T", expr)
		}
		if raw, ok := overwrittenRaw(lit); ok {
			lit.Raw = raw
			return lit, nil
		}
		arg, ok := lit.Value.(string)
		if !ok {
			return nil, fmt.Errorf("compiled_action args must only contain string literals, got %s", lit.Raw)
		}
		gnArg, err := compiledActionArgToGN(arg, paths)
		if err != nil {
			return nil, err
		}
		lit.Raw = gnArg
		return lit, nil
	}
}

// compiledActionArgToGN converts a single `compiled_action()` argument to a
// GN expression. See `compiledActionArgsTransformer` for supported substitutions.
func compiledActionArgToGN(arg string, outs map[string]string) (string, error) {
	var parts []string
	for len(arg) > 0 {
		start := strings.Index(arg, "$(")
		if start == -1 {
			parts = append(parts, gnQuoteLiteral(arg))
			break
		}
		if start > 0 {
			parts = append(parts, gnQuoteLiteral(arg[:start]))
			arg = arg[start:]
		}

		if strings.HasPrefix(arg, compiledActionRuleDir) {
			parts = append(parts, "rebase_path(target_gen_dir, root_build_dir)")
			arg = arg[len(compiledActionRuleDir):]
			continue
		}

		m := compiledActionLocationRE.FindStringSubmatchIndex(arg)
		if m == nil || m[0] != 0 {
			end := strings.Index(arg, ")")
			if end == -1 {
				end = len(arg) - 1
			}
			return "", fmt.Errorf("unsupported substitution %q in compiled_action args, only $(location <label>), $(execpath <label>), and %s are supported", arg[:end+1], compiledActionRuleDir)
		}
		label := arg[m[4]:m[5]]
		path, ok := outs[label]
		if ok && path == "" {
			return "", fmt.Errorf("$(%s %s) refers to deps entry %s; add a path_overwrite annotation with its GN output path", arg[m[2]:m[3]], label, label)
		}
		if !ok {
			var err error
			path, err = bazelFileLabelToGNPath(label)
			if err != nil {
				return "", err
			}
		}
		parts = append(parts, fmt.Sprintf("rebase_path(%s, root_build_dir)", gnQuote(path)))
		arg = arg[m[1]:]
	}
	if len(parts) == 0 {
		return `""`, nil
	}
	return strings.Join(parts, " + "), nil
}

// bazelFileLabelToGNPath converts a Bazel label of a source file to a GN path.
//
// For example:
//
//	foo.txt             -> foo.txt
//	:foo.txt            -> foo.txt
//	//path/to:foo.txt   -> //path/to/foo.txt
//	//:prebuilt/foo.txt -> //prebuilt/foo.txt
func bazelFileLabelToGNPath(label string) (string, error) {
	if strings.HasPrefix(label, "@") {
		return "", fmt.Errorf("labels in external repositories are not supported, got %q", label)
	}
	if strings.HasPrefix(label, ":") {
		return label[1:], nil
	}
	if !strings.HasPrefix(label, "//") {
		return label, nil
	}
	pkg, file, ok := strings.Cut(label[2:], ":")
	if !ok {
		return "", fmt.Errorf("expected a label of a file in the form //path/to:file, got %q", label)
	}
	if pkg == "" {
		return "//" + file, nil
	}
	return "//" + pkg + "/" + file, nil
}

// gnQuote returns s as a quoted GN string literal. `$` is kept as-is so GN
// variables in paths (e.g. `$target_gen_dir`) are expanded.
func gnQuote(s string) string {
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	return `"` + s + `"`
}

// gnQuoteLiteral returns s as a quoted GN string literal that GN does not
// expand, i.e. `$` is escaped too.
func gnQuoteLiteral(s string) string {
	quoted := gnQuote(s)
	return strings.ReplaceAll(quoted, "$", `\$`)
}
