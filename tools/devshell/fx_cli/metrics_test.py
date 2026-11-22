# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for fx_cli.metrics."""

import contextlib
import dataclasses
import io
import json
import os
import pathlib
import platform
import re
import stat
import subprocess
import sys
import tempfile
import unittest
import uuid
from collections.abc import Mapping
from unittest import mock

import metadata
import metrics
import test_utils

_GA4_QUERY_FRAGMENT = (
    "\x61\x70\x69\x5f\x73\x65\x63\x72\x65\x74="
    "xjiXbh8eSGiExMthvAXd6w&measurement_id=G-L2YHSDD8ZF"
)


class _TtyStringIO(io.StringIO):
    """StringIO buffer that reports itself as an interactive TTY."""

    def isatty(self) -> bool:
        return True


def _base_env(
    home: pathlib.Path,
    *,
    hostname: str = "dev.example.com",
    xdg_data_home: pathlib.Path | None = None,
    extra: Mapping[str, str] | None = None,
) -> dict[str, str]:
    env_map = {
        "HOME": str(home),
        "HOSTNAME": hostname,
        "PATH": os.environ.get("PATH", ""),
        "USER": "testuser",
    }
    if xdg_data_home is not None:
        env_map["XDG_DATA_HOME"] = str(xdg_data_home)
    if extra is not None:
        env_map |= extra
    return env_map


class MetricsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root_dir = pathlib.Path(self._tmp.name)
        self.fuchsia_dir = self.root_dir / "fuchsia"
        self.fuchsia_dir.mkdir(parents=True, exist_ok=True)
        self.home_dir = self.root_dir / "home"
        self.home_dir.mkdir(parents=True, exist_ok=True)
        self.xdg_dir = self.root_dir / "xdg_data"
        self.xdg_dir.mkdir(parents=True, exist_ok=True)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _internal_dir(self, base: pathlib.Path | None = None) -> pathlib.Path:
        root = base if base is not None else self.xdg_dir
        path = root / "Fuchsia" / "metrics"
        path.mkdir(parents=True, exist_ok=True)
        return path

    def _write_fx_metrics_cfg(
        self, content: str, path: pathlib.Path | None = None
    ) -> pathlib.Path:
        cfg_path = (
            path
            if path is not None
            else self.fuchsia_dir / ".fx" / "config" / "metrics"
        )
        cfg_path.parent.mkdir(parents=True, exist_ok=True)
        cfg_path.write_text(content, encoding="utf-8")
        return cfg_path

    def test_metrics_config_should_report_and_frozen(self) -> None:
        cfg_disabled = metrics.MetricsConfig(
            level=0,
            enabled=0,
            uuid="",
            debug_log_file="",
            other_uuid="",
            is_internal=False,
        )
        self.assertFalse(cfg_disabled.should_report)
        with self.assertRaises(dataclasses.FrozenInstanceError):
            setattr(cfg_disabled, "level", 1)

        self.assertTrue(
            dataclasses.replace(cfg_disabled, level=1, enabled=1).should_report
        )
        self.assertFalse(
            dataclasses.replace(
                cfg_disabled, debug_log_file="/tmp/metrics.log"
            ).should_report
        )
        self.assertFalse(
            dataclasses.replace(
                cfg_disabled,
                level=2,
                enabled=1,
                debug_log_file="/tmp/metrics.log",
                is_mocked=True,
            ).should_report
        )

    def test_metrics_module_decoupled_from_fx_builtins(self) -> None:
        self.assertNotIn("fx_builtins", vars(metrics))
        self.assertNotIn("fx_builtins", sys.modules)

    @mock.patch.object(
        metrics.socket,
        "gethostname",
        autospec=True,
        spec_set=True,
        return_value="cloudtop.c.googlers.com",
    )
    def test_is_internal_user_and_hostname_fallback(
        self, mock_gethostname: mock.MagicMock
    ) -> None:
        self.assertTrue(
            metrics.is_internal_user(env={"HOSTNAME": "host.corp.google.com"})
        )
        self.assertTrue(
            metrics.is_internal_user(env={"HOSTNAME": "host.c.googlers.com"})
        )
        self.assertFalse(
            metrics.is_internal_user(env={"HOSTNAME": "dev.example.com"})
        )
        self.assertFalse(
            metrics.is_internal_user(
                env={"HOSTNAME": "corp.google.com.example.org"}
            )
        )
        self.assertTrue(metrics.is_internal_user(env={}))
        self.assertTrue(metrics.is_internal_user(env={"HOSTNAME": ""}))
        mock_gethostname.return_value = "external-box.local"
        self.assertFalse(metrics.is_internal_user(env={}))

    @mock.patch.object(
        metrics.socket,
        "gethostname",
        autospec=True,
        spec_set=True,
        return_value="fallback.c.googlers.com",
    )
    def test_sanitize_string(self, mock_gethostname: mock.MagicMock) -> None:
        checkout_dir = self.root_dir / "alice" / "fuchsia"
        checkout_dir.mkdir(parents=True, exist_ok=True)
        env_map = {
            "USER": "alice",
            "HOSTNAME": "ws.corp.google.com",
        }
        raw_text = (
            f"{checkout_dir}/out/default --user alice "
            "--host ws.corp.google.com"
        )
        sanitized = metrics.sanitize_string(raw_text, checkout_dir, env=env_map)
        self.assertEqual(
            sanitized,
            "$FUCHSIA_DIR/out/default --user $USER --host $HOSTNAME",
        )

        fallback_sanitized = metrics.sanitize_string(
            "host=fallback.c.googlers.com path=" + str(checkout_dir),
            checkout_dir,
            env={"USER": ""},
        )
        self.assertEqual(
            fallback_sanitized,
            "host=$HOSTNAME path=$FUCHSIA_DIR",
        )
        mock_gethostname.assert_called()

    def test_read_config_short_circuits_when_mocked(self) -> None:
        mock_marker = (
            self.fuchsia_dir
            / "tools"
            / "devshell"
            / "lib"
            / "metrics.sh.mock_side_effects"
        )
        mock_marker.parent.mkdir(parents=True, exist_ok=True)
        mock_marker.write_text("# mock\n", encoding="utf-8")
        (self._internal_dir() / "analytics-status-internal").write_text(
            "2\n", encoding="utf-8"
        )
        self._write_fx_metrics_cfg('METRICS_ENABLED=1\nMETRICS_UUID="abc"\n')
        env_map = _base_env(
            self.home_dir,
            hostname="host.corp.google.com",
            xdg_data_home=self.xdg_dir,
        )

        cfg, config_ok = metrics.read_config(self.fuchsia_dir, env=env_map)
        self.assertTrue(config_ok)
        self.assertTrue(cfg.is_mocked)
        self.assertFalse(cfg.should_report)
        self.assertEqual(cfg.level, 0)
        self.assertEqual(cfg.enabled, 0)

    def test_read_config_external_and_shell_parsing(self) -> None:
        env_map = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)

        # Missing config -> unconfigured, disabled
        cfg_missing, ok_missing = metrics.read_config(
            self.fuchsia_dir, env=env_map
        )
        self.assertFalse(ok_missing)
        self.assertEqual(cfg_missing.level, 0)
        self.assertEqual(cfg_missing.enabled, 0)
        self.assertFalse(cfg_missing.is_internal)

        # METRICS_ENABLED=1 with empty METRICS_UUID -> invalid/disabled
        self._write_fx_metrics_cfg('METRICS_ENABLED=1\nMETRICS_UUID=""\n')
        cfg_empty_uuid, ok_empty_uuid = metrics.read_config(
            self.fuchsia_dir, env=env_map
        )
        self.assertFalse(ok_empty_uuid)
        self.assertEqual(cfg_empty_uuid.level, 0)
        self.assertEqual(cfg_empty_uuid.enabled, 0)

        # Inline '#' comment and last-assignment-wins (enable then disable)
        self._write_fx_metrics_cfg(
            "# header comment\n"
            "METRICS_ENABLED=1 # turn on\n"
            'METRICS_UUID="ext-uuid-1"\n'
            "METRICS_ENABLED=0 # disable overrides prior line\n"
        )
        cfg_last_off, ok_last_off = metrics.read_config(
            self.fuchsia_dir, env=env_map
        )
        self.assertTrue(ok_last_off)
        self.assertEqual(cfg_last_off.level, 0)
        self.assertEqual(cfg_last_off.enabled, 0)
        self.assertEqual(cfg_last_off.other_uuid, "")

        # Last-assignment-wins (disable then enable), single/double quotes,
        # quoted '#' preserved, and empty other_uuid when shared uuid is absent.
        self._write_fx_metrics_cfg(
            "METRICS_ENABLED=0\n"
            "METRICS_ENABLED=1 # enable wins\n"
            "METRICS_UUID='ext-uuid-2' # single quoted\n"
            '_METRICS_DEBUG_LOG_FILE="/tmp/keep#hash.log"\n'
        )
        cfg_enabled, ok_enabled = metrics.read_config(
            self.fuchsia_dir, env=env_map
        )
        self.assertTrue(ok_enabled)
        self.assertEqual(cfg_enabled.level, 1)
        self.assertEqual(cfg_enabled.enabled, 1)
        self.assertEqual(cfg_enabled.uuid, "ext-uuid-2")
        self.assertEqual(cfg_enabled.debug_log_file, "/tmp/keep#hash.log")
        self.assertEqual(cfg_enabled.other_uuid, "")

        # METRICS_CONFIG env override + shared uuid file
        (self._internal_dir() / "uuid").write_text(
            "shared-ffx-uuid\n", encoding="utf-8"
        )
        custom_cfg = self.root_dir / "custom_metrics_cfg"
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="custom-uuid"\n', path=custom_cfg
        )
        cfg_custom, ok_custom = metrics.read_config(
            self.fuchsia_dir,
            env=env_map | {"METRICS_CONFIG": str(custom_cfg)},
        )
        self.assertTrue(ok_custom)
        self.assertEqual(cfg_custom.uuid, "custom-uuid")
        self.assertEqual(cfg_custom.other_uuid, "shared-ffx-uuid")

    @mock.patch.object(metrics, "subprocess", autospec=True, spec_set=True)
    def test_read_config_disabled_file_debug_log_does_not_enable(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        disabled_log = self.root_dir / "disabled.log"
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=0\nMETRICS_UUID="stale-ext-uuid"\n'
            f"_METRICS_DEBUG_LOG_FILE={disabled_log}\n"
        )
        env_map = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        cfg, config_ok = metrics.read_config(self.fuchsia_dir, env=env_map)
        self.assertTrue(config_ok)
        self.assertEqual(cfg.level, 0)
        self.assertEqual(cfg.enabled, 0)
        self.assertEqual(cfg.uuid, "")
        self.assertEqual(cfg.debug_log_file, str(disabled_log))
        self.assertFalse(cfg.debug_mode)
        self.assertFalse(cfg.should_report)

        metrics.send_events(
            self.fuchsia_dir,
            cfg,
            ({"name": "invoke", "params": {}, "timestamp_micros": 1},),
            env=env_map,
        )
        mock_subprocess.run.assert_not_called()
        self.assertFalse(disabled_log.exists())

        env_dbg_log = str(self.root_dir / "env_ext_override.log")
        cfg_override = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env=env_map | {"_METRICS_DEBUG_LOG_FILE": env_dbg_log},
        )
        self.assertEqual(cfg_override.level, 1)
        self.assertEqual(cfg_override.uuid, "TEST")
        self.assertTrue(cfg_override.debug_mode)

    def test_read_config_internal_levels_uuid_autogen_and_stale_ignored(
        self,
    ) -> None:
        env_map = _base_env(
            self.home_dir,
            hostname="dev.corp.google.com",
            xdg_data_home=self.xdg_dir,
        )
        status_file = self._internal_dir() / "analytics-status-internal"

        # Level 0 ignores stale METRICS_ENABLED=1 and stale METRICS_UUID in
        # .fx/config/metrics and never reports even with on-disk debug log.
        status_file.write_text("0\n", encoding="utf-8")
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="stale-uuid"\n'
            "_METRICS_DEBUG_LOG_FILE=/tmp/int_disabled.log\n"
        )
        cfg_zero, ok_zero = metrics.read_config(self.fuchsia_dir, env=env_map)
        self.assertTrue(ok_zero)
        self.assertTrue(cfg_zero.is_internal)
        self.assertEqual(cfg_zero.level, 0)
        self.assertEqual(cfg_zero.enabled, 0)
        self.assertEqual(cfg_zero.uuid, "")
        self.assertFalse(cfg_zero.should_report)

        cfg_zero_override = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env=env_map | {"_METRICS_DEBUG_LOG_FILE": "/tmp/env_int_dbg.log"},
        )
        self.assertEqual(cfg_zero_override.level, 1)
        self.assertEqual(cfg_zero_override.uuid, "TEST")

        # Level 2 with missing .fx/config/metrics auto-generates UUID and writes
        status_file.write_text("2\n", encoding="utf-8")
        cfg_path = self.fuchsia_dir / ".fx" / "config" / "metrics"
        cfg_path.unlink()
        cfg_two, ok_two = metrics.read_config(self.fuchsia_dir, env=env_map)
        self.assertTrue(ok_two)
        self.assertTrue(cfg_two.is_internal)
        self.assertEqual(cfg_two.level, 2)
        self.assertEqual(cfg_two.enabled, 1)
        self.assertEqual(cfg_two.other_uuid, "")
        parsed_uuid = uuid.UUID(cfg_two.uuid)
        self.assertEqual(str(parsed_uuid).upper(), cfg_two.uuid.upper())
        written_text = cfg_path.read_text(encoding="utf-8")
        self.assertIn(f'METRICS_UUID="{cfg_two.uuid}"', written_text)
        self.assertNotIn("METRICS_ENABLED", written_text)

        # Level 1 with empty METRICS_UUID auto-generates UUID and preserves
        # _METRICS_DEBUG_LOG_FILE in .fx/config/metrics.
        status_file.write_text("1\n", encoding="utf-8")
        self._write_fx_metrics_cfg(
            'METRICS_UUID=""\n_METRICS_DEBUG_LOG_FILE=/tmp/dbg.log\n'
        )
        cfg_one, ok_one = metrics.read_config(self.fuchsia_dir, env=env_map)
        self.assertTrue(ok_one)
        self.assertEqual(cfg_one.level, 1)
        self.assertEqual(cfg_one.enabled, 1)
        self.assertNotEqual(cfg_one.uuid, "")
        self.assertEqual(cfg_one.debug_log_file, "/tmp/dbg.log")
        self.assertIn(
            "_METRICS_DEBUG_LOG_FILE=/tmp/dbg.log",
            cfg_path.read_text(encoding="utf-8"),
        )

        # Legacy unmigrated internal user (analytics-status-internal absent
        # while .fx/config/metrics has METRICS_ENABLED=1 + METRICS_UUID):
        # preserves level=1 reporting and exit-code 0 from read_config, while
        # read_and_validate still emits the Googler migration notice.
        status_file.unlink()
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="legacy-int-uuid"\n'
        )
        cfg_legacy, ok_legacy = metrics.read_config(
            self.fuchsia_dir, env=env_map
        )
        self.assertTrue(ok_legacy)
        self.assertTrue(cfg_legacy.is_internal)
        self.assertEqual(cfg_legacy.level, 1)
        self.assertEqual(cfg_legacy.enabled, 1)
        self.assertEqual(cfg_legacy.uuid, "legacy-int-uuid")

        legacy_err = io.StringIO()
        cfg_legacy_val, ok_legacy_val = metrics.read_and_validate(
            self.fuchsia_dir, False, stderr=legacy_err, env=env_map
        )
        self.assertFalse(ok_legacy_val)
        self.assertEqual(cfg_legacy_val.level, 1)
        self.assertIn("Googler", legacy_err.getvalue())

        # Unconfigured internal user (no analytics-status-internal and no
        # .fx/config/metrics) still exits 0 from `read-config` CLI.
        cfg_path.unlink()
        cfg_unconf_int, ok_unconf_int = metrics.read_config(
            self.fuchsia_dir, env=env_map
        )
        self.assertTrue(ok_unconf_int)
        self.assertEqual(cfg_unconf_int.level, 0)
        int_stdout = io.StringIO()
        with (
            mock.patch.dict(os.environ, env_map, clear=True),
            contextlib.redirect_stdout(int_stdout),
        ):
            self.assertEqual(
                metrics.main(
                    [
                        "read-config",
                        f"--fuchsia-dir={self.fuchsia_dir}",
                    ]
                ),
                0,
            )
        self.assertEqual(
            int_stdout.getvalue().splitlines(),
            [
                "METRICS_LEVEL=0",
                "METRICS_ENABLED=0",
                "METRICS_UUID=''",
                "OTHER_TOOLS_ANALYTICS_UUID=''",
                "_METRICS_DEBUG_LOG_FILE=''",
            ],
        )
        ext_env = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        ext_stdout = io.StringIO()
        with (
            mock.patch.dict(os.environ, ext_env, clear=True),
            contextlib.redirect_stdout(ext_stdout),
        ):
            self.assertEqual(
                metrics.main(
                    [
                        "read-config",
                        f"--fuchsia-dir={self.fuchsia_dir}",
                    ]
                ),
                1,
            )

    def test_read_config_default_xdg_and_env_debug_flags(self) -> None:
        default_internal_dir = (
            self.home_dir / ".local" / "share" / "Fuchsia" / "metrics"
        )
        default_internal_dir.mkdir(parents=True, exist_ok=True)
        (default_internal_dir / "analytics-status-internal").write_text(
            "1\n", encoding="utf-8"
        )
        (default_internal_dir / "uuid").write_text(
            "xdg-uuid\n", encoding="utf-8"
        )
        self._write_fx_metrics_cfg('METRICS_UUID="fx-uuid"\n')

        env_map = _base_env(
            self.home_dir,
            hostname="dev.c.googlers.com",
            extra={
                "_METRICS_DEBUG": "1",
                "_METRICS_DEBUG_LOG_FILE": "/tmp/env_dbg.log",
                "_METRICS_USE_VALIDATION_SERVER": "1",
            },
        )
        cfg, config_ok = metrics.read_config(self.fuchsia_dir, env=env_map)
        self.assertTrue(config_ok)
        self.assertEqual(cfg.level, 1)
        self.assertEqual(cfg.uuid, "fx-uuid")
        self.assertEqual(cfg.other_uuid, "xdg-uuid")
        self.assertEqual(cfg.debug_log_file, "/tmp/env_dbg.log")
        self.assertTrue(cfg.debug_mode)
        self.assertTrue(cfg.use_validation_server)

    def test_write_config_and_write_config_internal(self) -> None:
        env_map = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        cfg_path = self.fuchsia_dir / ".fx" / "config" / "metrics"

        metrics.write_config(
            self.fuchsia_dir,
            1,
            "write-uuid-1",
            "/tmp/write.log",
            env=env_map,
        )
        text = cfg_path.read_text(encoding="utf-8")
        self.assertIn("METRICS_ENABLED=1", text)
        self.assertIn('METRICS_UUID="write-uuid-1"', text)
        self.assertIn("_METRICS_DEBUG_LOG_FILE=/tmp/write.log", text)

        # Idempotent rewrite preserves content without failure
        metrics.write_config(
            self.fuchsia_dir,
            1,
            "write-uuid-1",
            "/tmp/write.log",
            env=env_map,
        )
        self.assertEqual(cfg_path.read_text(encoding="utf-8"), text)

        metrics.write_config(
            self.fuchsia_dir, 0, "ignored-when-disabled", env=env_map
        )
        disabled_text = cfg_path.read_text(encoding="utf-8")
        self.assertIn("METRICS_ENABLED=0", disabled_text)
        self.assertIn('METRICS_UUID=""', disabled_text)

        metrics.write_config_internal(
            self.fuchsia_dir, "int-uuid-1", "/tmp/int.log", env=env_map
        )
        int_text = cfg_path.read_text(encoding="utf-8")
        self.assertIn('METRICS_UUID="int-uuid-1"', int_text)
        self.assertIn("_METRICS_DEBUG_LOG_FILE=/tmp/int.log", int_text)
        self.assertNotIn("METRICS_ENABLED", int_text)

    def test_maybe_log(self) -> None:
        log_file = self.root_dir / "events.log"
        metrics.maybe_log("", ("should", "not", "write"))
        self.assertFalse(log_file.exists())

        metrics.maybe_log(str(log_file), ("first", "RESULT=202"))
        metrics.maybe_log(str(log_file), ("second", "RESULT=200"))
        lines = log_file.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 2)
        self.assertRegex(lines[0], r"^\d{8}_\d{6}: first RESULT=202$")
        self.assertRegex(lines[1], r"^\d{8}_\d{6}: second RESULT=200$")

        # Explicit --log-file="" suppresses logging even when on-disk config
        # specifies _METRICS_DEBUG_LOG_FILE; omitting --log-file falls back.
        disk_log = self.root_dir / "disk_events.log"
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="u-log"\n'
            f"_METRICS_DEBUG_LOG_FILE={disk_log}\n"
        )
        ext_env = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        with mock.patch.dict(os.environ, ext_env, clear=True):
            self.assertEqual(
                metrics.main(
                    [
                        "maybe-log",
                        f"--fuchsia-dir={self.fuchsia_dir}",
                        "--log-file=",
                        "--",
                        "suppressed",
                    ]
                ),
                0,
            )
            self.assertFalse(disk_log.exists())
            self.assertEqual(
                metrics.main(
                    [
                        "maybe-log",
                        f"--fuchsia-dir={self.fuchsia_dir}",
                        "--",
                        "from-config",
                    ]
                ),
                0,
            )
        disk_lines = disk_log.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(disk_lines), 1)
        self.assertRegex(disk_lines[0], r"^\d{8}_\d{6}: from-config$")

        missing_parent_log = self.root_dir / "nonexistent_dir" / "events.log"
        metrics.maybe_log(str(missing_parent_log), ("ignored",))
        self.assertFalse(missing_parent_log.exists())

    def test_read_and_validate_migrations_and_warnings(self) -> None:
        env_ext = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        legacy_cfg = self.fuchsia_dir / ".fx-metrics-config"
        new_cfg = self.fuchsia_dir / ".fx" / "config" / "metrics"
        legacy_cfg.write_text(
            'METRICS_ENABLED=1\nMETRICS_UUID="migrated-uuid"\n',
            encoding="utf-8",
        )

        # TTY migration prints blue ANSI INFO prefix
        tty_err = _TtyStringIO()
        cfg_mig, ok_mig = metrics.read_and_validate(
            self.fuchsia_dir, False, stderr=tty_err, env=env_ext
        )
        self.assertTrue(ok_mig)
        self.assertEqual(cfg_mig.level, 1)
        self.assertEqual(cfg_mig.uuid, "migrated-uuid")
        self.assertFalse(legacy_cfg.exists())
        self.assertTrue(new_cfg.is_file())
        self.assertIn("\033[1;34mINFO:\033[0m Moving ", tty_err.getvalue())

        # Plain non-TTY warning when external config is removed
        new_cfg.unlink()
        plain_err = io.StringIO()
        cfg_warn, ok_warn = metrics.read_and_validate(
            self.fuchsia_dir, False, stderr=plain_err, env=env_ext
        )
        self.assertFalse(ok_warn)
        self.assertEqual(cfg_warn.level, 0)
        self.assertIn("WARNING: ", plain_err.getvalue())
        self.assertIn("Please opt in or out", plain_err.getvalue())
        self.assertNotIn("\033[", plain_err.getvalue())

        # hide_init_warning suppresses external warning
        suppressed_err = io.StringIO()
        metrics.read_and_validate(
            self.fuchsia_dir, True, stderr=suppressed_err, env=env_ext
        )
        self.assertEqual(suppressed_err.getvalue(), "")

        # Internal silent migration from analytics-status == "0"
        env_int = _base_env(
            self.home_dir,
            hostname="box.corp.google.com",
            xdg_data_home=self.xdg_dir,
        )
        int_dir = self._internal_dir()
        (int_dir / "analytics-status").write_text("0\n", encoding="utf-8")
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="stale-id"\n'
        )
        silent_err = io.StringIO()
        cfg_silent, ok_silent = metrics.read_and_validate(
            self.fuchsia_dir, False, stderr=silent_err, env=env_int
        )
        self.assertTrue(ok_silent)
        self.assertEqual(cfg_silent.level, 0)
        self.assertEqual(cfg_silent.enabled, 0)
        self.assertEqual(silent_err.getvalue(), "")
        self.assertEqual(
            (int_dir / "analytics-status-internal").read_text(encoding="utf-8"),
            "0\n",
        )

        # Unconfigured internal user emits TTY yellow INIT_WARNING_INTERNAL
        (int_dir / "analytics-status-internal").unlink()
        (int_dir / "analytics-status").unlink()
        new_cfg.unlink()
        int_tty_err = _TtyStringIO()
        cfg_int_unconf, ok_int_unconf = metrics.read_and_validate(
            self.fuchsia_dir, False, stderr=int_tty_err, env=env_int
        )
        self.assertFalse(ok_int_unconf)
        self.assertEqual(cfg_int_unconf.level, 0)
        self.assertIn("\033[1;33mWARNING:\033[0m", int_tty_err.getvalue())
        self.assertIn("Googler", int_tty_err.getvalue())
        self.assertIn(
            "ffx config analytics enable-enhanced", int_tty_err.getvalue()
        )

        # Legacy NO_COLOR disables ANSI even on TTY
        no_color_err = _TtyStringIO()
        metrics.read_and_validate(
            self.fuchsia_dir,
            False,
            stderr=no_color_err,
            env=env_int | {"NO_COLOR": "1"},
        )
        self.assertNotIn("\033[", no_color_err.getvalue())
        self.assertIn("WARNING: ", no_color_err.getvalue())

        # Mock short-circuit emits nothing
        mock_marker = (
            self.fuchsia_dir
            / "tools"
            / "devshell"
            / "lib"
            / "metrics.sh.mock_side_effects"
        )
        mock_marker.parent.mkdir(parents=True, exist_ok=True)
        mock_marker.write_text("# mock\n", encoding="utf-8")
        mock_err = io.StringIO()
        cfg_mocked, ok_mocked = metrics.read_and_validate(
            self.fuchsia_dir, False, stderr=mock_err, env=env_int
        )
        self.assertTrue(ok_mocked)
        self.assertTrue(cfg_mocked.is_mocked)
        self.assertEqual(mock_err.getvalue(), "")

    def test_should_hide_init_warning(self) -> None:
        clean_env = {"HOME": str(self.home_dir)}
        self.assertFalse(
            metrics.should_hide_init_warning("build", (), env=clean_env)
        )
        self.assertTrue(
            metrics.should_hide_init_warning(
                "metrics", ("enable",), env=clean_env
            )
        )
        self.assertTrue(
            metrics.should_hide_init_warning(
                "ffx", ("target", "list"), env=clean_env
            )
        )
        self.assertFalse(
            metrics.should_hide_init_warning(
                "ffx",
                (
                    "--config",
                    "fuchsia.analytics.ffx_invoker=fx",
                    "target",
                    "list",
                ),
                env=clean_env,
            )
        )
        self.assertTrue(
            metrics.should_hide_init_warning(
                "ffx",
                (
                    "--config",
                    "fuchsia.analytics.ffx_invoker=fx",
                    "config",
                    "analytics",
                    "show",
                ),
                env=clean_env,
            )
        )
        self.assertTrue(
            metrics.should_hide_init_warning(
                "build", (), env=clean_env | {"ANTIGRAVITY_AGENT": "1"}
            )
        )
        self.assertTrue(
            metrics.should_hide_init_warning(
                "build", (), env=clean_env | {"FFX_AGENT_SESSION_ID": "abc"}
            )
        )

    @mock.patch.object(metrics, "subprocess", autospec=True, spec_set=True)
    def test_init_command_metrics_empty_home_new_user_and_env_debug(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        fake_proc = mock.create_autospec(subprocess.Popen, instance=True)
        fake_proc._child_created = True
        mock_subprocess.Popen.return_value = fake_proc
        mock_subprocess.DEVNULL = subprocess.DEVNULL

        # Empty or unset HOME silently disables without warnings or new_user spawn
        for no_home_env in (
            {"HOME": "", "HOSTNAME": "box.corp.google.com"},
            {"HOSTNAME": "box.corp.google.com"},
        ):
            empty_home_err = io.StringIO()
            cfg_no_home = metrics.init_command_metrics(
                self.fuchsia_dir,
                "build",
                (),
                stderr=empty_home_err,
                env=no_home_env,
            )
            self.assertEqual(cfg_no_home.level, 0)
            self.assertEqual(cfg_no_home.enabled, 0)
            self.assertTrue(cfg_no_home.is_internal)
            self.assertEqual(empty_home_err.getvalue(), "")
            mock_subprocess.Popen.assert_not_called()

        # Unconfigured internal user shows notice and spawns detached
        # send-new-user process
        int_env = _base_env(
            self.home_dir,
            hostname="box.corp.google.com",
            xdg_data_home=self.xdg_dir,
        )
        notice_err = io.StringIO()
        cfg_new = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=notice_err,
            env=int_env,
        )
        self.assertEqual(cfg_new.level, 0)
        self.assertIn("Googler", notice_err.getvalue())
        mock_subprocess.Popen.assert_called_once()
        popen_args = mock_subprocess.Popen.call_args.args[0]
        popen_kwargs = mock_subprocess.Popen.call_args.kwargs
        self.assertIn("send-new-user", popen_args)
        self.assertTrue(popen_kwargs.get("start_new_session"))
        self.assertFalse(fake_proc._child_created)

        # Missing analytics dir (no HOME or XDG_DATA_HOME) skips spawning
        # send-new-user rather than spawning a worker that immediately exits.
        mock_subprocess.Popen.reset_mock()
        metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env={
                "HOME": "",
                "XDG_DATA_HOME": "",
                "HOSTNAME": "box.corp.google.com",
                "_METRICS_TESTING_DEBUG_LOG_FILE": str(
                    self.root_dir / "no_analytics_dir.log"
                ),
            },
        )
        mock_subprocess.Popen.assert_not_called()

        # Env _METRICS_DEBUG_LOG_FILE enables level 1 with UUID "TEST" when
        # unconfigured, and preserves level 2 & real UUID when configured.
        env_dbg_log = str(self.root_dir / "env_debug.log")
        cfg_env_unconf = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env=_base_env(
                self.home_dir,
                hostname="ext.example.com",
                xdg_data_home=self.xdg_dir,
                extra={"_METRICS_DEBUG_LOG_FILE": env_dbg_log},
            ),
        )
        self.assertEqual(cfg_env_unconf.level, 1)
        self.assertEqual(cfg_env_unconf.uuid, "TEST")
        self.assertEqual(cfg_env_unconf.debug_log_file, env_dbg_log)
        self.assertTrue(cfg_env_unconf.debug_mode)

        cfg_env_no_home = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env={
                "HOME": "",
                "HOSTNAME": "ext.example.com",
                "_METRICS_DEBUG_LOG_FILE": env_dbg_log,
            },
        )
        self.assertEqual(cfg_env_no_home.level, 1)
        self.assertEqual(cfg_env_no_home.enabled, 1)
        self.assertEqual(cfg_env_no_home.uuid, "TEST")
        self.assertEqual(cfg_env_no_home.debug_log_file, env_dbg_log)
        self.assertTrue(cfg_env_no_home.debug_mode)

        (self._internal_dir() / "analytics-status-internal").write_text(
            "2\n", encoding="utf-8"
        )
        self._write_fx_metrics_cfg('METRICS_UUID="real-int-uuid"\n')
        cfg_env_lvl2 = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env=int_env | {"_METRICS_DEBUG_LOG_FILE": env_dbg_log},
        )
        self.assertEqual(cfg_env_lvl2.level, 2)
        self.assertEqual(cfg_env_lvl2.uuid, "real-int-uuid")
        self.assertEqual(cfg_env_lvl2.debug_log_file, env_dbg_log)
        self.assertTrue(cfg_env_lvl2.debug_mode)

    @mock.patch.object(metrics, "subprocess", autospec=True, spec_set=True)
    def test_is_remote_workflow_device(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        build_dir = self.fuchsia_dir / "out" / "default"
        build_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "out/default\n", encoding="utf-8"
        )
        base_env = _base_env(self.home_dir)

        # Unbuilt ffx: checks FUCHSIA_DEVICE_ADDR / FUCHSIA_NODENAME / .device
        self.assertFalse(
            metrics.is_remote_workflow_device(self.fuchsia_dir, env=base_env)
        )
        self.assertTrue(
            metrics.is_remote_workflow_device(
                self.fuchsia_dir,
                env=base_env | {"FUCHSIA_DEVICE_ADDR": "[::1]:8022"},
            )
        )
        self.assertTrue(
            metrics.is_remote_workflow_device(
                self.fuchsia_dir,
                env=base_env | {"FUCHSIA_NODENAME": "[::1]:8022"},
            )
        )
        device_file = self.fuchsia_dir / "out" / "default.device"
        device_file.write_text("[::1]:8022\n", encoding="utf-8")
        self.assertTrue(
            metrics.is_remote_workflow_device(self.fuchsia_dir, env=base_env)
        )

        # Built ffx: env and ${build_dir}.device take precedence over ffx
        ffx_bin = build_dir / "host-tools" / "ffx"
        ffx_bin.parent.mkdir(parents=True, exist_ok=True)
        ffx_bin.write_text("", encoding="utf-8")
        ffx_bin.chmod(0o755)

        mock_subprocess.DEVNULL = subprocess.DEVNULL
        mock_subprocess.PIPE = subprocess.PIPE
        mock_subprocess.TimeoutExpired = subprocess.TimeoutExpired
        self.assertTrue(
            metrics.is_remote_workflow_device(
                self.fuchsia_dir,
                env=base_env | {"FUCHSIA_DEVICE_ADDR": "[::1]:8022"},
            )
        )
        self.assertFalse(
            metrics.is_remote_workflow_device(
                self.fuchsia_dir,
                env=base_env | {"FUCHSIA_NODENAME": "local-node"},
            )
        )
        # device_file still contains "[::1]:8022" -> returns True without ffx
        self.assertTrue(
            metrics.is_remote_workflow_device(self.fuchsia_dir, env=base_env)
        )
        mock_subprocess.run.assert_not_called()

        # Without env or .device, falls back to built ffx with analytics
        # disabled, FUCHSIA_BUILD_DIR set, and a timeout.
        device_file.unlink()
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=(), returncode=0, stdout="[::1]:8022\n", stderr=""
        )
        self.assertTrue(
            metrics.is_remote_workflow_device(self.fuchsia_dir, env=base_env)
        )
        run_args = mock_subprocess.run.call_args.args[0]
        run_kwargs = mock_subprocess.run.call_args.kwargs
        self.assertSequenceEqual(
            run_args,
            (
                str(ffx_bin),
                "--config",
                "ffx.analytics.disabled=true",
                "target",
                "default",
                "get",
            ),
        )
        self.assertEqual(run_kwargs.get("stdin"), subprocess.DEVNULL)
        self.assertEqual(run_kwargs.get("stdout"), subprocess.PIPE)
        self.assertEqual(run_kwargs.get("stderr"), subprocess.DEVNULL)
        self.assertTrue(run_kwargs.get("close_fds"))
        self.assertEqual(
            run_kwargs["env"].get("FUCHSIA_BUILD_DIR"), str(build_dir)
        )
        self.assertGreater(run_kwargs.get("timeout", 0), 0)

        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=(), returncode=0, stdout="local-node\n", stderr=""
        )
        self.assertFalse(
            metrics.is_remote_workflow_device(self.fuchsia_dir, env=base_env)
        )

        # TimeoutExpired is handled gracefully
        mock_subprocess.run.side_effect = subprocess.TimeoutExpired(
            cmd=run_args, timeout=5.0
        )
        self.assertFalse(
            metrics.is_remote_workflow_device(self.fuchsia_dir, env=base_env)
        )
        mock_subprocess.run.side_effect = None

        # Relative HOST_OUT_DIR anchors against build_dir in _find_built_ffx
        ffx_bin.unlink()
        rel_ffx = build_dir / "host_arm64" / "ffx"
        rel_ffx.parent.mkdir(parents=True, exist_ok=True)
        rel_ffx.write_text("", encoding="utf-8")
        rel_ffx.chmod(0o755)
        self.assertEqual(
            metrics._find_built_ffx(build_dir, {"HOST_OUT_DIR": "host_arm64"}),
            rel_ffx,
        )

    def test_oserror_guards_on_readonly_paths(self) -> None:
        env_ext = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        old_cfg = self.fuchsia_dir / ".fx-metrics-config"
        old_cfg.write_text(
            'METRICS_ENABLED=1\nMETRICS_UUID="ro-uuid"\n', encoding="utf-8"
        )
        with mock.patch.object(
            metrics.shutil,
            "move",
            autospec=True,
            spec_set=True,
            side_effect=PermissionError("read-only checkout"),
        ):
            cfg_ro, ok_ro = metrics.read_and_validate(
                self.fuchsia_dir, True, stderr=io.StringIO(), env=env_ext
            )
        self.assertFalse(ok_ro)
        self.assertEqual(cfg_ro.level, 0)
        old_cfg.unlink()

        # Read-only internal silent migration suppresses notice and stays off
        env_int = _base_env(
            self.home_dir,
            hostname="box.corp.google.com",
            xdg_data_home=self.xdg_dir,
        )
        int_dir = self._internal_dir()
        (int_dir / "analytics-status").write_text("0\n", encoding="utf-8")
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="stale-ro"\n'
        )
        ro_err = io.StringIO()
        with mock.patch.object(
            pathlib.Path,
            "write_text",
            side_effect=PermissionError("read-only home"),
        ):
            cfg_int_ro, ok_int_ro = metrics.read_and_validate(
                self.fuchsia_dir, False, stderr=ro_err, env=env_int
            )
        self.assertTrue(ok_int_ro)
        self.assertEqual(cfg_int_ro.level, 0)
        self.assertEqual(ro_err.getvalue(), "")

        # Internal auto-repair write_config_internal OSError does not raise
        (int_dir / "analytics-status-internal").write_text(
            "2\n", encoding="utf-8"
        )
        (self.fuchsia_dir / ".fx" / "config" / "metrics").unlink()
        with mock.patch.object(
            metrics,
            "write_config_internal",
            autospec=True,
            spec_set=True,
            side_effect=PermissionError("read-only checkout"),
        ):
            cfg_repair, ok_repair = metrics.read_config(
                self.fuchsia_dir, env=env_int
            )
        self.assertTrue(ok_repair)
        self.assertEqual(cfg_repair.level, 2)
        self.assertNotEqual(cfg_repair.uuid, "")

        # os.getcwd() OSError in build_user_properties falls back cleanly
        with mock.patch.object(
            metrics.os,
            "getcwd",
            autospec=True,
            spec_set=True,
            side_effect=FileNotFoundError("deleted cwd"),
        ):
            props_cwd = metrics.build_user_properties(
                self.fuchsia_dir, cfg_repair, env={"HOME": str(self.home_dir)}
            )
        self.assertEqual(props_cwd["is_cog"]["value"], 0)

        # args.json stat() OSError in build_build_event yields size 0
        build_dir = self.fuchsia_dir / "out" / "ro_stat"
        build_dir.mkdir(parents=True, exist_ok=True)
        args_json = build_dir / "args.json"
        args_json.write_text("{}", encoding="utf-8")
        orig_stat = pathlib.Path.stat

        def _fail_args_stat(
            path_obj: pathlib.Path, *args: object, **kwargs: object
        ) -> os.stat_result:
            if path_obj == args_json:
                raise PermissionError("stat blocked")
            return orig_stat(path_obj, *args, **kwargs)  # type: ignore[arg-type]

        with mock.patch.object(
            pathlib.Path, "stat", autospec=True, side_effect=_fail_args_stat
        ):
            ev_stat = metrics.build_build_event(
                self.fuchsia_dir,
                dataclasses.replace(cfg_repair, level=1),
                1000,
                2000,
                0,
                "",
                "",
                build_dir,
                1,
                0,
                "false",
                env=env_int,
            )
        self.assertEqual(ev_stat["params"]["args_json_size"], 0)

    def test_resolve_build_dir_normalization(self) -> None:
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "//out/from_file/\n", encoding="utf-8"
        )
        self.assertEqual(
            metrics._resolve_build_dir(
                self.fuchsia_dir, {"_FX_BUILD_DIR": "///out/custom///"}
            ),
            self.fuchsia_dir / "out" / "custom",
        )
        self.assertEqual(
            metrics._resolve_build_dir(
                self.fuchsia_dir,
                {"_FX_BUILD_DIR": "//", "FUCHSIA_BUILD_DIR": "   "},
            ),
            self.fuchsia_dir / "out" / "from_file",
        )

    def test_build_user_properties_and_abfs_cog_flags(self) -> None:
        abfs_ws = self.root_dir / "abfs_ws"
        abfs_fuchsia = abfs_ws / "fuchsia"
        abfs_fuchsia.mkdir(parents=True, exist_ok=True)
        (abfs_ws / ".abfs.ws").write_text("", encoding="utf-8")

        cfg_lvl1 = metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="u1",
            debug_log_file="",
            other_uuid="other-1",
            is_internal=True,
        )
        env_map = _base_env(
            self.home_dir,
            hostname="box.corp.google.com",
            extra={
                "BASH_VERSION": "5.2.15(1)-release",
                "NINJA_PERSISTENT_MODE": "on",
                "PWD": "/google/cartfs/mount/user/fuchsia",
            },
        )
        props = metrics.build_user_properties(
            abfs_fuchsia, cfg_lvl1, env=env_map
        )
        self.assertEqual(props["os"]["value"], platform.system())
        self.assertEqual(props["arch"]["value"], platform.machine())
        self.assertEqual(
            props["kernel_release"]["value"],
            f"{platform.system()} {platform.release()}",
        )
        self.assertEqual(props["shell"]["value"], "bash")
        self.assertEqual(props["shell_version"]["value"], "5.2.15(1)-release")
        self.assertEqual(props["ninja_persistent"]["value"], "on")
        self.assertEqual(props["other_uuid"]["value"], "other-1")
        self.assertEqual(props["internal"]["value"], 1)
        self.assertEqual(props["is_cog"]["value"], 1)
        self.assertEqual(props["is_abfs"]["value"], 1)
        self.assertEqual(props["metrics_level"]["value"], 1)
        self.assertEqual(props["metrics_version"]["value"], 13)
        self.assertEqual(props["nproc"]["value"], -1)
        self.assertIs(type(props["internal"]["value"]), int)
        self.assertIs(type(props["metrics_version"]["value"]), int)

        cfg_lvl2 = dataclasses.replace(cfg_lvl1, level=2, is_internal=False)
        props_lvl2 = metrics.build_user_properties(
            self.fuchsia_dir,
            cfg_lvl2,
            env=_base_env(
                self.home_dir,
                extra={
                    "ZSH_VERSION": "5.9",
                    "NINJA_PERSISTENT_MODE": "invalid-mode",
                    "PWD": str(self.fuchsia_dir),
                },
            ),
        )
        self.assertEqual(props_lvl2["shell"]["value"], "zsh")
        self.assertEqual(props_lvl2["shell_version"]["value"], "5.9")
        self.assertEqual(props_lvl2["ninja_persistent"]["value"], "unsupported")
        self.assertEqual(props_lvl2["internal"]["value"], 0)
        self.assertEqual(props_lvl2["is_cog"]["value"], 0)
        self.assertEqual(props_lvl2["is_abfs"]["value"], 0)
        self.assertGreater(int(props_lvl2["nproc"]["value"]), 0)

        # Forwarded _FX_BASH_VERSION / _FX_ZSH_VERSION and directory .abfs.ws
        abfs_dir_ws = self.root_dir / "abfs_dir_ws"
        abfs_dir_fuchsia = abfs_dir_ws / "fuchsia"
        abfs_dir_fuchsia.mkdir(parents=True, exist_ok=True)
        (abfs_dir_ws / ".abfs.ws").mkdir(parents=True, exist_ok=True)
        props_fwd_bash = metrics.build_user_properties(
            abfs_dir_fuchsia,
            cfg_lvl1,
            env=_base_env(
                self.home_dir,
                extra={"_FX_BASH_VERSION": "5.2.37(1)-release"},
            ),
        )
        self.assertEqual(props_fwd_bash["shell"]["value"], "bash")
        self.assertEqual(
            props_fwd_bash["shell_version"]["value"], "5.2.37(1)-release"
        )
        self.assertEqual(props_fwd_bash["is_abfs"]["value"], 1)

        props_fwd_zsh = metrics.build_user_properties(
            self.fuchsia_dir,
            cfg_lvl1,
            env=_base_env(
                self.home_dir,
                extra={"_FX_ZSH_VERSION": "5.9"},
            ),
        )
        self.assertEqual(props_fwd_zsh["shell"]["value"], "zsh")
        self.assertEqual(props_fwd_zsh["shell_version"]["value"], "5.9")

    def test_build_feature_events(self) -> None:
        features = metadata.load_optional_features_meta(None)
        self.assertGreater(len(features), 0)
        first_enabled = next(f for f in features if f.default_enabled)
        first_disabled = next(f for f in features if not f.default_enabled)

        events = metrics.build_feature_events(
            self.fuchsia_dir,
            env={
                f"FUCHSIA_DISABLED_{first_enabled.name}": "1",
                f"FUCHSIA_DISABLED_{first_disabled.name}": "0",
            },
        )
        by_name = {ev["params"]["feature"]: ev for ev in events}
        self.assertEqual(by_name[first_enabled.name]["name"], "feature")
        self.assertEqual(
            by_name[first_enabled.name]["params"]["status"], "disabled"
        )
        self.assertEqual(
            by_name[first_disabled.name]["params"]["status"], "enabled"
        )
        self.assertIs(
            type(by_name[first_enabled.name]["timestamp_micros"]), int
        )

    def test_build_command_start_events_and_vendor_set_redaction(self) -> None:
        cfg_lvl1 = metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="u1",
            debug_log_file="",
            other_uuid="",
            is_internal=True,
        )
        env_map = _base_env(
            self.home_dir,
            hostname="dev.corp.google.com",
            extra={
                "FUCHSIA_FX_INVOKER": "cli",
                "FUCHSIA_METRICS_CALL_STACK": "fx " * 300,
            },
        )

        # Disabled config yields empty event list
        cfg_off = dataclasses.replace(cfg_lvl1, level=0, enabled=0)
        self.assertSequenceEqual(
            metrics.build_command_start_events(
                self.fuchsia_dir, cfg_off, "build", (), env=env_map
            ),
            (),
        )

        # `fx set` emits package hits + invoke hit (with optional features)
        set_events = metrics.build_command_start_events(
            self.fuchsia_dir,
            cfg_lvl1,
            "set",
            ("core.x64", "--with", "//a,//b", "--with-base", "//c"),
            include_features=True,
            env=env_map,
        )
        names = tuple(ev["name"] for ev in set_events)
        self.assertIn("feature", names)
        self.assertEqual(names[-1], "invoke")
        set_hits = tuple(
            ev["params"] for ev in set_events if ev["name"] == "set"
        )
        self.assertSequenceEqual(
            set_hits,
            (
                {"with_type": "fx-with", "package_name": "//a"},
                {"with_type": "fx-with", "package_name": "//b"},
                {"with_type": "fx-with-base", "package_name": "//c"},
            ),
        )
        invoke_params = set_events[-1]["params"]
        self.assertEqual(invoke_params["subcommand"], "set")
        self.assertEqual(invoke_params["args"], "")
        self.assertEqual(invoke_params["args_truncated"], "0")
        self.assertEqual(invoke_params["invoker"], "cli")
        self.assertEqual(len(invoke_params["call_stack"]), 500)

        # Vendor command rewrites subcommand to vendor/<v>/<subcmd>
        vendor_events = metrics.build_command_start_events(
            self.fuchsia_dir,
            cfg_lvl1,
            "vendor",
            ("google", "flash-kola", "--fast"),
            env=env_map,
        )
        self.assertEqual(
            vendor_events[-1]["params"]["subcommand"],
            "vendor/google/flash-kola",
        )
        self.assertEqual(vendor_events[-1]["params"]["args"], "")

        # Regex & known/unknown op tracking at level 1
        pkg_url = "fuchsia-pkg://fuchsia.com/foo#meta/bar.cm"
        run_ev = metrics.build_command_start_events(
            self.fuchsia_dir, cfg_lvl1, "run", (pkg_url, "--extra"), env=env_map
        )[-1]
        self.assertEqual(run_ev["params"]["args"], pkg_url)

        shell_pkg_ev = metrics.build_command_start_events(
            self.fuchsia_dir,
            cfg_lvl1,
            "shell",
            ("run", pkg_url, "--extra"),
            env=env_map,
        )[-1]
        self.assertEqual(shell_pkg_ev["params"]["args"], f"run {pkg_url}")

        shell_op_ev = metrics.build_command_start_events(
            self.fuchsia_dir, cfg_lvl1, "shell", ("uname", "-a"), env=env_map
        )[-1]
        self.assertEqual(shell_op_ev["params"]["args"], "uname")

        shell_unknown_ev = metrics.build_command_start_events(
            self.fuchsia_dir,
            cfg_lvl1,
            "shell",
            ("secret_cmd", "--arg"),
            env=env_map,
        )[-1]
        self.assertEqual(
            shell_unknown_ev["params"]["args"], "$unknown_subcommand"
        )

        # Level 2 sanitizes and chunks at 500 / 1000 chars
        cfg_lvl2 = dataclasses.replace(cfg_lvl1, level=2)
        long_tail = "X" * 1050
        lvl2_ev = metrics.build_command_start_events(
            self.fuchsia_dir,
            cfg_lvl2,
            "build",
            (
                f"{self.fuchsia_dir}/a",
                "testuser",
                "dev.corp.google.com",
                long_tail,
            ),
            env=env_map,
        )[-1]
        self.assertTrue(
            lvl2_ev["params"]["args"].startswith(
                "$FUCHSIA_DIR/a $USER $HOSTNAME "
            )
        )
        self.assertEqual(len(lvl2_ev["params"]["args"]), 500)
        self.assertEqual(len(lvl2_ev["params"]["args2"]), 500)
        self.assertEqual(lvl2_ev["params"]["args_truncated"], "1")

    def test_build_command_finish_event(self) -> None:
        cfg_lvl1 = metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="u1",
            debug_log_file="",
            other_uuid="",
            is_internal=True,
        )
        env_map = _base_env(
            self.home_dir,
            hostname="dev.corp.google.com",
            extra={
                "FUCHSIA_FX_INVOKER": "antigravity",
                "FUCHSIA_METRICS_CALL_STACK": "fx",
            },
        )
        ev1 = metrics.build_command_finish_event(
            self.fuchsia_dir,
            cfg_lvl1,
            1_000_000,
            1_055_500,
            3,
            "vendor",
            1,
            ("google", "flash-kola", "--secret"),
            env=env_map,
        )
        self.assertEqual(ev1["name"], "finish")
        p1 = ev1["params"]
        self.assertEqual(p1["subcommand"], "vendor")
        self.assertEqual(p1["args"], "")
        self.assertEqual(p1["args2"], "")
        self.assertEqual(p1["args_truncated"], "0")
        self.assertEqual(p1["exit_status"], "3")
        self.assertEqual(p1["is_remote"], "1")
        self.assertIs(type(p1["timing"]), int)
        self.assertEqual(p1["timing"], 55)
        self.assertIs(type(p1["start_time_micros"]), int)
        self.assertEqual(p1["start_time_micros"], 1_000_000)
        self.assertIs(type(p1["end_time_micros"]), int)
        self.assertEqual(p1["end_time_micros"], 1_055_500)
        self.assertEqual(p1["invoker"], "antigravity")
        self.assertEqual(p1["call_stack"], "fx")

        cfg_lvl2 = dataclasses.replace(cfg_lvl1, level=2)
        ev2 = metrics.build_command_finish_event(
            self.fuchsia_dir,
            cfg_lvl2,
            1_000_000,
            2_000_000,
            0,
            "build",
            0,
            (f"{self.fuchsia_dir}/tgt", "Y" * 1100),
            env=env_map,
        )
        p2 = ev2["params"]
        self.assertTrue(p2["args"].startswith("$FUCHSIA_DIR/tgt "))
        self.assertEqual(len(p2["args"]), 500)
        self.assertEqual(len(p2["args2"]), 500)
        self.assertEqual(p2["args_truncated"], "1")
        self.assertEqual(p2["is_remote"], "0")

    def test_build_build_event(self) -> None:
        build_dir = self.fuchsia_dir / "out" / "default"
        build_dir.mkdir(parents=True, exist_ok=True)
        args_payload = {
            "build_info_board": "x64",
            "build_info_product": "core",
            "rbe_mode": "cloud",
            "compilation_mode": "balanced",
            "optimize": "moderate",
            "select_variant": ["profile"],
            "target_cpu": "x64",
            "rust_incremental": "out/inc",
            "main_pb_label": "//products:core",
            "cxx_rbe_enable": True,
            "use_restat_wrapper": True,
        }
        (build_dir / "args.json").write_text(
            json.dumps(args_payload), encoding="utf-8"
        )
        env_map = _base_env(
            self.home_dir,
            hostname="dev.corp.google.com",
            extra={
                "FUCHSIA_FX_ITERATIVE": "1",
                "FUCHSIA_FX_TEST_RUN": "0",
                "FUCHSIA_FX_MULTI_RUN": "1",
                "FUCHSIA_FX_INVOKER": "cli",
            },
        )
        cfg_lvl2 = metrics.MetricsConfig(
            level=2,
            enabled=1,
            uuid="u2",
            debug_log_file="",
            other_uuid="",
            is_internal=True,
        )
        ev = metrics.build_build_event(
            self.fuchsia_dir,
            cfg_lvl2,
            10_000,
            25_000,
            0,
            f"-j 64 {self.fuchsia_dir}",
            "//src:all",
            build_dir,
            1,
            0,
            "false",
            env=env_map,
        )
        self.assertEqual(ev["name"], "build")
        bp = ev["params"]
        self.assertEqual(bp["ninja_switches"], "-j 64 $FUCHSIA_DIR")
        self.assertEqual(bp["fuchsia_targets"], "//src:all")
        self.assertEqual(bp["main_pb_label"], "//products:core")
        self.assertEqual(bp["legacy_flag_count"], 2)
        self.assertEqual(bp["env_flags"], "+iterative-test+multi-quiet")
        self.assertEqual(bp["target_count"], 0)
        self.assertEqual(bp["is_clean_build"], 0)
        self.assertIn('"b":"x64"', bp["args_json1"])
        self.assertGreater(bp["args_json_size"], 0)

        cfg_lvl1 = dataclasses.replace(cfg_lvl2, level=1)
        ev1 = metrics.build_build_event(
            self.fuchsia_dir,
            cfg_lvl1,
            10_000,
            25_000,
            0,
            "-j 64",
            "//src:all",
            build_dir,
            1,
            1,
            "true",
            env=env_map,
        )
        self.assertEqual(ev1["params"]["ninja_switches"], "")
        self.assertEqual(ev1["params"]["fuchsia_targets"], "")
        self.assertEqual(ev1["params"]["args_json1"], "")
        self.assertIn("+quiet", ev1["params"]["env_flags"])

        # Empty or corrupt args.json keeps args_json1 empty at level 2
        for bad_content in ("", "{invalid json"):
            (build_dir / "args.json").write_text(bad_content, encoding="utf-8")
            ev_bad = metrics.build_build_event(
                self.fuchsia_dir,
                cfg_lvl2,
                10_000,
                25_000,
                0,
                "",
                "",
                build_dir,
                1,
                0,
                "false",
                env=env_map,
            )
            self.assertEqual(ev_bad["params"]["args_json1"], "")
            self.assertEqual(ev_bad["params"]["args_json2"], "")
        (build_dir / "args.json").write_text(
            json.dumps(args_payload), encoding="utf-8"
        )

        # Non-noop build counts actions via count-ninja-actions.py with
        # bytecode suppression (-B -S and PYTHONDONTWRITEBYTECODE=1).
        counter_script = (
            self.fuchsia_dir
            / "tools"
            / "devshell"
            / "contrib"
            / "lib"
            / "count-ninja-actions.py"
        )
        counter_script.parent.mkdir(parents=True, exist_ok=True)
        counter_script.write_text(
            "import os, sys\n"
            "assert sys.flags.dont_write_bytecode == 1\n"
            "assert sys.flags.no_site == 1\n"
            "assert os.environ.get('PYTHONDONTWRITEBYTECODE') == '1'\n"
            "print(7)\n",
            encoding="utf-8",
        )
        ev_actions = metrics.build_build_event(
            self.fuchsia_dir,
            cfg_lvl1,
            10_000,
            25_000,
            0,
            "",
            "",
            build_dir,
            0,
            "false",
            "false",
            env=test_utils.isolated_python_env(env_map),
        )
        self.assertEqual(ev_actions["params"]["target_count"], 7)
        self.assertEqual(ev_actions["params"]["is_clean_build"], 0)

        # CLI parser accepts boolean strings for --is-clean-build (matching jq --argjson).
        parser = metrics._build_parser()
        ns_false = parser.parse_args(
            [
                "track-build-event",
                f"--fuchsia-dir={self.fuchsia_dir}",
                "--start-time-micros=1000",
                "--end-time-micros=2000",
                "--exit-status=0",
                f"--build-dir={build_dir}",
                "--is-no-op=0",
                "--is-clean-build=false",
            ]
        )
        self.assertEqual(ns_false.is_clean_build, 0)
        ns_true = parser.parse_args(
            [
                "track-build-event",
                f"--fuchsia-dir={self.fuchsia_dir}",
                "--start-time-micros=1000",
                "--end-time-micros=2000",
                "--exit-status=0",
                f"--build-dir={build_dir}",
                "--is-no-op=0",
                "--is-clean-build=true",
            ]
        )
        self.assertEqual(ns_true.is_clean_build, 1)

    def test_build_test_custom_and_new_user_events(self) -> None:
        build_dir = self.fuchsia_dir / "out" / "default"
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "args.json").write_text(
            json.dumps(
                {
                    "build_info_product": "test_product",
                    "build_info_board": "test_board",
                    "main_pb_label": "//products:bundle",
                }
            ),
            encoding="utf-8",
        )
        stats = json.dumps(
            {"t": 1000000, "a": 0, "sel": "--package foo --and --component bar"}
        )
        test_ev = metrics.build_test_event(
            1_000_000, 2_000_000, "0", "0", stats, build_dir
        )
        self.assertEqual(test_ev["name"], "test")
        tp = test_ev["params"]
        self.assertEqual(tp["product"], "test_product")
        self.assertEqual(tp["board"], "test_board")
        self.assertEqual(tp["main_product_bundle"], "//products:bundle")
        self.assertEqual(
            tp["test_selection"], "--package foo --and --component bar"
        )
        self.assertEqual(tp["stats_json1"], '{"t":1000000,"a":0}')
        self.assertNotIn("sel", tp["stats_json1"])
        self.assertEqual(tp["stats_json2"], "")

        zero_sel_ev = metrics.build_test_event(
            1_000_000,
            2_000_000,
            "0",
            "0",
            json.dumps({"t": 1, "sel": 0}),
            build_dir,
        )
        self.assertEqual(zero_sel_ev["params"]["test_selection"], "0")
        false_sel_ev = metrics.build_test_event(
            1_000_000,
            2_000_000,
            "0",
            "0",
            json.dumps({"t": 1, "sel": False}),
            build_dir,
        )
        self.assertEqual(false_sel_ev["params"]["test_selection"], "")

        long_raw = ("A" * 500) + ("B" * 200)
        long_ev = metrics.build_test_event(
            1_000_000, 2_000_000, "130", "1", long_raw, build_dir
        )
        self.assertEqual(long_ev["params"]["was_aborted"], "1")
        self.assertEqual(long_ev["params"]["stats_json1"], "A" * 500)
        self.assertEqual(long_ev["params"]["stats_json2"], "B" * 200)

        long_label_ev = metrics.build_custom_event(
            "worktree", "daily_snapshot", "L" * 600
        )
        assert long_label_ev is not None
        self.assertEqual(long_label_ev["params"]["label"], "L" * 500)
        self.assertIsNotNone(
            metrics.build_custom_event("test", "myaction", "mylabel")
        )
        self.assertIsNone(
            metrics.build_custom_event("build", "myaction", "mylabel")
        )

        new_user_payload = metrics.build_new_user_payload(
            "shared-uuid-123",
            self.fuchsia_dir,
            env=_base_env(self.home_dir, hostname="ws.corp.google.com"),
        )
        self.assertEqual(new_user_payload["client_id"], "new_user")
        self.assertEqual(
            new_user_payload["user_properties"]["other_uuid"]["value"],
            "new_user",
        )
        self.assertEqual(new_user_payload["events"][0]["name"], "new_user")
        self.assertEqual(
            new_user_payload["events"][0]["params"]["uuid"], "shared-uuid-123"
        )

    @mock.patch.object(metrics, "subprocess", autospec=True, spec_set=True)
    def test_send_events_compact_json_debug_curl_and_batching(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        log_path = self.root_dir / "send.log"
        env_map = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        cfg_debug = metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="u-debug",
            debug_log_file=str(log_path),
            other_uuid="other-u",
            is_internal=False,
            debug_mode=True,
            use_validation_server=False,
        )

        # Should skip mocked or empty events
        metrics.send_events(
            self.fuchsia_dir,
            dataclasses.replace(cfg_debug, is_mocked=True),
            ({"name": "invoke", "params": {}, "timestamp_micros": 1},),
            env=env_map,
        )
        self.assertFalse(log_path.exists())

        # 26 events split into 25 + 1 batches; debug_mode skips curl -> 202
        batch_events = tuple(
            {
                "name": "feature",
                "params": {"feature": f"f{i}", "status": "enabled"},
                "timestamp_micros": 100 + i,
            }
            for i in range(26)
        )
        metrics.send_events(
            self.fuchsia_dir, cfg_debug, batch_events, env=env_map
        )
        mock_subprocess.run.assert_not_called()
        lines = log_path.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 2)
        payloads: list[dict[str, object]] = []
        for line in lines:
            match = re.fullmatch(r"\d{8}_\d{6}: (.+) RESULT=202", line)
            self.assertIsNotNone(match, msg=f"unexpected log line: {line}")
            assert match is not None
            json_part = match.group(1)
            self.assertNotIn(": ", json_part)
            self.assertNotIn(", ", json_part)
            payloads.append(json.loads(json_part))
        first_payload = payloads[0]
        second_payload = payloads[1]
        self.assertIsInstance(first_payload["events"], list)
        self.assertIsInstance(second_payload["events"], list)
        assert isinstance(first_payload["events"], list)
        assert isinstance(second_payload["events"], list)
        self.assertEqual(len(first_payload["events"]), 25)
        self.assertEqual(len(second_payload["events"]), 1)

        # Validation server invokes curl against debug/mp/collect
        mock_subprocess.run.return_value = subprocess.CompletedProcess(
            args=(), returncode=0, stdout="200", stderr=""
        )
        cfg_val = dataclasses.replace(cfg_debug, use_validation_server=True)
        metrics.send_events(
            self.fuchsia_dir,
            cfg_val,
            batch_events[:1],
            env=env_map,
        )
        mock_subprocess.run.assert_called_once()
        curl_cmd = mock_subprocess.run.call_args.args[0]
        self.assertEqual(curl_cmd[0], "curl")
        self.assertTrue(
            any(
                f"https://www.google-analytics.com/debug/mp/collect?{_GA4_QUERY_FRAGMENT}"
                in arg
                for arg in curl_cmd
            )
        )

        # Non-debug invokes curl against mp/collect
        mock_subprocess.run.reset_mock()
        cfg_prod = dataclasses.replace(
            cfg_debug, debug_mode=False, use_validation_server=False
        )
        metrics.send_events(
            self.fuchsia_dir,
            cfg_prod,
            batch_events[:1],
            env=env_map,
        )
        mock_subprocess.run.assert_called_once()
        prod_cmd = mock_subprocess.run.call_args.args[0]
        self.assertTrue(
            any(
                f"https://www.google-analytics.com/mp/collect?{_GA4_QUERY_FRAGMENT}"
                in arg
                for arg in prod_cmd
            )
        )

    @mock.patch.object(metrics, "subprocess", autospec=True, spec_set=True)
    def test_spawn_command_start_and_finish_metrics(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        cfg_off = metrics.MetricsConfig(
            level=0,
            enabled=0,
            uuid="",
            debug_log_file="",
            other_uuid="",
            is_internal=False,
        )
        self.assertIsNone(
            metrics.spawn_command_start_metrics(
                self.fuchsia_dir, "get-src-dir", (), cfg_off
            )
        )
        self.assertIsNone(
            metrics.spawn_command_finish_metrics(
                self.fuchsia_dir, "get-src-dir", (), 100, 200, 0, cfg_off
            )
        )
        mock_subprocess.Popen.assert_not_called()

        cfg_on = dataclasses.replace(
            cfg_off, level=1, enabled=1, uuid="u-spawn"
        )
        mock_subprocess.DEVNULL = subprocess.DEVNULL

        fake_start = mock.create_autospec(subprocess.Popen, instance=True)
        fake_start._child_created = True
        mock_subprocess.Popen.return_value = fake_start
        start_proc = metrics.spawn_command_start_metrics(
            self.fuchsia_dir,
            "vendor",
            ("google", "flash-kola", "--fast"),
            cfg_on,
        )
        self.assertIs(start_proc, fake_start)
        start_cmd = mock_subprocess.Popen.call_args.args[0]
        start_kwargs = mock_subprocess.Popen.call_args.kwargs
        self.assertIn("-B", start_cmd)
        self.assertIn("-S", start_cmd)
        self.assertEqual(
            start_kwargs["env"].get("PYTHONDONTWRITEBYTECODE"), "1"
        )
        self.assertIn("report-start", start_cmd)
        self.assertTrue(start_kwargs.get("start_new_session"))
        self.assertTrue(start_kwargs.get("close_fds"))
        self.assertEqual(start_kwargs.get("stdin"), subprocess.DEVNULL)
        self.assertEqual(start_kwargs.get("stdout"), subprocess.DEVNULL)
        self.assertEqual(start_kwargs.get("stderr"), subprocess.DEVNULL)
        self.assertFalse(fake_start._child_created)
        fake_start.wait.assert_not_called()

        mock_subprocess.Popen.reset_mock()
        fake_finish = mock.create_autospec(subprocess.Popen, instance=True)
        fake_finish._child_created = True
        mock_subprocess.Popen.return_value = fake_finish
        finish_proc = metrics.spawn_command_finish_metrics(
            self.fuchsia_dir,
            "get-src-dir",
            (),
            1_000_000,
            1_050_000,
            0,
            cfg_on,
        )
        self.assertIs(finish_proc, fake_finish)
        finish_cmd = mock_subprocess.Popen.call_args.args[0]
        finish_kwargs = mock_subprocess.Popen.call_args.kwargs
        self.assertIn("-B", finish_cmd)
        self.assertIn("-S", finish_cmd)
        self.assertEqual(
            finish_kwargs["env"].get("PYTHONDONTWRITEBYTECODE"), "1"
        )
        self.assertIn("track-command-finished", finish_cmd)
        self.assertIn("--subcommand", finish_cmd)
        self.assertTrue(finish_kwargs.get("start_new_session"))
        self.assertFalse(fake_finish._child_created)
        fake_finish.wait.assert_not_called()

        with mock.patch.object(
            metrics,
            "_resolve_metrics_script_path",
            return_value=self.root_dir / "metrics.pyz" / "metrics.py",
        ):
            archive_cmd, archive_env = metrics._metrics_subprocess_cmd_and_env(
                ("report-start",), {"HOME": str(self.home_dir)}
            )
        self.assertEqual(
            archive_cmd[:5], (sys.executable, "-B", "-S", "-m", "metrics")
        )
        self.assertEqual(archive_env.get("PYTHONDONTWRITEBYTECODE"), "1")

    def test_spawn_start_and_finish_end_to_end(self) -> None:
        log_path = self.root_dir / "spawn_e2e.log"
        pycache_dir = self.root_dir / "metrics_pycache"
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="e2e-uuid"\n'
            f"_METRICS_DEBUG_LOG_FILE={log_path}\n"
        )
        with mock.patch.dict(
            os.environ,
            {
                "HOME": str(self.home_dir),
                "HOSTNAME": "ext.example.com",
                "XDG_DATA_HOME": str(self.xdg_dir),
                "PYTHONPYCACHEPREFIX": str(pycache_dir),
                "_METRICS_DEBUG": "1",
                "_METRICS_DEBUG_LOG_FILE": str(log_path),
                "FUCHSIA_METRICS_CALL_STACK": "fx",
            },
            clear=False,
        ):
            os.environ.pop("PYTHONDONTWRITEBYTECODE", None)
            cfg = metrics.init_command_metrics(
                self.fuchsia_dir,
                "set",
                ("core.x64", "--with", "//pkg:a"),
                stderr=io.StringIO(),
            )
            start_proc = metrics.spawn_command_start_metrics(
                self.fuchsia_dir,
                "set",
                ("core.x64", "--with", "//pkg:a"),
                cfg,
            )
            finish_proc = metrics.spawn_command_finish_metrics(
                self.fuchsia_dir,
                "set",
                ("core.x64", "--with", "//pkg:a"),
                1_000_000,
                1_050_000,
                0,
                cfg,
            )
            self.assertIsNotNone(start_proc)
            self.assertIsNotNone(finish_proc)
            assert start_proc is not None
            assert finish_proc is not None
            self.assertFalse(getattr(start_proc, "_child_created"))
            self.assertFalse(getattr(finish_proc, "_child_created"))
            self.assertEqual(start_proc.wait(), 0)
            self.assertEqual(finish_proc.wait(), 0)
        self.assertFalse(pycache_dir.exists())
        log_text = log_path.read_text(encoding="utf-8")
        self.assertIn('"name":"feature"', log_text)
        self.assertIn('"name":"set"', log_text)
        self.assertIn('"name":"invoke"', log_text)
        self.assertIn('"name":"finish"', log_text)
        self.assertIn('"timing":50', log_text)
        self.assertIn(" RESULT=202", log_text)

    def test_disabled_read_config_skips_other_uuid_disk_io(self) -> None:
        self._write_fx_metrics_cfg('METRICS_ENABLED=0\nMETRICS_UUID=""\n')

        env_ext = _base_env(self.home_dir, xdg_data_home=self.xdg_dir)
        env_int = _base_env(
            self.home_dir,
            hostname="box.corp.google.com",
            xdg_data_home=self.xdg_dir,
        )
        with mock.patch.object(
            metrics,
            "_read_other_uuid",
            wraps=metrics._read_other_uuid,
        ) as spy_read_other:
            cfg_ext, ok_ext = metrics.read_config(self.fuchsia_dir, env=env_ext)
            cfg_int, ok_int = metrics.read_config(self.fuchsia_dir, env=env_int)
        self.assertTrue(ok_ext)
        self.assertTrue(ok_int)
        self.assertEqual(cfg_ext.level, 0)
        self.assertEqual(cfg_int.level, 0)
        spy_read_other.assert_not_called()

    def test_subprocess_timeouts_for_curl_and_count_ninja_actions(self) -> None:
        log_path = self.root_dir / "curl_timeout.log"
        payload = {"client_id": "u", "events": [], "user_properties": {}}

        with mock.patch.object(
            metrics.subprocess,
            "run",
            autospec=True,
            spec_set=True,
            side_effect=subprocess.TimeoutExpired(cmd=("curl",), timeout=20.0),
        ) as mock_run:
            metrics._post_payload(
                payload,
                debug_mode=False,
                use_validation_server=False,
                debug_log_file=str(log_path),
                env_map={"HOME": str(self.home_dir)},
            )
        curl_cmd = mock_run.call_args.args[0]
        curl_kwargs = mock_run.call_args.kwargs
        self.assertIn("--connect-timeout", curl_cmd)
        self.assertIn("--max-time", curl_cmd)
        self.assertIsNotNone(curl_kwargs.get("timeout"))
        self.assertIn("RESULT=\n", log_path.read_text(encoding="utf-8"))

        build_dir = self.fuchsia_dir / "out" / "timeout_build"
        build_dir.mkdir(parents=True, exist_ok=True)
        counter_script = (
            self.fuchsia_dir
            / "tools"
            / "devshell"
            / "contrib"
            / "lib"
            / "count-ninja-actions.py"
        )
        counter_script.parent.mkdir(parents=True, exist_ok=True)
        counter_script.write_text("print(99)\n", encoding="utf-8")
        cfg_on = metrics.MetricsConfig(
            level=1,
            enabled=1,
            uuid="u-timeout",
            debug_log_file="",
            other_uuid="",
            is_internal=True,
        )
        with mock.patch.object(
            metrics.subprocess,
            "run",
            autospec=True,
            spec_set=True,
            side_effect=subprocess.TimeoutExpired(
                cmd=(sys.executable,), timeout=15.0
            ),
        ) as mock_counter_run:
            ev = metrics.build_build_event(
                self.fuchsia_dir,
                cfg_on,
                1000,
                2000,
                0,
                "",
                "",
                build_dir,
                0,
                0,
                "false",
                env={"HOME": str(self.home_dir)},
            )
        self.assertIsNotNone(mock_counter_run.call_args.kwargs.get("timeout"))
        self.assertEqual(ev["params"]["target_count"], 0)

    def test_send_new_user_atomic_uuid_creation_and_race_recovery(
        self,
    ) -> None:
        analytics_dir = self._internal_dir()
        uuid_path = analytics_dir / "uuid"
        log_path = self.root_dir / "new_user_atomic.log"
        base_env = _base_env(
            self.home_dir,
            hostname="box.corp.google.com",
            xdg_data_home=self.xdg_dir,
            extra={
                "_METRICS_TESTING_DEBUG_LOG_FILE": str(log_path),
            },
        )

        # Case 1: clean creation uses atomic link, leaves 0o600 file and no temp files
        with mock.patch.dict(os.environ, base_env, clear=True):
            rc = metrics.main(
                ["send-new-user", "--fuchsia-dir", str(self.fuchsia_dir)]
            )
        self.assertEqual(rc, 0)
        self.assertTrue(uuid_path.is_file())
        created_uuid = uuid_path.read_text(encoding="utf-8").strip()
        self.assertEqual(
            stat.S_IMODE(uuid_path.stat().st_mode),
            0o600,
        )
        self.assertEqual(list(analytics_dir.glob(".uuid_*")), [])
        self.assertIn(created_uuid, log_path.read_text(encoding="utf-8"))

        # Case 2: concurrent race where another process wins creation of uuid
        uuid_path.unlink()
        log_path.unlink()
        winning_uuid = "WINNING-UUID-1234"

        def _racy_link(src: os.PathLike[str], dst: os.PathLike[str]) -> None:
            del src
            pathlib.Path(dst).write_text(f"{winning_uuid}\n", encoding="utf-8")
            raise FileExistsError("already created by peer")

        with mock.patch.dict(os.environ, base_env, clear=True):
            with mock.patch.object(
                metrics.os,
                "link",
                autospec=True,
                spec_set=True,
                side_effect=_racy_link,
            ):
                rc_race = metrics.main(
                    ["send-new-user", "--fuchsia-dir", str(self.fuchsia_dir)]
                )
        self.assertEqual(rc_race, 0)
        self.assertEqual(
            uuid_path.read_text(encoding="utf-8").strip(), winning_uuid
        )
        self.assertEqual(list(analytics_dir.glob(".uuid_*")), [])
        race_log = log_path.read_text(encoding="utf-8")
        self.assertIn(f'"uuid":"{winning_uuid}"', race_log)
        self.assertIn(" RESULT=202", race_log)

    def test_testing_debug_log_file_does_not_force_enable_metrics(
        self,
    ) -> None:
        log_path = self.root_dir / "testing_non_promoting.log"
        self._write_fx_metrics_cfg('METRICS_ENABLED=0\nMETRICS_UUID=""\n')
        env_test = _base_env(
            self.home_dir,
            xdg_data_home=self.xdg_dir,
            extra={"_METRICS_TESTING_DEBUG_LOG_FILE": str(log_path)},
        )

        cfg_read, ok_read = metrics.read_config(self.fuchsia_dir, env=env_test)
        self.assertTrue(ok_read)
        self.assertEqual(cfg_read.level, 0)
        self.assertEqual(cfg_read.enabled, 0)
        self.assertFalse(cfg_read.should_report)
        self.assertTrue(cfg_read.debug_mode)
        self.assertEqual(cfg_read.debug_log_file, str(log_path))

        cfg_resolved = metrics._resolve_reporting_config(
            self.fuchsia_dir, env_test
        )
        self.assertEqual(cfg_resolved.level, 0)
        self.assertFalse(cfg_resolved.should_report)

        cfg_init = metrics.init_command_metrics(
            self.fuchsia_dir,
            "build",
            (),
            stderr=io.StringIO(),
            env=env_test,
        )
        self.assertEqual(cfg_init.level, 0)
        self.assertEqual(cfg_init.enabled, 0)
        self.assertFalse(cfg_init.should_report)

        # When enabled on disk, _METRICS_TESTING_DEBUG_LOG_FILE still redirects
        # logging and keeps debug_mode=True without live network POSTs.
        self._write_fx_metrics_cfg(
            'METRICS_ENABLED=1\nMETRICS_UUID="on-disk-uuid"\n'
        )
        cfg_on = metrics._resolve_reporting_config(self.fuchsia_dir, env_test)
        self.assertEqual(cfg_on.level, 1)
        self.assertEqual(cfg_on.uuid, "on-disk-uuid")
        self.assertTrue(cfg_on.should_report)
        self.assertTrue(cfg_on.debug_mode)
        self.assertEqual(cfg_on.debug_log_file, str(log_path))


if __name__ == "__main__":
    unittest.main()
