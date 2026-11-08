# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


def _find_tool_launcher() -> Path:
    rel = "tools/devshell/host_tools/get_main_pb_path/get-main-pb-path"
    candidates = [
        Path(rel),
        Path("_main") / rel,
    ]
    runfiles_dir = os.environ.get("RUNFILES_DIR")
    if runfiles_dir:
        candidates.insert(0, Path(runfiles_dir) / "_main" / rel)
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    out_dir = Path(os.environ.get("FUCHSIA_OUT_DIR") or "out")
    staged = out_dir / "_fx_host_tools" / "get-main-pb-path"
    if staged.is_file():
        return staged.resolve()
    raise unittest.SkipTest(
        "get-main-pb-path launcher not found in Bazel runfiles or out/_fx_host_tools"
    )


class GetMainPbPathToolTests(unittest.TestCase):
    def test_bundles_cipd_jq_without_checkout_prebuilt_dir(self) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "get-main-pb-path.runtime_dir"
        jq_bin = runtime_dir / ".bin" / "jq"
        self.assertTrue(jq_bin.exists(), msg=f"Missing bundled jq: {jq_bin}")
        self.assertTrue(os.access(jq_bin, os.X_OK))
        self.assertTrue(
            not jq_bin.is_symlink() or "prebuilt" in jq_bin.resolve().parts
        )
        self.assertIn(
            "Copyright 2026 The Fuchsia Authors.",
            (runtime_dir / "get_main_pb_path.sh").read_text(encoding="utf-8"),
        )

    def _populate_build_dir(
        self,
        build_dir: Path,
        args_data: dict[str, object],
        pbs_data: list[dict[str, str]],
    ) -> None:
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "args.json").write_text(json.dumps(args_data) + "\n")
        (build_dir / "product_bundles.json").write_text(
            json.dumps(pbs_data) + "\n"
        )

    def test_resolves_main_pb_label_from_args_json_with_scrubbed_path(
        self,
    ) -> None:
        launcher = _find_tool_launcher()
        with tempfile.TemporaryDirectory() as td:
            fuchsia_dir = Path(td) / "fuchsia"
            build_dir = fuchsia_dir / "out" / "default"
            self._populate_build_dir(
                build_dir,
                {"main_pb_label": "//products/minimal:product_bundle.x64"},
                [
                    {
                        "label": "//products/minimal:product_bundle.x64(//build/toolchain/fuchsia:x64)",
                        "name": "minimal.x64",
                        "path": "obj/products/minimal/product_bundle.x64/product_bundle",
                    }
                ],
            )
            (fuchsia_dir / ".fx-build-dir").write_text("out/default\n")

            env = {
                "FUCHSIA_DIR": str(fuchsia_dir),
                "PATH": "/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(
                proc.stdout,
                "obj/products/minimal/product_bundle.x64/product_bundle\n",
            )

    def test_accepts_explicit_pb_label_argument(self) -> None:
        launcher = _find_tool_launcher()
        with tempfile.TemporaryDirectory() as td:
            build_dir = Path(td) / "out"
            self._populate_build_dir(
                build_dir,
                {},
                [
                    {
                        "label": "//products/core:product_bundle.arm64(//build/toolchain/fuchsia:arm64)",
                        "name": "core.arm64",
                        "path": "obj/products/core/product_bundle.arm64/product_bundle",
                    }
                ],
            )
            env = {
                "FUCHSIA_BUILD_DIR": str(build_dir),
                "PATH": "/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher), "//products/core:product_bundle.arm64"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(
                proc.stdout,
                "obj/products/core/product_bundle.arm64/product_bundle\n",
            )

    def test_falls_back_to_build_info_product_and_board(self) -> None:
        launcher = _find_tool_launcher()
        with tempfile.TemporaryDirectory() as td:
            build_dir = Path(td) / "out"
            self._populate_build_dir(
                build_dir,
                {
                    "build_info_product": "workbench_eng",
                    "build_info_board": "x64",
                },
                [
                    {
                        "label": "//products/workbench:workbench_eng.x64",
                        "name": "workbench_eng.x64",
                        "path": "obj/products/workbench/workbench_eng.x64/product_bundle",
                    }
                ],
            )
            env = {
                "FUCHSIA_BUILD_DIR": str(build_dir),
                "PATH": "/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(
                proc.stdout,
                "obj/products/workbench/workbench_eng.x64/product_bundle\n",
            )

    def test_fails_when_no_matching_product_found(self) -> None:
        launcher = _find_tool_launcher()
        with tempfile.TemporaryDirectory() as td:
            build_dir = Path(td) / "out"
            self._populate_build_dir(
                build_dir,
                {"main_pb_label": "//products/nonexistent:pb"},
                [],
            )
            env = {
                "FUCHSIA_BUILD_DIR": str(build_dir),
                "PATH": "/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 1)
            self.assertIn("No matching product found!", proc.stdout)

    def test_honors_fx_build_dir_flag_override(self) -> None:
        launcher = _find_tool_launcher()
        with tempfile.TemporaryDirectory() as td:
            fuchsia_dir = Path(td) / "fuchsia"
            override_dir = fuchsia_dir / "out" / "override"
            self._populate_build_dir(
                override_dir,
                {"main_pb_label": "//products/core:product_bundle.x64"},
                [
                    {
                        "label": "//products/core:product_bundle.x64",
                        "name": "core.x64",
                        "path": "obj/products/core/product_bundle.x64/product_bundle",
                    }
                ],
            )
            (fuchsia_dir / ".fx-build-dir").write_text("out/other\n")
            env = {
                "FUCHSIA_DIR": str(fuchsia_dir),
                "_FX_BUILD_DIR": "out/override",
                "PATH": "/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(
                proc.stdout,
                "obj/products/core/product_bundle.x64/product_bundle\n",
            )

    def test_ignores_host_jq_on_path_and_fails_without_runtime_tool(
        self,
    ) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "get-main-pb-path.runtime_dir"
        raw_script = runtime_dir / "get_main_pb_path.sh"

        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            build_dir = td_path / "out"
            self._populate_build_dir(
                build_dir,
                {"main_pb_label": "//products/minimal:product_bundle.x64"},
                [
                    {
                        "label": "//products/minimal:product_bundle.x64",
                        "name": "minimal.x64",
                        "path": "obj/products/minimal/product_bundle.x64/product_bundle",
                    }
                ],
            )

            poison_bin = td_path / "poison_bin"
            poison_bin.mkdir()
            poison_jq = poison_bin / "jq"
            poison_jq.write_text(
                "#!/bin/bash\necho 'POISON_HOST_JQ_CALLED' >&2\nexit 99\n"
            )
            poison_jq.chmod(0o755)

            env_with_poison = {
                "FUCHSIA_BUILD_DIR": str(build_dir),
                "PATH": f"{poison_bin}:/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher)],
                env=env_with_poison,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertNotIn("POISON_HOST_JQ_CALLED", proc.stderr)
            self.assertEqual(
                proc.stdout,
                "obj/products/minimal/product_bundle.x64/product_bundle\n",
            )

            proc_undeclared = subprocess.run(
                [str(raw_script)],
                env={
                    "FUCHSIA_BUILD_DIR": str(build_dir),
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(proc_undeclared.returncode, 0)

    def test_fails_when_build_dir_missing(self) -> None:
        launcher = _find_tool_launcher()
        with tempfile.TemporaryDirectory() as td:
            missing_dir = Path(td) / "does_not_exist"
            env = {
                "FUCHSIA_BUILD_DIR": str(missing_dir),
                "PATH": "/usr/bin:/bin",
            }
            proc = subprocess.run(
                [str(launcher)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 1)
            self.assertIn("does not exist", proc.stderr)

    def test_empty_env_spaces_and_special_jq_chars(self) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "get-main-pb-path.runtime_dir"
        raw_script = runtime_dir / "get_main_pb_path.sh"

        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)

            isolated_dir = td_path / "isolated"
            isolated_script = isolated_dir / "a" / "b" / "c" / "d" / "script.sh"
            isolated_script.parent.mkdir(parents=True)
            source_script = Path(
                "tools/devshell/host_tools/get_main_pb_path/get_main_pb_path.sh"
            )
            script_content = (
                source_script.read_text(encoding="utf-8")
                if source_script.is_file()
                else raw_script.read_text(encoding="utf-8")
            )
            isolated_script.write_text(script_content, encoding="utf-8")
            isolated_script.chmod(0o755)
            no_env_proc = subprocess.run(
                [str(isolated_script)],
                env={
                    "PATH": "/usr/bin:/bin",
                    "FX_RUNTIME_TOOLS_DIR": str(runtime_dir / ".bin"),
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(no_env_proc.returncode, 1)

            symlink_checkout = td_path / "symlink_checkout"
            (symlink_checkout / ".jiri_root").mkdir(parents=True)
            checkout_script = (
                symlink_checkout
                / "tools"
                / "devshell"
                / "host_tools"
                / "get_main_pb_path"
                / "get_main_pb_path.sh"
            )
            checkout_script.parent.mkdir(parents=True)
            checkout_script.write_text(script_content, encoding="utf-8")
            checkout_script.chmod(0o755)
            self._populate_build_dir(
                symlink_checkout / "out" / "default",
                {"main_pb_label": "//products/minimal:product_bundle.x64"},
                [
                    {
                        "label": "//products/minimal:product_bundle.x64",
                        "name": "minimal.x64",
                        "path": "obj/products/minimal/product_bundle",
                    }
                ],
            )
            (symlink_checkout / ".fx-build-dir").write_text(
                "out/default\n", encoding="utf-8"
            )
            symlink_entry = td_path / "bin_link" / "get-main-pb-path"
            symlink_entry.parent.mkdir(parents=True)
            symlink_entry.symlink_to(checkout_script)
            symlink_proc = subprocess.run(
                [str(symlink_entry)],
                env={
                    "PATH": "/usr/bin:/bin",
                    "FX_RUNTIME_TOOLS_DIR": str(runtime_dir / ".bin"),
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(
                symlink_proc.returncode, 0, msg=symlink_proc.stderr
            )
            self.assertEqual(
                symlink_proc.stdout, "obj/products/minimal/product_bundle\n"
            )

            fuchsia_empty_marker = td_path / "fuchsia_empty_marker"
            fuchsia_empty_marker.mkdir()
            (fuchsia_empty_marker / ".fx-build-dir").write_text(
                "\n", encoding="utf-8"
            )
            empty_marker_proc = subprocess.run(
                [str(launcher)],
                env={
                    "FUCHSIA_DIR": str(fuchsia_empty_marker),
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(empty_marker_proc.returncode, 1)

            fuchsia_spaces = td_path / "fuchsia with spaces"
            spaced_build_dir = fuchsia_spaces / "out" / "default build"
            self._populate_build_dir(
                spaced_build_dir,
                {"main_pb_label": "//products/minimal:product_bundle.x64"},
                [
                    {
                        "label": "//products/minimal:product_bundle.x64",
                        "name": "minimal.x64",
                        "path": "obj/products/minimal/product_bundle",
                    }
                ],
            )
            (fuchsia_spaces / ".fx-build-dir").write_text(
                "//out/default build\n", encoding="utf-8"
            )
            spaced_proc = subprocess.run(
                [str(launcher)],
                env={
                    "FUCHSIA_DIR": str(fuchsia_spaces),
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(spaced_proc.returncode, 0, msg=spaced_proc.stderr)
            self.assertEqual(
                spaced_proc.stdout, "obj/products/minimal/product_bundle\n"
            )

            special_build_dir = td_path / "out_special"
            self._populate_build_dir(
                special_build_dir,
                {
                    "build_info_product": 'prod"quote',
                    "build_info_board": "x64",
                },
                [],
            )
            special_label_proc = subprocess.run(
                [str(launcher), '//products/foo"bar):pb'],
                env={
                    "FUCHSIA_BUILD_DIR": str(special_build_dir),
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(special_label_proc.returncode, 1)
            self.assertEqual(special_label_proc.stderr, "")
            self.assertIn(
                "No matching product found!", special_label_proc.stdout
            )

            special_name_proc = subprocess.run(
                [str(launcher)],
                env={
                    "FUCHSIA_BUILD_DIR": str(special_build_dir),
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(special_name_proc.returncode, 1)
            self.assertEqual(special_name_proc.stderr, "")
            self.assertIn(
                "No matching product found!", special_name_proc.stdout
            )

            # 1. FUCHSIA_BUILD_DIR passed as relative or //out/... by external caller is normalized against FUCHSIA_DIR
            rel_env_proc = subprocess.run(
                [str(launcher)],
                env={
                    "FUCHSIA_DIR": str(fuchsia_spaces),
                    "FUCHSIA_BUILD_DIR": "//out/default build",
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(
                rel_env_proc.returncode, 0, msg=rel_env_proc.stderr
            )
            self.assertEqual(
                rel_env_proc.stdout, "obj/products/minimal/product_bundle\n"
            )

            # 2. Rejects >1 positional arguments with non-zero exit code
            extra_args_proc = subprocess.run(
                [str(launcher), "//products/minimal:pb", "--unexpected-arg"],
                env={
                    "FUCHSIA_BUILD_DIR": str(special_build_dir),
                    "PATH": "/usr/bin:/bin",
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(extra_args_proc.returncode, 0)
            self.assertIn("Usage:", extra_args_proc.stderr)

            # 3. Staged copy inside out/_fx_host_tools/get-main-pb-path.runtime_dir (3 levels deep)
            # or out/_fx_host_tools/vendor/acme/get-main-pb-path.runtime_dir (5 levels deep)
            # locates FUCHSIA_DIR by walking upward when FUCHSIA_DIR is unset in env.
            staged_rt_script = (
                symlink_checkout
                / "out"
                / "_fx_host_tools"
                / "get-main-pb-path.runtime_dir"
                / "get_main_pb_path.sh"
            )
            staged_rt_script.parent.mkdir(parents=True, exist_ok=True)
            staged_rt_script.write_text(script_content, encoding="utf-8")
            staged_rt_script.chmod(0o755)
            staged_proc = subprocess.run(
                [str(staged_rt_script)],
                env={
                    "PATH": "/usr/bin:/bin",
                    "FX_RUNTIME_TOOLS_DIR": str(runtime_dir / ".bin"),
                },
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(staged_proc.returncode, 0, msg=staged_proc.stderr)
            self.assertEqual(
                staged_proc.stdout, "obj/products/minimal/product_bundle\n"
            )


if __name__ == "__main__":
    unittest.main()
