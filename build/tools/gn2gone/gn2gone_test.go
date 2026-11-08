// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
)

// findGN locates gn: from $GN2GONE_TEST_GN (e.g. when running `go test`
// directly), or as test data staged next to the test binary by
// //build/tools/gn2gone:gn2gone_test_data.
func findGN(t *testing.T) string {
	t.Helper()
	if p := os.Getenv("GN2GONE_TEST_GN"); p != "" {
		return p
	}
	hostDir := map[string]string{"arm64": "host_arm64", "amd64": "host_x64"}[runtime.GOARCH]
	p, err := filepath.Abs(filepath.Join(hostDir, "test_data", "gn2gone", "gn"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(p); err != nil {
		t.Fatalf("can't find gn test data (%v); set $GN2GONE_TEST_GN", err)
	}
	return p
}

func writeTree(t *testing.T, root string, files map[string]string) {
	t.Helper()
	for p, content := range files {
		full := filepath.Join(root, p)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
	}
}

const sentinel = bazel2gn.SentinelComment + "\n\nimport(\"//build/tools/bazel2gn/bazel_migration.gni\")\n\n"

func fixtureTree() map[string]string {
	return map[string]string{
		".gn": `buildconfig = "//build/BUILDCONFIG.gn"
default_args = {
  dir_pw = "//"
}
`,
		// READY, DELETE_BUILD_GN: only an assert and a test group above, and
		// the test is wired through //tools:host_tests.
		"lib/ready/BUILD.gn": `assert(is_host, "These targets are only compiled on host.")

group("tests") {
  testonly = true
  deps = [ ":ready_test($host_toolchain)" ]
}

` + sentinel + `go_library("ready") {
  sources = [ "ready.go" ]
}

go_test("ready_test") {
  library = ":ready"
}
`,
		"lib/ready/BUILD.bazel": `go_library(name = "ready")

go_test(name = "ready_test")
`,
		// No sentinel: parent test group.
		"lib/BUILD.gn": `group("tests") {
  testonly = true
  deps = [
    "other:tests",
    "ready:tests",
    "unwired:tests",
  ]
}
`,
		// BLOCKED through a template subtarget.
		"lib/blocked/BUILD.gn": sentinel + `rustc_library("blocked") {
  sources = [ "lib.rs" ]
}
`,
		"app/BUILD.gn": `# A comment mentioning "//lib/ready:ready" must not count.
executable("app") {
  deps = [ "//lib/blocked:blocked_rust" ]
}
`,
		// BLOCKED_TESTS_UNWIRED: no bazel_test_suite reaches the test.
		"lib/unwired/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":unwired_test" ]
}

` + sentinel + `go_library("unwired") {
}

go_test("unwired_test") {
}
`,
		"tools/BUILD.gn": `bazel_test_suite("bazel_tests") {
  target_tests = [
    "//lib/dual:d_tests",
    "//lib/opaque:o_test",
  ]
  host_tests = [
    "//lib/dt/tests:dt_test",
    "//lib/manualt:all",
    "//lib/tagged:all",
    "//lib/varsuite:suite",
    "//tools:host_tests",
  ]
}
`,
		"tools/BUILD.bazel": `test_suite(
    name = "host_tests",
    tests = ["//lib/ready:ready_test"],
)
`,
		// READY, STRIP_BELOW_SENTINEL: a live host tool shim above.
		"lib/strip/BUILD.gn": `bazel_host_tool("strip") {
  bazel_target = ":strip_bin"
}

` + sentinel + `go_binary("strip_bin") {
}
`,
		"bundles/BUILD.gn": `group("tools") {
  deps = [ "//lib/strip" ]
}
`,
		// EMPTY_SENTINEL.
		"lib/empty/BUILD.gn": `source_set("empty") {
}

` + sentinel,
		// BLOCKED through a variable from //.gn default_args.
		"lib/viavar/BUILD.gn": sentinel + `source_set("viavar") {
}
`,
		"third_party/pw/BUILD.gn": `group("x") {
  public_deps = [ "$dir_pw/lib/viavar" ]
}
`,
		// BLOCKED by a test group that bazel2gn generated (from a Bazel
		// test_suite) below another directory's sentinel. That entry can't be
		// edited, so //lib/suite must be unsynced first.
		"lib/suite/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "//lib/chained:chained_test" ]
}
`,
		"lib/chained/BUILD.gn": sentinel + `go_test("chained_test") {
}
`,
		// BLOCKED_TESTS_UNWIRED: a test_suite whose `tests` is a variable
		// doesn't count as "every test in the package".
		"lib/varsuite/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":vs_test" ]
}

` + sentinel + `go_test("vs_test") {
}
`,
		"lib/varsuite/BUILD.bazel": `TESTS = [":other_test"]

test_suite(
    name = "suite",
    tests = TESTS,
)

go_test(name = "vs_test")
`,
		// BLOCKED through a relative label in a .gni, used by the BUILD.gn
		// next to it (like //src/developer/ffx/lib/ffx_libraries_list.gni).
		"lib/ffx/libs.gni": `all_libs = [ "termio" ]
`,
		"lib/ffx/BUILD.gn": `import("libs.gni")
group("lib") {
  deps = []
  foreach(lib, all_libs) {
    deps += [ lib ]
  }
}
`,
		"lib/ffx/termio/BUILD.gn": sentinel + `rustc_library("termio") {
}
`,
		// BLOCKED_TESTS_UNWIRED: Bazel's implicit "every test in the
		// package" skips tests tagged "manual".
		"lib/manualt/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":m_test" ]
}

` + sentinel + `go_test("m_test") {
}
`,
		"lib/manualt/BUILD.bazel": `test_suite(name = "all")

go_test(
    name = "m_test",
    tags = ["manual"],
)
`,
		// BLOCKED_TESTS_UNWIRED: a test_suite with a `tags` filter selects a
		// subset, not every test.
		"lib/tagged/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":t_test" ]
}

` + sentinel + `go_test("t_test") {
}
`,
		"lib/tagged/BUILD.bazel": `test_suite(
    name = "all",
    tags = ["-flaky"],
)

go_test(name = "t_test")
`,
		// READY, but STRIP_BELOW_SENTINEL: the test group also pulls in tests
		// from a variable, which deleting BUILD.gn would drop from CQ.
		"lib/opaque/BUILD.gn": `import("//build/extra_tests.gni")

group("tests") {
  testonly = true
  deps = [ ":o_test" ] + extra_tests
}

` + sentinel + `go_test("o_test") {
}
`,
		// BLOCKED_DUAL_TESTED: //lib/dt/tests has no GN consumers, but GN
		// runs its test of //lib/dt, which GN still builds (//app2 uses it).
		"lib/dt/BUILD.gn": sentinel + `go_library("dt") {
}
`,
		"app2/BUILD.gn": `executable("app2") {
  deps = [ "//lib/dt" ]
}
`,
		"lib/dt/tests/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":dt_test" ]
}

` + sentinel + `go_test("dt_test") {
  deps = [ "//lib/dt" ]
}
`,
		// BLOCKED_TESTS_UNWIRED: the same test runs on host and on device in
		// GN, but Bazel only runs the device package (target_tests). The host
		// copy must not count as wired.
		"lib/dual/BUILD.gn": `group("tests") {
  testonly = true
  deps = [
    ":d_test($host_toolchain)",
    ":d_tests",
  ]
}

` + sentinel + `rustc_test("d_test") {
}

fuchsia_unittest_package("d_tests") {
  deps = [ ":d_test" ]
}
`,
		"lib/dual/BUILD.bazel": `rustc_test(name = "d_test")

fx_unittest_package(
    name = "d_tests",
    unit_tests = [":d_test"],
)

wrap_host_rust_test(
    name = "d_host_test",
    binary_name = "d_test",
)
`,
		// BLOCKED_DUAL_TESTED without a GN test group: the synced test of
		// //lib/media (still built by GN) isn't run by GN yet because its GN
		// package is still skipped, but its GN copy must stay.
		"lib/media/BUILD.gn": sentinel + `source_set("media") {
}
`,
		"app3/BUILD.gn": `executable("app3") {
  deps = [ "//lib/media" ]
}
`,
		"lib/media/unit_tests/BUILD.gn": sentinel + `executable("m_unit_test_bin") {
  testonly = true
  deps = [ "//lib/media" ]
}
`,
		// BLOCKED: a same-directory relative label (".:target").
		"lib/samedir/BUILD.gn": `group("samedir") {
  deps = [ ".:samedir_bin" ]
}

` + sentinel + `executable("samedir_bin") {
}
`,
		// Dynamic labels with no usable static prefix are reported.
		"build/dynamic.gni": `template("dyn") {
  group(target_name) {
    deps = [ "//$invoker.dir" ]
  }
}
`,
	}
}

func load(t *testing.T, root string) *Index {
	t.Helper()
	idx, stats, err := Load(context.Background(), LoadOptions{Root: root, GNBin: findGN(t), BatchSize: 3, Workers: 2})
	if err != nil {
		t.Fatalf("Load() error: %v", err)
	}
	if len(stats.ParseErrors) > 0 {
		t.Fatalf("Load() parse errors: %v", stats.ParseErrors)
	}
	return idx
}

func TestScan(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	idx := load(t, root)

	type result struct {
		State     State
		Mode      Mode
		Unwired   []string
		UnderTest []string
		Blockers  []string
	}
	got := map[string]result{}
	for _, r := range idx.ScanAll() {
		res := result{State: r.State, Mode: r.Mode, Unwired: r.UnwiredTests, UnderTest: r.UnderTest}
		for _, b := range r.Blockers {
			res.Blockers = append(res.Blockers, b.String())
		}
		got[r.Dir] = res
	}
	want := map[string]result{
		"lib/ready":   {State: StateReady, Mode: ModeDeleteBuildGN},
		"lib/strip":   {State: StateReady, Mode: ModeStripBelowSentinel},
		"lib/empty":   {State: StateEmptySentinel},
		"lib/unwired": {State: StateBlockedTestsUnwired, Unwired: []string{"unwired_test"}},
		"lib/blocked": {State: StateBlocked, Blockers: []string{
			`app/BUILD.gn:3 executable("app") deps "//lib/blocked:blocked_rust" -> :blocked [subtarget]`,
		}},
		"lib/viavar": {State: StateBlocked, Blockers: []string{
			`third_party/pw/BUILD.gn:2 group("x") public_deps "$dir_pw/lib/viavar" -> :viavar [var_substituted]`,
		}},
		"lib/chained": {State: StateBlocked, Blockers: []string{
			`lib/suite/BUILD.gn:7 group("tests") deps "//lib/chained:chained_test" -> :chained_test (synced in //lib/suite)`,
		}},
		"lib/suite":            {State: StateReady, Mode: ModeDeleteBuildGN},
		"lib/varsuite":         {State: StateBlockedTestsUnwired, Unwired: []string{"vs_test"}},
		"lib/opaque":           {State: StateReady, Mode: ModeStripBelowSentinel},
		"lib/dt/tests":         {State: StateBlockedDualTested, UnderTest: []string{"//lib/dt:dt"}},
		"lib/dual":             {State: StateBlockedTestsUnwired, Unwired: []string{"d_test (host)"}},
		"lib/media/unit_tests": {State: StateBlockedDualTested, UnderTest: []string{"//lib/media:media"}},
		"lib/media": {State: StateBlocked, Blockers: []string{
			`app3/BUILD.gn:2 executable("app3") deps "//lib/media" -> :media`,
			`lib/media/unit_tests/BUILD.gn:7 executable("m_unit_test_bin") deps "//lib/media" -> :media (synced in //lib/media/unit_tests)`,
		}},
		"lib/samedir": {State: StateBlocked, Blockers: []string{
			`lib/samedir/BUILD.gn:2 group("samedir") deps ".:samedir_bin" -> :samedir_bin`,
		}},
		"lib/dt": {State: StateBlocked, Blockers: []string{
			`app2/BUILD.gn:2 executable("app2") deps "//lib/dt" -> :dt`,
			`lib/dt/tests/BUILD.gn:11 go_test("dt_test") deps "//lib/dt" -> :dt (synced in //lib/dt/tests)`,
		}},
		"lib/manualt": {State: StateBlockedTestsUnwired, Unwired: []string{"m_test"}},
		"lib/tagged":  {State: StateBlockedTestsUnwired, Unwired: []string{"t_test"}},
		"lib/ffx/termio": {State: StateBlocked, Blockers: []string{
			`lib/ffx/libs.gni:1 all_libs "termio" -> (any target)`,
		}},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("ScanAll() diff (-want +got):\n%s", diff)
	}

	var unattributed []string
	for _, l := range idx.Resolver.Unattributed() {
		unattributed = append(unattributed, l.File+" "+l.Value)
	}
	if diff := cmp.Diff([]string{"build/dynamic.gni //$invoker.dir"}, unattributed); diff != "" {
		t.Errorf("Unattributed() diff (-want +got):\n%s", diff)
	}
}

func TestDynamicLabels(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"lib/ready/BUILD.gn": sentinel + `source_set("ready") {
}
`,
		"lib/other/BUILD.gn": sentinel + `source_set("other") {
}
`,
		"app/BUILD.gn": `group("app") {
  deps = [
    # The dynamic part continues the last path component.
    "//lib/rea$x",
    # Only the target name is dynamic.
    "//lib/other:$y",
  ]
  data_deps = [ "//lib/re" + z ]
}
`,
		// Relative labels with a dynamic part.
		"lib/BUILD.gn": `group("g") {
  deps = [
    "rea$x:tests",
    "oth" + y,
  ]
}

# A list of directories turned into "$t:bin" labels: the bare entry may
# name any target in the directory, not just the default one.
foreach(t, [ "viafor" ]) {
  print("$t:bin")
}
`,
		"lib/viafor/BUILD.gn": `group("viafor") {
}

` + sentinel + `executable("bin") {
}
`,
	})
	idx := load(t, root)
	got := map[string][]string{}
	for _, r := range idx.ScanAll() {
		for _, b := range r.Blockers {
			got[r.Dir] = append(got[r.Dir], b.String())
		}
	}
	want := map[string][]string{
		"lib/ready": {
			`app/BUILD.gn:4 group("app") deps "//lib/rea$x" -> (any target) [dynamic_prefix]`,
			`app/BUILD.gn:8 group("app") data_deps "//lib/re" -> (any target) [dynamic_prefix]`,
			`lib/BUILD.gn:3 group("g") deps "rea$x:tests" -> (any target) [dynamic_prefix]`,
		},
		"lib/other": {
			`app/BUILD.gn:6 group("app") deps "//lib/other:$y" -> (any target) [dynamic_prefix]`,
			`lib/BUILD.gn:4 group("g") deps "oth" -> (any target) [dynamic_prefix]`,
		},
		"lib/viafor": {
			`lib/BUILD.gn:10 "viafor" -> (any target)`,
		},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("blockers diff (-want +got):\n%s", diff)
	}
	var unattributed []string
	for _, l := range idx.Resolver.Unattributed() {
		unattributed = append(unattributed, l.Value)
	}
	if diff := cmp.Diff([]string(nil), unattributed); diff != "" {
		t.Errorf("Unattributed() diff (-want +got):\n%s", diff)
	}
}

// Like //sdk/lib/c/libc.gni's `".$suffix"`: a trailing "." is the directory
// itself, so the string must not match a sibling sharing the name as a prefix
// (//sdk/lib/component), only directories under it.
func TestTrailingDot(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"sdk/c/libc.gni": `subtarget = ".$suffix"
`,
		"sdk/c/sub/BUILD.gn": sentinel + `source_set("sub") {
}
`,
		"sdk/component/BUILD.gn": sentinel + `source_set("component") {
}
`,
	})
	idx := load(t, root)
	got := map[string]State{}
	for _, r := range idx.ScanAll() {
		got[r.Dir] = r.State
	}
	want := map[string]State{"sdk/c/sub": StateBlocked, "sdk/component": StateReady}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("states diff (-want +got):\n%s", diff)
	}
}

func TestQuery(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	idx := load(t, root)

	refs, err := idx.Query("//lib/ready:ready_test")
	if err != nil {
		t.Fatalf("Query() error: %v", err)
	}
	var got []string
	for _, r := range refs {
		got = append(got, r.String())
	}
	want := []string{`lib/ready/BUILD.gn:5 group("tests") deps ":ready_test($host_toolchain)" -> :ready_test`}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("Query() diff (-want +got):\n%s", diff)
	}

	if _, err := idx.Query("//app"); err == nil {
		t.Errorf("Query(//app) succeeded, want error for a directory without the sentinel")
	}
	if _, err := idx.Query("//lib/ready:nope"); err == nil {
		t.Errorf("Query(//lib/ready:nope) succeeded, want error for an unknown target")
	}
}

func TestUnsync(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	idx := load(t, root)

	plan := idx.PlanUnsync(idx.ScanAll())
	if diff := cmp.Diff([]string{"lib/opaque", "lib/ready", "lib/strip", "lib/suite"}, plan.Dirs); diff != "" {
		t.Errorf("plan.Dirs diff (-want +got):\n%s", diff)
	}
	if err := plan.Apply(context.Background(), root, findGN(t)); err != nil {
		t.Fatalf("Apply() error: %v", err)
	}

	if _, err := os.Stat(filepath.Join(root, "lib/ready/BUILD.gn")); !os.IsNotExist(err) {
		t.Errorf("lib/ready/BUILD.gn still exists, want deleted (err=%v)", err)
	}
	read := func(p string) string {
		b, err := os.ReadFile(filepath.Join(root, p))
		if err != nil {
			t.Fatal(err)
		}
		return string(b)
	}
	if diff := cmp.Diff(`group("tests") {
  testonly = true
  deps = [
    "other:tests",
    "unwired:tests",
  ]
}
`, read("lib/BUILD.gn")); diff != "" {
		t.Errorf("lib/BUILD.gn diff (-want +got):\n%s", diff)
	}
	if diff := cmp.Diff(`bazel_host_tool("strip") {
  bazel_target = ":strip_bin"
}
`, read("lib/strip/BUILD.gn")); diff != "" {
		t.Errorf("lib/strip/BUILD.gn diff (-want +got):\n%s", diff)
	}

	// Unsyncing //lib/suite unblocks //lib/chained on the next run.
	// Unsynced directories are no longer sentinel directories, so they're
	// not planned again.
	idx = load(t, root)
	plan = idx.PlanUnsync(idx.ScanAll())
	if diff := cmp.Diff([]string{"lib/chained"}, plan.Dirs); diff != "" {
		t.Errorf("second PlanUnsync() dirs diff (-want +got):\n%s", diff)
	}
}

func TestRemoveEntry(t *testing.T) {
	for _, tc := range []struct {
		line, literal, want string
		drop, wantErr       bool
	}{
		{line: `    ":foo",`, literal: ":foo", drop: true},
		{line: `    ":foo"  # comment`, literal: ":foo", drop: true},
		{line: `  deps = [ ":foo($host_toolchain)" ]`, literal: ":foo($host_toolchain)", want: `  deps = [ ]`},
		{line: `  deps = [ ":foo", ":bar" ]`, literal: ":foo", wantErr: true},
	} {
		got, drop, err := removeEntry(tc.line, tc.literal)
		if (err != nil) != tc.wantErr {
			t.Errorf("removeEntry(%q, %q) error = %v, wantErr %v", tc.line, tc.literal, err, tc.wantErr)
			continue
		}
		if drop != tc.drop || strings.TrimSpace(got) != strings.TrimSpace(strings.ReplaceAll(tc.want, "[ ]", "[]")) {
			t.Errorf("removeEntry(%q, %q) = (%q, %v), want (%q, %v)", tc.line, tc.literal, got, drop, tc.want, tc.drop)
		}
	}
}

// A string appended to an existing value can't start a label, so it must not
// block anything (like //build/test.gni's `_output_file += "." + ext`, which
// used to block every sentinel directory under //build). An absolute label
// is kept even then.
func TestSuffixLiterals(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"build/test.gni": `template("t") {
  _output_file = target_name
  _output_file += "." + _output_extension
  _label = invoker.label
  _label += ".$suffix"
  _tests = invoker.dir + ":tests"
  _abs = ""
  _abs += "//build/tools/abs:abs"
}
`,
		"build/tools/merge/BUILD.gn": sentinel + `executable("merge") {
}
`,
		"build/tools/abs/BUILD.gn": sentinel + `executable("abs") {
}
`,
		// Entries of a list appended with += are separate values: they
		// still resolve.
		"build/tools/BUILD.gn": `group("tools") {
  deps = []
  deps += [ "rel:rel" ]
  deps += [ "//build/tools/abslist:abslist" ]
}
`,
		"build/tools/rel/BUILD.gn": sentinel + `executable("rel") {
}
`,
		"build/tools/abslist/BUILD.gn": sentinel + `executable("abslist") {
}
`,
	})
	idx := load(t, root)
	got := map[string]State{}
	for _, r := range idx.ScanAll() {
		got[r.Dir] = r.State
	}
	want := map[string]State{
		"build/tools/merge":   StateReady,
		"build/tools/abs":     StateBlocked,
		"build/tools/rel":     StateBlocked,
		"build/tools/abslist": StateBlocked,
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("states diff (-want +got):\n%s", diff)
	}
}

// A directory with its own library is assumed to test that library, even if
// its tests also depend on a GN-built library in an ancestor directory.
func TestDualTestedOwnLibrary(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"lib/a/BUILD.gn": sentinel + `go_library("a") {
}
`,
		"app/BUILD.gn": `executable("app") {
  deps = [ "//lib/a" ]
}
`,
		// Has its own library: not BLOCKED_DUAL_TESTED.
		"lib/a/sub/BUILD.gn": sentinel + `go_library("sub") {
  deps = [ "//lib/a" ]
}

go_test("sub_test") {
  deps = [
    ":sub",
    "//lib/a",
  ]
}
`,
		// Only tests: they test //lib/a.
		"lib/a/tests/BUILD.gn": sentinel + `go_test("a_test") {
  deps = [ "//lib/a" ]
}
`,
		// A helper binary isn't a library: the tests still test //lib/a.
		"lib/a/withbin/BUILD.gn": sentinel + `go_binary("helper") {
}

go_test("withbin_test") {
  deps = [ "//lib/a" ]
}
`,
		// Tests of hand-written GN libraries (kept in both build systems,
		// so with no sentinel) stay in GN too: one in a parent directory
		// (including via a subtarget like :hw_cpp)...
		"lib/hw/BUILD.gn": `fidl("hw") {
}
`,
		"lib/hw/test/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":hw_test" ]
}

` + sentinel + `go_test("hw_test") {
  deps = [ "//lib/hw:hw_cpp" ]
}
`,
		// ...one above the sentinel in an ancestor sentinel directory...
		"lib/anc/BUILD.gn": `source_set("anc_hw") {
}

` + sentinel + `go_library("anc_unused") {
}
`,
		"lib/anc/test/BUILD.gn": sentinel + `go_test("anc_test") {
  deps = [ "//lib/anc:anc_hw" ]
}
`,
		// ...and one above the sentinel in the same directory.
		"lib/mock/BUILD.gn": `source_set("mock") {
}

` + sentinel + `go_test("mock_test") {
  deps = [ ":mock" ]
}
`,
		// A private helper library above the sentinel that nothing else uses
		// is scaffolding, not GN-built code under test.
		"lib/privabove/BUILD.gn": `source_set("scaffolding") {
  visibility = [ ":*" ]
}

` + sentinel + `go_test("priv_test") {
  deps = [ ":scaffolding" ]
}
`,
		// A private library is test scaffolding, not the directory's own
		// library: the tests still test //lib/a, including in a sibling
		// test directory (where private scaffolding must not clear testOnly).
		"lib/a/wrapper/BUILD.gn": sentinel + `go_library("wrapper") {
  deps = [ "//lib/a" ]
  visibility = [ ":*" ]
}

go_test("wrapper_test") {
  deps = [ ":wrapper" ]
}
`,
		"lib/a-tests/BUILD.gn": sentinel + `rustc_library("wrapper") {
  deps = [ "//lib/a" ]
  visibility = [ ":*" ]
}

rustc_test("wrapper_test") {
  deps = [ ":wrapper" ]
}
`,
		// Hand-written Rust proc-macro in a sibling directory referenced via
		// proc_macro_deps still blocks the sibling test directory.
		"lib/pm/BUILD.gn": `rustc_macro("pm") {
}
`,
		"lib/pm-tests/BUILD.gn": sentinel + `rustc_test("pm_test") {
  proc_macro_deps = [ "//lib/pm" ]
}
`,
		// rustc_staticlib counts as a library (own library -> READY), whereas
		// fuchsia_library_fuzzer without testonly does not.
		"lib/a/staticlib/BUILD.gn": sentinel + `rustc_staticlib("c_api") {
  deps = [ "//lib/a" ]
}

go_test("c_api_test") {
  deps = [ ":c_api" ]
}
`,
		"lib/a/fuzzer/BUILD.gn": sentinel + `fuchsia_library_fuzzer("a_fuzzer") {
  deps = [ "//lib/a" ]
}

go_test("fuzzer_smoke_test") {
  deps = [ "//lib/a" ]
}
`,
	})
	idx := load(t, root)
	got := map[string]State{}
	for _, r := range idx.ScanAll() {
		got[r.Dir] = r.State
	}
	want := map[string]State{
		"lib/a":           StateBlocked,
		"lib/a-tests":     StateBlockedDualTested,
		"lib/a/fuzzer":    StateBlockedDualTested,
		"lib/a/staticlib": StateReady,
		"lib/a/sub":       StateReady,
		"lib/a/tests":     StateBlockedDualTested,
		"lib/anc":         StateReady,
		"lib/anc/test":    StateBlockedDualTested,
		"lib/hw/test":     StateBlockedDualTested,
		"lib/mock":        StateBlockedDualTested,
		"lib/pm-tests":    StateBlockedDualTested,
		"lib/privabove":   StateReady,
		"lib/a/withbin":   StateBlockedDualTested,
		"lib/a/wrapper":   StateBlockedDualTested,
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("states diff (-want +got):\n%s", diff)
	}
}

// GN test group entries that name synced aggregates: a Bazel alias or
// test_suite counts as wired when the tests behind it are, but a synced group
// that also reaches hand-written GN tests can't be unsynced: GN would stop
// running them.
func TestTestGroupAggregates(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"BUILD.gn": `group("root_gn_tests") {
  testonly = true
}
`,
		"tests/BUILD.gn": `group("tests") {
  testonly = true
  deps = [
    "//lib/agg:tests",
    "//lib/al:host_tests",
    "//lib/chain:tests",
    "//lib/cycle:tests",
    "//lib/dynagg:tests",
    "//lib/hostagg:all_tests",
    "//lib/hostbad:all_tests",
    "//lib/opaqueagg:tests",
    "//lib/overagg:tests",
    "//lib/partial:tests",
    "//lib/relagg:tests",
    "//lib/rootagg:tests",
    "//lib/selal:tests",
    "//lib/selident:tests",
    "//lib/tagged:tests",
    "//lib/tc:tests",
  ]
}
`,
		"tools/BUILD.gn": `bazel_test_suite("bazel_tests") {
  target_tests = [
    "//lib/agg:tests",
    "//lib/al/inner:x_test",
    "//lib/dynagg:tests",
    "//lib/hostbad:host_tests",
    "//lib/opaqueagg:tests",
    "//lib/overagg:t1_test",
    "//lib/partial:p1_test",
    "//lib/chain:tests",
    "//lib/relagg:tests",
    "//lib/rootagg:tests",
    "//lib/tc:tests",
    "//lib/tagged:t1_test",
    "//lib/selal:all",
    "//lib/selident:x_test",
  ]
  host_tests = [
    "//lib/hostagg:h_test",
  ]
}
`,
		// READY: the alias points at a test_suite whose only test is wired.
		"lib/al/BUILD.gn": sentinel + `group("host_tests") {
  testonly = true
  public_deps = [ "//lib/al/inner:host_tests" ]
}
`,
		"lib/al/BUILD.bazel": `alias(
    name = "host_tests",
    testonly = True,
    actual = "//lib/al/inner:host_tests",
)
`,
		"lib/al/inner/BUILD.gn": sentinel + `group("host_tests") {
  testonly = true
  deps = [ ":x_test" ]
}

go_test("x_test") {
}
`,
		"lib/al/inner/BUILD.bazel": `test_suite(
    name = "host_tests",
    tests = [":x_test"],
)

go_test(name = "x_test")
`,
		// BLOCKED_TESTS_UNWIRED: only one of the test_suite's tests is wired.
		"lib/partial/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [
    ":p1_test",
    ":p2_test",
  ]
}

go_test("p1_test") {
}

go_test("p2_test") {
}
`,
		"lib/partial/BUILD.bazel": `test_suite(
    name = "tests",
    tests = [
        ":p1_test",
        ":p2_test",
    ],
)

go_test(name = "p1_test")

go_test(name = "p2_test")
`,
		// BLOCKED_TESTS_UNWIRED: wired in Bazel, but the synced group is how
		// GN reaches a hand-written GN test group.
		"lib/agg/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "//lib/agg/gnonly:tests" ]
}
`,
		"lib/agg/BUILD.bazel": `test_suite(
    name = "tests",
    tests = ["//lib/agg/gnonly:tests"],
)
`,
		"lib/agg/gnonly/BUILD.gn": `group("tests") {
  testonly = true
}
`,
		// BLOCKED_TESTS_UNWIRED: the GN-only test is behind a synced group in
		// another sentinel directory, which nothing else in GN reaches.
		"lib/chain/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "//lib/chain/inner:tests" ]
}
`,
		"lib/chain/BUILD.bazel": `test_suite(
    name = "tests",
    tests = ["//lib/chain/inner:tests"],
)
`,
		"lib/chain/inner/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "//lib/agg/gnonly:tests" ]
}
`,
		// BLOCKED_TESTS_UNWIRED: a toolchain suffix doesn't hide a GN-only
		// test.
		"lib/tc/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  public_deps = [ "//lib/agg/gnonly:tests($host_toolchain)" ]
}
`,
		"lib/tc/BUILD.bazel": `test_suite(
    name = "tests",
    tests = ["//lib/agg/gnonly:tests"],
)
`,
		// BLOCKED_TESTS_UNWIRED: aliases pointing at each other are never
		// wired.
		"lib/cycle/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ ":c_test" ]
}

go_test("c_test") {
}
`,
		"lib/cycle/BUILD.bazel": `alias(
    name = "tests",
    actual = ":other",
)

alias(
    name = "other",
    actual = ":tests",
)

go_test(name = "c_test")
`,
		// BLOCKED_TESTS_UNWIRED: an alias that can't be expanded isn't a
		// test, so the package's implicit "every test" suite doesn't run it.
		"lib/selal/BUILD.gn": sentinel + `group("tests") {
  testonly = true
}
`,
		"lib/selal/BUILD.bazel": `test_suite(name = "all")

alias(
    name = "tests",
    actual = select({"//conditions:default": ":x"}),
)
`,
		// BLOCKED_TESTS_UNWIRED: a test_suite with a tags filter isn't
		// expanded, even if its listed tests are wired.
		"lib/tagged/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ ":t1_test" ]
}

go_test("t1_test") {
}
`,
		"lib/tagged/BUILD.bazel": `test_suite(
    name = "tests",
    tags = ["-flaky"],
    tests = [":t1_test"],
)

go_test(name = "t1_test")
`,
		// BLOCKED_TESTS_UNWIRED: a bare relative label inside a synced group
		// (e.g. from path_overwrite) naming a non-sentinel GN-only directory
		// still counts as a GN-only leaf.
		"lib/relagg/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "gnonly" ]
}
`,
		"lib/relagg/BUILD.bazel": `test_suite(
    name = "tests",
    tests = ["//lib/relagg/gnonly"],
)
`,
		"lib/relagg/gnonly/BUILD.gn": `group("gnonly") {
  testonly = true
}
`,
		// BLOCKED_TESTS_UNWIRED: select() with an identifier key and 1 string
		// literal must not be treated as a plain string alias.
		"lib/selident/BUILD.gn": sentinel + `group("tests") {
  testonly = true
}

go_test("x_test") {
}
`,
		"lib/selident/BUILD.bazel": `alias(
    name = "tests",
    actual = select({HOST_CONSTRAINTS: ":x_test"}),
)

go_test(name = "x_test")
`,
		// BLOCKED_TESTS_UNWIRED: root "//:root_gn_tests" inside a synced group
		// is caught as a GN-only leaf.
		"lib/rootagg/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "//:root_gn_tests" ]
}
`,
		"lib/rootagg/BUILD.bazel": `test_suite(
    name = "tests",
    tests = ["//:root_gn_tests"],
)
`,
		// BLOCKED_TESTS_UNWIRED: OpaqueDeps and unresolved "$" dynamic deps
		// inside synced groups count as GN-only leaves.
		"lib/opaqueagg/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = extra_tests
}
`,
		"lib/opaqueagg/BUILD.bazel": `test_suite(name = "tests")
`,
		"lib/dynagg/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "$unmatched_var:tests" ]
}
`,
		"lib/dynagg/BUILD.bazel": `test_suite(name = "tests")
`,
		// READY: unconditional group("all_tests") transitioning internally to
		// host_tests($host_toolchain) is checked in modeHost.
		"lib/hostagg/BUILD.gn": sentinel + `if (is_host) {
  group("host_tests") {
    testonly = true
    public_deps = [ ":h_test" ]
  }
  go_test("h_test") {
  }
}

group("all_tests") {
  testonly = true
  public_deps = [ ":host_tests($host_toolchain)" ]
}
`,
		"lib/hostagg/BUILD.bazel": `alias(
    name = "host_tests",
    actual = ":h_test",
    target_compatible_with = HOST_CONSTRAINTS,
)

alias(
    name = "all_tests",
    actual = ":host_tests",
)

go_test(name = "h_test")
`,
		// BLOCKED_TESTS_UNWIRED: HOST_CONSTRAINTS target listed in
		// target_tests instead of host_tests does not count as wired.
		"lib/hostbad/BUILD.gn": sentinel + `if (is_host) {
  group("host_tests") {
    testonly = true
    public_deps = [ ":hb_test" ]
  }
  go_test("hb_test") {
  }
}

group("all_tests") {
  testonly = true
  public_deps = [ ":host_tests($host_toolchain)" ]
}
`,
		"lib/hostbad/BUILD.bazel": `alias(
    name = "host_tests",
    actual = ":hb_test",
    target_compatible_with = HOST_CONSTRAINTS,
)

alias(
    name = "all_tests",
    actual = ":host_tests",
)

go_test(name = "hb_test")
`,
		// BLOCKED_TESTS_UNWIRED: synced group adding an unwired synced test via
		// raw_overwrite in GN is caught by the RegionBelow leaf check.
		"lib/overagg/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [
    ":extra_test",
    ":t1_test",
  ]
}

go_test("t1_test") {
}

go_test("extra_test") {
}
`,
		"lib/overagg/BUILD.bazel": `test_suite(
    name = "tests",
    tests = [":t1_test"],
)

go_test(name = "t1_test")

go_test(name = "extra_test")
`,
	})
	idx := load(t, root)
	type result struct {
		State   State
		Unwired []string
	}
	got := map[string]result{}
	for _, r := range idx.ScanAll() {
		got[r.Dir] = result{r.State, r.UnwiredTests}
	}
	want := map[string]result{
		"lib/al":          {State: StateReady},
		"lib/al/inner":    {State: StateBlocked},
		"lib/dynagg":      {State: StateBlockedTestsUnwired, Unwired: []string{"$unmatched_var:tests (GN-only, via :tests)"}},
		"lib/hostagg":     {State: StateReady},
		"lib/hostbad":     {State: StateBlockedTestsUnwired, Unwired: []string{"all_tests (host)"}},
		"lib/opaqueagg":   {State: StateBlockedTestsUnwired, Unwired: []string{"//lib/opaqueagg:tests (GN-only, via :tests)"}},
		"lib/overagg":     {State: StateBlockedTestsUnwired, Unwired: []string{"extra_test"}},
		"lib/partial":     {State: StateBlockedTestsUnwired, Unwired: []string{"tests"}},
		"lib/agg":         {State: StateBlockedTestsUnwired, Unwired: []string{"//lib/agg/gnonly:tests (GN-only, via :tests)"}},
		"lib/chain":       {State: StateBlockedTestsUnwired, Unwired: []string{"//lib/agg/gnonly:tests (GN-only, via :tests)"}},
		"lib/chain/inner": {State: StateBlocked},
		"lib/cycle":       {State: StateBlockedTestsUnwired, Unwired: []string{"tests"}},
		"lib/relagg":      {State: StateBlockedTestsUnwired, Unwired: []string{"gnonly (GN-only, via :tests)"}},
		"lib/rootagg":     {State: StateBlockedTestsUnwired, Unwired: []string{"//:root_gn_tests (GN-only, via :tests)"}},
		"lib/selal":       {State: StateBlockedTestsUnwired, Unwired: []string{"tests"}},
		"lib/selident":    {State: StateBlockedTestsUnwired, Unwired: []string{"tests"}},
		"lib/tagged":      {State: StateBlockedTestsUnwired, Unwired: []string{"tests"}},
		"lib/tc":          {State: StateBlockedTestsUnwired, Unwired: []string{"//lib/agg/gnonly:tests($host_toolchain) (GN-only, via :tests)"}},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("ScanAll() diff (-want +got):\n%s", diff)
	}
}

func TestUnsyncFollowUps(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"tools/BUILD.gn": `bazel_test_suite("bazel_tests") {
  target_tests = [
    "//lib/dyn/sub:sub_test",
    "//lib/y:y_test",
  ]
}
`,
		// STRIP_BELOW_SENTINEL leaves a private config that only the
		// stripped target used.
		"lib/x/BUILD.gn": `config("inc") {
  visibility = [ ":*" ]
  include_dirs = [ "." ]
}

` + sentinel + `source_set("x") {
  configs += [ ":inc" ]
}
`,
		// DELETE_BUILD_GN empties the parent test group, leaves visibility
		// patterns (hand-written and generated), and cleans BUILD.bazel.
		"lib/y/BUILD.gn": sentinel + `go_test("y_test") {
}
`,
		"lib/y/BUILD.bazel": `# Copyright 2026 The Fuchsia Authors. All rights reserved.

# GN keeps its own group("tests") above the sentinel.
# @bazel2gn:skip
test_suite(
    name = "tests",
    tests = [":y_test"],
)

# Keep include path until header moves.
# @bazel2gn:skip
fx_cc_test_binary(
    name = "y_test",
    srcs = ["y.cc"],  # @bazel2gn:path_overwrite:y_gn.cc
    gn_output_name = "y_test_bin",
    public_configs = [
        ":cfg",
    ],
)

idk_cc_shared_library_zx(
    name = "y_idk",
    srcs = ["y.cc"],
    configs = [":cfg"],
    inputs = ["y.ifs"],
    ldflags = ["-Wl,-soname=liby.so"],
)

rustc_staticlib(
    name = "y_rs",
    srcs = ["lib.rs"],
    gn_quiet_clippy = True,
    output_name = "y_rs_gn",
)

rustc_library(
    name = "y_rlib",
    srcs = ["lib.rs"],
    output_name = "y_rlib_gn",
)
`,
		"lib/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ "y:y_test" ]
}

group("dyn") {
}
`,
		"lib/z/BUILD.gn": `group("z") {
  visibility = [ "//lib/y:*" ]
}
`,
		"third_party/gen/BUILD.gn": `# Copyright 2026 The Fuchsia Authors.
# DO NOT EDIT. THIS FILE IS GENERATED BY CARGO GNAW. EDIT THE CARGO TOML FILE

group("gen") {
  visibility = [ "//lib/y:*" ]
}
`,
		// Emptying a test group whose directory is listed as a bare entry in
		// another file's variable (like ffx's `_tools` -> `"$tool:tests"`)
		// warns about that caller.
		"ffx/tools/BUILD.gn": `_tools = [ "//lib/dyn" ]
group("tests") {
  testonly = true
  deps = []
  foreach(t, _tools) {
    deps += [ "$t:tests" ]
  }
}
`,
		"lib/dyn/defs.gni": `_here = rebase_path(".", root_build_dir)
`,
		"lib/dyn/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ "sub:sub_test" ]
}
`,
		"lib/dyn/sub/BUILD.gn": `config("sub_inc") {
  include_dirs = [
    "..",
    "//lib/dyn",
  ]
}

` + sentinel + `go_test("sub_test") {
}
`,
		"lib/dyn/sub/BUILD.bazel": `go_test(name = "sub_test")
`,
	})
	gnBin := findGN(t)
	idx := load(t, root)
	plan := idx.PlanUnsync(idx.ScanAll())
	if diff := cmp.Diff([]string{"lib/dyn/sub", "lib/x", "lib/y"}, plan.Dirs); diff != "" {
		t.Errorf("plan.Dirs diff (-want +got):\n%s", diff)
	}
	want := []string{
		`lib/BUILD.gn:1 group("tests") is now empty; remove it and the entries (including any dynamic ":tests" references) that point at it`,
		`lib/dyn/BUILD.gn:1 group("tests") is now empty; check dynamic ":tests" callers (ffx/tools/BUILD.gn:1) before removing it`,
		`lib/x/BUILD.gn:1 config("inc") was only used by the stripped targets; remove it if nothing else needs it`,
		`lib/y/BUILD.bazel:10 "# Keep include path until header moves." was above a removed bazel2gn item; check whether it still applies`,
		`lib/y/BUILD.bazel:3 "# GN keeps its own group(\"tests\") above the sentinel." mentions GN/bazel2gn; remove or update it`,
		`lib/z/BUILD.gn:2 "//lib/y:*" names a deleted BUILD.gn`,
		`third_party/gen/BUILD.gn:5 "//lib/y:*" names a deleted BUILD.gn (generated file: DO NOT EDIT. THIS FILE IS GENERATED BY CARGO GNAW. EDIT THE CARGO TOML FILE)`,
	}
	if diff := cmp.Diff(want, plan.FollowUps); diff != "" {
		t.Errorf("plan.FollowUps diff (-want +got):\n%s", diff)
	}
	if err := plan.Apply(context.Background(), root, gnBin); err != nil {
		t.Fatalf("Apply() error: %v", err)
	}
	gotBazel, err := os.ReadFile(filepath.Join(root, "lib/y/BUILD.bazel"))
	if err != nil {
		t.Fatal(err)
	}
	wantBazel := `# Copyright 2026 The Fuchsia Authors. All rights reserved.

# GN keeps its own group("tests") above the sentinel.
test_suite(
    name = "tests",
    tests = [":y_test"],
)

# Keep include path until header moves.
fx_cc_test_binary(
    name = "y_test",
    srcs = ["y.cc"],
)

idk_cc_shared_library_zx(
    name = "y_idk",
    srcs = ["y.cc"],
)

rustc_staticlib(
    name = "y_rs",
    srcs = ["lib.rs"],
)

rustc_library(
    name = "y_rlib",
    srcs = ["lib.rs"],
)
`
	if diff := cmp.Diff(wantBazel, string(gotBazel)); diff != "" {
		t.Errorf("lib/y/BUILD.bazel after Apply() diff (-want +got):\n%s", diff)
	}
}

func TestUnsyncWaves(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	gnBin := findGN(t)
	opts := LoadOptions{Root: root, GNBin: gnBin, BatchSize: 3, Workers: 2}
	idx, stats, err := Load(context.Background(), opts)
	if err != nil || len(stats.ParseErrors) > 0 {
		t.Fatalf("Load() = %v, %v", err, stats.ParseErrors)
	}
	plan, waves, err := UnsyncWaves(context.Background(), idx, opts)
	if err != nil {
		t.Fatalf("UnsyncWaves() error: %v", err)
	}
	if waves != 2 {
		t.Errorf("waves = %d, want 2", waves)
	}
	if diff := cmp.Diff([]string{"lib/chained", "lib/opaque", "lib/ready", "lib/strip", "lib/suite"}, plan.Dirs); diff != "" {
		t.Errorf("plan.Dirs diff (-want +got):\n%s", diff)
	}
	for _, f := range []string{"lib/ready/BUILD.gn", "lib/suite/BUILD.gn", "lib/chained/BUILD.gn"} {
		if _, err := os.Stat(filepath.Join(root, f)); !os.IsNotExist(err) {
			t.Errorf("%s still exists after UnsyncWaves (err=%v)", f, err)
		}
	}
}
