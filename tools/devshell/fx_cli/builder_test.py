# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for fx_cli.builder."""

import hashlib
import inspect
import io
import os
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile
from collections.abc import Mapping
from pathlib import Path
from typing import Self, TextIO
from unittest import mock

import builder
import test_utils
from build.scripts import lock_utils


def _git_blob_sha1(data: bytes) -> str:
    header = f"blob {len(data)}\0".encode("ascii")
    return hashlib.sha1(header + data).hexdigest()


def _parse_nul_env_dump(path: Path) -> dict[str, str]:
    env: dict[str, str] = {}
    for entry in path.read_bytes().split(b"\0"):
        if not entry:
            continue
        key, sep, value = entry.decode("utf-8").partition("=")
        if sep:
            env[key] = value
    return env


class _BuilderTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._tmp.name).resolve()
        (self.fuchsia_dir / ".fx-root").write_text("", encoding="utf-8")
        (self.fuchsia_dir / "out").mkdir(parents=True, exist_ok=True)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _make_context(
        self,
        *,
        fuchsia_dir: Path | None = None,
        build_dir: Path | None = None,
        env: Mapping[str, str] | None = None,
        stderr: TextIO | None = None,
    ) -> builder.BuildContext:
        return builder.BuildContext(
            fuchsia_dir=fuchsia_dir or self.fuchsia_dir,
            build_dir=build_dir,
            env={} if env is None else env,
            stderr=io.StringIO() if stderr is None else stderr,
        )

    def _stage_tool(
        self,
        cmd_name: str,
        *,
        install_dir: Path | None = None,
        content: str = "#!/bin/sh\n",
    ) -> Path:
        target_dir = (
            install_dir
            if install_dir is not None
            else (self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR)
        )
        target_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = target_dir / cmd_name
        tool_bin.write_text(content, encoding="utf-8")
        tool_bin.chmod(0o755)
        return tool_bin


class BuilderTest(_BuilderTestBase):
    def test_build_context_paths_and_resolution(self) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "out/default\n", encoding="utf-8"
        )

        ctx = self._make_context()
        self.assertEqual(
            ctx.fx_reentry_script,
            self.fuchsia_dir / builder._FX_REENTRY_REL_PATH,
        )
        self.assertEqual(ctx.out_dir, self.fuchsia_dir / "out")
        self.assertEqual(
            ctx.install_dir(),
            self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR,
        )
        self.assertEqual(
            ctx.install_dir(vendor="example"),
            self.fuchsia_dir
            / builder._STAGED_HOST_TOOLS_REL_DIR
            / "vendor/example",
        )
        self.assertEqual(ctx.resolve_build_dir(), build_dir)

        # Custom FUCHSIA_OUT_DIR overrides out_dir and install_dir.
        custom_out = self.fuchsia_dir / "custom_out"
        custom_ctx = self._make_context(
            env={"FUCHSIA_OUT_DIR": str(custom_out)}
        )
        self.assertEqual(custom_ctx.out_dir, custom_out)
        self.assertEqual(
            custom_ctx.install_dir(),
            custom_out / builder._STAGED_HOST_TOOLS_DIR_NAME,
        )
        self.assertEqual(
            custom_ctx.install_dir(vendor="example"),
            custom_out / builder._STAGED_HOST_TOOLS_DIR_NAME / "vendor/example",
        )

        # with_build_dir normalizes relative and //-prefixed paths immutably.
        slashed_dir = self.fuchsia_dir / "out/slashed"
        slashed_dir.mkdir(parents=True, exist_ok=True)
        updated_ctx = ctx.with_build_dir(Path("//out/slashed"))
        self.assertIsNone(ctx.build_dir)
        self.assertEqual(updated_ctx.build_dir, slashed_dir)
        self.assertEqual(updated_ctx.resolve_build_dir(), slashed_dir)

        # host_bazel_args_for_marker returns None unless marker exists.
        marker = self.fuchsia_dir / builder._ALWAYS_BUILD_MARKER
        self.assertIsNone(ctx.host_bazel_args_for_marker(marker))
        marker.write_text("", encoding="utf-8")
        self.assertEqual(
            ctx.host_bazel_args_for_marker(marker),
            build_dir / builder._HOST_BAZEL_ARGS_REL_PATH,
        )

    def test_script_mode_python_resolution(self) -> None:
        script_rel = "tools/my_tool/run.py"
        script_abs = self.fuchsia_dir / script_rel
        script_abs.parent.mkdir(parents=True, exist_ok=True)
        script_abs.write_text("#!/usr/bin/env python3\n", encoding="utf-8")

        entry = {
            "script": f"//{script_rel}",
            "category": "Build",
            "summary": "Test python script tool",
        }
        stderr_buf = io.StringIO()
        resolved = builder.ensure_registered_host_tool(
            self._make_context(
                env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
                stderr=stderr_buf,
            ),
            "my-tool",
            entry=entry,
        )
        self.assertEqual(resolved, script_abs)
        self.assertIn(
            "[fx-registry] skip build 'my-tool': script mode, running //tools/my_tool/run.py directly",
            stderr_buf.getvalue(),
        )

    def test_script_mode_missing_or_non_executable_fails(self) -> None:
        entry_py = {
            "script": "//tools/missing.py",
            "category": "Build",
        }
        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self._make_context(), "missing-py", entry=entry_py
            )

        sh_rel = "tools/non_exec.sh"
        sh_abs = self.fuchsia_dir / sh_rel
        sh_abs.parent.mkdir(parents=True, exist_ok=True)
        sh_abs.write_text("#!/bin/bash\n", encoding="utf-8")
        sh_abs.chmod(0o644)

        entry_sh = {
            "script": f"//{sh_rel}",
            "category": "Build",
        }
        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self._make_context(), "non-exec", entry=entry_sh
            )

    def test_target_mode_stamp_hit_fast_path(self) -> None:
        target = "//tools/my_tool:my-tool"
        entry = {
            "target": target,
            "category": "Build",
        }
        src_file = self.fuchsia_dir / "tools/my_tool/main.py"
        src_file.parent.mkdir(parents=True, exist_ok=True)
        src_bytes = b"print('hello')\n"
        src_file.write_bytes(src_bytes)
        src_hash = _git_blob_sha1(src_bytes)

        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        tool_bin = self._stage_tool(
            "my-tool", install_dir=install_dir, content="#!/bin/bash\necho ok\n"
        )

        rt_dir = install_dir / f"my-tool{builder._RUNTIME_DIR_SUFFIX}"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / builder._SOURCES_MANIFEST).write_text(
            f"{src_hash}  tools/my_tool/main.py\n", encoding="utf-8"
        )

        stamp_dir = install_dir / ".stamps"
        stamp_dir.mkdir(parents=True, exist_ok=True)
        stamp_hash = builder.compute_stamp_hash(self.fuchsia_dir, target)
        (stamp_dir / "my-tool.stamp").write_text(
            f"{stamp_hash}\n", encoding="utf-8"
        )

        stderr_buf = io.StringIO()
        resolved = builder.ensure_registered_host_tool(
            self._make_context(
                env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
                stderr=stderr_buf,
            ),
            "my-tool",
            entry=entry,
        )
        self.assertEqual(resolved, tool_bin)
        self.assertIn(
            f"[fx-registry] skip build 'my-tool' (//tools/my_tool:my-tool): up-to-date (stamp and {builder._SOURCES_MANIFEST} valid), using //{builder._STAGED_HOST_TOOLS_REL_DIR}/my-tool",
            stderr_buf.getvalue(),
        )

    def test_compute_stamp_hash_matches_canonical_sha256_format(self) -> None:
        target = "//tools/my_tool:my-tool"
        (self.fuchsia_dir / "manifests").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "manifests/jiri.lock").write_text(
            "lock-v1\n", encoding="utf-8"
        )
        (self.fuchsia_dir / "tools/my_tool").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "tools/my_tool/BUILD.bazel").write_text(
            "# build v1\n", encoding="utf-8"
        )
        stamp1 = builder.compute_stamp_hash(self.fuchsia_dir, target)
        self.assertTrue(
            stamp1.startswith(f"{target}|sha256:"),
            f"Unexpected stamp format: {stamp1}",
        )
        digest_part = stamp1.split("|sha256:", 1)[1]
        self.assertEqual(len(digest_part), 64)

        # Changing BUILD.bazel changes the stamp digest
        (self.fuchsia_dir / "tools/my_tool/BUILD.bazel").write_text(
            "# build v2\n", encoding="utf-8"
        )
        stamp2 = builder.compute_stamp_hash(self.fuchsia_dir, target)
        self.assertNotEqual(stamp1, stamp2)

    def test_compute_stamp_hash_falls_back_to_unknown_when_git_missing(
        self,
    ) -> None:
        target = "//tools/my_tool:my-tool"
        with mock.patch(
            "subprocess.run",
            side_effect=FileNotFoundError(
                2, "No such file or directory", "git"
            ),
        ):
            stamp = builder.compute_stamp_hash(self.fuchsia_dir, target)
        self.assertEqual(stamp, f"{target}|unknown")

    def test_compute_stamp_hash_includes_host_bazel_args(self) -> None:
        target = "//tools/my_tool:my-tool"
        (self.fuchsia_dir / "manifests").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "manifests/jiri.lock").write_text(
            "lock-v1\n", encoding="utf-8"
        )
        (self.fuchsia_dir / "tools/my_tool").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "tools/my_tool/BUILD.bazel").write_text(
            "# build v1\n", encoding="utf-8"
        )
        host_args_file = (
            self.fuchsia_dir / "out/default" / builder._HOST_BAZEL_ARGS_REL_PATH
        )
        host_args_file.parent.mkdir(parents=True, exist_ok=True)
        host_args_file.write_text('{"select_variant": []}\n', encoding="utf-8")

        # 1. Base stamp with host_bazel_args=None vs host_bazel_args=host_args_file
        stamp_none = builder.compute_stamp_hash(self.fuchsia_dir, target)
        stamp_with_args = builder.compute_stamp_hash(
            self.fuchsia_dir, target, host_bazel_args=host_args_file
        )
        self.assertNotEqual(stamp_none, stamp_with_args)

        # 2. Modifying host.json changes the computed stamp
        host_args_file.write_text(
            '{"select_variant": ["host_asan"]}\n', encoding="utf-8"
        )
        stamp_with_asan = builder.compute_stamp_hash(
            self.fuchsia_dir, target, host_bazel_args=host_args_file
        )
        self.assertNotEqual(stamp_with_args, stamp_with_asan)

        # 3. Test casfs.hash xattr fast path incorporates host_bazel_args without calling getxattr on host.json
        xattrs_queried: list[str] = []

        def fake_getxattr(
            path: os.PathLike[str] | str | bytes | int, name: str
        ) -> bytes:
            path_str = str(path)
            xattrs_queried.append(path_str)
            if "host.json" in path_str:
                raise OSError(95, "Operation not supported on out/")
            return b"deadbeef" * 5 + b"\n"

        with mock.patch("os.getxattr", side_effect=fake_getxattr):
            casfs_stamp1 = builder.compute_stamp_hash(
                self.fuchsia_dir, target, host_bazel_args=host_args_file
            )
            self.assertTrue(casfs_stamp1.startswith(f"{target}|sha256:"))
            self.assertFalse(any("host.json" in p for p in xattrs_queried))

            host_args_file.write_text(
                '{"select_variant": []}\n', encoding="utf-8"
            )
            casfs_stamp2 = builder.compute_stamp_hash(
                self.fuchsia_dir, target, host_bazel_args=host_args_file
            )
            self.assertTrue(casfs_stamp2.startswith(f"{target}|sha256:"))
            self.assertNotEqual(casfs_stamp1, casfs_stamp2)

    def test_file_git_hash_and_stamp_hash_path_binding(self) -> None:
        target = "//tools/my_tool:my-tool"
        (self.fuchsia_dir / "vendor/foo/manifests").mkdir(
            parents=True, exist_ok=True
        )
        lock_a = self.fuchsia_dir / "vendor/foo/manifests/a.lock"
        lock_a.write_text("identical-content\n", encoding="utf-8")
        (self.fuchsia_dir / "tools/my_tool").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "tools/my_tool/BUILD.bazel").write_text(
            "# build\n", encoding="utf-8"
        )

        # 1. _file_git_hash rejects non-40-char xattr and accepts 40-char with newline
        with mock.patch("os.getxattr", return_value=b"short\n"):
            self.assertEqual(
                builder._file_git_hash(lock_a),
                _git_blob_sha1(b"identical-content\n"),
            )
        valid_40 = "a" * 40
        with mock.patch(
            "os.getxattr", return_value=(valid_40 + "\n").encode("utf-8")
        ):
            self.assertEqual(builder._file_git_hash(lock_a), valid_40)

        # 2. Renaming a tracked file with identical content under casfs.hash changes the stamp hash
        with mock.patch(
            "os.getxattr", return_value=(valid_40 + "\n").encode("utf-8")
        ):
            stamp_a = builder.compute_stamp_hash(self.fuchsia_dir, target)
            lock_b = self.fuchsia_dir / "vendor/foo/manifests/b.lock"
            lock_a.rename(lock_b)
            stamp_b = builder.compute_stamp_hash(self.fuchsia_dir, target)
            self.assertNotEqual(stamp_a, stamp_b)

        # 3. Deduplicates <pkg_path>/BUILD.bazel when packages.manifest also lists pkg_path/BUILD.bazel
        pkg_manifest = self.fuchsia_dir / builder._PACKAGES_MANIFEST
        pkg_manifest.write_text("tools/my_tool/BUILD.bazel\n", encoding="utf-8")
        seen_paths: list[str] = []

        def spy_git_hash(path: Path) -> str:
            seen_paths.append(str(path))
            return valid_40

        with mock.patch.object(
            builder, "_file_git_hash", side_effect=spy_git_hash
        ):
            builder.compute_stamp_hash(
                self.fuchsia_dir, target, packages_manifest=pkg_manifest
            )
        build_bazel_str = str(self.fuchsia_dir / "tools/my_tool/BUILD.bazel")
        self.assertEqual(seen_paths.count(build_bazel_str), 1)

    def test_compiled_tool_stamp_invalidates_on_host_bazel_args_change(
        self,
    ) -> None:
        target = "//tools/my_tool:my-tool"
        entry = {
            "target": target,
            "category": "Build",
        }
        src_file = self.fuchsia_dir / "tools/my_tool/main.cc"
        src_file.parent.mkdir(parents=True, exist_ok=True)
        src_bytes = b"int main() { return 0; }\n"
        src_file.write_bytes(src_bytes)
        src_hash = _git_blob_sha1(src_bytes)

        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "out/default\n", encoding="utf-8"
        )
        host_args_file = build_dir / builder._HOST_BAZEL_ARGS_REL_PATH
        host_args_file.parent.mkdir(parents=True, exist_ok=True)
        host_args_file.write_text('{"select_variant": []}\n', encoding="utf-8")

        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        tool_bin = self._stage_tool(
            "my-tool", install_dir=install_dir, content="#!/bin/bash\necho ok\n"
        )

        rt_dir = install_dir / f"my-tool{builder._RUNTIME_DIR_SUFFIX}"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / builder._ALWAYS_BUILD_MARKER).write_text("", encoding="utf-8")
        (rt_dir / builder._SOURCES_MANIFEST).write_text(
            f"{src_hash}  tools/my_tool/main.cc\n", encoding="utf-8"
        )

        stamp_dir = install_dir / ".stamps"
        stamp_dir.mkdir(parents=True, exist_ok=True)
        stamp_hash = builder.compute_stamp_hash(
            self.fuchsia_dir,
            target,
            host_bazel_args=host_args_file,
        )
        (stamp_dir / "my-tool.stamp").write_text(
            f"{stamp_hash}\n", encoding="utf-8"
        )

        build_calls: list[str] = []

        def fake_build(*args: object, **kwargs: object) -> None:
            build_calls.append("rebuild")

        # 1. Calling ensure_registered_host_tool with matching host.json hits fast-path (no build)
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ):
            resolved = builder.ensure_registered_host_tool(
                self._make_context(),
                "my-tool",
                entry=entry,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(len(build_calls), 0)

        # 2. Modifying host.json invalidates stamp and triggers rebuild for compiled .always_build tool
        host_args_file.write_text(
            '{"select_variant": ["host_asan"]}\n', encoding="utf-8"
        )
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ):
            resolved = builder.ensure_registered_host_tool(
                self._make_context(),
                "my-tool",
                entry=entry,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(len(build_calls), 1)

        # 3. For a non-.always_build tool, changing or removing host.json does NOT invalidate stamp
        (rt_dir / builder._ALWAYS_BUILD_MARKER).unlink()
        build_calls.clear()
        non_compiled_stamp = builder.compute_stamp_hash(
            self.fuchsia_dir,
            target,
            host_bazel_args=None,
        )
        (stamp_dir / "my-tool.stamp").write_text(
            f"{non_compiled_stamp}\n", encoding="utf-8"
        )
        host_args_file.write_text(
            '{"select_variant": ["host_coverage"]}\n', encoding="utf-8"
        )
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ):
            resolved = builder.ensure_registered_host_tool(
                self._make_context(),
                "my-tool",
                entry=entry,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(len(build_calls), 0)

    def test_target_mode_rebuild_when_missing_or_stale(self) -> None:
        target = "//tools/my_tool:my-tool"
        entry = {
            "target": target,
            "category": "Build",
        }
        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        tool_bin = install_dir / "my-tool"

        def fake_build(*args: object, **kwargs: object) -> None:
            self._stage_tool(
                "my-tool", install_dir=install_dir, content="#!/bin/bash\n"
            )

        stderr_buf = io.StringIO()
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ) as mock_build:
            resolved = builder.ensure_registered_host_tool(
                self._make_context(
                    env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
                    stderr=stderr_buf,
                ),
                "my-tool",
                entry=entry,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(mock_build.call_count, 1)
        self.assertIn("staged binary missing", stderr_buf.getvalue())

        # Now test rebuild when source hash changes
        src_file = self.fuchsia_dir / "tools/my_tool/main.py"
        src_file.parent.mkdir(parents=True, exist_ok=True)
        src_file.write_bytes(b"v1\n")
        rt_dir = install_dir / f"my-tool{builder._RUNTIME_DIR_SUFFIX}"
        rt_dir.mkdir(parents=True, exist_ok=True)
        v1_hash = _git_blob_sha1(b"v1\n")
        (rt_dir / builder._SOURCES_MANIFEST).write_text(
            f"{v1_hash}  tools/my_tool/main.py\n",
            encoding="utf-8",
        )
        stamp_dir = install_dir / ".stamps"
        stamp_dir.mkdir(parents=True, exist_ok=True)
        (stamp_dir / "my-tool.stamp").write_text(
            f"{builder.compute_stamp_hash(self.fuchsia_dir, target)}\n",
            encoding="utf-8",
        )
        # Modify source file
        src_file.write_bytes(b"v2-modified\n")

        stderr_buf2 = io.StringIO()
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ) as mock_build2:
            builder.ensure_registered_host_tool(
                self._make_context(
                    env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
                    stderr=stderr_buf2,
                ),
                "my-tool",
                entry=entry,
            )
        self.assertEqual(mock_build2.call_count, 1)
        self.assertIn(
            "source 'tools/my_tool/main.py' modified", stderr_buf2.getvalue()
        )

    def test_no_build_and_force_build_flags(self) -> None:
        target = "//tools/my_tool:my-tool"
        entry = {
            "target": target,
            "category": "Build",
        }
        # --no-build when binary does not exist -> raises HostToolBuildError
        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self._make_context(), "my-tool", entry=entry, no_build=True
            )

        # Create binary without stamp -> --no-build succeeds without building
        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        tool_bin = self._stage_tool(
            "my-tool", install_dir=install_dir, content="#!/bin/bash\n"
        )

        stderr_buf = io.StringIO()
        resolved = builder.ensure_registered_host_tool(
            self._make_context(
                env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
                stderr=stderr_buf,
            ),
            "my-tool",
            entry=entry,
            no_build=True,
        )
        self.assertEqual(resolved, tool_bin)
        self.assertIn("--no-build", stderr_buf.getvalue())

        # Create valid stamp -> force_build=True forces build
        rt_dir = install_dir / f"my-tool{builder._RUNTIME_DIR_SUFFIX}"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / builder._SOURCES_MANIFEST).write_text("", encoding="utf-8")
        stamp_dir = install_dir / ".stamps"
        stamp_dir.mkdir(parents=True, exist_ok=True)
        (stamp_dir / "my-tool.stamp").write_text(
            f"{builder.compute_stamp_hash(self.fuchsia_dir, target)}\n",
            encoding="utf-8",
        )

        stderr_buf_force = io.StringIO()
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage"
        ) as mock_build:
            builder.ensure_registered_host_tool(
                self._make_context(
                    env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
                    stderr=stderr_buf_force,
                ),
                "my-tool",
                entry=entry,
                force_build=True,
            )
        self.assertEqual(mock_build.call_count, 1)
        self.assertIn(
            "--build forced rebuild",
            stderr_buf_force.getvalue(),
        )

    def test_materialize_ephemeral_symlinks(self) -> None:
        external_dir = self.fuchsia_dir / "out/default/gen/build/bazel/execroot"
        external_dir.mkdir(parents=True, exist_ok=True)
        external_file = external_dir / "compiled_bin"
        external_file.write_bytes(b"ELF_PAYLOAD")
        external_file.chmod(0o755)

        stage_dir = (
            self.fuchsia_dir
            / builder._STAGED_HOST_TOOLS_REL_DIR
            / ".staging/my-tool"
        )
        stage_dir.mkdir(parents=True, exist_ok=True)

        # 1. Internal relative symlink -> must remain a symlink
        internal_target = stage_dir / "libfoo.so.1"
        internal_target.write_bytes(b"SHARED_LIB")
        internal_link = stage_dir / "libfoo.so"
        internal_link.symlink_to("libfoo.so.1")

        # 2. External symlink -> must be materialized into regular file
        ext_link = stage_dir / "my-tool"
        ext_link.symlink_to(external_file)

        builder.materialize_ephemeral_symlinks(stage_dir)

        self.assertTrue(internal_link.is_symlink())
        self.assertEqual(os.readlink(internal_link), "libfoo.so.1")
        self.assertFalse(ext_link.is_symlink())
        self.assertTrue(ext_link.is_file())
        self.assertEqual(ext_link.read_bytes(), b"ELF_PAYLOAD")
        self.assertTrue(bool(ext_link.stat().st_mode & stat.S_IXUSR))

    def test_copy_file_abfs_aware_virtual_clone(self) -> None:
        src = self.fuchsia_dir / "src_bin"
        src.write_bytes(b"HELLO_ABFS")
        src.chmod(0o755)
        dst = self.fuchsia_dir / "dst_bin"

        recorded_calls = []

        def fake_getxattr(path: object, attr: str) -> bytes:
            if str(path) == str(src) and attr == "user.casfs.rbe_ari_hash":
                return b"deadbeef1234\n"
            raise OSError("No such attribute")

        def fake_setxattr(path: object, attr: str, val: bytes) -> None:
            mode = os.stat(dst).st_mode
            recorded_calls.append((attr, val, mode))

        with mock.patch("os.getxattr", side_effect=fake_getxattr), mock.patch(
            "os.setxattr", side_effect=fake_setxattr
        ):
            builder._copy_file_abfs_aware(src, dst)

        self.assertEqual(len(recorded_calls), 1)
        attr_name, attr_val, recorded_mode = recorded_calls[0]
        self.assertEqual(attr_name, "casfs.ari_size")
        self.assertEqual(attr_val, b"rbe:deadbeef1234:10:10")
        self.assertTrue(bool(recorded_mode & stat.S_IXUSR))
        self.assertTrue(bool(recorded_mode & stat.S_IWUSR))

        # Also test OSError fallback during setxattr: cleans up 0-byte file and copies full content
        dst_fallback = self.fuchsia_dir / "dst_fallback"

        def failing_setxattr(path: object, attr: str, val: bytes) -> None:
            raise OSError("Virtual clone unsupported")

        with mock.patch("os.getxattr", side_effect=fake_getxattr), mock.patch(
            "os.setxattr", side_effect=failing_setxattr
        ):
            builder._copy_file_abfs_aware(src, dst_fallback)

        self.assertTrue(dst_fallback.is_file())
        self.assertEqual(dst_fallback.read_bytes(), b"HELLO_ABFS")

    def test_extract_bazel_progress(self) -> None:
        self.assertEqual(
            builder._extract_bazel_progress(
                "[150 / 1,170] Compiling foo/bar.cc; 2s remote-cache\n"
            ),
            "[150 / 1,170] Compiling foo/bar.cc; 2s remote-cache",
        )
        self.assertEqual(
            builder._extract_bazel_progress(
                "\r\x1b[32m[1,024 / 2,048]\x1b[0m Linking host_bin\r\n"
            ),
            "[1,024 / 2,048] Linking host_bin",
        )
        self.assertEqual(
            builder._extract_bazel_progress(
                "Analyzing: target //tools/cmc:cmc (45 packages loaded)\n"
            ),
            "Analyzing: target //tools/cmc:cmc (45 packages loaded)",
        )
        self.assertEqual(
            builder._extract_bazel_progress("Loading: 12 packages loaded\n"),
            "Loading: 12 packages loaded",
        )
        self.assertEqual(
            builder._extract_bazel_progress("Computing main repo mapping: \n"),
            "Computing main repo mapping:",
        )
        self.assertEqual(
            builder._extract_bazel_progress(
                "INFO: Analyzed target //tools/cmc:cmc (10 packages loaded).\n"
            ),
            "INFO: Analyzed target //tools/cmc:cmc (10 packages loaded).",
        )
        self.assertIsNone(builder._extract_bazel_progress(""))
        self.assertIsNone(
            builder._extract_bazel_progress("INFO: Found 1 target...\n")
        )
        self.assertIsNone(
            builder._extract_bazel_progress("Simulated noisy build stdout\n")
        )

    def test_run_bazel_build_and_stage_logs_and_progress(self) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        bazel_top_dir = build_dir / "gen/build/bazel"
        output_base = bazel_top_dir / "output_base"
        bin_dir = (
            output_base
            / "execroot/_main/bazel-out/x86_64-opt/bin/tools/my_tool"
        )
        bin_dir.mkdir(parents=True, exist_ok=True)
        built_bin = bin_dir / "my-tool"
        built_bin.write_text("#!/bin/bash\necho ok\n", encoding="utf-8")
        built_bin.chmod(0o755)

        non_tty_stderr = io.StringIO()
        non_tty_ctx = self._make_context(
            build_dir=build_dir,
            env={"PATH": os.environ.get("PATH", "")},
            stderr=non_tty_stderr,
        )
        args_file = self.fuchsia_dir / "reentry_args.txt"
        reentry_script = non_tty_ctx.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text(
            f"""#!/bin/bash
set -euo pipefail
printf '%s\\n' "$@" > "{args_file}"
for arg in "$@"; do
  if [[ "${{arg}}" == "--config=quiet" ]]; then
    echo "Unexpected --config=quiet" >&2
    exit 2
  fi
  case "${{arg}}" in
    --build_event_json_file=*)
      bep_file="${{arg#--build_event_json_file=}}"
      printf '{{"id":{{"targetCompleted":{{}}}},"namedSetOfFiles":{{"files":[{{"uri":"file://{built_bin}"}}]}}}}\\n' > "${{bep_file}}"
      ;;
  esac
done
echo "Computing main repo mapping:"
echo "Analyzing: target //tools/my_tool:my-tool (10 packages loaded)"
echo "[150 / 1,170] Compiling tools/my_tool/main.cc; 1s remote-cache"
echo "Simulated noisy build stdout"
""",
            encoding="utf-8",
        )
        reentry_script.chmod(0o755)

        install_dir = non_tty_ctx.install_dir()
        stamp_file = install_dir / ".stamps/my-tool.stamp"

        # 1. Non-TTY stderr: only prints INFO line, no transient ANSI progress lines
        builder._run_bazel_build_and_stage(
            context=non_tty_ctx,
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
        )
        recorded_args = args_file.read_text(encoding="utf-8").splitlines()
        self.assertSequenceEqual(
            recorded_args[:-1],
            (
                "build",
                "--no-checks",
                "--no-status",
                "--no-resultstore",
                "--skip-auth-check",
                "--host",
                "@//tools/my_tool:my-tool",
                "--",
                "--experimental_convenience_symlinks=ignore",
            ),
        )
        self.assertTrue(
            recorded_args[-1].startswith("--build_event_json_file=")
        )
        non_tty_out = non_tty_stderr.getvalue()
        self.assertIn(
            f"INFO: Building required tool 'my-tool' with Bazel (log: //{builder._STAGED_HOST_TOOLS_REL_DIR}/.bazel_logs/my-tool.",
            non_tty_out,
        )
        self.assertNotIn("\r\x1b[K", non_tty_out)
        self.assertNotIn("Simulated noisy build stdout", non_tty_out)

        logs = list((install_dir / ".bazel_logs").glob("my-tool.*.log"))
        self.assertEqual(len(logs), 1)
        log_content = logs[0].read_text(encoding="utf-8")
        self.assertIn("Simulated noisy build stdout", log_content)
        self.assertIn(
            "[150 / 1,170] Compiling tools/my_tool/main.cc", log_content
        )

        # 2. TTY stderr: emits single-line \r\x1b[K progress updates and clears at the end
        class _TtyStringIO(io.StringIO):
            def isatty(self) -> bool:
                return True

        tty_stderr = _TtyStringIO()
        tty_ctx = self._make_context(
            build_dir=build_dir,
            env={"PATH": os.environ.get("PATH", "")},
            stderr=tty_stderr,
        )
        builder._run_bazel_build_and_stage(
            context=tty_ctx,
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
        )
        tty_out = tty_stderr.getvalue()
        self.assertIn(
            f"INFO: Building required tool 'my-tool' with Bazel (log: //{builder._STAGED_HOST_TOOLS_REL_DIR}/.bazel_logs/my-tool.",
            tty_out,
        )
        self.assertIn(
            "\r\x1b[K  Analyzing: target //tools/my_tool:my-tool (10 packages loaded)",
            tty_out,
        )
        self.assertIn(
            "\r\x1b[K  [150 / 1,170] Compiling tools/my_tool/main.cc; 1s remote-cache",
            tty_out,
        )
        self.assertTrue(tty_out.endswith("\r\x1b[K"))
        self.assertNotIn("Simulated noisy build stdout", tty_out)

    def test_fx_reentry_env_contract_omits_auth_and_sanitizes_pythonhome(
        self,
    ) -> None:
        build_dir = self.fuchsia_dir / "out/custom"
        bazel_top_dir = build_dir / "gen/build/bazel"
        bin_dir = (
            bazel_top_dir
            / "output_base/execroot/_main/bazel-out/x86_64-opt/bin/tools/my_tool"
        )
        bin_dir.mkdir(parents=True, exist_ok=True)
        built_bin = bin_dir / "my-tool"
        built_bin.write_text("#!/bin/bash\necho ok\n", encoding="utf-8")
        built_bin.chmod(0o755)

        missing_stderr = io.StringIO()
        missing_ctx = self._make_context(
            build_dir=build_dir,
            stderr=missing_stderr,
        )
        install_dir = missing_ctx.install_dir()
        stamp_file = install_dir / ".stamps/my-tool.stamp"

        # 1. Missing scripts/fx-reentry prints ERROR to stderr and fails closed
        with self.assertRaises(builder.HostToolBuildError) as missing_exc:
            builder._run_bazel_build_and_stage(
                context=missing_ctx,
                cmd_name="my-tool",
                target="//tools/my_tool:my-tool",
                install_dir=install_dir,
                stamp_file=stamp_file,
            )
        self.assertIn("ERROR:", missing_stderr.getvalue())
        self.assertIn("fx-reentry not found", missing_stderr.getvalue())
        self.assertIn("fx-reentry not found", str(missing_exc.exception))

        # Use pure bash builtins (rather than sys.executable) so the fake script
        # runs under Swarming compact Python tests even after PYTHONHOME and
        # PYTHONPATH are stripped by _build_fx_reentry_env.
        env_dump_file = self.fuchsia_dir / "reentry_env.nul"
        reentry_script = missing_ctx.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text(
            f"""#!/bin/bash
set -euo pipefail
: > "{env_dump_file}"
compgen -e | while IFS= read -r name; do
  if [ -n "${{!name+x}}" ]; then
    printf '%s=%s\\0' "${{name}}" "${{!name}}" >> "{env_dump_file}"
  fi
done
for arg in "$@"; do
  case "${{arg}}" in
    --build_event_json_file=*)
      bep_file="${{arg#--build_event_json_file=}}"
      printf '{{"id":{{"targetCompleted":{{}}}},"namedSetOfFiles":{{"files":[{{"uri":"file://{built_bin}"}}]}}}}\\n' > "${{bep_file}}"
      ;;
  esac
done
""",
            encoding="utf-8",
        )
        reentry_script.chmod(0o755)

        # 2. Auth vars and FX_BUILD_LOGDIR are never synthesized; FX_ENTRY_CMD
        # defaults to cmd_name; _FX_BUILD_DIR, FX_BUILD_QUIET, FX_FORCE_BUILD*,
        # and staged _fx_host_tools PYTHON* vars are sanitized; and
        # _HOST_TOOL_BUILDING_ENV marks the active host tool build.
        staged_rt = (
            f"/some/{builder._STAGED_HOST_TOOLS_REL_DIR}/"
            f"foo{builder._RUNTIME_DIR_SUFFIX}"
        )
        clean_env = {
            "PATH": os.environ.get("PATH", ""),
            "_FX_BUILD_DIR": str(self.fuchsia_dir / "out/stale"),
            "FX_BUILD_QUIET": "1",
            "FX_FORCE_BUILD": "1",
            "FX_FORCE_BUILD_HOST_TOOL": "1",
            "TUI_ENABLED": "1",
            "PYTHONHOME": f"{staged_rt}/py",
            "PYTHONPATH": f"{staged_rt}/lib.zip",
            "PYTHONSAFEPATH": "1",
            "PYTHONPYCACHEPREFIX": f"{staged_rt}/__pycache__",
        }
        builder._run_bazel_build_and_stage(
            context=self._make_context(build_dir=build_dir, env=clean_env),
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
        )

        captured_env = _parse_nul_env_dump(env_dump_file)
        self.assertNotIn("FX_BUILD_RBE_AUTH_TYPE", captured_env)
        self.assertNotIn("FX_BUILD_RS_AUTH_TYPE", captured_env)
        self.assertNotIn("FX_BUILD_LOGDIR", captured_env)
        self.assertNotIn("_FX_BUILD_DIR", captured_env)
        self.assertNotIn("FX_BUILD_QUIET", captured_env)
        self.assertNotIn("FX_FORCE_BUILD", captured_env)
        self.assertNotIn("FX_FORCE_BUILD_HOST_TOOL", captured_env)
        self.assertNotIn("PYTHONHOME", captured_env)
        self.assertNotIn("PYTHONPATH", captured_env)
        self.assertNotIn("PYTHONSAFEPATH", captured_env)
        self.assertNotIn("PYTHONPYCACHEPREFIX", captured_env)
        self.assertEqual(captured_env["FUCHSIA_DIR"], str(self.fuchsia_dir))
        self.assertEqual(captured_env["FUCHSIA_BUILD_DIR"], str(build_dir))
        self.assertEqual(captured_env["FX_ENTRY_CMD"], "my-tool")
        self.assertEqual(captured_env["TUI_ENABLED"], "0")
        self.assertEqual(
            captured_env[builder._HOST_TOOL_BUILDING_ENV], "my-tool"
        )

        # 3. Existing FX_ENTRY_CMD from an outer fx invocation is preserved.
        outer_env = {
            "PATH": os.environ.get("PATH", ""),
            "FX_ENTRY_CMD": "outer-cmd",
        }
        builder._run_bazel_build_and_stage(
            context=self._make_context(build_dir=build_dir, env=outer_env),
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
        )
        captured_outer_env = _parse_nul_env_dump(env_dump_file)
        self.assertEqual(captured_outer_env["FX_ENTRY_CMD"], "outer-cmd")

    def test_run_bazel_build_and_stage_non_executable_fx_reentry_prints_error(
        self,
    ) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True, exist_ok=True)
        stderr_buf = io.StringIO()
        ctx = self._make_context(build_dir=build_dir, stderr=stderr_buf)
        reentry_script = ctx.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text("#!/bin/bash\n", encoding="utf-8")
        reentry_script.chmod(0o644)

        install_dir = ctx.install_dir()
        stamp_file = install_dir / ".stamps/my-tool.stamp"
        with self.assertRaises(builder.HostToolBuildError) as exc:
            builder._run_bazel_build_and_stage(
                context=ctx,
                cmd_name="my-tool",
                target="//tools/my_tool:my-tool",
                install_dir=install_dir,
                stamp_file=stamp_file,
            )
        self.assertIn("ERROR:", stderr_buf.getvalue())
        self.assertIn(str(reentry_script), stderr_buf.getvalue())
        self.assertEqual(stderr_buf.getvalue().strip(), str(exc.exception))

    def test_build_fx_reentry_env_preserves_non_staged_python_vars(
        self,
    ) -> None:
        build_dir = self.fuchsia_dir / "out/custom"
        ctx = self._make_context(
            build_dir=build_dir,
            env={
                "PATH": "/usr/bin",
                "PYTHONHOME": "/usr/local/python3",
                "PYTHONPATH": "/custom/site-packages",
                "PYTHONSAFEPATH": "1",
                "PYTHONPYCACHEPREFIX": "/custom/pycache",
                "_FX_BUILD_DIR": "/stale/out",
                "FX_BUILD_QUIET": "1",
                "FX_FORCE_BUILD": "1",
                "FX_FORCE_BUILD_HOST_TOOL": "1",
                "FX_ENTRY_CMD": "outer-cmd",
            },
        )
        build_env = builder._build_fx_reentry_env(ctx, "my-tool")
        self.assertEqual(build_env["PYTHONHOME"], "/usr/local/python3")
        self.assertEqual(build_env["PYTHONPATH"], "/custom/site-packages")
        self.assertEqual(build_env["PYTHONSAFEPATH"], "1")
        self.assertEqual(build_env["PYTHONPYCACHEPREFIX"], "/custom/pycache")
        self.assertEqual(build_env["FUCHSIA_DIR"], str(self.fuchsia_dir))
        self.assertEqual(build_env["FUCHSIA_BUILD_DIR"], str(build_dir))
        self.assertEqual(build_env["FX_ENTRY_CMD"], "outer-cmd")
        self.assertEqual(build_env["TUI_ENABLED"], "0")
        self.assertEqual(build_env[builder._HOST_TOOL_BUILDING_ENV], "my-tool")
        self.assertNotIn("_FX_BUILD_DIR", build_env)
        self.assertNotIn("FX_BUILD_QUIET", build_env)
        self.assertNotIn("FX_FORCE_BUILD", build_env)
        self.assertNotIn("FX_FORCE_BUILD_HOST_TOOL", build_env)

    def test_build_fx_reentry_env_strips_staged_python_vars_from_pythonpath(
        self,
    ) -> None:
        build_dir = self.fuchsia_dir / "out/custom"
        ctx = self._make_context(
            build_dir=build_dir,
            env={
                "PATH": "/usr/bin",
                "PYTHONPATH": f"/some/{builder._STAGED_HOST_TOOLS_REL_DIR}/foo{builder._RUNTIME_DIR_SUFFIX}/lib.zip",
                "PYTHONSAFEPATH": "1",
                "PYTHONPYCACHEPREFIX": "/some/pycache",
            },
        )
        build_env = builder._build_fx_reentry_env(ctx, "my-tool")
        self.assertNotIn("PYTHONHOME", build_env)
        self.assertNotIn("PYTHONPATH", build_env)
        self.assertNotIn("PYTHONSAFEPATH", build_env)
        self.assertNotIn("PYTHONPYCACHEPREFIX", build_env)
        self.assertEqual(build_env["FUCHSIA_DIR"], str(self.fuchsia_dir))
        self.assertEqual(build_env["FUCHSIA_BUILD_DIR"], str(build_dir))
        self.assertEqual(build_env["FX_ENTRY_CMD"], "my-tool")
        self.assertEqual(build_env["TUI_ENABLED"], "0")
        self.assertEqual(build_env[builder._HOST_TOOL_BUILDING_ENV], "my-tool")

    @mock.patch.object(builder, "subprocess", autospec=True, spec_set=True)
    def test_run_bazel_build_and_stage_closes_stdout_after_terminate_and_wait(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True, exist_ok=True)
        ctx = self._make_context(build_dir=build_dir)
        reentry_script = ctx.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text("#!/bin/bash\n", encoding="utf-8")
        reentry_script.chmod(0o755)

        events: list[str] = []

        class _InterruptingStdout:
            def __iter__(self) -> Self:
                return self

            def __next__(self) -> str:
                events.append("read")
                raise KeyboardInterrupt

            def close(self) -> None:
                events.append("stdout.close")

        fake_proc = mock.create_autospec(subprocess.Popen, instance=True)
        fake_proc.stdout = _InterruptingStdout()
        fake_proc.terminate.side_effect = lambda: events.append("terminate")

        def _fake_wait() -> int:
            events.append("wait")
            return -15

        fake_proc.wait.side_effect = _fake_wait
        mock_subprocess.Popen.return_value = fake_proc

        install_dir = ctx.install_dir()
        stamp_file = install_dir / ".stamps/my-tool.stamp"
        with self.assertRaises(KeyboardInterrupt):
            builder._run_bazel_build_and_stage(
                context=ctx,
                cmd_name="my-tool",
                target="//tools/my_tool:my-tool",
                install_dir=install_dir,
                stamp_file=stamp_file,
            )
        self.assertSequenceEqual(
            events, ("read", "terminate", "wait", "stdout.close")
        )

    def test_staged_host_tool_is_intact_deep_runfiles(self) -> None:
        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        tool_bin = self._stage_tool(
            "my-tool", install_dir=install_dir, content="#!/bin/bash\n"
        )

        deep_dir = (
            install_dir
            / f"my-tool{builder._RUNTIME_DIR_SUFFIX}"
            / "my-tool.runfiles"
            / "_main"
            / "tools"
            / "devshell"
            / "fx_cli"
        )
        deep_dir.mkdir(parents=True, exist_ok=True)
        broken_link = deep_dir / "broken_mod.py"
        broken_link.symlink_to(self.fuchsia_dir / "nonexistent_target.py")

        self.assertFalse(
            builder._staged_host_tool_is_intact("my-tool", tool_bin)
        )

    def test_staged_host_tool_is_intact_alt_runtime_dir(self) -> None:
        # A staged wrapper whose own `<cmd>.runtime_dir` is missing but which
        # references another tool's runtime_dir must be checked against that
        # alternate directory.
        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        install_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = install_dir / "my-tool"
        tool_bin.write_bytes(
            f"#!/bin/bash\nexec /x/{builder._STAGED_HOST_TOOLS_DIR_NAME}/my_tool_host{builder._RUNTIME_DIR_SUFFIX}/run\n".encode(
                "ascii"
            )
        )
        tool_bin.chmod(0o755)

        # Alt runtime_dir missing -> not intact.
        self.assertFalse(
            builder._staged_host_tool_is_intact("my-tool", tool_bin)
        )

        alt_dir = install_dir / f"my_tool_host{builder._RUNTIME_DIR_SUFFIX}"
        alt_dir.mkdir()
        (alt_dir / "ok.py").write_text("", encoding="utf-8")
        self.assertTrue(
            builder._staged_host_tool_is_intact("my-tool", tool_bin)
        )

        broken_link = alt_dir / "broken_mod.py"
        broken_link.symlink_to(self.fuchsia_dir / "nonexistent_target.py")
        self.assertFalse(
            builder._staged_host_tool_is_intact("my-tool", tool_bin)
        )

    def test_find_import_root(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            checkout_tree = root / "checkout"
            lock_utils_file = checkout_tree / "build/scripts/lock_utils.py"
            lock_utils_file.parent.mkdir(parents=True)
            lock_utils_file.write_text("", encoding="utf-8")
            nested_file = checkout_tree / "tools/devshell/fx_cli/builder.py"
            nested_file.parent.mkdir(parents=True)

            self.assertEqual(
                builder._find_import_root(nested_file), checkout_tree
            )
            self.assertEqual(
                builder._find_import_root(checkout_tree), checkout_tree
            )

            # Nearest enclosing root containing build/scripts/lock_utils.py wins.
            inner_tree = checkout_tree / "runfiles/_main"
            inner_lock_utils = inner_tree / "build/scripts/lock_utils.py"
            inner_lock_utils.parent.mkdir(parents=True)
            inner_lock_utils.write_text("", encoding="utf-8")
            inner_file = inner_tree / "tools/devshell/fx_cli/builder.py"
            inner_file.parent.mkdir(parents=True)
            self.assertEqual(builder._find_import_root(inner_file), inner_tree)

        # Returns None when no ancestor contains build/scripts/lock_utils.py.
        self.assertIsNone(
            builder._find_import_root(Path("/nonexistent_no_lock_utils/a/b.py"))
        )

    def test_find_active_build_pid_delegates_to_lock_utils(self) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        with mock.patch.object(
            lock_utils,
            "find_active_build_pid",
            side_effect=[None, "4321"],
            autospec=True,
        ) as mock_find_active:
            self.assertIsNone(builder._find_active_build_pid(build_dir, env={}))
            self.assertEqual(
                builder._find_active_build_pid(build_dir, env={}),
                "4321",
            )
            self.assertEqual(
                mock_find_active.call_args_list,
                [mock.call(build_dir), mock.call(build_dir)],
            )
            # FX_SIMULATE_ACTIVE_BUILD short-circuits before inspecting lock_utils.
            self.assertEqual(
                builder._find_active_build_pid(
                    build_dir, env={"FX_SIMULATE_ACTIVE_BUILD": "9999"}
                ),
                "9999",
            )
            self.assertEqual(mock_find_active.call_count, 2)

    def test_persistent_checkout_and_chained_or_broken_symlinks(self) -> None:
        # 1. Regular file in checkout must NOT be treated as persistent (must be materialized)
        checkout_src = self.fuchsia_dir / "tools/my_tool/helper.py"
        checkout_src.parent.mkdir(parents=True, exist_ok=True)
        checkout_src.write_bytes(b"print('helper')\n")
        self.assertFalse(
            builder._is_persistent_checkout_path(
                checkout_src,
                self.fuchsia_dir,
                self.fuchsia_dir / "out",
                None,
                None,
            )
        )

        # Directory under prebuilt is persistent
        pb_dir = (
            self.fuchsia_dir / ("pre" + "built") / "third_party" / "python3"
        )
        pb_dir.mkdir(parents=True, exist_ok=True)
        self.assertTrue(
            builder._is_persistent_checkout_path(
                pb_dir,
                self.fuchsia_dir,
                self.fuchsia_dir / "out",
                None,
                None,
            )
        )

        # 2. Chained relative symlink A -> B where B points outside stage_dir:
        # B is materialized into a file, while A remains a relative symlink to B.
        stage_dir = (
            self.fuchsia_dir
            / builder._STAGED_HOST_TOOLS_REL_DIR
            / ".staging/chain-test"
        )
        stage_dir.mkdir(parents=True, exist_ok=True)
        link_b = stage_dir / "b_target.py"
        link_b.symlink_to(checkout_src)
        link_a = stage_dir / "a_alias.py"
        link_a.symlink_to("b_target.py")

        # Also test copying a read-only external directory makes top-level dir writable (0o755)
        ext_ro_dir = self.fuchsia_dir / "out/default/ro_dir"
        ext_ro_dir.mkdir(parents=True, exist_ok=True)
        (ext_ro_dir / "item.txt").write_text("data", encoding="utf-8")
        ext_ro_dir.chmod(0o555)
        ro_link = stage_dir / "copied_ro_dir"
        ro_link.symlink_to(ext_ro_dir)

        try:
            builder.materialize_ephemeral_symlinks(
                stage_dir, fuchsia_dir=self.fuchsia_dir
            )
            self.assertFalse(link_b.is_symlink())
            self.assertTrue(link_b.is_file())
            self.assertTrue(link_a.is_symlink())
            self.assertEqual(os.readlink(link_a), "b_target.py")
            self.assertFalse(ro_link.is_symlink())
            self.assertTrue(ro_link.is_dir())
            self.assertTrue(bool(ro_link.stat().st_mode & stat.S_IWUSR))
        finally:
            ext_ro_dir.chmod(0o755)

        # 3. Broken external symlink raises HostToolBuildError
        broken_stage = (
            self.fuchsia_dir
            / builder._STAGED_HOST_TOOLS_REL_DIR
            / ".staging/broken-test"
        )
        broken_stage.mkdir(parents=True, exist_ok=True)
        (broken_stage / "dangling").symlink_to(
            self.fuchsia_dir / "out/default/missing_artifact"
        )
        with self.assertRaises(builder.HostToolBuildError):
            builder.materialize_ephemeral_symlinks(
                broken_stage, fuchsia_dir=self.fuchsia_dir
            )

    def test_fx_reentry_build_bep_selection_and_staging_transitions(
        self,
    ) -> None:
        # Test under an ancestor directory named 'bin' to verify BEP /bazel-out/.../bin/ parsing
        sub_root = self.fuchsia_dir / "bin" / "ws"
        (sub_root / ".fx-root").parent.mkdir(parents=True, exist_ok=True)
        (sub_root / ".fx-root").write_text("", encoding="utf-8")
        build_dir = sub_root / "out/custom"
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "args.gn").write_text("", encoding="utf-8")
        bazel_top_dir = build_dir / "gen/build/bazel"
        output_base = bazel_top_dir / "output_base"
        bin_dir = (
            output_base
            / "execroot/_main/bazel-out/x86_64-opt/bin/tools/my_tool"
        )
        bin_dir.mkdir(parents=True, exist_ok=True)

        # Both raw binary 'my-tool' (want1) and fx_host_tool target 'my_tool_host' (want2) exist without .runtime_dir
        raw_bin = bin_dir / "my-tool"
        raw_bin.write_text("#!/bin/bash\necho RAW\n", encoding="utf-8")
        raw_bin.chmod(0o755)
        launcher_bin = bin_dir / "my_tool_host"
        launcher_bin.write_text(
            "#!/bin/bash\necho LAUNCHER\n", encoding="utf-8"
        )
        launcher_bin.chmod(0o755)

        ctx = self._make_context(
            fuchsia_dir=sub_root,
            build_dir=build_dir,
            env={"PATH": os.environ.get("PATH", "")},
        )
        reentry_script = ctx.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        build_meta_file = sub_root / "fx_reentry_meta.txt"
        reentry_script.write_text(
            f"""#!/bin/bash
set -euo pipefail
printf 'FUCHSIA_DIR=%s\\nFUCHSIA_BUILD_DIR=%s\\n' "${{FUCHSIA_DIR}}" "${{FUCHSIA_BUILD_DIR}}" > "{build_meta_file}"
for arg in "$@"; do
  case "${{arg}}" in
    --build_event_json_file=*)
      bep_file="${{arg#--build_event_json_file=}}"
      printf '{{"id":{{"targetCompleted":{{}}}},"namedSetOfFiles":{{"files":[{{"uri":"file://{raw_bin}"}},{{"uri":"file://{launcher_bin}"}}]}}}}\\n' > "${{bep_file}}"
      ;;
  esac
done
""",
            encoding="utf-8",
        )
        reentry_script.chmod(0o755)

        install_dir = ctx.install_dir()
        install_dir.mkdir(parents=True, exist_ok=True)
        # Pre-seed stale symlinks for both dst_runtime_dir and cmd_rt_dir_name
        ext_dir = sub_root / "ext_preserved"
        ext_dir.mkdir(parents=True, exist_ok=True)
        (ext_dir / "keep.txt").write_text("keep", encoding="utf-8")
        host_rt_name = f"my_tool_host{builder._RUNTIME_DIR_SUFFIX}"
        cmd_rt_name = f"my-tool{builder._RUNTIME_DIR_SUFFIX}"
        (install_dir / host_rt_name).symlink_to(ext_dir)
        (install_dir / cmd_rt_name).symlink_to(host_rt_name)

        stamp_file = install_dir / ".stamps/my-tool.stamp"
        builder._run_bazel_build_and_stage(
            context=ctx,
            cmd_name="my-tool",
            target="//tools/my_tool:my_tool_host",
            install_dir=install_dir,
            stamp_file=stamp_file,
        )

        # 1. fx-reentry build was invoked directly (even without gen/build/bazel/bazel) with build_dir env
        self.assertSequenceEqual(
            build_meta_file.read_text(encoding="utf-8").splitlines(),
            (
                f"FUCHSIA_DIR={sub_root}",
                f"FUCHSIA_BUILD_DIR={build_dir}",
            ),
        )
        # 2. BEP selected want2 (my_tool_host) over want1 (my-tool)
        self.assertIn(
            "LAUNCHER",
            (install_dir / "my-tool").read_text(encoding="utf-8"),
        )
        # 3. Stale runtime_dir symlinks were cleaned up without deleting ext_preserved
        self.assertTrue((ext_dir / "keep.txt").is_file())
        self.assertFalse((install_dir / cmd_rt_name).is_symlink())
        self.assertFalse((install_dir / cmd_rt_name).exists())

    def test_no_build_and_resolve_build_dir_edge_cases(self) -> None:
        # 1. _resolve_build_dir strips leading //
        bdir = self.fuchsia_dir / "out/slashed"
        bdir.mkdir(parents=True, exist_ok=True)
        self.assertEqual(
            builder._resolve_build_dir(self.fuchsia_dir, "//out/slashed"), bdir
        )

        # 2. --no-build rejects a staged binary whose runtime_dir has a broken symlink
        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        self._stage_tool(
            "broken-tool", install_dir=install_dir, content="#!/bin/bash\n"
        )
        rt_dir = install_dir / f"broken-tool{builder._RUNTIME_DIR_SUFFIX}"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / "dangling.so").symlink_to(self.fuchsia_dir / "missing.so")

        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self._make_context(),
                "broken-tool",
                entry={"target": "//tools:broken-tool"},
                no_build=True,
            )

    def test_stamp_hash_third_party_lockfiles_and_manifests(self) -> None:
        target = "//tools/cmc:fx_cmc"
        (self.fuchsia_dir / "tools/cmc").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "tools/cmc/BUILD.bazel").write_text(
            "# cmc\n", encoding="utf-8"
        )
        rust_lock = self.fuchsia_dir / "third_party/rust_crates/Cargo.lock"
        rust_lock.parent.mkdir(parents=True, exist_ok=True)
        rust_lock.write_text("cargo-v1\n", encoding="utf-8")
        tp_manifest = self.fuchsia_dir / "manifests/third_party/zlib/manifest"
        tp_manifest.parent.mkdir(parents=True, exist_ok=True)
        tp_manifest.write_text("zlib-v1\n", encoding="utf-8")

        stamp1 = builder.compute_stamp_hash(self.fuchsia_dir, target)
        rust_lock.write_text("cargo-v2\n", encoding="utf-8")
        stamp2 = builder.compute_stamp_hash(self.fuchsia_dir, target)
        self.assertNotEqual(stamp1, stamp2)

        tp_manifest.write_text("zlib-v2\n", encoding="utf-8")
        stamp3 = builder.compute_stamp_hash(self.fuchsia_dir, target)
        self.assertNotEqual(stamp2, stamp3)

    def test_materialize_rewrites_ephemeral_workspace_symlink_to_persistent(
        self,
    ) -> None:
        pb_dir = (
            self.fuchsia_dir
            / ("pre" + "built")
            / "third_party"
            / "jq"
            / "linux-x64"
        )
        pb_dir.mkdir(parents=True, exist_ok=True)
        jq_bin = pb_dir / "jq"
        jq_bin.write_text("#!/bin/sh\n", encoding="utf-8")
        jq_bin.chmod(0o755)

        ephemeral_ws_link = (
            self.fuchsia_dir
            / "out/default/gen/build/bazel/workspace"
            / ("pre" + "built")
        )
        ephemeral_ws_link.parent.mkdir(parents=True, exist_ok=True)
        ephemeral_ws_link.symlink_to(self.fuchsia_dir / ("pre" + "built"))

        stage_dir = (
            self.fuchsia_dir
            / builder._STAGED_HOST_TOOLS_REL_DIR
            / ".staging/ephemeral-ws"
        )
        (stage_dir / ".bin").mkdir(parents=True, exist_ok=True)
        bin_link = stage_dir / ".bin" / "jq"
        bin_link.symlink_to(ephemeral_ws_link / "third_party/jq/linux-x64/jq")

        builder.materialize_ephemeral_symlinks(
            stage_dir,
            fuchsia_dir=self.fuchsia_dir,
            bazel_top_dir=self.fuchsia_dir / "out/default/gen/build/bazel",
            out_dir=self.fuchsia_dir / "out",
            build_dir=self.fuchsia_dir / "out/default",
        )
        self.assertTrue(bin_link.is_symlink())
        self.assertEqual(os.readlink(bin_link), str(jq_bin.resolve()))

    def test_bep_percent_encoded_uri_and_symlinked_output_artifact(
        self,
    ) -> None:
        spaced_root = self.fuchsia_dir / "space dir"
        (spaced_root / ".fx-root").parent.mkdir(parents=True, exist_ok=True)
        (spaced_root / ".fx-root").write_text("", encoding="utf-8")
        build_dir = spaced_root / "out/default"
        bazel_top_dir = build_dir / "gen/build/bazel"
        output_base = bazel_top_dir / "output_base"
        (bazel_top_dir / "workspace").mkdir(parents=True, exist_ok=True)
        bin_dir = (
            output_base
            / "execroot/_main/bazel-out/x86_64-opt/bin/tools/my_tool"
        )
        bin_dir.mkdir(parents=True, exist_ok=True)

        # Make built_bin a symlink pointing outside output_base into spaced_root
        checkout_script = spaced_root / "tools/my_tool/my_tool.sh"
        checkout_script.parent.mkdir(parents=True, exist_ok=True)
        checkout_script.write_text(
            "#!/bin/bash\necho SYMLINK_OK\n", encoding="utf-8"
        )
        checkout_script.chmod(0o755)
        built_bin = bin_dir / "my-tool"
        built_bin.symlink_to(checkout_script)

        ctx = self._make_context(
            fuchsia_dir=spaced_root,
            build_dir=build_dir,
            env={"PATH": os.environ.get("PATH", "")},
        )
        encoded_uri_path = str(built_bin).replace(" ", "%20")
        reentry_script = ctx.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text(
            f"""#!/bin/bash
set -euo pipefail
for arg in "$@"; do
  case "${{arg}}" in
    --build_event_json_file=*)
      bep_file="${{arg#--build_event_json_file=}}"
      printf '%s\\n' '{{"id":{{"targetCompleted":{{}}}},"namedSetOfFiles":{{"files":[{{"uri":"file://{encoded_uri_path}"}}]}}}}' > "${{bep_file}}"
      ;;
  esac
done
""",
            encoding="utf-8",
        )
        reentry_script.chmod(0o755)

        install_dir = ctx.install_dir()
        stamp_file = install_dir / ".stamps/my-tool.stamp"
        builder._run_bazel_build_and_stage(
            context=ctx,
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
        )
        self.assertIn(
            "SYMLINK_OK",
            (install_dir / "my-tool").read_text(encoding="utf-8"),
        )

    def test_force_build_overrides_env_no_build_and_mutually_exclusive_flags(
        self,
    ) -> None:
        target = "//tools/my_tool:my-tool"
        entry = {"target": target}
        install_dir = self.fuchsia_dir / builder._STAGED_HOST_TOOLS_REL_DIR
        tool_bin = install_dir / "my-tool"

        def fake_build(*args: object, **kwargs: object) -> None:
            self._stage_tool(
                "my-tool", install_dir=install_dir, content="#!/bin/bash\n"
            )

        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ) as mock_build:
            resolved = builder.ensure_registered_host_tool(
                self._make_context(
                    env={"FX_NO_BUILD": "1", "FX_NO_BUILD_HOST_TOOL": "1"}
                ),
                "my-tool",
                entry=entry,
                force_build=True,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(mock_build.call_count, 1)

        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self._make_context(),
                "my-tool",
                entry=entry,
                no_build=True,
                force_build=True,
            )

        with self.assertRaises(SystemExit):
            builder.main(["--no-build", "--build", "my-tool"])


class RecursiveBuildGuardTest(_BuilderTestBase):
    def setUp(self) -> None:
        super().setUp()
        self.stderr_buf = io.StringIO()
        self.env = {
            builder._HOST_TOOL_BUILDING_ENV: "tool-a",
            "FX_OUTPUT_REGISTRY_ACTIONS": "1",
        }
        self.context = self._make_context(
            env=self.env,
            stderr=self.stderr_buf,
        )
        self.install_dir = self.context.install_dir()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_self_recursion_fails_before_flock(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        mock_lock.side_effect = AssertionError(
            "flock must not be acquired on recursive build"
        )
        with self.assertRaises(builder.HostToolBuildError) as exc:
            builder.ensure_registered_host_tool(
                self.context,
                "tool-a",
                entry={"target": "//tools/a:tool-a"},
            )
        self.assertIn("Recursive host tool build detected", str(exc.exception))
        self.assertIn("tool-a", self.stderr_buf.getvalue())
        self.assertIn("[fx-registry]", self.stderr_buf.getvalue())
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_transitive_recursion_fails_before_flock(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        mock_lock.side_effect = AssertionError(
            "flock must not be acquired on recursive build"
        )
        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self.context,
                "tool-b",
                entry={"target": "//tools/b:tool-b"},
            )
        self.assertIn(
            "Recursive host tool build detected", self.stderr_buf.getvalue()
        )
        self.assertIn("tool-b", self.stderr_buf.getvalue())
        self.assertIn("tool-a", self.stderr_buf.getvalue())
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_allows_up_to_date_fast_path(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        target_b = "//tools/b:tool-b"
        tool_b = self._stage_tool("tool-b", install_dir=self.install_dir)
        stamp_b = self.install_dir / ".stamps/tool-b.stamp"
        stamp_b.parent.mkdir(parents=True, exist_ok=True)
        stamp_b.write_text(
            builder.compute_stamp_hash(self.fuchsia_dir, target_b) + "\n",
            encoding="utf-8",
        )
        mock_lock.side_effect = AssertionError(
            "flock must not be acquired on fast path"
        )
        resolved = builder.ensure_registered_host_tool(
            self.context,
            "tool-b",
            entry={"target": target_b},
        )
        self.assertEqual(resolved, tool_b)
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    def test_allows_script_mode_tools(self) -> None:
        helper_script = self.fuchsia_dir / "tools/helper/run.sh"
        helper_script.parent.mkdir(parents=True, exist_ok=True)
        helper_script.write_text("#!/bin/sh\n", encoding="utf-8")
        helper_script.chmod(0o755)
        resolved_script = builder.ensure_registered_host_tool(
            self.context,
            "helper",
            entry={"script": "//tools/helper/run.sh"},
        )
        self.assertEqual(resolved_script, helper_script)

    @mock.patch.object(builder, "subprocess", autospec=True, spec_set=True)
    def test_direct_bazel_build_fails_before_popen(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        stamp_b = self.install_dir / ".stamps/tool-b.stamp"
        with self.assertRaises(builder.HostToolBuildError):
            builder._run_bazel_build_and_stage(
                context=self.context.with_build_dir(
                    self.fuchsia_dir / "out/default"
                ),
                cmd_name="tool-b",
                target="//tools/b:tool-b",
                install_dir=self.install_dir,
                stamp_file=stamp_b,
            )
        self.assertIn(
            "Recursive host tool build detected", self.stderr_buf.getvalue()
        )
        mock_subprocess.Popen.assert_not_called()


class AncestorBuildLockTest(_BuilderTestBase):
    def setUp(self) -> None:
        super().setUp()
        self.build_dir = self.fuchsia_dir / "out/default"
        self.build_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "out/default\n", encoding="utf-8"
        )
        self.lock_file = self.fuchsia_dir / "out/default.build_lock"
        self.lock_file.write_text(f"{os.getpid()}\n", encoding="utf-8")
        self.stderr_buf = io.StringIO()
        self.env = {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}
        self.context = self._make_context(
            env=self.env,
            stderr=self.stderr_buf,
        )
        self.install_dir = self.context.install_dir()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_reuses_existing_intact_binary_before_flock(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        # Staged binary exists without a stamp so it misses the fast path.
        tool_bin = self._stage_tool("tool-a", install_dir=self.install_dir)
        mock_lock.side_effect = AssertionError(
            "flock must not be acquired when ancestor holds .build_lock"
        )
        resolved = builder.ensure_registered_host_tool(
            self.context,
            "tool-a",
            entry={"target": "//tools/a:tool-a"},
        )
        self.assertEqual(resolved, tool_bin)
        self.assertIn(
            "[fx-registry] skip build 'tool-a'", self.stderr_buf.getvalue()
        )
        self.assertIn("WARNING:", self.stderr_buf.getvalue())
        self.assertIn(str(os.getpid()), self.stderr_buf.getvalue())
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_fails_before_flock_when_unbuilt(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        mock_lock.side_effect = AssertionError(
            "flock must not be acquired when ancestor holds .build_lock"
        )
        with self.assertRaises(builder.HostToolBuildError) as exc:
            builder.ensure_registered_host_tool(
                self.context,
                "tool-a",
                entry={"target": "//tools/a:tool-a"},
            )
        self.assertIn(
            "[fx-registry] error 'tool-a'", self.stderr_buf.getvalue()
        )
        self.assertIn("ERROR:", self.stderr_buf.getvalue())
        self.assertIn(str(os.getpid()), self.stderr_buf.getvalue())
        self.assertIn("ancestor", str(exc.exception))
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_fails_before_flock_when_force_build(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        self._stage_tool("tool-a", install_dir=self.install_dir)
        mock_lock.side_effect = AssertionError(
            "flock must not be acquired when ancestor holds .build_lock"
        )
        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self.context.with_build_dir(self.build_dir),
                "tool-a",
                entry={"target": "//tools/a:tool-a"},
                force_build=True,
            )
        self.assertIn(
            "[fx-registry] error 'tool-a'", self.stderr_buf.getvalue()
        )
        self.assertIn("ERROR:", self.stderr_buf.getvalue())
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(lock_utils, "BlockingFileLock", autospec=True)
    def test_fails_before_flock_when_staged_binary_broken(
        self,
        mock_lock: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        self._stage_tool("tool-a", install_dir=self.install_dir)
        rt_dir = self.install_dir / f"tool-a{builder._RUNTIME_DIR_SUFFIX}"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / "dangling.so").symlink_to(self.fuchsia_dir / "missing.so")

        mock_lock.side_effect = AssertionError(
            "flock must not be acquired when ancestor holds .build_lock"
        )
        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self.context.with_build_dir(self.build_dir),
                "tool-a",
                entry={"target": "//tools/a:tool-a"},
            )
        self.assertIn(
            "[fx-registry] error 'tool-a'", self.stderr_buf.getvalue()
        )
        self.assertIn("ERROR:", self.stderr_buf.getvalue())
        mock_lock.assert_not_called()
        mock_build.assert_not_called()

    @mock.patch.object(builder, "subprocess", autospec=True, spec_set=True)
    def test_direct_bazel_build_fails_before_popen(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        reentry_script = self.context.fx_reentry_script
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text("#!/bin/bash\n", encoding="utf-8")
        reentry_script.chmod(0o755)

        stamp_file = self.install_dir / ".stamps/tool-a.stamp"
        with self.assertRaises(builder.HostToolBuildError) as exc:
            builder._run_bazel_build_and_stage(
                context=self.context.with_build_dir(self.build_dir),
                cmd_name="tool-a",
                target="//tools/a:tool-a",
                install_dir=self.install_dir,
                stamp_file=stamp_file,
            )
        self.assertIn(
            "[fx-registry] error 'tool-a'", self.stderr_buf.getvalue()
        )
        self.assertIn("ERROR:", self.stderr_buf.getvalue())
        self.assertIn(str(os.getpid()), self.stderr_buf.getvalue())
        self.assertIn("ancestor", str(exc.exception))
        mock_subprocess.Popen.assert_not_called()

    @mock.patch.object(
        lock_utils, "find_active_build_pid", return_value="9999", autospec=True
    )
    @mock.patch.object(
        lock_utils,
        "find_ancestor_build_pid",
        return_value=None,
        autospec=True,
    )
    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    def test_non_ancestor_build_lock_reuses_existing_binary_under_flock(
        self,
        mock_build: mock.MagicMock,
        mock_find_ancestor: mock.MagicMock,
        mock_find_active: mock.MagicMock,
    ) -> None:
        tool_bin = self._stage_tool("tool-a", install_dir=self.install_dir)
        resolved = builder.ensure_registered_host_tool(
            self.context.with_build_dir(self.build_dir),
            "tool-a",
            entry={"target": "//tools/a:tool-a"},
        )
        self.assertEqual(resolved, tool_bin)
        self.assertIn("active fx build", self.stderr_buf.getvalue())
        self.assertIn("WARNING:", self.stderr_buf.getvalue())
        mock_find_ancestor.assert_called_once_with(self.build_dir)
        mock_find_active.assert_called_once_with(self.build_dir)
        mock_build.assert_not_called()


class BuilderHelpersTest(_BuilderTestBase):
    def test_module_path_constants(self) -> None:
        self.assertEqual(
            builder._DEFAULT_BAZEL_TOP_DIR_REL, Path("gen/build/bazel")
        )
        self.assertEqual(
            builder._BAZEL_TOP_DIR_CFG_REL,
            Path("build/bazel/config/bazel_top_dir"),
        )

    def test_env_helpers_require_explicit_env_mapping(self) -> None:
        for fn in (
            builder._fx_registry_log_enabled,
            builder._emit_registry_action,
            builder._find_active_build_pid,
        ):
            self.assertIs(
                inspect.signature(fn).parameters["env"].default,
                inspect.Parameter.empty,
                f"{fn.__name__} must require an explicit env mapping",
            )

        self.assertTrue(
            builder._fx_registry_log_enabled(
                {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}
            )
        )
        self.assertFalse(
            builder._fx_registry_log_enabled(
                {"FX_OUTPUT_REGISTRY_ACTIONS": "0"}
            )
        )

        stderr_buf = io.StringIO()
        builder._emit_registry_action(
            "hello", stderr_buf, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}
        )
        self.assertIn("[fx-registry] hello", stderr_buf.getvalue())

        self.assertEqual(
            builder._find_active_build_pid(
                None, {"FX_SIMULATE_ACTIVE_BUILD": "4242"}
            ),
            "4242",
        )

    def test_resolve_bazel_top_dir_default_and_config_override(self) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        self.assertEqual(
            builder._resolve_bazel_top_dir(self.fuchsia_dir, build_dir),
            build_dir / "gen/build/bazel",
        )

        cfg = self.fuchsia_dir / builder._BAZEL_TOP_DIR_CFG_REL
        cfg.parent.mkdir(parents=True, exist_ok=True)
        cfg.write_text("custom/bazel_top\n", encoding="utf-8")
        self.assertEqual(
            builder._resolve_bazel_top_dir(self.fuchsia_dir, build_dir),
            build_dir / "custom/bazel_top",
        )

    @mock.patch.object(builder, "_run_bazel_build_and_stage", autospec=True)
    @mock.patch.object(
        lock_utils, "BlockingFileLock", autospec=True, spec_set=True
    )
    def test_skips_rebuild_when_updated_while_waiting_for_staging_lock(
        self,
        mock_lock_cls: mock.MagicMock,
        mock_build: mock.MagicMock,
    ) -> None:
        target = "//tools/demo:my-tool"
        stderr_buf = io.StringIO()
        ctx = self._make_context(
            build_dir=self.fuchsia_dir / "out/default",
            env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
            stderr=stderr_buf,
        )
        paths = builder._ToolStagingPaths.for_tool(ctx.install_dir(), "my-tool")
        tool_bin = paths.tool_bin

        # Simulate a concurrent invocation staging the tool and stamp while this
        # caller waits to acquire BlockingFileLock.
        def fake_enter() -> mock.MagicMock:
            self._stage_tool("my-tool", install_dir=paths.install_dir)
            paths.stamp_file.parent.mkdir(parents=True, exist_ok=True)
            stamp_hash = builder.compute_stamp_hash(self.fuchsia_dir, target)
            paths.stamp_file.write_text(f"{stamp_hash}\n", encoding="utf-8")
            return mock_lock_instance

        mock_lock_instance = mock_lock_cls.return_value
        mock_lock_instance.waited = True
        mock_lock_instance.__enter__.side_effect = fake_enter

        resolved = builder.ensure_registered_host_tool(
            ctx, "my-tool", entry={"target": target}
        )

        self.assertEqual(resolved, tool_bin)
        mock_lock_cls.assert_called_once_with(
            paths.lock_file,
            waiting_message=(
                "Waiting for concurrent build of host tool 'my-tool'..."
            ),
            message_stream=stderr_buf,
        )
        self.assertIn(
            "updated by concurrent invocation while waiting for lock",
            stderr_buf.getvalue(),
        )
        mock_build.assert_not_called()

    def test_find_binary_in_bep_precedence_and_validation(self) -> None:
        bazel_top_dir = self.fuchsia_dir / "out/default/gen/build/bazel"
        bin_dir = (
            bazel_top_dir
            / "output_base/execroot/_main/bazel-out/x86_64-opt/bin/tools/demo"
        )
        bin_dir.mkdir(parents=True, exist_ok=True)
        cand1 = bin_dir / "my-tool"
        cand1.write_text("#!/bin/bash\n", encoding="utf-8")
        fallback = bin_dir / "demo_host"
        fallback.write_text("#!/bin/bash\n", encoding="utf-8")
        (bin_dir / f"demo_host{builder._RUNTIME_DIR_SUFFIX}").mkdir()

        bep_file = self.fuchsia_dir / "bep.json"
        bep_file.write_text(
            f'{{"uri":"file://{cand1}"}}\n{{"uri":"file://{fallback}"}}\n',
            encoding="utf-8",
        )
        self.assertEqual(
            builder._find_binary_in_bep(
                bep_file, "my-tool", "//tools/demo:demo_host", bazel_top_dir
            ),
            fallback,
        )

        # Artifact outside output_base must be rejected.
        outside_bin = (
            self.fuchsia_dir / "bazel-out/x86_64-opt/bin/tools/demo/my-tool"
        )
        outside_bin.parent.mkdir(parents=True, exist_ok=True)
        outside_bin.write_text("#!/bin/bash\n", encoding="utf-8")
        bep_file.write_text(
            f'{{"uri":"file://{outside_bin}"}}\n', encoding="utf-8"
        )
        with self.assertRaises(builder.HostToolBuildError):
            builder._find_binary_in_bep(
                bep_file, "my-tool", "//tools/demo:my-tool", bazel_top_dir
            )

    def test_compute_rebuild_reason_and_wait_for_active_build(self) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True, exist_ok=True)
        target = "//tools/demo:my-tool"
        stderr_buf = io.StringIO()
        ctx = self._make_context(
            build_dir=build_dir,
            env={"FX_OUTPUT_REGISTRY_ACTIONS": "1"},
            stderr=stderr_buf,
        )
        paths = builder._ToolStagingPaths.for_tool(ctx.install_dir(), "my-tool")

        self.assertEqual(
            builder._compute_rebuild_reason(
                ctx, "my-tool", target, paths, force_build=True
            ),
            "--build forced rebuild",
        )
        self.assertIn(
            "staged binary missing",
            builder._compute_rebuild_reason(
                ctx, "my-tool", target, paths, force_build=False
            )
            or "",
        )

        tool_bin = self._stage_tool("my-tool", install_dir=paths.install_dir)
        self.assertEqual(
            builder._compute_rebuild_reason(
                ctx, "my-tool", target, paths, force_build=False
            ),
            "stamp missing",
        )

        paths.stamp_file.parent.mkdir(parents=True, exist_ok=True)
        stamp_hash = builder.compute_stamp_hash(self.fuchsia_dir, target)
        paths.stamp_file.write_text(f"{stamp_hash}\n", encoding="utf-8")
        self.assertIsNone(
            builder._compute_rebuild_reason(
                ctx, "my-tool", target, paths, force_build=False
            )
        )

        # Wait helper returns None when no active build holds lock.
        self.assertIsNone(
            builder._wait_for_active_build(
                ctx, "my-tool", target, paths, force_build=False
            )
        )

        # Wait helper reuses intact existing binary when active build holds lock.
        active_ctx = self._make_context(
            build_dir=build_dir,
            env={
                "FX_OUTPUT_REGISTRY_ACTIONS": "1",
                "FX_SIMULATE_ACTIVE_BUILD": "9999",
            },
            stderr=stderr_buf,
        )
        self.assertEqual(
            builder._wait_for_active_build(
                active_ctx, "my-tool", target, paths, force_build=False
            ),
            tool_bin,
        )
        self.assertIn("active fx build (PID 9999)", stderr_buf.getvalue())

    def test_cli_help_under_isolated_site_and_safepath(self) -> None:
        builder_py = Path(builder.__file__)
        with tempfile.TemporaryDirectory() as tmp:
            if builder_py.is_file():
                script_to_run = builder_py
            else:
                archive_path = next(
                    (p for p in builder_py.parents if p.is_file()), None
                )
                if archive_path is None or not zipfile.is_zipfile(archive_path):
                    self.skipTest(
                        f"Could not locate standalone or zipapp builder.py: {builder_py}"
                    )
                extract_dir = Path(tmp) / "extracted"
                with zipfile.ZipFile(archive_path) as zf:
                    zf.extractall(extract_dir)
                script_to_run = extract_dir / "builder.py"
            completed = subprocess.run(
                [sys.executable, "-S", str(script_to_run), "--help"],
                env=test_utils.isolated_python_env({"PYTHONSAFEPATH": "1"}),
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        self.assertIn("usage:", completed.stdout.lower())

    def test_cli_help_without_checkout_sentinels_under_safepath(self) -> None:
        # Reproduce Swarming CAS / Bazel runfiles layouts where neither
        # `.fx-root` nor `.jiri_manifest` (nor `__init__.py`) is present.
        staged_modules = (
            ("tools/devshell/fx_cli/builder.py", builder),
            ("tools/devshell/fx_cli/registry.py", builder.registry),
            ("build/scripts/lock_utils.py", lock_utils),
            ("build/profile/procfs.py", lock_utils.procfs),
        )
        with tempfile.TemporaryDirectory() as tmp:
            isolate_root = Path(tmp) / "isolate"
            for rel_path, mod in staged_modules:
                dest = isolate_root / rel_path
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(inspect.getsource(mod), encoding="utf-8")

            staged_builder_py = (
                isolate_root / "tools/devshell/fx_cli/builder.py"
            )
            completed = subprocess.run(
                [sys.executable, "-S", str(staged_builder_py), "--help"],
                env=test_utils.isolated_python_env({"PYTHONSAFEPATH": "1"}),
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        self.assertIn("usage:", completed.stdout.lower())


if __name__ == "__main__":
    unittest.main()
