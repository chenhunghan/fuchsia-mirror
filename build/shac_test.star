# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //build/shac.star."""

load(
    "//build/shac.star",
    "_build_python_type_check_coverage",
    "_find_type_checked_sources",
    "_resolve",
)

# Helpers for constructing nodes of the AST produced by
# `gn format --dump-tree=json`.

def _str(s):
    return {"type": "LITERAL", "value": "\"%s\"" % s}

def _list(*items):
    return {"type": "LIST", "child": list(items)}

def _assign(name, value, op = "="):
    return {
        "type": "BINARY",
        "value": op,
        "child": [{"type": "IDENTIFIER", "value": name}, value],
    }

def _target(template, name, *stmts):
    return {
        "type": "FUNCTION",
        "value": template,
        "child": [_list(_str(name)), {"type": "BLOCK", "child": list(stmts)}],
    }

def _file(*stmts):
    return {"type": "BLOCK", "child": list(stmts)}

def test_resolve():
    cases = [
        ("", "foo.py", "foo.py"),
        ("scripts", "foo.py", "scripts/foo.py"),
        ("scripts", "../foo.py", "foo.py"),
        ("scripts", "./a/../b.py", "scripts/b.py"),
        ("scripts", "//build/foo.py", "foo.py"),
        ("scripts", "//build", ""),
        ("", "//src/foo.py", None),
        ("", "/abs/foo.py", None),
        ("", "../foo.py", None),
    ]
    for base_dir, path, want in cases:
        asserts.eq(_resolve(base_dir, path), want, msg = "_resolve(%r, %r)" % (base_dir, path))

def test_find_type_checked_sources():
    tree = _file(
        _target(
            "python_library",
            "lib",
            _assign("sources", _list(_str("a.py"), _str("b.txt"), _str("$gen_dir/c.py"))),
        ),
        _target("python_binary", "bin", _assign("main_source", _str("main.py"))),
        _target(
            "python_host_test",
            "test",
            _assign("main_source", _str("test.py")),
            _assign("sources", _list(_str("helper.py"))),
            _assign("sources", _list(_str("more.py")), op = "+="),
        ),
        _target(
            "python_build_time_tests",
            "build_time",
            _assign("tests", _list(_str("t1.py"))),
            _assign("inputs", _list(_str("//build/other.py"), _str("//src/outside.py"))),
        ),
        _target(
            "python_mobly_test",
            "mobly",
            _assign("main_source", _str("mobly.py")),
            _assign("enable_mypy", {"type": "LITERAL", "value": "false"}),
        ),
        # Not type checked.
        _target("action", "act", _assign("script", _str("script.py"))),
    )
    got = _find_type_checked_sources(tree, "dir")
    asserts.eq(sorted(got), sorted([
        "dir/a.py",
        "dir/main.py",
        "dir/test.py",
        "dir/helper.py",
        "dir/more.py",
        "dir/t1.py",
        "other.py",
        "dir/mobly.py",
    ]))

def test_find_type_checked_sources_concatenation():
    tree = _file(_target(
        "python_library",
        "lib",
        _assign("sources", {
            "type": "BINARY",
            "value": "+",
            "child": [_list(_str("a.py")), _list(_str("b.py"))],
        }),
    ))
    asserts.eq(_find_type_checked_sources(tree, ""), ["a.py", "b.py"])

def test_find_type_checked_sources_source_root():
    tree = _file(
        _target(
            "python_library",
            "lib",
            _assign("source_root", _str("py")),
            _assign("sources", _list(_str("pkg/a.py"))),
        ),
        _target(
            "python_library",
            "outside",
            _assign("source_root", _str("//src/py")),
            _assign("sources", _list(_str("b.py"))),
        ),
    )
    asserts.eq(_find_type_checked_sources(tree, "dir"), ["dir/py/pkg/a.py"])

def test_find_type_checked_sources_nested():
    tree = _file({
        "type": "CONDITION",
        "child": [
            {"type": "IDENTIFIER", "value": "is_host"},
            {
                "type": "BLOCK",
                "child": [
                    _target("python_library", "lib", _assign("sources", _list(_str("a.py")))),
                ],
            },
        ],
    })
    asserts.eq(_find_type_checked_sources(tree, ""), ["a.py"])

def _coverage_finding(path):
    return testing.finding(
        level = "error",
        message = (
            "Python files under //build must be type checked. " +
            "Add this file to the `sources` of a GN Python target such " +
            "as a python_library(), python_binary(), python_host_test() " +
            "or python_build_time_tests(), or to " +
            "//build:type_checked_scripts if it isn't part of any other " +
            "target. Sources must be listed as string literals in the " +
            "target itself for this check to see them."
        ),
        filepath = path,
    )

# These tests exec the real prebuilt gn binary.

def test_build_python_type_check_coverage():
    res = testing.run(
        _build_python_type_check_coverage,
        subdir = "build",
        files = {
            "BUILD.gn": testing.file(
                content = """\
python_library("type_checked_scripts") {
  sources = [ "tracked.py" ]
}
""",
                affected = False,
            ),
            "tracked.py": "",
            "untracked.py": testing.file(action = "A"),
            "__init__.py": testing.file(affected = False),
            "foo_pb2.py": testing.file(affected = False),
            "sub/BUILD.gn": testing.file(
                content = """\
python_host_test("test") {
  main_source = "test.py"
  sources = [ "//build/sub/lib.py" ]
}
""",
                affected = False,
            ),
            "sub/test.py": testing.file(affected = False),
            "sub/lib.py": testing.file(affected = False),
            "sub/script.py": testing.file(affected = False),
        },
    )
    asserts.eq(res.findings, (
        _coverage_finding("sub/script.py"),
        _coverage_finding("untracked.py"),
    ))

def test_build_python_type_check_coverage_unaffected():
    res = testing.run(
        _build_python_type_check_coverage,
        subdir = "build",
        files = {
            "BUILD.gn": testing.file(content = "", affected = False),
            "untracked.py": testing.file(affected = False),
            "README.md": "",
        },
    )
    asserts.eq(res.findings, ())
