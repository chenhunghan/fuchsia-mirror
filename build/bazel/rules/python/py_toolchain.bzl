# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helpers to access and use the Python interpreter directly."""

# Attributes to add to your rule to use generate_python_build_action() in its implementation.
PY_TOOLCHAIN_ATTRS = {
    "_py_toolchain": attr.label(
        default = "@rules_python//python:current_py_toolchain",
        cfg = "exec",
        providers = [DefaultInfo, platform_common.ToolchainInfo],
    ),
}

def _find_python_interpreter_and_runfiles(ctx):
    """Finds the Python interpreter and its runfiles.

    Args:
        ctx: The context of the rule. The corresponding rule definition
            should include PY_TOOLCHAIN_ATTRS in its `attrs`.
    Returns:
        A tuple of the Python interpreter and its runfiles.
    """
    if not hasattr(ctx.attr, "_py_toolchain"):
        fail("Missing _py_toolchain attribute, did you forget to include 'PY_TOOLCHAIN_ATTRS' in your rule's 'attrs'?")
    toolchain_info = ctx.attr._py_toolchain[platform_common.ToolchainInfo]
    if not toolchain_info.py3_runtime:
        fail("A Bazel python3 runtime is required, and none was configured!")

    python3_executable = toolchain_info.py3_runtime.interpreter
    python3_runfiles = ctx.runfiles(transitive_files = toolchain_info.py3_runtime.files)
    return python3_executable, python3_runfiles

def generate_python_build_action(
        ctx,
        *,
        py_script,
        inputs,
        outputs,
        arguments = [],
        **kwargs):
    """Generates an action invoking a Python script directly with the Python interpreter.

    In practice this is much faster than using a py_binary(), which will wrap everything with an
    intermediate script and a tree of runfiles files before running the action.

    Recommended for build actions that do not need to be invoked with `bazel run` later.

    Args:
        ctx: The context of the rule. The corresponding rule definition
            must include PY_TOOLCHAIN_ATTRS in its `attrs`.
        py_script: The Python script File to run.
        inputs: List or depset of File objects required by the action. This must
            include all non-stdlib modules imported transitively from the script.
        outputs: List of output File objects.
        arguments: List of string arguments passed to the script.
        **kwargs: Additional arguments to pass to ctx.actions.run().
    """
    # Get Python3 interpreter and its runfiles.

    python3_executable, python3_runfiles = _find_python_interpreter_and_runfiles(ctx)

    if type(inputs) == "depset":
        action_inputs = depset(
            direct = [py_script],
            transitive = [inputs, python3_runfiles.files],
        )
    else:
        action_inputs = depset(
            direct = [py_script] + inputs,
            transitive = [python3_runfiles.files],
        )
    ctx.actions.run(
        executable = python3_executable,
        arguments = [py_script.path] + arguments,
        inputs = action_inputs,
        outputs = outputs,
        **kwargs
    )
