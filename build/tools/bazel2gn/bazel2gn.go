// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"slices"
	"strings"

	"go.starlark.net/syntax"
)

// ParseBytes parses the given Starlark source content in memory. `path` is
// only used for error reporting.
func ParseBytes(path string, src []byte) (*syntax.File, error) {
	opts := syntax.FileOptions{
		// Empty means default file-level settings for parsing.
	}
	return opts.Parse(path, src, syntax.RetainComments)
}

// indent indents input lines by input levels.
func indent(lines []string, level int) []string {
	var indented []string
	prefix := strings.Repeat(indentPrefix, level)
	for _, l := range lines {
		indented = append(indented, prefix+l)
	}
	return indented
}

// unwrapParenExpr removes any surrounding parentheses from an expression.
func unwrapParenExpr(expr syntax.Expr) syntax.Expr {
	for {
		paren, ok := expr.(*syntax.ParenExpr)
		if !ok {
			return expr
		}
		expr = paren.X
	}
}

// FileToGN converts all statements in a Bazel file to GN.
func FileToGN(f *syntax.File) ([]string, error) {
	return fileToGN(f, "", false, nil)
}

// FileToGNForPackage converts all statements in a Bazel file belonging to
// `pkgPath` (relative to Fuchsia root) to GN.
func FileToGNForPackage(f *syntax.File, pkgPath string) ([]string, error) {
	return fileToGN(f, strings.Trim(pkgPath, "/"), true, nil)
}

// FileToGNWithConfig converts all statements in a Bazel file belonging to
// `pkgPath` (relative to Fuchsia root) to GN using the optional repository
// extension configuration `cfg`.
func FileToGNWithConfig(f *syntax.File, pkgPath string, cfg *Config) ([]string, error) {
	return fileToGN(f, strings.Trim(pkgPath, "/"), true, cfg)
}

func fileToGN(f *syntax.File, pkgPath string, hasPkgPath bool, cfg *Config) ([]string, error) {
	if pkgPath == "." {
		pkgPath = ""
	}
	if err := inlineRustTestCrates(f); err != nil {
		return nil, err
	}
	if err := resolveTestPackages(f); err != nil {
		return nil, err
	}
	if err := resolveComponentTargetsWithConfig(f, cfg); err != nil {
		return nil, err
	}
	if err := resolveLinuxResourceTargetsWithConfig(f, cfg); err != nil {
		return nil, err
	}
	if err := resolveResourceTargets(f); err != nil {
		return nil, err
	}
	var lines []string
	for _, stmt := range f.Stmts {
		stmtLines, err := stmtToGN(stmt, pkgPath, hasPkgPath, cfg)
		if err != nil {
			return nil, stmtError(stmt, err)
		}
		lines = append(lines, stmtLines...)
	}
	return lines, nil
}

// FileError is an error caused by the contents of a specific file, such as a
// statement in a BUILD.bazel file that can't be converted to GN.
type FileError struct {
	// Path is the path of the file. For errors from this package, it's the
	// path passed to ParseBytes.
	Path string
	// Line is the 1-based line in the file, or 0 if unknown.
	Line int
	Err  error
}

// stmtError returns a FileError for an error converting `stmt` to GN.
func stmtError(stmt syntax.Stmt, err error) *FileError {
	start, _ := stmt.Span()
	return &FileError{Path: start.Filename(), Line: int(start.Line), Err: err}
}

func (e *FileError) Error() string {
	if e.Line > 0 {
		return fmt.Sprintf("%s:%d: %v", e.Path, e.Line, e.Err)
	}
	return fmt.Sprintf("%s: %v", e.Path, e.Err)
}

func (e *FileError) Unwrap() error {
	return e.Err
}

// StmtToGN converts a Bazel statement [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#statements
func StmtToGN(stmt syntax.Stmt) ([]string, error) {
	return stmtToGN(stmt, "", false, nil)
}

func stmtToGN(stmt syntax.Stmt, pkgPath string, hasPkgPath bool, cfg *Config) ([]string, error) {
	// Skip the statement if it is mark for skipping by users.
	shouldSkip, err := hasSkipAnnotation(stmt)
	if err != nil {
		return nil, fmt.Errorf("failed to check skip annotation: %v", err)
	}
	if shouldSkip {
		return nil, nil
	}

	switch v := stmt.(type) {
	case *syntax.LoadStmt:
		if hasRawAttributeAnnotation(v) {
			return nil, fmt.Errorf("found raw_attribute annotation in unexpected location, it should be right above a target definition to take effect")
		}
		// Load statements are ignored during conversion.
		return nil, nil
	case *syntax.ExprStmt:
		if call, ok := v.X.(*syntax.CallExpr); ok {
			return callExprToGNWithConfig(call, v.Comments(), pkgPath, hasPkgPath, cfg)
		}
		if hasRawAttributeAnnotation(v) {
			return nil, fmt.Errorf("found raw_attribute annotation in unexpected location, it should be right above a target definition to take effect")
		}
		return exprToGN(v.X, nil)
	case *syntax.AssignStmt:
		if hasRawAttributeAnnotation(v) {
			return nil, fmt.Errorf("found raw_attribute annotation in unexpected location, it should be right above a target definition to take effect")
		}
		return assignStmtToGN(v)
	default:
		return nil, fmt.Errorf("statement of type %T is not supported to be converted to GN, node details: %#v", stmt, stmt)
	}
}

// exprToGN converts a Bazel expression [0] to GN.
//
// It applies input transformers first before delegating to more specific
// conversion functions based on expression type.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#expressions
func exprToGN(expr syntax.Expr, transformers []transformer) ([]string, error) {
	for _, ts := range transformers {
		var err error
		expr, err = ts(expr)
		if err != nil {
			return nil, fmt.Errorf("applying special handler before converting expression: %v", err)
		}
	}

	switch v := expr.(type) {
	case *syntax.CallExpr:
		// NOTE: I'm not sure whether we need to plumb transformers here, so far it
		// is not necessary. callExprToGN should be a top-level entry point for
		// macro and rules.
		return callExprToGN(v, v.Comments(), "", false)
	case *syntax.BinaryExpr:
		return binaryExprToGN(v, transformers)
	case *syntax.Ident:
		return identToGN(v)
	case *syntax.Literal:
		return []string{v.Raw}, nil
	case *syntax.ListExpr:
		return listExprToGN(v, transformers)
	case *syntax.DictExpr:
		return dictExprToGN(v, transformers)
	case *syntax.ParenExpr:
		return exprToGN(v.X, transformers)
	default:
		return nil, fmt.Errorf("expression of type %T is not supported when converting to GN, node details: %#v", expr, expr)
	}
}

// identToGN converts a Bazel identifier to GN.
func identToGN(ident *syntax.Ident) ([]string, error) {
	val, ok := specialIdentifiers[ident.Name]
	if !ok {
		val = ident.Name
	}
	return []string{val}, nil
}

// assignStmtToGN converts a Bazel assignment statement [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#assignments
func assignStmtToGN(stmt *syntax.AssignStmt) ([]string, error) {
	lhs, err := exprToGN(stmt.LHS, nil)
	if err != nil {
		return nil, fmt.Errorf("converting lhs of assignment statement: %v", err)
	}
	if len(lhs) == 0 {
		return nil, errors.New("lhs of assignment statement is unexpectedly empty")
	}

	transformers, err := transformersFromComments(stmt.Comments())
	if err != nil {
		return nil, err
	}

	var ret []string
	if hasBranching(stmt.RHS) {
		lc, err := listConcatWithSelectToGN(lhs[0], stmt.RHS, transformers)
		if err != nil {
			return nil, err
		}
		if stmt.Op == syntax.EQ {
			ret = append(ret, fmt.Sprintf("%s = []", lhs[0]))
		}
		ret = append(ret, lc...)
	} else {
		rhs, err := exprToGN(stmt.RHS, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting rhs of assignment statement: %v", err)
		}
		if len(rhs) == 0 {
			return nil, errors.New("rhs of assignment statement is unexpectedly empty")
		}

		ret = append(ret, fmt.Sprintf("%s %s %s", lhs[0], opToGN(stmt.Op), rhs[0]))
		ret = append(ret, rhs[1:]...)
	}

	ret = append(ret, []string{
		"",
		`# To avoid "Assignment had no effect" from GN.`,
		`# It's possible this variable is only used in if conditions (e.g. is_host).`,
		fmt.Sprintf(`not_needed([ "%s" ])`, lhs[0]),
		"",
	}...)

	return ret, nil
}

// targetCompatibleWithToGNConditions converts a Bazel `target_compatible_with`
// expression to GN conditions.
//
// It supports identifiers representing a list of constraints and lists of
// constraints containing literals and identifiers.
// It does not support concatenating multiple lists of constraints.
func targetCompatibleWithToGNConditions(expr syntax.Expr) ([]string, error) {
	switch v := expr.(type) {
	case *syntax.Ident:
		// An identifier not in a list. It must be a variable representing a list.
		gnCondition, ok := bazelConstraintListVarsToGNConditions[v.Name]
		if !ok {
			return nil, fmt.Errorf("unsupported target_compatible_with variable: %v", v.Name)
		}
		return []string{gnCondition}, nil
	case *syntax.ListExpr:
		var gnConditions []string
		for _, elm := range v.List {
			switch elm := elm.(type) {
			case *syntax.Literal:
				val, ok := elm.Value.(string)
				if !ok {
					return nil, fmt.Errorf("unexpected literal type in target_compatible_with: %T", elm.Value)
				}
				gnCondition, ok := bazelConstraintsToGNConditions[val]
				if !ok {
					return nil, fmt.Errorf("unsupported target_compatible_with variable: %v", val)
				}
				gnConditions = append(gnConditions, gnCondition)
			case *syntax.Ident:
				gnCondition, ok := bazelConstraintsToGNConditions[elm.Name]
				if !ok {
					return nil, fmt.Errorf("unsupported target_compatible_with variable: %v", elm.Name)
				}
				gnConditions = append(gnConditions, gnCondition)
			default:
				return nil, fmt.Errorf("unsupported expression type in target_compatible_with list: %T", elm)
			}
		}
		return gnConditions, nil
	default:
		return nil, fmt.Errorf("unsupported type %T as value to target_compatible_with in Bazel, node details: %#v", expr, expr)
	}
}

// callExprToGN converts a Bazel call expression [0] to GN. These calls should
// be macro or Bazel rules known to the converter.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#function-and-method-calls
func callExprToGN(expr *syntax.CallExpr, stmtComments *syntax.Comments, pkgPath string, hasPkgPath bool) ([]string, error) {
	return callExprToGNWithConfig(expr, stmtComments, pkgPath, hasPkgPath, nil)
}

func callExprToGNWithConfig(expr *syntax.CallExpr, stmtComments *syntax.Comments, pkgPath string, hasPkgPath bool, cfg *Config) ([]string, error) {
	fn, ok := expr.Fn.(*syntax.Ident)
	if !ok {
		return nil, fmt.Errorf("expected identifier for call expression, got %T", expr.Fn)
	}
	bazelRule := fn.Name
	ruleCfg, hasRuleCfg := cfg.lookupRule(bazelRule)
	start, _ := expr.Span()
	rawAttrs, err := rawAttributesFromComments(start.Line, stmtComments, expr.Comments())
	if err != nil {
		return nil, err
	}

	if bazelRule == "rustc_check_cfg_build_flags" {
		if len(rawAttrs) > 0 {
			return nil, fmt.Errorf("raw_attribute annotation is not supported on %s", bazelRule)
		}
		return rustcCheckCfgBuildFlagsToGN(expr)
	}
	if bazelRule == internalFxTestPackageRule {
		if len(rawAttrs) > 0 {
			return nil, errors.New("raw_attribute annotation is not supported on fx_package")
		}
		return fxTestPackageToGN(expr)
	}
	if bazelRule == internalFxUnittestComponentRule {
		if len(rawAttrs) > 0 {
			return nil, errors.New("raw_attribute annotation is not supported on fx_unittest_component")
		}
		return fxUnittestComponentToGN(expr)
	}
	var gnTemplateName string
	switch bazelRule {
	case internalRenamedBinaryRule:
		gnTemplateName = "renamed_binary"
	default:
		var ok bool
		gnTemplateName, ok = bazelRuleToGNTemplate[bazelRule]
		if !ok && hasRuleCfg {
			gnTemplateName, ok = ruleCfg.GNTemplate, true
		}
		if !ok {
			return nil, fmt.Errorf("%s is not a known Bazel rule to convert to GN", bazelRule)
		}
	}

	var ifsOutputName string
	switch bazelRule {
	case ccLinkStubFromIfsFileRule:
		if hasRawOutputNameAttr(rawAttrs) {
			return nil, errRawOutputNameOnCcLinkStub
		}
		var err error
		if ifsOutputName, err = ccLinkStubFromIfsFileOutputName(expr); err != nil {
			return nil, err
		}
	case "cmc_merge":
		if err := prepareCmcMerge(expr, rawAttrs); err != nil {
			return nil, err
		}
	case "cml_shard":
		if err := validateCmlShard(expr); err != nil {
			return nil, err
		}
	case "fx_component_manifest", "fx_component_manifest_with_expectations":
		if err := validateFxComponentManifest(expr); err != nil {
			return nil, err
		}
	case "fx_compiled_manifest":
		if err := prepareFxCompiledManifest(expr); err != nil {
			return nil, err
		}
	case "fx_component", internalFxTestComponentRule, internalFxTestComponentWithExpectationsRule:
		if err := validateFxComponent(expr); err != nil {
			return nil, err
		}
	case "rustc_test", "host_rustc_test":
		if err := validateRustTestOutputName(expr, rawAttrs); err != nil {
			return nil, err
		}
	case compiledActionRule:
		if err := validateCompiledAction(expr); err != nil {
			return nil, err
		}
	case "resource", "fuchsia_package_resource", "resource_group", "fuchsia_package_resource_group", "resource_tree", "fuchsia_package_resource_collection":
		if err := prepareResourceCall(expr, &gnTemplateName, pkgPath, hasPkgPath); err != nil {
			return nil, err
		}
	}
	// TODO(jayzhuang): Handle package level settings, e.g. visibility.
	if gnTemplateName == noGNEquivalent || bazelRule == "package" {
		if len(rawAttrs) > 0 {
			return nil, fmt.Errorf("raw_attribute annotation is not supported on %s", bazelRule)
		}
		return nil, nil
	}
	if bazelRule == "fx_unittest_package" {
		if len(rawAttrs) > 0 {
			return nil, fmt.Errorf("raw_attribute annotation is not supported on %s", bazelRule)
		}
		return fxUnittestPackageToGN(expr)
	}
	if bazelRule == ctfFxPackageRule {
		if len(rawAttrs) > 0 {
			return nil, fmt.Errorf("raw_attribute annotation is not supported on %s", bazelRule)
		}
		return ctfFxPackageToGN(expr)
	}
	if bazelRule == "linux_resource" {
		if len(rawAttrs) > 0 {
			return nil, fmt.Errorf("raw_attribute annotation is not supported on %s", bazelRule)
		}
		return linuxResourceToGN(expr)
	}
	if bazelRule == "icu_fuchsia_package" {
		if len(rawAttrs) > 0 {
			return nil, fmt.Errorf("raw_attribute annotation is not supported on %s", bazelRule)
		}
		return icuFuchsiaPackageToGN(expr)
	}

	attrsToOmit := attrsToOmitByRules[bazelRule]
	unsupportedAttrs := unsupportedAttrsByRules[bazelRule]

	// Loops through all arguments to handle special ones first.
	var name string
	var nameExpr syntax.Expr
	var sharedLibName syntax.Expr
	var remainingArgs []*syntax.BinaryExpr
	var wrappingConditions []string
	var hasWithUnitTests bool
	var isLinkShared bool
	var hasOutputNameAttr bool
	for _, arg := range expr.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("only attribute assignments (e.g. `attr = value`) are allowed in Bazel targets to be converted to GN, got %#v", arg)
		}

		// Skip the binary expression if it is marked for skipping.
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return nil, fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		if hasRawAttributeAnnotation(binaryExpr) {
			return nil, fmt.Errorf("found raw_attribute annotation on attribute, it should be above the target definition to take effect")
		}

		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return nil, fmt.Errorf("unexpected node type on the left hand side of binary expression in target definition, want syntax.Ident, got %T", binaryExpr.X)
		}
		if ident.Name == "with_unit_tests" || ident.Name == "with_host_unit_tests" {
			if rhsIdent, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.Ident); !ok || rhsIdent.Name != "False" {
				hasWithUnitTests = true
			}
		}
		// Validate `testonly` on `fx_cc_test_binary` before `attrsToOmit` drops
		// it so `testonly = False` (or any non-`True` expression) fails
		// conversion rather than silently generating a `testonly = true` GN
		// `cc_test_executable()`.
		if (bazelRule == "fx_cc_test_binary" || bazelRule == "vulkan_test") && ident.Name == "testonly" {
			if rhsIdent, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.Ident); !ok || rhsIdent.Name != "True" {
				return nil, fmt.Errorf("%s targets are always testonly; testonly must be True if specified", bazelRule)
			}
		}
		// `gn_output_name` on `fx_cc_binary` and `fx_cc_test_binary` is renamed to GN
		// `output_name` through `attrMapsByRules`. Only validate it here:
		// require a string literal so that `fx_packaged_binary` and
		// `linux_resource` lookups (see `indexGNBinaries`) see the same name as
		// GN, and reject a conflicting raw `output_name` annotation.
		if (bazelRule == "fx_cc_binary" || bazelRule == "fx_cc_test_binary" || bazelRule == "icu_executable" || bazelRule == "vulkan_executable" || bazelRule == "vulkan_test") && ident.Name == "gn_output_name" {
			if _, err := stringLiteralValue(binaryExpr.Y); err != nil {
				return nil, fmt.Errorf("gn_output_name of %s must be a string literal: %v", bazelRule, err)
			}
			if hasRawOutputNameAttr(rawAttrs) {
				return nil, fmt.Errorf("%s sets both gn_output_name and a `# @bazel2gn:raw_attribute:output_name` annotation; remove the annotation", bazelRule)
			}
			hasOutputNameAttr = true
		}
		if attrsToOmit[ident.Name] || (hasRuleCfg && ruleCfg.omitsAttr(ident.Name)) {
			continue
		}
		if bazelRule == "fx_cc_shared_library" && ident.Name == "shared_lib_name" {
			// Converted to `output_name` by `fxCcSharedLibraryOutputNameToGN()`.
			sharedLibName = binaryExpr.Y
			continue
		}
		if unsupportedAttrs[ident.Name] {
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of %s; use # @bazel2gn:skip and write the GN copy by hand", ident.Name, bazelRule)
		}
		if ident.Name == "name" {
			lines, err := exprToGN(binaryExpr.Y, nil)
			if err != nil {
				return nil, fmt.Errorf("converting target name: %v", err)
			}
			name = strings.Join(lines, "\n")
			nameExpr = binaryExpr.Y

			continue
		}
		if ident.Name == "target_compatible_with" {
			var err error
			wrappingConditions, err = targetCompatibleWithToGNConditions(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("converting Bazel target_compatible_with to GN conditions: %v", err)
			}
			continue
		}
		isCCLibrary := bazelRule == "cc_library" || bazelRule == "fx_cc_library"
		if isCCLibrary && ident.Name == "alwayslink" {
			// Convert `cc_library` or `fx_cc_library` with `alwayslink = True` to `source_set`.
			if rhsIdent, ok := binaryExpr.Y.(*syntax.Ident); ok && rhsIdent.Name == "True" {
				gnTemplateName = "source_set"
			}
			// Regardless of its value, `alwayslink` is not field in GN, so ignore it.
			continue
		}
		isCCBinary := bazelRule == "cc_binary" || bazelRule == "fx_cc_binary"
		if isCCBinary && ident.Name == "linkshared" {
			rhsIdent, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.Ident)
			if !ok || (rhsIdent.Name != "True" && rhsIdent.Name != "False") {
				return nil, fmt.Errorf("linkshared must be set to True or False, got %T", binaryExpr.Y)
			}
			if rhsIdent.Name == "True" {
				gnTemplateName = "loadable_module"
				isLinkShared = true
			}
			// Regardless of its value, `linkshared` is not a field in GN, so ignore it.
			continue
		}
		remainingArgs = append(remainingArgs, binaryExpr)
	}
	if name == "" {
		return nil, errors.New("missing `name` attribute in Bazel target")
	}
	if cond, ok := implicitConditionsByRules[bazelRule]; ok && !slices.Contains(wrappingConditions, cond) {
		wrappingConditions = append(wrappingConditions, cond)
	}

	// `compiled_action()` args can refer to outputs, depfile, or overwritten srcs, so collect them first.
	var compiledActionOutputs map[string]string
	if bazelRule == compiledActionRule {
		var err error
		compiledActionOutputs, err = compiledActionPaths(expr)
		if err != nil {
			return nil, fmt.Errorf("converting compiled_action paths: %v", err)
		}
	}

	ret := []string{fmt.Sprintf("%s(%s) {", gnTemplateName, name)}
	if ifsOutputName != "" {
		ret = append(ret, indent([]string{fmt.Sprintf("output_name = %s", gnQuote(ifsOutputName))}, 1)...)
	}
	if isLinkShared {
		// GN `loadable_module()` automatically appends `.so`, so strip a trailing
		// `.so` from `name` into `output_name` to avoid `<name>.so.so`.
		if baseName, hasSOSuffix := strings.CutSuffix(strings.Trim(name, `"`), ".so"); hasSOSuffix && !hasRawOutputNameAttr(rawAttrs) && !hasOutputNameAttr {
			ret = append(ret, indent([]string{fmt.Sprintf("output_name = %s", gnQuote(baseName))}, 1)...)
		}
	}
	if bazelRule == "fx_cc_shared_library" {
		lines, err := fxCcSharedLibraryOutputNameToGN(nameExpr, sharedLibName, rawAttrs)
		if err != nil {
			return nil, err
		}
		ret = append(ret, indent(lines, 1)...)
	}

	// `aspect_hints` maps to `deps` in GN, but Buildifier places `aspect_hints`
	// near the top of the target before `deps` or `implementation_deps`. If
	// another attribute on the target also maps to `deps`, move `aspect_hints`
	// (and other attributes appended to `deps`, see `isAppendedDepsAttr()`)
	// after the last such attribute and append (`+=`) to `deps`.
	remainingArgs, appendDeps, initDeps := reorderAppendedDepsWithConfig(remainingArgs, bazelRule, cfg)

	var extraExprs []string
	if extra, ok := extraGnExpressionByRules[bazelRule]; ok {
		extraExprs = append(extraExprs, extra...)
	}
	if hasRuleCfg && len(ruleCfg.ExtraGNExpressions) > 0 {
		extraExprs = append(extraExprs, ruleCfg.ExtraGNExpressions...)
	}
	if len(extraExprs) > 0 {
		// GN fails on a second assignment to the same variable, so reject raw
		// attributes or target attributes that set a variable bazel2gn already
		// sets for this rule.
		for _, e := range extraExprs {
			extraName := gnAssignedName(e)
			if rawAttrsAssign(rawAttrs, extraName) {
				return nil, fmt.Errorf("bazel2gn always sets `%s` on %s; remove the `# @bazel2gn:raw_attribute:%s` annotation", e, bazelRule, extraName)
			}
			if len(matchingArgsForGNVar(remainingArgs, bazelRule, extraName, cfg)) > 0 {
				return nil, fmt.Errorf("bazel2gn always sets `%s` on %s; remove the conflicting attribute for %q", e, bazelRule, extraName)
			}
		}
	}

	// Unlike `extraGnExpressionByRules`, a raw attribute or target attribute
	// that sets the same variable replaces or precedes the default instead of
	// failing.
	var defaultExprs []string
	if defaults, ok := defaultGnExpressionByRules[bazelRule]; ok {
		defaultExprs = append(defaultExprs, defaults...)
	}
	if hasRuleCfg && len(ruleCfg.DefaultGNExpressions) > 0 {
		defaultExprs = append(defaultExprs, ruleCfg.DefaultGNExpressions...)
	}
	var defaultExprsBefore []string
	var defaultExprsAfter []string
	for _, d := range defaultExprs {
		defaultName := gnAssignedName(d)
		if rawAttrsAssign(rawAttrs, defaultName) {
			continue
		}
		matching := matchingArgsForGNVar(remainingArgs, bazelRule, defaultName, cfg)
		if len(matching) == 0 {
			defaultExprsAfter = append(defaultExprsAfter, d)
			continue
		}
		unconditionallyInitialized := slices.ContainsFunc(matching, func(arg *syntax.BinaryExpr) bool {
			return attrUnconditionallyInitializesGNVarWithConfig(arg, bazelRule, cfg)
		})
		if !unconditionallyInitialized {
			defaultExprsBefore = append(defaultExprsBefore, d)
		}
	}

	if initDeps && !rawAttrsAssign(defaultExprsBefore, "deps") {
		ret = append(ret, indent([]string{"deps = []"}, 1)...)
	}
	if len(defaultExprsBefore) > 0 {
		ret = append(ret, indent(defaultExprsBefore, 1)...)
	}

	// Loop through all args again to actually build the content of this target.
	for _, arg := range remainingArgs {
		lines, err := attrAssignmentToGNWithConfig(arg, bazelRule, compiledActionOutputs, appendDeps, hasWithUnitTests, cfg)
		if err != nil {
			return nil, fmt.Errorf("converting Bazel attribute to GN: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	if len(extraExprs) > 0 {
		ret = append(ret, indent(extraExprs, 1)...)
	}
	if len(defaultExprsAfter) > 0 {
		ret = append(ret, indent(defaultExprsAfter, 1)...)
	}
	ret = append(ret, indent(rawAttrs, 1)...)

	ret = append(ret, "}")
	if len(wrappingConditions) > 0 {
		ret = append([]string{
			fmt.Sprintf("if (%s) {", strings.Join(wrappingConditions, " && ")),
		}, indent(ret, 1)...)
		ret = append(ret, "}")
	}
	return ret, nil
}

// validateCmlShard enforces the `cml_shard` naming convention required for
// compatibility with `bazel2gn` for now: `name` must end in `.shard`, and
// `shard` must be specified and equal to `name + ".cml"` (or `":" + name + ".cml"`).
// This allows `bazel2gn` to convert `cml_shard` target labels (e.g. in
// `expect_includes`) into `.cml` file paths without cross-target lookup.
func validateCmlShard(expr *syntax.CallExpr) error {
	var name, shard string
	for _, arg := range expr.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("only attribute assignments (e.g. `attr = value`) are allowed in cml_shard, got %#v", arg)
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("unexpected node type on the left hand side of binary expression in cml_shard, want syntax.Ident, got %T", binaryExpr.X)
		}
		switch ident.Name {
		case "name":
			lit, ok := binaryExpr.Y.(*syntax.Literal)
			if !ok {
				return fmt.Errorf("cml_shard name must be a string literal, got %T", binaryExpr.Y)
			}
			name, _ = lit.Value.(string)
		case "shard":
			lit, ok := binaryExpr.Y.(*syntax.Literal)
			if !ok {
				return fmt.Errorf("cml_shard shard must be a string literal, got %T", binaryExpr.Y)
			}
			shard, _ = lit.Value.(string)
		}
	}
	if name == "" {
		return errors.New("missing `name` attribute in cml_shard target")
	}
	if !strings.HasSuffix(name, ".shard") {
		return fmt.Errorf("cml_shard name %q must end in \".shard\" (for bazel2gn compatibility)", name)
	}
	if shard == "" {
		return fmt.Errorf("missing `shard` attribute in cml_shard target %q", name)
	}
	wantShard := name + ".cml"
	if strings.TrimPrefix(shard, ":") != wantShard {
		return fmt.Errorf("cml_shard %q must set `shard = %q` (got %q) for bazel2gn compatibility", name, wantShard, shard)
	}
	return nil
}

// convertAttrName converts input Bazel attrName to the corresponding parameter name in GN.
//
// This function takes the current Bazel rule being converted into consideration.
func convertAttrName(attrName, bazelRule string) string {
	return convertAttrNameWithConfig(attrName, bazelRule, nil)
}

func convertAttrNameWithConfig(attrName, bazelRule string, cfg *Config) string {
	if ruleCfg, ok := cfg.lookupRule(bazelRule); ok {
		if ret, ok := ruleCfg.AttrMap[attrName]; ok {
			return ret
		}
	}
	ret, ok := commonAttrMap[attrName]
	if ok {
		return ret
	}
	if ruleAttrMap, ok := attrMapsByRules[bazelRule]; ok {
		if ret, ok := ruleAttrMap[attrName]; ok {
			return ret
		}
	}
	return attrName
}

// hasClearAnnotation returns true if the input expr has a clear annotation comment.
//
// This function only picks up suffix comments, for example:
//
// ```
//
//	# This comment is NOT considered
//	copts = [] # This comment is considered
//
//	copts = [ # This comment is NOT considered
//	] # This comment is considered
//
// ```
func hasClearAnnotation(expr syntax.Expr) bool {
	comments := expr.Comments()
	if comments != nil {
		for _, c := range comments.Suffix {
			if c.Text == clearAnnotation {
				return true
			}
		}
	}
	return false
}

// disableInstrumentationToGN converts the value of a Bazel
// `disable_instrumentation` attribute to GN.
//
// `disable_instrumentation = True` becomes
// `exclude_toolchain_tags = [ "instrumented" ]`, and `False` produces nothing.
// Only literal booleans are supported.
func disableInstrumentationToGN(expr syntax.Expr) ([]string, error) {
	ident, ok := unwrapParenExpr(expr).(*syntax.Ident)
	if !ok {
		return nil, fmt.Errorf("%s must be set to True or False, got %T", disableInstrumentationAttr, expr)
	}
	switch ident.Name {
	case "True":
		return []string{disableInstrumentationGN}, nil
	case "False":
		return nil, nil
	default:
		return nil, fmt.Errorf("%s must be set to True or False, got %s", disableInstrumentationAttr, ident.Name)
	}
}

// execPropertiesToGN converts a Bazel `exec_properties` attribute to GN.
//
// Only `exec_properties = RBE_USE_LARGE_WORKER` on Rust targets is supported,
// which becomes `rustHighmemGN`. Other values are an error, because GN
// templates don't accept `exec_properties`.
func execPropertiesToGN(expr *syntax.BinaryExpr, bazelRule string) ([]string, error) {
	if _, ok := overwrittenRaw(expr); ok {
		return nil, fmt.Errorf("%q is not supported on %s; use %q and %q instead", rawOverwriteAnnotationPrefix, execPropertiesAttr, skipAnnotation, rawAttributeAnnotationPrefix)
	}
	if !rustHighmemGNTemplates[bazelRuleToGNTemplate[bazelRule]] {
		return nil, fmt.Errorf("bazel2gn only supports %s on Rust binary, library, and test rules, got %s; use %q and %q instead", execPropertiesAttr, bazelRule, skipAnnotation, rawAttributeAnnotationPrefix)
	}
	ident, ok := unwrapParenExpr(expr.Y).(*syntax.Ident)
	if !ok {
		return nil, fmt.Errorf("bazel2gn only supports `%s = %s`, got %T; use %q and %q instead", execPropertiesAttr, rbeUseLargeWorker, expr.Y, skipAnnotation, rawAttributeAnnotationPrefix)
	}
	if ident.Name != rbeUseLargeWorker {
		return nil, fmt.Errorf("bazel2gn only supports `%s = %s`, got %s; use %q and %q instead", execPropertiesAttr, rbeUseLargeWorker, ident.Name, skipAnnotation, rawAttributeAnnotationPrefix)
	}
	return slices.Clone(rustHighmemGN), nil
}

// isAppendedDepsAttr returns whether `attr` of `bazelRule` maps to GN `deps`
// and is appended (`+=`) to the `deps` set by other attributes, instead of
// being one of the attributes that set `deps`.
func isAppendedDepsAttr(attr, bazelRule string) bool {
	switch attr {
	case "aspect_hints":
		return true
	case "dynamic_deps":
		// `deps` and `dynamic_deps` of `fx_cc_shared_library()` both map to
		// GN `deps`.
		return bazelRule == "fx_cc_shared_library"
	}
	return false
}

func isAppendedDepsAttrWithConfig(attr, bazelRule string, cfg *Config) bool {
	if isAppendedDepsAttr(attr, bazelRule) {
		return true
	}
	if convertAttrNameWithConfig(attr, bazelRule, cfg) != "deps" {
		return false
	}
	if ruleCfg, ok := cfg.lookupRule(bazelRule); ok {
		op := ruleCfg.AttrAssignmentOps[attr]
		return op == "+=" || op == "-="
	}
	return false
}

// reorderAppendedDeps moves attributes appended to GN `deps` (see
// `isAppendedDepsAttr()`) to immediately follow the last other attribute that
// converts to `deps` in GN, if any such attribute exists. Returns the
// reordered slice, whether the appended attributes should use `+=`, and
// whether `deps = []` must be emitted at the start of the target because no
// preceding `deps`-mapped attribute unconditionally assigns to `deps`.
func reorderAppendedDeps(args []*syntax.BinaryExpr, bazelRule string) ([]*syntax.BinaryExpr, bool, bool) {
	return reorderAppendedDepsWithConfig(args, bazelRule, nil)
}

func reorderAppendedDepsWithConfig(args []*syntax.BinaryExpr, bazelRule string, cfg *Config) ([]*syntax.BinaryExpr, bool, bool) {
	var appended []*syntax.BinaryExpr
	firstAppendedIdx := -1
	lastOtherDepsIdx := -1
	unconditionallyInitializesDeps := false
	for i, arg := range args {
		ident := arg.X.(*syntax.Ident)
		if isAppendedDepsAttrWithConfig(ident.Name, bazelRule, cfg) {
			if firstAppendedIdx == -1 {
				firstAppendedIdx = i
			}
			appended = append(appended, arg)
		} else if convertAttrNameWithConfig(ident.Name, bazelRule, cfg) == "deps" {
			lastOtherDepsIdx = i
			if attrUnconditionallyInitializesGNVarWithConfig(arg, bazelRule, cfg) {
				unconditionallyInitializesDeps = true
			}
		}
	}
	hasExplicitAppendedOp := false
	if ruleCfg, ok := cfg.lookupRule(bazelRule); ok {
		for _, arg := range appended {
			if op := ruleCfg.AttrAssignmentOps[arg.X.(*syntax.Ident).Name]; op == "+=" || op == "-=" {
				hasExplicitAppendedOp = true
				break
			}
		}
	}
	// A single attribute mapping to `deps` can simply assign it unless a
	// configured rule explicitly gave that attribute `+=` or `-=`.
	if len(appended) == 0 || (lastOtherDepsIdx == -1 && len(appended) == 1 && !hasExplicitAppendedOp) {
		return args, false, false
	}
	initDeps := !unconditionallyInitializesDeps
	if lastOtherDepsIdx == -1 || firstAppendedIdx > lastOtherDepsIdx {
		// Either only appended attributes map to `deps`, so they all append
		// to the `deps = []` emitted first, or they already follow the other
		// attributes mapping to `deps`.
		return args, true, initDeps
	}
	reordered := make([]*syntax.BinaryExpr, 0, len(args))
	for i, arg := range args {
		if isAppendedDepsAttrWithConfig(arg.X.(*syntax.Ident).Name, bazelRule, cfg) {
			continue
		}
		reordered = append(reordered, arg)
		if i == lastOtherDepsIdx {
			reordered = append(reordered, appended...)
		}
	}
	return reordered, true, initDeps
}

// attrUnconditionallyInitializesGNVar reports whether converting `arg` to GN
// unconditionally defines its target GN variable at the outer scope of the
// target (so a subsequent `+=` will not fail with `Undefined identifier`).
func attrUnconditionallyInitializesGNVar(arg *syntax.BinaryExpr) bool {
	return attrUnconditionallyInitializesGNVarWithConfig(arg, "", nil)
}

func attrUnconditionallyInitializesGNVarWithConfig(arg *syntax.BinaryExpr, bazelRule string, cfg *Config) bool {
	lhs := arg.X.(*syntax.Ident)
	if idkFuchsiaSpecificAttrs[lhs.Name] || idkNonFuchsiaSpecificAttrs[lhs.Name] {
		return false
	}
	if ruleCfg, ok := cfg.lookupRule(bazelRule); ok {
		if op, ok := ruleCfg.AttrAssignmentOps[lhs.Name]; ok {
			if op != "=" {
				return false
			}
			// Explicit "=" in rule config overrides default bazelAttrToGNAssignmentOps.
		} else if op, ok := bazelAttrToGNAssignmentOps[lhs.Name]; ok && op != "=" {
			return false
		}
	} else if op, ok := bazelAttrToGNAssignmentOps[lhs.Name]; ok && op != "=" {
		return false
	}
	if _, ok := overwrittenRaw(arg); ok {
		return true
	}
	rhsY := unwrapParenExpr(arg.Y)
	if isSelectCall(rhsY) {
		return selectHasDefault(rhsY.(*syntax.CallExpr))
	}
	if _, ok := rhsY.(*syntax.CondExpr); ok {
		return true
	}
	// Both list concatenation with branching (which emits `<var> = []` first when
	// op == "=") and plain non-branching expressions unconditionally assign `<var> = ...`.
	return true
}

// selectHasDefault reports whether a `select({...})` call contains a
// `"//conditions:default"` branch.
func selectHasDefault(expr *syntax.CallExpr) bool {
	if len(expr.Args) == 0 {
		return false
	}
	dict, ok := expr.Args[0].(*syntax.DictExpr)
	if !ok {
		return false
	}
	for _, entry := range dict.List {
		e, ok := entry.(*syntax.DictEntry)
		if !ok {
			continue
		}
		key, ok := e.Key.(*syntax.Literal)
		if !ok || len(key.Raw) < 2 {
			continue
		}
		if key.Raw[1:len(key.Raw)-1] == defaultBazelSelectCondition {
			return true
		}
	}
	return false
}

// attrAssignmentToGN converts a Bazel assignment [0] to GN. These assignments
// are used to assign values to fields during target definitions in Bazel.
//
// This function handles the special cases in attribute assignments, e.g. when
// select calls are involved. This is done through applying node transformers
// funcs during the traversal.
//
// NOTE: Assignment is a special binary expression with operator =.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#assignments
func attrAssignmentToGN(expr *syntax.BinaryExpr, bazelRule string, compiledActionOutputs map[string]string, appendDeps bool, hasWithUnitTests bool) ([]string, error) {
	return attrAssignmentToGNWithConfig(expr, bazelRule, compiledActionOutputs, appendDeps, hasWithUnitTests, nil)
}

func attrAssignmentToGNWithConfig(expr *syntax.BinaryExpr, bazelRule string, compiledActionOutputs map[string]string, appendDeps bool, hasWithUnitTests bool, cfg *Config) ([]string, error) {
	lhs, ok := expr.X.(*syntax.Ident)
	if !ok {
		return nil, fmt.Errorf("expecting an identifier on the left hand side of attribute assignment, got %T", expr.X)
	}
	attrName := convertAttrNameWithConfig(lhs.Name, bazelRule, cfg)
	ruleCfg, hasRuleCfg := cfg.lookupRule(bazelRule)

	// `exec_properties` has no GN equivalent attribute and must never be
	// emitted verbatim, even with an overwrite annotation.
	if lhs.Name == execPropertiesAttr {
		return execPropertiesToGN(expr, bazelRule)
	}

	// First, handle overwrite annotations for the entire right hand side.
	//
	// This requires determining which assignment operator to use now even though
	// we may not end up using it.
	var op string
	opFound := false
	if hasRuleCfg {
		op, opFound = ruleCfg.AttrAssignmentOps[lhs.Name]
	}
	if !opFound {
		op, opFound = bazelAttrToGNAssignmentOps[lhs.Name]
	}
	if !opFound {
		op = "="
	}

	if idkFuchsiaSpecificAttrs[lhs.Name] || idkNonFuchsiaSpecificAttrs[lhs.Name] {
		op = "+="
	}
	if !opFound && appendDeps && isAppendedDepsAttrWithConfig(lhs.Name, bazelRule, cfg) {
		op = "+="
	}

	// By default "configs" uses += to concatenate values set in Bazel with =.
	// Check for clear annotation to explicitly clear configs in GN, which
	// requires using =.
	//
	// TODO(https://fxbug.dev/430953918): Figure out a better way to handle configs and public_configs conversion.
	if attrName == "configs" && hasClearAnnotation(expr) {
		op = "="
	}

	// Now handle the overwrite annotation. The right hand side will be exactly what is in the
	// annotation. No transformations, special handling of select() or conditional expressions,
	// or wrapping (implemented below) will be performed.
	if raw, ok := overwrittenRaw(expr); ok {
		return []string{fmt.Sprintf("%s %s %s", attrName, op, raw)}, nil
	}

	// `disable_instrumentation = True` has no direct GN equivalent attribute, it
	// becomes an `exclude_toolchain_tags` assignment.
	if lhs.Name == disableInstrumentationAttr {
		return disableInstrumentationToGN(expr.Y)
	}

	// Intercept genrule cmd assignment and convert it directly.
	//
	// NOTE: This means bazel2gn does NOT support select calls in `cmd` currently.
	if bazelRule == "genrule" && attrName == "cmd" {
		return genruleCmdToGN(expr.Y)
	}

	// Intercept tags attribute to sync assert_no_deps and
	// exclude_toolchain_tags.
	if attrName == "tags" {
		return tagsToGN(expr.Y, bazelRule, hasWithUnitTests)
	}

	// Intercept rustenv attribute to convert dict to list of strings.
	if attrName == "rustenv" {
		return rustenvToGN(expr.Y)
	}

	transformers, err := transformersFromComments(expr.Comments())
	if err != nil {
		return nil, err
	}

	matchedRuleCfgTransformer := false
	if hasRuleCfg {
		if tName, ok := ruleCfg.AttrTransformers[lhs.Name]; ok {
			transformers = append(transformers, transformerAnnotationNames[tName])
			matchedRuleCfgTransformer = true
		} else if tName, ok := ruleCfg.AttrTransformers[attrName]; ok {
			transformers = append(transformers, transformerAnnotationNames[tName])
			matchedRuleCfgTransformer = true
		}
	}

	if !matchedRuleCfgTransformer {
		switch attrName {
		case "visibility":
			transformers = append(transformers, bazelVisibilityToGN)
		case "deps", "data_deps", "test_deps", "proc_macro_deps", "link_deps", "args_deps", "plugin_deps", "fidls", "fidl_target", "non_rust_deps", "sdk_deps", "non_sdk_deps", "validations", "cm_label", "source_deps", "icu_deps", "icu_public_deps", "subpackages", "public_configs":
			transformers = append(transformers, bazelDepToGN)
		case "files":
			if bazelRule == "resource_group" || bazelRule == "fuchsia_package_resource_group" {
				transformers = append(transformers, bazelResourceGroupFilesToGN)
			}
		case "public_deps":
			transformers = append(transformers, bazelDepToGN)
			if bazelRule == "alias" {
				// `alias` rules map from a single path in bazel to a list in GN
				transformers = append(transformers, bazelExprToGNList)
			}
		case "configs":
			if _, ok := buildFlagsAttrMap[lhs.Name]; ok {
				// `build_flags()` labels are the same as their GN `config()` labels.
				transformers = append(transformers, bazelDepToGN)
			} else {
				transformers = append(transformers, bazelCOptToGNConfig)
			}
		case "api", "outputs", "sources", "inputs", "args_sources", "args_source_root":
			if bazelRule == compiledActionRule && attrName == "outputs" {
				transformers = append(transformers, compiledActionOutsTransformer)
			} else if bazelRule == "cmc_merge" && attrName == "sources" {
				transformers = append(transformers, bazelCmcMergeSrcsToGN)
			} else {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "includes":
			switch bazelRule {
			case "expect_includes":
				transformers = append(transformers, bazelCmlIncludesToGN)
			case "fx_generated_expectations":
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "manifest":
			if isFxManifestRule(bazelRule) || bazelRule == "icu_fuchsia_component" {
				transformers = append(transformers, bazelManifestToGN(bazelRule))
			}
		case "depfile":
			if bazelRule == compiledActionRule {
				transformers = append(transformers, compiledActionScalarOutTransformer(expr))
			}
		case "expectations":
			switch bazelRule {
			case "fx_expectations":
				transformers = append(transformers, bazelFilePathsToGN)
			case internalFxTestComponentWithExpectationsRule:
				transformers = append(transformers, bazelExpectationsToGN)
			}
		case "args":
			if bazelRule == compiledActionRule {
				transformers = append(transformers, compiledActionArgsTransformer(compiledActionOutputs))
			}
		case "tool":
			if bazelRule == compiledActionRule {
				transformers = append(transformers, compiledActionToolTransformer(expr))
			}
		case "tool_output_name", "mnemonic":
			if bazelRule == compiledActionRule {
				transformers = append(transformers, compiledActionStringAttrTransformer(attrName))
			}
		case "config_data":
			if bazelRule == "ffx_plugin" {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "abi":
			// `cc_link_stub_from_ifs_file()` takes an `.ifs` file label in Bazel,
			// and `ifs_shared_library()` a file path in GN.
			if bazelRule == ccLinkStubFromIfsFileRule {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "checked_in_source":
			// `rustc_bindgen_golden()` takes a file label in Bazel and a file path
			// in GN. Its `headers` are source paths in both.
			if bazelRule == "rustc_bindgen_golden" {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "source":
			// `bind_library()` `src` (mapped to GN `source`) and `intl_strings()` take a file label in Bazel and a file path in GN.
			if bazelRule == "bind_library" || bazelRule == "intl_strings" {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "amendments_path", "template_path":
			// `fostr_fidl()` takes file labels in Bazel and file paths in GN.
			if bazelRule == "fostr_fidl" {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "parcelables", "interfaces":
			// AIDL rules take `.aidl` file labels in Bazel and file paths in GN.
			if bazelRule == "aidl" || bazelRule == "aidl_cpp" || bazelRule == "aidl_ndk" || bazelRule == "aidl_rust" {
				transformers = append(transformers, bazelFilePathsToGN)
			}
		case "ldflags":
			transformers = append(transformers, bazelLdflagsToGN)
		}
	}

	rhsY := unwrapParenExpr(expr.Y)

	// This is a simple `attr = select(...)`, convert in-place.
	if isSelectCall(rhsY) {
		return selectToGN(attrName, op, rhsY.(*syntax.CallExpr), transformers)
	}

	if condExpr, ok := rhsY.(*syntax.CondExpr); ok {
		return condExprToGN(attrName, op, condExpr, transformers)
	}

	// It is not a simple `select` call on the RHS, and `select`s are found in
	// subtree, so assume this is list concatenation with `select`s in them.
	//
	// NOTE: Currently `select`s and `if`s are only supported in list concatenation when
	// they are used in binary expressions. Other usages will fail this call.
	if hasBranching(rhsY) {
		lc, err := listConcatWithSelectToGN(attrName, rhsY, transformers)
		if err != nil {
			return nil, err
		}
		if op == "=" {
			// Start with an empty list so it's easy to += new elements from later
			// conversions.
			return append([]string{fmt.Sprintf("%s = []", attrName)}, lc...), nil
		}
		return lc, nil
	}

	rhs, err := exprToGN(expr.Y, transformers)
	if err != nil {
		return nil, fmt.Errorf("converting rhs of binary expression: %v", err)
	}
	if len(rhs) == 0 {
		return nil, errors.New("rhs hand side of binary expression is unexpectedly empty")
	}
	if attrName == "visibility" {
		if _, ok := rhsY.(*syntax.ListExpr); ok {
			rhs = ensureSamePackageVisibility(rhs)
		}
	}

	ret := []string{fmt.Sprintf("%s %s %s", attrName, op, rhs[0])}
	ret = append(ret, rhs[1:]...)

	// Handle any additional work necessary for specific assignments.
	if attrName == "sdk_headers_for_internal_use" {
		ret = handle_sdk_headers_for_internal_use(ret, rhs, 0)
	}

	// Wrap the entire assignment in a condition if appropriate.
	// This should be the last thing before returning the result.
	switch {
	case idkFuchsiaSpecificAttrs[lhs.Name]:
		ret = append([]string{"if (is_fuchsia) {"}, indent(ret, 1)...)
		ret = append(ret, "}")
	case idkNonFuchsiaSpecificAttrs[lhs.Name]:
		ret = append([]string{"if (!is_fuchsia) {"}, indent(ret, 1)...)
		ret = append(ret, "}")
	}

	return ret, nil
}

// binaryExprToGN converts a general Bazel binary expression [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#binary-operators
func binaryExprToGN(expr *syntax.BinaryExpr, transformers []transformer) ([]string, error) {
	lhs, err := exprToGN(expr.X, transformers)
	if err != nil {
		return nil, fmt.Errorf("converting lhs of binary expression: %v", err)
	}
	if len(lhs) == 0 {
		return nil, errors.New("lhs of binary expression is unexpectedly empty")
	}

	rhs, err := exprToGN(expr.Y, transformers)
	if err != nil {
		return nil, fmt.Errorf("converting rhs of binary expression: %v", err)
	}
	if len(rhs) == 0 {
		return nil, errors.New("rhs hand side of binary expression is unexpectedly empty")
	}

	ret := lhs[:len(lhs)-1]
	ret = append(ret, fmt.Sprintf("%s %s %s", lhs[len(lhs)-1], opToGN(expr.Op), rhs[0]))
	ret = append(ret, rhs[1:]...)
	return ret, nil
}

func opToGN(op syntax.Token) string {
	ret, ok := specialTokens[op]
	if !ok {
		return op.String()
	}
	return ret
}

// samePackageGNVisibilities are GN visibility entries that already include targets in the same
// BUILD.gn file.
var samePackageGNVisibilities = map[string]bool{
	`"*"`:   true,
	`"//*"`: true,
	`":*"`:  true,
	`"./*"`: true,
}

// ensureSamePackageVisibility adds `":*"` to a converted GN visibility list unless it already
// covers the current package.
//
// In Bazel, targets are always visible to other targets in the same package, so `":__pkg__"` is
// redundant in BUILD.bazel. GN has no such implicit rule: a restricted visibility list also applies
// to targets in the same BUILD.gn, e.g. the test targets generated by `with_unit_tests` or
// hand-written targets above the bazel2gn sentinel. Adding `":*"` keeps GN consistent with Bazel.
//
// lines is the output of listExprToGN: "[", one line per element, then "]".
func ensureSamePackageVisibility(lines []string) []string {
	if len(lines) < 2 || lines[0] != "[" || lines[len(lines)-1] != "]" {
		return lines
	}
	for _, l := range lines[1 : len(lines)-1] {
		if samePackageGNVisibilities[strings.TrimSuffix(strings.TrimSpace(l), ",")] {
			return lines
		}
	}
	ret := make([]string, 0, len(lines)+1)
	ret = append(ret, lines[0])
	ret = append(ret, indent([]string{`":*",`}, 1)...)
	return append(ret, lines[1:]...)
}

// listExprToGN converts a Bazel list expression [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#lists
func listExprToGN(expr *syntax.ListExpr, transformers []transformer) ([]string, error) {
	ret := []string{"["}

	for _, elm := range expr.List {
		shouldSkip, err := hasSkipAnnotation(elm)
		if err != nil {
			return nil, fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}

		elmLines, err := exprToGN(elm, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting list element: %v", err)
		}
		if len(elmLines) == 0 {
			continue
		}
		elmLines[len(elmLines)-1] = elmLines[len(elmLines)-1] + ","
		ret = append(ret, indent(elmLines, 1)...)
	}

	ret = append(ret, "]")
	return ret, nil
}

// dictExprToGN converts a Bazel dictionary expression to a GN scope.
func dictExprToGN(expr *syntax.DictExpr, transformers []transformer) ([]string, error) {
	ret := []string{"{"}

	for _, entry := range expr.List {
		entryDictEntry, ok := entry.(*syntax.DictEntry)
		if !ok {
			return nil, fmt.Errorf("unexpected node type in dictionary entry: %T", entry)
		}

		key, err := exprToGN(entryDictEntry.Key, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting dictionary key: %v", err)
		}

		value, err := exprToGN(entryDictEntry.Value, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting dictionary value: %v", err)
		}

		// In GN, keys are identifiers, so they should not be quoted.
		// Starlark dictionary keys are strings, so they are quoted.
		// We need to remove the quotes from the key.
		key[0] = strings.Trim(key[0], `"`)

		lines := []string{fmt.Sprintf("%s = %s", key[0], value[0])}
		if len(value) > 1 {
			lines = append(lines, value[1:]...)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	ret = append(ret, "}")
	return ret, nil
}

// hasSkipAnnotation returns true if the given node has a skip annotation.
//
// A skip annotation is a comment that exactly matches `@bazel2gn:skip`, and
// right above or after the node on the same line. The node will be skipped
// during conversion if the annotation is found.
//
// For example, these are skip annotations:
//
// ```
//
//	# Some other comment
//	# @bazel2gn:skip
//	go_library(
//	  ...
//	  # @bazel2gn:skip
//	  attr = val,  # @bazel2gn:skip
//	)
//
// ```
//
// And these are NOT skip annotations:
//
// ```
//
//	# @bazel2gn:skip
//	# Skip annotation should be on the same line or right above the node.
//	go_library(
//		...
//	)
//	# @bazel2gn:skip
//
// ```
func hasSkipAnnotation(node syntax.Node) (bool, error) {
	comments := node.Comments()
	if comments == nil {
		return false, nil
	}
	if len(comments.Before) > 0 {
		// For comments before, match the last line.
		if comments.Before[len(comments.Before)-1].Text == skipAnnotation {
			return true, nil
		}
	}
	if len(comments.Suffix) > 0 {
		// For suffixes, match the comment immediately following.
		if comments.Suffix[0].Text == skipAnnotation {
			return true, nil
		}
	}
	allComments := [][]syntax.Comment{
		comments.Before,
		comments.After,
	}
	for _, c := range allComments {
		if hasAnnotation(c, skipAnnotation) {
			return false, fmt.Errorf("found skip annotation in unexpected location, it should be on the line right above the target definition to take effect")
		}
	}
	return false, nil
}

func hasAnnotation(comments []syntax.Comment, annotation string) bool {
	for _, comment := range comments {
		if comment.Text == annotation {
			return true
		}
	}
	return false
}

// transformersFromAnnotations returns the list of transformers to apply to a
// target definition based on the annotations found in the comments.
func transformersFromAnnotations(comments []syntax.Comment) ([]transformer, error) {
	transformers := []transformer{}
	for _, comment := range comments {
		if strings.HasPrefix(comment.Text, transformerAnnotationPrefix) {
			n := strings.TrimPrefix(comment.Text, transformerAnnotationPrefix)
			transformer, ok := transformerAnnotationNames[n]
			if !ok {
				return nil, fmt.Errorf("unknown transformer: %s", n)
			}
			transformers = append(transformers, transformer)
		}
	}
	return transformers, nil
}

func transformersFromComments(comments *syntax.Comments) ([]transformer, error) {
	if comments == nil {
		return nil, nil
	}
	before, err := transformersFromAnnotations(comments.Before)
	if err != nil {
		return nil, err
	}
	suffix, err := transformersFromAnnotations(comments.Suffix)
	if err != nil {
		return nil, err
	}
	return append(before, suffix...), nil
}

// gnAssignedName returns the variable that a GN assignment such as
// `enable_mypy = false` replaces. It returns "" when `=` is absent or part of
// `==`, or for `+=` and `-=`, which modify a variable's existing value instead
// of replacing it: a raw `x += ...` neither conflicts with nor replaces a
// generated `x = ...`, and needs it to come first.
func gnAssignedName(assignment string) string {
	name, after, found := strings.Cut(assignment, "=")
	if !found || strings.HasPrefix(after, "=") {
		return ""
	}
	name = strings.TrimSpace(name)
	if strings.HasSuffix(name, "+") || strings.HasSuffix(name, "-") {
		return ""
	}
	return name
}

// matchingArgsForGNVar returns the subset of `args` whose converted GN
// variable name equals `gnVar`.
func matchingArgsForGNVar(args []*syntax.BinaryExpr, bazelRule, gnVar string, cfg *Config) []*syntax.BinaryExpr {
	if gnVar == "" {
		return nil
	}
	var matching []*syntax.BinaryExpr
	for _, arg := range args {
		ident := arg.X.(*syntax.Ident)
		if ident.Name == "tags" || ident.Name == execPropertiesAttr || ident.Name == disableInstrumentationAttr {
			continue
		}
		if convertAttrNameWithConfig(ident.Name, bazelRule, cfg) == gnVar {
			matching = append(matching, arg)
		}
	}
	return matching
}

// rawAttrsAssign reports whether any of `rawAttrs` assigns the GN variable
// `name`. It's always false for an empty `name` (from a `+=` or `-=`).
func rawAttrsAssign(rawAttrs []string, name string) bool {
	return name != "" && slices.ContainsFunc(rawAttrs, func(raw string) bool {
		return gnAssignedName(raw) == name
	})
}

// rawAttributesFromComments extracts raw GN lines from `@bazel2gn:raw_attribute:`
// comment annotations placed before a target definition.
func rawAttributesFromComments(targetStartLine int32, commentsList ...*syntax.Comments) ([]string, error) {
	var rawAttrs []string
	seen := make(map[*syntax.Comments]bool)
	for _, comments := range commentsList {
		if comments == nil || seen[comments] {
			continue
		}
		seen[comments] = true
		seenRawAttr := false
		for i, c := range comments.Before {
			if after, ok := strings.CutPrefix(c.Text, rawAttributeAnnotationPrefix); ok {
				expectedLine := targetStartLine - int32(len(comments.Before)-i)
				if c.Start.Line != expectedLine {
					return nil, fmt.Errorf("found raw_attribute annotation in unexpected location, it should be right above the target definition to take effect")
				}
				seenRawAttr = true
				trimmed := strings.TrimSpace(after)
				if trimmed == "" {
					return nil, fmt.Errorf("empty %q annotation", rawAttributeAnnotationPrefix)
				}
				rawAttrs = append(rawAttrs, trimmed)
			} else if seenRawAttr {
				return nil, fmt.Errorf("found raw_attribute annotation in unexpected location, it should be right above the target definition to take effect")
			}
		}
		for _, c := range append(comments.Suffix, comments.After...) {
			if strings.HasPrefix(c.Text, rawAttributeAnnotationPrefix) {
				return nil, fmt.Errorf("found raw_attribute annotation in unexpected location, it should be right above the target definition to take effect")
			}
		}
	}
	return rawAttrs, nil
}

// hasRawAttributeAnnotation reports whether any comment in the AST subtree
// rooted at `node` starts with `rawAttributeAnnotationPrefix`.
func hasRawAttributeAnnotation(node syntax.Node) bool {
	if node == nil {
		return false
	}
	var found bool
	syntax.Walk(node, func(n syntax.Node) bool {
		if n == nil {
			return true
		}
		comments := n.Comments()
		if comments == nil {
			return true
		}
		for _, list := range [][]syntax.Comment{comments.Before, comments.Suffix, comments.After} {
			for _, c := range list {
				if strings.HasPrefix(c.Text, rawAttributeAnnotationPrefix) {
					found = true
					return false
				}
			}
		}
		return true
	})
	return found
}
