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

func TestCmcMergeFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "default and custom output_name with cml, json, and shard srcs",
			bazel: `cmc_merge(
	name = "merged_manifest",
	testonly = True,
	srcs = [
		"meta/base.cml",
		":meta/extra.shard.cml",
		"//sdk/lib/syslog:client.shard",
		"//src/foo:config.json",
	],
	visibility = ["//src/foo:__pkg__"],
)

cmc_merge(
	name = "custom_out",
	output_name = "nested/merged.json",
	srcs = ["a.json", "b.json"],
)`,
			wantGN: `cmc_merge("merged_manifest") {
	testonly = true
	sources = [
		"meta/base.cml",
		"meta/extra.shard.cml",
		"//sdk/lib/syslog/client.shard.cml",
		"//src/foo/config.json",
	]
	output_name = "merged_manifest.cml"
	visibility = [
		":*",
		"//src/foo:*",
	]
}
cmc_merge("custom_out") {
	output_name = "nested/merged.json"
	sources = [
		"a.json",
		"b.json",
	]
}`,
		},
		{
			name: "manifest and chained cmc_merge consumers",
			bazel: `cmc_merge(
	name = "base_merge",
	srcs = ["meta/a.cml", "meta/b.cml"],
)

cmc_merge(
	name = "outer_merge",
	output_name = "custom_outer.cml",
	srcs = [":base_merge", "//sdk/lib/syslog:client.shard"],
)

fx_component(
	name = "comp",
	compiled_manifest = ":comp_manifest",
)

fx_component_manifest(
	name = "comp_manifest",
	component_name = "my-comp",
	manifest = ":outer_merge",
)

fx_compiled_manifest(
	name = "raw_cm",
	manifest = ":base_merge",
)

fx_component_manifest_with_expectations(
	name = "expect_manifest",
	manifest = ":outer_merge",
)`,
			wantGN: `cmc_merge("base_merge") {
	sources = [
		"meta/a.cml",
		"meta/b.cml",
	]
	output_name = "base_merge.cml"
}
cmc_merge("outer_merge") {
	output_name = "custom_outer.cml"
	sources = [
		"$target_out_dir/base_merge.cml",
		"//sdk/lib/syslog/client.shard.cml",
	]
	deps = [
		":base_merge",
	]
}
fuchsia_component_manifest("comp_manifest") {
	component_name = "my-comp"
	manifest = "$target_out_dir/custom_outer.cml"
	deps = [
		":outer_merge",
	]
}
fuchsia_component("comp") {
	cm_label = ":comp_manifest"
}
cm("raw_cm") {
	manifest = "$target_out_dir/base_merge.cml"
	output_name = "cml/raw_cm/raw_cm.cm"
	config_values_package_path = "meta/raw_cm.cvf"
	deps = [
		":base_merge",
	]
}
fuchsia_component_manifest_with_expectations("expect_manifest") {
	manifest = "$target_out_dir/custom_outer.cml"
	deps = [
		":outer_merge",
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

func TestCmcMergeErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name:  "missing srcs",
			bazel: `cmc_merge(name = "merged")`,
		},
		{
			name:  "empty srcs",
			bazel: `cmc_merge(name = "merged", srcs = [])`,
		},
		{
			name:  "unsupported attribute",
			bazel: `cmc_merge(name = "merged", srcs = ["a.cml"], from_file = "list.txt")`,
		},
		{
			name:  "cross-package non-file target in srcs",
			bazel: `cmc_merge(name = "merged", srcs = ["//other/pkg:target"])`,
		},
		{
			name: "referencing skipped cmc_merge",
			bazel: `# @bazel2gn:skip
cmc_merge(name = "merged", srcs = ["a.cml", "b.cml"])
fx_component_manifest(name = "m", manifest = ":merged")`,
		},
		{
			name: "fx_component_manifest referencing json cmc_merge",
			bazel: `cmc_merge(name = "merged", output_name = "out.json", srcs = ["a.json"])
fx_component_manifest(name = "m", manifest = ":merged")`,
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
