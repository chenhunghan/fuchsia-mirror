# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines SHAC checks for Bazel files."""

load("./common.star", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def _bazel_default_applicable_licenses(ctx):
    """Checks that non-third-party BUILD.bazel files have default_applicable_licenses set."""
    allowlist_path = "build/bazel/shac/allowlists.json"
    data = json.decode(str(ctx.io.read_file(allowlist_path)))
    ignored_prefixes = data.get("ignored_prefixes", [])
    ignored_files = data.get("ignored_files", [])

    for f in ctx.scm.affected_files(glob = ["BUILD.bazel"]):
        should_ignore = False
        if f in ignored_files:
            should_ignore = True
        else:
            for p in ignored_prefixes:
                prefix = p if p.endswith("/") else p + "/"
                if f.startswith(prefix):
                    should_ignore = True
                    break

        if should_ignore:
            continue

        contents = str(ctx.io.read_file(f))

        # Match package( ... default_applicable_licenses = ["//:license"] ... )
        # Using [^)]* to match anything except closing parenthesis to stay within the package call.
        if not ctx.re.allmatches(r"package\([^)#]*default_applicable_licenses\s*=\s*\[\"//:license\"\][^)]*\)", contents):
            ctx.emit.finding(
                level = "error",
                message = "BUILD.bazel files must include `default_applicable_licenses = [\"//:license\"]` within a `package()` call to set the default file-level license.",
                filepath = f,
            )

def _bazel_disallowed_workspace_root_package_labels(ctx):
    """Reports an error if a disallowed workspace root package label is used.

    Workspace root package labels (e.g. `//:...`) should only refer to targets
    or files defined directly in the root BUILD.bazel file. Disallowed labels
    are indicated by a slash after the colon (e.g. `//:foo/bar`), which bypasses
    package boundaries.

    Upstream third-party source files (!/third_party/**/src/**) are excluded
    because upstream projects manage their own package structures and should not
    be subject to Fuchsia workspace conventions. This exception may need to be
    relaxed if we have to write BUILD.bazel files for third-party repositories.
    """

    # Temporarily exclude repositories other than `fuchsia.git` to avoid
    # breaking unknown use cases.
    # TODO(https://fxbug.dev/560343570): Remove this.
    if ctx.scm.root != get_fuchsia_dir(ctx):
        return

    files = list(ctx.scm.affected_files(glob = [
        "*.bazel",
        "*.bzl",
        "!/third_party/**/src/**",
        # TODO(https://fxbug.dev/560343570): Fix existing violations and remove.
        "!/vendor/**",
    ]).keys())
    if not files:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        cipd_platform_name(ctx),
    )
    checker_script = "%s/scripts/shac/bazel_root_package_labels.py" % fuchsia_dir

    res = os_exec(
        ctx,
        [python_bin, checker_script, "--root", ctx.scm.root, "--json"] + files,
        ok_retcodes = (0, 1),
    ).wait()

    findings = json.decode(res.stdout)
    if type(findings) != "list":
        fail("Expected a list of findings from %s (exit code %d), got: %s\nstderr: %s" % (
            checker_script,
            res.retcode,
            res.stdout,
            res.stderr,
        ))

    if bool(res.retcode) != bool(findings):
        fail("%s exited with code %d, but reported %d findings" % (
            checker_script,
            res.retcode,
            len(findings),
        ))

    for finding in findings:
        kwargs = {
            "level": finding.get("level", "error"),
            "filepath": finding["filepath"],
        }
        if finding.get("line"):
            kwargs["line"] = finding["line"]
        if finding.get("col"):
            kwargs["col"] = finding["col"]
        if finding.get("end_col"):
            kwargs["end_col"] = finding["end_col"]
        ctx.emit.finding(message = finding["message"], **kwargs)

def register_bazel_checks():
    shac.register_check(shac.check(_bazel_default_applicable_licenses, formatter = False))
    shac.register_check(shac.check(_bazel_disallowed_workspace_root_package_labels, formatter = False))
