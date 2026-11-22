# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""golden_test() macro definition."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")
load("//build/bazel/rules/host_tests:host_test_data.bzl", "FuchsiaHostTestDataInfo")

visibility(["//..."])

# LINT.IfChange(golden_test_query)
# Lets tools like fx check-goldens find golden_test()s with `bazel query`.
GOLDEN_TEST_TAG = "golden_test"
# LINT.ThenChange(//tools/devshell/contrib/check-goldens.py:golden_test_query)

def _golden_test_data_impl(ctx):
    files = {}
    for dest, target in ctx.attr.candidates.items():
        target_files = target[DefaultInfo].files.to_list()
        if len(target_files) != 1:
            fail("golden_test() candidate %s must provide exactly one file, got %d" % (
                target.label,
                len(target_files),
            ))
        files[dest] = target_files[0]
    for dest, target in ctx.attr.goldens.items():
        target_files = target[DefaultInfo].files.to_list()
        if len(target_files) != 1 or not target_files[0].is_source:
            fail("golden_test() golden %s must be a source file" % target.label)
        if dest != "golden_test_data/golden/" + target_files[0].short_path:
            fail("golden_test() golden %s must name the source file %s directly" % (
                target.label,
                target_files[0].short_path,
            ))
        files[dest] = target_files[0]
    return FuchsiaHostTestDataInfo(label = ctx.label, files = files)

# Like host_test_data_map(), but rejects candidates that don't provide exactly
# one file and goldens that aren't source files, which `--update` couldn't
# write to.
_golden_test_data = rule(
    implementation = _golden_test_data_impl,
    provides = [FuchsiaHostTestDataInfo],
    attrs = {
        "candidates": attr.string_keyed_label_dict(allow_files = True, mandatory = True),
        "goldens": attr.string_keyed_label_dict(allow_files = True, mandatory = True),
    },
)

def _golden_test_impl(name, visibility, candidates, goldens):
    if len(candidates) != len(goldens):
        fail("candidates and goldens must have the same length")
    package = native.package_name()
    candidates_map = {}
    goldens_map = {}
    test_args = ["--label=//%s:%s" % (package, name)]
    for candidate, golden in zip(candidates, goldens):
        if golden.repo_name:
            fail("goldens must be in the main repository, got: %s" % golden)
        golden_source_path = golden.package + "/" + golden.name if golden.package else golden.name
        candidates_map["golden_test_data/candidate/" + golden_source_path] = candidate
        goldens_map["golden_test_data/golden/" + golden_source_path] = golden

        # LINT.IfChange(golden_test_args)
        test_args += [
            "--candidate=golden_test_data/candidate/" + golden_source_path,
            "--golden=golden_test_data/golden/" + golden_source_path,
            "--golden-source-path=" + golden_source_path,
        ]
        # LINT.ThenChange(//tools/devshell/contrib/check-goldens.py:golden_test_args)

    test_data = name + "_data"
    _golden_test_data(
        name = test_data,
        testonly = True,
        candidates = candidates_map,
        goldens = goldens_map,
        target_compatible_with = HOST_OS_CONSTRAINTS,
    )

    host_test(
        name = name,
        binary = "//build/testing:golden_test",
        data = [":" + test_data],
        tags = [GOLDEN_TEST_TAG],
        target_compatible_with = HOST_OS_CONSTRAINTS,
        test_args = test_args,
        visibility = visibility,
    )

# A symbolic macro, so the //build/testing:golden_test binary only has to be
# visible to this package. Labels it references that aren't attributes are
# checked against this package too, which is why goldens are passed as labels
# rather than as paths.
_golden_test = macro(
    implementation = _golden_test_impl,
    attrs = {
        # Parallel lists: the Nth candidate is compared with the Nth golden.
        # The paths are passed to the test at loading time, and dict
        # attributes can't hold a select(), so neither is configurable.
        "candidates": attr.label_list(
            mandatory = True,
            allow_files = True,
            configurable = False,
        ),
        "goldens": attr.label_list(
            mandatory = True,
            allow_files = True,
            configurable = False,
        ),
    },
)

def golden_test(name, goldens, visibility = None):
    """Defines a host test that compares generated files to checked-in goldens.

    Use this for goldens that record a tool's output for code review. For
    goldens that guard an artifact the build produces, use
    verify_golden_files() instead, so a stale golden fails the build of that
    artifact.

    On a mismatch, the test prints a diff and the command to update the
    goldens:

        fx bazel run --config=host //<package>:<name> -- --update

    `fx check-goldens` also finds these tests. Unlike verify_golden_files(),
    building with `update_goldens=true` does not update them.

    Each golden must already exist, since it's a source file; create an empty
    file before running `--update` for a new golden.

    Args:
      name: The name of the test target.
      goldens: Maps each golden's path, relative to the current package, to
        the label of the generated file it should match (e.g. a genrule
        output). Each label must provide exactly one file.
      visibility: Usual meaning.
    """
    if not goldens:
        fail("golden_test() needs at least one golden")
    _golden_test(
        name = name,
        candidates = goldens.values(),
        goldens = goldens.keys(),
        visibility = visibility,
    )
