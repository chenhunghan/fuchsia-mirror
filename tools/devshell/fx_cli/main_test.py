# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for fx_cli.main."""

import io
import json
import os
import pathlib
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
from collections.abc import Callable, Sequence
from typing import Any, TextIO
from unittest import mock

import main as fx_main
import test_utils


def _bound_arg(mock_fn: mock.MagicMock, index: int, name: str) -> Any:
    assert mock_fn.call_args is not None
    kwargs = mock_fn.call_args.kwargs
    if name in kwargs:
        return kwargs[name]
    return mock_fn.call_args.args[index]


class _RecordingStringIO(io.StringIO):
    def __init__(self, trace_events: list[str], label: str) -> None:
        super().__init__()
        self._trace_events = trace_events
        self._label = label

    def write(self, s: str) -> int:
        if not self._trace_events or self._trace_events[-1] != self._label:
            self._trace_events.append(self._label)
        return super().write(s)


def _locate_scripts_fx() -> pathlib.Path | None:
    abs_file = pathlib.Path(__file__).absolute()
    for base_file in (abs_file, abs_file.resolve()):
        test_data_fx = base_file.parent.parent / "test_data" / "fx"
        if test_data_fx.is_file():
            return test_data_fx
        for parent in base_file.parents:
            candidate = parent / "scripts" / "fx"
            if candidate.is_file():
                return candidate
    runfiles_dir = os.environ.get("RUNFILES_DIR")
    if runfiles_dir:
        rf_root = pathlib.Path(runfiles_dir)
        for rel in (
            pathlib.Path("_main") / "scripts" / "fx",
            pathlib.Path("scripts") / "fx",
        ):
            candidate = rf_root / rel
            if candidate.is_file():
                return candidate
    env_fdir = os.environ.get("FUCHSIA_DIR")
    if env_fdir:
        candidate = pathlib.Path(env_fdir) / "scripts" / "fx"
        if candidate.is_file():
            return candidate
    return None


def _python_launcher(executable: str = sys.executable) -> str:
    """Return a Python executable safe to run after PYTHONHOME is unset.

    Under Bazel host_py_test, sys.executable is @prebuilt_python's
    `python<X>.<Y>-real`, which relies on its sibling `python3` launcher script
    to restore PYTHONHOME and `lib_python<X>.<Y>.zip` when callers like
    `scripts/fx` unset ambient PYTHONHOME.
    """
    py_path = pathlib.Path(executable).absolute()
    launcher = py_path.parent / "python3"
    if py_path.name.endswith("-real") and launcher.is_file():
        return str(launcher)
    return executable


class MainTest(unittest.TestCase):
    def setUp(self) -> None:
        env_patcher = mock.patch.dict(os.environ, {"HOME": ""}, clear=False)
        env_patcher.start()
        self.addCleanup(env_patcher.stop)
        os.environ.pop("_FX_START_TIME_MICROS", None)
        os.environ.pop("FUCHSIA_METRICS_CALL_STACK", None)

        self._tmp = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self._tmp.name)
        (self.fuchsia_dir / ".fx-root").write_text("", encoding="utf-8")
        reg_dir = self.fuchsia_dir / "tools" / "devshell"
        reg_dir.mkdir(parents=True, exist_ok=True)
        (reg_dir / "fx_host_tools.json").write_text(
            json.dumps(
                {
                    "get-src-dir": {
                        "category": "Build",
                        "summary": "print the root of the Fuchsia source tree",
                        "script": "//tools/devshell/host_tools/get_src_dir/get_src_dir.sh",
                    },
                    "my-reg-cmd": {
                        "category": "Other",
                        "summary": "registered command",
                        "script": "//tools/my_reg_cmd.sh",
                    },
                }
            ),
            encoding="utf-8",
        )
        reg_cmd_script = self.fuchsia_dir / "tools" / "my_reg_cmd.sh"
        reg_cmd_script.write_text(
            '#!/bin/bash\necho "ran: $*"\n', encoding="utf-8"
        )
        reg_cmd_script.chmod(0o755)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_parse_global_args_all_options(self) -> None:
        parsed = fx_main.parse_global_args(
            [
                "--dir",
                "out/custom",
                "-t",
                "fuchsia-node",
                "-i",
                "-x",
                "-xx",
                "--no-build",
                "--build",
                "--enable=incremental",
                "--disable",
                "legacy_fastboot",
                "--invoker",
                "ffx_wrapper",
                "my-reg-cmd",
                "--flag",
                "arg1",
            ]
        )
        self.assertEqual(parsed.build_dir, "out/custom")
        self.assertEqual(parsed.target_device, "fuchsia-node")
        self.assertTrue(parsed.iterative)
        self.assertTrue(parsed.trace_x)
        self.assertTrue(parsed.trace_xx)
        self.assertFalse(parsed.no_build)
        self.assertTrue(parsed.force_build)
        self.assertIn(("enable", "incremental"), parsed.feature_flags)
        self.assertIn(("disable", "legacy_fastboot"), parsed.feature_flags)
        self.assertEqual(parsed.invoker, "ffx_wrapper")
        self.assertEqual(parsed.command_name, "my-reg-cmd")
        self.assertSequenceEqual(parsed.command_args, ("--flag", "arg1"))

    def test_parse_global_args_help_flag(self) -> None:
        parsed = fx_main.parse_global_args(["-h", "my-reg-cmd"])
        self.assertEqual(parsed.command_name, "help")
        self.assertSequenceEqual(parsed.command_args, ("my-reg-cmd",))

        parsed_h = fx_main.parse_global_args(["-h"])
        self.assertEqual(parsed_h.command_name, "help")
        self.assertSequenceEqual(parsed_h.command_args, ())

        parsed_h_full = fx_main.parse_global_args(["-h", "--full"])
        self.assertEqual(parsed_h_full.command_name, "help")
        self.assertSequenceEqual(parsed_h_full.command_args, ("--full",))

        with self.assertRaises(fx_main.GlobalArgError) as ctx_empty:
            fx_main.parse_global_args([])
        self.assertIn("Missing command name", str(ctx_empty.exception))
        self.assertTrue(ctx_empty.exception.show_usage)

        stdout_buf1 = io.StringIO()
        stdout_buf2 = io.StringIO()
        stdout_buf3 = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.object(
            fx_main, "exec_legacy_fx", autospec=True, return_value=0
        ) as mock_legacy:
            rc1 = fx_main.main(
                ["-h"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf1,
                stderr=stderr_buf,
            )
            rc2 = fx_main.main(
                ["-h", "--full"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf2,
                stderr=stderr_buf,
            )
            rc3 = fx_main.main(
                ["help", "--no-contrib"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf3,
                stderr=stderr_buf,
            )
        self.assertEqual(rc1, 0)
        self.assertEqual(rc2, 0)
        self.assertEqual(rc3, 0)
        self.assertIn("usage: fx [--dir BUILD_DIR]", stdout_buf1.getvalue())
        self.assertEqual(stdout_buf2.getvalue(), "")
        self.assertEqual(stdout_buf1.getvalue(), stdout_buf3.getvalue())
        self.assertEqual(stderr_buf.getvalue(), "")
        mock_legacy.assert_called_once_with(self.fuchsia_dir, ("-h", "--full"))

    def test_is_valid_device_supports_all_vars_sh_formats(self) -> None:
        valid_devices = (
            "",
            "serial:EM-C0B2F2D96",
            "id:EM-C0B2F2D96",
            "usb:cid:42",
            "vsock:cid:3",
            "uart:/dev/ttyUSB0",
            "[fe80::7:8%eth-0]",
            "[fe80::7:8%eth0]:5222",
            "192.168.3.1:8022",
            "strut-wind-ahead-turf",
            ":8022",
            ":8122",
        )
        for dev in valid_devices:
            self.assertTrue(
                fx_main.is_valid_device(dev), f"Expected valid device: {dev}"
            )
            parsed = fx_main.parse_global_args(["-t", dev, "my-reg-cmd"])
            self.assertEqual(parsed.target_device, dev)

        invalid_devices = (
            "   ",
            "invalid device with spaces",
            "AAbbb",
            "aa_bbb",
            ":122333",
        )
        for dev in invalid_devices:
            self.assertFalse(
                fx_main.is_valid_device(dev), f"Expected invalid device: {dev}"
            )

        parsed2 = fx_main.parse_global_args(["--help"])
        self.assertEqual(parsed2.command_name, "help")
        self.assertSequenceEqual(parsed2.command_args, ())

    def test_empty_target_flag_matches_bash_semantics(self) -> None:
        parsed_short = fx_main.parse_global_args(["-t", "", "my-reg-cmd"])
        self.assertEqual(parsed_short.target_device, "")
        self.assertEqual(parsed_short.command_name, "my-reg-cmd")
        self.assertSequenceEqual(parsed_short.raw_global_args, ("-t", ""))

        parsed_long_eq = fx_main.parse_global_args(["--target=", "my-reg-cmd"])
        self.assertEqual(parsed_long_eq.target_device, "")
        self.assertEqual(parsed_long_eq.command_name, "my-reg-cmd")
        self.assertSequenceEqual(parsed_long_eq.raw_global_args, ("--target=",))

        parsed_short_eq = fx_main.parse_global_args(["-t=", "my-reg-cmd"])
        self.assertEqual(parsed_short_eq.target_device, "")
        self.assertEqual(parsed_short_eq.command_name, "my-reg-cmd")
        self.assertSequenceEqual(parsed_short_eq.raw_global_args, ("-t=",))

        parsed_override_empty = fx_main.parse_global_args(
            ["-t", "first-dev", "-t", "", "my-reg-cmd"]
        )
        self.assertEqual(parsed_override_empty.target_device, "")

        parsed_override_nonempty = fx_main.parse_global_args(
            ["-t", "", "-t", "second-dev", "my-reg-cmd"]
        )
        self.assertEqual(parsed_override_nonempty.target_device, "second-dev")

        for no_cmd_argv in (("-t", ""), ("--target=",), ("-t=",)):
            with self.assertRaises(fx_main.GlobalArgError) as ctx_no_cmd:
                fx_main.parse_global_args(no_cmd_argv)
            self.assertIn("Missing command name", str(ctx_no_cmd.exception))
            self.assertTrue(ctx_no_cmd.exception.show_usage)

        with self.assertRaises(fx_main.GlobalArgError) as ctx_missing_val:
            fx_main.parse_global_args(["-t"])
        self.assertIn(
            "Missing device name for -t argument",
            str(ctx_missing_val.exception),
        )
        self.assertTrue(ctx_missing_val.exception.show_usage)

        build_dir = self.fuchsia_dir / "out" / "default"
        build_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / "out" / "default.device").write_text(
            "file-default-device\n", encoding="utf-8"
        )

        env_dump_script = (
            "#!/bin/bash\n"
            'printf "NODENAME=%s\\n" "${FUCHSIA_NODENAME-<unset>}"\n'
            'printf "SET_BY_FLAG=%s\\n" "${FUCHSIA_NODENAME_SET_BY_FX_FLAG-<unset>}"\n'
            'printf "ADDR=%s\\n" "${FUCHSIA_DEVICE_ADDR-<unset>}"\n'
            'printf "FROM_FILE=%s\\n" "${FUCHSIA_NODENAME_IS_FROM_FILE-<unset>}"\n'
        )
        reg_cmd_script = self.fuchsia_dir / "tools" / "my_reg_cmd.sh"
        reg_cmd_script.write_text(env_dump_script, encoding="utf-8")
        reg_cmd_script.chmod(0o755)

        expected_env_lines = (
            "NODENAME=",
            "SET_BY_FLAG=FUCHSIA_NODENAME_SET_BY_FX_FLAG",
            "ADDR=<unset>",
            "FROM_FILE=<unset>",
        )
        inherited_target_env = {
            "FUCHSIA_NODENAME": "env-dev",
            "FUCHSIA_DEVICE_ADDR": "192.0.2.1",
            "FUCHSIA_NODENAME_IS_FROM_FILE": "true",
        }

        # Registered script: -t "" and --target= clear inherited target vars and
        # export FUCHSIA_NODENAME="" with FUCHSIA_NODENAME_SET_BY_FX_FLAG.
        for flag_args in (
            ("--dir", "out/default", "-t", "", "my-reg-cmd"),
            ("--dir", "out/default", "--target=", "my-reg-cmd"),
            ("--dir", "out/default", "-t", "first-dev", "-t", "", "my-reg-cmd"),
        ):
            stdout_buf = io.StringIO()
            stderr_buf = io.StringIO()
            with mock.patch.dict(os.environ, inherited_target_env, clear=False):
                rc = fx_main.main(
                    flag_args,
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=stdout_buf,
                    stderr=stderr_buf,
                )
            self.assertEqual(rc, 0, msg=stderr_buf.getvalue())
            self.assertSequenceEqual(
                stdout_buf.getvalue().splitlines(), expected_env_lines
            )

        # Registered target: <build_dir>.device must not override explicit -t "".
        reg_file = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )
        reg_data = json.loads(reg_file.read_text(encoding="utf-8"))
        reg_data["my-target-cmd"] = {
            "category": "Other",
            "summary": "registered target command",
            "target": "//tools/foo:bar",
        }
        reg_file.write_text(json.dumps(reg_data), encoding="utf-8")
        target_bin = self.fuchsia_dir / "tools" / "my_target_cmd.sh"
        target_bin.write_text(env_dump_script, encoding="utf-8")
        target_bin.chmod(0o755)

        stdout_target = io.StringIO()
        stderr_target = io.StringIO()
        with mock.patch.dict(os.environ, inherited_target_env, clear=False):
            with mock.patch.object(
                fx_main.builder,
                "ensure_registered_host_tool",
                autospec=True,
                spec_set=True,
                return_value=target_bin,
            ):
                rc_target = fx_main.main(
                    ["--dir", "out/default", "--target=", "my-target-cmd"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=stdout_target,
                    stderr=stderr_target,
                )
        self.assertEqual(rc_target, 0, msg=stderr_target.getvalue())
        self.assertSequenceEqual(
            stdout_target.getvalue().splitlines(), expected_env_lines
        )

        # Built-in subcommand accepts -t "".
        stdout_builtin = io.StringIO()
        stderr_builtin = io.StringIO()
        with mock.patch.dict(os.environ, inherited_target_env, clear=False):
            rc_builtin = fx_main.main(
                ["-t", "", "get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_builtin,
                stderr=stderr_builtin,
            )
        self.assertEqual(rc_builtin, 0, msg=stderr_builtin.getvalue())
        self.assertEqual(
            stdout_builtin.getvalue().strip(), str(self.fuchsia_dir)
        )

        # Legacy fallback forwards -t "" and --target= verbatim to scripts/fx.
        for legacy_argv in (
            ("-t", "", "unmigrated-cmd"),
            ("--target=", "unmigrated-cmd"),
        ):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                spec_set=True,
                return_value=0,
            ) as mock_legacy:
                rc_legacy = fx_main.main(
                    legacy_argv,
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
            self.assertEqual(rc_legacy, 0)
            mock_legacy.assert_called_once_with(self.fuchsia_dir, legacy_argv)

    def test_parse_global_args_rejects_deprecated_d_config_and_invalid_dir(
        self,
    ) -> None:
        with self.assertRaises(fx_main.GlobalArgError) as ctx:
            fx_main.parse_global_args(["-d", "device", "build"])
        self.assertIn("Unknown global argument -d", str(ctx.exception))

        with self.assertRaises(fx_main.GlobalArgError) as ctx_cfg:
            fx_main.parse_global_args(["--config", "my_cfg", "build"])
        self.assertIn(
            "Unknown global argument --config", str(ctx_cfg.exception)
        )

        with self.assertRaises(fx_main.GlobalArgError) as ctx2:
            fx_main.parse_global_args(["--dir", "not_out_dir", "build"])
        self.assertIn(
            "Build directory (--dir BUILD_DIR) must be a subdirectory of",
            str(ctx2.exception),
        )

    def test_fx_reentry_mode(self) -> None:
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        # Missing FX_ENTRY_CMD fails
        with mock.patch.dict(os.environ, {"FX_ENTRY_CMD": ""}, clear=False):
            rc = fx_main.run_fx_reentry(
                self.fuchsia_dir,
                ["get-src-dir"],
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
        self.assertNotEqual(rc, 0)

        # With FX_ENTRY_CMD set, executes command
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FX_ENTRY_CMD": "parent-cmd"}, clear=False
        ):
            rc = fx_main.run_fx_reentry(
                self.fuchsia_dir,
                ["get-src-dir"],
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
        self.assertEqual(rc, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(self.fuchsia_dir))

        # Missing command prints error and returns 1
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FX_ENTRY_CMD": "parent-cmd"}, clear=False
        ):
            rc_missing = fx_main.run_fx_reentry(
                self.fuchsia_dir,
                [],
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
        self.assertEqual(rc_missing, 1)
        self.assertIn(
            "ERROR: No command or function specified.", stderr_buf.getvalue()
        )

    def test_direct_dispatch_vs_legacy_fallback(self) -> None:
        # Built-in / registered command dispatches directly without calling legacy fallback
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.object(
            fx_main, "exec_legacy_fx", autospec=True
        ) as mock_legacy:
            rc = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
        self.assertEqual(rc, 0)
        self.assertEqual(mock_legacy.call_count, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(self.fuchsia_dir))

        # Unmigrated legacy command delegates to scripts/fx-legacy with pre-resolved env vars
        legacy_script = self.fuchsia_dir / "tools" / "devshell" / "legacy-cmd"
        legacy_script.write_text("#!/bin/bash\n", encoding="utf-8")
        legacy_script.chmod(0o755)

        captured_env: dict[str, str] = {}

        def _fake_exec_legacy(fdir: pathlib.Path, argv: Sequence[str]) -> int:
            del fdir, argv
            for k in (
                "_FX_PRE_RESOLVED_CMD",
                "_FX_PRE_RESOLVED_KIND",
                "_FX_PRE_RESOLVED_VENDOR",
                "_FX_PRE_RESOLVED_EXEC",
                "_FX_PRE_RESOLVED_MODE",
            ):
                if k in os.environ:
                    captured_env[k] = os.environ[k]
            return 0

        with mock.patch.object(
            fx_main,
            "exec_legacy_fx",
            autospec=True,
            side_effect=_fake_exec_legacy,
        ) as mock_legacy2:
            rc2 = fx_main.main(
                ["--dir", "out/default", "legacy-cmd", "arg1"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
        self.assertEqual(rc2, 0)
        self.assertEqual(mock_legacy2.call_count, 1)
        self.assertEqual(captured_env.get("_FX_PRE_RESOLVED_CMD"), "legacy-cmd")
        self.assertEqual(
            captured_env.get("_FX_PRE_RESOLVED_KIND"), "legacy_script"
        )
        self.assertEqual(
            captured_env.get("_FX_PRE_RESOLVED_EXEC"), str(legacy_script)
        )
        self.assertEqual(captured_env.get("_FX_PRE_RESOLVED_MODE"), "legacy")

    def test_legacy_fallback_pre_resolved_not_found(self) -> None:
        captured_env: dict[str, str] = {}

        def _fake_exec_legacy(fdir: pathlib.Path, argv: Sequence[str]) -> int:
            del fdir, argv
            for k in (
                "_FX_PRE_RESOLVED_CMD",
                "_FX_PRE_RESOLVED_KIND",
                "_FX_PRE_RESOLVED_VENDOR",
                "_FX_PRE_RESOLVED_EXEC",
                "_FX_PRE_RESOLVED_MODE",
            ):
                if k in os.environ:
                    captured_env[k] = os.environ[k]
            return 1

        with mock.patch.object(
            fx_main,
            "exec_legacy_fx",
            autospec=True,
            side_effect=_fake_exec_legacy,
        ) as mock_legacy:
            rc = fx_main.main(
                ["nonexistent-cmd-xyz"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
        self.assertEqual(rc, 1)
        mock_legacy.assert_called_once_with(
            self.fuchsia_dir, ("nonexistent-cmd-xyz",)
        )
        self.assertEqual(
            captured_env.get("_FX_PRE_RESOLVED_CMD"), "nonexistent-cmd-xyz"
        )
        self.assertEqual(captured_env.get("_FX_PRE_RESOLVED_KIND"), "not_found")
        self.assertEqual(captured_env.get("_FX_PRE_RESOLVED_EXEC"), "")

    def test_pre_resolved_env_stripped_on_direct_vendor_help_and_reentry(
        self,
    ) -> None:
        stale_env = {
            "_FX_PRE_RESOLVED_CMD": "stale-cmd",
            "_FX_PRE_RESOLVED_KIND": "not_found",
            "_FX_PRE_RESOLVED_VENDOR": "stale-vendor",
            "_FX_PRE_RESOLVED_EXEC": "/stale/exec",
            "_FX_PRE_RESOLVED_MODE": "legacy",
        }
        pre_keys = tuple(stale_env.keys())

        # Direct registered command must not observe inherited _FX_PRE_RESOLVED_*.
        reg_cmd_script = self.fuchsia_dir / "tools" / "my_reg_cmd.sh"
        reg_cmd_script.write_text(
            "#!/bin/bash\n"
            "for v in _FX_PRE_RESOLVED_CMD _FX_PRE_RESOLVED_KIND "
            "_FX_PRE_RESOLVED_VENDOR _FX_PRE_RESOLVED_EXEC _FX_PRE_RESOLVED_MODE; do\n"
            '  if [[ -n "${!v+x}" ]]; then echo "LEAKED:${v}=${!v}"; fi\n'
            "done\n"
            'echo "CLEAN_DIRECT"\n',
            encoding="utf-8",
        )
        reg_cmd_script.chmod(0o755)
        with mock.patch.dict(os.environ, stale_env, clear=False):
            out_direct = io.StringIO()
            rc_direct = fx_main.main(
                ["my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=out_direct,
                stderr=io.StringIO(),
            )
        self.assertEqual(rc_direct, 0)
        self.assertEqual(out_direct.getvalue().strip(), "CLEAN_DIRECT")

        # `fx vendor <v> <legacy-cmd>` fallback must not populate or forward _FX_PRE_RESOLVED_*.
        vendor_script = (
            self.fuchsia_dir
            / "vendor"
            / "acme"
            / "scripts"
            / "devshell"
            / "v-legacy"
        )
        vendor_script.parent.mkdir(parents=True, exist_ok=True)
        vendor_script.write_text("#!/bin/bash\n", encoding="utf-8")
        vendor_script.chmod(0o755)

        vendor_captured: dict[str, str] = {}

        def _capture_vendor(fdir: pathlib.Path, argv: Sequence[str]) -> int:
            del fdir, argv
            for k in pre_keys:
                if k in os.environ:
                    vendor_captured[k] = os.environ[k]
            return 0

        with mock.patch.dict(os.environ, stale_env, clear=False):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                side_effect=_capture_vendor,
            ) as mock_vendor_legacy:
                rc_vendor = fx_main.main(
                    ["vendor", "acme", "v-legacy"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
        self.assertEqual(rc_vendor, 0)
        mock_vendor_legacy.assert_called_once_with(
            self.fuchsia_dir, ("vendor", "acme", "v-legacy")
        )
        self.assertEqual(vendor_captured, {})

        # `fx help --full` fallback must not forward inherited _FX_PRE_RESOLVED_*.
        help_captured: dict[str, str] = {}

        def _capture_help(fdir: pathlib.Path, argv: Sequence[str]) -> int:
            del fdir, argv
            for k in pre_keys:
                if k in os.environ:
                    help_captured[k] = os.environ[k]
            return 0

        with mock.patch.dict(os.environ, stale_env, clear=False):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                side_effect=_capture_help,
            ) as mock_help_legacy:
                rc_help = fx_main.main(
                    ["help", "--full"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
        self.assertEqual(rc_help, 0)
        mock_help_legacy.assert_called_once_with(
            self.fuchsia_dir, ("help", "--full")
        )
        self.assertEqual(help_captured, {})

        # `run_fx_reentry` on a legacy command must neither populate nor forward _FX_PRE_RESOLVED_*.
        legacy_script = self.fuchsia_dir / "tools" / "devshell" / "legacy-re"
        legacy_script.write_text("#!/bin/bash\n", encoding="utf-8")
        legacy_script.chmod(0o755)
        reentry_script = self.fuchsia_dir / "scripts" / "fx-reentry"
        reentry_script.parent.mkdir(parents=True, exist_ok=True)
        reentry_script.write_text(
            "#!/bin/bash\n"
            "for v in _FX_PRE_RESOLVED_CMD _FX_PRE_RESOLVED_KIND "
            "_FX_PRE_RESOLVED_VENDOR _FX_PRE_RESOLVED_EXEC _FX_PRE_RESOLVED_MODE; do\n"
            '  if [[ -n "${!v+x}" ]]; then echo "LEAKED:${v}=${!v}"; fi\n'
            "done\n"
            'echo "CLEAN_REENTRY|$*"\n',
            encoding="utf-8",
        )
        reentry_script.chmod(0o755)
        with mock.patch.dict(
            os.environ, {**stale_env, "FX_ENTRY_CMD": "parent"}, clear=False
        ):
            out_reentry = io.StringIO()
            rc_reentry = fx_main.run_fx_reentry(
                self.fuchsia_dir,
                ["legacy-re", "arg1"],
                stdout=out_reentry,
                stderr=io.StringIO(),
            )
            remaining = {k: os.environ[k] for k in pre_keys if k in os.environ}
        self.assertEqual(rc_reentry, 0)
        self.assertEqual(
            out_reentry.getvalue().strip(), "CLEAN_REENTRY|legacy-re arg1"
        )
        self.assertEqual(remaining, {})

    @mock.patch.object(fx_main, "_run_iterative", autospec=True, spec_set=True)
    def test_iterative_mode_does_not_populate_pre_resolved_env(
        self, mock_iterative: mock.MagicMock
    ) -> None:
        reg_file = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )
        observed_after_rerun: dict[str, str] = {}

        def _fake_iterative(
            fdir: pathlib.Path,
            display_name: str,
            run_cmd: Callable[[], int],
            *,
            stdout: TextIO | None = None,
        ) -> int:
            del fdir, display_name, stdout
            # Simulate the registered tool disappearing before the next iteration.
            reg_file.write_text("{}", encoding="utf-8")
            rc_rerun = run_cmd()
            for k in (
                "_FX_PRE_RESOLVED_CMD",
                "_FX_PRE_RESOLVED_KIND",
                "_FX_PRE_RESOLVED_VENDOR",
                "_FX_PRE_RESOLVED_EXEC",
                "_FX_PRE_RESOLVED_MODE",
            ):
                if k in os.environ:
                    observed_after_rerun[k] = os.environ[k]
            return rc_rerun

        mock_iterative.side_effect = _fake_iterative
        rc = fx_main.main(
            ["-i", "my-reg-cmd"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertEqual(rc, 0)
        mock_iterative.assert_called_once()
        self.assertEqual(observed_after_rerun, {})

    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_finish_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_start_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_direct_dispatch_with_metrics_config_present(
        self,
        mock_legacy: mock.MagicMock,
        mock_start: mock.MagicMock,
        mock_finish: mock.MagicMock,
    ) -> None:
        # Even when .fx/config/metrics exists, direct commands must execute in
        # Python and report 2-phase metrics instead of calling exec_legacy_fx.
        metrics_cfg = self.fuchsia_dir / ".fx" / "config" / "metrics"
        metrics_cfg.parent.mkdir(parents=True, exist_ok=True)
        metrics_cfg.write_text(
            'METRICS_ENABLED=1\nMETRICS_UUID="test-uuid"\n', encoding="utf-8"
        )

        observed_stacks: list[str | None] = []
        mock_start.side_effect = (
            lambda *_args, **_kwargs: observed_stacks.append(
                os.environ.get("FUCHSIA_METRICS_CALL_STACK")
            )
        )
        mock_finish.side_effect = (
            lambda *_args, **_kwargs: observed_stacks.append(
                os.environ.get("FUCHSIA_METRICS_CALL_STACK")
            )
        )

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {
                "HOME": str(self.fuchsia_dir),
                "XDG_DATA_HOME": str(self.fuchsia_dir / "xdg"),
                "HOSTNAME": "ext.example.com",
            },
            clear=False,
        ):
            rc = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
            self.assertEqual(os.environ.get("FUCHSIA_METRICS_CALL_STACK"), "fx")

        self.assertEqual(rc, 0)
        self.assertEqual(mock_legacy.call_count, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(self.fuchsia_dir))
        self.assertEqual(stderr_buf.getvalue(), "")
        self.assertSequenceEqual(observed_stacks, ("fx", "fx"))

        self.assertEqual(mock_start.call_count, 1)
        self.assertEqual(
            _bound_arg(mock_start, 0, "fuchsia_dir"), self.fuchsia_dir
        )
        self.assertEqual(
            _bound_arg(mock_start, 1, "command_name"), "get-src-dir"
        )
        self.assertSequenceEqual(_bound_arg(mock_start, 2, "command_args"), ())
        start_cfg = _bound_arg(mock_start, 3, "cfg")
        self.assertEqual(start_cfg.level, 1)
        self.assertEqual(start_cfg.enabled, 1)
        self.assertEqual(start_cfg.uuid, "test-uuid")
        self.assertFalse(start_cfg.is_internal)

        self.assertEqual(mock_finish.call_count, 1)
        self.assertEqual(
            _bound_arg(mock_finish, 0, "fuchsia_dir"), self.fuchsia_dir
        )
        self.assertEqual(
            _bound_arg(mock_finish, 1, "command_name"), "get-src-dir"
        )
        self.assertSequenceEqual(_bound_arg(mock_finish, 2, "command_args"), ())
        self.assertLessEqual(
            _bound_arg(mock_finish, 3, "start_time_micros"),
            _bound_arg(mock_finish, 4, "end_time_micros"),
        )
        self.assertEqual(_bound_arg(mock_finish, 5, "exit_status"), 0)
        self.assertEqual(_bound_arg(mock_finish, 6, "cfg"), start_cfg)

    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_finish_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_start_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics, "init_command_metrics", autospec=True, spec_set=True
    )
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_two_phase_metrics_before_execute_ordering(
        self,
        mock_legacy: mock.MagicMock,
        mock_init: mock.MagicMock,
        mock_start: mock.MagicMock,
        mock_finish: mock.MagicMock,
    ) -> None:
        cfg = fx_main.metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="test-uuid",
            debug_log_file="",
            other_uuid="",
            is_internal=False,
        )

        # 1. Built-in command: init -> start -> execute -> finish
        builtin_events: list[str] = []

        def _builtin_init(
            fuchsia_dir: pathlib.Path,
            command_name: str,
            command_args: Sequence[str],
            **_kwargs: Any,
        ) -> Any:
            del fuchsia_dir, command_name, command_args
            builtin_events.append("init")
            return cfg

        def _builtin_start(*_args: Any, **_kwargs: Any) -> None:
            builtin_events.append("start")

        def _builtin_finish(*_args: Any, **_kwargs: Any) -> None:
            builtin_events.append("finish")

        mock_init.side_effect = _builtin_init
        mock_start.side_effect = _builtin_start
        mock_finish.side_effect = _builtin_finish

        rec_stdout = _RecordingStringIO(builtin_events, "execute")
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FUCHSIA_METRICS_CALL_STACK": "parent"}, clear=False
        ):
            rc_builtin = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=rec_stdout,
                stderr=stderr_buf,
            )
            self.assertEqual(
                os.environ.get("FUCHSIA_METRICS_CALL_STACK"), "parent fx"
            )
        self.assertEqual(rc_builtin, 0)
        self.assertSequenceEqual(
            builtin_events, ("init", "start", "execute", "finish")
        )

        # 2. Registered script: call stack updated after init and before child
        trace_file = self.fuchsia_dir / "reg_trace.log"
        reg_cmd_script = self.fuchsia_dir / "tools" / "my_reg_cmd.sh"
        reg_cmd_script.write_text(
            f'#!/bin/bash\necho "execute:${{FUCHSIA_METRICS_CALL_STACK:-}}" >> "{trace_file}"\nexit 7\n',
            encoding="utf-8",
        )
        reg_cmd_script.chmod(0o755)

        def _reg_init(*_args: Any, **_kwargs: Any) -> Any:
            stack = os.environ.get("FUCHSIA_METRICS_CALL_STACK", "")
            with trace_file.open("a", encoding="utf-8") as f:
                f.write(f"init:{stack}\n")
            return cfg

        def _reg_start(*_args: Any, **_kwargs: Any) -> None:
            stack = os.environ.get("FUCHSIA_METRICS_CALL_STACK", "")
            with trace_file.open("a", encoding="utf-8") as f:
                f.write(f"start:{stack}\n")

        def _reg_finish(*args: Any, **kwargs: Any) -> None:
            exit_code = kwargs.get("exit_status", args[5])
            with trace_file.open("a", encoding="utf-8") as f:
                f.write(f"finish:{exit_code}\n")

        mock_init.side_effect = _reg_init
        mock_start.side_effect = _reg_start
        mock_finish.side_effect = _reg_finish
        mock_init.reset_mock()
        mock_start.reset_mock()
        mock_finish.reset_mock()

        stdout_buf = io.StringIO()
        stderr_reg = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FUCHSIA_METRICS_CALL_STACK": "caller"}, clear=False
        ):
            rc_reg = fx_main.main(
                ["my-reg-cmd", "--flag", "arg1"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_reg,
            )

        self.assertEqual(rc_reg, 7)
        self.assertEqual(mock_legacy.call_count, 0)
        self.assertSequenceEqual(
            trace_file.read_text(encoding="utf-8").splitlines(),
            (
                "init:caller",
                "start:caller fx",
                "execute:caller fx",
                "finish:7",
            ),
        )
        self.assertEqual(
            _bound_arg(mock_start, 1, "command_name"), "my-reg-cmd"
        )
        self.assertSequenceEqual(
            _bound_arg(mock_start, 2, "command_args"), ("--flag", "arg1")
        )
        self.assertSequenceEqual(
            _bound_arg(mock_finish, 2, "command_args"), ("--flag", "arg1")
        )
        self.assertEqual(_bound_arg(mock_finish, 5, "exit_status"), 7)

    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_finish_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_start_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics, "init_command_metrics", autospec=True, spec_set=True
    )
    @mock.patch.object(fx_main, "subprocess", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "shutil", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_iterative_mode_starts_metrics_once_without_finish(
        self,
        mock_legacy: mock.MagicMock,
        mock_shutil: mock.MagicMock,
        mock_subprocess: mock.MagicMock,
        mock_init: mock.MagicMock,
        mock_start: mock.MagicMock,
        mock_finish: mock.MagicMock,
    ) -> None:
        r_fd, w_fd = os.pipe()
        self.addCleanup(lambda: os.close(r_fd))
        os.write(w_fd, b"src/ MODIFY foo.cc\n")
        os.close(w_fd)

        fake_proc = mock.create_autospec(subprocess.Popen, instance=True)
        fake_proc.stdout = mock.create_autospec(
            io.BufferedReader, instance=True
        )
        fake_proc.stdout.fileno.return_value = r_fd
        mock_shutil.which.return_value = "/usr/bin/inotifywait"
        mock_subprocess.Popen.return_value = fake_proc
        mock_subprocess.PIPE = subprocess.PIPE
        mock_subprocess.TimeoutExpired = subprocess.TimeoutExpired

        cfg = fx_main.metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="test-uuid",
            debug_log_file="",
            other_uuid="",
            is_internal=False,
        )
        mock_init.return_value = cfg

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FUCHSIA_METRICS_CALL_STACK": ""}, clear=False
        ):
            rc = fx_main.main(
                ["-i", "get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )

        self.assertEqual(rc, 0)
        self.assertEqual(mock_legacy.call_count, 0)
        out_lines = stdout_buf.getvalue().splitlines()
        self.assertEqual(out_lines.count(str(self.fuchsia_dir)), 2)
        self.assertEqual(mock_init.call_count, 1)
        self.assertEqual(mock_start.call_count, 1)
        self.assertEqual(mock_finish.call_count, 0)

    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_finish_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_start_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics, "init_command_metrics", autospec=True, spec_set=True
    )
    @mock.patch.object(
        fx_main.builder,
        "ensure_registered_host_tool",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_pre_dispatch_failures_emit_no_metrics(
        self,
        mock_legacy: mock.MagicMock,
        mock_ensure_tool: mock.MagicMock,
        mock_init: mock.MagicMock,
        mock_start: mock.MagicMock,
        mock_finish: mock.MagicMock,
    ) -> None:
        reg_path = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )

        # 1. RegistryError -> exit code 2, no metrics initialized or emitted
        reg_path.write_text("{malformed-json", encoding="utf-8")
        stderr_reg = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FUCHSIA_METRICS_CALL_STACK": "caller"}, clear=False
        ):
            rc_reg_err = fx_main.main(
                ["my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=stderr_reg,
            )
            self.assertEqual(
                os.environ.get("FUCHSIA_METRICS_CALL_STACK"), "caller"
            )
        self.assertEqual(rc_reg_err, 2)

        # 2. HostToolBuildError -> exit code 1, no metrics initialized or emitted
        reg_path.write_text(
            json.dumps(
                {
                    "my-target-cmd": {
                        "category": "Build",
                        "summary": "target tool",
                        "target": "//tools/foo:bar",
                    }
                }
            ),
            encoding="utf-8",
        )
        mock_ensure_tool.side_effect = fx_main.builder.HostToolBuildError(
            "build failed"
        )
        stderr_build = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FUCHSIA_METRICS_CALL_STACK": "caller"}, clear=False
        ):
            rc_build_err = fx_main.main(
                ["my-target-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=stderr_build,
            )
            self.assertEqual(
                os.environ.get("FUCHSIA_METRICS_CALL_STACK"), "caller"
            )
        self.assertEqual(rc_build_err, 1)

        # 3. Direct _dispatch_direct_command never invokes before_execute on
        # pre-dispatch failures or legacy fallback (None).
        callback_calls: list[str] = []

        def _record_before() -> None:
            callback_calls.append("called")

        rc_direct_err = fx_main._dispatch_direct_command(
            self.fuchsia_dir,
            "my-target-cmd",
            (),
            before_execute=_record_before,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertEqual(rc_direct_err, 1)
        rc_direct_none = fx_main._dispatch_direct_command(
            self.fuchsia_dir,
            "unregistered-legacy-cmd",
            (),
            before_execute=_record_before,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertIsNone(rc_direct_none)
        self.assertSequenceEqual(callback_calls, ())

        # 4. Agent sandbox block -> exit code 1, no metrics emitted
        status_file = self.fuchsia_dir / "proc_status_block"
        status_file.write_text("Name:\tfx\nSeccomp:\t2\n", encoding="utf-8")
        with mock.patch.dict(
            os.environ,
            {
                "ANTIGRAVITY_AGENT": "1",
                "_FX_PROC_STATUS": str(status_file),
                "FUCHSIA_METRICS_CALL_STACK": "",
            },
            clear=False,
        ):
            rc_sandbox = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
        self.assertEqual(rc_sandbox, 1)

        self.assertEqual(mock_legacy.call_count, 0)
        self.assertEqual(mock_init.call_count, 0)
        self.assertEqual(mock_start.call_count, 0)
        self.assertEqual(mock_finish.call_count, 0)

    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_finish_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_start_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics, "init_command_metrics", autospec=True, spec_set=True
    )
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_help_and_legacy_fallback_emit_no_metrics(
        self,
        mock_legacy: mock.MagicMock,
        mock_init: mock.MagicMock,
        mock_start: mock.MagicMock,
        mock_finish: mock.MagicMock,
    ) -> None:
        mock_legacy.return_value = 0

        rc_unknown_help = fx_main.main(
            ["help", "nonexistent-command-xyz"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        rc_builtin_help = fx_main.main(
            ["help", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        rc_dash_h = fx_main.main(
            ["-h"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )

        legacy_script = self.fuchsia_dir / "tools" / "devshell" / "legacy-cmd"
        legacy_script.write_text("#!/bin/bash\n", encoding="utf-8")
        legacy_script.chmod(0o755)
        with mock.patch.dict(
            os.environ, {"FUCHSIA_METRICS_CALL_STACK": "caller"}, clear=False
        ):
            rc_legacy = fx_main.main(
                ["legacy-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
            self.assertEqual(
                os.environ.get("FUCHSIA_METRICS_CALL_STACK"), "caller"
            )

        self.assertEqual(rc_unknown_help, 0)
        self.assertEqual(rc_builtin_help, 0)
        self.assertEqual(rc_dash_h, 0)
        self.assertEqual(rc_legacy, 0)
        self.assertEqual(mock_init.call_count, 0)
        self.assertEqual(mock_start.call_count, 0)
        self.assertEqual(mock_finish.call_count, 0)

    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_help_unknown_and_legacy_dispatched_directly(
        self, mock_legacy: mock.MagicMock
    ) -> None:
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_main.main(
            ["help", "nonexistent-command-xyz"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        self.assertEqual(mock_legacy.call_count, 0)
        self.assertIn(
            "Command nonexistent-command-xyz not found. Try `fx help`",
            stdout_buf.getvalue(),
        )
        self.assertEqual(stderr_buf.getvalue(), "")

    def test_agent_sandbox_check_blocks_sandboxed_execution(self) -> None:
        status_file = self.fuchsia_dir / "proc_status"
        status_file.write_text("Name:\tfx\nSeccomp:\t2\n", encoding="utf-8")

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()

        with mock.patch.dict(
            os.environ,
            {
                "ANTIGRAVITY_AGENT": "1",
                "_FX_PROC_STATUS": str(status_file),
            },
            clear=False,
        ):
            # Non-help command is blocked
            rc = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
            self.assertEqual(rc, 1)
            self.assertIn("BypassSandbox: true", stderr_buf.getvalue())

            # help command is allowed and handled directly in Python
            stdout_buf_help = io.StringIO()
            stderr_buf_help = io.StringIO()
            with mock.patch.object(
                fx_main, "exec_legacy_fx", autospec=True, return_value=0
            ) as mock_legacy:
                rc_help = fx_main.main(
                    ["help"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=stdout_buf_help,
                    stderr=stderr_buf_help,
                )
            self.assertEqual(rc_help, 0)
            self.assertEqual(mock_legacy.call_count, 0)
            self.assertIn(
                "usage: fx [--dir BUILD_DIR]", stdout_buf_help.getvalue()
            )
            self.assertNotIn("BypassSandbox: true", stderr_buf_help.getvalue())

    def _setup_scripts_fx_sandbox(
        self,
        *,
        vars_sh_script: str,
        py3_script: str,
    ) -> pathlib.Path:
        real_fx = _locate_scripts_fx()
        self.assertIsNotNone(
            real_fx, "Could not locate //scripts/fx in test environment"
        )
        assert real_fx is not None
        scripts_dir = self.fuchsia_dir / "scripts"
        scripts_dir.mkdir(parents=True, exist_ok=True)
        fx_copy = scripts_dir / "fx"
        shutil.copy2(real_fx, fx_copy)
        fx_copy.chmod(0o755)

        vars_sh = self.fuchsia_dir / "tools" / "devshell" / "lib" / "vars.sh"
        vars_sh.parent.mkdir(parents=True, exist_ok=True)
        vars_sh.write_text(vars_sh_script, encoding="utf-8")

        host_os = "mac" if sys.platform == "darwin" else "linux"
        machine = platform.machine()
        host_cpu = "arm64" if machine in ("aarch64", "arm64") else "x64"
        host_platform = f"{host_os}-{host_cpu}"
        py3_bin = (
            self.fuchsia_dir
            / ("pre" + "built")
            / "third_party"
            / "python3"
            / host_platform
            / "bin"
            / "python3"
        )
        py3_bin.parent.mkdir(parents=True, exist_ok=True)
        py3_bin.write_text(py3_script, encoding="utf-8")
        py3_bin.chmod(0o755)

        (self.fuchsia_dir / ".git").mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fxsync").mkdir(parents=True, exist_ok=True)
        sync_tool = self.fuchsia_dir / "tools" / "devshell" / "sync"
        sync_tool.parent.mkdir(parents=True, exist_ok=True)
        sync_tool.write_text('#!/bin/bash\necho "ran:sync"\n', encoding="utf-8")
        sync_tool.chmod(0o755)

        main_py = self.fuchsia_dir / "tools" / "devshell" / "fx_cli" / "main.py"
        main_py.parent.mkdir(parents=True, exist_ok=True)
        main_py.write_text("# stub main.py\n", encoding="utf-8")
        return fx_copy

    def test_scripts_fx_python_cli_gate(self) -> None:
        fx_copy = self._setup_scripts_fx_sandbox(
            vars_sh_script='echo "ran:fx-legacy:$*"\nexit 0\n',
            py3_script=(
                "#!/bin/bash\n"
                'echo "ran:python-main:$*"\n'
                'echo "PATH=$PATH"\n'
                'echo "PYCACHE=$PYTHONPYCACHEPREFIX"\n'
                'echo "FX_BASH_VER=$(printenv _FX_BASH_VERSION || true)"\n'
                'echo "EXPORTED_BASH_VER=$(printenv BASH_VERSION || true)"\n'
            ),
        )
        scripts_dir = fx_copy.parent

        base_env = os.environ | {"_FX_TEST_FUCHSIA_DIR": str(self.fuchsia_dir)}
        base_env.pop("PYTHONPYCACHEPREFIX", None)
        base_env.pop("FUCHSIA_OUT_DIR", None)
        base_env.pop("BASH_VERSION", None)
        base_env.pop("_FX_BASH_VERSION", None)

        env_off = base_env | {"FX_PYTHON_CLI": "0"}
        res_off = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=env_off,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res_off.returncode, 0, msg=res_off.stderr)
        self.assertIn("ran:fx-legacy:get-src-dir", res_off.stdout)

        env_on = base_env | {"FX_PYTHON_CLI": "1"}
        res_on = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=env_on,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res_on.returncode, 0, msg=res_on.stderr)
        self.assertIn("ran:sync", res_on.stdout)
        self.assertIn(f"PATH={scripts_dir}:", res_on.stdout)
        self.assertIn(
            f"PYCACHE={self.fuchsia_dir}/out/_build_caches/__pycache__",
            res_on.stdout,
        )
        self.assertIn("ran:python-main:-S", res_on.stdout)
        self.assertIn("main.py get-src-dir", res_on.stdout)
        fx_bash_ver_lines = [
            line
            for line in res_on.stdout.splitlines()
            if line.startswith("FX_BASH_VER=")
        ]
        self.assertEqual(len(fx_bash_ver_lines), 1)
        self.assertGreater(len(fx_bash_ver_lines[0]), len("FX_BASH_VER="))
        self.assertIn("EXPORTED_BASH_VER=\n", res_on.stdout)

        # Even with an existing build directory in .fx-build-dir, PYTHONPYCACHEPREFIX
        # stays under $FUCHSIA_OUT_DIR/_build_caches/__pycache__ (independent of build dir).
        from_file_dir = self.fuchsia_dir / "out" / "from_file"
        from_file_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "//out/from_file\n", encoding="utf-8"
        )
        res_ds = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=env_on,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res_ds.returncode, 0, msg=res_ds.stderr)
        self.assertIn(
            f"PYCACHE={self.fuchsia_dir}/out/_build_caches/__pycache__",
            res_ds.stdout,
        )

        # Custom FUCHSIA_OUT_DIR places the cache under $FUCHSIA_OUT_DIR/_build_caches.
        custom_out = self.fuchsia_dir / "custom_out"
        res_custom_out = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=env_on | {"FUCHSIA_OUT_DIR": str(custom_out)},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(
            res_custom_out.returncode, 0, msg=res_custom_out.stderr
        )
        self.assertIn(
            f"PYCACHE={custom_out}/_build_caches/__pycache__",
            res_custom_out.stdout,
        )

    def test_scripts_fx_real_python_does_not_create_build_dir_pycache(
        self,
    ) -> None:
        fx_copy = self._setup_scripts_fx_sandbox(
            vars_sh_script="exit 0\n",
            py3_script=f'#!/bin/bash\nexec "{_python_launcher()}" "$@"\n',
        )
        cli_dir = self.fuchsia_dir / "tools" / "devshell" / "fx_cli"
        (cli_dir / "_helper_mod.py").write_text(
            "VALUE = 42\n", encoding="utf-8"
        )
        (cli_dir / "main.py").write_text(
            "import sys\n"
            f"sys.path.insert(0, {str(cli_dir)!r})\n"
            "import _helper_mod\n"
            "print(_helper_mod.VALUE)\n",
            encoding="utf-8",
        )
        # Simulate post-`fx clean` build directory (exists with args.gn only).
        build_dir = self.fuchsia_dir / "out" / "cleaned"
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "args.gn").write_text("", encoding="utf-8")
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "out/cleaned\n", encoding="utf-8"
        )

        run_env = os.environ | {
            "_FX_TEST_FUCHSIA_DIR": str(self.fuchsia_dir),
            "FX_PYTHON_CLI": "1",
        }
        run_env.pop("PYTHONPYCACHEPREFIX", None)
        run_env.pop("PYTHONDONTWRITEBYTECODE", None)
        run_env.pop("FUCHSIA_OUT_DIR", None)

        res = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=run_env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res.returncode, 0, msg=res.stderr)
        self.assertIn("42", res.stdout)
        self.assertFalse((build_dir / "__pycache__").exists())
        self.assertTrue(
            (
                self.fuchsia_dir / "out" / "_build_caches" / "__pycache__"
            ).is_dir()
        )

    def test_scripts_fx_clears_ambient_pythonhome_and_pythonsafepath(
        self,
    ) -> None:
        fx_copy = self._setup_scripts_fx_sandbox(
            vars_sh_script="exit 0\n",
            py3_script=(
                "#!/bin/bash\n"
                'echo "PYTHONHOME=${PYTHONHOME-<UNSET>}"\n'
                'echo "PYTHONSAFEPATH=${PYTHONSAFEPATH-<UNSET>}"\n'
                'echo "PYTHONPATH=${PYTHONPATH-<UNSET>}"\n'
            ),
        )
        run_env = os.environ | {
            "_FX_TEST_FUCHSIA_DIR": str(self.fuchsia_dir),
            "FX_PYTHON_CLI": "1",
            "PYTHONHOME": str(self.fuchsia_dir / "nonexistent/prebuilt_python"),
            "PYTHONSAFEPATH": "1",
            "PYTHONPATH": "/caller/pythonpath",
        }

        res = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=run_env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res.returncode, 0, msg=res.stderr)
        self.assertIn("PYTHONHOME=<UNSET>", res.stdout)
        self.assertIn("PYTHONSAFEPATH=<UNSET>", res.stdout)
        self.assertIn("PYTHONPATH=/caller/pythonpath", res.stdout)

        # Staged /_fx_host_tools/ PYTHONHOME clears PYTHONPATH as well.
        staged_home_env = run_env | {
            "PYTHONHOME": str(
                self.fuchsia_dir / "out/_fx_host_tools/foo/runtime"
            ),
            "PYTHONPATH": "/caller/pythonpath",
        }
        res_staged_home = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=staged_home_env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(
            res_staged_home.returncode, 0, msg=res_staged_home.stderr
        )
        self.assertIn("PYTHONHOME=<UNSET>", res_staged_home.stdout)
        self.assertIn("PYTHONSAFEPATH=<UNSET>", res_staged_home.stdout)
        self.assertIn("PYTHONPATH=<UNSET>", res_staged_home.stdout)

        # Staged /_fx_host_tools/ PYTHONPATH is cleared even when PYTHONHOME is unset.
        staged_path_env = run_env | {
            "PYTHONPATH": str(
                self.fuchsia_dir / "out/_fx_host_tools/foo/runtime/lib"
            ),
        }
        staged_path_env.pop("PYTHONHOME", None)
        res_staged_path = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=staged_path_env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(
            res_staged_path.returncode, 0, msg=res_staged_path.stderr
        )
        self.assertIn("PYTHONHOME=<UNSET>", res_staged_path.stdout)
        self.assertIn("PYTHONSAFEPATH=<UNSET>", res_staged_path.stdout)
        self.assertIn("PYTHONPATH=<UNSET>", res_staged_path.stdout)

        # Also verify _apply_global_env clears inherited PYTHONHOME and
        # PYTHONSAFEPATH when main.py is invoked directly.
        parsed = fx_main.parse_global_args(["get-src-dir"])
        with mock.patch.dict(
            os.environ,
            {
                "PYTHONHOME": "/nonexistent/python",
                "PYTHONSAFEPATH": "1",
            },
            clear=True,
        ):
            fx_main._apply_global_env(self.fuchsia_dir, parsed, ["get-src-dir"])
            self.assertNotIn("PYTHONHOME", os.environ)
            self.assertNotIn("PYTHONSAFEPATH", os.environ)
            self.assertEqual(
                os.environ.get("PYTHONPATH"), str(self.fuchsia_dir)
            )

    def test_scripts_fx_legacy_fallback_sync_and_pycache(
        self,
    ) -> None:
        fx_copy = self._setup_scripts_fx_sandbox(
            vars_sh_script=(
                'echo "LEGACY_PYCACHE=${PYTHONPYCACHEPREFIX-<unset>}"\n'
                'echo "LEGACY_START=${start_time}"\n'
                'echo "ran:fx-legacy:$*"\n'
                "exit 0\n"
            ),
            py3_script=(
                "#!/bin/bash\n"
                'echo "PY3_ARG1=$1"\n'
                'echo "PY3_ARG2=$2"\n'
                'echo "PY3_SYNTH=${_FX_SYNTHESIZED_PYCACHE-<unset>}"\n'
                'echo "PY3_PYCACHE=${PYTHONPYCACHEPREFIX-}"\n'
                'echo "PY3_START=${_FX_START_TIME_MICROS-}"\n'
                'if [[ "${3:-}" == "unregistered-cmd" ]]; then\n'
                "  export _FX_LEGACY_FALLBACK=1\n"
                "  shift 2\n"
                '  exec "${_FX_TEST_FUCHSIA_DIR}/scripts/fx" "$@"\n'
                "fi\n"
            ),
        )

        base_env = os.environ | {"_FX_TEST_FUCHSIA_DIR": str(self.fuchsia_dir)}
        for var in (
            "PYTHONPYCACHEPREFIX",
            "FUCHSIA_OUT_DIR",
            "_FX_SYNTHESIZED_PYCACHE",
            "_FX_LEGACY_FALLBACK",
            "_FX_START_TIME_MICROS",
        ):
            base_env.pop(var, None)

        expected_default_pycache = (
            f"{self.fuchsia_dir}/out/_build_caches/__pycache__"
        )

        # Case A: legacy fallback runs sync once, sets shared _build_caches
        # pycache without _FX_SYNTHESIZED_PYCACHE, and forwards start_time.
        env_a = base_env | {"FX_PYTHON_CLI": "1"}
        res_a = subprocess.run(
            [str(fx_copy), "unregistered-cmd"],
            cwd=str(self.fuchsia_dir),
            env=env_a,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res_a.returncode, 0, msg=res_a.stderr)
        lines_a = res_a.stdout.splitlines()
        self.assertEqual(res_a.stdout.count("ran:sync"), 1, res_a.stdout)
        self.assertIn("PY3_ARG1=-S", lines_a)
        self.assertTrue(
            any(
                line.startswith("PY3_ARG2=")
                and line.endswith("/tools/devshell/fx_cli/main.py")
                for line in lines_a
            ),
            lines_a,
        )
        self.assertIn("PY3_SYNTH=<unset>", lines_a)
        self.assertIn(f"PY3_PYCACHE={expected_default_pycache}", lines_a)
        self.assertIn(f"LEGACY_PYCACHE={expected_default_pycache}", lines_a)
        py3_start = next(
            (
                line.split("=", 1)[1]
                for line in lines_a
                if line.startswith("PY3_START=")
            ),
            "",
        )
        legacy_start = next(
            (
                line.split("=", 1)[1]
                for line in lines_a
                if line.startswith("LEGACY_START=")
            ),
            "",
        )
        self.assertTrue(bool(legacy_start), lines_a)
        self.assertEqual(py3_start, legacy_start)

        # Case B: caller-supplied PYTHONPYCACHEPREFIX is preserved.
        env_b = base_env | {
            "FX_PYTHON_CLI": "1",
            "PYTHONPYCACHEPREFIX": "/caller/custom/pycache",
        }
        res_b = subprocess.run(
            [str(fx_copy), "get-src-dir"],
            cwd=str(self.fuchsia_dir),
            env=env_b,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(res_b.returncode, 0, msg=res_b.stderr)
        lines_b = res_b.stdout.splitlines()
        self.assertIn("PY3_PYCACHE=/caller/custom/pycache", lines_b)
        self.assertIn("PY3_SYNTH=<unset>", lines_b)

    def test_apply_global_env_clears_ambient_fuchsia_build_dir_on_dir_flag(
        self,
    ) -> None:
        parsed = fx_main.parse_global_args(["--dir", "out/new_dir", "set"])
        with mock.patch.dict(
            os.environ,
            {
                "FUCHSIA_BUILD_DIR": "/stale/out/old_dir",
                "PYTHONPYCACHEPREFIX": "",
                "DEBUGINFOD_URLS": "",
            },
            clear=False,
        ):
            fx_main._apply_global_env(
                self.fuchsia_dir, parsed, ["--dir", "out/new_dir", "set"]
            )
            self.assertNotIn("FUCHSIA_BUILD_DIR", os.environ)
            self.assertEqual(os.environ.get("_FX_BUILD_DIR"), "out/new_dir")
            self.assertEqual(
                os.environ.get("FUCHSIA_BUILD_DIR_FROM_FX"), "out/new_dir"
            )
            self.assertEqual(
                os.environ.get("FX_CACHE_DIR"), str(self.fuchsia_dir / ".fx")
            )
            self.assertIn(
                "https://storage.googleapis.com/fuchsia-artifacts",
                os.environ.get("DEBUGINFOD_URLS", ""),
            )
            self.assertTrue(
                os.environ.get("PYTHONPYCACHEPREFIX", "").endswith(
                    "/__pycache__"
                )
            )

    def test_vendor_command_registry_lookup_log_format(self) -> None:
        vendor_reg_dir = (
            self.fuchsia_dir / "vendor" / "acme" / "scripts" / "devshell"
        )
        vendor_reg_dir.mkdir(parents=True, exist_ok=True)
        (vendor_reg_dir / "fx_host_tools.json").write_text(
            json.dumps(
                {
                    "acme-tool": {
                        "category": "Other",
                        "summary": "acme vendor tool",
                        "script": "//vendor/acme/tools/acme_tool.sh",
                    },
                }
            ),
            encoding="utf-8",
        )
        vendor_script = (
            self.fuchsia_dir / "vendor" / "acme" / "tools" / "acme_tool.sh"
        )
        vendor_script.parent.mkdir(parents=True, exist_ok=True)
        vendor_script.write_text(
            '#!/bin/bash\necho "acme-tool:$*"\n', encoding="utf-8"
        )
        vendor_script.chmod(0o755)

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ, {"FX_OUTPUT_REGISTRY_ACTIONS": "1"}, clear=False
        ):
            rc = fx_main._dispatch_direct_command(
                self.fuchsia_dir,
                "vendor",
                ["acme", "acme-tool", "--arg"],
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
        self.assertEqual(rc, 0)
        self.assertIn(
            "[fx-registry] lookup 'vendor acme acme-tool': found in vendor 'acme' registry (mode=script, script=//vendor/acme/tools/acme_tool.sh)",
            stderr_buf.getvalue(),
        )

    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_trace_flags_dispatch_directly_without_legacy_fallback(
        self, mock_legacy: mock.MagicMock
    ) -> None:
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("FUCHSIA_DEVSHELL_VERBOSITY", None)
            rc_x = fx_main.main(
                ["-x", "get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
            self.assertEqual(os.environ.get("FUCHSIA_DEVSHELL_VERBOSITY"), "1")
            self.assertEqual(os.environ.get("FX_ENABLE_SHELL_TRACING"), "true")
            rc_xx = fx_main.main(
                ["-xx", "get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
            self.assertEqual(rc_x, 0)
            self.assertEqual(rc_xx, 0)
            self.assertEqual(mock_legacy.call_count, 0)

            # When falling back to fx-legacy, -x must not pre-set FUCHSIA_DEVSHELL_VERBOSITY=1
            os.environ.pop("FUCHSIA_DEVSHELL_VERBOSITY", None)
            mock_legacy.return_value = 0
            fx_main.main(
                ["-x", "unmigrated-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=stdout_buf,
                stderr=stderr_buf,
            )
            self.assertNotIn("FUCHSIA_DEVSHELL_VERBOSITY", os.environ)

    @mock.patch.object(fx_main, "shutil", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_iterative_mode_missing_inotifywait(
        self, mock_legacy: mock.MagicMock, mock_shutil: mock.MagicMock
    ) -> None:
        mock_shutil.which.return_value = None
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_main.main(
            ["-i", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 1)
        self.assertEqual(mock_legacy.call_count, 0)
        self.assertEqual(os.environ.get("FUCHSIA_FX_ITERATIVE"), "1")
        out_text = stdout_buf.getvalue()
        self.assertIn(str(self.fuchsia_dir), out_text)
        self.assertIn("Missing inotifywait", out_text)
        self.assertIn("Try: sudo apt-get install inotify-tools", out_text)

    @mock.patch.object(fx_main, "subprocess", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "shutil", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_iterative_mode_batches_events_and_reruns_command(
        self,
        mock_legacy: mock.MagicMock,
        mock_shutil: mock.MagicMock,
        mock_subprocess: mock.MagicMock,
    ) -> None:
        r_fd, w_fd = os.pipe()
        self.addCleanup(lambda: os.close(r_fd))
        os.write(w_fd, b"src/ MODIFY foo.cc\nsrc/ MODIFY bar.cc\n")
        os.close(w_fd)

        fake_proc = mock.create_autospec(subprocess.Popen, instance=True)
        fake_proc.stdout = mock.create_autospec(
            io.BufferedReader, instance=True
        )
        fake_proc.stdout.fileno.return_value = r_fd
        mock_shutil.which.return_value = "/usr/bin/inotifywait"
        mock_subprocess.Popen.return_value = fake_proc
        mock_subprocess.PIPE = subprocess.PIPE
        mock_subprocess.TimeoutExpired = subprocess.TimeoutExpired

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_main.main(
            ["-i", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        self.assertEqual(mock_legacy.call_count, 0)
        mock_subprocess.Popen.assert_called_once()
        popen_args = mock_subprocess.Popen.call_args.args[0]
        self.assertSequenceEqual(
            popen_args[:4],
            ("inotifywait", "-qrme", "modify", "--exclude"),
        )
        out_lines = stdout_buf.getvalue().splitlines()
        self.assertEqual(
            out_lines.count(str(self.fuchsia_dir)),
            2,
            f"Expected initial run + 1 batched rerun, got: {out_lines}",
        )
        self.assertTrue(any("fx -i get-src-dir" in line for line in out_lines))
        self.assertTrue(any("--- Done in " in line for line in out_lines))

    def test_forwards_sigterm_to_child_process(self) -> None:
        state_file = self.fuchsia_dir / "sig_state.txt"
        sig_script = self.fuchsia_dir / "tools" / "devshell" / "sig_tool.sh"
        sig_script.write_text(
            "#!/bin/bash\n"
            f"trap 'echo GOT_SIGTERM >> \"{state_file}\"; exit 143' TERM\n"
            f'echo "CHILD_PID=$$" > "{state_file}"\n'
            "while true; do sleep 0.05; done\n",
            encoding="utf-8",
        )
        sig_script.chmod(0o755)

        reg_path = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )
        data = json.loads(reg_path.read_text(encoding="utf-8"))
        data["sig-tool"] = {
            "category": "Other",
            "summary": "signal test tool",
            "script": "//tools/devshell/sig_tool.sh",
        }
        reg_path.write_text(json.dumps(data), encoding="utf-8")

        main_py_path = pathlib.Path(fx_main.__file__).resolve()
        if main_py_path.is_file():
            pypath_entry = str(main_py_path.parent)
            cmd_argv = [sys.executable, str(main_py_path), "sig-tool"]
        else:
            archive_path = next(p for p in main_py_path.parents if p.is_file())
            pypath_entry = str(archive_path)
            cmd_argv = [
                sys.executable,
                "-c",
                "import sys, main; sys.exit(main.main(sys.argv[1:]))",
                "sig-tool",
            ]
        existing_pypath = os.environ.get("PYTHONPATH", "")
        new_pypath = (
            f"{pypath_entry}:{existing_pypath}"
            if existing_pypath
            else pypath_entry
        )
        env = os.environ | {
            "_FX_TEST_FUCHSIA_DIR": str(self.fuchsia_dir),
            "_FX_PROC_STATUS": str(self.fuchsia_dir / "nonexistent"),
            "PYTHONPATH": new_pypath,
        }
        env.pop("PYTHONSAFEPATH", None)
        proc = subprocess.Popen(
            cmd_argv,
            cwd=str(self.fuchsia_dir),
            env=env,
        )
        child_pid: int | None = None
        try:
            deadline = time.monotonic() + 5.0
            while time.monotonic() < deadline:
                if state_file.is_file():
                    text = state_file.read_text(encoding="utf-8")
                    if "CHILD_PID=" in text:
                        first_line = text.splitlines()[0]
                        child_pid = int(first_line.split("=", 1)[1])
                        break
                time.sleep(0.02)
            self.assertIsNotNone(child_pid, "sig-tool did not start in time")

            # Send SIGTERM only to main.py PID (not process group)
            proc.send_signal(signal.SIGTERM)
            proc.wait(timeout=5.0)

            deadline = time.monotonic() + 2.0
            got_sigterm = False
            while time.monotonic() < deadline:
                if "GOT_SIGTERM" in state_file.read_text(encoding="utf-8"):
                    got_sigterm = True
                    break
                time.sleep(0.02)
            self.assertTrue(
                got_sigterm,
                "Expected main.py to forward SIGTERM to child sig-tool process",
            )
            self.assertEqual(proc.returncode, 143)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            if child_pid is not None:
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_parse_global_args_uses_runtime_fuchsia_dir_optional_features(
        self,
    ) -> None:
        lib_dir = self.fuchsia_dir / "tools" / "devshell" / "lib"
        lib_dir.mkdir(parents=True, exist_ok=True)
        (lib_dir / "fx-optional-features.sh").write_text(
            "_FX_OPTIONAL_FEATURES_SPEC=(\n"
            '  "runtime_only_feat|disabled|Runtime feature"\n'
            ")\n",
            encoding="utf-8",
        )
        parsed = fx_main.parse_global_args(
            ["--enable=runtime_only_feat", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
        )
        fx_main._apply_global_env(
            self.fuchsia_dir,
            parsed,
            ["--enable=runtime_only_feat", "get-src-dir"],
        )
        self.assertEqual(
            os.environ.get("FUCHSIA_DISABLED_runtime_only_feat"), "0"
        )
        with self.assertRaises(fx_main.GlobalArgError):
            fx_main.parse_global_args(
                ["--enable=incremental", "get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
            )

    def test_is_invoked_by_agent_empty_var_and_custom_agents_txt(self) -> None:
        lib_dir = self.fuchsia_dir / "tools" / "devshell" / "lib"
        lib_dir.mkdir(parents=True, exist_ok=True)
        (lib_dir / "agents.txt").write_text(
            "ANTIGRAVITY_AGENT\nCUSTOM_RUNTIME_AGENT\n", encoding="utf-8"
        )
        (lib_dir / "agent_session_ids.txt").write_text("", encoding="utf-8")

        with mock.patch.dict(os.environ, {"ANTIGRAVITY_AGENT": ""}, clear=True):
            self.assertFalse(fx_main._is_invoked_by_agent(self.fuchsia_dir))

        with mock.patch.dict(
            os.environ, {"CUSTOM_RUNTIME_AGENT": "1"}, clear=True
        ):
            self.assertTrue(fx_main._is_invoked_by_agent(self.fuchsia_dir))

    def test_apply_global_env_double_slash_build_dir_pycache(self) -> None:
        from_file = self.fuchsia_dir / "out" / "from_file"
        from_file.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "//out/from_file\n", encoding="utf-8"
        )
        expected_pycache = f"{self.fuchsia_dir}/out/_build_caches/__pycache__"
        os.environ.pop("PYTHONPYCACHEPREFIX", None)
        os.environ.pop("FUCHSIA_OUT_DIR", None)
        parsed = fx_main.parse_global_args(
            ["get-src-dir"], fuchsia_dir=self.fuchsia_dir
        )
        fx_main._apply_global_env(self.fuchsia_dir, parsed, ["get-src-dir"])
        self.assertEqual(
            os.environ.get("PYTHONPYCACHEPREFIX"), expected_pycache
        )

        custom_dir = self.fuchsia_dir / "out" / "custom_ds"
        custom_dir.mkdir(parents=True, exist_ok=True)
        os.environ.pop("PYTHONPYCACHEPREFIX", None)
        os.environ.pop("FUCHSIA_OUT_DIR", None)
        parsed2 = fx_main.parse_global_args(
            ["--dir", "///out/custom_ds///", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
        )
        fx_main._apply_global_env(
            self.fuchsia_dir,
            parsed2,
            ["--dir", "///out/custom_ds///", "get-src-dir"],
        )
        self.assertEqual(os.environ.get("_FX_BUILD_DIR"), str(custom_dir))
        self.assertEqual(
            os.environ.get("PYTHONPYCACHEPREFIX"), expected_pycache
        )

        # Custom FUCHSIA_OUT_DIR is honored for PYTHONPYCACHEPREFIX.
        custom_out = self.fuchsia_dir / "custom_out_env"
        os.environ.pop("PYTHONPYCACHEPREFIX", None)
        os.environ["FUCHSIA_OUT_DIR"] = str(custom_out)
        fx_main._apply_global_env(self.fuchsia_dir, parsed, ["get-src-dir"])
        self.assertEqual(
            os.environ.get("PYTHONPYCACHEPREFIX"),
            f"{custom_out}/_build_caches/__pycache__",
        )

        # Empty FUCHSIA_OUT_DIR falls back to $FUCHSIA_DIR/out (matching vars.sh).
        os.environ.pop("PYTHONPYCACHEPREFIX", None)
        os.environ["FUCHSIA_OUT_DIR"] = ""
        fx_main._apply_global_env(self.fuchsia_dir, parsed, ["get-src-dir"])
        self.assertEqual(
            os.environ.get("FUCHSIA_OUT_DIR"), str(self.fuchsia_dir / "out")
        )
        self.assertEqual(
            os.environ.get("PYTHONPYCACHEPREFIX"), expected_pycache
        )

    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "_run_iterative", autospec=True, spec_set=True)
    @mock.patch.object(
        fx_main, "_dispatch_direct_command", autospec=True, spec_set=True
    )
    def test_iterative_mode_ctrl_c_initial_and_help(
        self,
        mock_dispatch: mock.MagicMock,
        mock_iterative: mock.MagicMock,
        mock_legacy: mock.MagicMock,
    ) -> None:
        mock_dispatch.return_value = 130
        rc = fx_main.main(
            ["-i", "my-reg-cmd"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertEqual(rc, 130)
        self.assertEqual(mock_iterative.call_count, 0)
        self.assertEqual(mock_legacy.call_count, 0)

        mock_dispatch.return_value = 0
        rc_help = fx_main.main(
            ["-i", "-h", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertEqual(rc_help, 0)
        self.assertEqual(mock_iterative.call_count, 0)

        rc_help2 = fx_main.main(
            ["-i", "help", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertEqual(rc_help2, 0)
        self.assertEqual(mock_iterative.call_count, 0)

    @mock.patch.object(fx_main, "subprocess", autospec=True, spec_set=True)
    @mock.patch.object(fx_main, "shutil", autospec=True, spec_set=True)
    def test_iterative_mode_ctrl_c_during_rerun(
        self,
        mock_shutil: mock.MagicMock,
        mock_subprocess: mock.MagicMock,
    ) -> None:
        r_fd, w_fd = os.pipe()
        self.addCleanup(lambda: os.close(r_fd))
        os.write(w_fd, b"src/ MODIFY foo.cc\n")

        fake_proc = mock.create_autospec(subprocess.Popen, instance=True)
        fake_proc.stdout = mock.create_autospec(
            io.BufferedReader, instance=True
        )
        fake_proc.stdout.fileno.return_value = r_fd
        mock_shutil.which.return_value = "/usr/bin/inotifywait"
        mock_subprocess.Popen.return_value = fake_proc
        mock_subprocess.PIPE = subprocess.PIPE
        mock_subprocess.TimeoutExpired = subprocess.TimeoutExpired

        try:
            rc = fx_main._run_iterative(
                self.fuchsia_dir,
                "cmd",
                lambda: 130,
                stdout=io.StringIO(),
            )
        finally:
            os.close(w_fd)
        self.assertEqual(rc, 130)

    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_vendor_empty_name_falls_back_to_legacy(
        self, mock_legacy: mock.MagicMock
    ) -> None:
        mock_legacy.return_value = 1
        rc = fx_main.main(
            ["vendor", "", "get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=io.StringIO(),
            stderr=io.StringIO(),
        )
        self.assertEqual(rc, 1)
        self.assertEqual(mock_legacy.call_count, 1)

    def test_registered_python_subcommand_no_dash_s_and_pythonpath(
        self,
    ) -> None:
        (self.fuchsia_dir / "fx_subcmd_helper.py").write_text(
            'VAL = "ok_from_tree"\n', encoding="utf-8"
        )
        py_cmd = self.fuchsia_dir / "tools" / "my_py_cmd.py"
        py_cmd.write_text(
            "import sys\n"
            "import fx_subcmd_helper\n"
            'print(f"NO_SITE={sys.flags.no_site}:{fx_subcmd_helper.VAL}")\n',
            encoding="utf-8",
        )
        reg_path = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )
        reg_data = json.loads(reg_path.read_text(encoding="utf-8"))
        reg_data["my-py-cmd"] = {
            "category": "Other",
            "summary": "registered python cmd",
            "script": "//tools/my_py_cmd.py",
        }
        reg_path.write_text(json.dumps(reg_data), encoding="utf-8")

        out_buf = io.StringIO()
        err_buf = io.StringIO()
        rc = fx_main.main(
            ["my-py-cmd"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=out_buf,
            stderr=err_buf,
        )
        self.assertEqual(rc, 0, msg=err_buf.getvalue())
        self.assertIn("NO_SITE=0:ok_from_tree", out_buf.getvalue())

    def test_macos_platform_blocked_for_non_help_and_allowed_for_help(
        self,
    ) -> None:
        out_buf = io.StringIO()
        err_buf = io.StringIO()
        with mock.patch.object(fx_main.sys, "platform", "darwin"):
            rc_blocked = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=out_buf,
                stderr=err_buf,
            )
            self.assertEqual(rc_blocked, 1)
            self.assertIn(
                "This tool is no longer supported on macOS per go/fuchsia-on-mac.",
                err_buf.getvalue(),
            )

            out_help = io.StringIO()
            err_help = io.StringIO()
            with mock.patch.object(
                fx_main, "exec_legacy_fx", autospec=True, return_value=0
            ):
                rc_help = fx_main.main(
                    ["help"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=out_help,
                    stderr=err_help,
                )
            self.assertEqual(rc_help, 0)

    @mock.patch.object(fx_main, "exec_legacy_fx", autospec=True, spec_set=True)
    def test_legacy_fallback_restores_env_no_fx_caller_no_duplicate_path(
        self, mock_legacy: mock.MagicMock
    ) -> None:
        scripts_dir = str(self.fuchsia_dir / "scripts")
        with mock.patch.dict(
            os.environ,
            {
                "PATH": f"{scripts_dir}:/usr/bin",
                "FUCHSIA_DEVICE_ADDR": "192.0.2.1",
            },
            clear=True,
        ):
            rc_direct = fx_main.main(
                ["get-src-dir"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
            self.assertEqual(rc_direct, 0)
            self.assertNotIn("FX_CALLER", os.environ)
            self.assertEqual(
                os.environ["PATH"].split(":").count(scripts_dir), 1
            )

        with mock.patch.dict(
            os.environ,
            {
                "PATH": f"{scripts_dir}:/usr/bin",
                "FUCHSIA_DEVICE_ADDR": "192.0.2.1",
            },
            clear=True,
        ):
            captured_env: dict[str, str] = {}

            def _capture_env(_fdir: pathlib.Path, _argv: object) -> int:
                captured_env.update(os.environ)
                return 0

            mock_legacy.side_effect = _capture_env
            rc_legacy = fx_main.main(
                ["-t", "my-node", "-xx", "--dir", "out/x", "unmigrated-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
            self.assertEqual(rc_legacy, 0)
            self.assertNotIn("FUCHSIA_NODENAME", captured_env)
            self.assertNotIn("_FX_NODENAME", captured_env)
            self.assertNotIn("FUCHSIA_NODENAME_SET_BY_FX_FLAG", captured_env)
            self.assertNotIn("FUCHSIA_DEVSHELL_VERBOSITY", captured_env)
            self.assertNotIn("_FX_BUILD_DIR", captured_env)
            self.assertEqual(
                captured_env.get("FUCHSIA_DEVICE_ADDR"), "192.0.2.1"
            )
            self.assertEqual(
                captured_env.get("PATH"), f"{scripts_dir}:/usr/bin"
            )

    def test_pycache_prefix_uses_build_caches_and_preserves_custom_override(
        self,
    ) -> None:
        from_file = self.fuchsia_dir / "out" / "from_file"
        other_dir = self.fuchsia_dir / "out" / "other"
        env_dir = self.fuchsia_dir / "out" / "env_dir"
        for bdir in (from_file, other_dir, env_dir):
            bdir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "//out/from_file\n", encoding="utf-8"
        )
        base_env = {"PATH": os.environ.get("PATH", "")}
        expected_pycache = f"{self.fuchsia_dir}/out/_build_caches/__pycache__"

        with mock.patch.dict(os.environ, base_env, clear=True):
            rc = fx_main.main(
                ["--dir", "out/other", "my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
            self.assertEqual(rc, 0)
            self.assertEqual(
                os.environ.get("PYTHONPYCACHEPREFIX"), expected_pycache
            )

        with mock.patch.dict(
            os.environ,
            base_env | {"FUCHSIA_BUILD_DIR": str(env_dir)},
            clear=True,
        ):
            rc = fx_main.main(
                ["my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
            self.assertEqual(rc, 0)
            self.assertEqual(
                os.environ.get("PYTHONPYCACHEPREFIX"), expected_pycache
            )

        captured_env: dict[str, str] = {}

        def _capture_legacy(_fdir: pathlib.Path, _argv: object) -> int:
            captured_env.update(os.environ)
            return 0

        # When scripts/fx sets the shared default PYTHONPYCACHEPREFIX, legacy
        # fallback preserves it verbatim (matching vars.sh).
        with mock.patch.dict(
            os.environ,
            base_env | {"PYTHONPYCACHEPREFIX": expected_pycache},
            clear=True,
        ):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                spec_set=True,
                side_effect=_capture_legacy,
            ):
                rc = fx_main.main(
                    ["--dir", "out/other", "unregistered-cmd"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
            self.assertEqual(rc, 0)
            self.assertEqual(
                captured_env.get("PYTHONPYCACHEPREFIX"), expected_pycache
            )

        # Caller-supplied PYTHONPYCACHEPREFIX is preserved on direct and legacy paths.
        with mock.patch.dict(
            os.environ,
            base_env | {"PYTHONPYCACHEPREFIX": "/caller/custom/pycache"},
            clear=True,
        ):
            rc_direct = fx_main.main(
                ["--dir", "out/other", "my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=io.StringIO(),
                stderr=io.StringIO(),
            )
            self.assertEqual(rc_direct, 0)
            self.assertEqual(
                os.environ.get("PYTHONPYCACHEPREFIX"), "/caller/custom/pycache"
            )

        captured_env.clear()
        with mock.patch.dict(
            os.environ,
            base_env | {"PYTHONPYCACHEPREFIX": "/caller/custom/pycache"},
            clear=True,
        ):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                spec_set=True,
                side_effect=_capture_legacy,
            ):
                rc_legacy = fx_main.main(
                    ["--dir", "out/other", "unregistered-cmd"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
            self.assertEqual(rc_legacy, 0)
            self.assertEqual(
                captured_env.get("PYTHONPYCACHEPREFIX"),
                "/caller/custom/pycache",
            )

    def test_start_time_micros_stripped_on_direct_and_reentry_preserved_on_legacy(
        self,
    ) -> None:
        reg_cmd_script = self.fuchsia_dir / "tools" / "my_reg_cmd.sh"
        reg_cmd_script.write_text(
            '#!/bin/bash\necho "START=${_FX_START_TIME_MICROS-<unset>}"\n',
            encoding="utf-8",
        )
        reg_cmd_script.chmod(0o755)
        base_env = {"PATH": os.environ.get("PATH", "")}

        out_direct = io.StringIO()
        with mock.patch.dict(
            os.environ,
            base_env | {"_FX_START_TIME_MICROS": "1700000000123456"},
            clear=True,
        ):
            rc = fx_main.main(
                ["my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=out_direct,
                stderr=io.StringIO(),
            )
            self.assertEqual(rc, 0)
            self.assertNotIn("_FX_START_TIME_MICROS", os.environ)
            self.assertIn("START=<unset>", out_direct.getvalue().splitlines())

        out_reentry = io.StringIO()
        with mock.patch.dict(
            os.environ,
            base_env
            | {
                "FX_ENTRY_CMD": "parent-cmd",
                "_FX_START_TIME_MICROS": "1700000000123456",
            },
            clear=True,
        ):
            rc = fx_main.main(
                ["fx-reentry", "my-reg-cmd"],
                fuchsia_dir=self.fuchsia_dir,
                stdout=out_reentry,
                stderr=io.StringIO(),
            )
            self.assertEqual(rc, 0)
            self.assertNotIn("_FX_START_TIME_MICROS", os.environ)
            self.assertIn("START=<unset>", out_reentry.getvalue().splitlines())

        captured_env: dict[str, str] = {}

        def _capture_legacy(_fdir: pathlib.Path, _argv: object) -> int:
            captured_env.update(os.environ)
            return 0

        with mock.patch.dict(
            os.environ,
            base_env | {"_FX_START_TIME_MICROS": "1700000000123456"},
            clear=True,
        ):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                spec_set=True,
                side_effect=_capture_legacy,
            ):
                rc = fx_main.main(
                    ["unregistered-cmd"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
            self.assertEqual(rc, 0)
            self.assertEqual(
                captured_env.get("_FX_START_TIME_MICROS"),
                "1700000000123456",
            )

    def test_entrypoint_under_isolated_site_and_safepath_without_pythonpath(
        self,
    ) -> None:
        captured_env: dict[str, str] = {}

        def _capture_legacy(_fdir: pathlib.Path, _argv: object) -> int:
            captured_env.update(os.environ)
            return 0

        with mock.patch.dict(
            os.environ,
            {"PATH": os.environ.get("PATH", "")},
            clear=True,
        ):
            with mock.patch.object(
                fx_main,
                "exec_legacy_fx",
                autospec=True,
                spec_set=True,
                side_effect=_capture_legacy,
            ):
                rc = fx_main.main(
                    ["unregistered-cmd"],
                    fuchsia_dir=self.fuchsia_dir,
                    stdout=io.StringIO(),
                    stderr=io.StringIO(),
                )
        self.assertEqual(rc, 0)
        self.assertNotIn("PYTHONPATH", captured_env)

        main_py_path = pathlib.Path(fx_main.__file__)
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp).resolve()
            fuchsia_dir = tmp_path / "fuchsia"
            fuchsia_dir.mkdir(parents=True, exist_ok=True)
            (fuchsia_dir / ".fx-root").write_text("", encoding="utf-8")
            if main_py_path.is_file():
                script_to_run = main_py_path
            else:
                archive_path = next(
                    (p for p in main_py_path.parents if p.is_file()), None
                )
                if archive_path is None or not zipfile.is_zipfile(archive_path):
                    self.skipTest(
                        f"Could not locate standalone or zipapp main.py: {main_py_path}"
                    )
                extract_dir = tmp_path / "extracted"
                with zipfile.ZipFile(archive_path) as zf:
                    zf.extractall(extract_dir)
                script_to_run = extract_dir / "main.py"
            completed = subprocess.run(
                [sys.executable, "-S", str(script_to_run), "get-src-dir"],
                env=test_utils.isolated_python_env(
                    {
                        "FUCHSIA_DIR": str(fuchsia_dir),
                        "PYTHONSAFEPATH": "1",
                        "PATH": os.environ.get("PATH", ""),
                    }
                ),
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        self.assertEqual(completed.stdout.strip(), str(fuchsia_dir))

    def test_isolated_python_env_preserves_only_compact_stdlib(self) -> None:
        base_env = {"FUCHSIA_DIR": "/tmp/fuchsia", "PYTHONSAFEPATH": "1"}
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            regular_dir = tmp_path / "regular_python" / "bin"
            regular_dir.mkdir(parents=True)
            regular_py = regular_dir / "python3.14"
            regular_py.write_text("", encoding="utf-8")

            compact_dir = tmp_path / "prebuilt_python"
            compact_dir.mkdir()
            compact_py = compact_dir / "python3.14-real"
            compact_py.symlink_to(regular_py)
            stdlib_zip = compact_dir / "lib_python3.14.zip"
            stdlib_zip.write_text("", encoding="utf-8")
            compact_launcher = compact_dir / "python3"
            compact_launcher.write_text("#!/bin/bash\n", encoding="utf-8")

            compact_env = test_utils.isolated_python_env(
                base_env,
                executable=str(compact_py),
                version_info=(3, 14),
            )
            self.assertEqual(compact_env["FUCHSIA_DIR"], "/tmp/fuchsia")
            self.assertEqual(compact_env["PYTHONSAFEPATH"], "1")
            self.assertEqual(compact_env["PYTHONHOME"], str(compact_dir))
            self.assertEqual(compact_env["PYTHONPATH"], str(stdlib_zip))
            self.assertEqual(
                _python_launcher(str(compact_py)), str(compact_launcher)
            )

            regular_env = test_utils.isolated_python_env(
                base_env,
                executable=str(regular_py),
                version_info=(3, 14),
            )
            self.assertEqual(regular_env, base_env)
            self.assertEqual(_python_launcher(str(regular_py)), str(regular_py))

    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_finish_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "spawn_command_start_metrics",
        autospec=True,
        spec_set=True,
    )
    @mock.patch.object(
        fx_main.metrics,
        "init_command_metrics",
        autospec=True,
        spec_set=True,
    )
    def test_metrics_hook_oserror_does_not_abort_fx(
        self,
        mock_init: mock.MagicMock,
        mock_start: mock.MagicMock,
        mock_finish: mock.MagicMock,
    ) -> None:
        mock_init.side_effect = PermissionError("read-only metrics config")
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_main.main(
            ["get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(self.fuchsia_dir))
        self.assertEqual(os.environ.get("FUCHSIA_METRICS_CALL_STACK"), "fx")
        mock_start.assert_not_called()
        mock_finish.assert_not_called()

        cfg = fx_main.metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="u-err",
            debug_log_file="",
            other_uuid="",
            is_internal=False,
        )
        mock_init.side_effect = None
        mock_init.return_value = cfg
        mock_start.side_effect = OSError("spawn start failed")
        mock_finish.side_effect = OSError("spawn finish failed")
        stdout_buf2 = io.StringIO()
        rc2 = fx_main.main(
            ["get-src-dir"],
            fuchsia_dir=self.fuchsia_dir,
            stdout=stdout_buf2,
            stderr=io.StringIO(),
        )
        self.assertEqual(rc2, 0)
        self.assertEqual(stdout_buf2.getvalue().strip(), str(self.fuchsia_dir))
        mock_start.assert_called_once()
        mock_finish.assert_called_once()


if __name__ == "__main__":
    unittest.main()
