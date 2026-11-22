// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestParseConfigErrors(t *testing.T) {
	tests := []struct {
		name    string
		json    string
		wantErr string
	}{
		{
			name:    "invalid json",
			json:    `{`,
			wantErr: "parsing .bazel2gn.json",
		},
		{
			name:    "unknown top-level field",
			json:    `{"gn_import": ["//foo.gni"]}`,
			wantErr: `unknown field "gn_import"`,
		},
		{
			name:    "unknown rule field",
			json:    `{"rules": {"custom_rule": {"gn_template": "custom", "extra_field": true}}}`,
			wantErr: `unknown field "extra_field"`,
		},
		{
			name:    "trailing json value",
			json:    `{"gn_imports": ["//foo.gni"]} {}`,
			wantErr: "unexpected trailing JSON value",
		},
		{
			name:    "trailing json garbage",
			json:    `{"gn_imports": ["//foo.gni"]} garbage`,
			wantErr: "unexpected trailing JSON data",
		},
		{
			name:    "empty gn_import",
			json:    `{"gn_imports": [""]}`,
			wantErr: "gn_imports entries must not be empty",
		},
		{
			name:    "empty rule name",
			json:    `{"rules": {"": {"gn_template": "foo"}}}`,
			wantErr: "rule name in bazel2gn config must not be empty",
		},
		{
			name:    "dollar rule name",
			json:    `{"rules": {"$internal": {"gn_template": "foo"}}}`,
			wantErr: "must not start with '$'",
		},
		{
			name:    "conflicts with built-in rule",
			json:    `{"rules": {"fx_component": {"gn_template": "other"}}}`,
			wantErr: "conflicts with built-in bazel2gn rule",
		},
		{
			name:    "missing gn_template",
			json:    `{"rules": {"custom_rule": {}}}`,
			wantErr: "must specify a non-empty gn_template",
		},
		{
			name:    "unknown transformer",
			json:    `{"rules": {"custom_rule": {"gn_template": "custom", "attr_transformers": {"manifest": "bogus"}}}}`,
			wantErr: `unknown transformer "bogus"`,
		},
		{
			name:    "unsupported assignment op",
			json:    `{"rules": {"custom_rule": {"gn_template": "custom", "attr_assignment_ops": {"configs": "*="}}}}`,
			wantErr: `unsupported operator "*="`,
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			_, err := ParseConfig([]byte(tc.json))
			if err == nil {
				t.Fatalf("ParseConfig(%s) unexpectedly succeeded", tc.json)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("ParseConfig() error = %q, want substring %q", err.Error(), tc.wantErr)
			}
		})
	}
}

func TestLoadConfigForDirHierarchy(t *testing.T) {
	root := t.TempDir()
	sub := filepath.Join(root, "sub", "pkg")
	if err := os.MkdirAll(sub, 0o755); err != nil {
		t.Fatal(err)
	}

	if err := os.WriteFile(filepath.Join(root, "sub", ConfigFileName), []byte(`{
  "gn_imports": ["//sub/migration.gni"],
  "rules": {
    "custom_pkg": {
      "gn_template": "custom_pkg_gn",
      "attrs_to_omit": ["bazel_only"],
      "attr_map": {"extra_deps": "deps"},
      "attr_transformers": {"manifest": "file_paths", "extra_deps": "deps"},
      "unwrap_packaged_binary_attrs": ["deps"]
    }
  }
}`), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(sub, ConfigFileName), []byte(`{
  "gn_imports": ["//sub/migration.gni", "//sub/pkg/local.gni"],
  "rules": {
    "local_helper": {
      "gn_template": "__NO_GN_EQUIVALENT__"
    }
  }
}`), 0o644); err != nil {
		t.Fatal(err)
	}

	cfg, err := LoadConfigForDir(root, "sub/pkg")
	if err != nil {
		t.Fatalf("LoadConfigForDir() failed: %v", err)
	}
	if cfg == nil {
		t.Fatal("LoadConfigForDir() returned nil config")
	}
	wantImports := []string{"//sub/migration.gni", "//sub/pkg/local.gni"}
	if diff := cmp.Diff(wantImports, cfg.GNImports); diff != "" {
		t.Errorf("GNImports diff (-want +got):\n%s", diff)
	}
	if _, ok := cfg.Rules["custom_pkg"]; !ok {
		t.Error("expected merged config to contain custom_pkg")
	}
	if !cfg.isNoGNEquivalent("local_helper") {
		t.Error("expected local_helper to be noGNEquivalent")
	}
}

func TestFileToGNWithConfig(t *testing.T) {
	cfgJSON := `{
  "gn_imports": ["//custom/rules/migration.gni"],
  "rules": {
    "custom_component_pkg": {
      "gn_template": "custom_component_pkg",
      "attrs_to_omit": ["includes"],
      "attr_map": {"extra": "package_deps"},
      "attr_transformers": {
        "configs": "deps",
        "manifest": "file_paths",
        "init_script": "file_paths",
        "extra": "deps",
        "deps": "deps"
      },
      "attr_assignment_ops": {
        "configs": "=",
        "extra_configs": "+="
      },
      "extra_gn_expressions": ["from_config = true"],
      "default_gn_expressions": ["default_flag = false"],
      "unwrap_packaged_binary_attrs": ["deps"]
    },
    "custom_bazel_only": {
      "gn_template": "__NO_GN_EQUIVALENT__"
    }
  }
}`
	cfg, err := ParseConfig([]byte(cfgJSON))
	if err != nil {
		t.Fatalf("ParseConfig() failed: %v", err)
	}

	src := `load("//custom/rules:defs.bzl", "custom_bazel_only", "custom_component_pkg")
load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")
load("//build/bazel/rules/packages:fx_packaged_binary.bzl", "fx_packaged_binary")

fx_cc_binary(
    name = "svc_bin",
    srcs = ["main.cc"],
)

fx_packaged_binary(
    name = "svc_bin_packaged",
    binary = ":svc_bin",
    binary_name = "svc_bin",
)

custom_bazel_only(
    name = "ignored_helper",
)

# @bazel2gn:raw_attribute:default_flag = true
custom_component_pkg(
    name = "svc_pkg",
    deps = [":svc_bin_packaged"],
    configs = [":pkg_cfg"],
    extra = ["//other/pkg:sidecar"],
    extra_configs = [":my_cfg"],
    includes = ["//sdk/lib/syslog:client.shard"],
    init_script = "init.rc",
    manifest = "meta/svc.cml",
    visibility = ["//custom/consumers:__pkg__"],
)
`
	f, err := ParseBytes("custom/pkg/BUILD.bazel", []byte(src))
	if err != nil {
		t.Fatalf("ParseBytes() failed: %v", err)
	}
	gotLines, err := FileToGNWithConfig(f, "custom/pkg", cfg)
	if err != nil {
		t.Fatalf("FileToGNWithConfig() failed: %v", err)
	}
	got := strings.Join(gotLines, "\n")
	want := strings.Join([]string{
		`executable("svc_bin") {`,
		`	sources = [`,
		`		"main.cc",`,
		`	]`,
		`}`,
		`custom_component_pkg("svc_pkg") {`,
		`	deps = [`,
		`		":svc_bin",`,
		`	]`,
		`	configs = [`,
		`		":pkg_cfg",`,
		`	]`,
		`	package_deps = [`,
		`		"//other/pkg:sidecar",`,
		`	]`,
		`	extra_configs += [`,
		`		":my_cfg",`,
		`	]`,
		`	init_script = "init.rc"`,
		`	manifest = "meta/svc.cml"`,
		`	visibility = [`,
		`		":*",`,
		`		"//custom/consumers:*",`,
		`	]`,
		`	from_config = true`,
		`	default_flag = true`,
		`}`,
	}, "\n")
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("FileToGNWithConfig() diff (-want +got):\n%s", diff)
	}
}

func TestFileToGNWithConfigAttrOverridesAndAppendedDeps(t *testing.T) {
	cfgJSON := `{
  "rules": {
    "custom_rule": {
      "gn_template": "custom_gn",
      "attr_map": {
        "srcs": "files",
        "hdrs": "headers",
        "extra_deps": "deps",
        "remove_deps": "deps"
      },
      "attr_transformers": {
        "srcs": "file_paths",
        "hdrs": "file_paths",
        "extra_deps": "deps",
        "remove_deps": "deps"
      },
      "attr_assignment_ops": {
        "extra_deps": "+=",
        "remove_deps": "-="
      }
    }
  }
}`
	cfg, err := ParseConfig([]byte(cfgJSON))
	if err != nil {
		t.Fatalf("ParseConfig() failed: %v", err)
	}

	src := `load("//custom:defs.bzl", "custom_rule")

custom_rule(
    name = "with_deps",
    extra_deps = [":extra"],
    hdrs = ["a.h"],
    remove_deps = [":unwanted"],
    srcs = ["a.cc"],
    deps = [":base"],
)

custom_rule(
    name = "only_appended_deps",
    extra_deps = [":extra"],
)

custom_rule(
    name = "only_removed_deps",
    remove_deps = [":unwanted"],
)
`
	f, err := ParseBytes("custom/BUILD.bazel", []byte(src))
	if err != nil {
		t.Fatalf("ParseBytes() failed: %v", err)
	}
	gotLines, err := FileToGNWithConfig(f, "custom", cfg)
	if err != nil {
		t.Fatalf("FileToGNWithConfig() failed: %v", err)
	}
	got := strings.Join(gotLines, "\n")
	want := strings.Join([]string{
		`custom_gn("with_deps") {`,
		`	headers = [`,
		`		"a.h",`,
		`	]`,
		`	files = [`,
		`		"a.cc",`,
		`	]`,
		`	deps = [`,
		`		":base",`,
		`	]`,
		`	deps += [`,
		`		":extra",`,
		`	]`,
		`	deps -= [`,
		`		":unwanted",`,
		`	]`,
		`}`,
		`custom_gn("only_appended_deps") {`,
		`	deps = []`,
		`	deps += [`,
		`		":extra",`,
		`	]`,
		`}`,
		`custom_gn("only_removed_deps") {`,
		`	deps = []`,
		`	deps -= [`,
		`		":unwanted",`,
		`	]`,
		`}`,
	}, "\n")
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("FileToGNWithConfig() diff (-want +got):\n%s", diff)
	}
}

func TestFileToGNWithConfigDefaultAndExtraExpressions(t *testing.T) {
	cfgJSON := `{
  "rules": {
    "custom_rule": {
      "gn_template": "custom_gn",
      "attr_map": {
        "flag_override": "opt_in",
        "extra_opts": "opts"
      },
      "attr_assignment_ops": {
        "extra_opts": "+="
      },
      "default_gn_expressions": [
        "opt_in = false",
        "opts = [ \"--base\" ]",
        "assert(defined(target_name) && target_name == \"x\")"
      ],
      "extra_gn_expressions": [
        "fixed = true"
      ]
    }
  }
}`
	cfg, err := ParseConfig([]byte(cfgJSON))
	if err != nil {
		t.Fatalf("ParseConfig() failed: %v", err)
	}

	t.Run("default skipped, before, and after", func(t *testing.T) {
		src := `load("//custom:defs.bzl", "custom_rule")

custom_rule(
    name = "unconditional_override",
    flag_override = True,
)

custom_rule(
    name = "conditional_and_append",
    extra_opts = ["--extra"],
    opt_in = select({
        "@platforms//os:fuchsia": True,
    }),
)
`
		f, err := ParseBytes("custom/BUILD.bazel", []byte(src))
		if err != nil {
			t.Fatalf("ParseBytes() failed: %v", err)
		}
		gotLines, err := FileToGNWithConfig(f, "custom", cfg)
		if err != nil {
			t.Fatalf("FileToGNWithConfig() failed: %v", err)
		}
		got := strings.Join(gotLines, "\n")
		want := strings.Join([]string{
			`custom_gn("unconditional_override") {`,
			`	opt_in = true`,
			`	fixed = true`,
			`	opts = [ "--base" ]`,
			`	assert(defined(target_name) && target_name == "x")`,
			`}`,
			`custom_gn("conditional_and_append") {`,
			`	opt_in = false`,
			`	opts = [ "--base" ]`,
			`	opts += [`,
			`		"--extra",`,
			`	]`,
			`	if (is_fuchsia) {`,
			`		opt_in = true`,
			`	}`,
			`	fixed = true`,
			`	assert(defined(target_name) && target_name == "x")`,
			`}`,
		}, "\n")
		if diff := cmp.Diff(want, got); diff != "" {
			t.Errorf("FileToGNWithConfig() diff (-want +got):\n%s", diff)
		}
	})

	t.Run("extra collision with target attr returns error", func(t *testing.T) {
		src := `load("//custom:defs.bzl", "custom_rule")

custom_rule(
    name = "conflicting",
    fixed = False,
)
`
		f, err := ParseBytes("custom/BUILD.bazel", []byte(src))
		if err != nil {
			t.Fatalf("ParseBytes() failed: %v", err)
		}
		if _, err := FileToGNWithConfig(f, "custom", cfg); err == nil {
			t.Fatal("FileToGNWithConfig() unexpectedly succeeded on conflicting extra_gn_expressions attr")
		} else if !strings.Contains(err.Error(), "fixed = true") {
			t.Errorf("unexpected error: %v", err)
		}
	})
}

func TestGNAssignedName(t *testing.T) {
	tests := []struct {
		expr string
		want string
	}{
		{expr: "opt_in = false", want: "opt_in"},
		{expr: `sdk = "shared"`, want: "sdk"},
		{expr: `deps += [":a"]`, want: ""},
		{expr: `deps -= [":a"]`, want: ""},
		{expr: `assert(defined(foo))`, want: ""},
		{expr: `foo == bar`, want: ""},
		{expr: `if (x == 1) {}`, want: ""},
	}
	for _, tc := range tests {
		if got := gnAssignedName(tc.expr); got != tc.want {
			t.Errorf("gnAssignedName(%q) = %q, want %q", tc.expr, got, tc.want)
		}
	}
}
