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

func TestExpectationsFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "converts expectation rules 1:1 with targets, direct json5, select, and manifest hoisting",
			bazel: `rustc_test(
	name = "example_tests",
	srcs = ["src/tests.rs"],
	crate_name = "expectation_comparer_example_tests",
	crate_root = "src/tests.rs",
	edition = "2024",
	deps = ["//src/lib/fuchsia"],
)

fx_packaged_binary(
	name = "example_tests_packaged",
	testonly = True,
	binary = ":example_tests",
	binary_name = "expectation_comparer_example_tests",
)

fx_expectations(
	name = "example_expectations",
	expectations = "//src/lib/testing/expectation:example_expectations.json5",
	includes = ["included_expectations.json5"],
	visibility = ["//visibility:private"],
)

fx_generated_expectations(
	name = "generated_expectations_example",
	includes = [
		"example_expectations_part1.json5",
		"//src/lib/testing/expectation:example_expectations_part2.json5",
	] + select({
		"@platforms//cpu:riscv64": ["riscv64_expectations.json5"],
		"//conditions:default": [],
	}),
)

fx_test_component_with_expectations(
	name = "expectation-example-tests",
	testonly = True,
	compiled_manifest = ":expectation_example_tests_manifest",
	expectations = ":example_expectations",
	treatment_of_cases_with_error_logs = "RUN_ONLY_CASES_WITH_ERROR_LOGS",
	visibility = ["//visibility:private"],
	deps = [":example_tests_packaged"],
)

fx_test_component_with_expectations(
	name = "expectation-example-tests-generated",
	compiled_manifest = ":expectation_example_tests_manifest",
	expectations = ":generated_expectations_example",
	treatment_of_cases_with_error_logs = "SKIP_CASES_WITH_ERROR_LOGS",
	deps = [":example_tests_packaged"],
)

fx_test_component_with_expectations(
	name = "expectation-example-tests-direct",
	compiled_manifest = ":expectation_example_tests_manifest",
	expectations = select({
		"@platforms//cpu:riscv64": "//src/foo:expects/riscv64.json5",
		"//conditions:default": "//src/bar:shared_expectations",
	}),
	deps = [":example_tests_packaged"],
)

fx_component_manifest_with_expectations(
	name = "expectation_example_tests_manifest",
	testonly = True,
	component_name = "expectation-example-tests",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/test_root.cml",
	restricted_features = ["dynamic_dictionaries"],
)

fx_package(
	name = "expectation-example-package.package",
	package_name = "expectation-example-package",
	test_components = [
		":expectation-example-tests",
		":expectation-example-tests-generated",
		":expectation-example-tests-direct",
	],
)

fx_test(
	name = "expectation-example-package",
	package = ":expectation-example-package.package",
	test_type = "system",
)`,
			wantGN: `rustc_test("example_tests") {
	sources = [
		"src/tests.rs",
	]
	output_name = "expectation_comparer_example_tests"
	source_root = "src/tests.rs"
	edition = "2024"
	deps = [
		"//src/lib/fuchsia",
	]
}
fuchsia_expectations("example_expectations") {
	expectations = "//src/lib/testing/expectation/example_expectations.json5"
	visibility = [
		":*",
	]
}
generated_expectations_file("generated_expectations_example") {
	includes = []
	includes += [
		"example_expectations_part1.json5",
		"//src/lib/testing/expectation/example_expectations_part2.json5",
	]
	if (current_cpu == "riscv64") {
		includes += [
			"riscv64_expectations.json5",
		]
	}
}
fuchsia_component_manifest_with_expectations("expectation_example_tests_manifest") {
	testonly = true
	component_name = "expectation-example-tests"
	manifest = "meta/test_root.cml"
	restricted_features = [
		"dynamic_dictionaries",
	]
}
fuchsia_test_component_with_expectations("expectation-example-tests") {
	testonly = true
	cm_label = ":expectation_example_tests_manifest"
	expectations = ":example_expectations"
	treatment_of_cases_with_error_logs = "RUN_ONLY_CASES_WITH_ERROR_LOGS"
	visibility = [
		":*",
	]
	deps = [
		":example_tests",
	]
	test_type = "system"
}
fuchsia_test_component_with_expectations("expectation-example-tests-generated") {
	cm_label = ":expectation_example_tests_manifest"
	expectations = ":generated_expectations_example"
	treatment_of_cases_with_error_logs = "SKIP_CASES_WITH_ERROR_LOGS"
	deps = [
		":example_tests",
	]
	test_type = "system"
}
fuchsia_test_component_with_expectations("expectation-example-tests-direct") {
	cm_label = ":expectation_example_tests_manifest"
	if (current_cpu == "riscv64") {
		expectations = "//src/foo/expects/riscv64.json5"
	} else {
		expectations = "//src/bar:shared_expectations"
	}
	deps = [
		":example_tests",
	]
	test_type = "system"
}
fuchsia_test_package("expectation-example-package.package") {
	package_name = "expectation-example-package-gn"
	test_components = [
		":expectation-example-tests",
		":expectation-example-tests-generated",
		":expectation-example-tests-direct",
	]
}`,
		},
		{
			name: "skips expectation test component and orphan manifest when fx_package is skipped or absent",
			bazel: `fx_component_manifest_with_expectations(
	name = "skipped_pkg_manifest",
	manifest = "meta/skipped.cml",
)

fx_test_component_with_expectations(
	name = "skipped_pkg_comp",
	compiled_manifest = ":skipped_pkg_manifest",
	expectations = "expects/skipped.json5",
)

# @bazel2gn:skip
fx_package(
	name = "skipped_pkg",
	test_components = [":skipped_pkg_comp"],
)

fx_component_manifest_with_expectations(
	name = "unpackaged_manifest",
	manifest = "meta/unpackaged.cml",
)

fx_test_component_with_expectations(
	name = "unpackaged_comp",
	compiled_manifest = ":unpackaged_manifest",
	expectations = "expects/unpackaged.json5",
)`,
			wantGN: "",
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

func TestExpectationsErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "fx_component_manifest_with_expectations missing manifest",
			bazel: `fx_component_manifest_with_expectations(
	name = "manifest",
)`,
			wantErr: "missing `manifest` attribute in fx_component_manifest_with_expectations",
		},
		{
			name: "fx_test_component_with_expectations missing expectations",
			bazel: `fx_component_manifest_with_expectations(
	name = "manifest",
	manifest = "meta/test.cml",
)

fx_test_component_with_expectations(
	name = "comp",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "pkg",
	test_components = [":comp"],
)`,
			wantErr: "missing `expectations` attribute",
		},
		{
			name: "fx_test_component_with_expectations invalid treatment",
			bazel: `fx_component_manifest_with_expectations(
	name = "manifest",
	manifest = "meta/test.cml",
)

fx_test_component_with_expectations(
	name = "comp",
	compiled_manifest = ":manifest",
	expectations = "exp.json5",
	treatment_of_cases_with_error_logs = "BAD",
)

fx_package(
	name = "pkg",
	test_components = [":comp"],
)`,
			wantErr: "invalid treatment_of_cases_with_error_logs",
		},
		{
			name: "fx_test_component_with_expectations disallows check_includes and component_name",
			bazel: `fx_component_manifest_with_expectations(
	name = "manifest",
	manifest = "meta/test.cml",
)

fx_test_component_with_expectations(
	name = "comp",
	check_includes = False,
	compiled_manifest = ":manifest",
	expectations = "exp.json5",
)

fx_package(
	name = "pkg",
	test_components = [":comp"],
)`,
			wantErr: `attribute "check_includes" of fx_test_component_with_expectations`,
		},
		{
			name: "fx_test_component_with_expectations cross-package compiled_manifest",
			bazel: `fx_test_component_with_expectations(
	name = "comp",
	compiled_manifest = "//other/pkg:manifest",
	expectations = "exp.json5",
)

fx_package(
	name = "pkg",
	test_components = [":comp"],
)`,
			wantErr: "must refer to a target in the same BUILD file",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazel2gn.FileToGN(f)
			if err == nil {
				t.Fatalf("Expected error converting invalid Bazel file, got nil:\n%s", tc.bazel)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("FileToGN() error = %q, want substring %q", err.Error(), tc.wantErr)
			}
		})
	}
}
