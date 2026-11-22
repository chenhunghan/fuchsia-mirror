# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from typing import Any

import portpicker
from daemon.constants import UDS_PATH
from shared.protocol import PROTOCOL_VERSION, serialize
from shared.protocol.hello import HelloRequest
from shared.protocol.start import StartRequest

DAEMON_CLEANUP_TIMEOUT = 5.0


async def _cleanup_process_group(proc: subprocess.Popen[Any] | None) -> None:
    """Kills the process group of the given process."""
    if proc is None:
        return

    stdout_file = getattr(proc, "stdout_file", None)
    if stdout_file and not stdout_file.closed:
        stdout_file.close()
    stderr_file = getattr(proc, "stderr_file", None)
    if stderr_file and not stderr_file.closed:
        stderr_file.close()

    if proc.poll() is None:
        try:
            pgid = os.getpgid(proc.pid)
            os.killpg(pgid, signal.SIGTERM)
            try:
                await asyncio.wait_for(
                    asyncio.to_thread(proc.wait), timeout=DAEMON_CLEANUP_TIMEOUT
                )
            except asyncio.TimeoutError:
                os.killpg(pgid, signal.SIGKILL)
                await asyncio.to_thread(proc.wait)
        except ProcessLookupError:
            pass


class FakeDapServerMixin(unittest.IsolatedAsyncioTestCase):
    """Mixin providing fake DAP server and daemon setup for CLI tests."""

    fake_dap_server: asyncio.AbstractServer | None
    received_dap_requests: list[dict[str, Any]]

    async def asyncSetUp(self) -> None:
        if UDS_PATH.exists():
            UDS_PATH.unlink()
        self.fake_dap_server = None
        self.received_dap_requests = []

    async def asyncTearDown(self) -> None:
        if self.fake_dap_server:
            self.fake_dap_server.close()
            await self.fake_dap_server.wait_closed()
        if UDS_PATH.exists():
            UDS_PATH.unlink()

    def get_daemon_path(self, current_dir: str) -> str:
        """Returns the filesystem path to the daemon binary under test."""
        return os.path.join(current_dir, "zxdb-daemon")

    def get_daemon_cmd(self, args: list[str]) -> list[str]:
        """Returns the command line invocation used to launch the daemon."""
        return [sys.executable] + args

    async def start_fake_dap_server(self, port: int) -> asyncio.AbstractServer:
        async def handle_client(
            reader: asyncio.StreamReader, writer: asyncio.StreamWriter
        ) -> None:
            try:
                while True:
                    header = await reader.readuntil(b"\r\n\r\n")
                    content_length = 0
                    for line in header.decode("utf-8").split("\r\n"):
                        if line.startswith("Content-Length:"):
                            content_length = int(line.split(":")[1].strip())
                    body = await reader.readexactly(content_length)
                    req = json.loads(body.decode("utf-8"))
                    self.received_dap_requests.append(req)

                    if req.get("command") == "initialize":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "initialize",
                            "body": {},
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()

                        # Send initialized event
                        event = {
                            "seq": req["seq"] + 1,
                            "type": "event",
                            "event": "initialized",
                        }
                        event_body = json.dumps(event).encode("utf-8")
                        event_header = (
                            f"Content-Length: {len(event_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(event_header + event_body)
                        await writer.drain()
                    elif req.get("command") == "pause":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "pause",
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()

                        # Simulate stopped event
                        event = {
                            "seq": req["seq"] + 1,
                            "type": "event",
                            "event": "stopped",
                            "body": {
                                "reason": "pause",
                                "threadId": req["arguments"]["threadId"],
                            },
                        }
                        event_body = json.dumps(event).encode("utf-8")
                        event_header = (
                            f"Content-Length: {len(event_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(event_header + event_body)
                        await writer.drain()
                    elif req.get("command") == "continue":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "continue",
                            "body": {"allThreadsContinued": True},
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "stackTrace":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "stackTrace",
                            "body": {
                                "stackFrames": [
                                    {
                                        "id": 1,
                                        "name": "main",
                                        "source": {
                                            "name": "main.cc",
                                            "path": "/path/to/main.cc",
                                        },
                                        "line": 10,
                                        "column": 1,
                                        "trust": "CFI",
                                    }
                                ],
                                "totalFrames": 1,
                            },
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "attach":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "attach",
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()

                        # Simulate process event
                        event = {
                            "seq": req["seq"] + 1,
                            "type": "event",
                            "event": "process",
                            "body": {
                                "name": "test_process",
                                "systemProcessId": 1234,
                                "isLocalProcess": False,
                                "startMethod": "attach",
                            },
                        }
                        event_body = json.dumps(event).encode("utf-8")
                        event_header = (
                            f"Content-Length: {len(event_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(event_header + event_body)
                        await writer.drain()
                    elif req.get("command") == "zxdb.Detach":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "zxdb.Detach",
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "threads":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "threads",
                            "body": {"threads": []},
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "evaluate":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "evaluate",
                            "body": {
                                "result": "42",
                                "type": "int",
                                "variablesReference": 0,
                            },
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
                    elif req.get("command") == "zxdb.ConsoleCommand":
                        resp = {
                            "seq": req["seq"],
                            "type": "response",
                            "request_seq": req["seq"],
                            "success": True,
                            "command": "zxdb.ConsoleCommand",
                            "body": {
                                "output": "Console output OK\n",
                            },
                        }
                        resp_body = json.dumps(resp).encode("utf-8")
                        resp_header = (
                            f"Content-Length: {len(resp_body)}\r\n\r\n".encode(
                                "utf-8"
                            )
                        )
                        writer.write(resp_header + resp_body)
                        await writer.drain()
            except (asyncio.IncompleteReadError, ConnectionResetError):
                pass
            finally:
                writer.close()
                try:
                    await writer.wait_closed()
                except Exception:
                    pass

        server = await asyncio.start_server(handle_client, "127.0.0.1", port)
        return server

    async def _setup_daemon_and_server(
        self,
    ) -> tuple[subprocess.Popen[Any], int]:
        port = portpicker.pick_unused_port()
        proc: subprocess.Popen[Any] | None = None
        read_fd: int | None = None
        write_fd: int | None = None
        stdout_file: Any = None
        stderr_file: Any = None

        # Start fake DAP server
        self.fake_dap_server = await self.start_fake_dap_server(port)

        try:
            read_fd, write_fd = os.pipe()
            os.set_inheritable(write_fd, True)

            # Start Daemon manually
            # Tests are run as a .pyz file, so sys.path[0] is the .pyz archive.
            current_dir = os.path.dirname(sys.path[0])
            daemon_path = self.get_daemon_path(current_dir)
            args = [
                daemon_path,
                "--port",
                str(port),
                f"--ready-fd={write_fd}",
            ]

            # zxdb-daemon expects to be executed from the context of fuchsia-vendored-python, which is
            # also how this test is always invoked. So it's safe for us to use our own interpreter to
            # invoke the zxdb-daemon as well.
            cmd = self.get_daemon_cmd(args)
            stdout_file = tempfile.TemporaryFile()
            stderr_file = tempfile.TemporaryFile()

            proc = subprocess.Popen(
                cmd,
                stdout=stdout_file,
                stderr=stderr_file,
                start_new_session=True,
                pass_fds=[write_fd],
            )
            setattr(proc, "stdout_file", stdout_file)
            setattr(proc, "stderr_file", stderr_file)

            # Close write end in parent
            os.close(write_fd)
            write_fd = None

            # Wait for signal on the pipe
            loop = asyncio.get_running_loop()
            future = loop.create_future()

            def on_read() -> None:
                assert read_fd is not None
                try:
                    data = os.read(read_fd, 1)
                    if not future.done():
                        future.set_result(data)
                except BlockingIOError:
                    pass
                except Exception as e:
                    if not future.done():
                        future.set_exception(e)

            os.set_blocking(read_fd, False)
            loop.add_reader(read_fd, on_read)
            try:
                await asyncio.wait_for(future, timeout=10.0)
            finally:
                loop.remove_reader(read_fd)
            os.close(read_fd)
            read_fd = None

            # Perform handshake to ensure daemon is connected to DAP server
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            req = HelloRequest(version=PROTOCOL_VERSION)
            writer.write(serialize(req).encode("utf-8"))
            await writer.drain()
            response = await asyncio.wait_for(reader.readline(), timeout=5.0)
            writer.close()
            await writer.wait_closed()

            resp_dict = json.loads(response.decode("utf-8"))
            self.assertTrue(resp_dict.get("success"))

            # Send StartRequest to initialize DAP session
            reader, writer = await asyncio.open_unix_connection(UDS_PATH)
            start_req = StartRequest(port=port, connect=True)
            writer.write(serialize(start_req).encode("utf-8"))
            await writer.drain()
            response = await asyncio.wait_for(reader.readline(), timeout=5.0)
            writer.close()
            await writer.wait_closed()

            resp_dict = json.loads(response.decode("utf-8"))
            self.assertTrue(resp_dict.get("success"))

            return proc, port
        except Exception:
            stderr_output = ""
            if (
                proc
                and getattr(proc, "stderr_file", None)
                and not getattr(proc, "stderr_file").closed
            ):
                getattr(proc, "stderr_file").seek(0)
                stderr_output = (
                    getattr(proc, "stderr_file")
                    .read()
                    .decode("utf-8", errors="replace")
                )
            elif stderr_file and not stderr_file.closed:
                stderr_file.seek(0)
                stderr_output = stderr_file.read().decode(
                    "utf-8", errors="replace"
                )

            await _cleanup_process_group(proc)
            if read_fd is not None:
                try:
                    os.close(read_fd)
                except OSError:
                    pass
            if write_fd is not None:
                try:
                    os.close(write_fd)
                except OSError:
                    pass
            if stdout_file and not stdout_file.closed:
                try:
                    stdout_file.close()
                except OSError:
                    pass
            if stderr_file and not stderr_file.closed:
                try:
                    stderr_file.close()
                except OSError:
                    pass

            if stderr_output:
                print(
                    f"[DAEMON CRASH STDERR]\n{stderr_output}", file=sys.stderr
                )
            raise
