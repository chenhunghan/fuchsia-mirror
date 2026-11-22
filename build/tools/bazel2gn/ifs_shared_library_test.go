// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestCcLinkStubFromIfsFileConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "ifs file in another package",
			bazel: `load("//build/bazel/rules/cc:shared_library.bzl", "cc_link_stub_from_ifs_file")

cc_link_stub_from_ifs_file(
    name = "driver_runtime",
    ifs_file = "//src/devices/bin/driver_runtime:libdriver_runtime.ifs",
    system_provided = True,
    target_compatible_with = ["@platforms//os:fuchsia"],
    visibility = ["//:__subpackages__"],
)
`,
			wantGN: `if (is_fuchsia) {
	ifs_shared_library("driver_runtime") {
		abi = "//src/devices/bin/driver_runtime/libdriver_runtime.ifs"
		visibility = [
			"//*",
		]
	}
}`,
		},
		{
			name: "stub_name and deps",
			bazel: `load("//build/bazel/rules/cc:shared_library.bzl", "cc_link_stub_from_ifs_file")

cc_link_stub_from_ifs_file(
    name = "libzircon_link_stub",
    ifs_file = "zircon.ifs",
    stub_name = "libzircon",
    system_provided = True,
    deps = [":headers"],
)
`,
			wantGN: `ifs_shared_library("libzircon_link_stub") {
	output_name = "zircon"
	abi = "zircon.ifs"
	public_deps = [
		":headers",
	]
}`,
		},
		{
			name: "output name from the ifs file",
			bazel: `load("//build/bazel/rules/cc:shared_library.bzl", "cc_link_stub_from_ifs_file")

cc_link_stub_from_ifs_file(
    name = "foo_stub",
    ifs_file = ":libfoo.ifs",
    system_provided = True,
)
`,
			wantGN: `ifs_shared_library("foo_stub") {
	output_name = "foo"
	abi = "libfoo.ifs"
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

func TestCcLinkStubFromIfsFileErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "system_provided not set",
			bazel: `cc_link_stub_from_ifs_file(
    name = "foo",
    ifs_file = "libfoo.ifs",
)
`,
			wantErr: "must set `system_provided = True`",
		},
		{
			name: "system_provided is False",
			bazel: `cc_link_stub_from_ifs_file(
    name = "foo",
    ifs_file = "libfoo.ifs",
    system_provided = False,
)
`,
			wantErr: "must set `system_provided = True`",
		},
		{
			name: "stub name without lib prefix",
			bazel: `cc_link_stub_from_ifs_file(
    name = "foo",
    ifs_file = "foo.ifs",
    system_provided = True,
)
`,
			wantErr: "names it `lib<output_name>.so`",
		},
		{
			name: "raw output_name",
			bazel: `# @bazel2gn:raw_attribute:output_name = "foo"
cc_link_stub_from_ifs_file(
    name = "foo",
    ifs_file = "libfoo.ifs",
    system_provided = True,
)
`,
			wantErr: "remove the `# @bazel2gn:raw_attribute:output_name` annotation",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Fatalf("Expected an error containing %q, got none", tc.wantErr)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("Expected an error containing %q, got: %v", tc.wantErr, err)
			}
		})
	}
}
