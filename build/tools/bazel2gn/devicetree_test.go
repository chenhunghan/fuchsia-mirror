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

func TestDevicetreeVisitorToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "basic devicetree_visitor",
			bazel: `devicetree_visitor(
	name = "my-visitor",
	srcs = [
		"my-visitor.cc",
		"my-visitor.h",
	],
	deps = [
		"//sdk/lib/driver/devicetree/visitors:driver-visitor",
	],
	implementation_deps = [
		"//sdk/lib/driver/logging/cpp",
	],
	non_sdk_deps = [
		"//sdk/lib/fidl/cpp",
	],
	defines = ["MY_DEFINE=1"],
	includes = ["include"],
	linkopts = ["-Wl,--no-undefined"],
	build_flags = ["//build/config:Wno-conversion"],
	disable_build_flags = ["//build/config:default_warnings"],
	enable_sdk_source_set = True,
	category = "partner",
	output_name = "custom-visitor",
	testonly = True,
	visibility = ["//src:__subpackages__"],
)`,
			wantGN: `devicetree_visitor("my-visitor") {
	sources = [
		"my-visitor.cc",
		"my-visitor.h",
	]
	public_deps = [
		"//sdk/lib/driver/devicetree/visitors:driver-visitor",
	]
	deps = [
		"//sdk/lib/driver/logging/cpp",
	]
	non_sdk_deps = [
		"//sdk/lib/fidl/cpp",
	]
	defines = [
		"MY_DEFINE=1",
	]
	ldflags = [
		"-Wl,--no-undefined",
	]
	configs += [
		"//build/config:Wno-conversion",
	]
	configs -= [
		"//build/config:default_warnings",
	]
	enable_sdk_source_set = true
	category = "partner"
	output_name = "custom-visitor"
	testonly = true
	visibility = [
		":*",
		"//src/*",
	]
}`,
		},
		{
			name: "devicetree_visitor with select",
			bazel: `devicetree_visitor(
	name = "board-visitor",
	srcs = ["board-visitor.cc"],
	deps = select({
		"@platforms//cpu:arm64": ["//src/arm64:helper"],
		"//conditions:default": ["//src/other:helper"],
	}),
)`,
			wantGN: `devicetree_visitor("board-visitor") {
	sources = [
		"board-visitor.cc",
	]
	if (current_cpu == "arm64") {
		public_deps = [
			"//src/arm64:helper",
		]
	} else {
		public_deps = [
			"//src/other:helper",
		]
	}
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("FileToGN(%s) failed: %v", tc.bazel, err)
			}
			if diff := cmp.Diff(tc.wantGN, strings.Join(gotGN, "\n")); diff != "" {
				t.Errorf("Diff (-want +got):\n%s", diff)
			}
		})
	}
}

func TestDevicetreeVisitorToGNErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "unsupported additional_linker_inputs",
			bazel: `devicetree_visitor(
	name = "v",
	srcs = ["v.cc"],
	additional_linker_inputs = ["extra.ld"],
)`,
			wantErr: "additional_linker_inputs",
		},
		{
			name: "unsupported local_defines",
			bazel: `devicetree_visitor(
	name = "v",
	srcs = ["v.cc"],
	local_defines = ["LOCAL=1"],
)`,
			wantErr: "local_defines",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazel2gn.FileToGN(f); err == nil {
				t.Fatalf("FileToGN(%s) succeeded unexpectedly; want error containing %q", tc.bazel, tc.wantErr)
			} else if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("FileToGN(%s) error = %q, want substring %q", tc.bazel, err.Error(), tc.wantErr)
			}
		})
	}
}

func TestDtbToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "basic dtb",
			bazel: `dtb(
	name = "board-dtb",
	srcs = ["board.dts.S"],
	hdrs = ["board.dtsi"],
	deps = [":board-headers"],
	dtcflags = [
		"-@",
		"-Wno-interrupt_provider",
	],
	testonly = True,
	visibility = ["//boards:__subpackages__"],
)`,
			wantGN: `dtb("board-dtb") {
	sources = [
		"board.dts.S",
	]
	deps = [
		":board-headers",
	]
	dtcflags = [
		"-@",
		"-Wno-interrupt_provider",
	]
	testonly = true
	visibility = [
		":*",
		"//boards/*",
	]
}`,
		},
		{
			name: "dtb with resource srcs",
			bazel: `dtb(
	name = "my-dtb",
	srcs = ["subdir/my-board.dts.S"],
)

resource(
	name = "my-dtb-resource",
	srcs = [":my-dtb"],
	outputs = ["data/devicetree/my-board.dtb"],
	testonly = True,
)`,
			wantGN: `dtb("my-dtb") {
	sources = [
		"subdir/my-board.dts.S",
	]
}
resource("my-dtb-resource") {
	sources = [
		"$target_out_dir/my-board.dtb",
	]
	outputs = [
		"data/devicetree/my-board.dtb",
	]
	testonly = true
	deps = [
		":my-dtb",
	]
}`,
		},
		{
			name: "dtb with resource src",
			bazel: `dtb(
	name = "plain-dtb",
	srcs = ["plain.dts"],
)

resource(
	name = "plain-dtb-resource",
	src = ":plain-dtb",
	dest = "data/plain.dtb",
)`,
			wantGN: `dtb("plain-dtb") {
	sources = [
		"plain.dts",
	]
}
resource("plain-dtb-resource") {
	sources = [
		"$target_out_dir/plain.dtb",
	]
	outputs = [
		"data/plain.dtb",
	]
	deps = [
		":plain-dtb",
	]
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("FileToGN(%s) failed: %v", tc.bazel, err)
			}
			if diff := cmp.Diff(tc.wantGN, strings.Join(gotGN, "\n")); diff != "" {
				t.Errorf("Diff (-want +got):\n%s", diff)
			}
		})
	}
}
