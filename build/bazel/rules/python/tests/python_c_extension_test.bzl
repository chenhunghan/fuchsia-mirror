# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Analysis tests for `python_c_extension()`."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load("@rules_python//python:py_info.bzl", "PyInfo")
load("//build/bazel/aspects:pyrefly.bzl", "pyrefly_aspect")

visibility("private")

_RuleAttrsInfo = provider(
    doc = "Captures rule attributes needed for analysis assertions.",
    fields = ["target_compatible_with", "testonly"],
)

def _rule_attrs_aspect_impl(_target, ctx):
    compat = []
    if hasattr(ctx.rule.attr, "target_compatible_with"):
        compat = [t.label for t in ctx.rule.attr.target_compatible_with]
    return [
        _RuleAttrsInfo(
            target_compatible_with = compat,
            testonly = getattr(ctx.rule.attr, "testonly", False),
        ),
    ]

_rule_attrs_aspect = aspect(
    implementation = _rule_attrs_aspect_impl,
)

def _end(env):
    if env.failures:
        fail("\n".join(env.failures))
    return analysistest.end(env)

def _python_c_extension_lib_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    asserts.true(
        env,
        PyInfo in target,
        "Expected PyInfo provider on target {}.".format(target),
    )
    py_info = target[PyInfo]

    imports = py_info.imports.to_list()
    asserts.true(
        env,
        ctx.attr.expected_import in imports,
        "Expected '{}' in PyInfo.imports, got {}.".format(ctx.attr.expected_import, imports),
    )

    trans_srcs = [f.short_path for f in py_info.transitive_sources.to_list()]
    asserts.true(
        env,
        ctx.attr.expected_staged_init in trans_srcs,
        "Expected '{}' in PyInfo.transitive_sources, got {}.".format(ctx.attr.expected_staged_init, trans_srcs),
    )

    actual_pyi = sorted([f.short_path for f in py_info.transitive_pyi_files.to_list()])
    asserts.equals(
        env,
        sorted(ctx.attr.expected_pyi_files),
        actual_pyi,
        "PyInfo.transitive_pyi_files mismatch.",
    )

    runfile_paths = [f.short_path for f in target[DefaultInfo].default_runfiles.files.to_list()]
    for req in ctx.attr.expected_runfile_substrings:
        asserts.true(
            env,
            any([req in p for p in runfile_paths]),
            "Expected runfile matching '{}' in {}.".format(req, runfile_paths),
        )
    for forbidden in ctx.attr.forbidden_runfile_substrings:
        asserts.false(
            env,
            any([forbidden in p for p in runfile_paths]),
            "Did not expect runfile matching '{}' in {}.".format(forbidden, runfile_paths),
        )

    attrs_info = target[_RuleAttrsInfo]
    expected_constraints = [Label(c) for c in ctx.attr.expected_constraints]
    asserts.equals(
        env,
        expected_constraints,
        attrs_info.target_compatible_with,
        "target_compatible_with mismatch.",
    )
    asserts.equals(
        env,
        ctx.attr.expected_testonly,
        attrs_info.testonly,
        "testonly mismatch.",
    )

    pyrefly_actions = [a for a in target.actions if a.mnemonic == "Pyrefly"]
    asserts.equals(
        env,
        1,
        len(pyrefly_actions),
        "Unexpected Pyrefly action count (actions={}).".format([a.mnemonic for a in target.actions]),
    )
    if pyrefly_actions and ctx.attr.expected_search_path:
        argv = pyrefly_actions[0].argv
        asserts.true(
            env,
            ctx.attr.expected_search_path in argv,
            "Expected search path '{}' in Pyrefly argv, got {}.".format(ctx.attr.expected_search_path, argv),
        )

    has_output_group_info = OutputGroupInfo in target
    asserts.true(
        env,
        has_output_group_info and hasattr(target[OutputGroupInfo], "pyrefly_checks"),
        "Expected OutputGroupInfo with pyrefly_checks, got OutputGroupInfo={}, fields={}.".format(
            has_output_group_info,
            dir(target[OutputGroupInfo]) if has_output_group_info else [],
        ),
    )
    validation_basenames = [f.basename for f in target[OutputGroupInfo]._validation.to_list()]
    expected_stamp = target.label.name + ".pyrefly.ok"
    asserts.true(
        env,
        expected_stamp in validation_basenames,
        "Expected '{}' in _validation output group, got {}.".format(expected_stamp, validation_basenames),
    )

    return _end(env)

python_c_extension_lib_test = analysistest.make(
    _python_c_extension_lib_test_impl,
    doc = "Verifies PyInfo, runfiles, platform constraints, and Pyrefly integration on python_c_extension().",
    extra_target_under_test_aspects = [pyrefly_aspect, _rule_attrs_aspect],
    attrs = {
        "expected_constraints": attr.string_list(doc = "Expected constraint values.", mandatory = True),
        "expected_import": attr.string(doc = "Expected PyInfo import path.", mandatory = True),
        "expected_pyi_files": attr.string_list(doc = "Expected `.pyi` stub short paths.", default = []),
        "expected_runfile_substrings": attr.string_list(doc = "Expected runfile path substrings.", default = []),
        "expected_search_path": attr.string(doc = "Expected Pyrefly search path.", default = ""),
        "expected_staged_init": attr.string(doc = "Expected staged `__init__.py` short path.", mandatory = True),
        "expected_testonly": attr.bool(doc = "Expected `testonly` flag.", default = True),
        "forbidden_runfile_substrings": attr.string_list(doc = "Forbidden runfile path substrings.", default = []),
    },
)

def _c_extension_shlib_data_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    default_info = target[DefaultInfo]

    file_paths = [f.short_path for f in default_info.files.to_list()]
    asserts.equals(
        env,
        [ctx.attr.expected_shlib_short_path],
        file_paths,
        "DefaultInfo.files mismatch.",
    )

    runfile_paths = [f.short_path for f in default_info.default_runfiles.files.to_list()]
    for req in ctx.attr.expected_runfile_substrings:
        asserts.true(
            env,
            any([req in p for p in runfile_paths]),
            "Expected runfile matching '{}' in {}.".format(req, runfile_paths),
        )
    for forbidden in ctx.attr.forbidden_runfile_substrings:
        asserts.false(
            env,
            any([forbidden in p for p in runfile_paths]),
            "Did not expect non-library runfile '{}' in {}.".format(forbidden, runfile_paths),
        )

    return _end(env)

c_extension_shlib_data_test = analysistest.make(
    _c_extension_shlib_data_test_impl,
    doc = "Verifies _c_extension_shlib_data keeps runtime shared libraries and filters out non-library data.",
    attrs = {
        "expected_runfile_substrings": attr.string_list(doc = "Expected runfile path substrings.", mandatory = True),
        "expected_shlib_short_path": attr.string(doc = "Expected extension `.so` short path.", mandatory = True),
        "forbidden_runfile_substrings": attr.string_list(doc = "Forbidden runfile path substrings.", default = []),
    },
)

_ShlibActionsInfo = provider(
    doc = "Captures CppLink and CppCompile action argvs through feature transitions.",
    fields = ["compile_argvs", "link_argvs", "mnemonics"],
)

def _shlib_actions_aspect_impl(target, ctx):
    if hasattr(ctx.rule.attr, "actual"):
        actual = ctx.rule.attr.actual
        if type(actual) == "list":
            actual = actual[0]
        return [actual[_ShlibActionsInfo]]
    return [
        _ShlibActionsInfo(
            compile_argvs = [a.argv for a in target.actions if a.mnemonic == "CppCompile"],
            link_argvs = [a.argv for a in target.actions if a.mnemonic == "CppLink"],
            mnemonics = [a.mnemonic for a in target.actions],
        ),
    ]

_shlib_actions_aspect = aspect(
    implementation = _shlib_actions_aspect_impl,
    attr_aspects = ["actual"],
)

def _c_extension_shlib_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)

    file_paths = [f.short_path for f in target[DefaultInfo].files.to_list()]
    asserts.equals(
        env,
        [ctx.attr.expected_shlib_short_path],
        file_paths,
        "DefaultInfo.files mismatch.",
    )

    actions_info = target[_ShlibActionsInfo]
    link_argvs = actions_info.link_argvs
    asserts.equals(
        env,
        1,
        len(link_argvs),
        "Unexpected CppLink action count (actions={}).".format(actions_info.mnemonics),
    )
    if link_argvs:
        argv_str = " ".join(link_argvs[0])
        asserts.true(
            env,
            "python_api_symbols.lds" in argv_str,
            "Expected python_api_symbols.lds linker script in CppLink argv, got {}.".format(argv_str),
        )
        for arg in link_argvs[0]:
            asserts.false(
                env,
                arg.startswith("-fsanitize"),
                "Unexpected sanitizer link flag '{}' in CppLink argv: {}.".format(arg, link_argvs[0]),
            )

    compile_argvs = actions_info.compile_argvs
    for argv in compile_argvs:
        for arg in argv:
            asserts.false(
                env,
                arg.startswith("-fsanitize"),
                "Unexpected sanitizer compile flag '{}' in CppCompile argv: {}.".format(arg, argv),
            )

    if ctx.attr.expected_define:
        asserts.true(
            env,
            len(compile_argvs) > 0,
            "Expected at least 1 CppCompile action, got {} (actions={}).".format(
                len(compile_argvs),
                actions_info.mnemonics,
            ),
        )
        expected_flag = "-D" + ctx.attr.expected_define
        asserts.true(
            env,
            all([expected_flag in argv for argv in compile_argvs]),
            "Expected '{}' in all CppCompile actions, got {}.".format(expected_flag, compile_argvs),
        )

    return _end(env)

c_extension_shlib_test = analysistest.make(
    _c_extension_shlib_test_impl,
    doc = "Verifies the shared library target, linker script, and compile defines of python_c_extension().",
    extra_target_under_test_aspects = [_shlib_actions_aspect],
    attrs = {
        "expected_define": attr.string(doc = "Expected preprocessor define.", default = ""),
        "expected_shlib_short_path": attr.string(doc = "Expected extension `.so` short path.", mandatory = True),
    },
)
