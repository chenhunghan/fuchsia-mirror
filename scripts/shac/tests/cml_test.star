# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/cml.star."""

load("//scripts/shac/cml.star", "_cml_format")
load("//scripts/shac/common.star", "FORMATTER_MSG")

def _tool_paths(*names, built = True):
    # Lists each tool for every platform so the tests don't depend on the
    # host platform.
    tools = []
    files = {}
    for name in names:
        for os, cpu in [("linux", "x64"), ("linux", "arm64"), ("mac", "x64"), ("mac", "arm64")]:
            path = "host_%s/%s" % (cpu, name)
            tools.append({
                "label": "//tools/%s:%s(//build/toolchain:host_%s)" % (name, name, cpu),
                "name": name,
                "os": os,
                "cpu": cpu,
                "path": path,
            })
            if built:
                files["out/default/" + path] = testing.file(affected = False)
    files["out/default/tool_paths.json"] = testing.file(content = json.encode(tools), affected = False)
    return files

def test_cml_format():
    formatted = "{\n    program: {},\n}\n"
    res = testing.run(
        _cml_format,
        files = _tool_paths("cmc") | {
            "meta/bad.cml": "{program:{}}\n",
            "meta/good.cml": formatted,
            "README.md": "",
        },
        exec_mocks = [
            testing.exec_mock(cmd = [testing.any_args, "format", "meta/bad.cml"], stdout = formatted),
            testing.exec_mock(cmd = [testing.any_args, "format", "meta/good.cml"], stdout = formatted),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = FORMATTER_MSG,
            filepath = "meta/bad.cml",
            replacements = [formatted],
        ),
    ))
    asserts.eq(res.files["meta/bad.cml"], formatted)

def test_cml_format_unbuilt_tool():
    asserts.fails(
        lambda: testing.run(
            _cml_format,
            files = _tool_paths("cmc", built = False) | {
                "meta/foo.cml": "{}\n",
            },
        ),
        "Tool 'cmc' has not been built yet.",
    )

def test_cml_format_no_cml_files():
    res = testing.run(
        _cml_format,
        files = _tool_paths("cmc", built = False) | {
            "README.md": "",
        },
    )
    asserts.eq(res.findings, ())

def test_cml_format_missing_tool():
    asserts.fails(
        lambda: testing.run(
            _cml_format,
            files = _tool_paths("not-cmc") | {
                "meta/foo.cml": "{}\n",
            },
        ),
        "no such tool in tool_paths.json: cmc",
    )
