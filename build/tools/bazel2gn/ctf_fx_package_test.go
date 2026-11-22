// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
)

func TestCtfFxPackageFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "package with a component",
			bazel: `fx_component_manifest(
	name = "manifest",
	testonly = True,
	component_name = "test_cases",
	manifest = "meta/test_cases.cml",
)

fx_component(
	name = "component",
	testonly = True,
	compiled_manifest = ":manifest",
)

ctf_fx_package(
	name = "pkg",
	package_name = "foo_test_cases",
	components = [":component"],
)`,
			wantGN: `fuchsia_component_manifest("manifest") {
	testonly = true
	component_name = "test_cases"
	manifest = "meta/test_cases.cml"
}
fuchsia_component("component") {
	testonly = true
	cm_label = ":manifest"
}
ctf_fuchsia_package("pkg") {
	testonly = true
	package_name = "foo_test_cases"
	deps = [
		":component",
	]
}`,
		},
		{
			// Unlike the GN copy of a test package, the GN copy of a CTF
			// package keeps the package name.
			name: "all supported attributes",
			bazel: `ctf_fx_package(
	name = "pkg",
	testonly = True,
	package_name = "foo-tests",
	components = [
		":component",
		"//src/foo:other_component",
	],
	subpackages = ["//src/bar:pkg"],
	tags = ["manual"],
	test_component_names = ["test_root"],
	visibility = ["//src/foo:__subpackages__"],
)`,
			wantGN: `ctf_fuchsia_package("pkg") {
	testonly = true
	package_name = "foo-tests"
	deps = [
		":component",
		"//src/foo:other_component",
	]
	subpackages = [
		"//src/bar:pkg",
	]
	test_component_names = [
		"test_root",
	]
	visibility = [
		":*",
		"//src/foo/*",
	]
}`,
		},
		{
			name: "package_name defaults to name",
			bazel: `ctf_fx_package(
	name = "foo-tests",
	components = [":component"],
)`,
			wantGN: `ctf_fuchsia_package("foo-tests") {
	testonly = true
	package_name = "foo-tests"
	deps = [
		":component",
	]
}`,
		},
		{
			// The svc pattern: a unit test component in the CTF package, and
			// a parent test package that subpackages it.
			name: "package with test components",
			bazel: `fx_cc_test_binary(
	name = "svc-unittests",
	srcs = ["dir_unittest.cc"],
)

fx_unittest_component(
	name = "svc-tests-component",
	binary = ":svc-unittests",
	component_name = "svc-tests",
)

ctf_fx_package(
	name = "svc-tests",
	testonly = True,
	test_component_names = ["svc-tests"],
	test_components = [":svc-tests-component"],
)

fx_component_manifest(
	name = "svc-tests-root_manifest",
	testonly = True,
	component_name = "svc-tests-root",
	manifest = "meta/svc-tests-root.cml",
)

fx_test_component(
	name = "svc-tests-root",
	compiled_manifest = ":svc-tests-root_manifest",
	test_type = "ctf",
)

fx_package(
	name = "svc-tests-latest.package",
	package_name = "svc-tests-latest",
	testonly = True,
	subpackages = [":svc-tests"],
	test_components = [":svc-tests-root"],
)

fx_test(
	name = "svc-tests-latest",
	package = ":svc-tests-latest.package",
)`,
			wantGN: `cc_test_executable("svc-unittests") {
	sources = [
		"dir_unittest.cc",
	]
	testonly = true
	emit_tool_path = false
}
fuchsia_unittest_component("svc-tests-component") {
	deps = [
		":svc-unittests",
	]
	component_name = "svc-tests"
}
ctf_fuchsia_package("svc-tests") {
	testonly = true
	package_name = "svc-tests"
	deps = [
		":svc-tests-component",
	]
	test_component_names = [
		"svc-tests",
	]
}
fuchsia_component_manifest("svc-tests-root_manifest") {
	testonly = true
	component_name = "svc-tests-root"
	manifest = "meta/svc-tests-root.cml"
}
fuchsia_test_component("svc-tests-root") {
	cm_label = ":svc-tests-root_manifest"
	test_type = "ctf"
}
fuchsia_test_package("svc-tests-latest.package") {
	package_name = "svc-tests-latest-gn"
	test_components = [
		":svc-tests-root",
	]
	subpackages = [
		":svc-tests",
	]
}`,
		},
		{
			name: "components and test components",
			bazel: `fx_test_component(
	name = "test_component",
	compiled_manifest = ":manifest",
)

ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
	components = [":component"],
	test_components = [":test_component"],
)`,
			wantGN: `fuchsia_test_component("test_component") {
	cm_label = ":manifest"
}
ctf_fuchsia_package("pkg") {
	testonly = true
	package_name = "foo-tests"
	deps = [
		":component",
	]
	deps += [
		":test_component",
	]
}`,
		},
		{
			name: "skipped package",
			bazel: `# The GN copy is written by hand.
# @bazel2gn:skip
ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
	resources = [":data"],
)`,
			wantGN: ``,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			lines, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("FileToGN() failed: %v", err)
			}
			if diff := cmp.Diff(tc.wantGN, strings.Join(lines, "\n")); diff != "" {
				t.Errorf("FileToGN() diff (-want +got):\n%s", diff)
			}
		})
	}
}

func TestCtfFxPackageErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "unsupported attribute",
			bazel: `ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
	resources = [":data"],
)`,
			wantErr: `attribute "resources" of ctf_fx_package`,
		},
		{
			name: "test component that bazel2gn can't convert",
			bazel: `# @bazel2gn:skip
fx_test_component(
	name = "test_component",
	compiled_manifest = ":manifest",
)

ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
	test_components = [":test_component"],
)`,
			wantErr: `use # @bazel2gn:skip on the ctf_fx_package and`,
		},
		{
			name: "raw attribute annotation",
			bazel: `# @bazel2gn:raw_attribute: deps += [ ":extra" ]
ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
)`,
			wantErr: "raw_attribute annotation is not supported on ctf_fx_package",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazel2gn.FileToGN(f)
			if err == nil {
				t.Fatalf("FileToGN() succeeded, want error containing %q", tc.wantErr)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("FileToGN() error = %q, want substring %q", err.Error(), tc.wantErr)
			}
		})
	}
}

// A test component that is listed by both a `ctf_fx_package()` and an
// `fx_package()` gets the `test_type` of the `fx_package()`'s `fx_test()`,
// whichever package comes first.
func TestCtfFxPackageSharedTestComponent(t *testing.T) {
	const component = `fx_test_component(
	name = "test_root",
	compiled_manifest = ":manifest",
)
`
	const ctfPackage = `
ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
	test_components = [":test_root"],
)
`
	const fxPackage = `
fx_package(
	name = "latest",
	test_components = [":test_root"],
)

fx_test(
	name = "latest_test",
	package = ":latest",
	test_type = "ctf",
)
`
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{name: "ctf_fx_package first", bazel: component + ctfPackage + fxPackage},
		{name: "fx_package first", bazel: component + fxPackage + ctfPackage},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			lines, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("FileToGN() failed: %v", err)
			}
			got := strings.Join(lines, "\n")
			want := `fuchsia_test_component("test_root") {
	cm_label = ":manifest"
	test_type = "ctf"
}`
			if !strings.Contains(got, want) {
				t.Errorf("FileToGN() =\n%s\nwant it to contain\n%s", got, want)
			}
		})
	}
}

// A `ctf_fx_package()` between two `fx_package()`s with different
// `test_type`s doesn't hide their conflict.
func TestCtfFxPackageSharedTestComponentConflict(t *testing.T) {
	bazel := `fx_test_component(
	name = "test_root",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "a",
	test_components = [":test_root"],
)

fx_test(
	name = "a_test",
	package = ":a",
	test_type = "ctf",
)

ctf_fx_package(
	name = "pkg",
	package_name = "foo-tests",
	test_components = [":test_root"],
)

fx_package(
	name = "b",
	test_components = [":test_root"],
)

fx_test(
	name = "b_test",
	package = ":b",
	test_type = "system",
)`
	f := toSyntaxFile(t, bazel)
	_, err := bazel2gn.FileToGN(f)
	if err == nil {
		t.Fatal("FileToGN() succeeded, want a test_type conflict error")
	}
	if want := "different test_type"; !strings.Contains(err.Error(), want) {
		t.Errorf("FileToGN() error = %q, want substring %q", err.Error(), want)
	}
}
