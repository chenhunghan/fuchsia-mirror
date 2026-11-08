#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# Ensure repository root is on sys.path when running standalone.
_REPO_ROOT = Path(__file__).resolve().parents[3]
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))

from build.bazel.scripts import generate_fx_host_tool


class GenerateFxHostToolTest(unittest.TestCase):
    def test_relocated_tool_preserves_cwd_runfiles_and_runtime_tools(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            data_file = execroot / "pkg" / "greeting.txt"
            data_file.parent.mkdir(parents=True)
            data_file.write_text("hello from runfiles")

            repo_mapping = bin_dir / "my_tool_bin.repo_mapping"
            repo_mapping.write_text(",main,_main\n")

            helper_tool = execroot / "prebuilt_out" / "custom-helper"
            helper_tool.parent.mkdir(parents=True)
            helper_tool.write_text("#!/bin/bash\necho helper-ok\n")
            helper_tool.chmod(0o755)

            entry_point = bin_dir / "my_tool_bin"
            entry_point.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'echo "cwd=${PWD}"\n'
                'echo "data=$(cat "${RUNFILES_DIR}/_main/pkg/greeting.txt")"\n'
                'echo "helper=$(custom-helper)"\n'
                'echo "args=$*"\n'
            )
            entry_point.chmod(0o755)

            manifest_file = bin_dir / "my_tool_bin.runfiles_manifest"
            manifest_file.write_text(
                f"_main/__init__.py \n"
                f"_main/pkg/greeting.txt pkg/greeting.txt\n"
                f"_repo_mapping {repo_mapping}\n"
            )

            bazel_out_launcher = bin_dir / "my-tool"
            bazel_out_runtime = bin_dir / "my-tool.runtime_dir"

            rc = generate_fx_host_tool.generate_fx_host_tool(
                entry_point=entry_point.relative_to(execroot),
                entry_runfiles_manifest=manifest_file.relative_to(execroot),
                output_launcher=bazel_out_launcher,
                output_runtime_dir=bazel_out_runtime,
                bazel_execroot=execroot,
                data_runfiles=[],
                runtime_tools={
                    "custom-helper": helper_tool.relative_to(execroot)
                },
            )
            self.assertEqual(rc, 0)

            # Simulate `fx` copying the built tool + runtime_dir into out/default/host-tools.
            host_tools_dir = tmp_path / "out" / "default" / "host-tools"
            host_tools_dir.mkdir(parents=True)
            shutil.copy2(bazel_out_launcher, host_tools_dir / "my-tool")
            shutil.copytree(
                bazel_out_runtime,
                host_tools_dir / "my-tool.runtime_dir",
                symlinks=True,
            )

            launcher_text = (host_tools_dir / "my-tool").read_text()
            self.assertIn(
                'export FX_RUNTIME_TOOLS_DIR="${_FX_RUNTIME_DIR}/.bin"',
                launcher_text,
            )

            caller_cwd = tmp_path / "caller_workdir"
            caller_cwd.mkdir()

            proc = subprocess.run(
                [str(host_tools_dir / "my-tool"), "--flag", "val"],
                cwd=caller_cwd,
                env={"PATH": "/usr/bin:/bin"},
                capture_output=True,
                text=True,
                check=True,
            )
            lines = proc.stdout.strip().splitlines()
            self.assertEqual(
                lines,
                [
                    f"cwd={caller_cwd.resolve()}",
                    "data=hello from runfiles",
                    "helper=helper-ok",
                    "args=--flag val",
                ],
            )
            # Empty __init__.py entry must also be materialized in relocated runfiles.
            self.assertTrue(
                (
                    host_tools_dir
                    / "my-tool.runtime_dir"
                    / "my_tool_bin.runfiles"
                    / "_main"
                    / "__init__.py"
                ).is_file()
            )

    def test_single_file_executable_without_runfiles_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            entry_point = bin_dir / "plain_tool"
            entry_point.write_text('#!/bin/bash\necho "plain:$1:${PWD}"\n')
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "plain-tool"
            out_runtime = bin_dir / "plain-tool.runtime_dir"

            rc = generate_fx_host_tool.generate_fx_host_tool(
                entry_point=entry_point.relative_to(execroot),
                entry_runfiles_manifest=None,
                output_launcher=out_launcher,
                output_runtime_dir=out_runtime,
                bazel_execroot=execroot,
                data_runfiles=[],
                runtime_tools={},
            )
            self.assertEqual(rc, 0)

            host_tools_dir = tmp_path / "host-tools"
            host_tools_dir.mkdir()
            shutil.copy2(out_launcher, host_tools_dir / "plain-tool")
            shutil.copytree(
                out_runtime,
                host_tools_dir / "plain-tool.runtime_dir",
                symlinks=True,
            )

            caller_cwd = tmp_path / "sub_cwd"
            caller_cwd.mkdir()
            proc = subprocess.run(
                [str(host_tools_dir / "plain-tool"), "ok"],
                cwd=caller_cwd,
                env={"PATH": os.environ.get("PATH", "/usr/bin:/bin")},
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(
                proc.stdout.strip(), f"plain:ok:{caller_cwd.resolve()}"
            )
            # Compiled/generated entry points inside bazel-out remain regular files, not symlinks.
            self.assertFalse((out_runtime / "plain_tool").is_symlink())

    def test_in_tree_source_files_staged_for_isolated_swarming_relocation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            build_bot_fuchsia_dir = tmp_path / "cache" / "incremental"
            build_bot_out = build_bot_fuchsia_dir / "out" / "not-default"
            execroot = (
                build_bot_out
                / "gen"
                / "build"
                / "bazel"
                / "output_base"
                / "execroot"
                / "_main"
            )
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            in_tree_src = (
                build_bot_fuchsia_dir
                / "tools"
                / "devshell"
                / "host_tools"
                / "foo"
                / "foo.sh"
            )
            in_tree_src.parent.mkdir(parents=True)
            in_tree_src.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'cat "${RUNFILES_DIR}/_main/tools/devshell/host_tools/foo/helper.txt"\n'
            )
            in_tree_src.chmod(0o755)

            in_tree_helper = in_tree_src.parent / "helper.txt"
            in_tree_helper.write_text("swarming-isolated-ok\n")

            (execroot / "tools" / "devshell" / "host_tools" / "foo").mkdir(
                parents=True
            )
            (
                execroot
                / "tools"
                / "devshell"
                / "host_tools"
                / "foo"
                / "foo.sh"
            ).symlink_to(in_tree_src)
            (
                execroot
                / "tools"
                / "devshell"
                / "host_tools"
                / "foo"
                / "helper.txt"
            ).symlink_to(in_tree_helper)

            out_launcher = bin_dir / "foo"
            out_runtime = bin_dir / "foo.runtime_dir"

            rc = generate_fx_host_tool.generate_fx_host_tool(
                entry_point=Path("tools/devshell/host_tools/foo/foo.sh"),
                entry_runfiles_manifest=None,
                output_launcher=out_launcher,
                output_runtime_dir=out_runtime,
                bazel_execroot=execroot,
                data_runfiles=["tools/devshell/host_tools/foo/helper.txt"],
                runtime_tools={},
                fuchsia_dir=build_bot_fuchsia_dir,
            )
            self.assertEqual(rc, 0)
            self.assertFalse((out_runtime / "foo.sh").is_symlink())
            self.assertFalse(
                (
                    out_runtime
                    / "foo.sh.runfiles"
                    / "_main"
                    / "tools"
                    / "devshell"
                    / "host_tools"
                    / "foo"
                    / "helper.txt"
                ).is_symlink()
            )

            # Simulate Swarming CAS unpacking `out/not-default` at `/b/s/w/ir/out/not-default`
            # on a clean test bot where `cache/incremental` does not exist.
            swarming_root = tmp_path / "swarming_ir"
            swarming_out = swarming_root / "out" / "not-default"
            swarming_out.parent.mkdir(parents=True)
            shutil.move(str(build_bot_out), str(swarming_out))
            shutil.rmtree(build_bot_fuchsia_dir)

            swarming_launcher = (
                swarming_out
                / "gen"
                / "build"
                / "bazel"
                / "output_base"
                / "execroot"
                / "_main"
                / "bazel-out"
                / "k8-fastbuild"
                / "bin"
                / "pkg"
                / "foo"
            )
            proc = subprocess.run(
                [str(swarming_launcher)],
                env={"PATH": "/usr/bin:/bin"},
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(proc.stdout.strip(), "swarming-isolated-ok")

    def test_always_build_flag_writes_marker_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            entry_point = bin_dir / "compiled_bin"
            entry_point.write_text("#!/bin/bash\nexit 0\n")
            entry_point.chmod(0o755)

            no_marker_runtime = bin_dir / "no-marker.runtime_dir"
            rc1 = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--output-launcher={bin_dir / 'no-marker'}",
                    f"--output-runtime-dir={no_marker_runtime}",
                    f"--bazel-execroot={execroot}",
                ]
            )
            self.assertEqual(rc1, 0)
            self.assertFalse((no_marker_runtime / ".always_build").exists())

            marker_runtime = bin_dir / "with-marker.runtime_dir"
            rc2 = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--output-launcher={bin_dir / 'with-marker'}",
                    f"--output-runtime-dir={marker_runtime}",
                    f"--bazel-execroot={execroot}",
                    "--always-build",
                ]
            )
            self.assertEqual(rc2, 0)
            self.assertTrue((marker_runtime / ".always_build").is_file())

    def test_runtime_fx_tool_composition_survives_relocation_and_source_deletion(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            child_bin_dir = (
                execroot / "bazel-out" / "k8-fastbuild" / "bin" / "child_pkg"
            )
            parent_bin_dir = (
                execroot / "bazel-out" / "k8-fastbuild" / "bin" / "parent_pkg"
            )
            child_bin_dir.mkdir(parents=True)
            parent_bin_dir.mkdir(parents=True)

            inner_helper = execroot / "prebuilt_out" / "inner-cipd-bin"
            inner_helper.parent.mkdir(parents=True)
            inner_helper.write_text("#!/bin/bash\necho inner-cipd-ok\n")
            inner_helper.chmod(0o755)

            child_data = execroot / "child_pkg" / "config.txt"
            child_data.parent.mkdir(parents=True)
            child_data.write_text("child-runfile-ok")

            child_entry = child_bin_dir / "child_entry_bin"
            child_entry.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'echo "child_fx_tools_dir_has_inner=$([[ -x "${FX_RUNTIME_TOOLS_DIR}/inner-cipd-bin" ]] && echo yes || echo no)"\n'
                'echo "child_helper=$(inner-cipd-bin)"\n'
                'echo "child_data=$(cat "${RUNFILES_DIR}/_main/child_pkg/config.txt")"\n'
                'echo "child_args=$*"\n'
            )
            child_entry.chmod(0o755)

            child_launcher = child_bin_dir / "child-tool"
            child_runtime = child_bin_dir / "child-tool.runtime_dir"
            rc_child = generate_fx_host_tool.main(
                [
                    f"--entry-point={child_entry.relative_to(execroot)}",
                    f"--output-launcher={child_launcher}",
                    f"--output-runtime-dir={child_runtime}",
                    f"--bazel-execroot={execroot}",
                    "--data-runfile=child_pkg/config.txt",
                    f"--runtime-tool=inner-cipd-bin={inner_helper.relative_to(execroot)}",
                ]
            )
            self.assertEqual(rc_child, 0)

            parent_entry = (
                execroot / "tools" / "parent_pkg" / "parent_script.sh"
            )
            parent_entry.parent.mkdir(parents=True)
            parent_entry.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'echo "fx_tools_dir_has_child=$([[ -x "${FX_RUNTIME_TOOLS_DIR}/child-tool" ]] && echo yes || echo no)"\n'
                'child-tool "$@"\n'
            )
            parent_entry.chmod(0o755)

            parent_launcher = parent_bin_dir / "parent-tool"
            parent_runtime = parent_bin_dir / "parent-tool.runtime_dir"
            rc_parent = generate_fx_host_tool.main(
                [
                    f"--entry-point={parent_entry.relative_to(execroot)}",
                    f"--output-launcher={parent_launcher}",
                    f"--output-runtime-dir={parent_runtime}",
                    f"--bazel-execroot={execroot}",
                    (
                        "--runtime-fx-tool=child-tool="
                        f"{child_launcher.relative_to(execroot)}:"
                        f"{child_runtime.relative_to(execroot)}"
                    ),
                ]
            )
            self.assertEqual(rc_parent, 0)

            staged_child_launcher = parent_runtime / ".bin" / "child-tool"
            staged_child_runtime = (
                parent_runtime / ".bin" / "child-tool.runtime_dir"
            )
            self.assertTrue(staged_child_launcher.is_file())
            self.assertFalse(staged_child_launcher.is_symlink())
            self.assertTrue(os.access(staged_child_launcher, os.X_OK))
            self.assertTrue(staged_child_runtime.is_dir())
            self.assertFalse(staged_child_runtime.is_symlink())

            # Relocate parent-tool to host-tools/ and DELETE child_bin_dir completely.
            host_tools_dir = tmp_path / "host-tools"
            host_tools_dir.mkdir()
            shutil.copy2(parent_launcher, host_tools_dir / "parent-tool")
            shutil.copytree(
                parent_runtime,
                host_tools_dir / "parent-tool.runtime_dir",
                symlinks=True,
            )
            shutil.rmtree(child_bin_dir)

            proc = subprocess.run(
                [str(host_tools_dir / "parent-tool"), "--via-parent"],
                env={"PATH": "/usr/bin:/bin"},
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(
                proc.stdout.strip().splitlines(),
                [
                    "fx_tools_dir_has_child=yes",
                    "child_fx_tools_dir_has_inner=yes",
                    "child_helper=inner-cipd-ok",
                    "child_data=child-runfile-ok",
                    "child_args=--via-parent",
                ],
            )

    def test_directory_runtime_tool_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            clang_tree = (
                execroot
                / "bazel-out"
                / "k8-fastbuild"
                / "bin"
                / "cipd"
                / "prebuilt"
                / "third_party"
                / "clang"
                / "linux-x64"
            )
            (clang_tree / "bin").mkdir(parents=True)
            clang_exe = clang_tree / "bin" / "clang"
            clang_exe.write_text("#!/bin/bash\necho clang-from-pkg-root\n")
            clang_exe.chmod(0o755)

            entry_point = bin_dir / "wrapper.sh"
            entry_point.write_text('#!/bin/bash\nexec clang "$@"\n')
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "llvm-wrap"
            out_runtime = bin_dir / "llvm-wrap.runtime_dir"

            rc = generate_fx_host_tool.generate_fx_host_tool(
                entry_point=entry_point.relative_to(execroot),
                entry_runfiles_manifest=None,
                output_launcher=out_launcher,
                output_runtime_dir=out_runtime,
                bazel_execroot=execroot,
                data_runfiles=[],
                runtime_tools={
                    "linux-x64": clang_tree.relative_to(execroot),
                },
            )
            self.assertEqual(rc, 1)

    def test_generated_data_runfiles_map_short_path_to_bazel_out_path(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            gen_shac = (
                execroot
                / "bazel-out"
                / "k8-fastbuild"
                / "bin"
                / "cipd"
                / "prebuilt"
                / "tools"
                / "shac"
                / "shac"
            )
            gen_shac.parent.mkdir(parents=True)
            gen_shac.write_text("#!/bin/bash\necho real-shac\n")
            gen_shac.chmod(0o755)

            entry_point = bin_dir / "runner"
            entry_point.write_text("#!/bin/bash\nexit 0\n")
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "fmt-tool"
            out_runtime = bin_dir / "fmt-tool.runtime_dir"

            short_path = "cipd/prebuilt/tools/shac/shac"
            exec_path = str(gen_shac.relative_to(execroot))
            rc = generate_fx_host_tool.generate_fx_host_tool(
                entry_point=entry_point.relative_to(execroot),
                entry_runfiles_manifest=None,
                output_launcher=out_launcher,
                output_runtime_dir=out_runtime,
                bazel_execroot=execroot,
                data_runfiles=[f"{short_path}={exec_path}"],
                runtime_tools={},
            )
            self.assertEqual(rc, 0)

            staged = (
                out_runtime / "runner.runfiles" / "_main" / Path(short_path)
            )
            self.assertTrue(staged.is_file())
            self.assertFalse(staged.is_symlink())
            self.assertEqual(staged.read_text(), gen_shac.read_text())

    def test_survives_bazel_clean_with_bzlmod_repo_mapping_and_cipd_dir(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            fuchsia_dir = tmp_path / "fuchsia"
            output_base = (
                fuchsia_dir
                / "out"
                / "default"
                / "gen"
                / "build"
                / "bazel"
                / "output_base"
            )
            execroot = output_base / "execroot" / "_main"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            # In-tree source file under $FUCHSIA_DIR (symlinked in execroot).
            real_src_data = fuchsia_dir / "pkg" / "in_tree.txt"
            real_src_data.parent.mkdir(parents=True)
            real_src_data.write_text("in-tree-v1")
            (execroot / "pkg").mkdir(parents=True)
            (execroot / "pkg" / "in_tree.txt").symlink_to(real_src_data)

            # External repository file under output_base/external/rules_python+/...
            ext_file = (
                output_base
                / "external"
                / "rules_python+"
                / "python"
                / "runfiles"
                / "runfiles.py"
            )
            ext_file.parent.mkdir(parents=True)
            ext_file.write_text("# external runfiles helper\n")

            # CIPD directory under $FUCHSIA_DIR/prebuilt/third_party/clang/linux-x64
            # with multicall clang++ -> clang -> llvm symlink chain and ../lib/libclang.so.
            clang_src_dir = (
                fuchsia_dir / "prebuilt" / "third_party" / "clang" / "linux-x64"
            )
            (clang_src_dir / "bin").mkdir(parents=True)
            (clang_src_dir / "lib").mkdir(parents=True)
            llvm_multicall = clang_src_dir / "bin" / "llvm"
            llvm_multicall.write_text(
                "#!/bin/bash\n"
                'echo "argv0=$(basename "$0")"\n'
                '_exe_dir="$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"\n'
                'echo "has_lib=$([[ -f "${_exe_dir}/../lib/libclang.so" ]] && echo yes || echo no)"\n'
            )
            llvm_multicall.chmod(0o755)
            (clang_src_dir / "bin" / "clang").symlink_to("llvm")
            (clang_src_dir / "bin" / "clang++").symlink_to("clang")
            (clang_src_dir / "lib" / "libclang.so").write_text("libclang")

            # TreeArtifact copy in bazel-out (which will be deleted on `fx bazel clean`).
            clang_tree = (
                bin_dir / "prebuilt" / "third_party" / "clang" / "linux-x64"
            )
            shutil.copytree(clang_src_dir, clang_tree, symlinks=True)

            # Single-file CIPD / wrapper tool in bazel-out with auxiliary wrapped_bin in runfiles.
            wrapper_src = fuchsia_dir / "tools" / "cipd" / "shac_runner.sh"
            wrapper_src.parent.mkdir(parents=True)
            wrapper_src.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'for rf in "${FX_RUNTIME_TOOLS_DIR}/.."/*.runfiles "${BASH_SOURCE[0]}.runfiles"; do\n'
                '  if [[ -x "${rf}/_main/tools/cipd/shac.wrapped_bin" ]]; then\n'
                '    exec "${rf}/_main/tools/cipd/shac.wrapped_bin" "$@"\n'
                "  fi\n"
                "done\n"
                "exit 1\n"
            )
            wrapper_src.chmod(0o755)
            wrapper_out = bin_dir / "shac"
            wrapper_out.symlink_to(wrapper_src)
            wrapped_bin = bin_dir / "shac.wrapped_bin"
            wrapped_bin.write_text("#!/bin/bash\necho shac-wrapped-ok:$*\n")
            wrapped_bin.chmod(0o755)

            repo_mapping = bin_dir / "tool_bin.repo_mapping"
            repo_mapping.write_text(
                ",fuchsia,_main\n,rules_python,rules_python+\n"
            )

            gen_so = bin_dir / "libhelper.so"
            gen_so.write_text("fake-so")

            entry_point = bin_dir / "tool_bin"
            entry_point.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'echo "manifest_env=${RUNFILES_MANIFEST_FILE:-UNSET}"\n'
                'echo "ld_env=${LD_LIBRARY_PATH:-UNSET}"\n'
                'echo "repo_map=$(head -n 1 "${RUNFILES_DIR}/_repo_mapping")"\n'
                'echo "in_tree=$(cat "${RUNFILES_DIR}/_main/pkg/in_tree.txt")"\n'
                'echo "ext_exists=$([[ -f "${RUNFILES_DIR}/rules_python+/python/runfiles/runfiles.py" ]] && echo yes || echo no)"\n'
                'echo "legacy_ext_removed=$([[ -e "${RUNFILES_DIR}/_main/external/rules_python+/python/runfiles/runfiles.py" ]] && echo no || echo yes)"\n'
                "clang++\n"
                "shac check\n"
            )
            entry_point.chmod(0o755)

            manifest_file = bin_dir / "tool_bin.runfiles_manifest"
            manifest_file.write_text(
                f"_main/pkg/in_tree.txt {execroot / 'pkg' / 'in_tree.txt'}\n"
                f"_main/pkg/libhelper.so {gen_so}\n"
                f"rules_python+/python/runfiles/runfiles.py {ext_file}\n"
                f"_main/external/rules_python+/python/runfiles/runfiles.py {ext_file}\n"
                f"_repo_mapping {repo_mapping}\n"
            )

            out_launcher = bin_dir / "clean-safe-tool"
            out_runtime = bin_dir / "clean-safe-tool.runtime_dir"
            rc = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--entry-runfiles-manifest={manifest_file.relative_to(execroot)}",
                    f"--output-launcher={out_launcher}",
                    f"--output-runtime-dir={out_runtime}",
                    f"--bazel-execroot={execroot}",
                    f"--fuchsia-dir={fuchsia_dir}",
                    f"--data-runfile=tools/cipd/shac.wrapped_bin={wrapped_bin.relative_to(execroot)}",
                    f"--runtime-tool=shac={wrapper_out.relative_to(execroot)}",
                    (
                        "--runtime-cipd-dir=linux-x64="
                        f"{clang_tree.relative_to(execroot)}:"
                        + "/".join(
                            ("prebuilt", "third_party", "clang", "linux-x64")
                        )
                    ),
                ]
            )
            self.assertEqual(rc, 0)

            launcher_text = out_launcher.read_text()
            self.assertNotIn("LD_LIBRARY_PATH", launcher_text)
            self.assertIn("unset RUNFILES_MANIFEST_FILE", launcher_text)

            # Relocate into out/default/host-tools and simulate `fx bazel clean --expunge`
            # by deleting the entire `output_base` directory tree.
            host_tools_dir = fuchsia_dir / "out" / "default" / "host-tools"
            host_tools_dir.mkdir(parents=True)
            shutil.copy2(out_launcher, host_tools_dir / "clean-safe-tool")
            shutil.copytree(
                out_runtime,
                host_tools_dir / "clean-safe-tool.runtime_dir",
                symlinks=True,
            )
            shutil.rmtree(output_base)
            # Deleting or mutating the original source file leaves the staged copy self-contained.
            real_src_data.unlink()

            # Manifest entries inside relocated runtime_dir must be relative, not pointing to deleted output_base.
            relocated_manifest = (
                host_tools_dir
                / "clean-safe-tool.runtime_dir"
                / "tool_bin.runfiles"
                / "MANIFEST"
            ).read_text()
            self.assertNotIn(str(output_base), relocated_manifest)

            # Even if caller environment has a stale RUNFILES_MANIFEST_FILE, launcher unsets it.
            caller_cwd = tmp_path / "arbitrary_cwd"
            caller_cwd.mkdir()
            proc = subprocess.run(
                [str(host_tools_dir / "clean-safe-tool")],
                cwd=caller_cwd,
                env={
                    "PATH": "/usr/bin:/bin",
                    "RUNFILES_MANIFEST_FILE": "/nonexistent/MANIFEST",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(
                proc.stdout.strip().splitlines(),
                [
                    "manifest_env=UNSET",
                    "ld_env=UNSET",
                    "repo_map=,fuchsia,_main",
                    "in_tree=in-tree-v1",
                    "ext_exists=yes",
                    "legacy_ext_removed=yes",
                    "argv0=clang++",
                    "has_lib=yes",
                    "shac-wrapped-ok:check",
                ],
            )

    def test_cipd_prebuilt_dir_in_data_runfiles_symlinks_to_persistent_prebuilt(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            fuchsia_dir = tmp_path / "fuchsia"
            output_base = (
                fuchsia_dir
                / "out"
                / "default"
                / "gen"
                / "build"
                / "bazel"
                / "output_base"
            )
            execroot = output_base / "execroot" / "_main"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            prebuilt_rel = Path("prebuilt", "third_party", "go", "linux-x64")
            go_src_dir = fuchsia_dir / prebuilt_rel
            (go_src_dir / "bin").mkdir(parents=True)
            go_bin = go_src_dir / "bin" / "go"
            go_bin.write_text("#!/bin/bash\necho go-ok\n")
            go_bin.chmod(0o755)
            (go_src_dir / "bin" / "gofmt").symlink_to("go")

            # Ephemeral TreeArtifact produced by cipd_prebuilt_directory in bazel-out.
            go_tree = (
                execroot
                / "bazel-out"
                / "k8-fastbuild"
                / "bin"
                / "tools"
                / "devshell"
                / "host_tools"
                / "cipd"
                / prebuilt_rel
            )
            shutil.copytree(go_src_dir, go_tree, symlinks=True)

            entry_point = bin_dir / "runner_bin"
            entry_point.write_text("#!/bin/bash\nexit 0\n")
            entry_point.chmod(0o755)

            short_path = f"tools/devshell/host_tools/cipd/{prebuilt_rel}"
            exec_path = str(go_tree.relative_to(execroot))

            out_launcher = bin_dir / "go-data-tool"
            out_runtime = bin_dir / "go-data-tool.runtime_dir"
            rc = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--output-launcher={out_launcher}",
                    f"--output-runtime-dir={out_runtime}",
                    f"--bazel-execroot={execroot}",
                    f"--fuchsia-dir={fuchsia_dir}",
                    f"--data-runfile={short_path}={exec_path}",
                ]
            )
            self.assertEqual(rc, 0)

            staged_go_dir = (
                out_runtime / "runner_bin.runfiles" / "_main" / Path(short_path)
            )
            self.assertTrue(staged_go_dir.is_symlink())
            self.assertEqual(staged_go_dir.resolve(), go_src_dir.resolve())

            # Deleting ephemeral output_base leaves the staged runfiles symlink intact.
            host_tools_dir = fuchsia_dir / "out" / "default" / "host-tools"
            host_tools_dir.mkdir(parents=True)
            shutil.copytree(
                out_runtime,
                host_tools_dir / "go-data-tool.runtime_dir",
                symlinks=True,
            )
            shutil.rmtree(output_base)

            relocated_go_dir = (
                host_tools_dir
                / "go-data-tool.runtime_dir"
                / "runner_bin.runfiles"
                / "_main"
                / Path(short_path)
            )
            self.assertTrue(relocated_go_dir.is_dir())
            self.assertTrue((relocated_go_dir / "bin" / "gofmt").is_symlink())

    def test_ephemeral_directory_in_data_runfiles_preserves_internal_symlinks(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            output_base = tmp_path / "output_base"
            execroot = output_base / "execroot" / "_main"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            gen_bundle = bin_dir / "gen_bundle_dir"
            (gen_bundle / "bin").mkdir(parents=True)
            multicall = gen_bundle / "bin" / "multicall"
            multicall.write_text('#!/bin/bash\necho "mc:$(basename "$0")"\n')
            multicall.chmod(0o755)
            (gen_bundle / "bin" / "sub1").symlink_to("multicall")
            (gen_bundle / "bin" / "sub2").symlink_to("sub1")

            entry_point = bin_dir / "bundle_runner"
            entry_point.write_text("#!/bin/bash\nexit 0\n")
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "bundle-tool"
            out_runtime = bin_dir / "bundle-tool.runtime_dir"
            rc = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--output-launcher={out_launcher}",
                    f"--output-runtime-dir={out_runtime}",
                    f"--bazel-execroot={execroot}",
                    f"--data-runfile=pkg/gen_bundle_dir={gen_bundle.relative_to(execroot)}",
                ]
            )
            self.assertEqual(rc, 0)

            host_tools_dir = tmp_path / "host-tools"
            host_tools_dir.mkdir()
            shutil.copytree(
                out_runtime,
                host_tools_dir / "bundle-tool.runtime_dir",
                symlinks=True,
            )
            shutil.rmtree(output_base)

            staged_bundle = (
                host_tools_dir
                / "bundle-tool.runtime_dir"
                / "bundle_runner.runfiles"
                / "_main"
                / "pkg"
                / "gen_bundle_dir"
            )
            self.assertTrue(staged_bundle.is_dir())
            self.assertFalse(staged_bundle.is_symlink())
            self.assertTrue((staged_bundle / "bin" / "sub1").is_symlink())
            self.assertEqual(
                os.readlink(staged_bundle / "bin" / "sub1"), "multicall"
            )
            self.assertTrue((staged_bundle / "bin" / "sub2").is_symlink())
            self.assertEqual(
                os.readlink(staged_bundle / "bin" / "sub2"), "sub1"
            )

    def test_sources_manifest_and_build_working_directory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            source_file = execroot / "tools" / "tool.py"
            source_file.parent.mkdir(parents=True)
            source_file.write_text("print('hello')\n", encoding="utf-8")

            runner_src = (
                execroot / "build" / "prebuilt" / "cipd" / "shac_runner.sh"
            )
            runner_src.parent.mkdir(parents=True)
            runner_src.write_text("#!/bin/bash\nexit 0\n", encoding="utf-8")
            runner_src.chmod(0o644)
            gen_symlink = bin_dir / "shac"
            gen_symlink.symlink_to(runner_src)

            prebuilt_rel = Path("prebuilt", "third_party", "foo", "bar.txt")
            prebuilt_file = execroot / prebuilt_rel
            prebuilt_file.parent.mkdir(parents=True)
            prebuilt_file.write_text("prebuilt", encoding="utf-8")

            entry_point = bin_dir / "compiled_bin"
            entry_point.write_text("#!/bin/bash\nexit 0\n")
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "my-tool"
            out_runtime = bin_dir / "my-tool.runtime_dir"
            rc = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--output-launcher={out_launcher}",
                    f"--output-runtime-dir={out_runtime}",
                    f"--bazel-execroot={execroot}",
                    f"--source-file=tools/tool.py={source_file.relative_to(execroot)}",
                    f"--source-file=build/prebuilt/cipd/shac_runner.sh={runner_src.relative_to(execroot)}",
                    f"--data-runfile=build/prebuilt/cipd/shac={gen_symlink.relative_to(execroot)}",
                    f"--data-runfile={prebuilt_rel}={prebuilt_file.relative_to(execroot)}",
                    f"--runtime-tool=shac={gen_symlink.relative_to(execroot)}",
                ]
            )
            self.assertEqual(rc, 0)
            manifest = out_runtime / "sources.manifest"
            self.assertTrue(manifest.is_file())
            content = manifest.read_text(encoding="utf-8")
            expected_git_sha = hashlib.sha1(
                b"blob 15\0print('hello')\n"
            ).hexdigest()
            self.assertIn(f"{expected_git_sha}  tools/tool.py\n", content)
            self.assertIn("  build/prebuilt/cipd/shac_runner.sh\n", content)
            self.assertNotIn("  build/prebuilt/cipd/shac\n", content)
            self.assertNotIn(str(prebuilt_rel), content)
            self.assertTrue(os.access(out_runtime / ".bin" / "shac", os.X_OK))

            launcher_content = out_launcher.read_text(encoding="utf-8")
            self.assertIn("BUILD_WORKING_DIRECTORY", launcher_content)
            self.assertIn('cd "${BUILD_WORKING_DIRECTORY}"', launcher_content)
            self.assertIn("unset BUILD_WORKING_DIRECTORY", launcher_content)

    def test_launcher_with_relative_invocation_and_build_working_directory(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "bin"
            bin_dir.mkdir(parents=True)

            entry_point = bin_dir / "tool_bin"
            entry_point.write_text(
                "#!/bin/bash\n"
                "set -euo pipefail\n"
                'echo "cwd=${PWD}"\n'
                'echo "bwd=${BUILD_WORKING_DIRECTORY:-UNSET}"\n'
                'echo "args=$*"\n'
            )
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "my-tool"
            out_runtime = bin_dir / "my-tool.runtime_dir"
            rc = generate_fx_host_tool.generate_fx_host_tool(
                entry_point=entry_point.relative_to(execroot),
                entry_runfiles_manifest=None,
                output_launcher=out_launcher,
                output_runtime_dir=out_runtime,
                bazel_execroot=execroot,
                data_runfiles=[],
                runtime_tools={},
            )
            self.assertEqual(rc, 0)

            launcher_lines = out_launcher.read_text(
                encoding="utf-8"
            ).splitlines()
            tool_dir_idx = next(
                i
                for i, line in enumerate(launcher_lines)
                if "_FX_TOOL_DIR=" in line
            )
            cd_idx = next(
                i
                for i, line in enumerate(launcher_lines)
                if 'cd "${BUILD_WORKING_DIRECTORY}"' in line
            )
            unset_bwd_idx = next(
                i
                for i, line in enumerate(launcher_lines)
                if "unset BUILD_WORKING_DIRECTORY" in line
            )
            self.assertLess(
                tool_dir_idx,
                cd_idx,
                msg="_FX_TOOL_DIR must be resolved before changing into BUILD_WORKING_DIRECTORY",
            )
            self.assertLess(
                cd_idx,
                unset_bwd_idx,
                msg="unset BUILD_WORKING_DIRECTORY must follow cd ${BUILD_WORKING_DIRECTORY}",
            )

            work_dir = tmp_path / "other_work_dir"
            work_dir.mkdir()

            # Execute launcher via relative path `./my-tool` from bin_dir,
            # with BUILD_WORKING_DIRECTORY pointing to work_dir.
            proc = subprocess.run(
                ["./my-tool", "hello", "world"],
                cwd=bin_dir,
                env={
                    "PATH": "/usr/bin:/bin",
                    "BUILD_WORKING_DIRECTORY": str(work_dir),
                },
                capture_output=True,
                text=True,
                check=True,
            )
            lines = proc.stdout.strip().splitlines()
            self.assertEqual(lines[0], f"cwd={work_dir.resolve()}")
            self.assertEqual(lines[1], "bwd=UNSET")
            self.assertEqual(lines[2], "args=hello world")

    def test_packages_manifest_generation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            execroot = tmp_path / "execroot"
            bin_dir = execroot / "bazel-out" / "k8-fastbuild" / "bin" / "pkg"
            bin_dir.mkdir(parents=True)

            entry_point = bin_dir / "compiled_bin"
            entry_point.write_text("#!/bin/bash\nexit 0\n")
            entry_point.chmod(0o755)

            out_launcher = bin_dir / "my-tool"
            out_runtime = bin_dir / "my-tool.runtime_dir"
            rc = generate_fx_host_tool.main(
                [
                    f"--entry-point={entry_point.relative_to(execroot)}",
                    f"--output-launcher={out_launcher}",
                    f"--output-runtime-dir={out_runtime}",
                    f"--bazel-execroot={execroot}",
                    "--source-package=tools/cmc",
                    "--source-package=tools/lib/cml",
                    "--source-package=",
                ]
            )
            self.assertEqual(rc, 0)
            pkg_manifest = out_runtime / "packages.manifest"
            self.assertTrue(pkg_manifest.is_file())
            content = pkg_manifest.read_text(encoding="utf-8")
            self.assertEqual(
                content,
                "BUILD.bazel\ntools/cmc/BUILD.bazel\ntools/lib/cml/BUILD.bazel\n",
            )


if __name__ == "__main__":
    unittest.main()
