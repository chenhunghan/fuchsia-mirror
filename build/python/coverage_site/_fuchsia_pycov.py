# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia Python coverage runtime collector.

Activated when the FUCHSIA_PYTHON_COVERAGE_DIR environment variable is set to a
directory path. Collects line coverage using coverage.py and writes a compact
JSON file per process on exit for merging by covargs.
"""

import atexit
import json
import os
import sys
import uuid
import zipfile
from collections.abc import Mapping

_cov = None
_srcmap: dict[str, str] = {}
_dumped = False
_pyz_archive: str | None = None


def _find_pyz_archive(path: str) -> tuple[str, str] | None:
    low = path.lower()
    idx = low.find(".pyz" + os.sep)
    if idx == -1 and os.altsep:
        idx = low.find(".pyz" + os.altsep)
    if idx >= 0:
        return path[: idx + 4], path[idx + 5 :]
    return None


def _patch_coverage_pyz(
    coverage_files: object, coverage_python: object
) -> None:
    orig_zip_location = getattr(coverage_files, "zip_location", None)
    if orig_zip_location is None or getattr(
        orig_zip_location, "_fuchsia_patched", False
    ):
        return

    def _zip_location_with_pyz(filename: str) -> tuple[str, str] | None:
        res = orig_zip_location(filename)
        if res is not None:
            return res
        pyz_parts = _find_pyz_archive(filename)
        if pyz_parts is not None:
            zippath, inner = pyz_parts
            if os.path.isfile(zippath):
                return (zippath, inner)
        return None

    setattr(_zip_location_with_pyz, "_fuchsia_patched", True)
    setattr(coverage_files, "zip_location", _zip_location_with_pyz)
    setattr(coverage_python, "zip_location", _zip_location_with_pyz)


def _init_pyz_state() -> None:
    """Discovers the running .pyz archive and loads its bundled source map."""
    global _pyz_archive, _srcmap
    if _pyz_archive is not None and _srcmap:
        return
    pyz_parts = _find_pyz_archive(os.path.abspath(__file__))
    candidates = (
        *((pyz_parts[0],) if pyz_parts is not None else ()),
        *(
            os.path.abspath(p)
            for p in (*sys.path, *sys.argv[:1])
            if p.lower().endswith(".pyz")
        ),
    )
    fallback_pyz = _pyz_archive
    for candidate in candidates:
        if not os.path.isfile(candidate):
            continue
        if fallback_pyz is None:
            fallback_pyz = candidate
        try:
            with zipfile.ZipFile(candidate, "r") as zf:
                _srcmap = json.loads(
                    zf.read("_fuchsia_pycov_srcmap.json").decode("utf-8")
                )
            _pyz_archive = candidate
            return
        except KeyError:
            pass
        except (OSError, zipfile.BadZipFile, json.JSONDecodeError, ValueError):
            pass
    if _pyz_archive is None:
        _pyz_archive = fallback_pyz


def _child_site_entries(cov_dir: str) -> tuple[str, ...]:
    fuchsia_dir = os.environ.get("FUCHSIA_DIR")
    if fuchsia_dir:
        site_dir = os.path.join(fuchsia_dir, "build", "python", "coverage_site")
        cov_src_dir = os.path.join(
            fuchsia_dir, "third_party", "pylibs", "coverage", "src"
        )
        if os.path.isdir(site_dir) and os.path.isdir(cov_src_dir):
            return (site_dir, cov_src_dir)

    if not _pyz_archive or not os.path.isfile(_pyz_archive):
        return ()

    helper_zip = os.path.join(cov_dir, "_pycov_site.zip")
    if not os.path.exists(helper_zip):
        tmp_zip = os.path.join(cov_dir, f".tmp_pycov_site_{os.getpid()}.zip")
        try:
            os.makedirs(cov_dir, exist_ok=True)
            wrote_sitecustomize = False
            wrote_pycov = False
            wrote_coverage_init = False
            with (
                zipfile.ZipFile(_pyz_archive, "r") as src_zf,
                zipfile.ZipFile(
                    tmp_zip, "w", compression=zipfile.ZIP_DEFLATED
                ) as dst_zf,
            ):
                for name in src_zf.namelist():
                    base = os.path.basename(name)
                    if name in (
                        "sitecustomize.py",
                        "_fuchsia_pycov.py",
                        "coverage_site/sitecustomize.py",
                        "coverage_site/_fuchsia_pycov.py",
                    ):
                        dst_zf.writestr(base, src_zf.read(name))
                        if base == "sitecustomize.py":
                            wrote_sitecustomize = True
                        elif base == "_fuchsia_pycov.py":
                            wrote_pycov = True
                    elif name.startswith("coverage/"):
                        dst_zf.writestr(name, src_zf.read(name))
                        if name == "coverage/__init__.py":
                            wrote_coverage_init = True
            if wrote_sitecustomize and wrote_pycov and wrote_coverage_init:
                os.replace(tmp_zip, helper_zip)
        except (OSError, zipfile.BadZipFile) as err:
            print(
                f"WARNING: _fuchsia_pycov failed to write {helper_zip}: {err}",
                file=sys.stderr,
            )
        finally:
            if os.path.exists(tmp_zip):
                try:
                    os.remove(tmp_zip)
                except OSError:
                    pass
    return (helper_zip,) if os.path.exists(helper_zip) else ()


def _ensure_child_site_env(cov_dir: str) -> None:
    """Ensures child sys.executable subprocesses inherit coverage hooks."""
    entries = _child_site_entries(cov_dir)
    if not entries:
        return
    cur_parts = tuple(
        p for p in os.environ.get("PYTHONPATH", "").split(os.pathsep) if p
    )
    missing = tuple(p for p in entries if p not in cur_parts)
    if missing:
        os.environ["PYTHONPATH"] = os.pathsep.join((*missing, *cur_parts))


def start() -> None:
    global _cov
    cov_dir = os.environ.get("FUCHSIA_PYTHON_COVERAGE_DIR")
    if not cov_dir:
        return
    cov_dir = os.path.abspath(cov_dir)
    os.environ["FUCHSIA_PYTHON_COVERAGE_DIR"] = cov_dir

    # Skip test listing invocations that inspect test cases without running them.
    if any(
        arg
        in (
            "list_host_python_unittests",
            "--list_host_python_unittests",
            "--list_tests",
            "list_mobly_tests",
        )
        for arg in sys.argv
    ):
        return

    # Normalize .pyz entries on sys.path (and sys.argv[0] before CPython adds
    # it to sys.path[0]) as well as environment paths so resolution remains
    # valid even if the test calls os.chdir().
    if (
        sys.argv
        and sys.argv[0].lower().endswith(".pyz")
        and not os.path.isabs(sys.argv[0])
    ):
        sys.argv[0] = os.path.abspath(sys.argv[0])
    for i, p in enumerate(sys.path):
        if (
            p.lower().endswith(".pyz") or _find_pyz_archive(p) is not None
        ) and not os.path.isabs(p):
            sys.path[i] = os.path.abspath(p)
    for env_key in ("FUCHSIA_DIR", "RUNFILES_DIR", "RUNFILES_MANIFEST_FILE"):
        env_val = os.environ.get(env_key)
        if env_val and not os.path.isabs(env_val):
            os.environ[env_key] = os.path.abspath(env_val)

    _init_pyz_state()
    _ensure_child_site_env(cov_dir)

    if _cov is not None:
        return

    try:
        import coverage
        import coverage.exceptions
        import coverage.files
        import coverage.python

        _patch_coverage_pyz(coverage.files, coverage.python)

        omit_patterns = (
            "*lib_python*.zip/*",
            "*_pycov_site.zip/*",
            "*/third_party/*",
            "*/prebuilt/*",
            "*_stage2_bootstrap.py",
            "*_bazel_site_init.py",
            "*_fuchsia_pycov.py",
            "*/sitecustomize.py",
        )
        cov = coverage.Coverage(
            data_file=None,
            auto_data=False,
            config_file=False,
            branch=False,
            omit=list(omit_patterns),
        )
        cov.start()
        _cov = cov
        atexit.register(stop_and_dump)
    except Exception as err:
        _cov = None
        print(
            f"WARNING: _fuchsia_pycov failed to initialize coverage: {err}",
            file=sys.stderr,
        )


def _is_excluded_repo_path(repo_rel: str) -> bool:
    if repo_rel.startswith(
        (
            "out/",
            "third_party/",
            "prebuilt/",
            "bazel-out/",
            "external/",
            ".jiri/",
            ".git/",
        )
    ):
        return True
    if "/third_party/" in repo_rel or "/prebuilt/" in repo_rel:
        return True
    return os.path.basename(repo_rel) in (
        "_stage2_bootstrap.py",
        "_bazel_site_init.py",
        "_fuchsia_pycov.py",
        "sitecustomize.py",
    )


def _collect_runfiles_map() -> tuple[dict[str, str], dict[str, str]]:
    """Returns (path_to_repo_rel, repo_rel_to_target_path) for Bazel .runfiles/_main."""
    path_to_rel = {}
    rel_to_target = {}
    runfiles_dirs = set()
    env_rf = os.environ.get("RUNFILES_DIR")
    if env_rf:
        runfiles_dirs.add(env_rf)
    env_manifest = os.environ.get("RUNFILES_MANIFEST_FILE", "")
    if env_manifest.endswith(".runfiles/MANIFEST"):
        runfiles_dirs.add(env_manifest[: -len("/MANIFEST")])

    for rf_dir in runfiles_dirs:
        rf_main = os.path.join(rf_dir, "_main")
        if not os.path.isdir(rf_main):
            continue
        for root, dirs, files in os.walk(rf_main):
            dirs[:] = [
                d
                for d in dirs
                if d
                not in (
                    "third_party",
                    "prebuilt",
                    "out",
                    "external",
                    ".git",
                    "__pycache__",
                )
                and not d.endswith(".venv")
            ]
            for fname in files:
                if not fname.endswith(".py"):
                    continue
                full_path = os.path.join(root, fname)
                repo_rel = os.path.relpath(full_path, rf_main).replace(
                    "\\", "/"
                )
                if _is_excluded_repo_path(repo_rel):
                    continue
                path_to_rel[os.path.abspath(full_path)] = repo_rel
                real_p = os.path.realpath(full_path)
                path_to_rel[real_p] = repo_rel
                rel_to_target[repo_rel] = real_p
    return path_to_rel, rel_to_target


def _resolve_source_path(
    measured_path: str, runfiles_path_map: Mapping[str, str]
) -> str | None:
    # 1. GN .pyz zipimport paths: /abs/path/to/foo.pyz/module/file.py
    pyz_parts = _find_pyz_archive(measured_path)
    if pyz_parts is not None:
        rel_in_pyz = pyz_parts[1].replace("\\", "/")
        repo_rel = _srcmap.get(rel_in_pyz)
        if repo_rel and not _is_excluded_repo_path(repo_rel):
            return repo_rel
        return None

    # 2. Bazel runfiles paths (including symlink-resolved paths)
    for candidate in (
        os.path.abspath(measured_path),
        os.path.realpath(measured_path),
    ):
        if candidate in runfiles_path_map:
            return runfiles_path_map[candidate]

    norm_path = os.path.abspath(measured_path).replace("\\", "/")
    marker = ".runfiles/_main/"
    marker_idx = norm_path.find(marker)
    if marker_idx >= 0:
        repo_rel = norm_path[marker_idx + len(marker) :]
        if not _is_excluded_repo_path(repo_rel):
            return repo_rel
        return None

    # 3. Loose files under FUCHSIA_DIR
    fuchsia_dir = os.environ.get("FUCHSIA_DIR")
    if fuchsia_dir:
        for base_dir, candidate in (
            (os.path.abspath(fuchsia_dir), os.path.abspath(measured_path)),
            (os.path.realpath(fuchsia_dir), os.path.realpath(measured_path)),
        ):
            prefix = base_dir.replace("\\", "/").rstrip("/") + "/"
            norm_cand = candidate.replace("\\", "/")
            if norm_cand.startswith(prefix):
                repo_rel = norm_cand[len(prefix) :]
                if not _is_excluded_repo_path(repo_rel):
                    return repo_rel
                return None

    return None


def stop_and_dump() -> None:
    global _dumped
    if _dumped or _cov is None:
        return
    _dumped = True

    cov_dir = os.environ.get("FUCHSIA_PYTHON_COVERAGE_DIR")
    if not cov_dir:
        return

    import coverage

    try:
        _cov.stop()
        measured = _cov.get_data().measured_files()
    except (coverage.exceptions.CoverageException, OSError) as err:
        print(
            f"WARNING: _fuchsia_pycov failed to collect coverage data: {err}",
            file=sys.stderr,
        )
        return
    if not measured:
        return

    runfiles_path_map, runfiles_rel_to_target = _collect_runfiles_map()
    files_to_analyze = {}
    seen_repo_rels = set()

    for f in measured:
        repo_rel = _resolve_source_path(f, runfiles_path_map)
        if repo_rel:
            files_to_analyze[f] = repo_rel
            seen_repo_rels.add(repo_rel)

    if not files_to_analyze:
        return

    # Include unimported first-party modules bundled into this .pyz so they
    # appear with 0% coverage.
    if _srcmap and _pyz_archive:
        for rel_in_pyz, repo_rel in _srcmap.items():
            if repo_rel not in seen_repo_rels and not _is_excluded_repo_path(
                repo_rel
            ):
                files_to_analyze[
                    os.path.join(_pyz_archive, rel_in_pyz)
                ] = repo_rel
                seen_repo_rels.add(repo_rel)

    # Include unimported first-party modules in Bazel .runfiles/_main.
    for repo_rel, target_path in runfiles_rel_to_target.items():
        if repo_rel not in seen_repo_rels:
            files_to_analyze[target_path] = repo_rel
            seen_repo_rels.add(repo_rel)

    merged_by_repo: dict[str, tuple[set[int], set[int]]] = {}
    for target_path, repo_rel in files_to_analyze.items():
        try:
            _, statements, _, missing, _ = _cov.analysis2(target_path)
        except (coverage.exceptions.CoverageException, OSError):
            continue
        if not statements:
            continue
        stmt_set = set(statements)
        exec_set = stmt_set - set(missing)
        prev_exec, prev_stmts = merged_by_repo.get(repo_rel, (set(), set()))
        merged_by_repo[repo_rel] = (prev_exec | exec_set, prev_stmts | stmt_set)

    out_files = {
        repo_rel: {
            "executed_lines": sorted(exec_lines),
            "missing_lines": sorted(stmt_lines - exec_lines),
        }
        for repo_rel, (exec_lines, stmt_lines) in merged_by_repo.items()
    }
    if not out_files:
        return

    tag = f"{os.getpid()}_{uuid.uuid4().hex[:8]}"
    tmp_path = os.path.join(cov_dir, f".tmp_pycov_{tag}.json")
    out_path = os.path.join(cov_dir, f"pycov_{tag}.json")
    try:
        os.makedirs(cov_dir, exist_ok=True)
        with open(tmp_path, "w", encoding="utf-8") as fp:
            json.dump({"files": out_files}, fp)
        os.replace(tmp_path, out_path)
    except (OSError, TypeError, ValueError) as err:
        print(
            f"WARNING: _fuchsia_pycov failed to write {out_path}: {err}",
            file=sys.stderr,
        )
    finally:
        if os.path.exists(tmp_path):
            try:
                os.remove(tmp_path)
            except OSError:
                pass
