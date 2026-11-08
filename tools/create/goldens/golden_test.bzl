# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines a golden test for `fx create`."""

load("@bazel_skylib//rules:write_file.bzl", "write_file")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host_tests:host_py_test.bzl", "host_py_test")
load("//build/bazel/rules/host_tests:host_test_data.bzl", "host_test_data_map")
load("//tools/create/templates:templates.bzl", "TEMPLATES")

visibility("private")

def golden_test(
        name,
        project_name,
        project_type,
        language,
        golden_files,
        project_subtype = "default",
        year = "2020",
        target_compatible_with = HOST_OS_CONSTRAINTS):
    """Defines a golden test for `fx create`.

    Generates a project with `fx create` and compares the output with the
    golden project of the same name.

    Args:
      name: The name of the test target.
      project_name: The name of the generated and golden project.
      project_type: The type of project to create. See `fx create --help`.
      language: The programming language to use when generating the project.
      golden_files: The list of all files in the golden project.
      project_subtype: The subtype of project to create. Defaults to "default".
      year: The year to use in the copyright header. Defaults to "2020".
      target_compatible_with: Platform constraints for the test target.
    """
    test_dir = "%s_test_data" % project_name
    target_goldens_json = "%s_goldens_json" % name
    target_test_data = "%s_test_data" % name

    # Generated projects that include a BUILD.bazel file are their own Bazel
    # package, so their files have to be referenced through that package.
    extra_args = []
    if ("%s/BUILD.bazel" % project_name) in golden_files:
        extra_args.append("--export-all-files")
        package = native.package_name()
        golden_labels = {
            f: "//%s/%s:%s" % (package, project_name, f.removeprefix(project_name + "/"))
            for f in golden_files
        }
    else:
        golden_labels = {f: f for f in golden_files}

    write_file(
        name = target_goldens_json,
        testonly = True,
        out = "%s_goldens.json" % name,
        content = [
            json.encode_indent(
                ["%s/%s" % (test_dir, f) for f in golden_files],
                indent = "  ",
            ),
        ],
        target_compatible_with = target_compatible_with,
    )

    files_map = {
        "%s/goldens.json" % test_dir: ":%s" % target_goldens_json,
        "host-tools/create": "//tools/create:create_bin",
        "host-tools/create_templates/templates.json": "//tools/create/templates:templates_json",
    }
    files_map |= {
        "%s/%s" % (test_dir, f): label
        for f, label in golden_labels.items()
    }
    files_map |= {
        "host-tools/create_templates/%s" % t: "//tools/create/templates:%s" % t
        for t in TEMPLATES
    }
    host_test_data_map(
        name = target_test_data,
        testonly = True,
        files_map = files_map,
        target_compatible_with = target_compatible_with,
    )

    host_py_test(
        name = name,
        srcs = ["test_script.py"],
        main = "test_script.py",
        target_compatible_with = target_compatible_with,
        test_args = [
            test_dir,
            "host-tools/create",
            "%s/goldens.json" % test_dir,
            project_type,
            project_subtype,
            project_name,
            "--lang",
            language,
            "--override-project-path",
            native.package_name(),
            "--override-copyright-year",
            year,
            "--silent",
        ] + extra_args,
        test_data = [":%s" % target_test_data],
    )
