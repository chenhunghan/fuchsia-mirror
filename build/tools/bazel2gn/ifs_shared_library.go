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

// ccLinkStubFromIfsFileRule is the Bazel macro from
// //build/bazel/rules/cc:shared_library.bzl that is converted to a GN
// `ifs_shared_library()` from //build/toolchain/ifs_shared_library.gni.
const ccLinkStubFromIfsFileRule = "cc_link_stub_from_ifs_file"

// ccLinkStubFromIfsFileOutputName validates a `cc_link_stub_from_ifs_file()`
// target and returns the GN `output_name` of the equivalent
// `ifs_shared_library()`, or "" if it is the default (the target name).
//
// Both build the link stub `lib<output_name>.so` from an `.ifs` file and do not
// build the library itself, so:
//
//   - `system_provided` must be `True`. Otherwise Bazel's `cc_import()` would
//     ship the stub as the runtime library, which GN never does.
//   - The stub is named after `stub_name` if set and after the `.ifs` file
//     otherwise, in Bazel, and after `output_name` (which defaults to the target
//     name) in GN. That name must start with `lib`, and the GN `output_name` is
//     the rest of it.
func ccLinkStubFromIfsFileOutputName(expr *syntax.CallExpr) (string, error) {
	var name, ifsFile, stubName string
	systemProvided := false
	for _, arg := range expr.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			continue
		}
		var err error
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
		case "ifs_file":
			ifsFile, err = stringLiteralValue(binaryExpr.Y)
		case "stub_name":
			stubName, err = stringLiteralValue(binaryExpr.Y)
		case "system_provided":
			rhsIdent, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.Ident)
			systemProvided = ok && rhsIdent.Name == "True"
		}
		if err != nil {
			return "", fmt.Errorf("%s of %s: %v", ident.Name, ccLinkStubFromIfsFileRule, err)
		}
	}
	if !systemProvided {
		return "", fmt.Errorf("%s must set `system_provided = True`: GN ifs_shared_library() only provides a link stub, and never ships it as the runtime library", ccLinkStubFromIfsFileRule)
	}
	if ifsFile == "" {
		return "", fmt.Errorf("%s must set `ifs_file`", ccLinkStubFromIfsFileRule)
	}

	stubBaseName := stubName
	if stubBaseName == "" {
		ifsPath, err := bazelFileLabelToGNPath(ifsFile)
		if err != nil {
			return "", err
		}
		base := path.Base(ifsPath)
		stubBaseName = strings.TrimSuffix(base, path.Ext(base))
	}
	outputName, ok := strings.CutPrefix(stubBaseName, "lib")
	if !ok || outputName == "" {
		return "", fmt.Errorf("the link stub of %s %q is named %q, but GN ifs_shared_library() names it `lib<output_name>.so`; set `stub_name` to `lib<name>`", ccLinkStubFromIfsFileRule, name, stubBaseName+".so")
	}
	if outputName == name {
		return "", nil
	}
	return outputName, nil
}

// errRawOutputNameOnCcLinkStub is returned when a `cc_link_stub_from_ifs_file()`
// target sets `output_name` with a raw attribute annotation.
var errRawOutputNameOnCcLinkStub = errors.New("bazel2gn sets the `output_name` of cc_link_stub_from_ifs_file from `stub_name` or `ifs_file`; remove the `# @bazel2gn:raw_attribute:output_name` annotation")
