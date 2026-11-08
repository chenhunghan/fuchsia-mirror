# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import locator


class LocatorTests(unittest.TestCase):
    def _run_locator(
        self, args: list[str], fuchsia_dir: Path
    ) -> subprocess.CompletedProcess[str]:
        """Run locator.main in-process and capture stdout, stderr, and exit code."""
        out_buf, err_buf = io.StringIO(), io.StringIO()
        full_args = ["--fuchsia-dir", str(fuchsia_dir)] + args
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
        with (
            mock.patch.dict(os.environ, env, clear=True),
            contextlib.redirect_stdout(out_buf),
            contextlib.redirect_stderr(err_buf),
        ):
            try:
                rc = locator.main(full_args)
            except SystemExit as exc:
                rc = exc.code if isinstance(exc.code, int) else 1
        return subprocess.CompletedProcess(
            full_args, rc, out_buf.getvalue(), err_buf.getvalue()
        )

    def _write_exec(
        self, path: Path, body: str = "#!/bin/sh\necho ok\n"
    ) -> Path:
        """Create an executable script file at path."""
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)
        return path

    def _write_registry(self, path: Path, entries: dict[str, Any]) -> Path:
        """Write an fx_host_tools.json file at path."""
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(entries, indent=4, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return path

    def test_compute_isolated_path(self) -> None:
        """Verify PATH isolation strips prebuilt/runtime dirs unless inject_prebuilt_paths=1."""
        current_path = "/parent.runtime_dir/.bin:/parent.runtime_dir/bin:/prebuilt/python3/bin:/usr/bin:/bin"
        prebuilts = "/prebuilt/python3/bin:/prebuilt/clang/bin"

        # Default (inject=0): strip parent runtime_dir bins and all prebuilt dirs.
        isolated = locator.compute_isolated_path(
            inject_prebuilt_paths=0,
            current_path=current_path,
            prebuilt_all_paths=prebuilts,
            saved_prebuilt_all_paths=prebuilts,
        )
        self.assertEqual(isolated, "/usr/bin:/bin")

        # Opt-in (inject=1): prepend prebuilt dirs ahead of ambient PATH segments.
        restored = locator.compute_isolated_path(
            inject_prebuilt_paths=1,
            current_path=isolated,
            prebuilt_all_paths=prebuilts,
            saved_prebuilt_all_paths=prebuilts,
        )
        self.assertEqual(
            restored, "/prebuilt/python3/bin:/prebuilt/clang/bin:/usr/bin:/bin"
        )

    def test_resolve_4_level_vendor_first_precedence(self) -> None:
        """Verify 5-level vendor-first command resolution precedence and multi-vendor collision handling."""
        with tempfile.TemporaryDirectory() as td:
            fuchsia_dir = Path(td)
            build_dir = fuchsia_dir / "out" / "default"

            # Level 5: Unregistered binary in <build_dir>/host-tools resolves last.
            host_only = self._write_exec(build_dir / "host-tools" / "host-only")
            res5 = locator.resolve_command(
                fuchsia_dir, "host-only", build_dir=build_dir
            )
            self.assertEqual(res5.kind, locator.CommandKind.LEGACY_FX_HOST_TOOL)
            self.assertEqual(res5.script, str(host_only))

            # Level 4: Core legacy script in //tools/devshell.
            core_legacy = self._write_exec(
                fuchsia_dir / "tools" / "devshell" / "foo"
            )
            res4 = locator.resolve_command(fuchsia_dir, "foo")
            self.assertEqual(res4.kind, locator.CommandKind.LEGACY_SCRIPT)
            self.assertEqual(res4.exec_path, str(core_legacy))

            # Level 3: Core registered tool overrides core legacy script.
            self._write_registry(
                fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json",
                {
                    "foo": {
                        "category": "Build",
                        "summary": "core registered foo",
                        "script": "//tools/devshell/foo_script.sh",
                    }
                },
            )
            res3 = locator.resolve_command(fuchsia_dir, "foo")
            self.assertEqual(res3.kind, locator.CommandKind.REGISTERED_SCRIPT)
            self.assertEqual(
                res3.exec_path,
                str(fuchsia_dir / "tools/devshell/foo_script.sh"),
            )

            # Level 2: Vendor legacy script overrides core registered tool.
            vendor_legacy = self._write_exec(
                fuchsia_dir
                / "vendor"
                / "google"
                / "scripts"
                / "devshell"
                / "foo"
            )
            res2 = locator.resolve_command(fuchsia_dir, "foo")
            self.assertEqual(res2.kind, locator.CommandKind.LEGACY_SCRIPT)
            self.assertEqual(res2.exec_path, str(vendor_legacy))

            # Level 1: Vendor registered tool overrides vendor legacy script.
            self._write_registry(
                fuchsia_dir
                / "vendor"
                / "google"
                / "scripts"
                / "devshell"
                / "fx_host_tools.json",
                {
                    "foo": {
                        "category": "Build",
                        "summary": "vendor registered foo",
                        "script": "//vendor/google/scripts/devshell/vendor_foo.sh",
                    }
                },
            )
            res1 = locator.resolve_command(fuchsia_dir, "foo")
            self.assertEqual(res1.kind, locator.CommandKind.REGISTERED_SCRIPT)
            self.assertEqual(
                res1.exec_path,
                str(
                    fuchsia_dir / "vendor/google/scripts/devshell/vendor_foo.sh"
                ),
            )

            # Explicit --vendor restriction scopes lookup to that vendor only.
            res_v = locator.resolve_command(
                fuchsia_dir, "foo", vendor_name="google"
            )
            self.assertEqual(res_v.kind, locator.CommandKind.REGISTERED_SCRIPT)
            res_other_v = locator.resolve_command(
                fuchsia_dir, "foo", vendor_name="other"
            )
            self.assertEqual(res_other_v.kind, locator.CommandKind.NOT_FOUND)

            # Multi-vendor collision: >=2 vendor registries defining 'foo' exits with code 2.
            self._write_registry(
                fuchsia_dir
                / "vendor"
                / "acme"
                / "scripts"
                / "devshell"
                / "fx_host_tools.json",
                {
                    "foo": {
                        "category": "Build",
                        "summary": "acme registered foo",
                        "script": "//vendor/acme/scripts/devshell/acme_foo.sh",
                    }
                },
            )
            proc_collision = self._run_locator(["resolve", "foo"], fuchsia_dir)
            self.assertEqual(proc_collision.returncode, 2)
            self.assertIn("multiple vendor repositories", proc_collision.stderr)
            self.assertIn("acme", proc_collision.stderr)
            self.assertIn("google", proc_collision.stderr)

            # Explicit vendor disambiguation resolves cleanly.
            proc_acme = self._run_locator(
                ["resolve", "foo", "--vendor", "acme"], fuchsia_dir
            )
            self.assertEqual(proc_acme.returncode, 0, msg=proc_acme.stderr)
            self.assertIn("KIND=registered_script", proc_acme.stdout)
            self.assertIn("VENDOR=acme", proc_acme.stdout)

    def test_help_command(self) -> None:
        """Verify help-command executes --help when present on disk and falls back to metadata otherwise."""
        with tempfile.TemporaryDirectory() as td:
            fuchsia_dir = Path(td)
            self._write_registry(
                fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json",
                {
                    "live-script": {
                        "category": "Testing",
                        "summary": "Script with on-disk executable",
                        "script": "//tools/devshell/live_runner.sh",
                    },
                    "my-cmd": {
                        "category": "Testing",
                        "summary": "Run custom test suite",
                        "script": "//tools/devshell/test_runner.sh",
                    },
                },
            )
            self._write_exec(
                fuchsia_dir / "tools" / "devshell" / "live_runner.sh",
                "#!/bin/sh\necho 'usage: fx live-script [--flag]'\n",
            )

            # Script missing on disk falls back to printing registry metadata.
            proc = self._run_locator(["help-command", "my-cmd"], fuchsia_dir)
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertIn("Run custom test suite", proc.stdout)
            self.assertIn("Category: Testing", proc.stdout)
            self.assertIn(
                "Registry: script //tools/devshell/test_runner.sh", proc.stdout
            )

            # Script present on disk runs `--help` directly.
            proc_live = self._run_locator(
                ["help-command", "live-script"], fuchsia_dir
            )
            self.assertEqual(proc_live.returncode, 0, msg=proc_live.stderr)
            self.assertIn("usage: fx live-script [--flag]", proc_live.stdout)

            # Unregistered command returns exit code 1.
            proc_not_found = self._run_locator(
                ["help-command", "unknown-cmd"], fuchsia_dir
            )
            self.assertEqual(proc_not_found.returncode, 1)

            # Target command staged in out/_fx_host_tools runs --help even without .fx-build-dir or --build-dir
            self._write_registry(
                fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json",
                {
                    "staged-bin": {
                        "category": "Build",
                        "summary": "Staged target binary",
                        "target": "//tools/devshell:staged_bin",
                    },
                },
            )
            self._write_exec(
                fuchsia_dir / "out" / "_fx_host_tools" / "staged-bin",
                "#!/bin/sh\necho 'usage: fx staged-bin [--help]'\n",
            )
            proc_staged = self._run_locator(
                ["help-command", "staged-bin"], fuchsia_dir
            )
            self.assertEqual(proc_staged.returncode, 0, msg=proc_staged.stderr)
            self.assertIn("usage: fx staged-bin [--help]", proc_staged.stdout)

            # resolve strips leading // from --build-dir when resolving relative to fuchsia_dir
            self._write_exec(
                fuchsia_dir
                / "out"
                / "slashed"
                / "host-tools"
                / "legacy-slashed-bin",
                "#!/bin/sh\necho ok\n",
            )
            proc_slashed = self._run_locator(
                [
                    "resolve",
                    "legacy-slashed-bin",
                    "--build-dir",
                    "//out/slashed",
                ],
                fuchsia_dir,
            )
            self.assertEqual(
                proc_slashed.returncode, 0, msg=proc_slashed.stderr
            )
            self.assertIn("KIND=legacy_fx_host_tool", proc_slashed.stdout)
            self.assertIn("MODE=legacy_host_tool", proc_slashed.stdout)


if __name__ == "__main__":
    unittest.main()
