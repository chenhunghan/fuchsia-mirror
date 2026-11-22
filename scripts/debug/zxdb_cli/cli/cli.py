# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import asyncio
import json
import sys
from collections.abc import Mapping
from typing import Any, Final

from cli.commands import (
    aspace,
    async_backtrace,
    attach,
    break_cmd,
    console,
    continue_cmd,
    detach,
    diagnose_hang,
    driver_list,
    evaluate,
    finish,
    get_state,
    handles,
    mem_read,
    modules,
    next_cmd,
    pause,
    schema,
    stack_trace,
    start,
    step_in,
    stop,
    threads,
    tracepoint,
    variables,
    wait_for_event,
    watch_cmd,
)
from cli.commands.base import BaseCommand
from daemon_manager.manager import UDS_PATH
from pydantic import ValidationError
from shared.protocol import (
    BaseRequest,
    deserialize_response,
    make_request,
    serialize,
)

# Socket read timeout floor and extra grace beyond a request's internal timeout
# so the daemon has time to return a structured timeout response before the UDS
# client disconnects.
DEFAULT_READ_TIMEOUT_SECS: Final[float] = 15.0
READ_TIMEOUT_GRACE_SECS: Final[float] = 5.0


_COMMAND_CLASSES = (
    # keep-sorted start
    aspace.Command,
    async_backtrace.Command,
    attach.Command,
    break_cmd.Command,
    console.Command,
    continue_cmd.Command,
    detach.Command,
    diagnose_hang.Command,
    driver_list.Command,
    evaluate.Command,
    finish.Command,
    get_state.Command,
    handles.Command,
    mem_read.Command,
    modules.Command,
    next_cmd.Command,
    pause.Command,
    schema.Command,
    stack_trace.Command,
    start.Command,
    step_in.Command,
    stop.Command,
    threads.Command,
    tracepoint.Command,
    variables.Command,
    wait_for_event.Command,
    watch_cmd.Command,
    # keep-sorted end
)


def request_to_namespace(req: BaseRequest[Any]) -> argparse.Namespace:
    """Converts a BaseRequest to argparse.Namespace for command executors."""
    return argparse.Namespace(**req.model_dump())


def _build_parser() -> (
    tuple[
        argparse.ArgumentParser,
        Mapping[str, type[BaseCommand]],
        Mapping[str, str],
    ]
):
    """Constructs the CLI argument parser, subparser command map, and alias table."""
    parser = argparse.ArgumentParser(description="fx debug cli")
    parser.add_argument("--json", help="JSON request string")
    parser.add_argument(
        "--ack-seq",
        type=int,
        help="Acknowledge events up to this sequence number",
    )
    subparsers = parser.add_subparsers(dest="command", required=False)

    commands: dict[str, type[BaseCommand]] = {}
    for cmd_class in _COMMAND_CLASSES:
        existing_choices = set(subparsers.choices.keys())
        cmd_class.register_cli(subparsers)
        new_choices = set(subparsers.choices.keys()) - existing_choices
        for choice in new_choices:
            commands[choice] = cmd_class

    alias_map = {
        alias: cmd_class.COMMAND_NAME
        for cmd_class in _COMMAND_CLASSES
        for alias in getattr(cmd_class, "ALIASES", [])
        if cmd_class.COMMAND_NAME
    }
    return parser, commands, alias_map


async def _dispatch_json_command(
    raw_json: str,
    commands: Mapping[str, type[BaseCommand]],
    alias_map: Mapping[str, str],
) -> tuple[BaseRequest[Any] | None, int | None]:
    """Parses and executes or constructs a request from a --json payload."""
    data = json.loads(raw_json)
    if not isinstance(data, dict):
        raise ValueError("JSON payload must be an object")
    if "command" in data and data["command"] in alias_map:
        data["command"] = alias_map[data["command"]]

    req = make_request(data)
    cmd_cls = commands.get(req.command)
    if cmd_cls is not None:
        exit_code = await cmd_cls.execute(request_to_namespace(req))
        if exit_code is not None:
            return req, exit_code
    return req, None


async def _dispatch_cli_command(
    parsed_args: argparse.Namespace,
    commands: Mapping[str, type[BaseCommand]],
) -> tuple[BaseRequest[Any] | None, int | None]:
    """Executes or constructs a request from parsed CLI subcommand arguments."""
    cmd_cls = commands.get(parsed_args.command)
    if cmd_cls is not None:
        exit_code = await cmd_cls.execute(parsed_args)
        if exit_code is not None:
            return None, exit_code

    try:
        args_dict = dict(vars(parsed_args))
        if cmd_cls is not None and cmd_cls.COMMAND_NAME:
            args_dict["command"] = cmd_cls.COMMAND_NAME
        req = make_request(args_dict)
    except (ValueError, ValidationError) as e:
        print(f"Error: {e}", file=sys.stderr)
        return None, 1
    return req, None


async def main(args: list[str]) -> int:
    parser, commands, alias_map = _build_parser()
    parsed_args = parser.parse_args(args)

    if parsed_args.json and parsed_args.command:
        print(
            "Error: --json and command are mutually exclusive", file=sys.stderr
        )
        return 1

    if not parsed_args.json and not parsed_args.command:
        print(
            "Error: Either --json or a command must be provided",
            file=sys.stderr,
        )
        return 1

    req: BaseRequest[Any] | None = None
    if parsed_args.json:
        try:
            req, exit_code = await _dispatch_json_command(
                parsed_args.json, commands, alias_map
            )
            if exit_code is not None:
                return exit_code
        except (ValueError, ValidationError) as e:
            print(f"Error: {e}", file=sys.stderr)
            return 1
    else:
        req, exit_code = await _dispatch_cli_command(parsed_args, commands)
        if exit_code is not None:
            return exit_code

    assert req is not None
    assert isinstance(req, BaseRequest)
    if parsed_args.ack_seq is not None:
        req = req.model_copy(update={"ack_seq": parsed_args.ack_seq})
    return await send_command(req)


async def send_command(req: BaseRequest[Any]) -> int:
    """Sends a serialized request to the daemon and prints the response."""
    if not UDS_PATH.exists():
        print(
            f"Daemon socket not found at {UDS_PATH}.\n\n"
            "To start the daemon, run:\n"
            "  fx debug cli start\n\n"
            "Or run your tests with debugging enabled:\n"
            "  fx test <test_name> --agent-debugging-mode",
            file=sys.stderr,
        )
        return 1

    try:
        reader, writer = await asyncio.open_unix_connection(UDS_PATH)
    except Exception as e:
        print(f"Error communicating with daemon: {e}", file=sys.stderr)
        return 1

    try:
        writer.write(serialize(req).encode("utf-8"))
        await writer.drain()

        req_timeout = getattr(req, "timeout", None)
        read_timeout = (
            max(
                DEFAULT_READ_TIMEOUT_SECS,
                float(req_timeout) + READ_TIMEOUT_GRACE_SECS,
            )
            if req_timeout is not None
            else DEFAULT_READ_TIMEOUT_SECS
        )

        try:
            response_line = await asyncio.wait_for(
                reader.readline(), timeout=read_timeout
            )
        except asyncio.TimeoutError:
            print(
                "Timed out waiting for response from daemon.", file=sys.stderr
            )
            return 1

        if response_line:
            try:
                resp = deserialize_response(response_line.decode("utf-8"), req)
                print(serialize(resp).strip())
            except ValidationError as e:
                print(
                    f"Error: Invalid response from daemon: {e}",
                    file=sys.stderr,
                )
                return 1
        else:
            print("No response received from daemon.", file=sys.stderr)
            return 1
        return 0
    except Exception as e:
        print(f"Error communicating with daemon: {e}", file=sys.stderr)
        return 1
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
