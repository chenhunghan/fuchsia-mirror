# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines SHAC checks for C and C++ files."""

load(
    "./common.star",
    "FORMATTER_MSG",
    "cipd_platform_name",
    "get_fuchsia_dir",
    "os_exec",
)

# Paths excluded from C/C++ formatting and static analysis checks.
_IGNORED_GLOBS = [
    "!/build/bazel/fuchsia_idk/validation_data/**",
    "!/build/sdk/generate_prebuild_idk/validation_data/**",
    "!/src/devices/tools/fidlgen_banjo/tests/**",
    "!**/goldens/**",
    "!**/third_party/**",
]

def _clang_format(ctx):
    """Formats C/C++/Proto code using clang-format."""
    cpp_files = list(ctx.scm.affected_files(glob = [
        "*.c",
        "*.cc",
        "*.cpp",
        "*.h",
        "*.hh",
        "*.hpp",
        "*.proto",
    ] + _IGNORED_GLOBS).keys())
    if not cpp_files:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    platform = cipd_platform_name(ctx)
    clang_format_bin = "%s/prebuilt/third_party/clang/%s/bin/clang-format" % (
        fuchsia_dir,
        platform,
    )

    base_cmd = [
        clang_format_bin,
        "-style=file",
        "-fallback-style=Google",
        "-sort-includes",
    ]

    batch_size = 500
    dry_run_procs = []
    for i in range(0, len(cpp_files), batch_size):
        batch = cpp_files[i:i + batch_size]
        dry_run_procs.append(
            os_exec(
                ctx,
                base_cmd + ["--dry-run", "--Werror", "--ferror-limit=1"] + batch,
                ok_retcodes = [0, 1],
            ),
        )

    violation_suffix = ": error: code should be clang-formatted [-Wclang-format-violations]"
    unformatted = []
    for proc in dry_run_procs:
        res = proc.wait()
        batch_unformatted = []
        for line in res.stderr.splitlines():
            if line.endswith(violation_suffix):
                filepath = line[:-len(violation_suffix)].rsplit(":", 2)[0]
                batch_unformatted.append(filepath)
        if res.retcode and not batch_unformatted:
            fail("clang-format failed:\n%s" % res.stderr)
        unformatted.extend(batch_unformatted)

    procs = []
    for filepath in unformatted:
        procs.append((
            filepath,
            os_exec(ctx, base_cmd + [filepath]),
        ))

    for filepath, proc in procs:
        formatted = proc.wait().stdout

        # Protobuf string fields in SARIF output (--json-output) require valid UTF-8.
        # If a file contains non-UTF-8 bytes (e.g. Latin-1/Windows-1252 comments),
        # omit replacements so shac does not crash marshaling sarif.ArtifactContent.text.
        is_valid_utf8 = (formatted == "".join(formatted.codepoints()))
        if is_valid_utf8:
            msg = FORMATTER_MSG
            replacements = [formatted]
        else:
            msg = (
                "File not formatted and contains non-UTF-8 bytes. " +
                "Convert the file to UTF-8 or add it to _IGNORED_GLOBS " +
                "if it should not be formatted."
            )
            replacements = []
        ctx.emit.finding(
            # Switch to "error" once existing unformatted files in the tree are cleaned up.
            level = "warning",
            message = msg,
            filepath = filepath,
            replacements = replacements,
        )

def _header_guards(ctx):
    """Checks and formats C/C++ header guards."""
    headers = list(ctx.scm.affected_files(glob = [
        "*.h",
    ] + _IGNORED_GLOBS).keys())
    if not headers:
        return

    fuchsia_dir = get_fuchsia_dir(ctx)
    platform = cipd_platform_name(ctx)
    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        platform,
    )
    checker_script = "%s/scripts/shac/check_header_guards.py" % fuchsia_dir

    procs = []
    for h in headers:
        procs.append((
            h,
            os_exec(
                ctx,
                [python_bin, checker_script, "--root", fuchsia_dir, "--emit", h],
                ok_retcodes = (0, 1),
            ),
        ))

    for h, proc in procs:
        res = proc.wait()
        if res.retcode != 0:
            ctx.emit.finding(
                level = "warning",
                message = res.stderr.strip() or ("Header guard issue in %s." % h),
                filepath = h,
            )
        else:
            formatted = res.stdout
            original = str(ctx.io.read_file(h))
            if formatted and formatted != original:
                ctx.emit.finding(
                    level = "warning",
                    message = FORMATTER_MSG,
                    filepath = h,
                    replacements = [formatted],
                )

def register_cpp_checks():
    shac.register_check(shac.check(_clang_format, formatter = True))
    shac.register_check(shac.check(_header_guards, formatter = True))
