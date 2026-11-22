# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import tempfile
import unittest
import unittest.mock as mock

from parameterized import parameterized

import args
import config

# We need a place to create a temporary file that is used to ensure
# output directory checking works correctly. Place it here and then
# cleanup after the tests are done executing.
GLOBAL_TEMP_DIRECTORY = tempfile.TemporaryDirectory()
GLOBAL_FILE_NAME = os.path.join(GLOBAL_TEMP_DIRECTORY.name, "tempfile")
open(GLOBAL_FILE_NAME, "w").close()


class TestArgs(unittest.TestCase):
    @classmethod
    def tearDownClass(cls) -> None:
        GLOBAL_TEMP_DIRECTORY.cleanup()
        return super().tearDownClass()

    def test_empty(self) -> None:
        flags = args.parse_args([])
        flags.validate()
        self.assertTrue(flags.allow_empty_selection)

    def test_allow_empty_selection(self) -> None:
        flags = args.parse_args(["--allow-empty-selection"])
        self.assertTrue(flags.allow_empty_selection)

        flags_no = args.parse_args(["--no-allow-empty-selection"])
        self.assertFalse(flags_no.allow_empty_selection)

    @parameterized.expand(
        [
            (
                "cannot show status with --simple",
                ["--simple", "--status"],
            ),
            (
                "cannot show style with --simple",
                ["--simple", "--style"],
            ),
            (
                "cannot show status with --agent-output",
                ["--agent-output", "--status"],
            ),
            (
                "cannot show style with --agent-output",
                ["--agent-output", "--style"],
            ),
            (
                "cannot show status when terminal is not a TTY",
                ["--status"],
            ),
            (
                "cannot run only host and only device tests",
                ["--device", "--host"],
            ),
            (
                "cannot combine --host with --e2e",
                ["--host", "--e2e"],
            ),
            (
                "cannot combine --host with --only-e2e",
                ["--host", "--only-e2e"],
            ),
            (
                "cannot combine --device with --e2e",
                ["--device", "--e2e"],
            ),
            (
                "cannot combine --device with --only-e2e",
                ["--device", "--only-e2e"],
            ),
            (
                "cannot exceed maximum status frequency",
                ["--status-delay", ".00001"],
            ),
            (
                "cannot have a negative suggestion count",
                ["--suggestion-count", "-1"],
            ),
            (
                "cannot run tests 0 times",
                ["--count", "0"],
            ),
            (
                "cannot have a negative timeout",
                ["--timeout", "-3"],
            ),
            (
                "cannot output to a file",
                ["--ffx-output-directory", GLOBAL_FILE_NAME],
            ),
            (
                "cannot output to a file (artifact flag)",
                ["--artifact-output-directory", GLOBAL_FILE_NAME],
            ),
            (
                "cannot output to a file (outdir flag)",
                ["--outdir", GLOBAL_FILE_NAME],
            ),
            (
                "cannot set a negative --parallel",
                ["--parallel", "-1"],
            ),
            (
                "cannot set a negative --parallel-cases",
                ["--parallel-cases", "-1"],
            ),
            (
                "invalid environment variable formatting is checked",
                ["-e", "abcd"],
            ),
            (
                "--break-on-failure and --breakpoint flags are not supported with host tests.",
                ["--break-on-failure", "--host"],
            ),
            (
                "--break-on-failure and --breakpoint flags are not supported with host tests.",
                ["--breakpoint", "test.cc:123", "--host"],
            ),
            (
                "--breakpoint does not support --use-existing-debugger.",
                ["--breakpoint", "test.cc:123", "--use-existing-debugger"],
            ),
            (
                "--break-on-failure must be set when passing --use-existing-debugger.",
                ["--use-existing-debugger"],
            ),
            (
                "--profiler is not supported with host tests",
                ["--profiler", "--host"],
            ),
            (
                "--profiler is incompatible with --list",
                ["--profiler", "--list"],
            ),
            (
                "--profiler is incompatible with --list-runtime-deps",
                ["--profiler", "--list-runtime-deps"],
            ),
            (
                "--profiler is incompatible with --show-affected-tests",
                ["--profiler", "--show-affected-tests"],
            ),
            (
                "--profiler is incompatible with --previous",
                ["--profiler", "-pr", "log"],
            ),
            (
                "--profiler requires --count 1",
                ["--profiler", "--count", "2"],
            ),
            (
                "--profiler is incompatible with --break-on-failure",
                ["--profiler", "--break-on-failure"],
            ),
            (
                "--profiler is incompatible with --breakpoint",
                ["--profiler", "--breakpoint", "test.cc:123"],
            ),
            (
                "--profiler-config rejects a missing file",
                [
                    "--profiler",
                    "--profiler-config",
                    os.path.join(GLOBAL_TEMP_DIRECTORY.name, "missing.json"),
                ],
            ),
        ]
    )
    @mock.patch("args.termout.is_valid", return_value=False)
    def test_validation_errors(
        self, _unused_name: str, arg_list: list[str], _mock: mock.Mock
    ) -> None:
        flags = args.parse_args(arg_list)
        try:
            with self.assertRaises(args.FlagError):
                flags.validate()
        except AssertionError:
            raise AssertionError("Expected FlagError from " + str(arg_list))

    def test_gemini_analysis(self) -> None:
        # test default behavior (no flag)
        flags = args.parse_args([])
        flags.validate()
        self.assertEqual(flags.gemini_analysis, None)

        # test flag without a value (should default to 1)
        flags = args.parse_args(["--gemini-analysis"])
        flags.validate()
        self.assertEqual(flags.gemini_analysis, 1)

        # test explicit values
        for i in range(1, 4):
            flags = args.parse_args([f"--gemini-analysis={i}"])
            flags.validate()
            self.assertEqual(flags.gemini_analysis, i)

        # test invalid value
        with self.assertRaises(argparse.ArgumentError):
            args.parse_args(["--gemini-analysis=0"])

        # test invalid value
        with self.assertRaises(argparse.ArgumentError):
            args.parse_args(["--gemini-analysis=5"])

    def test_gemini_model_arg(self) -> None:
        # test default behavior (no flag)
        flags = args.parse_args([])
        flags.validate()
        self.assertEqual(
            flags.gemini_model, "gemini-2.5-flash-lite-preview-09-2025"
        )

        # test explicit value
        flags = args.parse_args(["--gemini-model", "test-model"])
        flags.validate()
        self.assertEqual(flags.gemini_model, "test-model")

    def test_simple(self) -> None:
        flags = args.parse_args(["--simple"])
        flags.validate()
        self.assertEqual(flags.style, False)
        self.assertEqual(flags.status, False)

    def test_agent_detection(self) -> None:
        agent_vars = [
            "ANTIGRAVITY_AGENT",
            "ANTIGRAVITY_CONVERSATION_ID",
            "ANTIGRAVITY_EDITOR_APP_ROOT",
            "GEMINI_CLI",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "FFX_AGENT_SESSION_ID",
        ]

        # Ensure agent env vars are NOT set
        env_without_agents = os.environ.copy()
        for var in agent_vars:
            if var in env_without_agents:
                del env_without_agents[var]

        with mock.patch.dict(os.environ, env_without_agents, clear=True):
            flags = args.parse_args([])
            flags.validate()
            self.assertEqual(flags.simple, False)
            self.assertEqual(flags.agent_output, False)

            flags = args.parse_args(["--quiet"])
            flags.validate()
            self.assertEqual(flags.quiet, True)

        # Test with each agent env var
        for var in agent_vars:
            env_with_agent = env_without_agents.copy()
            env_with_agent[var] = "true"
            with mock.patch.dict(os.environ, env_with_agent, clear=True):
                flags = args.parse_args([])
                flags.validate()
                self.assertEqual(flags.simple, True)
                self.assertEqual(flags.agent_output, True)

        # Test override with --no-simple
        for var in agent_vars:
            env_with_agent = env_without_agents.copy()
            env_with_agent[var] = "true"
            with mock.patch.dict(os.environ, env_with_agent, clear=True):
                flags = args.parse_args(["--no-simple"])
                flags.validate()
                self.assertEqual(flags.simple, False)

        # Test override with --no-agent-output
        for var in agent_vars:
            env_with_agent = env_without_agents.copy()
            env_with_agent[var] = "true"
            with mock.patch.dict(os.environ, env_with_agent, clear=True):
                flags = args.parse_args(["--no-agent-output"])
                flags.validate()
                self.assertEqual(flags.agent_output, False)

    def test_allow_temporary_emulator(self) -> None:
        # Default should be True
        flags = args.parse_args([])
        flags.validate()
        self.assertEqual(flags.allow_temporary_emulator, True)

        flags = args.parse_args(["--allow-temporary-emulator"])
        flags.validate()
        self.assertEqual(flags.allow_temporary_emulator, True)

        flags = args.parse_args(["--no-allow-temporary-emulator"])
        flags.validate()
        self.assertEqual(flags.allow_temporary_emulator, False)

    def test_preflight(self) -> None:
        # Default should be True
        flags = args.parse_args([])
        flags.validate()
        self.assertEqual(flags.preflight, True)

        flags = args.parse_args(["--preflight"])
        flags.validate()
        self.assertEqual(flags.preflight, True)

        flags = args.parse_args(["--no-preflight"])
        flags.validate()
        self.assertEqual(flags.preflight, False)

    def test_capture_syslog(self) -> None:
        # Default should be True
        flags = args.parse_args([])
        flags.validate()
        self.assertEqual(flags.capture_syslog, True)

        flags = args.parse_args(["--capture-syslog"])
        flags.validate()
        self.assertEqual(flags.capture_syslog, True)

        flags = args.parse_args(["--no-capture-syslog"])
        flags.validate()
        self.assertEqual(flags.capture_syslog, False)

    def test_json(self) -> None:
        flags = args.parse_args(["--json"])
        flags.validate()
        self.assertEqual(flags.json, True)
        self.assertEqual(flags.logpath, args.LOG_TO_STDOUT_OPTION)

        flags = args.parse_args([])
        flags.validate()
        self.assertEqual(flags.json, False)
        self.assertIsNone(flags.logpath)

    def test_save_log_path_to_file(self) -> None:
        flags = args.parse_args(["--save-log-path-to-file", "/tmp/logpath_ptr"])
        flags.validate()
        self.assertEqual(flags.save_log_path_to_file, "/tmp/logpath_ptr")

        flags = args.parse_args([])
        flags.validate()
        self.assertIsNone(flags.save_log_path_to_file)

    def test_e2e(self) -> None:
        flags = args.parse_args(["--only-e2e"])
        flags.validate()
        self.assertEqual(flags.e2e, True)
        self.assertEqual(flags.only_e2e, True)

    def test_deprecated_and_removed_flags(self) -> None:
        import contextlib
        import io

        stderr_buf = io.StringIO()
        with contextlib.redirect_stderr(stderr_buf):
            flags = args.parse_args(["--use-test-pilot"])
            flags.validate()
        self.assertIn("deprecated", stderr_buf.getvalue())

        with self.assertRaises(argparse.ArgumentError):
            args.parse_args(["--no-use-test-pilot"])

    def test_profiler(self) -> None:
        # Default is off, with no extra `ffx profiler` arguments.
        flags = args.parse_args([])
        flags.validate()
        self.assertFalse(flags.profiler)
        self.assertIsNone(flags.profiler_config)
        self.assertEqual(flags.profiler_attach_args, [])
        self.assertEqual(flags.profiler_stop_args, [])

        flags = args.parse_args(["--profiler"])
        flags.validate()
        self.assertTrue(flags.profiler)
        self.assertEqual(flags.profiler_attach_args, [])
        self.assertEqual(flags.profiler_stop_args, [])

        flags = args.parse_args(["--profiler", "--dry"])
        flags.validate()
        self.assertTrue(flags.profiler)
        self.assertTrue(flags.dry)

        with self.assertRaisesRegex(args.FlagError, r"fxbug\.dev/535175455"):
            args.parse_args(["--profiler", "--count", "2"]).validate()

        # --profiler-config is only read with --profiler, so it can live in
        # ~/.fxtestrc without affecting other runs.
        config_path = os.path.join(GLOBAL_TEMP_DIRECTORY.name, "profiler.json")
        args.parse_args(["--profiler-config", config_path]).validate()

        # Test arguments after `--` are kept separate from the profiler config.
        with open(config_path, "w") as f:
            f.write(
                '{"attach": ["--sample-period-us", "1000"],'
                ' "stop": ["--output-format", "perfetto"]}'
            )
        flags = args.parse_args(
            [
                "--profiler",
                "--profiler-config",
                config_path,
                "my_test",
                "--",
                "--runs",
                "5",
            ]
        )
        flags.validate()
        self.assertTrue(flags.profiler)
        self.assertEqual(flags.profiler_config, config_path)
        self.assertEqual(
            flags.profiler_attach_args, ["--sample-period-us", "1000"]
        )
        self.assertEqual(
            flags.profiler_stop_args, ["--output-format", "perfetto"]
        )
        self.assertEqual(flags.extra_args, ["--runs", "5"])

        for contents, error in [
            ("not json", r"^Failed to read --profiler-config "),
            ('["--system-wide"]', r"^--profiler-config must be a JSON object"),
            ('{"launch": []}', r"^--profiler-config must be a JSON object"),
            (
                '{"attach": [1000]}',
                r'^--profiler-config "attach" must be a list',
            ),
            (
                '{"stop": "--abort"}',
                r'^--profiler-config "stop" must be a list',
            ),
            (
                '{"attach": ["--duration", "5"], "stop": ["--abort"]}',
                r"^--profiler-config cannot include --abort, --duration;",
            ),
        ]:
            with open(config_path, "w") as f:
                f.write(contents)
            with self.assertRaisesRegex(args.FlagError, error):
                args.parse_args(
                    ["--profiler", "--profiler-config", config_path]
                ).validate()

    def test_expand_output_variable(self) -> None:
        flags = args.parse_args(["--outdir", "$FUCHSIA_OUT/test_out"])
        flags.validate()
        flags.update_artifacts_directory_with_out_path("out/default")
        self.assertEqual(
            flags.artifact_output_directory, "out/default/test_out"
        )

        flags = args.parse_args(["--outdir", "${FUCHSIA_OUT}/test_out2"])
        flags.validate()
        flags.update_artifacts_directory_with_out_path("out/default2")
        self.assertEqual(
            flags.artifact_output_directory, "out/default2/test_out2"
        )

    def test_default_merging(self) -> None:
        config_file = config.ConfigFile(
            "path", args.parse_args(["--parallel=10"])
        )
        flags = args.parse_args([], config_file.default_flags)
        self.assertEqual(flags.parallel, 10)
        flags = args.parse_args(["--parallel=1"], config_file.default_flags)
        self.assertEqual(flags.parallel, 1)

    def test_quiet_override(self) -> None:
        """
        You can override -q in a config file with --no-quiet on the command line
        """
        config_file = config.ConfigFile("path", args.parse_args(["-q"]))
        flags = args.parse_args([], config_file.default_flags)
        self.assertEqual(flags.quiet, True)
        flags = args.parse_args(["--no-quiet"], config_file.default_flags)
        self.assertEqual(flags.quiet, False)

    def test_selections_after_test_filter(self) -> None:
        """Passing more selections after a --test-filter works"""
        flags = args.parse_args(["foo", "--test-filter", "some*", "bar"])
        flags.validate()
        self.assertListEqual(flags.test_filter, ["some*"])
        self.assertListEqual(flags.selection, ["foo", "bar"])

        flags = args.parse_args(
            [
                "foo",
                "--test-filter",
                "some*",
                "bar",
                "-a",
                "baz",
                "alpha",
                "--test-filter",
                "something*",
                "beta",
                "-c",
                "gamma.cm",
            ]
        )
        flags.validate()
        self.assertListEqual(flags.test_filter, ["some*", "something*"])
        self.assertListEqual(
            flags.selection,
            [
                "foo",
                "bar",
                "--and",
                "baz",
                "alpha",
                "beta",
                "--component",
                "gamma.cm",
            ],
        )

    def test_exact_after_selections(self) -> None:
        """Passing --exact after a selection works"""
        flags = args.parse_args(["-p", "foo", "-a", "-c", "bar", "--exact"])
        flags.validate()
        self.assertEqual(flags.exact, True)
        self.assertListEqual(
            flags.selection,
            ["--package", "foo", "--and", "--component", "bar"],
        )

    @parameterized.expand(
        [
            ("default is None", [], [], None),
            ("config file overrides output", [], ["--output"], True),
            ("-o shows output", ["-o"], [], True),
            ("--output shows output", ["--output"], [], True),
            ("--no-output hides output", ["--no-output"], [], False),
            (
                "--no-output overrides config",
                ["--no-output"],
                ["--output"],
                False,
            ),
        ]
    )
    def test_output_toggle(
        self,
        _unused_name: str,
        arguments: list[str],
        config_arguments: list[str],
        expected_value: bool,
    ) -> None:
        config_file = config.ConfigFile(
            "path", args.parse_args(config_arguments)
        )
        flags = args.parse_args(arguments, config_file.default_flags)
        self.assertEqual(flags.output, expected_value)

    @parameterized.expand(
        [
            ("<no arguments>", [], False, False),
            ("--break-on-failure", ["--break-on-failure"], True, True),
            ("--breakpoint", ["--breakpoint", "test.cc:123"], True, True),
            (
                "--break-on-failure --use-existing-debugger",
                ["--break-on-failure", "--use-existing-debugger"],
                True,
                False,
            ),
            (
                "--breakpoint --break-on-failure",
                ["--break-on-failure", "--breakpoint", "test.cc:123"],
                True,
                True,
            ),
        ]
    )
    def test_should_start_debugger(
        self,
        _unused_name: str,
        arguments: list[str],
        debugger_will_attach: bool,
        debugger_should_spawn: bool,
    ) -> None:
        """Passing --break-on-failure or --breakpoint does not set debugger_should_spawn()."""
        flags = args.parse_args(arguments)
        flags.validate()
        self.assertEqual(flags.debugger_will_attach(), debugger_will_attach)
        self.assertEqual(flags.debugger_should_spawn(), debugger_should_spawn)

    def test_agent_debugging_mode(self) -> None:
        """Tests that --agent-debugging-mode implies --break-on-failure and --enable-debug-adapter."""
        flags = args.parse_args(["--agent-debugging-mode"])
        flags.validate()
        self.assertEqual(flags.agent_debugging_mode, True)
        self.assertEqual(flags.break_on_failure, True)
        self.assertEqual(flags.enable_debug_adapter, True)

    def test_break_on_failure_boolean_optional(self) -> None:
        """Tests that boolean optional flags work and raise FlagError on conflict."""
        # Test --no-break-on-failure correctly resolves to False
        flags = args.parse_args(["--no-break-on-failure"])
        flags.validate()
        self.assertEqual(flags.break_on_failure, False)

        # Test --no-enable-debug-adapter correctly resolves to False
        flags = args.parse_args(["--no-enable-debug-adapter"])
        flags.validate()
        self.assertEqual(flags.enable_debug_adapter, False)

        # Test that explicitly disabling either flag while enabling agent debugging raises FlagError
        with self.assertRaises(args.FlagError):
            flags = args.parse_args(
                ["--agent-debugging-mode", "--no-break-on-failure"]
            )
            flags.validate()

        with self.assertRaises(args.FlagError):
            flags = args.parse_args(
                ["--agent-debugging-mode", "--no-enable-debug-adapter"]
            )
            flags.validate()

    def test_help_concise_vs_advanced(self) -> None:
        """Tests that --help hides advanced options while --help-advanced shows them."""
        import contextlib
        import io

        always_present_args = [
            "--test-filter",
            "--output",
            "--help-advanced",
        ]
        advanced_optional_args = [
            "--save-log-path-to-file",
            "--ffx-usb-socket-path",
            "--remote-suggestion-builder",
        ]

        help_buf = io.StringIO()
        with self.assertRaises(SystemExit) as cm, contextlib.redirect_stdout(
            help_buf
        ):
            args.parse_args(["--help"])
        self.assertEqual(cm.exception.code, 0)
        help_output = help_buf.getvalue()

        # Core flags should be present in standard --help.
        for flag in always_present_args:
            self.assertIn(flag, help_output)

        # Advanced/internal flags should be suppressed in standard --help.
        for flag in advanced_optional_args:
            self.assertNotIn(flag, help_output)

        adv_buf = io.StringIO()
        with self.assertRaises(SystemExit) as cm, contextlib.redirect_stdout(
            adv_buf
        ):
            args.parse_args(["--help-advanced"])
        self.assertEqual(cm.exception.code, 0)
        adv_output = adv_buf.getvalue()

        # Both core and advanced/internal flags should be present in --help-advanced.
        for flag in always_present_args:
            self.assertIn(flag, adv_output)
        for flag in advanced_optional_args:
            self.assertIn(flag, adv_output)

    @mock.patch.dict(os.environ, {"COLUMNS": "80"})
    def test_help_output_size_within_antigravity_limit(self) -> None:
        """Tests that standard --help output does not exceed Antigravity's output limit."""
        import contextlib
        import io

        help_buf = io.StringIO()
        with self.assertRaises(SystemExit) as cm, contextlib.redirect_stdout(
            help_buf
        ):
            args.parse_args(["--help"])
        self.assertEqual(cm.exception.code, 0)
        help_output = help_buf.getvalue()

        # Antigravity truncates command output exceeding 8,192 bytes.
        # Ensure concise --help stays under this limit.
        antigravity_max_output_bytes = 8192
        output_bytes = len(help_output.encode("utf-8"))
        self.assertLess(
            output_bytes,
            antigravity_max_output_bytes,
            f"--help output ({output_bytes} bytes) exceeds the Antigravity limit of "
            f"{antigravity_max_output_bytes} bytes. Consider marking new flags with adv_help().",
        )

    def test_parallel_zero_flag_rejected(self) -> None:
        """Test that --parallel 0 is rejected during flag validation."""
        flags = args.parse_args(["--parallel", "0"])
        with self.assertRaises(args.FlagError) as ctx:
            flags.validate()
        self.assertIn(
            "--parallel must be a positive number", str(ctx.exception)
        )
