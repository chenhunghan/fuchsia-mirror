# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC checks validating fx_host_tools.json registries."""

load("./common.star", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def check_fx_host_tools_registry(ctx):
    """Validates fx_host_tools.json schema, sorting, and script/package paths.

    Args:
      ctx: A SHAC ctx instance.
    """
    fuchsia_dir = get_fuchsia_dir(ctx)
    is_core = ctx.scm.root == fuchsia_dir
    devshell_dir = "tools/devshell" if is_core else "scripts/devshell"
    reg_rel = "%s/fx_host_tools.json" % devshell_dir

    if (
        not is_core and
        not ctx.scm.all_files(glob = "/" + reg_rel) and
        not ctx.scm.affected_files(glob = "/" + reg_rel, include_deleted = True)
    ):
        return

    if not ctx.scm.affected_files(
        glob = [
            "fx_host_tools.json",
            "BUILD.bazel",
            "*.bzl",
            "/%s/**" % devshell_dir,
        ],
        include_deleted = True,
    ):
        return

    python_bin = "%s/prebuilt/third_party/python3/%s/bin/python3" % (
        fuchsia_dir,
        cipd_platform_name(ctx),
    )
    registry_py = "%s/tools/devshell/fx_cli/registry.py" % fuchsia_dir
    cmd = [
        python_bin,
        registry_py,
        "check",
        "--fuchsia-dir",
        fuchsia_dir,
    ]
    if not is_core:
        vendor_name = ctx.scm.root.rsplit("/", 1)[-1]
        cmd += ["--vendor", vendor_name]
    cmd += ["--registry-file", "%s/%s" % (ctx.scm.root, reg_rel)]

    res = os_exec(
        ctx,
        cmd,
        ok_retcodes = (0, 1, 2),
    ).wait()

    if res.retcode != 0:
        found_errors = False
        for line in res.stderr.splitlines():
            line = line.strip()
            if not line.startswith("ERROR: "):
                continue
            found_errors = True
            line = line[len("ERROR: "):]
            filepath = None
            msg = line
            if ": " in line:
                cand_path, rest = line.split(": ", 1)
                prefix = ctx.scm.root + "/"
                if cand_path.startswith(prefix):
                    rel_path = cand_path[len(prefix):]
                    if rel_path and not rel_path.startswith("../"):
                        filepath = rel_path
                        msg = rest
            if filepath and not ctx.scm.all_files(glob = "/" + filepath):
                msg = "%s: %s" % (filepath, msg)
                filepath = None
            if filepath:
                ctx.emit.finding(
                    level = "error",
                    filepath = filepath,
                    message = msg,
                )
            else:
                ctx.emit.finding(
                    level = "error",
                    message = msg,
                )
        if not found_errors:
            ctx.emit.finding(
                level = "error",
                message = "fx registry check failed (exit code %d):\n%s" % (
                    res.retcode,
                    res.stderr or res.stdout or "Unknown error",
                ),
            )

def register_fx_host_tools_checks():
    """Registers fx_host_tools.json SHAC checks."""
    shac.register_check(shac.check(check_fx_host_tools_registry))
