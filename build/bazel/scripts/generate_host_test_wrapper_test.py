#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import tempfile
import unittest
import unittest.mock
from pathlib import Path

from build.bazel.scripts import generate_host_test_wrapper


class HostTestWrapperGeneratorTest(unittest.TestCase):
    def test_ld_library_path_formatting_and_isolation(self) -> None:
        # Input variables
        env_vars = [
            ("A", "1"),
            ("LD_LIBRARY_PATH", "user_path"),
            ("B", "2"),
        ]
        so_dirs = {".", "lib/foo"}

        # Call format helper directly to verify actual behavior
        (
            filtered_env,
            export_statement,
        ) = generate_host_test_wrapper.format_ld_library_path_export(
            env_vars, so_dirs
        )

        # Verify LD_LIBRARY_PATH isolation
        self.assertEqual(filtered_env, [("A", "1"), ("B", "2")])

        # Verify unescaped double-quoted export string using built-in ${PWD}
        expected = 'export LD_LIBRARY_PATH="${PWD}:${PWD}/lib/foo:user_path${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"'
        self.assertEqual(export_statement, expected)

    def _make_fuchsia_layout(self, root: Path) -> dict[str, Path]:
        """Mimic the Fuchsia layout in root and return interesting paths.

        This creates a source file, a Bazel workspace symlink to its top-level
        directory, an execroot symlink to the workspace entry, a Bazel symlink
        artifact (e.g. from copy_file()) pointing through the workspace, a
        toolchain directory with an intentional symlink (like clang's ld.lld),
        and an external repository with a wrapper script next to a symlink to
        a prebuilt binary.
        """
        src_file = root / "tools" / "foo" / "bar.py"
        src_file.parent.mkdir(parents=True)
        src_file.write_text("")
        python_bin = root / "toolchains" / "python" / "python3"
        python_bin.parent.mkdir(parents=True)
        python_bin.write_text("")
        clang_bin = root / "toolchains" / "clang" / "bin"
        clang_bin.mkdir(parents=True)
        (clang_bin / "llvm").write_text("")
        (clang_bin / "ld.lld").symlink_to("llvm")

        workspace = root / "out" / "gen" / "workspace"
        workspace.mkdir(parents=True)
        (workspace / "tools").symlink_to("../../../tools")
        (workspace / "toolchains").symlink_to("../../../toolchains")

        output_base = root / "out" / "gen" / "output_base"
        execroot = output_base / "execroot" / "_main"
        execroot.mkdir(parents=True)
        (execroot / "tools").symlink_to(workspace / "tools")
        (execroot / "toolchains").symlink_to(workspace / "toolchains")
        artifact = execroot / "bazel-out" / "bin" / "bar.py"
        artifact.parent.mkdir(parents=True)
        artifact.symlink_to(workspace / "tools" / "foo" / "bar.py")

        repo = output_base / "external" / "prebuilt_python"
        repo.mkdir(parents=True)
        (repo / "python3").write_text("")
        (repo / "python3.14-real").symlink_to(python_bin)
        (execroot / "external").symlink_to("../../external")

        return {
            "src_file": src_file,
            "clang_bin": clang_bin,
            "workspace": workspace,
            "execroot": execroot,
            "artifact": artifact,
            "runtime_dir": root / "out" / "runtime_dir",
        }

    def test_resolve_through_directory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            # Resolve in case the temporary directory path contains symlinks.
            root = Path(tmp).resolve()
            paths = self._make_fuchsia_layout(root)
            workspace = paths["workspace"]
            execroot = paths["execroot"]
            resolve = generate_host_test_wrapper.resolve_through_directory
            src_file = str(paths["src_file"])
            self.assertEqual(
                resolve(execroot / "tools" / "foo" / "bar.py", workspace),
                src_file,
            )
            self.assertEqual(resolve(paths["artifact"], workspace), src_file)
            # ".." applies to the resolved location, not lexically: this goes
            # through workspace/tools, then up to the checkout root, while
            # lexically it would be execroot/README.md.
            readme = root / "README.md"
            readme.write_text("")
            self.assertEqual(
                resolve(Path(f"{execroot}/tools/../README.md"), workspace),
                str(readme),
            )
            # Symlinks outside of the workspace are not followed.
            self.assertEqual(
                resolve(
                    execroot / "toolchains" / "clang" / "bin" / "ld.lld",
                    workspace,
                ),
                str(paths["clang_bin"] / "ld.lld"),
            )
            # Paths that do not go through the workspace are not resolved.
            self.assertIsNone(resolve(paths["src_file"], workspace))
            self.assertIsNone(
                resolve(
                    execroot / "external" / "prebuilt_python" / "python3",
                    workspace,
                )
            )

    def test_make_runtime_symlink_resolves_workspace_symlink_chains(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            paths = self._make_fuchsia_layout(Path(tmp).resolve())
            execroot = paths["execroot"]
            for name, target in [
                ("from_execroot.py", execroot / "tools" / "foo" / "bar.py"),
                ("from_artifact.py", paths["artifact"]),
            ]:
                dest = paths["runtime_dir"] / "sub" / name
                dest.parent.mkdir(parents=True, exist_ok=True)
                generate_host_test_wrapper.make_runtime_symlink(
                    dest, target, paths["workspace"]
                )
                link = os.readlink(dest)
                self.assertFalse(os.path.isabs(link))
                self.assertEqual(link, "../../../tools/foo/bar.py")
                self.assertTrue(dest.samefile(paths["src_file"]))

    def test_make_runtime_symlink_keeps_symlinks_outside_workspace(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            paths = self._make_fuchsia_layout(Path(tmp).resolve())
            dest = paths["runtime_dir"] / "test_data" / "ld.lld"
            dest.parent.mkdir(parents=True)
            generate_host_test_wrapper.make_runtime_symlink(
                dest,
                paths["execroot"] / "toolchains" / "clang" / "bin" / "ld.lld",
                paths["workspace"],
            )
            # Points at ld.lld in the checkout, not directly at llvm.
            self.assertEqual(
                os.readlink(dest), "../../../toolchains/clang/bin/ld.lld"
            )

    def test_make_runtime_symlink_keeps_other_targets(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            paths = self._make_fuchsia_layout(Path(tmp).resolve())
            execroot = paths["execroot"]
            # The python3 wrapper script locates python3.14-real next to its
            # own real path, so these must keep pointing into the external
            # repository rather than being resolved to the prebuilt binary.
            for name in ["python3", "python3.14-real"]:
                dest = paths["runtime_dir"] / "test_data" / name
                dest.parent.mkdir(parents=True, exist_ok=True)
                generate_host_test_wrapper.make_runtime_symlink(
                    dest,
                    execroot / "external" / "prebuilt_python" / name,
                    paths["workspace"],
                )
                self.assertEqual(
                    os.readlink(dest),
                    f"../../gen/output_base/execroot/_main/external/prebuilt_python/{name}",
                )


if __name__ == "__main__":
    unittest.main()
