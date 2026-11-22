# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for fx_cli.fx_builtins."""

import dataclasses
import io
import json
import os
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import fx_builtins
import metadata


class BuiltinsTest(unittest.TestCase):
    def setUp(self) -> None:
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
                    "registry": {
                        "category": "Build",
                        "summary": "query the fx host tools registry",
                        "script": "//tools/devshell/fx_cli/registry.py",
                    },
                }
            ),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_get_src_dir(self) -> None:
        stdout_buf = io.StringIO()
        rc = fx_builtins.run_get_src_dir(
            self.fuchsia_dir, [], stdout=stdout_buf
        )
        self.assertEqual(rc, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(self.fuchsia_dir))

    def test_get_build_dir(self) -> None:
        # Missing build dir fails
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {"_FX_BUILD_DIR": "", "FUCHSIA_BUILD_DIR": ""},
            clear=False,
        ):
            rc = fx_builtins.run_get_build_dir(
                self.fuchsia_dir, [], stdout=stdout_buf, stderr=stderr_buf
            )
        self.assertNotEqual(rc, 0)

        # Missing fx.config and args.gn fails even when directory exists
        build_dir = self.fuchsia_dir / "out" / "default"
        build_dir.mkdir(parents=True, exist_ok=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "out/default\n", encoding="utf-8"
        )
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {"_FX_BUILD_DIR": "", "FUCHSIA_BUILD_DIR": ""},
            clear=False,
        ):
            rc = fx_builtins.run_get_build_dir(
                self.fuchsia_dir, [], stdout=stdout_buf, stderr=stderr_buf
            )
        self.assertEqual(rc, 1)
        self.assertIn(
            f"ERROR: Build directory missing or removed. ({build_dir})",
            stderr_buf.getvalue(),
        )

        # Configured via .fx-build-dir with args.gn present
        (build_dir / "args.gn").write_text("", encoding="utf-8")
        stdout_buf = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {"_FX_BUILD_DIR": "", "FUCHSIA_BUILD_DIR": ""},
            clear=False,
        ):
            rc = fx_builtins.run_get_build_dir(
                self.fuchsia_dir, [], stdout=stdout_buf, stderr=stderr_buf
            )
        self.assertEqual(rc, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(build_dir))

        # FUCHSIA_BUILD_DIR takes precedence over .fx-build-dir (including out-of-tree paths)
        env_dir = self.fuchsia_dir / "out" / "from_env"
        env_dir.mkdir(parents=True, exist_ok=True)
        (env_dir / "fx.config").write_text("", encoding="utf-8")
        stdout_buf = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {"_FX_BUILD_DIR": "", "FUCHSIA_BUILD_DIR": "out/from_env"},
            clear=False,
        ):
            rc = fx_builtins.run_get_build_dir(
                self.fuchsia_dir, [], stdout=stdout_buf, stderr=stderr_buf
            )
        self.assertEqual(rc, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(env_dir))

        # Overridden via build_dir_override (--dir)
        override_dir = self.fuchsia_dir / "out" / "custom"
        override_dir.mkdir(parents=True, exist_ok=True)
        (override_dir / "args.gn").write_text("", encoding="utf-8")
        stdout_buf = io.StringIO()
        rc = fx_builtins.run_get_build_dir(
            self.fuchsia_dir,
            [],
            build_dir_override="out/custom",
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        self.assertEqual(stdout_buf.getvalue().strip(), str(override_dir))

    def test_normalize_and_resolve_build_dir(self) -> None:
        self.assertFalse(hasattr(fx_builtins, "normalize_build_dir"))
        self.assertEqual(
            metadata._normalize_build_dir(self.fuchsia_dir, "///out/custom///"),
            self.fuchsia_dir / "out" / "custom",
        )
        self.assertEqual(
            metadata._normalize_build_dir(
                self.fuchsia_dir, "  //out/foo//bar/  "
            ),
            self.fuchsia_dir / "out" / "foo" / "bar",
        )
        self.assertEqual(
            metadata._normalize_build_dir(
                self.fuchsia_dir, "out//nested///dir/"
            ),
            self.fuchsia_dir / "out" / "nested" / "dir",
        )
        self.assertEqual(
            metadata._normalize_build_dir(self.fuchsia_dir, "/abs//path///"),
            pathlib.Path("/abs/path"),
        )
        for empty_raw in ("", "   ", "/", "//", "///", "  //  "):
            self.assertIsNone(
                metadata._normalize_build_dir(self.fuchsia_dir, empty_raw),
                msg=f"expected None for {empty_raw!r}",
            )

        (self.fuchsia_dir / ".fx-build-dir").write_text(
            "  ///out/from_file///  \n", encoding="utf-8"
        )
        # Invalid/empty higher-priority candidates fall through to the next source.
        self.assertEqual(
            fx_builtins.resolve_build_dir(
                self.fuchsia_dir,
                build_dir_override="   ",
                env={"_FX_BUILD_DIR": "//", "FUCHSIA_BUILD_DIR": "/"},
            ),
            self.fuchsia_dir / "out" / "from_file",
        )
        self.assertEqual(
            fx_builtins.resolve_build_dir(
                self.fuchsia_dir,
                build_dir_override="///",
                env={
                    "_FX_BUILD_DIR": "  ",
                    "FUCHSIA_BUILD_DIR": "///out/from_env///",
                },
            ),
            self.fuchsia_dir / "out" / "from_env",
        )
        self.assertEqual(
            fx_builtins.resolve_build_dir(
                self.fuchsia_dir,
                build_dir_override="///out/override///",
                env={"_FX_BUILD_DIR": "out/ignored"},
            ),
            self.fuchsia_dir / "out" / "override",
        )

        # In-memory candidates resolve lazily without touching .fx-build-dir,
        # and a non-UTF-8 .fx-build-dir returns None cleanly on fallback.
        (self.fuchsia_dir / ".fx-build-dir").write_bytes(b"\xff\xfe\n")
        with mock.patch.object(
            pathlib.Path,
            "is_file",
            side_effect=AssertionError("unexpected .fx-build-dir stat"),
        ):
            self.assertEqual(
                fx_builtins.resolve_build_dir(
                    self.fuchsia_dir,
                    build_dir_override="out/override",
                    env={},
                ),
                self.fuchsia_dir / "out" / "override",
            )
            self.assertEqual(
                fx_builtins.resolve_build_dir(
                    self.fuchsia_dir,
                    env={"FUCHSIA_BUILD_DIR": "out/from_env"},
                ),
                self.fuchsia_dir / "out" / "from_env",
            )
        self.assertIsNone(
            fx_builtins.resolve_build_dir(self.fuchsia_dir, env={})
        )

    def test_registry_builtin_dispatch(self) -> None:
        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_builtins.run_registry(
            self.fuchsia_dir,
            ["list", "--format=names"],
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        names = stdout_buf.getvalue().splitlines()
        self.assertIn("get-src-dir", names)
        self.assertIn("registry", names)

    def test_help_builtin(self) -> None:
        script_path = (
            self.fuchsia_dir
            / "tools/devshell/host_tools/get_src_dir/get_src_dir.sh"
        )
        script_path.parent.mkdir(parents=True, exist_ok=True)
        script_path.write_text(
            "#!/bin/bash\n"
            "#### CATEGORY=Build\n"
            "### print the root of the Fuchsia source tree\n"
            "## usage: fx get-src-dir\n",
            encoding="utf-8",
        )
        script_path.chmod(0o755)

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_builtins.run_help(
            self.fuchsia_dir,
            ["get-src-dir"],
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        self.assertIn("usage: fx get-src-dir", stdout_buf.getvalue())
        self.assertIn(
            "Global fx options: fx [OPTION] get-src-dir ...",
            stdout_buf.getvalue(),
        )

        # When the script itself prints "For global options, try `fx help`.",
        # run_help should not append _format_global_options a second time.
        script_path.write_text(
            "#!/bin/bash\n"
            'echo "usage: fx get-src-dir"\n'
            'echo "For global options, try `fx help`."\n',
            encoding="utf-8",
        )
        stdout_buf2 = io.StringIO()
        rc2 = fx_builtins.run_help(
            self.fuchsia_dir,
            ["get-src-dir"],
            stdout=stdout_buf2,
            stderr=stderr_buf,
        )
        self.assertEqual(rc2, 0)
        self.assertIn("usage: fx get-src-dir", stdout_buf2.getvalue())
        self.assertNotIn(
            "Global fx options: fx [OPTION] get-src-dir ...",
            stdout_buf2.getvalue(),
        )

        # When registered script is missing on disk, prints registry metadata summary.
        script_path.unlink()
        stdout_buf3 = io.StringIO()
        rc3 = fx_builtins.run_help(
            self.fuchsia_dir,
            ["get-src-dir"],
            stdout=stdout_buf3,
            stderr=stderr_buf,
        )
        self.assertEqual(rc3, 0)
        self.assertIn(
            "print the root of the Fuchsia source tree", stdout_buf3.getvalue()
        )
        self.assertIn("Category: Build", stdout_buf3.getvalue())
        self.assertIn(
            "Registry: script //tools/devshell/host_tools/get_src_dir/get_src_dir.sh",
            stdout_buf3.getvalue(),
        )

    def test_help_legacy_script_and_unknown_and_collision(self) -> None:
        legacy_path = self.fuchsia_dir / "tools" / "devshell" / "my-legacy"
        legacy_path.write_text(
            "#!/bin/bash\n"
            "#### CATEGORY=Other\n"
            "### short summary\n"
            "## usage: fx my-legacy [--flag]\n"
            "##\n"
            "## Detailed help for my-legacy in _FX_BUILD_DIR.\n",
            encoding="utf-8",
        )
        legacy_path.chmod(0o755)

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        rc = fx_builtins.run_help(
            self.fuchsia_dir,
            ["my-legacy"],
            build_dir_override="out/custom",
            stdout=stdout_buf,
            stderr=stderr_buf,
        )
        self.assertEqual(rc, 0)
        self.assertIn("usage: fx my-legacy [--flag]", stdout_buf.getvalue())
        self.assertIn(
            "Global fx options: fx [OPTION] my-legacy ...",
            stdout_buf.getvalue(),
        )

        # Unknown command prints to stdout and returns 0 without delegating to fx-legacy
        out_unk = io.StringIO()
        err_unk = io.StringIO()
        rc_unk = fx_builtins.run_help(
            self.fuchsia_dir,
            ["nonexistent-command-xyz"],
            stdout=out_unk,
            stderr=err_unk,
        )
        self.assertEqual(rc_unk, 0)
        self.assertIn(
            "Command nonexistent-command-xyz not found. Try `fx help`",
            out_unk.getvalue(),
        )
        self.assertEqual(err_unk.getvalue(), "")

    def test_help_legacy_sibling_fx_metadata(self) -> None:
        contrib_dir = self.fuchsia_dir / "tools" / "devshell" / "contrib"
        wrapper_dir = contrib_dir / "lib" / "test"
        wrapper_dir.mkdir(parents=True, exist_ok=True)
        wrapper = wrapper_dir / "python_wrapper.sh"
        wrapper.write_text(
            "#!/bin/bash\n### Shared wrapper without doc comments\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)

        # Symlink wrapper paired with sibling .fx file containing ## doc comments.
        wrapped_cmd = contrib_dir / "wrapped-cmd"
        wrapped_cmd.symlink_to(pathlib.Path("lib/test/python_wrapper.sh"))
        (contrib_dir / "wrapped-cmd.fx").write_text(
            "#### CATEGORY=Test\n"
            "### Wrapped command summary\n"
            "## usage: fx wrapped-cmd [--verbose]\n"
            "##\n"
            "## Runs in _FX_BUILD_DIR.\n",
            encoding="utf-8",
        )
        out_wrapped = io.StringIO()
        err_wrapped = io.StringIO()
        rc_wrapped = fx_builtins.run_help(
            self.fuchsia_dir,
            ["wrapped-cmd"],
            build_dir_override="out/custom",
            stdout=out_wrapped,
            stderr=err_wrapped,
        )
        self.assertEqual(rc_wrapped, 0)
        self.assertEqual(err_wrapped.getvalue(), "")
        self.assertIn(
            "usage: fx wrapped-cmd [--verbose]", out_wrapped.getvalue()
        )
        self.assertIn(
            f"Runs in {self.fuchsia_dir / 'out' / 'custom'}.",
            out_wrapped.getvalue(),
        )
        self.assertNotIn("No help found", out_wrapped.getvalue())
        self.assertIn(
            "Global fx options: fx [OPTION] wrapped-cmd ...",
            out_wrapped.getvalue(),
        )

        # Binary executable paired with sibling .fx metadata uses the .fx docs.
        bin_cmd = contrib_dir / "bin-cmd"
        bin_cmd.write_bytes(b"\x7fELF\x00binary-tool")
        bin_cmd.chmod(0o755)
        (contrib_dir / "bin-cmd.fx").write_text(
            "#### CATEGORY=Diagnostic\n" "## usage: fx bin-cmd [--check]\n",
            encoding="utf-8",
        )
        out_bin = io.StringIO()
        err_bin = io.StringIO()
        rc_bin = fx_builtins.run_help(
            self.fuchsia_dir,
            ["bin-cmd"],
            stdout=out_bin,
            stderr=err_bin,
        )
        self.assertEqual(rc_bin, 0)
        self.assertEqual(err_bin.getvalue(), "")
        self.assertIn("usage: fx bin-cmd [--check]", out_bin.getvalue())
        self.assertNotIn("is a host tool and no metadata", out_bin.getvalue())
        self.assertIn(
            "Global fx options: fx [OPTION] bin-cmd ...",
            out_bin.getvalue(),
        )

        # Binary executable without sibling .fx reports host tool without metadata.
        bin_no_meta = contrib_dir / "bin-no-meta"
        bin_no_meta.write_bytes(b"\x7fELF\x00binary-tool")
        bin_no_meta.chmod(0o755)
        out_bin_no_meta = io.StringIO()
        rc_bin_no_meta = fx_builtins.run_help(
            self.fuchsia_dir,
            ["bin-no-meta"],
            stdout=out_bin_no_meta,
        )
        self.assertEqual(rc_bin_no_meta, 0)
        self.assertIn(
            "'//tools/devshell/contrib/bin-no-meta' is a host tool and no metadata was found. "
            "Try running `fx bin-no-meta -h`",
            out_bin_no_meta.getvalue(),
        )

        # Sibling .fx takes precedence when both <cmd> and <cmd>.fx exist.
        both_cmd = contrib_dir / "both-cmd"
        both_cmd.write_text(
            "#!/bin/bash\n## Script doc should be shadowed.\n",
            encoding="utf-8",
        )
        both_cmd.chmod(0o755)
        (contrib_dir / "both-cmd.fx").write_text(
            "## Metadata doc takes precedence.\n",
            encoding="utf-8",
        )
        out_both = io.StringIO()
        rc_both = fx_builtins.run_help(
            self.fuchsia_dir,
            ["both-cmd"],
            stdout=out_both,
        )
        self.assertEqual(rc_both, 0)
        self.assertIn("Metadata doc takes precedence.", out_both.getvalue())
        self.assertNotIn("Script doc should be shadowed.", out_both.getvalue())

        # Sibling .fx without ## lines prints "No help found" instead of wrapper docs.
        wrapper_with_docs = wrapper_dir / "documented_wrapper.sh"
        wrapper_with_docs.write_text(
            "#!/bin/bash\n## Generic wrapper doc should not leak.\n",
            encoding="utf-8",
        )
        wrapper_with_docs.chmod(0o755)
        nodoc_wrapped = contrib_dir / "nodoc-wrapped"
        nodoc_wrapped.symlink_to(pathlib.Path("lib/test/documented_wrapper.sh"))
        (contrib_dir / "nodoc-wrapped.fx").write_text(
            "#### CATEGORY=Documentation\n### Summary only\n",
            encoding="utf-8",
        )
        out_nodoc_wrapped = io.StringIO()
        rc_nodoc_wrapped = fx_builtins.run_help(
            self.fuchsia_dir,
            ["nodoc-wrapped"],
            stdout=out_nodoc_wrapped,
        )
        self.assertEqual(rc_nodoc_wrapped, 0)
        self.assertIn(
            "No help found. Try `fx nodoc-wrapped -h`",
            out_nodoc_wrapped.getvalue(),
        )
        self.assertNotIn(
            "Generic wrapper doc should not leak.",
            out_nodoc_wrapped.getvalue(),
        )

        # Vendor command paired with sibling .fx metadata uses the .fx docs.
        vendor_dir = (
            self.fuchsia_dir / "vendor" / "acme" / "scripts" / "devshell"
        )
        vendor_dir.mkdir(parents=True, exist_ok=True)
        vendor_cmd = vendor_dir / "acme-wrapped"
        vendor_cmd.write_text("#!/bin/bash\necho ok\n", encoding="utf-8")
        vendor_cmd.chmod(0o755)
        (vendor_dir / "acme-wrapped.fx").write_text(
            "## usage: fx acme-wrapped\n",
            encoding="utf-8",
        )
        out_vendor = io.StringIO()
        rc_vendor = fx_builtins.run_help(
            self.fuchsia_dir,
            ["acme-wrapped"],
            stdout=out_vendor,
        )
        self.assertEqual(rc_vendor, 0)
        self.assertIn("usage: fx acme-wrapped", out_vendor.getvalue())

        # Normal text script with no sibling .fx and no ## lines prints "No help found".
        plain_nodoc = contrib_dir / "plain-nodoc"
        plain_nodoc.write_text("#!/bin/bash\necho hi\n", encoding="utf-8")
        plain_nodoc.chmod(0o755)
        out_plain = io.StringIO()
        rc_plain = fx_builtins.run_help(
            self.fuchsia_dir,
            ["plain-nodoc"],
            stdout=out_plain,
        )
        self.assertEqual(rc_plain, 0)
        self.assertIn(
            "No help found. Try `fx plain-nodoc -h`", out_plain.getvalue()
        )
        self.assertIn(
            "Global fx options: fx [OPTION] plain-nodoc ...",
            out_plain.getvalue(),
        )

        # Standalone .fx metadata file (no sibling executable) still prints its docs.
        (contrib_dir / "only-meta.fx").write_text(
            "## usage: fx only-meta\n",
            encoding="utf-8",
        )
        out_only_meta = io.StringIO()
        rc_only_meta = fx_builtins.run_help(
            self.fuchsia_dir,
            ["only-meta"],
            stdout=out_only_meta,
        )
        self.assertEqual(rc_only_meta, 0)
        self.assertIn("usage: fx only-meta", out_only_meta.getvalue())

    @mock.patch.object(fx_builtins, "subprocess", autospec=True, spec_set=True)
    def test_help_bare_and_short_flags_print_usage_directly(
        self, mock_subprocess: mock.MagicMock
    ) -> None:
        out_bare = io.StringIO()
        err_bare = io.StringIO()
        rc_bare = fx_builtins.run_help(
            self.fuchsia_dir,
            [],
            stdout=out_bare,
            stderr=err_bare,
        )
        out_flag = io.StringIO()
        err_flag = io.StringIO()
        rc_flag = fx_builtins.run_help(
            self.fuchsia_dir,
            ["--no-contrib"],
            stdout=out_flag,
            stderr=err_flag,
        )
        self.assertEqual(rc_bare, 0)
        self.assertEqual(rc_flag, 0)
        self.assertEqual(err_bare.getvalue(), "")
        self.assertEqual(err_flag.getvalue(), "")
        mock_subprocess.run.assert_not_called()
        self.assertIn("usage: fx [--dir BUILD_DIR]", out_bare.getvalue())
        self.assertEqual(out_bare.getvalue(), out_flag.getvalue())

    def test_help_vendor_registered_command_via_top_level_help(self) -> None:
        vendor_reg_dir = (
            self.fuchsia_dir / "vendor" / "acme" / "scripts" / "devshell"
        )
        vendor_reg_dir.mkdir(parents=True, exist_ok=True)
        (vendor_reg_dir / "fx_host_tools.json").write_text(
            json.dumps(
                {
                    "acme-target-cmd": {
                        "category": "Build",
                        "summary": "acme target command",
                        "target": "//vendor/acme/tools:acme_target_cmd",
                    },
                    "acme-script-cmd": {
                        "category": "Build",
                        "summary": "acme script command",
                        "script": "//vendor/acme/tools/acme_script_cmd.sh",
                    },
                }
            ),
            encoding="utf-8",
        )
        staged_vendor_bin = (
            self.fuchsia_dir
            / "out"
            / "_fx_host_tools"
            / "vendor"
            / "acme"
            / "acme-target-cmd"
        )
        staged_vendor_bin.parent.mkdir(parents=True, exist_ok=True)
        staged_vendor_bin.write_text(
            '#!/bin/bash\necho "acme-target-cmd detailed flag help"\n',
            encoding="utf-8",
        )
        staged_vendor_bin.chmod(0o755)

        out_target = io.StringIO()
        err_target = io.StringIO()
        with mock.patch.dict(
            os.environ,
            {"FUCHSIA_OUT_DIR": str(self.fuchsia_dir / "out")},
            clear=False,
        ):
            rc_target = fx_builtins.run_help(
                self.fuchsia_dir,
                ["acme-target-cmd"],
                stdout=out_target,
                stderr=err_target,
            )
        self.assertEqual(rc_target, 0)
        self.assertIn(
            "acme-target-cmd detailed flag help",
            out_target.getvalue(),
        )

        script_file = (
            self.fuchsia_dir
            / "vendor"
            / "acme"
            / "tools"
            / "acme_script_cmd.sh"
        )
        script_file.parent.mkdir(parents=True, exist_ok=True)
        script_file.write_text(
            '#!/bin/bash\necho "acme-script-cmd help"\n', encoding="utf-8"
        )
        script_file.chmod(0o755)

        out_script = io.StringIO()
        err_script = io.StringIO()
        rc_script = fx_builtins.run_help(
            self.fuchsia_dir,
            ["acme-script-cmd"],
            stdout=out_script,
            stderr=err_script,
        )
        self.assertEqual(rc_script, 0)
        self.assertIn("acme-script-cmd help", out_script.getvalue())

    def test_help_registered_non_executable_script_fallbacks(self) -> None:
        reg_path = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )
        reg_data = json.loads(reg_path.read_text(encoding="utf-8"))
        reg_data["noexec-doc"] = {
            "category": "Other",
            "summary": "non-exec script with doc comments",
            "script": "//tools/devshell/noexec_doc.sh",
        }
        reg_data["noexec-nodoc"] = {
            "category": "Other",
            "summary": "non-exec script without doc comments",
            "script": "//tools/devshell/noexec_nodoc.sh",
        }
        reg_path.write_text(json.dumps(reg_data), encoding="utf-8")

        doc_script = self.fuchsia_dir / "tools" / "devshell" / "noexec_doc.sh"
        doc_script.write_text(
            "#!/bin/bash\n"
            "## usage: fx noexec-doc\n"
            "## Doc fallback works.\n",
            encoding="utf-8",
        )
        doc_script.chmod(0o644)

        nodoc_script = (
            self.fuchsia_dir / "tools" / "devshell" / "noexec_nodoc.sh"
        )
        nodoc_script.write_text("#!/bin/bash\necho hi\n", encoding="utf-8")
        nodoc_script.chmod(0o644)

        out_doc = io.StringIO()
        err_doc = io.StringIO()
        with mock.patch.object(
            fx_builtins.builder,
            "ensure_registered_host_tool",
            autospec=True,
        ) as mock_ensure:
            rc_doc = fx_builtins.run_help(
                self.fuchsia_dir,
                ["noexec-doc"],
                stdout=out_doc,
                stderr=err_doc,
            )
        self.assertEqual(rc_doc, 0)
        self.assertEqual(mock_ensure.call_count, 0)
        self.assertIn("usage: fx noexec-doc", out_doc.getvalue())
        self.assertIn("Doc fallback works.", out_doc.getvalue())
        self.assertIn(
            "Global fx options: fx [OPTION] noexec-doc ...",
            out_doc.getvalue(),
        )

        out_nodoc = io.StringIO()
        err_nodoc = io.StringIO()
        with mock.patch.object(
            fx_builtins.builder,
            "ensure_registered_host_tool",
            autospec=True,
        ) as mock_ensure2:
            rc_nodoc = fx_builtins.run_help(
                self.fuchsia_dir,
                ["noexec-nodoc"],
                stdout=out_nodoc,
                stderr=err_nodoc,
            )
        self.assertEqual(rc_nodoc, 0)
        self.assertEqual(mock_ensure2.call_count, 0)
        self.assertIn(
            "non-exec script without doc comments", out_nodoc.getvalue()
        )
        self.assertIn("Category: Other", out_nodoc.getvalue())
        self.assertIn(
            "Registry: script //tools/devshell/noexec_nodoc.sh",
            out_nodoc.getvalue(),
        )

    def test_help_registered_python_script_no_dash_s_and_pythonpath(
        self,
    ) -> None:
        (self.fuchsia_dir / "help_helper_mod.py").write_text(
            'MARKER = "from_fuchsia_dir"\n', encoding="utf-8"
        )
        py_script = self.fuchsia_dir / "tools" / "devshell" / "py_help_cmd.py"
        py_script.write_text(
            "import sys\n"
            "import help_helper_mod\n"
            'print(f"NO_SITE={sys.flags.no_site}:{help_helper_mod.MARKER}")\n',
            encoding="utf-8",
        )
        py_script.chmod(0o644)

        reg_path = (
            self.fuchsia_dir / "tools" / "devshell" / "fx_host_tools.json"
        )
        reg_data = json.loads(reg_path.read_text(encoding="utf-8"))
        reg_data["py-help-cmd"] = {
            "category": "Other",
            "summary": "python help cmd",
            "script": "//tools/devshell/py_help_cmd.py",
        }
        reg_path.write_text(json.dumps(reg_data), encoding="utf-8")

        out_py = io.StringIO()
        err_py = io.StringIO()
        rc_py = fx_builtins.run_help(
            self.fuchsia_dir,
            ["py-help-cmd"],
            stdout=out_py,
            stderr=err_py,
        )
        self.assertEqual(rc_py, 0, msg=err_py.getvalue())
        self.assertIn("NO_SITE=0:from_fuchsia_dir", out_py.getvalue())

    def test_optional_features_meta_is_dataclass(self) -> None:
        self.assertTrue(dataclasses.is_dataclass(fx_builtins.OptionalFeature))
        features = fx_builtins.load_optional_features_meta(self.fuchsia_dir)
        self.assertTrue(len(features) >= 5)
        for feat in features:
            self.assertIsInstance(feat, fx_builtins.OptionalFeature)
            self.assertIsInstance(feat.name, str)
            self.assertIsInstance(feat.default_enabled, bool)
            self.assertIsInstance(feat.description, tuple)
            self.assertTrue(len(feat.description) > 0)

        lib_dir = self.fuchsia_dir / "tools" / "devshell" / "lib"
        lib_dir.mkdir(parents=True, exist_ok=True)
        features_sh = lib_dir / "fx-optional-features.sh"
        features_sh.write_text(
            "_FX_OPTIONAL_FEATURES_SPEC=(\n"
            '  "old_feat|enabled|Old description"\n'
            ")\n"
            "_FX_OPTIONAL_FEATURES_SPEC=(\n"
            '  "custom_feat|disabled|First line"\n'
            '  "custom_feat|disabled|Second line"\n'
            ")\n",
            encoding="utf-8",
        )
        custom_features = fx_builtins.load_optional_features_meta(
            self.fuchsia_dir
        )
        self.assertSequenceEqual(
            custom_features,
            (
                fx_builtins.OptionalFeature(
                    name="custom_feat",
                    default_enabled=False,
                    description=("First line", "Second line"),
                ),
            ),
        )

        # Empty file in fuchsia_dir must not fall back to embedded defaults.
        features_sh.write_text("", encoding="utf-8")
        self.assertSequenceEqual(
            fx_builtins.load_optional_features_meta(self.fuchsia_dir), ()
        )

    def test_load_agent_env_vars(self) -> None:
        lib_dir = self.fuchsia_dir / "tools" / "devshell" / "lib"
        lib_dir.mkdir(parents=True, exist_ok=True)
        (lib_dir / "agents.txt").write_text(
            "# comment\n\nCUSTOM_AGENT_ONE\nSHARED_VAR\n", encoding="utf-8"
        )
        (lib_dir / "agent_session_ids.txt").write_text(
            "SHARED_VAR\nCUSTOM_SESSION_ID\n", encoding="utf-8"
        )
        self.assertSequenceEqual(
            fx_builtins.load_agent_env_vars(self.fuchsia_dir),
            ("CUSTOM_AGENT_ONE", "SHARED_VAR", "CUSTOM_SESSION_ID"),
        )

    def test_builtins_registry(self) -> None:
        self.assertEqual(
            set(fx_builtins.BUILTINS.keys()),
            {"get-src-dir", "get-build-dir", "registry", "help"},
        )
        for name, entry in fx_builtins.BUILTINS.items():
            self.assertIsInstance(entry, fx_builtins.BuiltinCommand)
            self.assertEqual(entry.name, name)
            self.assertTrue(callable(entry.handler))

    def test_compute_prebuilt_all_paths_matches_platform_sh(self) -> None:
        platform_sh_text = fx_builtins._read_lib_text(
            self.fuchsia_dir, "platform.sh"
        )
        self.assertTrue(platform_sh_text)
        platform_sh_copy = self.fuchsia_dir / "platform_copy.sh"
        platform_sh_copy.write_text(platform_sh_text, encoding="utf-8")
        cp = subprocess.run(
            (
                "/bin/bash",
                "-c",
                'export FUCHSIA_DIR="$1"; source "$2" && printf "%s" "${!3}"',
                "bash",
                str(self.fuchsia_dir),
                str(platform_sh_copy),
                "PREBUILT_" + "ALL_PATHS",
            ),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=True,
        )
        self.assertEqual(
            fx_builtins.compute_prebuilt_all_paths(self.fuchsia_dir),
            cp.stdout,
        )


if __name__ == "__main__":
    unittest.main()
