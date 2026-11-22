# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import os
import tempfile
import typing
import unittest
import unittest.mock as mock
from pathlib import Path

import build_dir
import package_server
from async_utils.command import AsyncCommandError, CommandOutput


class TestPackageServer(unittest.IsolatedAsyncioTestCase):
    @mock.patch("package_server.lib.FxCmd")
    async def test_is_running_true(self, mock_fx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.return_code = 0
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        # Configure start to be an async mock that returns cmd_mock
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        self.assertTrue(await package_server.is_running())
        mock_fx_cmd.return_value.start.assert_called_with(
            "is-package-server-running"
        )

    @mock.patch("package_server.lib.FxCmd")
    async def test_is_running_false(self, mock_fx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        process_mock.return_code = 1
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        self.assertFalse(await package_server.is_running())

    @mock.patch("package_server.lib.is_running")
    async def test_wait_for_package_server_success(
        self, mock_is_running: mock.Mock
    ) -> None:
        mock_is_running.return_value = True
        process_mock = mock.Mock()
        process_mock.wait = mock.Mock(return_value=asyncio.Future())

        self.assertTrue(
            await package_server.wait_for_package_server(
                process_mock, timeout=1, interval=0.1
            )
        )

    @mock.patch("package_server.lib.is_running")
    async def test_wait_for_package_server_process_exits(
        self, mock_is_running: mock.Mock
    ) -> None:
        mock_is_running.return_value = False
        process_mock = mock.Mock()
        process_mock.wait = mock.AsyncMock(return_value=None)

        self.assertFalse(
            await package_server.wait_for_package_server(
                process_mock, timeout=1, interval=0.1
            )
        )

    @mock.patch("package_server.lib.is_running")
    async def test_wait_for_package_server_timeout(
        self,
        mock_is_running: mock.Mock,
    ) -> None:
        mock_is_running.return_value = False
        process_mock = mock.Mock()
        process_mock.wait = mock.Mock(return_value=asyncio.Future())

        self.assertFalse(
            await package_server.wait_for_package_server(
                process_mock, timeout=0.01, interval=0.001
            )
        )

    @mock.patch.object(build_dir, "get_build_directory", autospec=True)
    @mock.patch.object(package_server.lib, "FfxCmd", autospec=True)
    async def test_get_arguments(
        self, mock_ffx_cmd: mock.Mock, mock_get_build_dir: mock.Mock
    ) -> None:
        mock_get_build_dir.return_value = Path("/some/out/dir")
        default_name = package_server.ephemeral_repository_name()

        args = await package_server.get_arguments()
        self.assertSequenceEqual(
            args,
            (
                "repository",
                "server",
                "start",
                "--foreground",
                "--address",
                "[::]:0",
                "--repository",
                default_name,
                "--repo-path",
                "/some/out/dir/amber-files",
                "--trusted-root",
                "/some/out/dir/amber-files/repository/9.root.json",
                "--alias",
                "fuchsia.com",
                "--alias",
                "chromium.org",
            ),
        )
        mock_ffx_cmd.assert_not_called()

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    def test_ephemeral_repository_name(
        self, mock_get_build_dir: mock.Mock
    ) -> None:
        mock_get_build_dir.return_value = Path("/some/out/dir")
        name1 = package_server.ephemeral_repository_name()
        name2 = package_server.ephemeral_repository_name()
        fxtest_name = package_server.ephemeral_repository_name("fxtest")
        explicit_dir_name = package_server.ephemeral_repository_name(
            "fxtest", "/some/out/dir"
        )
        mock_get_build_dir.return_value = Path("/other/out/dir")
        name3 = package_server.ephemeral_repository_name()

        self.assertEqual(name1, name2)
        self.assertNotEqual(name1, name3)
        self.assertEqual(fxtest_name, explicit_dir_name)
        self.assertRegex(name1, r"^tmp-[0-9a-f]{16}$")
        self.assertRegex(fxtest_name, r"^fxtest-[0-9a-f]{16}$")
        self.assertEqual(name1.split("-", 1)[1], fxtest_name.split("-", 1)[1])

        with tempfile.TemporaryDirectory() as td:
            real_out = os.path.join(td, "real_out")
            os.makedirs(os.path.join(real_out, "amber-files"))
            link_out = os.path.join(td, "link_out")
            os.symlink(real_out, link_out)
            self.assertEqual(
                package_server.ephemeral_repository_name("fxtest", real_out),
                package_server.ephemeral_repository_name("fxtest", link_out),
            )

    @mock.patch.object(build_dir, "get_build_directory", autospec=True)
    async def test_get_arguments_explicit_port_and_background(
        self, mock_get_build_dir: mock.Mock
    ) -> None:
        mock_get_build_dir.return_value = Path("/some/out/dir")

        cases = (
            ("custom_name_default_port", "my-repo", 0, False, "--foreground"),
            ("explicit_port_background", "my-repo", 8084, True, "--background"),
        )
        for case_name, repo_name, port, background, mode_flag in cases:
            with self.subTest(case=case_name):
                args = await package_server.get_arguments(
                    name=repo_name, port=port, background=background
                )
                self.assertSequenceEqual(
                    args,
                    (
                        "repository",
                        "server",
                        "start",
                        mode_flag,
                        "--address",
                        f"[::]:{port}",
                        "--repository",
                        repo_name,
                        "--repo-path",
                        "/some/out/dir/amber-files",
                        "--trusted-root",
                        "/some/out/dir/amber-files/repository/9.root.json",
                        "--alias",
                        "fuchsia.com",
                        "--alias",
                        "chromium.org",
                    ),
                )

    @mock.patch("package_server.lib.build_dir.get_build_directory")
    def test_is_package_repository_built(
        self, mock_get_build_dir: mock.Mock
    ) -> None:
        mock_path = mock.MagicMock()
        mock_get_build_dir.return_value = mock_path
        repo_json = mock_path / "amber-files" / "repository" / "9.root.json"

        repo_json.is_file.return_value = True
        self.assertTrue(package_server.is_package_repository_built())

        repo_json.is_file.return_value = False
        self.assertFalse(package_server.is_package_repository_built())

    @mock.patch.object(
        build_dir,
        "get_build_directory",
        autospec=True,
        return_value=Path("/some/out/dir"),
    )
    @mock.patch.object(
        package_server.lib, "is_package_repository_built", autospec=True
    )
    @mock.patch.object(
        package_server.lib, "wait_for_package_server", autospec=True
    )
    @mock.patch.object(package_server.lib, "get_arguments", autospec=True)
    @mock.patch.object(package_server.lib, "FfxCmd", autospec=True)
    @mock.patch.object(package_server.lib, "FxCmd", autospec=True)
    @mock.patch.object(asyncio, "create_subprocess_exec", autospec=True)
    async def test_start_success(
        self,
        mock_exec: mock.Mock,
        mock_fx_cmd: mock.Mock,
        mock_ffx_cmd: mock.Mock,
        mock_get_arguments: mock.Mock,
        mock_wait: mock.Mock,
        mock_is_built: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        mock_is_built.return_value = True
        mock_wait.return_value = True
        mock_get_arguments.return_value = ("arg1", "arg2")
        mock_ffx_cmd.return_value.command_line.side_effect = (
            lambda *args: ("ffx",) + args
        )
        mock_fx_cmd.return_value.command_line.side_effect = (
            lambda *args: ("fx",) + args
        )
        server_process_mock = mock.Mock()
        mock_exec.return_value = server_process_mock

        with self.subTest(case="default_name"):
            self.assertEqual(await package_server.start(), server_process_mock)
            mock_get_arguments.assert_called_once_with(
                package_server.ephemeral_repository_name()
            )
            mock_ffx_cmd.return_value.command_line.assert_called_with(
                "arg1", "arg2"
            )
            mock_fx_cmd.return_value.command_line.assert_called_with(
                "ffx", "arg1", "arg2"
            )
            mock_exec.assert_called_with(
                "fx",
                "ffx",
                "arg1",
                "arg2",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            mock_wait.assert_called_with(server_process_mock, 30, 0.2)

        mock_get_arguments.reset_mock()
        with self.subTest(case="explicit_name"):
            self.assertEqual(
                await package_server.start(name="custom"),
                server_process_mock,
            )
            mock_get_arguments.assert_called_once_with("custom")

    @mock.patch("package_server.lib.is_package_repository_built")
    async def test_start_fails_when_repo_not_built(
        self,
        mock_is_built: mock.Mock,
    ) -> None:
        mock_is_built.return_value = False
        with self.assertRaisesRegex(
            package_server.PackageServingException,
            "The package repository is not built!",
        ):
            await package_server.start()

    @mock.patch("package_server.lib.FfxCmd")
    async def test_stop(self, mock_ffx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        await package_server.stop(name="my-repo")
        mock_ffx_cmd.return_value.start.assert_called_with(
            "repository",
            "server",
            "stop",
            "my-repo",
        )
        cmd_mock.run_to_completion.assert_called()

    @mock.patch("package_server.lib.FfxCmd")
    async def test_stop_all(self, mock_ffx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        process_mock = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=process_mock)
        mock_ffx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        await package_server.stop()
        mock_ffx_cmd.return_value.start.assert_called_with(
            "repository",
            "server",
            "stop",
        )
        cmd_mock.run_to_completion.assert_called()

    @mock.patch("package_server.lib.FxCmd")
    async def test_deregister(self, mock_fx_cmd: mock.Mock) -> None:
        cmd_mock = mock.Mock()
        output = CommandOutput("", "", 0, 0.1, None)
        cmd_mock.run_to_completion = mock.AsyncMock(return_value=output)
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        self.assertEqual(
            await package_server.deregister(name="my-repo"),
            output,
        )
        mock_fx_cmd.assert_called_once_with(build_directory=None, timeout=5.0)
        mock_fx_cmd.return_value.start.assert_called_once_with(
            "ffx",
            "target",
            "repository",
            "deregister",
            "-r",
            "my-repo",
        )

    @mock.patch("package_server.lib.is_running")
    async def test_run_and_stop_on_exit_already_running(
        self,
        mock_is_running: mock.Mock,
    ) -> None:
        mock_is_running.return_value = True
        async with package_server.run_and_stop_on_exit():
            pass
        mock_is_running.assert_called()

    @mock.patch(
        "package_server.lib.build_dir.get_build_directory",
        return_value=Path("/some/out/dir"),
    )
    @mock.patch("package_server.lib.deregister")
    @mock.patch("package_server.lib.stop")
    @mock.patch("package_server.lib.start")
    @mock.patch("package_server.lib.is_running")
    async def test_run_and_stop_on_exit_starts_and_stops(
        self,
        mock_is_running: mock.Mock,
        mock_start: mock.Mock,
        mock_stop: mock.Mock,
        mock_deregister: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        mock_is_running.return_value = False
        process_mock = mock.Mock()
        process_mock.terminate = mock.Mock()
        process_mock.wait = mock.AsyncMock(return_value=None)
        mock_start.return_value = process_mock

        async with package_server.run_and_stop_on_exit():
            pass

        expected_repo_name = package_server.ephemeral_repository_name()
        mock_is_running.assert_called()
        mock_start.assert_called_once_with(expected_repo_name)
        mock_deregister.assert_called_once_with(expected_repo_name)
        mock_stop.assert_called_once_with(expected_repo_name)

        process_mock.terminate.assert_called()
        process_mock.wait.assert_called()

    @mock.patch(
        "package_server.lib.build_dir.get_build_directory",
        return_value=Path("/some/out/dir"),
    )
    @mock.patch("package_server.lib.start")
    @mock.patch("package_server.lib.is_running")
    async def test_run_and_stop_on_exit_failure(
        self,
        mock_is_running: mock.Mock,
        mock_start: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        mock_is_running.return_value = False
        mock_start.side_effect = package_server.PackageServingException("fail")

        with self.assertRaises(package_server.PackageServingCLIException):
            async with package_server.run_and_stop_on_exit():
                pass

    def test_is_fuchsia_repo_registered(self) -> None:
        data_aliases = json.dumps(
            {"ok": {"data": [{"name": "devhost", "aliases": ["fuchsia.com"]}]}}
        )
        self.assertTrue(package_server.is_fuchsia_repo_registered(data_aliases))

        data_name = json.dumps(
            {"ok": {"data": [{"name": "fuchsia.com", "aliases": []}]}}
        )
        self.assertTrue(package_server.is_fuchsia_repo_registered(data_name))

        data_pkg = json.dumps(
            {
                "ok": {
                    "data": [
                        {"name": "fuchsia-pkg://fuchsia.com", "aliases": []}
                    ]
                }
            }
        )
        self.assertTrue(package_server.is_fuchsia_repo_registered(data_pkg))

        self.assertFalse(
            package_server.is_fuchsia_repo_registered(
                json.dumps({"ok": {"data": []}})
            )
        )

        with self.assertRaises(json.JSONDecodeError):
            package_server.is_fuchsia_repo_registered("not json")

        data_multiple = json.dumps(
            {
                "ok": {
                    "data": [
                        {"name": "stale-repo", "aliases": ["fuchsia.com"]},
                        {"name": "fxtest-abc123", "aliases": ["fuchsia.com"]},
                    ]
                }
            }
        )
        self.assertTrue(
            package_server.is_fuchsia_repo_registered(
                data_multiple, repo_name="fxtest-abc123"
            )
        )
        self.assertFalse(
            package_server.is_fuchsia_repo_registered(
                data_multiple, repo_name="fxtest-other"
            )
        )

        data_pkg_repo = json.dumps(
            {
                "ok": {
                    "data": [
                        {
                            "name": "fuchsia-pkg://fxtest-abc123",
                            "aliases": ["fuchsia.com"],
                        }
                    ]
                }
            }
        )
        self.assertTrue(
            package_server.is_fuchsia_repo_registered(
                data_pkg_repo, repo_name="fxtest-abc123"
            )
        )

    async def test_is_connected_to_device(self) -> None:
        runner = mock.AsyncMock(
            side_effect=[
                CommandOutput("ok", "", 0, 0.1, None),
                CommandOutput(
                    json.dumps(
                        {
                            "ok": {
                                "data": [
                                    {
                                        "name": "devhost",
                                        "aliases": ["fuchsia.com"],
                                    }
                                ]
                            }
                        }
                    ),
                    "",
                    0,
                    0.1,
                    None,
                ),
            ]
        )
        self.assertTrue(
            await package_server.is_connected_to_device(run_command=runner)
        )
        self.assertEqual(runner.call_count, 2)

        runner_not_running = mock.AsyncMock(
            return_value=CommandOutput("", "", 1, 0.1, None)
        )
        self.assertFalse(
            await package_server.is_connected_to_device(
                run_command=runner_not_running
            )
        )
        self.assertEqual(runner_not_running.call_count, 1)

        runner_bad_json = mock.AsyncMock(
            side_effect=[
                CommandOutput("ok", "", 0, 0.1, None),
                CommandOutput("not json", "", 0, 0.1, None),
            ]
        )
        self.assertFalse(
            await package_server.is_connected_to_device(
                run_command=runner_bad_json
            )
        )
        self.assertEqual(runner_bad_json.call_count, 2)

    async def test_wait_for_registration(self) -> None:
        stale_output = json.dumps(
            {
                "ok": {
                    "data": [{"name": "stale-repo", "aliases": ["fuchsia.com"]}]
                }
            }
        )
        registered_output = json.dumps(
            {
                "ok": {
                    "data": [
                        {"name": "stale-repo", "aliases": ["fuchsia.com"]},
                        {"name": "fxtest-1234", "aliases": ["fuchsia.com"]},
                    ]
                }
            }
        )
        runner = mock.AsyncMock(
            side_effect=[
                CommandOutput(stale_output, "", 0, 0.1, None),
                CommandOutput(registered_output, "", 0, 0.1, None),
            ]
        )
        with mock.patch("asyncio.sleep", new_callable=mock.AsyncMock):
            self.assertTrue(
                await package_server.wait_for_registration(
                    repo_name="fxtest-1234", run_command=runner
                )
            )
        self.assertEqual(runner.call_count, 2)

        runner_exhausted = mock.AsyncMock(
            return_value=CommandOutput(stale_output, "", 0, 0.1, None)
        )
        with mock.patch("asyncio.sleep", new_callable=mock.AsyncMock):
            self.assertFalse(
                await package_server.wait_for_registration(
                    repo_name="fxtest-1234",
                    retries=3,
                    run_command=runner_exhausted,
                )
            )
        self.assertEqual(runner_exhausted.call_count, 3)

    async def test_serve_lifecycle_and_warnings(self) -> None:
        events: list[str] = []
        warnings: list[str] = []
        serve_abort_signal: asyncio.Event | None = None
        deregister_calls = 0

        async def fake_runner(
            *args: str, **kwargs: typing.Any
        ) -> CommandOutput | None:
            nonlocal serve_abort_signal, deregister_calls
            if "deregister" in args:
                deregister_calls += 1
                events.append(f"deregister_{deregister_calls}")
                # Fail on teardown deregistration to exercise warning callback.
                rc = 0 if deregister_calls == 1 else 1
                return CommandOutput("", "", rc, 0.1, None)
            if "serve" in args:
                events.append("serve")
                serve_abort_signal = kwargs.get("abort_signal")
                if serve_abort_signal is not None:
                    await serve_abort_signal.wait()
                return CommandOutput("", "", 0, 0.1, None)
            return CommandOutput("", "", 0, 0.1, None)

        task, cancel_event = await package_server.serve(
            name="fxtest-1234",
            run_command=fake_runner,
            on_warning=warnings.append,
        )
        cancel_event.set()
        await task

        self.assertEqual(events, ["deregister_1", "serve", "deregister_2"])
        self.assertIsNotNone(serve_abort_signal)
        assert serve_abort_signal is not None
        self.assertTrue(serve_abort_signal.is_set())
        self.assertEqual(len(warnings), 1)
        self.assertIn(
            "Failed to deregister temporary package repository fxtest-1234",
            warnings[0],
        )

    @mock.patch("package_server.lib.FxCmd")
    async def test_serve_forwards_build_directory_to_default_runner(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        cmd_mock = mock.Mock()
        cmd_mock.terminate = mock.Mock()
        cmd_mock.kill = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(
            return_value=CommandOutput("", "", 0, 0.1, None)
        )
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        task, cancel_event = await package_server.serve(
            prefix="fxtest",
            build_directory=Path("/custom/out/dir"),
        )
        cancel_event.set()
        await task

        for call in mock_fx_cmd.call_args_list:
            self.assertEqual(
                call.kwargs.get("build_directory"), "/custom/out/dir"
            )

    async def test_serve_stop_timeout_emits_warning(self) -> None:
        warnings: list[str] = []
        serve_cancelled = False

        async def hanging_runner(
            *args: str, **kwargs: typing.Any
        ) -> CommandOutput | None:
            nonlocal serve_cancelled
            if "serve" in args:
                try:
                    await asyncio.Future()
                except asyncio.CancelledError:
                    serve_cancelled = True
                    raise
            return CommandOutput("", "", 0, 0.1, None)

        with mock.patch(
            "package_server.lib._DEFAULT_SERVER_STOP_TIMEOUT", 0.01
        ):
            task, cancel_event = await package_server.serve(
                name="fxtest-1234",
                run_command=hanging_runner,
                on_warning=warnings.append,
            )
            cancel_event.set()
            await task

        self.assertTrue(serve_cancelled)
        self.assertEqual(len(warnings), 1)
        self.assertIn(
            "Timed out waiting for temporary package server (fxtest-1234) to stop.",
            warnings[0],
        )

    @mock.patch("package_server.lib.FxCmd")
    async def test_default_command_runner_abort_signal_terminates_and_kills(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        runner_done = asyncio.Event()
        cmd_mock = mock.Mock()

        async def wait_until_killed() -> CommandOutput:
            await runner_done.wait()
            return CommandOutput("", "", -9, 0.1, None)

        cmd_mock.terminate = mock.Mock()
        cmd_mock.kill = mock.Mock(side_effect=runner_done.set)
        cmd_mock.run_to_completion = mock.AsyncMock(
            side_effect=wait_until_killed
        )
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        abort_signal = asyncio.Event()
        runner = package_server.lib._default_command_runner()
        with mock.patch("package_server.lib._DEFAULT_ABORT_GRACE_PERIOD", 0.01):
            run_task = asyncio.create_task(
                runner("serve", "-l", "0", abort_signal=abort_signal)
            )
            await asyncio.sleep(0)
            abort_signal.set()
            await run_task

        mock_fx_cmd.return_value.start.assert_called_once_with(
            "serve", "-l", "0"
        )
        cmd_mock.terminate.assert_called_once()
        cmd_mock.kill.assert_called_once()

    @mock.patch("package_server.lib.FxCmd")
    async def test_default_command_runner_cancellation_kills_process(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        async def never_finishes() -> CommandOutput:
            await asyncio.Future()
            return CommandOutput("", "", 0, 0.1, None)

        cmd_mock = mock.Mock()
        cmd_mock.kill = mock.Mock()
        cmd_mock.run_to_completion = mock.AsyncMock(side_effect=never_finishes)
        mock_fx_cmd.return_value.start = mock.AsyncMock(return_value=cmd_mock)

        runner = package_server.lib._default_command_runner()
        run_task = asyncio.create_task(runner("serve", "-l", "0"))
        await asyncio.sleep(0)
        run_task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await run_task

        cmd_mock.kill.assert_called_once()

    @mock.patch("package_server.lib.FxCmd")
    async def test_default_command_runner_handles_async_command_error(
        self, mock_fx_cmd: mock.Mock
    ) -> None:
        mock_fx_cmd.return_value.start = mock.AsyncMock(
            side_effect=AsyncCommandError("failed")
        )
        runner = package_server.lib._default_command_runner()
        self.assertIsNone(await runner("is-package-server-running"))

    async def test_get_running_server_names_matches_canonical_path(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as td:
            real_out = os.path.join(td, "real_out")
            amber_dir = os.path.join(real_out, "amber-files")
            os.makedirs(amber_dir)
            link_out = os.path.join(td, "link_out")
            os.symlink(real_out, link_out)
            other_amber = os.path.join(td, "other_out", "amber-files")
            os.makedirs(other_amber)

            payload = json.dumps(
                {
                    "ok": {
                        "data": [
                            {
                                "name": "devhost",
                                "repo_path": {"file": f"{amber_dir}/"},
                            },
                            {
                                "name": "zxdb-1234",
                                "repo_path": {
                                    "file": os.path.join(
                                        link_out, "amber-files"
                                    )
                                },
                            },
                            {
                                "name": "other-server",
                                "repo_path": {"file": other_amber},
                            },
                            {"name": "", "repo_path": {"file": amber_dir}},
                            {"name": "missing-path"},
                            "unexpected-string-entry",
                        ]
                    }
                }
            )
            runner = mock.AsyncMock(
                return_value=CommandOutput(payload, "", 0, 0.1, None)
            )

            names = await package_server.get_running_server_names(
                build_directory=link_out, run_command=runner
            )

        self.assertEqual(names, frozenset({"devhost", "zxdb-1234"}))
        runner.assert_awaited_once_with(
            "ffx",
            "--machine",
            "json",
            "repository",
            "server",
            "list",
            timeout=15.0,
            quiet_mode=True,
        )

    @mock.patch.object(
        build_dir,
        "get_build_directory",
        autospec=True,
        return_value=Path("/some/out/dir"),
    )
    async def test_get_running_server_names_handles_errors(
        self, _mock_get_build_dir: mock.Mock
    ) -> None:
        cases = (
            ("none_output", None, "Failed to list running package servers"),
            (
                "timeout",
                CommandOutput("", "", -1, 15.0, None, was_timeout=True),
                "Timed out listing running package servers",
            ),
            (
                "nonzero_exit",
                CommandOutput("", "err", 1, 0.1, None),
                r"Failed to list running package servers \(exit code 1\): err",
            ),
            (
                "bad_json",
                CommandOutput("{invalid", "", 0, 0.1, None),
                "Invalid JSON from ffx repository server list",
            ),
            (
                "error_envelope",
                CommandOutput(
                    json.dumps({"user_error": {"message": "boom"}}),
                    "",
                    0,
                    0.1,
                    None,
                ),
                "Unexpected JSON envelope from ffx repository server list",
            ),
            (
                "non_list_data",
                CommandOutput(
                    json.dumps({"ok": {"data": "oops"}}), "", 0, 0.1, None
                ),
                "Unexpected JSON envelope from ffx repository server list",
            ),
        )
        for case_name, output, expected_msg in cases:
            with self.subTest(case=case_name):
                runner = mock.AsyncMock(return_value=output)
                with self.assertRaisesRegex(
                    package_server.PackageServingException, expected_msg
                ):
                    await package_server.get_running_server_names(
                        run_command=runner
                    )

    @mock.patch.object(
        package_server.lib,
        "_default_command_runner",
        autospec=True,
    )
    @mock.patch.object(
        build_dir,
        "get_build_directory",
        autospec=True,
        return_value=Path("/default/out/dir"),
    )
    async def test_get_running_server_names_passes_default_out_dir_to_runner(
        self,
        _mock_get_build_dir: mock.Mock,
        mock_default_runner: mock.Mock,
    ) -> None:
        runner = mock.AsyncMock(
            return_value=CommandOutput(
                json.dumps({"ok": {"data": []}}), "", 0, 0.1, None
            )
        )
        mock_default_runner.return_value = runner
        self.assertEqual(
            await package_server.get_running_server_names(), frozenset()
        )
        mock_default_runner.assert_called_once_with(
            build_directory=Path("/default/out/dir")
        )

    @mock.patch.object(
        package_server.lib,
        "is_package_repository_built",
        autospec=True,
        return_value=False,
    )
    async def test_start_background_returns_false_when_not_built(
        self, mock_is_built: mock.Mock
    ) -> None:
        runner = mock.AsyncMock()
        self.assertFalse(
            await package_server.start_background(
                "zxdb-1234", run_command=runner
            )
        )
        mock_is_built.assert_called_once_with()
        runner.assert_not_called()

    @mock.patch.object(
        build_dir,
        "get_build_directory",
        autospec=True,
        return_value=Path("/some/out/dir"),
    )
    @mock.patch.object(
        package_server.lib,
        "is_package_repository_built",
        autospec=True,
        return_value=True,
    )
    async def test_start_background_returns_true_when_already_running_port_zero(
        self,
        _mock_is_built: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        list_payload = json.dumps(
            {
                "ok": {
                    "data": [
                        {
                            "name": "zxdb-1234",
                            "repo_path": {"file": "/some/out/dir/amber-files"},
                        }
                    ]
                }
            }
        )
        runner = mock.AsyncMock(
            return_value=CommandOutput(list_payload, "", 0, 0.1, None)
        )
        self.assertTrue(
            await package_server.start_background(
                "zxdb-1234", port=0, run_command=runner
            )
        )
        runner.assert_awaited_once_with(
            "ffx",
            "--machine",
            "json",
            "repository",
            "server",
            "list",
            timeout=60,
            quiet_mode=True,
        )

    @mock.patch.object(
        build_dir,
        "get_build_directory",
        autospec=True,
        return_value=Path("/some/out/dir"),
    )
    @mock.patch.object(
        package_server.lib,
        "is_package_repository_built",
        autospec=True,
        return_value=True,
    )
    async def test_start_background_success(
        self,
        _mock_is_built: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        default_name = package_server.ephemeral_repository_name()
        empty_list = json.dumps({"ok": {"data": []}})
        cases = (
            ("explicit_name", "zxdb-1234", 0, 0, False),
            ("default_name_explicit_port", None, 8084, 0, False),
            (
                "explicit_port_already_running_same_port",
                "zxdb-1234",
                8084,
                0,
                True,
            ),
            ("name_conflict_already_running", "zxdb-1234", 0, 1, False),
        )
        for (
            case_name,
            repo_arg,
            port,
            start_rc,
            pre_running,
        ) in cases:
            with self.subTest(case=case_name):
                expected_name = repo_arg or default_name
                list_payload = json.dumps(
                    {
                        "ok": {
                            "data": [
                                {
                                    "name": expected_name,
                                    "repo_path": {
                                        "file": "/some/out/dir/amber-files"
                                    },
                                }
                            ]
                        }
                    }
                )
                pre_payload = list_payload if pre_running else empty_list
                runner = mock.AsyncMock(
                    side_effect=(
                        CommandOutput(pre_payload, "", 0, 0.1, None),
                        CommandOutput(
                            "",
                            "NameConflict" if start_rc else "",
                            start_rc,
                            0.1,
                            None,
                        ),
                        CommandOutput(list_payload, "", 0, 0.1, None),
                    )
                )
                started = await package_server.start_background(
                    repo_arg, port=port, run_command=runner
                )

                self.assertTrue(started)
                self.assertEqual(runner.await_count, 3)
                pre_list_call = runner.await_args_list[0]
                self.assertEqual(pre_list_call.kwargs["timeout"], 60)
                start_call = runner.await_args_list[1]
                self.assertEqual(start_call.kwargs["timeout"], 60)
                start_args = start_call.args
                self.assertEqual(start_args[0], "ffx")
                self.assertIn("--background", start_args)
                self.assertNotIn("--foreground", start_args)
                self.assertIn(f"[::]:{port}", start_args)
                self.assertIn(expected_name, start_args)
                list_call = runner.await_args_list[2]
                self.assertEqual(list_call.kwargs["timeout"], 60)

    @mock.patch.object(
        build_dir,
        "get_build_directory",
        autospec=True,
        return_value=Path("/some/out/dir"),
    )
    @mock.patch.object(
        package_server.lib,
        "is_package_repository_built",
        autospec=True,
        return_value=True,
    )
    async def test_start_background_raises_on_failure(
        self,
        _mock_is_built: mock.Mock,
        _mock_get_build_dir: mock.Mock,
    ) -> None:
        empty_list = json.dumps({"ok": {"data": []}})
        this_dir_list = json.dumps(
            {
                "ok": {
                    "data": [
                        {
                            "name": "zxdb-1234",
                            "repo_path": {"file": "/some/out/dir/amber-files"},
                        }
                    ]
                }
            }
        )
        other_dir_list = json.dumps(
            {
                "ok": {
                    "data": [
                        {
                            "name": "zxdb-1234",
                            "repo_path": {"file": "/other/out/dir/amber-files"},
                        }
                    ]
                }
            }
        )
        cases = (
            (
                "pre_list_probe_fails",
                0,
                (CommandOutput("", "pre list err", 1, 0.1, None),),
                r"Failed to list running package servers \(exit code 1\): pre list err",
                1,
            ),
            (
                "none_output",
                0,
                (CommandOutput(empty_list, "", 0, 0.1, None), None),
                "Failed to start package server",
                2,
            ),
            (
                "timeout_not_started",
                0,
                (
                    CommandOutput(empty_list, "", 0, 0.1, None),
                    CommandOutput("", "", -1, 60.0, None, was_timeout=True),
                    CommandOutput(empty_list, "", 0, 0.1, None),
                ),
                "didn't start within",
                3,
            ),
            (
                "timeout_newly_appeared_stops_server",
                0,
                (
                    CommandOutput(empty_list, "", 0, 0.1, None),
                    CommandOutput("", "", -1, 60.0, None, was_timeout=True),
                    CommandOutput(this_dir_list, "", 0, 0.1, None),
                    CommandOutput("", "", 0, 0.1, None),
                ),
                "didn't start within",
                4,
            ),
            (
                "timeout_already_running_explicit_port_does_not_stop",
                8084,
                (
                    CommandOutput(this_dir_list, "", 0, 0.1, None),
                    CommandOutput("", "", -1, 60.0, None, was_timeout=True),
                    CommandOutput(this_dir_list, "", 0, 0.1, None),
                ),
                "didn't start within",
                3,
            ),
            (
                "timeout_other_build_dir_does_not_stop",
                0,
                (
                    CommandOutput(other_dir_list, "", 0, 0.1, None),
                    CommandOutput("", "", -1, 60.0, None, was_timeout=True),
                    CommandOutput(other_dir_list, "", 0, 0.1, None),
                ),
                "didn't start within",
                3,
            ),
            (
                "timeout_post_list_fails_does_not_stop",
                0,
                (
                    CommandOutput(empty_list, "", 0, 0.1, None),
                    CommandOutput("", "", -1, 60.0, None, was_timeout=True),
                    CommandOutput("", "list err", 1, 0.1, None),
                ),
                "didn't start within",
                3,
            ),
            (
                "nonzero_exit",
                0,
                (
                    CommandOutput(empty_list, "", 0, 0.1, None),
                    CommandOutput("", "bind failed", 1, 0.1, None),
                    CommandOutput(empty_list, "", 0, 0.1, None),
                ),
                "exited with code 1: bind failed",
                3,
            ),
            (
                "nonzero_exit_other_build_dir_conflict",
                0,
                (
                    CommandOutput(other_dir_list, "", 0, 0.1, None),
                    CommandOutput("", "NameConflict", 1, 0.1, None),
                    CommandOutput(other_dir_list, "", 0, 0.1, None),
                ),
                "exited with code 1: NameConflict",
                3,
            ),
            (
                "nonzero_exit_explicit_port_conflict",
                8084,
                (
                    CommandOutput(this_dir_list, "", 0, 0.1, None),
                    CommandOutput("", "NameConflict", 1, 0.1, None),
                ),
                "exited with code 1: NameConflict",
                2,
            ),
            (
                "not_listed",
                0,
                (
                    CommandOutput(empty_list, "", 0, 0.1, None),
                    CommandOutput("", "", 0, 0.1, None),
                    CommandOutput(empty_list, "", 0, 0.1, None),
                ),
                "is not running after startup",
                3,
            ),
            (
                "list_probe_fails",
                0,
                (
                    CommandOutput(empty_list, "", 0, 0.1, None),
                    CommandOutput("", "", 0, 0.1, None),
                    CommandOutput("", "list err", 1, 0.1, None),
                ),
                r"Failed to list running package servers \(exit code 1\): list err",
                3,
            ),
        )
        for (
            case_name,
            port,
            side_effect,
            expected_msg,
            expected_calls,
        ) in cases:
            with self.subTest(case=case_name):
                runner = mock.AsyncMock(side_effect=side_effect)
                with self.assertRaisesRegex(
                    package_server.PackageServingException, expected_msg
                ):
                    await package_server.start_background(
                        "zxdb-1234", port=port, run_command=runner
                    )
                self.assertEqual(runner.await_count, expected_calls)
                if case_name == "timeout_newly_appeared_stops_server":
                    self.assertSequenceEqual(
                        runner.await_args_list[3].args,
                        ("ffx", "repository", "server", "stop", "zxdb-1234"),
                    )
