// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"strconv"

	"go.starlark.net/syntax"
)

// ctfFxPackageRule is the Bazel macro for a Fuchsia package that is released
// in CTF, see //sdk/ctf/build/ctf_fx_package.bzl.
const ctfFxPackageRule = "ctf_fx_package"

// ctfFxPackageToGN converts a Bazel `ctf_fx_package()` macro call to a GN
// `ctf_fuchsia_package()`, which also defines the `<name>_archive` target
// that GN `ctf-tests` groups depend on.
//
// Unlike the GN copies of test packages (see `gnTestPackageNameSuffix`), the
// GN copy keeps the package name. CTF releases, the templates in
// //sdk/ctf/build/generate_ctf_tests.gni, and the manifests of the tests that
// subpackage a CTF package all identify it by name. Only the GN copy is
// published or released until the CTF release is built with Bazel.
//
// Only the attributes that have a direct GN equivalent are supported. Other
// attributes are errors, so packages using them need `# @bazel2gn:skip` and a
// hand-written GN copy.
func ctfFxPackageToGN(call *syntax.CallExpr) ([]string, error) {
	var name, packageName string
	var components, testComponents, subpackages, testComponentNames, visArg *syntax.BinaryExpr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("%s only accepts keyword arguments, got %T", ctfFxPackageRule, arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return nil, fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return nil, fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}

		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("%s name: %v", ctfFxPackageRule, err)
			}
		case "package_name":
			packageName, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("%s package_name: %v", ctfFxPackageRule, err)
			}
		case "components":
			components = binaryExpr
		case "test_components":
			testComponents = binaryExpr
		case "subpackages":
			subpackages = binaryExpr
		case "test_component_names":
			testComponentNames = binaryExpr
		case "visibility":
			visArg = binaryExpr
		case "tags", "testonly":
			// CTF packages are always testonly, and Bazel tags have no GN
			// meaning for packages.
		default:
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of %s; use # @bazel2gn:skip and write the GN copy by hand", ident.Name, ctfFxPackageRule)
		}
	}

	if name == "" {
		return nil, fmt.Errorf("missing `name` attribute in %s", ctfFxPackageRule)
	}
	if packageName == "" {
		// Like `fx_package()`, the package name defaults to `name`. GN
		// `ctf_fuchsia_package()` requires `package_name`.
		packageName = name
	}

	body := []string{
		"testonly = true",
		fmt.Sprintf("package_name = %s", strconv.Quote(packageName)),
	}
	hasDeps := false
	for _, attr := range []struct {
		gnName       string
		arg          *syntax.BinaryExpr
		transformers []transformer
	}{
		// GN `ctf_fuchsia_package()` takes components and test components in
		// `deps`. The test components are converted to GN by
		// `resolveTestPackages()`.
		{"deps", components, []transformer{bazelDepToGN}},
		{"deps", testComponents, []transformer{bazelDepToGN}},
		{"subpackages", subpackages, []transformer{bazelDepToGN}},
		{"test_component_names", testComponentNames, nil},
	} {
		if attr.arg == nil {
			continue
		}
		lines, err := exprToGN(attr.arg.Y, attr.transformers)
		if err != nil {
			return nil, fmt.Errorf("converting %s %q %s: %v", ctfFxPackageRule, name, attr.gnName, err)
		}
		if len(lines) == 0 {
			return nil, errors.New("unexpected empty GN expression")
		}
		op := " = "
		if attr.gnName == "deps" {
			if hasDeps {
				op = " += "
			}
			hasDeps = true
		}
		lines[0] = attr.gnName + op + lines[0]
		body = append(body, lines...)
	}

	if visArg != nil {
		lines, err := attrAssignmentToGN(visArg, ctfFxPackageRule, nil /*compiledActionOutputs*/, false /*appendDeps*/, false /*hasWithUnitTests*/)
		if err != nil {
			return nil, fmt.Errorf("converting %s visibility: %v", ctfFxPackageRule, err)
		}
		body = append(body, lines...)
	}

	ret := []string{fmt.Sprintf("ctf_fuchsia_package(%s) {", strconv.Quote(name))}
	ret = append(ret, indent(body, 1)...)
	ret = append(ret, "}")
	return ret, nil
}
