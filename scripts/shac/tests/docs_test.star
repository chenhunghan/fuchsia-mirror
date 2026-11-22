# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/docs.star."""

load("//scripts/shac/docs.star", "_codelinks", "_doc_checker")

# Split up so that the codelinks check doesn't flag this file.
_GOB_DOCS = "https://fuchsia.googlesource.com" + "/fuchsia/+/refs/heads/main/docs/"
_FUCHSIA_DEV = "https://fuchsia.dev/fuchsia-src/"

_DOC_CHECKER_TOOL_PATHS = testing.file(
    content = json.encode([
        {
            "name": "doc-checker",
            "os": os,
            "cpu": cpu,
            "path": "doc-checker",
            "label": "//tools/doc_checker:doc-checker",
        }
        for os in ["linux", "mac"]
        for cpu in ["x64", "arm64"]
    ]),
    affected = False,
)

def _doc_checker_finding(filepath, line, level, message, help_suggestion = None):
    return {
        "doc_line": {
            "file_name": "%s/%s" % (testing.root, filepath),
            "line_num": line,
        },
        "level": level,
        "message": message,
        "help_suggestion": help_suggestion,
    }

def _run_doc_checker(files, findings = (), retcode = 0, stderr = ""):
    return testing.run(
        _doc_checker,
        files = files | {
            "out/default/doc-checker": testing.file(affected = False),
            "out/default/tool_paths.json": _DOC_CHECKER_TOOL_PATHS,
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [testing.root + "/out/default/doc-checker", "--json", "--root", testing.root],
                retcode = retcode,
                stdout = json.encode(findings),
                stderr = stderr,
            ),
        ],
    )

def test_doc_checker_skipped_outside_fuchsia_git():
    # No exec_mocks: fails if doc-checker is invoked.
    res = testing.run(
        _doc_checker,
        vars = {"fuchsia_dir": "subpath"},
        files = {"docs/README.md": "# Title\n"},
    )
    asserts.eq(res.findings, ())

def test_doc_checker_skipped_on_non_doc_add_or_modify():
    # No exec_mocks: fails if doc-checker is invoked when non-doc files are only
    # added or modified, even with unaffected //docs files in the tree.
    res = testing.run(
        _doc_checker,
        files = {
            "docs/README.md": testing.file(affected = False),
            "docs/_toc.yaml": testing.file(affected = False),
            "src/foo/bar.cc": testing.file(action = "M"),
            "src/devices/bind/fuchsia/fuchsia.bind": testing.file(action = "A"),
        },
    )
    asserts.eq(res.findings, ())

def test_doc_checker_triggers_on_docs_and_filters_findings():
    res = _run_doc_checker(
        files = {
            "docs/affected.md": testing.file(action = "M"),
            "docs/unaffected.md": testing.file(affected = False),
        },
        retcode = 1,
        findings = [
            _doc_checker_finding(
                "docs/affected.md",
                10,
                "Info",
                "Informational note",
            ),
            _doc_checker_finding(
                "docs/affected.md",
                20,
                "Warning",
                "Warning on affected file",
                help_suggestion = "Try fixing this",
            ),
            _doc_checker_finding(
                "docs/unaffected.md",
                30,
                "Warning",
                "Suppressed warning on unaffected file",
            ),
            _doc_checker_finding(
                "docs/unaffected.md",
                40,
                "Error",
                "Propagated error on unaffected file",
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "notice",
            filepath = "docs/affected.md",
            line = 10,
            message = "Informational note\n\nRun `fx doc-checker` to reproduce.",
        ),
        testing.finding(
            level = "warning",
            filepath = "docs/affected.md",
            line = 20,
            message = "Warning on affected file\n\nTry fixing this\n\nRun `fx doc-checker` to reproduce.",
        ),
        testing.finding(
            level = "error",
            filepath = "docs/unaffected.md",
            line = 40,
            message = "Propagated error on unaffected file\n\nRun `fx doc-checker` to reproduce.",
        ),
    ))

def test_doc_checker_triggers_on_deleted_non_doc_file():
    msg = "in-tree link to /src/devices/bind/fuchsia/fuchsia.bind could not be found"
    res = _run_doc_checker(
        files = {
            "docs/bind.md": testing.file(affected = False),
            "src/devices/bind/fuchsia/fuchsia.bind": testing.file(action = "D"),
        },
        retcode = 1,
        findings = [
            _doc_checker_finding("docs/bind.md", 98, "Error", msg),
            _doc_checker_finding("docs/bind.md", 12, "Warning", "Ignored warning"),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            filepath = "docs/bind.md",
            line = 98,
            message = msg + "\n\nRun `fx doc-checker` to reproduce.",
        ),
    ))

def test_doc_checker_triggers_on_renamed_non_doc_file():
    msg = "in-tree link to /src/devices/bind/fuchsia/fuchsia.bind could not be found"
    res = _run_doc_checker(
        files = {
            "docs/bind.md": testing.file(affected = False),
            # git mv reports only the destination path with action = "R".
            "src/devices/bind/fuchsia/fuchsia_renamed.bind": testing.file(action = "R"),
        },
        retcode = 1,
        findings = [
            _doc_checker_finding("docs/bind.md", 98, "Error", msg),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            filepath = "docs/bind.md",
            line = 98,
            message = msg + "\n\nRun `fx doc-checker` to reproduce.",
        ),
    ))

def test_doc_checker_failure_without_findings():
    asserts.fails(
        lambda: _run_doc_checker(
            files = {"docs/README.md": "# Title\n"},
            findings = None,
            retcode = 1,
            stderr = "fatal error parsing args",
        ),
        "doc-checker failed:\nfatal error parsing args",
    )

def test_codelinks():
    res = testing.run(_codelinks, files = {
        "src/foo.cc": "// See %sfoo/bar.md.\n" % _GOB_DOCS,
    })
    url_len = len(_GOB_DOCS + "foo/bar.md")
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = (
                "Documentation links should point to fuchsia.dev rather than " +
                "fuchsia.googlesource.com. Consider changing this to %sfoo/bar." % _FUCHSIA_DEV
            ),
            filepath = "src/foo.cc",
            line = 1,
            col = 8,
            end_col = 8 + url_len,
            replacements = [_FUCHSIA_DEV + "foo/bar"],
        ),
    ))
    asserts.eq(res.files["src/foo.cc"], "// See %sfoo/bar.\n" % _FUCHSIA_DEV)

def test_codelinks_ignores_docs_dir():
    res = testing.run(_codelinks, files = {
        "docs/foo.md": "See %sfoo/bar.md.\n" % _GOB_DOCS,
    })
    asserts.eq(res.findings, ())

def test_codelinks_fuchsia_dev():
    res = testing.run(_codelinks, files = {
        "src/foo.cc": "// See %sfoo/bar.\n" % _FUCHSIA_DEV,
    })
    asserts.eq(res.findings, ())
