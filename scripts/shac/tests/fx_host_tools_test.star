# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the check_fx_host_tools_registry check in //scripts/shac/fx_host_tools.star."""

load("//scripts/shac/fx_host_tools.star", "check_fx_host_tools_registry")

_CORE_VARS = {"fuchsia_dir": "."}
_VENDOR_VARS = {"fuchsia_dir": "../.."}

_CORE_CMD = [
    testing.any_args,
    testing.root + "/tools/devshell/fx_cli/registry.py",
    "check",
    "--fuchsia-dir",
    testing.root,
    "--registry-file",
    testing.root + "/tools/devshell/fx_host_tools.json",
]

def test_irrelevant_files_skipped():
    """Irrelevant files do not invoke fx registry check."""
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _CORE_VARS,
        files = {
            "README.md": testing.file(action = "M"),
            "src/foo/bar.cc": testing.file(action = "M"),
            "tools/other/baz.py": testing.file(action = "M"),
        },
    )
    asserts.eq(res.findings, ())

def test_tracked_file_finding():
    """ERROR lines referencing a tracked file attach filepath and strip prefix."""
    reg_rel = "tools/devshell/fx_host_tools.json"
    err_msg = "Top-level keys in fx_host_tools.json must be sorted alphabetically."
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _CORE_VARS,
        files = {
            reg_rel: testing.file(action = "M"),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = _CORE_CMD,
                retcode = 1,
                stderr = "ERROR: %s/%s: %s\n" % (testing.root, reg_rel, err_msg),
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            filepath = reg_rel,
            message = err_msg,
        ),
    ))

def test_untracked_file_fallback():
    """ERROR lines referencing an untracked or deleted file omit filepath."""
    deleted_rel = "tools/devshell/fx_host_tools.json"
    untracked_rel = "vendor/acme/scripts/devshell/fx_host_tools.json"
    stderr = "\n".join([
        "ERROR: %s/%s: Registry file does not exist." % (testing.root, deleted_rel),
        "ERROR: %s/%s: Invalid entry." % (testing.root, untracked_rel),
    ]) + "\n"
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _CORE_VARS,
        files = {
            deleted_rel: testing.file(action = "D"),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = _CORE_CMD,
                retcode = 1,
                stderr = stderr,
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "%s: Registry file does not exist." % deleted_rel,
        ),
        testing.finding(
            level = "error",
            message = "%s: Invalid entry." % untracked_rel,
        ),
    ))

def test_ignores_non_error_stderr_noise():
    """Incidental non-ERROR stderr lines (e.g. mkdir / fx-warn) are ignored when ERROR lines exist."""
    reg_rel = "tools/devshell/fx_host_tools.json"
    err_msg = "Expected top-level JSON object in fx_host_tools.json."
    stderr = "\n".join([
        "mkdir: cannot create directory '/work/.fx': Read-only file system",
        "WARNING: metrics disabled",
        "ERROR: %s/%s: %s" % (testing.root, reg_rel, err_msg),
    ]) + "\n"
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _CORE_VARS,
        files = {
            reg_rel: testing.file(action = "M"),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = _CORE_CMD,
                retcode = 1,
                stderr = stderr,
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            filepath = reg_rel,
            message = err_msg,
        ),
    ))

def test_nonzero_exit_without_error_lines_emits_fallback():
    """Non-zero exit without any ERROR: lines emits a single fallback finding."""
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _CORE_VARS,
        files = {
            "tools/devshell/fx_host_tools.json": testing.file(action = "M"),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = _CORE_CMD,
                retcode = 2,
                stderr = "Fatal internal error\n",
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "fx registry check failed (exit code 2):\nFatal internal error\n",
        ),
    ))

def test_vendor_repo_check():
    """Vendor repository runs fx registry check scoped to --vendor and vendor registry file."""
    vendor_reg = "scripts/devshell/fx_host_tools.json"
    err_msg = "Entry 'foo' references missing Bazel package."
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _VENDOR_VARS,
        files = {
            vendor_reg: testing.file(action = "M"),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    testing.root + "/../../tools/devshell/fx_cli/registry.py",
                    "check",
                    "--fuchsia-dir",
                    testing.root + "/../..",
                    "--vendor",
                    testing.any_args,
                    "--registry-file",
                    testing.root + "/" + vendor_reg,
                ],
                retcode = 1,
                stderr = "ERROR: %s/%s: %s\n" % (testing.root, vendor_reg, err_msg),
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            filepath = vendor_reg,
            message = err_msg,
        ),
    ))

def test_vendor_repo_without_registry_skipped():
    """Vendor repository without scripts/devshell/fx_host_tools.json is skipped."""
    res = testing.run(
        check_fx_host_tools_registry,
        vars = _VENDOR_VARS,
        files = {
            "src/foo/BUILD.bazel": testing.file(action = "M"),
        },
    )
    asserts.eq(res.findings, ())
