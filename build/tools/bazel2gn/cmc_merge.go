// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"strings"

	"go.starlark.net/syntax"
)

// generatedDepsAttr is an internal keyword argument injected onto `cmc_merge()`,
// `fx_component_manifest()`, `fx_component_manifest_with_expectations()`, and
// `fx_compiled_manifest()` when they consume a `cmc_merge()` target in the same
// `BUILD.bazel` file.
const generatedDepsAttr = "$deps"

var allowedCmcMergeAttrs = map[string]bool{
	"name":                   true,
	"srcs":                   true,
	"output_name":            true,
	"testonly":               true,
	"visibility":             true,
	"tags":                   true,
	"target_compatible_with": true,
	generatedDepsAttr:        true,
}

// LINT.IfChange(cmc_merge_output_name)
func defaultCmcMergeOutputName(name string) string {
	return name + ".cml"
}

// LINT.ThenChange(//build/bazel_sdk/fuchsia_rules_common/components/component_manifest.bzl:cmc_merge_output_name)

func ruleCall(stmt syntax.Stmt) (*syntax.CallExpr, string) {
	if call, ok := callOfStmt(stmt); ok {
		if fn, ok := call.Fn.(*syntax.Ident); ok {
			return call, fn.Name
		}
	}
	return nil, ""
}

// prepareCmcMerge validates a Bazel `cmc_merge()` call and injects
// `output_name = "<name>.cml"` when `output_name` is omitted, matching the
// Bazel rule's default (GN `cmc_merge()` defaults to `target_name`).
func prepareCmcMerge(call *syntax.CallExpr, rawAttrs []string) error {
	var name, outputName string
	hasSrcs := false
	newArgs := make([]syntax.Expr, 0, len(call.Args)+1)
	insertIdx := -1

	for _, arg := range call.Args {
		kv, ok := arg.(*syntax.BinaryExpr)
		if !ok || kv.Op != syntax.EQ {
			return fmt.Errorf("cmc_merge only accepts keyword arguments, got %T", arg)
		}
		if skip, err := hasSkipAnnotation(kv); err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		} else if skip {
			newArgs = append(newArgs, arg)
			continue
		}
		ident, ok := kv.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", kv.X)
		}
		if !allowedCmcMergeAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of cmc_merge; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
		var err error
		switch ident.Name {
		case "name":
			if name, err = stringLiteralValue(kv.Y); err != nil {
				return fmt.Errorf("cmc_merge name: %v", err)
			}
		case "srcs":
			hasSrcs = true
			if isEmptyListExpr(kv.Y) {
				return errors.New("`srcs` must not be empty in cmc_merge")
			}
			newArgs = append(newArgs, arg)
			insertIdx = len(newArgs)
			continue
		case "output_name":
			if outputName, err = stringLiteralValue(kv.Y); err != nil {
				return fmt.Errorf("cmc_merge output_name: %v", err)
			}
			if outputName == "" {
				continue
			}
		}
		newArgs = append(newArgs, arg)
	}

	if name == "" {
		return errors.New("missing `name` attribute in cmc_merge")
	}
	if !hasSrcs {
		return fmt.Errorf("missing `srcs` attribute in cmc_merge %q", name)
	}
	if outputName == "" && !hasRawOutputNameAttr(rawAttrs) {
		outArg := attrExpr("output_name", stringLiteral(defaultCmcMergeOutputName(name)))
		call.Args = append(newArgs[:insertIdx:insertIdx], append([]syntax.Expr{outArg}, newArgs[insertIdx:]...)...)
		return nil
	}
	call.Args = newArgs
	return nil
}

// resolveCmcMergeTargets rewrites same-file `:cmc_merge` references in
// `fx_component_manifest(manifest = ...)`,
// `fx_component_manifest_with_expectations(manifest = ...)`,
// `fx_compiled_manifest(manifest = ...)`, and `cmc_merge(srcs = ...)` into
// `"$target_out_dir/<output_name>"` and adds GN `deps`.
func resolveCmcMergeTargets(f *syntax.File) error {
	type mergeInfo struct {
		outputName string
		skipped    bool
	}
	merges := map[string]mergeInfo{}
	for _, stmt := range f.Stmts {
		call, rule := ruleCall(stmt)
		if rule != "cmc_merge" {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		name, ok := callStringAttr(call, "name")
		if !ok || name == "" {
			continue
		}
		outName := defaultCmcMergeOutputName(name)
		if rawOut, ok := rawOutputNameFromStmt(stmt, call); ok {
			outName = rawOut
		}
		if explicit, ok := unskippedCallStringAttr(call, "output_name"); ok && explicit != "" {
			outName = explicit
		}
		merges[name] = mergeInfo{outputName: outName, skipped: skip}
	}
	if len(merges) == 0 {
		return nil
	}

	rewriteLit := func(lit *syntax.Literal, rule, consumer string) (string, error) {
		if _, ok := overwrittenPath(lit); ok {
			return "", nil
		}
		s, ok := lit.Value.(string)
		if !ok {
			return "", nil
		}
		target, ok := strings.CutPrefix(s, ":")
		if !ok {
			return "", nil
		}
		info, isMerge := merges[target]
		if !isMerge {
			return "", nil
		}
		if rule == "cmc_merge" && target == consumer {
			return "", fmt.Errorf("cmc_merge %q cannot reference itself in `srcs`", consumer)
		}
		if info.skipped {
			return "", fmt.Errorf("%s %q references skipped cmc_merge target %q", rule, consumer, s)
		}
		outPath := "$target_out_dir/" + info.outputName
		lit.Value = outPath
		lit.Raw = gnQuote(outPath)
		return ":" + target, nil
	}

	for _, stmt := range f.Stmts {
		call, rule := ruleCall(stmt)
		var targetAttr string
		switch {
		case rule == "cmc_merge":
			targetAttr = "srcs"
		case isFxManifestRule(rule):
			targetAttr = "manifest"
		default:
			continue
		}
		if skip, err := hasSkipAnnotation(stmt); err != nil {
			return err
		} else if skip {
			continue
		}
		consumer, _ := callStringAttr(call, "name")
		for i, arg := range call.Args {
			if skip, err := hasSkipAnnotation(arg); err != nil || skip {
				continue
			}
			if attr, ok := attrName(arg); !ok || attr != targetAttr {
				continue
			}
			kv := arg.(*syntax.BinaryExpr)
			if _, hasRaw := overwrittenRaw(kv); hasRaw {
				break
			}
			var deps []syntax.Expr
			seen := map[string]bool{}
			addDep := func(d string) {
				if d != "" && !seen[d] {
					seen[d] = true
					deps = append(deps, stringLiteral(d))
				}
			}
			rhs := unwrapParenExpr(kv.Y)
			switch v := rhs.(type) {
			case *syntax.Literal:
				dep, err := rewriteLit(v, rule, consumer)
				if err != nil {
					return stmtError(stmt, err)
				}
				addDep(dep)
			case *syntax.ListExpr:
				for _, elem := range v.List {
					if skip, err := hasSkipAnnotation(elem); err != nil || skip {
						continue
					}
					if lit, ok := unwrapParenExpr(elem).(*syntax.Literal); ok {
						dep, err := rewriteLit(lit, rule, consumer)
						if err != nil {
							return stmtError(stmt, err)
						}
						addDep(dep)
					}
				}
			}
			var nestedErr error
			syntax.Walk(rhs, func(n syntax.Node) bool {
				if n == nil {
					return false
				}
				if skip, err := hasSkipAnnotation(n); err == nil && skip {
					return false
				}
				if lit, ok := n.(*syntax.Literal); ok {
					if _, hasOverwrite := overwrittenPath(lit); !hasOverwrite {
						if s, ok := lit.Value.(string); ok {
							if target, ok := strings.CutPrefix(s, ":"); ok {
								if _, isMerge := merges[target]; isMerge {
									nestedErr = fmt.Errorf("bazel2gn only supports direct literal cmc_merge references in %s `%s`, got %q in complex expression; use # @bazel2gn:skip", rule, targetAttr, s)
									return false
								}
							}
						}
					}
				}
				return nestedErr == nil
			})
			if nestedErr != nil {
				return stmtError(stmt, nestedErr)
			}
			if len(deps) > 0 {
				depArg := attrExpr(generatedDepsAttr, &syntax.ListExpr{List: deps})
				call.Args = append(call.Args[:i+1:i+1], append([]syntax.Expr{depArg}, call.Args[i+1:]...)...)
			}
			break
		}
	}
	return nil
}
