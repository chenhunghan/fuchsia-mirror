# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Standalone tests for the Fuchsia Python coverage runtime collector."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from unittest import mock

import coverage
import coverage_site._fuchsia_pycov as pycov


def _write_file(path: str, content: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fp:
        fp.write(content)


def _read_merged_pycov(cov_dir: str) -> dict[str, dict[str, set[int]]]:
    merged: dict[str, dict[str, set[int]]] = {}
    for name in os.listdir(cov_dir):
        if name.startswith("pycov_") and name.endswith(".json"):
            with open(os.path.join(cov_dir, name), "r", encoding="utf-8") as fp:
                for path, info in json.load(fp)["files"].items():
                    entry = merged.setdefault(
                        path, {"executed": set(), "missing": set()}
                    )
                    entry["executed"].update(info["executed_lines"])
                    entry["missing"].update(info["missing_lines"])
    return merged


def _resolve_pkg_dir(module_file: str | None, extract_dir: str) -> str:
    """Returns an on-disk package directory from the source tree or a .pyz."""
    assert module_file is not None
    if os.path.isfile(module_file):
        return os.path.dirname(module_file)
    lower = module_file.lower()
    idx = lower.find(".pyz" + os.sep)
    if idx < 0 and os.altsep:
        idx = lower.find(".pyz" + os.altsep)
    assert idx >= 0, f"Cannot resolve package directory for {module_file!r}"
    archive_path = module_file[: idx + 4]
    inner_dir = os.path.dirname(module_file[idx + 5 :].replace("\\", "/"))
    prefix = f"{inner_dir}/" if inner_dir else ""
    with zipfile.ZipFile(archive_path, "r") as zf:
        for member in zf.infolist():
            if member.is_dir() or not member.filename.startswith(prefix):
                continue
            rel = member.filename[len(prefix) :]
            if not rel or rel.endswith(".pyc"):
                continue
            dest = os.path.join(extract_dir, rel.replace("/", os.sep))
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with open(dest, "wb") as out_f:
                out_f.write(zf.read(member.filename))
    return extract_dir


def _copy_coverage_site(dst_dir: str, src_dir: str) -> str:
    os.makedirs(dst_dir, exist_ok=True)
    for fname in ("_fuchsia_pycov.py", "sitecustomize.py"):
        shutil.copy2(os.path.join(src_dir, fname), os.path.join(dst_dir, fname))
    return dst_dir


class TestFuchsiaPycov(unittest.TestCase):
    def test_sitecustomize_collects_and_propagates_with_fuchsia_dir(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as td:
            fake_fuchsia = os.path.join(td, "fuchsia")
            pycov_src_dir = _resolve_pkg_dir(
                pycov.__file__, os.path.join(td, "_pycov_src")
            )
            site_dir = _copy_coverage_site(
                os.path.join(fake_fuchsia, "build", "python", "coverage_site"),
                pycov_src_dir,
            )
            cov_pkg_dir = _resolve_pkg_dir(
                coverage.__file__, os.path.join(td, "_cov_src", "coverage")
            )
            cov_parent_dir = os.path.dirname(cov_pkg_dir)
            cov_link_parent = os.path.join(
                fake_fuchsia, "third_party", "pylibs", "coverage", "src"
            )
            os.makedirs(cov_link_parent, exist_ok=True)
            os.symlink(cov_pkg_dir, os.path.join(cov_link_parent, "coverage"))

            lib_py = os.path.join(fake_fuchsia, "src", "demo", "lib.py")
            child_py = os.path.join(fake_fuchsia, "src", "demo", "child.py")
            app_py = os.path.join(fake_fuchsia, "src", "demo", "app.py")
            _write_file(lib_py, "def helper():\n    return 42\n")
            _write_file(child_py, "def child_fn():\n    return 7\nchild_fn()\n")
            _write_file(
                app_py,
                (
                    "import sys\n"
                    f"sys.path.insert(0, {os.path.dirname(lib_py)!r})\n"
                    "import lib\n"
                    "lib.helper()\n"
                ),
            )

            cov_out = os.path.join(td, "cov_site")
            proc = subprocess.run(
                [sys.executable, app_py],
                env={
                    "PATH": os.environ.get("PATH", ""),
                    "FUCHSIA_DIR": fake_fuchsia,
                    "FUCHSIA_PYTHON_COVERAGE_DIR": cov_out,
                    "PYTHONPATH": os.pathsep.join((site_dir, cov_parent_dir)),
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            merged = _read_merged_pycov(cov_out)
            self.assertIn("src/demo/app.py", merged)
            self.assertIn("src/demo/lib.py", merged)
            self.assertTrue(merged["src/demo/lib.py"]["executed"])

            # Verify -S parent startup propagates PYTHONPATH via FUCHSIA_DIR
            # to a normal child process without pre-populated PYTHONPATH.
            parent_dash_s = os.path.join(
                fake_fuchsia, "src", "demo", "parent_dash_s.py"
            )
            _write_file(
                parent_dash_s,
                (
                    "import subprocess, sys\n"
                    f"sys.path[:0] = [{site_dir!r}, {cov_parent_dir!r}]\n"
                    "import _fuchsia_pycov\n"
                    "def run_parent() -> None:\n"
                    f"    subprocess.run([sys.executable, {child_py!r}], check=True)\n"
                    "_fuchsia_pycov.start()\n"
                    "try:\n"
                    "    run_parent()\n"
                    "finally:\n"
                    "    _fuchsia_pycov.stop_and_dump()\n"
                ),
            )
            cov_out_child = os.path.join(td, "cov_child")
            proc_child = subprocess.run(
                [sys.executable, "-S", parent_dash_s],
                env={
                    "PATH": os.environ.get("PATH", ""),
                    "FUCHSIA_DIR": fake_fuchsia,
                    "FUCHSIA_PYTHON_COVERAGE_DIR": cov_out_child,
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc_child.returncode, 0, proc_child.stderr)
            merged_child = _read_merged_pycov(cov_out_child)
            self.assertIn("src/demo/parent_dash_s.py", merged_child)
            self.assertIn("src/demo/child.py", merged_child)
            self.assertTrue(merged_child["src/demo/child.py"]["executed"])

    def test_synthetic_pyz_srcmap_unimported_and_pycov_site_zip_child(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as td:
            cov_pkg_dir = _resolve_pkg_dir(
                coverage.__file__, os.path.join(td, "_cov_src", "coverage")
            )
            site_dir = _resolve_pkg_dir(
                pycov.__file__, os.path.join(td, "_pycov_src")
            )
            child_rf_dir = os.path.join(td, "child.runfiles")
            child_script = os.path.join(
                child_rf_dir, "_main", "src", "child.py"
            )
            _write_file(
                child_script,
                "def child_fn():\n    return 1\n\nchild_fn()\n",
            )

            pyz_path = os.path.join(td, "synthetic.pyz")
            srcmap = {
                "demo_pkg/__init__.py": "src/demo_pkg/__init__.py",
                "demo_pkg/used.py": "src/demo_pkg/used.py",
                "demo_pkg/unused.py": "src/demo_pkg/unused.py",
            }
            with zipfile.ZipFile(pyz_path, "w") as zf:
                for root, _, files in os.walk(cov_pkg_dir):
                    for fname in files:
                        if fname.endswith(".py"):
                            full = os.path.join(root, fname)
                            rel = os.path.relpath(full, cov_pkg_dir).replace(
                                os.sep, "/"
                            )
                            zf.write(full, f"coverage/{rel}")
                for fname in ("_fuchsia_pycov.py", "sitecustomize.py"):
                    zf.write(os.path.join(site_dir, fname), fname)
                zf.writestr(
                    "_fuchsia_pycov_srcmap.json",
                    json.dumps(srcmap, sort_keys=True),
                )
                zf.writestr("demo_pkg/__init__.py", "")
                zf.writestr("demo_pkg/used.py", "def run():\n    return 10\n")
                zf.writestr(
                    "demo_pkg/unused.py", "def untouched():\n    return 99\n"
                )
                zf.writestr(
                    "__main__.py",
                    (
                        "import os, subprocess, sys\n"
                        "import _fuchsia_pycov\n"
                        "_fuchsia_pycov.start()\n"
                        "try:\n"
                        "    from demo_pkg import used\n"
                        "    used.run()\n"
                        f"    os.chdir({os.path.dirname(child_rf_dir)!r})\n"
                        "    env = dict(os.environ)\n"
                        f"    env['RUNFILES_DIR'] = {child_rf_dir!r}\n"
                        f"    subprocess.run([sys.executable, {child_script!r}], env=env, check=True)\n"
                        "finally:\n"
                        "    _fuchsia_pycov.stop_and_dump()\n"
                    ),
                )

            cov_out = os.path.join(td, "cov_out")
            # Run with relative `.pyz` path, `-S`, and no FUCHSIA_DIR/PYTHONPATH
            # to verify zip sys.path normalization, _srcmap, and _pycov_site.zip.
            proc = subprocess.run(
                [sys.executable, "-S", "synthetic.pyz"],
                env={
                    "PATH": os.environ.get("PATH", ""),
                    "FUCHSIA_PYTHON_COVERAGE_DIR": cov_out,
                },
                cwd=td,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertTrue(
                os.path.isfile(os.path.join(cov_out, "_pycov_site.zip"))
            )

            merged = _read_merged_pycov(cov_out)
            self.assertIn("src/demo_pkg/used.py", merged)
            self.assertIn("src/demo_pkg/unused.py", merged)
            self.assertIn("src/child.py", merged)
            self.assertTrue(merged["src/demo_pkg/used.py"]["executed"])
            self.assertEqual(
                merged["src/demo_pkg/unused.py"]["executed"], set()
            )
            self.assertTrue(merged["src/demo_pkg/unused.py"]["missing"])
            self.assertTrue(merged["src/child.py"]["executed"])
            self.assertNotIn(
                "build/python/coverage_site/_fuchsia_pycov.py", merged
            )

    def test_noop_when_env_unset_or_listing_tests(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            pycov_src_dir = _resolve_pkg_dir(
                pycov.__file__, os.path.join(td, "_pycov_src")
            )
            site_dir = _copy_coverage_site(
                os.path.join(td, "site"), pycov_src_dir
            )
            cov_pkg_dir = _resolve_pkg_dir(
                coverage.__file__, os.path.join(td, "_cov_src", "coverage")
            )
            cov_parent_dir = os.path.dirname(cov_pkg_dir)
            script = os.path.join(td, "simple.py")
            _write_file(script, "x = 1 + 2\n")
            pypath = os.pathsep.join((site_dir, cov_parent_dir))

            proc_unset = subprocess.run(
                [sys.executable, script],
                env={"PATH": os.environ.get("PATH", ""), "PYTHONPATH": pypath},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc_unset.returncode, 0, proc_unset.stderr)

            for list_arg in ("--list_tests", "list_host_python_unittests"):
                cov_out = os.path.join(td, f"cov_{list_arg.lstrip('-')}")
                proc_list = subprocess.run(
                    [sys.executable, script, list_arg],
                    env={
                        "PATH": os.environ.get("PATH", ""),
                        "PYTHONPATH": pypath,
                        "FUCHSIA_PYTHON_COVERAGE_DIR": cov_out,
                    },
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(proc_list.returncode, 0, proc_list.stderr)
                self.assertFalse(os.path.exists(cov_out))

    def test_runfiles_resolution_chdir_and_loose_file_mapping(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            fuchsia_dir = os.path.join(td, "fuchsia")
            rf_dir = os.path.join(td, "test.runfiles")
            rf_main = os.path.join(rf_dir, "_main")
            os.makedirs(os.path.join(rf_main, "src", "pkg"), exist_ok=True)
            real_py = os.path.join(fuchsia_dir, "src", "pkg", "mod.py")
            _write_file(real_py, "def f():\n    return 1\n")
            rf_py = os.path.join(rf_main, "src", "pkg", "mod.py")
            os.symlink(real_py, rf_py)

            loose_py = os.path.join(fuchsia_dir, "scripts", "tool.py")
            _write_file(loose_py, "x = 1\n")

            with mock.patch.dict(
                os.environ,
                {"RUNFILES_DIR": rf_dir, "FUCHSIA_DIR": fuchsia_dir},
                clear=False,
            ):
                path_to_rel, rel_to_target = pycov._collect_runfiles_map()
                self.assertEqual(
                    pycov._resolve_source_path(rf_py, path_to_rel),
                    "src/pkg/mod.py",
                )
                self.assertEqual(
                    pycov._resolve_source_path(real_py, path_to_rel),
                    "src/pkg/mod.py",
                )
                self.assertIn("src/pkg/mod.py", rel_to_target)
                self.assertEqual(
                    pycov._resolve_source_path(loose_py, path_to_rel),
                    "scripts/tool.py",
                )

    def test_exclusion_uses_basename_and_child_zip_requires_helpers(
        self,
    ) -> None:
        self.assertFalse(
            pycov._is_excluded_repo_path("src/foo/test_sitecustomize.py")
        )
        self.assertTrue(
            pycov._is_excluded_repo_path(
                "build/python/coverage_site/sitecustomize.py"
            )
        )
        self.assertTrue(pycov._is_excluded_repo_path(".git/hooks/pre.py"))
        self.assertFalse(pycov._is_excluded_repo_path("src/.github_helper.py"))

        with tempfile.TemporaryDirectory() as td:
            empty_pyz = os.path.join(td, "no_cov.pyz")
            with zipfile.ZipFile(empty_pyz, "w") as zf:
                zf.writestr("__main__.py", "print('hi')\n")
            cov_out = os.path.join(td, "cov_out")
            with (
                mock.patch.object(pycov, "_pyz_archive", empty_pyz),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                self.assertEqual(pycov._child_site_entries(cov_out), ())
                self.assertFalse(
                    os.path.exists(os.path.join(cov_out, "_pycov_site.zip"))
                )

            prefixed_pyz = os.path.join(td, "prefixed.pyz")
            with zipfile.ZipFile(prefixed_pyz, "w") as zf:
                zf.writestr("coverage_site/sitecustomize.py", "")
                zf.writestr("coverage_site/_fuchsia_pycov.py", "")
                zf.writestr("coverage/__init__.py", "")
            with (
                mock.patch.object(pycov, "_pyz_archive", prefixed_pyz),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                entries = pycov._child_site_entries(cov_out)
                helper_zip = os.path.join(cov_out, "_pycov_site.zip")
                self.assertEqual(entries, (helper_zip,))
                with zipfile.ZipFile(helper_zip, "r") as zf:
                    names = set(zf.namelist())
                self.assertIn("sitecustomize.py", names)
                self.assertIn("_fuchsia_pycov.py", names)
                self.assertIn("coverage/__init__.py", names)


if __name__ == "__main__":
    unittest.main()
