// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestCompiledActionConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "output directory",
			bazel: `compiled_action(
    name = "generate",
    testonly = True,
    outs = [
        "a.far",
        "b.far",
    ],
    args = [
        "--output-dir",
        "$(RULEDIR)",
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	testonly = true
	outputs = [
		"$target_gen_dir/a.far",
		"$target_gen_dir/b.far",
	]
	args = [
		"--output-dir",
		rebase_path(target_gen_dir, root_build_dir),
	]
	tool = ":generator"
}`,
		},
		{
			name: "locations of inputs and outputs",
			bazel: `compiled_action(
    name = "generate",
    srcs = [
        "input.json",
        "//:root_pkg/third_party/foo/data.zip",
        "//path/to:other.txt",
    ],
    outs = ["gen/out.rs"],
    args = [
        "$(location :input.json)",
        "$(execpath //:root_pkg/third_party/foo/data.zip)",
        "--other=$(location //path/to:other.txt)",
        "--out=$(location :gen/out.rs)",
        "--rule-dir=$(RULEDIR)/gen",
    ],
    tool = "//path/to/tool:bin",
    visibility = ["//foo:__pkg__"],
)
`,
			wantGN: `compiled_action("generate") {
	sources = [
		"input.json",
		"//root_pkg/third_party/foo/data.zip",
		"//path/to/other.txt",
	]
	outputs = [
		"$target_gen_dir/gen/out.rs",
	]
	args = [
		rebase_path("input.json", root_build_dir),
		rebase_path("//root_pkg/third_party/foo/data.zip", root_build_dir),
		"--other=" + rebase_path("//path/to/other.txt", root_build_dir),
		"--out=" + rebase_path("$target_gen_dir/gen/out.rs", root_build_dir),
		"--rule-dir=" + rebase_path(target_gen_dir, root_build_dir) + "/gen",
	]
	tool = "//path/to/tool:bin"
	visibility = [
		":*",
		"//foo:*",
	]
}`,
		},
		{
			name: "outputs with path overwrite",
			bazel: `compiled_action(
    name = "generate",
    outs = [
        "out.rs",  # @bazel2gn:path_overwrite:$root_gen_dir/foo/out.rs
    ],
    args = [
        "$(location out.rs)",
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$root_gen_dir/foo/out.rs",
	]
	args = [
		rebase_path("$root_gen_dir/foo/out.rs", root_build_dir),
	]
	tool = ":generator"
}`,
		},
		{
			name: "literal arguments are escaped",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [
        "$HOME",
        "say \"hi\"",
        "a\\b",
        "",
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = [
		"\$HOME",
		"say \"hi\"",
		"a\\b",
		"",
	]
	tool = ":generator"
}`,
		},
		{
			name: "third-party tool",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :out.txt)"],
    tool = "//third_party/rust_crates/vendor:some-tool",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = [
		rebase_path("$target_gen_dir/out.txt", root_build_dir),
	]
	tool = "//third_party/rust_crates:some-tool"
}`,
		},
		{
			name: "raw overwrite of args",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :out.txt)"],  # @bazel2gn:raw_overwrite:rebase_path(outputs, root_build_dir)
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = rebase_path(outputs, root_build_dir)
	tool = ":generator"
}`,
		},
		{
			name: "raw overwrite of args element",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [
        "--out",
        "$(location :out.txt)",  # @bazel2gn:raw_overwrite:rebase_path(outputs[0], root_build_dir)
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = [
		"--out",
		rebase_path(outputs[0], root_build_dir),
	]
	tool = ":generator"
}`,
		},
		{
			name: "generate_epoch with cross-package tool and tool_output_name",
			bazel: `compiled_action(
    name = "generate_epoch",
    srcs = ["//src/sys/pkg/bin/system-updater/epoch:history"],
    outs = ["epoch.json"],
    args = [
        "--history",
        "$(location //src/sys/pkg/bin/system-updater/epoch:history)",
        "--output",
        "$(location :epoch.json)",
    ],
    tool = "//src/sys/pkg/bin/system-updater/epoch:generator",
    tool_output_name = "epoch_generator",
)
`,
			wantGN: `compiled_action("generate_epoch") {
	sources = [
		"//src/sys/pkg/bin/system-updater/epoch/history",
	]
	outputs = [
		"$target_gen_dir/epoch.json",
	]
	args = [
		"--history",
		rebase_path("//src/sys/pkg/bin/system-updater/epoch/history", root_build_dir),
		"--output",
		rebase_path("$target_gen_dir/epoch.json", root_build_dir),
	]
	tool = "//src/sys/pkg/bin/system-updater/epoch:generator"
	tool_output_name = "epoch_generator"
}`,
		},
		{
			name: "depfile, deps, mnemonic, testonly, visibility, and tags",
			bazel: `compiled_action(
    name = "generate_type_fingerprints",
    testonly = True,
    srcs = ["input.idl"],
    outs = ["comparisons.json"],
    args = [
        "--input",
        "$(execpath input.idl)",
        "--depfile",
        "$(location :type_fingerprints.d)",
        "--depfile-flag=$(execpath type_fingerprints.d)",
        "--comparisons=$(location comparisons.json)",
    ],
    depfile = "type_fingerprints.d",
    mnemonic = "FPRINT",
    tags = [
        "manual",
        "assert_no_deps=//third_party/rust_crates/vendor:regex",
    ],
    tool = "//src/storage/fxfs/tools:fxfs_tool",
    tool_output_name = "fxfs",
    visibility = ["//src/storage/fxfs:__subpackages__"],
    deps = [
        ":extra_inputs",
        "//third_party/rust_crates/vendor:serde_json",
    ],
)
`,
			wantGN: `compiled_action("generate_type_fingerprints") {
	testonly = true
	sources = [
		"input.idl",
	]
	outputs = [
		"$target_gen_dir/comparisons.json",
	]
	args = [
		"--input",
		rebase_path("input.idl", root_build_dir),
		"--depfile",
		rebase_path("$target_gen_dir/type_fingerprints.d", root_build_dir),
		"--depfile-flag=" + rebase_path("$target_gen_dir/type_fingerprints.d", root_build_dir),
		"--comparisons=" + rebase_path("$target_gen_dir/comparisons.json", root_build_dir),
	]
	depfile = "$target_gen_dir/type_fingerprints.d"
	mnemonic = "FPRINT"
	assert_no_deps = [
		"//third_party/rust_crates:regex",
	]
	tool = "//src/storage/fxfs/tools:fxfs_tool"
	tool_output_name = "fxfs"
	visibility = [
		":*",
		"//src/storage/fxfs/*",
	]
	deps = [
		":extra_inputs",
		"//third_party/rust_crates:serde_json",
	]
}`,
		},
		{
			name: "srcs, deps, and depfile with path overwrite",
			bazel: `compiled_action(
    name = "do_codegen",
    srcs = [
        ":arm64.rs",  # @bazel2gn:path_overwrite://src/starnix/lib/linux_uapi/src/arm64.rs
    ],
    outs = ["arm_manual_kgsl.rs"],
    args = [
        "$(location :arm64.rs)",
        "--in=$(execpath arm64.rs)",
        "--dep=$(location :generated_input)",
        "--dep-bare=$(execpath generated_input)",
        "--depfile=$(location :codegen.d)",
        "$(location :arm_manual_kgsl.rs)",
    ],
    depfile = "codegen.d",  # @bazel2gn:path_overwrite:$target_out_dir/codegen.d
    tool = ":kgsl_codegen",
    deps = [
        ":generated_input",  # @bazel2gn:path_overwrite:$target_gen_dir/generated_input.json
    ],
)
`,
			wantGN: `compiled_action("do_codegen") {
	sources = [
		"//src/starnix/lib/linux_uapi/src/arm64.rs",
	]
	outputs = [
		"$target_gen_dir/arm_manual_kgsl.rs",
	]
	args = [
		rebase_path("//src/starnix/lib/linux_uapi/src/arm64.rs", root_build_dir),
		"--in=" + rebase_path("//src/starnix/lib/linux_uapi/src/arm64.rs", root_build_dir),
		"--dep=" + rebase_path("$target_gen_dir/generated_input.json", root_build_dir),
		"--dep-bare=" + rebase_path("$target_gen_dir/generated_input.json", root_build_dir),
		"--depfile=" + rebase_path("$target_out_dir/codegen.d", root_build_dir),
		rebase_path("$target_gen_dir/arm_manual_kgsl.rs", root_build_dir),
	]
	depfile = "$target_out_dir/codegen.d"
	tool = ":kgsl_codegen"
	deps = [
		"$target_gen_dir/generated_input.json",
	]
}`,
		},
		{
			name: "args with select and list concatenation",
			bazel: `compiled_action(
    name = "generate",
    srcs = ["in.json"],
    outs = ["out.json"],
    args = [
        "--in",
        "$(location :in.json)",
    ] + select({
        "@platforms//os:fuchsia": ["--out=$(location :out.json)"],
        "//conditions:default": ["$(execpath out.json)"],
    }),
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	sources = [
		"in.json",
	]
	outputs = [
		"$target_gen_dir/out.json",
	]
	args = []
	args += [
		"--in",
		rebase_path("in.json", root_build_dir),
	]
	if (is_fuchsia) {
		args += [
			"--out=" + rebase_path("$target_gen_dir/out.json", root_build_dir),
		]
	} else {
		args += [
			rebase_path("$target_gen_dir/out.json", root_build_dir),
		]
	}
	tool = ":generator"
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel targets: %v", err)
			}
			if diff := cmp.Diff(tc.wantGN, gotGN); diff != "" {
				t.Errorf("Diff found after converting Bazel targets (-want +got):\n%s", diff)
			}
		})
	}
}

func TestCompiledActionConversionErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "unsupported make variable",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(SRCS)"],
    tool = ":generator",
)`,
		},
		{
			name: "unsupported plural location",
			bazel: `compiled_action(
    name = "generate",
    srcs = [":inputs"],
    outs = ["out.txt"],
    args = ["$(locations :inputs)"],
    tool = ":generator",
)`,
		},
		{
			name: "unterminated substitution",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :out.txt"],
    tool = ":generator",
)`,
		},
		{
			name: "external repository label",
			bazel: `compiled_action(
    name = "generate",
    srcs = ["@foo//:bar.txt"],
    outs = ["out.txt"],
    args = ["$(location @foo//:bar.txt)"],
    tool = ":generator",
)`,
		},
		{
			name: "label without target name",
			bazel: `compiled_action(
    name = "generate",
    srcs = ["//foo"],
    outs = ["out.txt"],
    args = ["$(location //foo)"],
    tool = ":generator",
)`,
		},
		{
			name: "args not a list literal",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ARGS,
    tool = ":generator",
)`,
		},
		{
			name: "non-literal arg",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [ARG],
    tool = ":generator",
)`,
		},
		{
			name: "outs not a list literal",
			bazel: `compiled_action(
    name = "generate",
    outs = OUTS,
    args = [],
    tool = ":generator",
)`,
		},
		{
			name: "unsupported attribute",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [],
    cmd = "echo hi",
    tool = ":generator",
)`,
		},
		{
			name: "depfile not a string literal",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [],
    depfile = ["out.d"],
    tool = ":generator",
)`,
		},
		{
			name: "tool not a string literal",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [],
    tool = [":generator"],
)`,
		},
		{
			name: "empty tool_output_name",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [],
    tool = ":generator",
    tool_output_name = "",
)`,
		},
		{
			name: "empty mnemonic",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [],
    mnemonic = "",
    tool = ":generator",
)`,
		},
		{
			name: "location of deps entry without path_overwrite",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :extra_inputs)"],
    tool = ":generator",
    deps = [":extra_inputs"],
)`,
		},
		{
			name: "execpath of bare deps entry without path_overwrite",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(execpath extra_inputs)"],
    tool = ":generator",
    deps = [":extra_inputs"],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Errorf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
			}
		})
	}
}
