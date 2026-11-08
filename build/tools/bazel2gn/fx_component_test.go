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

func TestFxComponentFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "multiple components sharing rustc_binary via fx_packaged_binary",
			bazel: `rustc_binary(
	name = "http_client",
	srcs = ["src/main.rs"],
	edition = "2024",
	deps = [":http_client_config"],
)

fx_packaged_binary(
	name = "http_client_packaged_bin",
	binary = ":http_client",
	binary_name = "http_client",
)

fx_component_manifest(
	name = "manifest",
	component_name = "http-client",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/http_client.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":http_client_packaged_bin"],
)

fx_component_manifest(
	name = "google_manifest",
	component_name = "google-http-client",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "//src/connectivity/policy/http-client:meta/google_http_client.cml",
)

fx_component(
	name = "google_component",
	compiled_manifest = ":google_manifest",
	deps = [":http_client_packaged_bin"],
)

fuchsia_structured_config_rust_lib(
	name = "http_client_config",
	cm_label = ":manifest",
)`,
			wantGN: `rustc_binary("http_client") {
	sources = [
		"src/main.rs",
	]
	edition = "2024"
	deps = [
		":http_client_config",
	]
}
fuchsia_component_manifest("manifest") {
	component_name = "http-client"
	manifest = "meta/http_client.cml"
}
fuchsia_component("component") {
	cm_label = ":manifest"
	deps = [
		":http_client",
	]
}
fuchsia_component_manifest("google_manifest") {
	component_name = "google-http-client"
	manifest = "//src/connectivity/policy/http-client/meta/google_http_client.cml"
}
fuchsia_component("google_component") {
	cm_label = ":google_manifest"
	deps = [
		":http_client",
	]
}
fuchsia_structured_config_rust_lib("http_client_config") {
	cm_label = ":manifest"
}`,
		},
		{
			name: "rustc_binary with crate_name matching binary_name unwraps directly",
			bazel: `rustc_binary(
	name = "exit-with-error-bin",
	testonly = True,
	srcs = ["src/main.rs"],
	crate_name = "exit_with_error_bin",
	edition = "2024",
)

fx_packaged_binary(
	name = "exit-with-error-bin_packaged_bin",
	testonly = True,
	binary = ":exit-with-error-bin",
	binary_name = "exit_with_error_bin",
)

fx_component_manifest(
	name = "component-manifest",
	testonly = True,
	component_name = "exit-with-error",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/exit-with-error.cml",
)

fx_component(
	name = "component",
	testonly = True,
	compiled_manifest = ":component-manifest",
	deps = [":exit-with-error-bin_packaged_bin"],
)`,
			wantGN: `rustc_binary("exit-with-error-bin") {
	testonly = true
	sources = [
		"src/main.rs",
	]
	output_name = "exit_with_error_bin"
	edition = "2024"
}
fuchsia_component_manifest("component-manifest") {
	testonly = true
	component_name = "exit-with-error"
	manifest = "meta/exit-with-error.cml"
}
fuchsia_component("component") {
	testonly = true
	cm_label = ":component-manifest"
	deps = [
		":exit-with-error-bin",
	]
}`,
		},
		{
			name: "fx_packaged_binary with renamed binary_name or custom install_root emits renamed_binary",
			bazel: `fx_cc_binary(
	name = "my_tool",
	srcs = ["main.cc"],
)

fx_packaged_binary(
	name = "renamed_tool_packaged_bin",
	testonly = True,
	binary = ":my_tool",
	binary_name = "alt_tool",
	install_root = "tools/",
	visibility = ["//visibility:private"],
)

fx_component_manifest(
	name = "manifest",
	testonly = True,
	manifest = "meta/tool.cml",
)

fx_component(
	name = "comp_a",
	testonly = True,
	compiled_manifest = ":manifest",
	deps = [":renamed_tool_packaged_bin"],
)

fx_component(
	name = "comp_b",
	testonly = True,
	compiled_manifest = ":manifest",
	deps = [":renamed_tool_packaged_bin"],
)`,
			wantGN: `executable("my_tool") {
	sources = [
		"main.cc",
	]
}
renamed_binary("renamed_tool_packaged_bin") {
	testonly = true
	source = "$root_out_dir/my_tool"
	source_deps = [
		":my_tool",
	]
	dest = "tools/alt_tool"
	visibility = [
		":*",
	]
}
fuchsia_component_manifest("manifest") {
	testonly = true
	manifest = "meta/tool.cml"
}
fuchsia_component("comp_a") {
	testonly = true
	cm_label = ":manifest"
	deps = [
		":renamed_tool_packaged_bin",
	]
}
fuchsia_component("comp_b") {
	testonly = true
	cm_label = ":manifest"
	deps = [
		":renamed_tool_packaged_bin",
	]
}`,
		},
		{
			name: "hoists fx_component_manifest defined after fx_component",
			bazel: `fx_package(
	name = "empty_container",
	package_name = "starless",
	components = [":empty_container_component"],
)

fx_component(
	name = "empty_container_component",
	compiled_manifest = ":empty_container_manifest",
	deps = ["//src/starnix/containers:default_init"],
)

fx_component_manifest(
	name = "empty_container_manifest",
	component_name = "empty_container",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/empty_container.cml",
)`,
			wantGN: `fuchsia_component_manifest("empty_container_manifest") {
	component_name = "empty_container"
	manifest = "meta/empty_container.cml"
}
fuchsia_component("empty_container_component") {
	cm_label = ":empty_container_manifest"
	deps = [
		"//src/starnix/containers:default_init",
	]
}`,
		},
		{
			name: "unreferenced fx_packaged_binary and skipped fx_component are ignored",
			bazel: `fx_packaged_binary(
	name = "cross_pkg_bin",
	binary = "//src/sys/pkg/bin/far:bin",
	binary_name = "far",
)

# @bazel2gn:skip
fx_component_manifest(
	name = "skipped_manifest",
	manifest = "meta/skipped.cml",
)

# @bazel2gn:skip
fx_component(
	name = "skipped_component",
	compiled_manifest = ":skipped_manifest",
	deps = [":cross_pkg_bin"],
)

fx_test_component(
	name = "test_component",
	compiled_manifest = ":skipped_manifest",
	deps = [":cross_pkg_bin"],
)`,
			wantGN: "",
		},
		{
			name: "resolves fx_packaged_binary inside select and shared variable",
			bazel: `fx_cc_binary(
	name = "bin_a",
	srcs = ["a.cc"],
)

fx_cc_binary(
	name = "bin_b",
	srcs = ["b.cc"],
)

fx_packaged_binary(
	name = "bin_a_pkg",
	binary = ":bin_a",
	binary_name = "bin_a",
)

fx_packaged_binary(
	name = "bin_b_pkg",
	binary = ":bin_b",
	binary_name = "bin_b",
)

_COMMON_DEPS = [":bin_a_pkg"]

fx_component_manifest(
	name = "manifest",
	manifest = "meta/c.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = _COMMON_DEPS + select({
		"@platforms//os:fuchsia": [":bin_b_pkg"],
		"//conditions:default": [],
	}),
)`,
			wantGN: `executable("bin_a") {
	sources = [
		"a.cc",
	]
}
executable("bin_b") {
	sources = [
		"b.cc",
	]
}
_COMMON_DEPS = [
	":bin_a",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "_COMMON_DEPS" ])

fuchsia_component_manifest("manifest") {
	manifest = "meta/c.cml"
}
fuchsia_component("component") {
	cm_label = ":manifest"
	deps = []
	deps += _COMMON_DEPS
	if (is_fuchsia) {
		deps += [
			":bin_b",
		]
	}
}`,
		},
		{
			name: "auto-skips fx_component_manifest consumed only by fx_test_component or skipped fx_component",
			bazel: `rustc_library(
	name = "lib",
	srcs = ["src/lib.rs"],
	edition = "2024",
)

fx_packaged_binary(
	name = "lib_test_packaged_bin",
	testonly = True,
	binary = ":lib_test",
	binary_name = "lib_lib_test",
)

fx_component_manifest(
	name = "test-manifest",
	testonly = True,
	component_name = "lib-test",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/lib_test.cml",
)

fx_test_component(
	name = "test-component",
	compiled_manifest = ":test-manifest",
	deps = [":lib_test_packaged_bin"],
)

fx_component_manifest(
	name = "skipped-comp-manifest",
	manifest = "meta/skipped.cml",
)

# @bazel2gn:skip
fx_component(
	name = "skipped-comp",
	compiled_manifest = ":skipped-comp-manifest",
)

fx_component_manifest(
	name = "shared-manifest",
	manifest = "meta/shared.cml",
)

# @bazel2gn:skip
fx_component(
	name = "skipped-shared-comp",
	compiled_manifest = ":shared-manifest",
)

fuchsia_structured_config_rust_lib(
	name = "shared_config",
	cm_label = ":shared-manifest",
)`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	edition = "2024"
}
fuchsia_component_manifest("shared-manifest") {
	manifest = "meta/shared.cml"
}
fuchsia_structured_config_rust_lib("shared_config") {
	cm_label = ":shared-manifest"
}`,
		},
		{
			name: "fx_component_manifest with restricted_features",
			bazel: `fx_component_manifest(
	name = "manifest",
	component_name = "dispatcher",
	manifest = "meta/dispatcher.cml",
	restricted_features = ["dynamic_dictionaries"],
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
)`,
			wantGN: `fuchsia_component_manifest("manifest") {
	component_name = "dispatcher"
	manifest = "meta/dispatcher.cml"
	restricted_features = [
		"dynamic_dictionaries",
	]
}
fuchsia_component("component") {
	cm_label = ":manifest"
}`,
		},
		{
			name: "fx_compiled_manifest converts to cm",
			bazel: `fuchsia_structured_config_rust_lib(
	name = "dispatcher_config",
	cm_label = ":dispatcher_cm",
)

fx_compiled_manifest(
	name = "elf_runner_cm",
	component_name = "elf_runner",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/elf_runner.cml",
)

fx_compiled_manifest(
	name = "dispatcher_cm",
	testonly = True,
	component_name = "dispatcher",
	manifest = "meta/dispatcher.cml",
	output_name = "custom/dispatcher.cm",
	restricted_features = ["dynamic_dictionaries"],
	visibility = [":__subpackages__"],
)

fx_compiled_manifest(
	name = "default_name",
	manifest = "meta/default_name.cml",
)`,
			wantGN: `cm("dispatcher_cm") {
	testonly = true
	manifest = "meta/dispatcher.cml"
	output_name = "custom/dispatcher.cm"
	config_values_package_path = "meta/dispatcher.cvf"
	restricted_features = [
		"dynamic_dictionaries",
	]
	visibility = [
		"./*",
	]
}
fuchsia_structured_config_rust_lib("dispatcher_config") {
	cm_label = ":dispatcher_cm"
}
cm("elf_runner_cm") {
	manifest = "meta/elf_runner.cml"
	output_name = "cml/elf_runner_cm/elf_runner.cm"
	config_values_package_path = "meta/elf_runner.cvf"
}
cm("default_name") {
	manifest = "meta/default_name.cml"
	output_name = "cml/default_name/default_name.cm"
	config_values_package_path = "meta/default_name.cvf"
}`,
		},
		{
			name: "DSO and cdylib fx_packaged_binary unwrapping and renamed_binary",
			bazel: `fuchsia_cc_dso(
	name = "cc_dso",
	is_async = True,
	srcs = ["main.cc"],
	output_name = "custom_cc_dso",
)

fx_packaged_binary(
	name = "cc_dso_packaged",
	binary = ":cc_dso",
	binary_name = "libcustom_cc_dso.so",
	install_root = "lib/",
)

fuchsia_rust_dso(
	name = "rust_dso",
	srcs = ["src/lib.rs"],
	edition = "2024",
)

fx_packaged_binary(
	name = "rust_dso_packaged",
	binary = ":rust_dso",
	binary_name = "librust_dso.so",
	install_root = "lib/",
)

rustc_cdylib(
	name = "plugin_cdylib",
	srcs = ["src/plugin.rs"],
	crate_name = "my-plugin",
	edition = "2024",
)

fx_packaged_binary(
	name = "plugin_cdylib_default_packaged",
	binary = ":plugin_cdylib",
	binary_name = "libmy_plugin.so",
	install_root = "lib/",
)

fx_packaged_binary(
	name = "plugin_cdylib_renamed_packaged",
	binary = ":plugin_cdylib",
	binary_name = "plugin.so",
	install_root = "plugins/",
)

fx_component_manifest(
	name = "dso_manifest",
	manifest = "meta/dso.cml",
)

fx_component(
	name = "dso_component",
	compiled_manifest = ":dso_manifest",
	deps = [
		":cc_dso_packaged",
		":rust_dso_packaged",
		":plugin_cdylib_default_packaged",
		":plugin_cdylib_renamed_packaged",
	],
)`,
			wantGN: `fuchsia_cc_dso("cc_dso") {
	async = true
	sources = [
		"main.cc",
	]
	output_name = "custom_cc_dso"
}
fuchsia_rust_dso("rust_dso") {
	sources = [
		"src/lib.rs",
	]
	edition = "2024"
}
rustc_cdylib("plugin_cdylib") {
	sources = [
		"src/plugin.rs",
	]
	crate_name = "my-plugin"
	edition = "2024"
}
renamed_binary("plugin_cdylib_renamed_packaged") {
	source = "$root_out_dir/libmy_plugin.so"
	source_deps = [
		":plugin_cdylib",
	]
	dest = "plugins/plugin.so"
}
fuchsia_component_manifest("dso_manifest") {
	manifest = "meta/dso.cml"
}
fuchsia_component("dso_component") {
	cm_label = ":dso_manifest"
	deps = [
		":cc_dso",
		":rust_dso",
		":plugin_cdylib",
		":plugin_cdylib_renamed_packaged",
	]
}`,
		},
		{
			name: "skipped fx_component using fx_compiled_manifest",
			bazel: `fx_compiled_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

# @bazel2gn:skip
fx_component(
	name = "component",
	compiled_manifest = ":manifest",
)`,
			wantGN: `cm("manifest") {
	manifest = "meta/foo.cml"
	output_name = "cml/manifest/manifest.cm"
	config_values_package_path = "meta/manifest.cvf"
}`,
		},
		{
			name: "fx_packaged_binary wrapping fx_cc_test_binary unwraps when install_root is bin/",
			bazel: `fx_cc_test_binary(
	name = "foo_test",
	srcs = ["foo_test.cc"],
)

fx_cc_test_binary(
	name = "bar_test_bin",
	srcs = ["bar_test.cc"],
	gn_output_name = "custom_bar_test",
)

fx_packaged_binary(
	name = "foo_test_in_bin_dir",
	testonly = True,
	binary = ":foo_test",
	binary_name = "foo_test",
)

fx_packaged_binary(
	name = "bar_test_in_bin_dir",
	testonly = True,
	binary = ":bar_test_bin",
	binary_name = "custom_bar_test",
	install_root = "bin/",
)

fx_component_manifest(
	name = "test_manifest",
	testonly = True,
	manifest = "meta/foo_test.cml",
)

fx_test_component(
	name = "foo_test_component",
	compiled_manifest = ":test_manifest",
	deps = [
		":foo_test_in_bin_dir",
		":bar_test_in_bin_dir",
	],
)

fx_package(
	name = "foo_test_pkg",
	test_components = [":foo_test_component"],
)`,
			wantGN: `cc_test_executable("foo_test") {
	sources = [
		"foo_test.cc",
	]
	testonly = true
	emit_tool_path = false
}
cc_test_executable("bar_test_bin") {
	sources = [
		"bar_test.cc",
	]
	output_name = "custom_bar_test"
	testonly = true
	emit_tool_path = false
}
fuchsia_component_manifest("test_manifest") {
	testonly = true
	manifest = "meta/foo_test.cml"
}
fuchsia_test_component("foo_test_component") {
	cm_label = ":test_manifest"
	deps = [
		":foo_test",
		":bar_test_bin",
	]
}
fuchsia_test_package("foo_test_pkg") {
	package_name = "foo_test_pkg-gn"
	test_components = [
		":foo_test_component",
	]
}`,
		},
		{
			name: "fx_packaged_binary wrapping fx_cc_test_binary emits renamed_binary when install_root is test/ or custom",
			bazel: `fx_cc_test_binary(
	name = "foo_test",
	srcs = ["foo_test.cc"],
)

fx_cc_test_binary(
	name = "bar_test",
	srcs = ["bar_test.cc"],
)

fx_packaged_binary(
	name = "foo_test_in_test_dir",
	testonly = True,
	binary = ":foo_test",
	binary_name = "foo_test",
	install_root = "test/",
)

fx_packaged_binary(
	name = "bar_test_renamed",
	testonly = True,
	binary = ":bar_test",
	binary_name = "renamed_bar_test",
)

fx_component_manifest(
	name = "test_manifest",
	testonly = True,
	manifest = "meta/foo_test.cml",
)

fx_test_component(
	name = "foo_test_component",
	compiled_manifest = ":test_manifest",
	deps = [
		":foo_test_in_test_dir",
		":bar_test_renamed",
	],
)

fx_package(
	name = "foo_test_pkg",
	test_components = [":foo_test_component"],
)`,
			wantGN: `cc_test_executable("foo_test") {
	sources = [
		"foo_test.cc",
	]
	testonly = true
	emit_tool_path = false
}
cc_test_executable("bar_test") {
	sources = [
		"bar_test.cc",
	]
	testonly = true
	emit_tool_path = false
}
renamed_binary("foo_test_in_test_dir") {
	testonly = true
	source = "$root_out_dir/foo_test"
	source_deps = [
		":foo_test",
	]
	dest = "test/foo_test"
}
renamed_binary("bar_test_renamed") {
	testonly = true
	source = "$root_out_dir/bar_test"
	source_deps = [
		":bar_test",
	]
	dest = "bin/renamed_bar_test"
}
fuchsia_component_manifest("test_manifest") {
	testonly = true
	manifest = "meta/foo_test.cml"
}
fuchsia_test_component("foo_test_component") {
	cm_label = ":test_manifest"
	deps = [
		":foo_test_in_test_dir",
		":bar_test_renamed",
	]
}
fuchsia_test_package("foo_test_pkg") {
	package_name = "foo_test_pkg-gn"
	test_components = [
		":foo_test_component",
	]
}`,
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

func TestFxComponentErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "fx_component_manifest missing manifest",
			bazel: `fx_component_manifest(
	name = "manifest",
)`,
		},
		{
			name: "fx_component_manifest unsupported attribute",
			bazel: `fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
	required_offers = ["fuchsia.logger.LogSink"],
)`,
		},
		{
			name: "fx_component missing compiled_manifest",
			bazel: `fx_component(
	name = "component",
)`,
		},
		{
			name: "fx_component cross-package compiled_manifest",
			bazel: `fx_component(
	name = "component",
	compiled_manifest = "//other/pkg:manifest",
)`,
		},
		{
			name: "fx_component unsupported attribute",
			bazel: `fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	data = ["foo.txt"],
)`,
		},
		{
			name: "fx_packaged_binary referencing cross-package binary",
			bazel: `fx_packaged_binary(
	name = "pkg_bin",
	binary = "//other/pkg:bin",
	binary_name = "bin",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":pkg_bin"],
)`,
		},
		{
			name: "fx_packaged_binary referencing skipped binary",
			bazel: `# @bazel2gn:skip
fx_cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)

fx_packaged_binary(
	name = "pkg_bin",
	binary = ":bin",
	binary_name = "bin",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":pkg_bin"],
)`,
		},
		{
			name: "skipped fx_packaged_binary requiring renamed_binary",
			bazel: `fx_cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)

# @bazel2gn:skip
fx_packaged_binary(
	name = "pkg_bin",
	binary = ":bin",
	binary_name = "renamed_bin",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":pkg_bin"],
)`,
		},
		{
			name: "fx_compiled_manifest missing manifest",
			bazel: `fx_compiled_manifest(
	name = "manifest",
)`,
		},
		{
			name: "fx_compiled_manifest unsupported attribute",
			bazel: `fx_compiled_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
	required_offers = ["fuchsia.logger.LogSink"],
)`,
		},
		{
			name: "fx_compiled_manifest non-literal component_name",
			bazel: `fx_compiled_manifest(
	name = "manifest",
	component_name = NAME,
	manifest = "meta/foo.cml",
)`,
		},
		{
			name: "fx_packaged_binary with non-default destination for fuchsia_cc_dso",
			bazel: `fuchsia_cc_dso(
	name = "my_dso",
	is_async = True,
	srcs = ["main.cc"],
)

fx_packaged_binary(
	name = "my_dso_packaged",
	binary = ":my_dso",
	binary_name = "libmy_dso.so",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/dso.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":my_dso_packaged"],
)`,
		},
		{
			name: "fx_component using fx_compiled_manifest",
			bazel: `fx_compiled_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
)`,
		},
		{
			name: "skipped fx_packaged_binary wrapping fx_cc_test_binary with test/ install_root",
			bazel: `fx_cc_test_binary(
	name = "foo_test",
	srcs = ["foo_test.cc"],
)

# @bazel2gn:skip
fx_packaged_binary(
	name = "pkg_test_bin",
	testonly = True,
	binary = ":foo_test",
	binary_name = "foo_test",
	install_root = "test/",
)

fx_component_manifest(
	name = "manifest",
	testonly = True,
	manifest = "meta/foo_test.cml",
)

fx_test_component(
	name = "test_component",
	compiled_manifest = ":manifest",
	deps = [":pkg_test_bin"],
)

fx_package(
	name = "test_pkg",
	test_components = [":test_component"],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazel2gn.FileToGN(f); err == nil {
				t.Fatalf("Expected error converting invalid Bazel file, got nil:\n%s", tc.bazel)
			}
		})
	}
}
