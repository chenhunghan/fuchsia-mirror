# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for fx_cli.builder."""

import hashlib
import io
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    from fx_cli import builder
except ImportError:
    import builder


def _git_blob_sha1(data: bytes) -> str:
    header = f"blob {len(data)}\0".encode("ascii")
    return hashlib.sha1(header + data).hexdigest()


class BuilderTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._tmp.name)
        (self.fuchsia_dir / ".fx-root").write_text("", encoding="utf-8")
        (self.fuchsia_dir / "out").mkdir(parents=True, exist_ok=True)

    def tearDown(self) -> None:
        self._tmp.cleanup()

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
        with mock.patch.dict(
            os.environ, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}, clear=False
        ):
            resolved = builder.ensure_registered_host_tool(
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                stderr=stderr_buf,
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
                self.fuchsia_dir, "missing-py", entry=entry_py
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
                self.fuchsia_dir, "non-exec", entry=entry_sh
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

        install_dir = self.fuchsia_dir / "out" / "_fx_host_tools"
        install_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = install_dir / "my-tool"
        tool_bin.write_text("#!/bin/bash\necho ok\n", encoding="utf-8")
        tool_bin.chmod(0o755)

        rt_dir = install_dir / "my-tool.runtime_dir"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / "sources.manifest").write_text(
            f"{src_hash}  tools/my_tool/main.py\n", encoding="utf-8"
        )

        stamp_dir = install_dir / ".stamps"
        stamp_dir.mkdir(parents=True, exist_ok=True)
        stamp_hash = builder.compute_stamp_hash(self.fuchsia_dir, target)
        (stamp_dir / "my-tool.stamp").write_text(
            f"{stamp_hash}\n", encoding="utf-8"
        )

        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {
                "FX_OUTPUT_REGISTRY_ACTIONS": "1",
                "FX_NO_BUILD_HOST_TOOL": "",
                "FX_NO_BUILD": "",
                "FX_FORCE_BUILD_HOST_TOOL": "",
                "FX_FORCE_BUILD": "",
            },
            clear=False,
        ):
            resolved = builder.ensure_registered_host_tool(
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                stderr=stderr_buf,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertIn(
            "[fx-registry] skip build 'my-tool' (//tools/my_tool:my-tool): up-to-date (stamp and sources.manifest valid), using //out/_fx_host_tools/my-tool",
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
        host_args_dir = self.fuchsia_dir / "out/default/bazel_args"
        host_args_dir.mkdir(parents=True, exist_ok=True)
        host_args_file = host_args_dir / "host.json"
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
        pkg_manifest = self.fuchsia_dir / "packages.manifest"
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
        host_args_dir = build_dir / "bazel_args"
        host_args_dir.mkdir(parents=True, exist_ok=True)
        host_args_file = host_args_dir / "host.json"
        host_args_file.write_text('{"select_variant": []}\n', encoding="utf-8")

        install_dir = self.fuchsia_dir / "out/_fx_host_tools"
        install_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = install_dir / "my-tool"
        tool_bin.write_text("#!/bin/bash\necho ok\n", encoding="utf-8")
        tool_bin.chmod(0o755)

        rt_dir = install_dir / "my-tool.runtime_dir"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / ".always_build").write_text("", encoding="utf-8")
        sources_manifest = rt_dir / "sources.manifest"
        sources_manifest.write_text(
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
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                build_dir=None,
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
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                build_dir=None,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(len(build_calls), 1)

        # 3. For a non-.always_build tool, changing or removing host.json does NOT invalidate stamp
        (rt_dir / ".always_build").unlink()
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
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                build_dir=None,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(len(build_calls), 0)

    def test_target_mode_rebuild_when_missing_or_stale(self) -> None:
        target = "//tools/my_tool:my-tool"
        entry = {
            "target": target,
            "category": "Build",
        }
        install_dir = self.fuchsia_dir / "out" / "_fx_host_tools"
        tool_bin = install_dir / "my-tool"

        def fake_build(*args: object, **kwargs: object) -> None:
            install_dir.mkdir(parents=True, exist_ok=True)
            tool_bin.write_text("#!/bin/bash\n", encoding="utf-8")
            tool_bin.chmod(0o755)

        stderr_buf = io.StringIO()
        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ) as mock_build:
            with mock.patch.dict(
                os.environ, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}, clear=False
            ):
                resolved = builder.ensure_registered_host_tool(
                    self.fuchsia_dir,
                    "my-tool",
                    entry=entry,
                    stderr=stderr_buf,
                )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(mock_build.call_count, 1)
        self.assertIn("staged binary missing", stderr_buf.getvalue())

        # Now test rebuild when source hash changes
        src_file = self.fuchsia_dir / "tools/my_tool/main.py"
        src_file.parent.mkdir(parents=True, exist_ok=True)
        src_file.write_bytes(b"v1\n")
        rt_dir = install_dir / "my-tool.runtime_dir"
        rt_dir.mkdir(parents=True, exist_ok=True)
        v1_hash = _git_blob_sha1(b"v1\n")
        (rt_dir / "sources.manifest").write_text(
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
            with mock.patch.dict(
                os.environ, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}, clear=False
            ):
                builder.ensure_registered_host_tool(
                    self.fuchsia_dir,
                    "my-tool",
                    entry=entry,
                    stderr=stderr_buf2,
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
                self.fuchsia_dir, "my-tool", entry=entry, no_build=True
            )

        # Create binary without stamp -> --no-build succeeds without building
        install_dir = self.fuchsia_dir / "out" / "_fx_host_tools"
        install_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = install_dir / "my-tool"
        tool_bin.write_text("#!/bin/bash\n", encoding="utf-8")
        tool_bin.chmod(0o755)

        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}, clear=False
        ):
            resolved = builder.ensure_registered_host_tool(
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                no_build=True,
                stderr=stderr_buf,
            )
        self.assertEqual(resolved, tool_bin)
        self.assertIn("--no-build", stderr_buf.getvalue())

        # Create valid stamp -> force_build=True forces build
        rt_dir = install_dir / "my-tool.runtime_dir"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / "sources.manifest").write_text("", encoding="utf-8")
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
            with mock.patch.dict(
                os.environ, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}, clear=False
            ):
                builder.ensure_registered_host_tool(
                    self.fuchsia_dir,
                    "my-tool",
                    entry=entry,
                    force_build=True,
                    stderr=stderr_buf_force,
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

        stage_dir = self.fuchsia_dir / "out/_fx_host_tools/.staging/my-tool"
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
        (bazel_top_dir / "workspace").mkdir(parents=True, exist_ok=True)
        bin_dir = (
            output_base
            / "execroot/_main/bazel-out/x86_64-opt/bin/tools/my_tool"
        )
        bin_dir.mkdir(parents=True, exist_ok=True)
        built_bin = bin_dir / "my-tool"
        built_bin.write_text("#!/bin/bash\necho ok\n", encoding="utf-8")
        built_bin.chmod(0o755)

        bazel_bin = bazel_top_dir / "bazel"
        bazel_bin.write_text(
            f"""#!/bin/bash
set -euo pipefail
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
        bazel_bin.chmod(0o755)

        install_dir = self.fuchsia_dir / "out/_fx_host_tools"
        stamp_file = install_dir / ".stamps/my-tool.stamp"

        # 1. Non-TTY stderr: only prints INFO line, no transient ANSI progress lines
        non_tty_stderr = io.StringIO()
        builder._run_bazel_build_and_stage(
            fuchsia_dir=self.fuchsia_dir,
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
            expected_stamp="stamp",
            build_dir=build_dir,
            stderr=non_tty_stderr,
        )
        non_tty_out = non_tty_stderr.getvalue()
        self.assertIn(
            "INFO: Building required tool 'my-tool' with Bazel (log: //out/_fx_host_tools/.bazel_logs/my-tool.",
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
        builder._run_bazel_build_and_stage(
            fuchsia_dir=self.fuchsia_dir,
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
            expected_stamp="stamp",
            build_dir=build_dir,
            stderr=tty_stderr,
        )
        tty_out = tty_stderr.getvalue()
        self.assertIn(
            "INFO: Building required tool 'my-tool' with Bazel (log: //out/_fx_host_tools/.bazel_logs/my-tool.",
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

    def test_staged_host_tool_is_intact_deep_runfiles(self) -> None:
        install_dir = self.fuchsia_dir / "out/_fx_host_tools"
        install_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = install_dir / "my-tool"
        tool_bin.write_text("#!/bin/bash\n", encoding="utf-8")
        tool_bin.chmod(0o755)

        deep_dir = (
            install_dir
            / "my-tool.runtime_dir"
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

    def test_find_active_build_pid_checks_inside_build_dir(self) -> None:
        build_dir = self.fuchsia_dir / "out/default"
        build_dir.mkdir(parents=True, exist_ok=True)
        inside_lock = build_dir / ".build_lock"
        inside_lock.write_text(f"{os.getpid()}\n", encoding="utf-8")

        with mock.patch.object(builder, "_is_ancestor_pid", return_value=False):
            self.assertEqual(
                builder._find_active_build_pid(build_dir),
                str(os.getpid()),
            )

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
        stage_dir = self.fuchsia_dir / "out/_fx_host_tools/.staging/chain-test"
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
            self.fuchsia_dir / "out/_fx_host_tools/.staging/broken-test"
        )
        broken_stage.mkdir(parents=True, exist_ok=True)
        (broken_stage / "dangling").symlink_to(
            self.fuchsia_dir / "out/default/missing_artifact"
        )
        with self.assertRaises(builder.HostToolBuildError):
            builder.materialize_ephemeral_symlinks(
                broken_stage, fuchsia_dir=self.fuchsia_dir
            )

    def test_fx_gen_passes_dir_flag_and_bep_selection_and_staging_transitions(
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

        fx_script = sub_root / "scripts/fx"
        fx_script.parent.mkdir(parents=True, exist_ok=True)
        gen_args_file = sub_root / "fx_gen_args.txt"
        fx_script.write_text(
            f"""#!/bin/bash
set -euo pipefail
printf '%s\\n' "$@" > "{gen_args_file}"
mkdir -p "{bazel_top_dir}/workspace"
cat > "{bazel_top_dir}/bazel" <<'EOF'
#!/bin/bash
for arg in "$@"; do
  case "${{arg}}" in
    --build_event_json_file=*)
      bep_file="${{arg#--build_event_json_file=}}"
      printf '{{"id":{{"targetCompleted":{{}}}},"namedSetOfFiles":{{"files":[{{"uri":"file://{raw_bin}"}},{{"uri":"file://{launcher_bin}"}}]}}}}\\n' > "${{bep_file}}"
      ;;
  esac
done
EOF
chmod +x "{bazel_top_dir}/bazel"
""",
            encoding="utf-8",
        )
        fx_script.chmod(0o755)

        install_dir = sub_root / "out/_fx_host_tools"
        install_dir.mkdir(parents=True, exist_ok=True)
        # Pre-seed stale symlinks for both dst_runtime_dir and cmd_rt_dir_name
        ext_dir = sub_root / "ext_preserved"
        ext_dir.mkdir(parents=True, exist_ok=True)
        (ext_dir / "keep.txt").write_text("keep", encoding="utf-8")
        (install_dir / "my_tool_host.runtime_dir").symlink_to(ext_dir)
        (install_dir / "my-tool.runtime_dir").symlink_to(
            "my_tool_host.runtime_dir"
        )

        stamp_file = install_dir / ".stamps/my-tool.stamp"
        builder._run_bazel_build_and_stage(
            fuchsia_dir=sub_root,
            cmd_name="my-tool",
            target="//tools/my_tool:my_tool_host",
            install_dir=install_dir,
            stamp_file=stamp_file,
            expected_stamp="stamp-val",
            build_dir=build_dir,
            stderr=io.StringIO(),
        )

        # 1. fx gen was invoked with --dir <build_dir> gen
        self.assertEqual(
            gen_args_file.read_text(encoding="utf-8").splitlines(),
            ["--dir", str(build_dir), "gen"],
        )
        # 2. BEP selected want2 (my_tool_host) over want1 (my-tool)
        self.assertIn(
            "LAUNCHER",
            (install_dir / "my-tool").read_text(encoding="utf-8"),
        )
        # 3. Stale runtime_dir symlinks were cleaned up without deleting ext_preserved
        self.assertTrue((ext_dir / "keep.txt").is_file())
        self.assertFalse((install_dir / "my-tool.runtime_dir").is_symlink())
        self.assertFalse((install_dir / "my-tool.runtime_dir").exists())

    def test_no_build_and_resolve_build_dir_edge_cases(self) -> None:
        # 1. _resolve_build_dir strips leading //
        bdir = self.fuchsia_dir / "out/slashed"
        bdir.mkdir(parents=True, exist_ok=True)
        self.assertEqual(
            builder._resolve_build_dir(self.fuchsia_dir, "//out/slashed"), bdir
        )

        # 2. --no-build rejects a staged binary whose runtime_dir has a broken symlink
        install_dir = self.fuchsia_dir / "out/_fx_host_tools"
        install_dir.mkdir(parents=True, exist_ok=True)
        tool_bin = install_dir / "broken-tool"
        tool_bin.write_text("#!/bin/bash\n", encoding="utf-8")
        tool_bin.chmod(0o755)
        rt_dir = install_dir / "broken-tool.runtime_dir"
        rt_dir.mkdir(parents=True, exist_ok=True)
        (rt_dir / "dangling.so").symlink_to(self.fuchsia_dir / "missing.so")

        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self.fuchsia_dir,
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
            self.fuchsia_dir / "out/_fx_host_tools/.staging/ephemeral-ws"
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

        encoded_uri_path = str(built_bin).replace(" ", "%20")
        bazel_bin = bazel_top_dir / "bazel"
        bazel_bin.write_text(
            f"""#!/bin/bash
set -euo pipefail
for arg in "$@"; do
  case "${{arg}}" in
    --build_event_json_file=*)
      bep_file="${{arg#--build_event_json_file=}}"
      printf '%s\n' '{{"id":{{"targetCompleted":{{}}}},"namedSetOfFiles":{{"files":[{{"uri":"file://{encoded_uri_path}"}}]}}}}' > "${{bep_file}}"
      ;;
  esac
done
""",
            encoding="utf-8",
        )
        bazel_bin.chmod(0o755)

        install_dir = spaced_root / "out/_fx_host_tools"
        stamp_file = install_dir / ".stamps/my-tool.stamp"
        builder._run_bazel_build_and_stage(
            fuchsia_dir=spaced_root,
            cmd_name="my-tool",
            target="//tools/my_tool:my-tool",
            install_dir=install_dir,
            stamp_file=stamp_file,
            expected_stamp="stamp",
            build_dir=build_dir,
            stderr=io.StringIO(),
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
        install_dir = self.fuchsia_dir / "out/_fx_host_tools"
        tool_bin = install_dir / "my-tool"

        def fake_build(*args: object, **kwargs: object) -> None:
            install_dir.mkdir(parents=True, exist_ok=True)
            tool_bin.write_text("#!/bin/bash\n", encoding="utf-8")
            tool_bin.chmod(0o755)

        with mock.patch.object(
            builder, "_run_bazel_build_and_stage", side_effect=fake_build
        ) as mock_build:
            with mock.patch.dict(
                os.environ,
                {"FX_NO_BUILD": "1", "FX_NO_BUILD_HOST_TOOL": "1"},
                clear=False,
            ):
                resolved = builder.ensure_registered_host_tool(
                    self.fuchsia_dir,
                    "my-tool",
                    entry=entry,
                    force_build=True,
                    stderr=io.StringIO(),
                )
        self.assertEqual(resolved, tool_bin)
        self.assertEqual(mock_build.call_count, 1)

        with self.assertRaises(builder.HostToolBuildError):
            builder.ensure_registered_host_tool(
                self.fuchsia_dir,
                "my-tool",
                entry=entry,
                no_build=True,
                force_build=True,
                stderr=io.StringIO(),
            )

        with self.assertRaises(SystemExit):
            builder.main(["--no-build", "--build", "my-tool"])


if __name__ == "__main__":
    unittest.main()
