// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"regexp"
	"strings"

	"go.starlark.net/syntax"
)

// sharedLibFileNameRe splits a shared library file name into the part before
// `.so` and the extension starting at `so` (e.g. `libfoo.so.1` into `libfoo`
// and `so.1`).
var sharedLibFileNameRe = regexp.MustCompile(`^(.+?)\.(so(?:\..+)?)$`)

// fxCcSharedLibraryOutputNameToGN returns the GN statements that make the GN
// `shared_library()` converted from an `fx_cc_shared_library()` produce the
// same file name as Bazel.
//
// Bazel names the output `shared_lib_name` when it's set. Otherwise it uses
// `name` if it ends with `.so` or contains `.so.`, and `lib<name>.so` if not.
// See `get_shared_library_name_and_soname()` in
// //build/bazel/rules/cc/fx_cc_shared_library.bzl.
//
// GN names the output `<output_prefix><output_name>.<output_extension>`, where
// `output_name` defaults to the target name, `output_extension` defaults to
// `so`, and the `lib` output prefix is only added when `output_name` doesn't
// already start with it (unless `output_prefix_override = true`).
func fxCcSharedLibraryOutputNameToGN(nameExpr, sharedLibNameExpr syntax.Expr, rawAttrs []string) ([]string, error) {
	name, err := stringLiteralValue(nameExpr)
	if err != nil {
		return nil, fmt.Errorf("name of fx_cc_shared_library must be a string literal: %v", err)
	}
	if strings.Contains(name, "/") {
		return nil, fmt.Errorf("fx_cc_shared_library name %q must not contain a subdirectory, GN target names can't", name)
	}

	var fileName string
	if sharedLibNameExpr != nil {
		if hasRawOutputNameAttr(rawAttrs) {
			return nil, errors.New("fx_cc_shared_library sets both shared_lib_name and a `# @bazel2gn:raw_attribute:output_name` annotation; remove the annotation")
		}
		fileName, err = stringLiteralValue(sharedLibNameExpr)
		if err != nil {
			return nil, fmt.Errorf("shared_lib_name of fx_cc_shared_library must be a string literal: %v", err)
		}
		if strings.Contains(fileName, "/") {
			return nil, fmt.Errorf("shared_lib_name %q of fx_cc_shared_library must not contain a subdirectory", fileName)
		}
	} else {
		if hasRawOutputNameAttr(rawAttrs) {
			// The annotation sets the GN output name explicitly.
			return nil, nil
		}
		if strings.HasSuffix(name, ".so") || strings.Contains(name, ".so.") {
			fileName = name
		} else {
			fileName = "lib" + name + ".so"
		}
	}

	m := sharedLibFileNameRe.FindStringSubmatch(fileName)
	if m == nil {
		return nil, fmt.Errorf("output file name %q of fx_cc_shared_library must end with `.so` or contain `.so.`", fileName)
	}
	stem, ext := m[1], m[2]

	var ret []string
	if strings.HasPrefix(stem, "lib") {
		gnDefaultStem := name
		if !strings.HasPrefix(name, "lib") {
			gnDefaultStem = "lib" + name
		}
		if stem != gnDefaultStem {
			ret = append(ret, fmt.Sprintf("output_name = %s", gnQuote(stem)))
		}
	} else {
		if stem != name {
			ret = append(ret, fmt.Sprintf("output_name = %s", gnQuote(stem)))
		}
		ret = append(ret, "output_prefix_override = true")
	}
	if ext != "so" {
		ret = append(ret, fmt.Sprintf("output_extension = %s", gnQuote(ext)))
	}
	return ret, nil
}
