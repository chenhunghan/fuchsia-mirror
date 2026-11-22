// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestFxCcSharedLibraryConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "default output name",
			bazel: `fx_cc_shared_library(
	name = "foo",
	visibility = ["//bar:__pkg__"],
	deps = [":foo_lib"],
)`,
			wantGN: `shared_library("foo") {
	visibility = [
		":*",
		"//bar:*",
	]
	deps = [
		":foo_lib",
	]
}`,
		},
		{
			name: "all supported attributes",
			bazel: `fx_cc_shared_library(
	name = "foo",
	build_flags = [":config"],
	disable_build_flags = ["//build/config:symbol_visibility_hidden"],
	disable_instrumentation = True,
	dynamic_deps = [":bar"],
	gn_public_configs = [":pub_config"],
	gn_public_deps = [
		":foo_headers",
		"//src/lib/fxl",  # @bazel2gn:path_overwrite://src/lib/fxl:fxl_gn
	],
	testonly = True,
	user_link_flags = ["-Wl,-z,defs"],
	deps = [":foo_lib"],
)`,
			wantGN: `shared_library("foo") {
	configs += [
		":config",
	]
	configs -= [
		"//build/config:symbol_visibility_hidden",
	]
	exclude_toolchain_tags = [ "instrumented" ]
	public_configs = [
		":pub_config",
	]
	public_deps = [
		":foo_headers",
		"//src/lib/fxl:fxl_gn",
	]
	testonly = true
	ldflags = [
		"-Wl,-z,defs",
	]
	deps = [
		":foo_lib",
	]
	deps += [
		":bar",
	]
}`,
		},
		{
			name: "only dynamic_deps",
			bazel: `fx_cc_shared_library(
	name = "foo",
	dynamic_deps = [":bar"],
)`,
			wantGN: `shared_library("foo") {
	deps = [
		":bar",
	]
}`,
		},
		{
			name: "dynamic_deps and aspect_hints without deps",
			bazel: `fx_cc_shared_library(
	name = "foo",
	aspect_hints = [":hint"],
	dynamic_deps = [":bar"],
)`,
			wantGN: `shared_library("foo") {
	deps = []
	deps += [
		":hint",
	]
	deps += [
		":bar",
	]
}`,
		},
		{
			name: "dynamic_deps with conditional deps",
			bazel: `fx_cc_shared_library(
	name = "foo",
	dynamic_deps = [":bar"],
	deps = select({
		"@platforms//os:fuchsia": [":foo_lib"],
	}),
)`,
			wantGN: `shared_library("foo") {
	deps = []
	if (is_fuchsia) {
		deps = [
			":foo_lib",
		]
	}
	deps += [
		":bar",
	]
}`,
		},
		{
			name: "name with .so suffix",
			bazel: `fx_cc_shared_library(
	name = "libfoo.so",
	deps = [":foo_lib"],
)`,
			wantGN: `shared_library("libfoo.so") {
	output_name = "libfoo"
	deps = [
		":foo_lib",
	]
}`,
		},
		{
			name: "name with .so suffix and no lib prefix",
			bazel: `fx_cc_shared_library(
	name = "foo.so",
)`,
			wantGN: `shared_library("foo.so") {
	output_name = "foo"
	output_prefix_override = true
}`,
		},
		{
			name: "name with version suffix",
			bazel: `fx_cc_shared_library(
	name = "libfoo.so.1",
)`,
			wantGN: `shared_library("libfoo.so.1") {
	output_name = "libfoo"
	output_extension = "so.1"
}`,
		},
		{
			name: "shared_lib_name matching the GN default",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "libfoo.so",
)`,
			wantGN: `shared_library("foo") {
}`,
		},
		{
			name: "shared_lib_name with lib prefix",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "libbar.so",
)`,
			wantGN: `shared_library("foo") {
	output_name = "libbar"
}`,
		},
		{
			name: "shared_lib_name without lib prefix",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "bar.so",
)`,
			wantGN: `shared_library("foo") {
	output_name = "bar"
	output_prefix_override = true
}`,
		},
		{
			name: "shared_lib_name equal to name without lib prefix",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "foo.so",
)`,
			wantGN: `shared_library("foo") {
	output_prefix_override = true
}`,
		},
		{
			name: "shared_lib_name with version suffix",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "libfoo.so.2",
)`,
			wantGN: `shared_library("foo") {
	output_extension = "so.2"
}`,
		},
		{
			name: "raw output_name annotation",
			bazel: `# @bazel2gn:raw_attribute:output_name = "bar"
fx_cc_shared_library(
	name = "libfoo.so",
)`,
			wantGN: `shared_library("libfoo.so") {
	output_name = "bar"
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestFxCcSharedLibraryConversionErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "exports_filter",
			bazel: `fx_cc_shared_library(
	name = "foo",
	exports_filter = [":bar"],
)`,
			wantErr: `doesn't support attribute "exports_filter"`,
		},
		{
			name: "roots",
			bazel: `fx_cc_shared_library(
	name = "foo",
	roots = [":bar"],
)`,
			wantErr: `doesn't support attribute "roots"`,
		},
		{
			name: "additional_linker_inputs",
			bazel: `fx_cc_shared_library(
	name = "foo",
	additional_linker_inputs = ["foo.ld"],
)`,
			wantErr: `doesn't support attribute "additional_linker_inputs"`,
		},
		{
			name: "transitive_features",
			bazel: `fx_cc_shared_library(
	name = "foo",
	transitive_features = ["-asan"],
)`,
			wantErr: `doesn't support attribute "transitive_features"`,
		},
		{
			name: "win_def_file",
			bazel: `fx_cc_shared_library(
	name = "foo",
	win_def_file = "foo.def",
)`,
			wantErr: `doesn't support attribute "win_def_file"`,
		},
		{
			name: "non-literal shared_lib_name",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = select({
		"@platforms//os:fuchsia": "libfoo.so",
		"//conditions:default": "libbar.so",
	}),
)`,
			wantErr: "shared_lib_name of fx_cc_shared_library must be a string literal",
		},
		{
			name: "shared_lib_name in a subdirectory",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "sub/libfoo.so",
)`,
			wantErr: "must not contain a subdirectory",
		},
		{
			name: "shared_lib_name without .so",
			bazel: `fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "libfoo.dylib",
)`,
			wantErr: "must end with `.so` or contain `.so.`",
		},
		{
			name: "name in a subdirectory",
			bazel: `fx_cc_shared_library(
	name = "sub/foo",
)`,
			wantErr: "must not contain a subdirectory",
		},
		{
			name: "shared_lib_name and raw output_name annotation",
			bazel: `# @bazel2gn:raw_attribute:output_name = "bar"
fx_cc_shared_library(
	name = "foo",
	shared_lib_name = "libfoo.so",
)`,
			wantErr: "sets both shared_lib_name and a `# @bazel2gn:raw_attribute:output_name` annotation",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Fatalf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("Error %q does not contain %q", err, tc.wantErr)
			}
		})
	}
}
