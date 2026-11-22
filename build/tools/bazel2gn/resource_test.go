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

func TestResourceFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "resource with src and dest",
			bazel: `resource(
	name = "shader",
	testonly = True,
	src = "//src/ui/lib/escher/shaders:flatland/flat_main.vert",
	dest = "data/shaders/flatland/flat_main.vert",
	allow_binary_output_dir = True,
	data_deps = [":other_res"],
	visibility = ["//src/ui:__subpackages__"],
)`,
			wantGN: `resource("shader") {
	testonly = true
	sources = [
		"//src/ui/lib/escher/shaders/flatland/flat_main.vert",
	]
	outputs = [
		"data/shaders/flatland/flat_main.vert",
	]
	allow_binary_output_dir = true
	data_deps = [
		":other_res",
	]
	visibility = [
		":*",
		"//src/ui/*",
	]
}`,
		},
		{
			name: "fuchsia_package_resource alias",
			bazel: `fuchsia_package_resource(
	name = "keymap",
	src = "data/keymap.json",
	dest = "data/keymap.json",
)`,
			wantGN: `resource("keymap") {
	sources = [
		"data/keymap.json",
	]
	outputs = [
		"data/keymap.json",
	]
}`,
		},
		{
			name: "resource with sources and outputs",
			bazel: `resource(
	name = "shaders",
	sources = [
		"shaders/main.frag",
		"shaders/main.vert",
	],
	outputs = ["data/shaders/{{source_file_part}}"],
	deps = [":gen_shaders"],
)`,
			wantGN: `resource("shaders") {
	sources = [
		"shaders/main.frag",
		"shaders/main.vert",
	]
	outputs = [
		"data/shaders/{{source_file_part}}",
	]
	deps = [
		":gen_shaders",
	]
}`,
		},
		{
			name: "resource referencing same-file compiled_action output",
			bazel: `compiled_action(
	name = "compile_shader",
	srcs = ["main.vert"],
	outs = ["main.spirv"],
	args = [
		"$(location :main.vert)",
		"-o",
		"$(location :main.spirv)",
	],
	tool = "//third_party/shaderc:glslc",
)

resource(
	name = "spirv_resources",
	sources = [":main.spirv"],
	outputs = ["data/{{source_file_part}}"],
)`,
			wantGN: `compiled_action("compile_shader") {
	sources = [
		"main.vert",
	]
	outputs = [
		"$target_gen_dir/main.spirv",
	]
	args = [
		rebase_path("main.vert", root_build_dir),
		"-o",
		rebase_path("$target_gen_dir/main.spirv", root_build_dir),
	]
	tool = "//third_party/shaderc:glslc"
}
resource("spirv_resources") {
	sources = [
		"$target_gen_dir/main.spirv",
	]
	outputs = [
		"data/{{source_file_part}}",
	]
	deps = [
		":compile_shader",
	]
}`,
		},
		{
			name: "resource_group with files",
			bazel: `resource_group(
	name = "goldens",
	testonly = True,
	files = [
		{
			"source": "golden1.png",
			"dest": "data/goldens/first.png",
		},
		{
			"source": "//src/ui/testing:golden2.png",
			"dest": "data/goldens/second.png",
		},
	],
	data_deps = [":extra_goldens"],
)`,
			wantGN: `resource_group("goldens") {
	testonly = true
	files = [
		{
			source = "golden1.png"
			dest = "data/goldens/first.png"
		},
		{
			source = "//src/ui/testing/golden2.png"
			dest = "data/goldens/second.png"
		},
	]
	data_deps = [
		":extra_goldens",
	]
}`,
		},
		{
			name: "fuchsia_package_resource_group with basename_only",
			bazel: `fuchsia_package_resource_group(
	name = "test-images",
	testonly = True,
	srcs = [
		"//src/storage/ext4:test_files/1file.img",
		"//src/storage/ext4:test_files/nest.img",
	],
	basename_only = True,
	dest = "data",
)`,
			wantGN: `resource("test-images") {
	testonly = true
	sources = [
		"//src/storage/ext4/test_files/1file.img",
		"//src/storage/ext4/test_files/nest.img",
	]
	outputs = [
		"data/{{source_file_part}}",
	]
}`,
		},
		{
			name: "fuchsia_package_resource_group with strip_prefix",
			bazel: `fuchsia_package_resource_group(
	name = "assets",
	srcs = [
		"res/icons/a.png",
		"res/sub/b.png",
	],
	dest = "data/assets",
	strip_prefix = "res/",
)`,
			wantGN: `resource_group("assets") {
	files = [
		{
			source = "res/icons/a.png"
			dest = "data/assets/icons/a.png"
		},
		{
			source = "res/sub/b.png"
			dest = "data/assets/sub/b.png"
		},
	]
}`,
		},
		{
			name: "resource_tree",
			bazel: `resource_tree(
	name = "my-resources",
	sources_root = "res",
	sources = [
		"foo.resource",
		"bar/bar-1.resource",
	],
	dest_dir = "data/resources",
)`,
			wantGN: `resource_tree("my-resources") {
	sources_root = "res"
	sources = [
		"foo.resource",
		"bar/bar-1.resource",
	]
	dest_dir = "data/resources"
}`,
		},
		{
			name: "fuchsia_package_resource_collection",
			bazel: `fuchsia_package_resource_collection(
	name = "all_resources",
	testonly = True,
	resources = [
		":res1",
		":res2",
	],
)`,
			wantGN: `group("all_resources") {
	testonly = true
	deps = [
		":res1",
		":res2",
	]
}`,
		},
		{
			name: "cc rules with data and test packages with resources",
			bazel: `fx_cc_library(
	name = "escher_lib",
	srcs = ["escher.cc"],
	data = ["//src/ui/lib/escher/shaders:escher_shaders_resources"],
)

fx_cc_test_binary(
	name = "renderer_unittests",
	srcs = ["renderer_test.cc"],
	data = [":shaders"],
)

fx_unittest_package(
	name = "single-test-pkg",
	resources = [":shaders"],
	unit_tests = [":renderer_unittests"],
)

fx_unittest_package(
	name = "multi-test-pkg",
	resources = [":shaders"],
	unit_tests = [
		":test_a",
		":test_b",
	],
)

fx_component_manifest(
	name = "comp_manifest",
	manifest = "meta/comp.cml",
)

fx_test_component(
	name = "comp",
	compiled_manifest = ":comp_manifest",
)

fx_package(
	name = "pkg_with_res",
	components = [":helper_comp"],
	resources = [":shaders"],
	test_components = [":comp"],
)`,
			wantGN: `static_library("escher_lib") {
	sources = [
		"escher.cc",
	]
	data_deps = [
		"//src/ui/lib/escher/shaders:escher_shaders_resources",
	]
}
cc_test_executable("renderer_unittests") {
	sources = [
		"renderer_test.cc",
	]
	data_deps = [
		":shaders",
	]
	testonly = true
	emit_tool_path = false
}
fuchsia_unittest_package("single-test-pkg") {
	package_name = "single-test-pkg-gn"
	component_name = "single-test-pkg"
	deps = [
		":renderer_unittests",
		":shaders",
	]
}
fuchsia_unittest_component("multi-test-pkg.test_a.component") {
	component_name = "test_a"
	visibility = [ ":*" ]
	deps = [
		":test_a",
	]
}
fuchsia_unittest_component("multi-test-pkg.test_b.component") {
	component_name = "test_b"
	visibility = [ ":*" ]
	deps = [
		":test_b",
	]
}
fuchsia_test_package("multi-test-pkg") {
	package_name = "multi-test-pkg-gn"
	test_components = [
		":multi-test-pkg.test_a.component",
		":multi-test-pkg.test_b.component",
	]
	deps = [
		":shaders",
	]
}
fuchsia_component_manifest("comp_manifest") {
	manifest = "meta/comp.cml"
}
fuchsia_test_component("comp") {
	cm_label = ":comp_manifest"
}
fuchsia_test_package("pkg_with_res") {
	package_name = "pkg_with_res-gn"
	test_components = [
		":comp",
	]
	deps = [
		":helper_comp",
		":shaders",
	]
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			lines, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			gotGN := strings.Join(lines, "\n")
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestResourceErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "resource missing dest",
			bazel: `resource(
	name = "foo",
	src = "a.txt",
)`,
			wantErr: "missing `dest`",
		},
		{
			name: "resource with both src and sources",
			bazel: `resource(
	name = "foo",
	src = "a.txt",
	dest = "data/a.txt",
	sources = ["a.txt"],
	outputs = ["data/{{source_file_part}}"],
)`,
			wantErr: "cannot specify both",
		},
		{
			name: "resource dest starting with slash",
			bazel: `resource(
	name = "foo",
	src = "a.txt",
	dest = "/data/a.txt",
)`,
			wantErr: "must not start with \"/\"",
		},
		{
			name: "resource_group missing files and srcs",
			bazel: `resource_group(
	name = "foo",
)`,
			wantErr: "missing `files` or (`srcs` and `dest`)",
		},
		{
			name: "resource_group files missing dest key",
			bazel: `resource_group(
	name = "foo",
	files = [{"source": "a.txt"}],
)`,
			wantErr: "must have both \"source\" and \"dest\" keys",
		},
		{
			name: "resource_tree dest_dir starting with slash",
			bazel: `resource_tree(
	name = "foo",
	sources = ["a.txt"],
	dest_dir = "/data",
)`,
			wantErr: "cannot start with \"/\"",
		},
		{
			name: "non-basename_only resource_group with // label and no package path",
			bazel: `fuchsia_package_resource_group(
	name = "foo",
	srcs = ["//src/foo:res/a.png"],
	dest = "data",
)`,
			wantErr: "requires package path context",
		},
		{
			name: "resource_group files with list concatenation",
			bazel: `resource_group(
	name = "foo",
	files = [{"source": "a.txt", "dest": "data/a.txt"}] + [{"source": "b.txt", "dest": "data/b.txt"}],
)`,
			wantErr: "must be a list of dicts",
		},
		{
			name: "resource_tree sources with label",
			bazel: `resource_tree(
	name = "foo",
	sources = [":a.txt"],
	dest_dir = "data",
)`,
			wantErr: "must be paths relative to sources_root",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazel2gn.FileToGN(f)
			if err == nil {
				t.Fatalf("Expected error containing %q, got nil", tc.wantErr)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("Expected error containing %q, got %v", tc.wantErr, err)
			}
		})
	}
}

func TestResourceGroupForPackage(t *testing.T) {
	bazel := `fuchsia_package_resource_group(
	name = "pkg_assets",
	srcs = [
		"//src/foo:res/a.png",
		"//src/foo/sub:b.png",
		"//other/pkg:c.png",
	],
	dest = "data",
)`
	wantGN := `resource_group("pkg_assets") {
	files = [
		{
			source = "//src/foo/res/a.png"
			dest = "data/res/a.png"
		},
		{
			source = "//src/foo/sub/b.png"
			dest = "data/sub/b.png"
		},
		{
			source = "//other/pkg/c.png"
			dest = "data/other/pkg/c.png"
		},
	]
}`
	f := toSyntaxFile(t, bazel)
	lines, err := bazel2gn.FileToGNForPackage(f, "src/foo")
	if err != nil {
		t.Fatalf("FileToGNForPackage failed: %v", err)
	}
	if diff := cmp.Diff(strings.Join(lines, "\n"), wantGN); diff != "" {
		t.Errorf("Diff found (-got +want):\n%s", diff)
	}
}
