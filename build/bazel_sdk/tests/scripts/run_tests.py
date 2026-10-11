#!/usr/bin/env python3

# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Run the Fuchsia Bazel SDK test suite.

This script can be used to run the test suite either via a pre-generated
launcher script (`--launcher=PATH`), or by configuring a launcher on the fly
against three different types of IDK or SDK directories:

- Against an export IDK:

  Use `--fuchsia_idk_directory=DIR` to specify its path. This will automatically
  create a temporary @fuchsia_sdk repository before running the test suite.

- Against an SDK directory:

  Use `--fuchsia_sdk_directory=DIR` to specify its path. For now, this assumes
  that the SDK directory is self-contained (i.e. does not depend on @rules_fuchsia).

- Against the in-tree @fuchsia_sdk repository:

  Use the `--fuchsia-in-tree-sdk` flag. This should auto-detect the location of
  the in-tree @fuchsia_sdk and @fuchsia_in_tree_idk repositories.

In all cases, the default --target_cpu value will be guessed by looking at the
Fuchsia build directory when generating the launcher.

See //build/bazel/bazel_sdk/README.md for more details about these three
categories.
"""

import argparse
import json
import os
import shlex
import subprocess
import sys
import typing as T
from pathlib import Path

import generate_bazel_launcher

_VERBOSE = False

StrOrPath = str | Path


def _generate_command_string(
    args: T.Sequence[StrOrPath], **kwargs: T.Any
) -> str:
    """Generate a string that prints a command to be run.

    Args:
      args: a list of string or Path items corresponding to the command.
      **kwargs: extra subprocess.run() extra arguments.

    Returns:
      A string that can be printed to a terminal showing the command to
      run.
    """
    output = ""
    margin = ""
    wrap_command = False
    cwd = kwargs.get("cwd")
    if cwd:
        margin = "  "
        output = f"(\n{margin}cd {cwd} &&\n"
        wrap_command = True

    env = kwargs.get("env")
    if env:
        for key, value in sorted(env.items()):
            if os.environ.get(key, None) != value:
                output += "%s%s=%s \\\n" % (margin, key, shlex.quote(value))

    for a in args:
        output += "%s%s \\\n" % (margin, shlex.quote(str(a)))

    if wrap_command:
        output += ")\n"

    return output


def _run_command(
    cmd_args: T.Sequence[StrOrPath],
    check_failure: bool = True,
    **kwargs: T.Any,
) -> "subprocess.CompletedProcess[str]":
    """Run a given command.

    Args:
      cmd_args: a list of string or Path items corresponding to the command.
      check_failure: set to False to ignore command failures. Otherwise the default
        is to print the command's stderr, then raising an exception.
      **kwargs: extra subprocess.run() named arguments.

    Returns:
      a subprocess.CompletedProcess value.
    """
    args = [str(a) for a in cmd_args]
    if _VERBOSE:
        print("RUN_COMMAND: %s" % _generate_command_string(args, **kwargs))

    ret = subprocess.run(args, **kwargs)

    if ret.returncode != 0 and check_failure:
        print(
            "FAILED COMMAND: %s" % _generate_command_string(args, **kwargs),
            file=sys.stderr,
        )
        if ret.stderr:
            print("ERROR: %s" % ret.stderr, file=sys.stderr)
        ret.check_returncode()

    return ret


def _get_command_output_lines(
    args: T.Sequence[StrOrPath],
    extra_env: dict[str, str] | None = None,
    **kwargs: T.Any,
) -> T.Sequence[str]:
    """Run a given command, then return its standard output as text lines.

    Args:
        args: a list of string or Path items corresponding to the command.
        extra_env: a dictionary of optional extra environment variable definitions.
        **kwargs: extra subprocess.run() named arguments.
    Returns:
        A sequence of strings, each one corresponding to one line of the output
        (line terminators are not included).
    """
    if extra_env:
        env = kwargs.get("env")
        if env is None:
            env = os.environ.copy()
        kwargs["env"] = env | extra_env

    ret = _run_command(
        args, capture_output=True, text=True, check_failure=True, **kwargs
    )
    return ret.stdout.splitlines()


def _print_error(msg: str) -> int:
    """Print error message to stderr then return 1."""
    print("ERROR: " + msg, file=sys.stderr)
    return 1


def _relative_path(path: Path) -> Path:
    return Path(os.path.relpath(path))


def _depfile_quote(path: str) -> str:
    r"""Quote a path properly for depfiles, if necessary.

    shlex.quote() does not work because paths with spaces
    are simply encased in single-quotes, while the Ninja
    depfile parser only supports escaping single chars
    (e.g. ' ' -> '\ ').

    Args:
       path: input file path.
    Returns:
       The input file path with proper quoting to be included
       directly in a depfile.
    """
    return path.replace("\\", "\\\\").replace(" ", "\\ ")


def _run_with_launcher(
    launcher_path: Path,
    bazel_repo_map: generate_bazel_launcher.BazelRepositoryMap,
    args: argparse.Namespace,
    extra_args: T.Sequence[str],
) -> int:
    """Invoke the generated Bazel launcher script, write stamp/depfile, and shut down."""
    launcher = str(launcher_path.absolute())

    # These arguments remove verbose output from Bazel, used in queries.
    bazel_quiet_args = [
        "--noshow_loading_progress",
        "--noshow_progress",
        "--ui_event_filters=-info",
    ]

    if args.clean:
        ret = _run_command(
            [launcher, "clean", "--expunge"],
            check_failure=False,
        )
        if ret.returncode != 0:
            return _print_error(
                "Could not clean bazel output base?\n%s\n" % ret.stderr
            )

    non_build_commands = (
        "info",
        "query",
        "cquery",
        "aquery",
        "vendor",
        "mod",
        "fetch",
        "sync",
        "shutdown",
        "clean",
        "version",
        "help",
    )

    post_cmd_args: list[str] = []
    if args.command not in non_build_commands:
        if args.bazel_build_events_log_json:
            args.bazel_build_events_log_json.parent.mkdir(
                parents=True, exist_ok=True
            )
            post_cmd_args.append(
                f"--build_event_json_file={args.bazel_build_events_log_json.resolve()}"
            )

        # Bazel action execution log.
        # This contains records of local and remote executions.
        # Same as --config=exec_log from template.bazelrc
        if args.bazel_exec_log_compact:
            args.bazel_exec_log_compact.parent.mkdir(
                parents=True, exist_ok=True
            )
            post_cmd_args.extend(
                [
                    f"--execution_log_compact_file={args.bazel_exec_log_compact.resolve()}",
                    "--remote_build_event_upload=all",
                ]
            )

        if args.quiet:
            post_cmd_args.extend(
                [
                    "--show_result=0",
                    "--test_output=errors",
                    "--test_summary=none",
                ]
            )
        elif args.test_output:
            post_cmd_args.append(f"--test_output={args.test_output}")

    cmd_args = [launcher, args.command] + post_cmd_args
    if args.command == "test":
        cmd_args.append(args.test_target)
    cmd_args.extend(extra_args)

    _run_command(cmd_args, check_failure=True)

    if args.stamp_file:
        args.stamp_file.write_bytes(b"")

    if args.depfile:
        query_target = f"set({args.test_target})"

        def find_build_files() -> set[Path]:
            # Perform a query to retrieve all build files.
            build_files = _get_command_output_lines(
                args=[launcher, "query"]
                + bazel_quiet_args
                + [f"buildfiles(deps({query_target}))"]
            )
            result = set()
            for b in build_files:
                resolved = bazel_repo_map.resolve_bazel_path(b)
                if resolved:
                    result.add(resolved)
            return result

        def find_source_files() -> set[Path]:
            # Perform a cquery to find all input source files.
            lines = _get_command_output_lines(
                args=[launcher, "cquery"]
                + bazel_quiet_args
                + [
                    "--output=label",
                    f'kind("source file", deps({query_target}))',
                ]
            )
            source_files = set()
            for l in lines:
                path, space, label = l.partition(" ")
                assert (
                    space == " " and label == "(null)"
                ), f"Invalid source file line: {l}"
                resolved = bazel_repo_map.resolve_bazel_path(path)
                if not resolved:
                    continue
                # If the file is a symlink, find its real location
                resolved = resolved.resolve()
                source_files.add(resolved)
            return source_files

        outputs = [args.stamp_file]
        implicit_inputs = [
            _relative_path(p)
            for p in (find_build_files() | find_source_files())
        ]
        with open(args.depfile, "w") as f:
            f.write(
                "%s: %s\n"
                % (
                    " ".join(_depfile_quote(str(p)) for p in outputs),
                    " ".join(_depfile_quote(str(p)) for p in implicit_inputs),
                )
            )

    # Shutdown the Bazel server daemon immediately to avoid Ninja build timeouts
    # See https://fxbug.dev/498320348
    _run_command(
        [launcher, "shutdown"],
        check_failure=False,
    )

    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    mutex_group = parser.add_mutually_exclusive_group(required=True)
    mutex_group.add_argument(
        "--launcher",
        "--use-launcher",
        dest="launcher",
        type=Path,
        help="Invoke a pre-generated Bazel launcher script generated by generate_bazel_launcher.py.",
    )
    parser.add_argument(
        "--launcher-json",
        type=Path,
        help="Optional path to launcher metdata path. Default to <LAUNCHER>.json",
    )
    generate_bazel_launcher.add_launcher_arguments(parser, mutex_group)

    parser.add_argument(
        "--stamp-file",
        type=Path,
        help="Output stamp file, written on success only.",
    )
    parser.add_argument(
        "--depfile", type=Path, help="Output Ninja depfile file."
    )
    parser.add_argument(
        "--verbose", action="store_true", help="Enable verbose mode."
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Do not print anything unless there is an error.",
    )
    parser.add_argument(
        "--clean", action="store_true", help="Force clean build."
    )
    parser.add_argument(
        "--command",
        default="test",
        help="Override bazel command, default is 'test'. Use -- to pass extra arguments.",
    )
    parser.add_argument(
        "--test_target",
        default="//:tests",
        help="Which target to invoke with `bazel test` (default is '//:tests')",
    )
    parser.add_argument(
        "--test_output",
        default="",
        help="See `bazel test --help` for the `test_output` argument.",
    )
    parser.add_argument(
        "--bazel-build-events-log-json",
        type=Path,
        help="Output path to JSON-formatted Build Event Protocol log file",
        metavar="LOG",
    )
    parser.add_argument(
        "--bazel-exec-log-compact",
        type=Path,
        help="Output path to zstd-compressed action execution log file (protobuf: spawn.proto)",
        metavar="LOG",
    )
    parser.add_argument("extra_args", nargs=argparse.REMAINDER)

    args = parser.parse_args()

    if args.quiet:
        args.verbose = None

    if args.verbose:
        global _VERBOSE
        _VERBOSE = True

    if args.depfile and not args.stamp_file:
        parser.error(
            "The --depfile option requires a --stamp-file output path!"
        )

    extra_args = []
    if args.extra_args:
        if args.extra_args[0] != "--":
            parser.error(
                'Use "--" to separate extra arguments passed to the bazel test command.'
            )
        extra_args = args.extra_args[1:]

    bazel_repo_map: None | generate_bazel_launcher.BazelRepositoryMap = None

    if args.launcher:
        if not args.launcher.exists():
            return _print_error(
                f"Bazel launcher script does not exist: {args.launcher}"
            )
        if args.launcher_json:
            launcher_json_path = args.launcher_json
        else:
            launcher_json_path = Path(f"{args.launcher}.json")
        if not launcher_json_path.exists():
            return _print_error(
                f"Bazel launcher json file does not exist: {launcher_json_path}"
            )
        with open(launcher_json_path) as f:
            bazel_repo_map = (
                generate_bazel_launcher.BazelRepositoryMap.from_config_dict(
                    json.load(f)
                )
            )
        return _run_with_launcher(
            args.launcher, bazel_repo_map, args, extra_args
        )

    # Fallback when invoked without --launcher: generate a launcher on the fly.
    (
        rc,
        launcher_path,
        bazel_repo_map,
    ) = generate_bazel_launcher.generate_launcher_from_args(args, parser)
    if rc != 0 or not launcher_path or not bazel_repo_map:
        return rc
    return _run_with_launcher(launcher_path, bazel_repo_map, args, extra_args)


if __name__ == "__main__":
    sys.exit(main())
