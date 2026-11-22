#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build/subbuild.py."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# Ensure the Fuchsia repository root is in sys.path when run standalone.
_SCRIPT_DIR = Path(__file__).resolve().parent
_FUCHSIA_DIR = _SCRIPT_DIR.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build import subbuild


class HostInfoTest(unittest.TestCase):
    def test_get_host_platform(self) -> None:
        platform = subbuild._get_host_platform()
        self.assertIn(platform, ("linux", "mac"))

    def test_get_host_tag(self) -> None:
        tag = subbuild._get_host_tag()
        self.assertIn(tag, ("linux-x64", "linux-arm64", "mac-x64", "mac-arm64"))


class CommandArgsToStringTest(unittest.TestCase):
    def test_basic_command(self) -> None:
        res = subbuild._command_args_to_string(["echo", "hello"], None, None)
        self.assertEqual(res, "echo hello")

    def test_command_with_cwd(self) -> None:
        res = subbuild._command_args_to_string(
            ["ninja", "all"], None, Path("/tmp/build")
        )
        self.assertEqual(res, "cd /tmp/build && ninja all")

    def test_command_with_special_chars(self) -> None:
        res = subbuild._command_args_to_string(
            ["echo", "hello world", "foo;bar"], None, None
        )
        self.assertEqual(res, "echo 'hello world' 'foo;bar'")


class RunCheckedCommandTest(unittest.TestCase):
    @mock.patch.object(subbuild, "_run_command")
    def test_failure_with_none_stdout_stderr(self, mock_run: mock.Mock) -> None:
        mock_run.return_value = mock.Mock(
            spec=subprocess.CompletedProcess,
            returncode=1,
            stdout=None,
            stderr=None,
        )
        with self.assertLogs(subbuild.logger, level="ERROR") as cm:
            ok = subbuild._run_checked_command(["false"], capture_output=False)
        self.assertFalse(ok)
        log_output = "\n".join(cm.output)
        self.assertNotIn("None", log_output)


class LabelUtilsTest(unittest.TestCase):
    def test_label_partition_valid(self) -> None:
        dir_part, name = subbuild._label_partition("//sdk:fuchsia_collection")
        self.assertEqual(dir_part, "sdk")
        self.assertEqual(name, "fuchsia_collection")

    def test_label_partition_nested(self) -> None:
        dir_part, name = subbuild._label_partition(
            "//src/connectivity/network:bin"
        )
        self.assertEqual(dir_part, "src/connectivity/network")
        self.assertEqual(name, "bin")

    def test_label_partition_invalid(self) -> None:
        with self.assertRaises(ValueError):
            subbuild._label_partition("invalid_label")
        with self.assertRaises(ValueError):
            subbuild._label_partition("//missing_colon")
        with self.assertRaises(ValueError):
            subbuild._label_partition(":no_leading_slash")

    def test_label_to_ninja_target(self) -> None:
        target = subbuild._label_to_ninja_target("//sdk:fuchsia_collection")
        self.assertEqual(target, "sdk:fuchsia_collection")

    def test_label_to_ninja_target_nested(self) -> None:
        target = subbuild._label_to_ninja_target("//zircon/system/ulib/fbl:fbl")
        self.assertEqual(target, "zircon/system/ulib/fbl:fbl")


class SubbuildExecutionTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.tmp_dir.name)
        self.host_tag = subbuild._get_host_tag()

        # Create mock prebuilts.
        self.gn_bin = (
            self.fuchsia_dir
            / "prebuilt"
            / "third_party"
            / "gn"
            / self.host_tag
            / "gn"
        )
        self.gn_bin.parent.mkdir(parents=True, exist_ok=True)
        self.gn_bin.touch(mode=0o755)

        self.ninja_bin = (
            self.fuchsia_dir
            / "prebuilt"
            / "third_party"
            / "ninja"
            / self.host_tag
            / "ninja"
        )
        self.ninja_bin.parent.mkdir(parents=True, exist_ok=True)
        self.ninja_bin.touch(mode=0o755)

        # Create mock ResultStore wrappers.
        self.fuchsia_rsproxy_wrap = (
            self.fuchsia_dir / subbuild._FUCHSIA_RSPROXY_WRAP
        )
        self.fuchsia_rsproxy_wrap.parent.mkdir(parents=True, exist_ok=True)
        self.fuchsia_rsproxy_wrap.touch(mode=0o755)

        self.rsninja_sh = self.fuchsia_dir / subbuild._RSNINJA_SH
        self.rsninja_sh.parent.mkdir(parents=True, exist_ok=True)
        self.rsninja_sh.touch(mode=0o755)

        self.output_build_dir = self.fuchsia_dir / "out" / "subbuild"
        self.output_build_dir.mkdir(parents=True, exist_ok=True)

        # Create mock subbuild_config.json and icu_build_config.json.
        (self.output_build_dir.parent / "icu_build_config.json").touch()
        config_data = {
            "build_should_trace_actions": False,
            "compilation_mode": "release",
            "compress_debuginfo": "zstd",
            "cxx_rbe_enable": True,
            "cxx_rbe_exec_strategy": "remote",
            "enable_jobserver": False,
            "link_rbe_enable": True,
            "link_rbe_exec_strategy": "remote",
            "rust_rbe_enable": True,
            "rust_rbe_exec_strategy": "remote",
            "sdk_id": "33.99991231.0.1",
            "sdk_sub_build_verbose": False,
            "update_goldens": False,
        }
        (self.output_build_dir.parent / "subbuild_config.json").write_text(
            json.dumps(config_data)
        )

        self.base_args = [
            "--fuchsia-dir",
            str(self.fuchsia_dir),
            "--target-label",
            "//sdk:fuchsia_collection",
            "--target-cpu",
            "x64",
            "--api-level",
            "33",
            "--output-build-dir",
            str(self.output_build_dir),
        ]

    def tearDown(self) -> None:
        self.tmp_dir.cleanup()

    def test_missing_gn_prebuilt_fails(self) -> None:
        self.gn_bin.unlink()
        exit_code = subbuild.main(self.base_args, {})
        self.assertEqual(exit_code, 1)

    def test_missing_ninja_prebuilt_fails(self) -> None:
        self.ninja_bin.unlink()
        exit_code = subbuild.main(self.base_args, {})
        self.assertEqual(exit_code, 1)

    @mock.patch.object(
        subbuild,
        "_run_command",
        return_value=mock.Mock(
            spec=subprocess.CompletedProcess, returncode=0, stdout="", stderr=""
        ),
    )
    @mock.patch.object(subbuild, "_run_checked_command", return_value=True)
    def test_clean_invokes_ninja_directly_without_quiet(
        self, mock_checked: mock.Mock, mock_run: mock.Mock
    ) -> None:
        exit_code = subbuild.main([*self.base_args, "--clean"], {})

        self.assertEqual(exit_code, 0)
        # Verify run_command was called with ninja_bin directly and without --quiet.
        mock_run.assert_called_once()
        clean_cmd = mock_run.call_args[0][0]
        self.assertEqual(clean_cmd[0], str(self.ninja_bin))
        self.assertIn("-t", clean_cmd)
        self.assertIn("clean", clean_cmd)
        self.assertNotIn("--quiet", clean_cmd)

    @mock.patch.object(
        subbuild,
        "_run_command",
        return_value=mock.Mock(
            spec=subprocess.CompletedProcess, returncode=0, stdout="", stderr=""
        ),
    )
    @mock.patch.object(subbuild, "_run_checked_command", return_value=True)
    def test_build_invokes_wrapper_pair_unconditionally(
        self, mock_checked: mock.Mock, mock_run: mock.Mock
    ) -> None:
        exit_code = subbuild.main(self.base_args, {})

        self.assertEqual(exit_code, 0)
        self.assertEqual(mock_checked.call_count, 2)
        full_cmd = mock_checked.call_args_list[1][0][0]
        child_env = mock_checked.call_args_list[1].kwargs["env"]

        # Verify fuchsia-rsproxy-wrap.sh wraps rsninja.sh directly.
        self.assertEqual(full_cmd[0], str(self.fuchsia_rsproxy_wrap))
        self.assertEqual(full_cmd[1], "--")
        self.assertEqual(full_cmd[2], str(self.rsninja_sh))
        # Ensure ninja_bin is not treated as a build target after rsninja.sh.
        self.assertNotEqual(full_cmd[3], str(self.ninja_bin))
        self.assertIn("sdk:fuchsia_collection", full_cmd)
        self.assertEqual(
            child_env,
            {"NINJA_STATUS": "IDK_SUBBUILD_api_33_x64 [%f/%t][%p/%w](%r) "},
        )

    @mock.patch.object(
        subbuild,
        "_run_checked_command",
        return_value=True,
        autospec=True,
        spec_set=True,
    )
    def test_propagates_disable_xattr_for_rbe(
        self, mock_checked: mock.Mock
    ) -> None:
        exit_code = subbuild.main((*self.base_args, "--setup-only"), {})
        self.assertEqual(exit_code, 0)
        self.assertNotIn(
            "disable_xattr_for_rbe",
            (self.output_build_dir / "args.gn").read_text(),
        )

        cfg_path = self.output_build_dir.parent / "subbuild_config.json"
        cfg = json.loads(cfg_path.read_text())
        cfg["disable_xattr_for_rbe"] = True
        cfg_path.write_text(json.dumps(cfg))

        exit_code = subbuild.main((*self.base_args, "--setup-only"), {})
        self.assertEqual(exit_code, 0)
        self.assertIn(
            "disable_xattr_for_rbe = true\n",
            (self.output_build_dir / "args.gn").read_text(),
        )


if __name__ == "__main__":
    unittest.main()
