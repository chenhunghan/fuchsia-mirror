# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Provides methods for Host-(Fuchsia)Target interactions via ADB."""

import atexit
import contextlib
import glob
import logging
import os
import shlex
import shutil
import socket
import stat
import tarfile
import tempfile
import threading
import time
import types
from collections.abc import Iterator
from importlib import resources
from pathlib import Path

from honeydew import errors
from honeydew.affordances.usb import usb as usb_affordance
from honeydew.transports import transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.adb.adb_server import AdbServer
from honeydew.transports.ffx import ffx
from honeydew.utils import common, host_shell

_ADB_PATH_ENV_VAR = "HONEYDEW_ADB_OVERRIDE"

# Default per-attempt timeout for check_connection().
_DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS: float = 30.0

_DEFAULT_TCP_CONNECT_TIMEOUT_SECS: float = 10.0
_TCP_CONNECT_POLL_INTERVAL_SECS: float = 1.0
_TCP_RECONNECT_TIMEOUT_SECS: float = 30.0

_DEFAULT_RUN_ATTEMPTS: int = 3

_COLLECT_ADB_DEVICES_TIMEOUT_SECS: float = 10.0

# Timeout and retry delay for querying adbd's UID in `Adb.verify_root()`. The
# single retry absorbs a transient connection drop (matches Mobly's
# `is_adb_root`).
_ROOT_CHECK_TIMEOUT_SECS: float = 10.0
_ROOT_CHECK_RETRY_DELAY_SECS: float = 0.2

# Serializes `adb forward` and `adb reverse` calls across all Adb instances in
# this process. The ADB server has known concurrency issues when it creates or
# removes socket bindings from several threads at once (matches Mobly's
# `ADB_PORT_LOCK`).
_ADB_PORT_LOCK: threading.Lock = threading.Lock()

_LOGGER: logging.Logger = logging.getLogger(__name__)


def _extract_keys_tar(
    tar_path: str | Path,
) -> tempfile.TemporaryDirectory[str]:
    """Extracts an ADB vendor keys tarball into a new temporary directory."""
    temp_dir = tempfile.TemporaryDirectory()
    try:
        target_dir = Path(temp_dir.name).resolve()
        with tarfile.open(tar_path, "r") as tar:
            for member in tar.getmembers():
                if member.issym() or member.islnk():
                    raise adb_errors.AdbError(
                        f"Symbolic or hard links are not allowed in key tar: {member.name}"
                    )
                member_path = target_dir.joinpath(member.name).resolve()
                try:
                    member_path.relative_to(target_dir)
                except ValueError:
                    raise adb_errors.AdbError(
                        f"Unsafe member in tar file: {member.name}"
                    )
            if hasattr(tarfile, "data_filter"):
                tar.extractall(path=temp_dir.name, filter="data")
            else:
                tar.extractall(path=temp_dir.name)
        _LOGGER.info(
            "Extracted adb keys from tarball to %s",
            temp_dir.name,
        )
        return temp_dir
    except Exception:
        temp_dir.cleanup()
        raise


def _get_bundled_keys_tar() -> tempfile.TemporaryDirectory[str] | None:
    """Extracts bundled adb_keys.tar from honeydew.data if present."""
    try:
        from honeydew import data  # type: ignore[attr-defined]

        with resources.as_file(
            resources.files(data).joinpath("adb_keys.tar")
        ) as f:
            if f.exists():
                _LOGGER.info("Using ADB vendor keys path: %s", f)
                return _extract_keys_tar(f)
    except (ImportError, FileNotFoundError, AttributeError, TypeError):
        pass
    return None


def _resolve_vendor_keys_path(
    vendor_keys_path: str | None = None,
) -> tuple[str | None, tempfile.TemporaryDirectory[str] | None]:
    """Resolves the vendor keys path, extracting tar archives if needed.

    Checks in order:
    1. Explicit `vendor_keys_path` argument (directory or `.tar` file).
    2. `ADB_VENDOR_KEYS` environment variable (directory or `.tar` file).
    3. Bundled Python data resource (`honeydew.data/adb_keys.tar`), which is
       packaged at build time from the `honeydew_adb_keys_dir` GN build arg
       (defaults to `//third_party/android/platform/vendor/google/security/adb`
       if present, or can be overridden via
       `fx set ... --args='honeydew_adb_keys_dir="//path/to/keys"'`).

    If the resolved path is a `.tar` archive and exists, extracts it to a
    temporary directory.

    Returns:
        A tuple of (resolved_path, temp_directory_object).
    """
    temp_dir: tempfile.TemporaryDirectory[str] | None = None

    if vendor_keys_path is None:
        vendor_keys_path = os.environ.get("ADB_VENDOR_KEYS")

    if vendor_keys_path:
        if os.path.exists(vendor_keys_path):
            _LOGGER.info("Using ADB vendor keys path: %s", vendor_keys_path)
            if vendor_keys_path.endswith(".tar"):
                temp_dir = _extract_keys_tar(vendor_keys_path)
                vendor_keys_path = temp_dir.name
        else:
            _LOGGER.warning(
                "ADB vendor keys path %s does not exist; falling back to bundled keys.",
                vendor_keys_path,
            )
            vendor_keys_path = None

    if not vendor_keys_path:
        temp_dir = _get_bundled_keys_tar()
        if temp_dir:
            vendor_keys_path = temp_dir.name

    return vendor_keys_path, temp_dir


def _get_adb_binary() -> str:
    """Returns the path to the `adb` binary.

    Prefers resolving from environment variable `HONEYDEW_ADB_OVERRIDE` if
    provided; otherwise, extract from Python resource, set permissions to
    executable, and store on disk. If running outside the build system without
    data resources, falls back to `PATH`.

    Returns:
        Absolute path to `adb` binary.

    Raises:
        adb_errors.InitializationError: If ADB binary is not found.
    """

    bin_path: str | None = os.getenv(_ADB_PATH_ENV_VAR)
    if bin_path is not None:
        return bin_path

    try:
        from honeydew import data  # type: ignore[attr-defined,unused-ignore]

        bin_fd = tempfile.NamedTemporaryFile(suffix="adb", delete=False)
        bin_path = bin_fd.name
        bin_fd.close()
        with resources.as_file(resources.files(data).joinpath("adb")) as f:
            f.chmod(f.stat().st_mode | stat.S_IEXEC)
            shutil.copy2(f, bin_path)
        atexit.register(os.unlink, bin_path)
        return bin_path
    except (ImportError, FileNotFoundError, AttributeError, TypeError):
        pass

    bin_path = shutil.which("adb")
    if bin_path is not None:
        return bin_path

    raise adb_errors.InitializationError(
        "ADB binary was not found in Python data resources, in PATH, or via "
        f"`{_ADB_PATH_ENV_VAR}`."
    )


def _find_usb_device_path(target_serial: str | None = None) -> str | None:
    """Finds the sysfs device path for the given serial (or Google VID)."""
    for dev_path in glob.glob("/sys/bus/usb/devices/*"):
        try:
            if target_serial:
                serial_file = os.path.join(dev_path, "serial")
                if not os.path.exists(serial_file):
                    continue
                with open(serial_file, "r", encoding="utf-8") as f:
                    ser = f.read().strip()
                if ser == target_serial:
                    return dev_path
            else:
                vendor_file = os.path.join(dev_path, "idVendor")
                if not os.path.exists(vendor_file):
                    continue
                with open(vendor_file, "r", encoding="utf-8") as f:
                    vid = f.read().strip()
                if vid == "18d1":
                    return dev_path
        except OSError:
            pass
    return None


def _has_adb_interface(dev_path: str) -> bool:
    """Checks if the given sysfs USB device path exposes an ADB interface."""
    for intf_path in glob.glob(os.path.join(dev_path, "*:*.*/bInterfaceClass")):
        intf_dir = os.path.dirname(intf_path)
        try:
            with open(
                os.path.join(intf_dir, "bInterfaceClass"), "r", encoding="utf-8"
            ) as f:
                cls = f.read().strip()
            with open(
                os.path.join(intf_dir, "bInterfaceSubClass"),
                "r",
                encoding="utf-8",
            ) as f:
                subcls = f.read().strip()
            with open(
                os.path.join(intf_dir, "bInterfaceProtocol"),
                "r",
                encoding="utf-8",
            ) as f:
                proto = f.read().strip()

            if cls == "ff" and subcls == "42" and proto == "01":
                return True
        except OSError:
            pass
    return False


def _check_adb_sysfs(target_serial: str | None = None) -> bool:
    """Checks sysfs to see if there is an ADB interface for the given serial."""
    dev_path = _find_usb_device_path(target_serial)
    if dev_path:
        return _has_adb_interface(dev_path)
    return False


def is_tcp_serial(serial_number: str | None) -> bool:
    """Returns True if the given serial number is a TCP host:port address.

    Args:
        serial_number: The ADB device serial number or TCP address.

    Returns:
        True if the given serial number is a TCP host:port address, False
        otherwise.
    """
    if not serial_number:
        return False
    host, sep, port_str = serial_number.rpartition(":")
    if not (
        sep
        and host
        and port_str.isascii()
        and port_str.isdigit()
        and 1 <= int(port_str) <= 65535
    ):
        return False
    if host.startswith("[") or host.endswith("]"):
        return host.startswith("[") and host.endswith("]") and ":" in host[1:-1]
    return ":" not in host


def _parse_is_root_uid(device_name: str, output: str) -> bool:
    # The output also contains stderr, e.g. adb daemon startup messages, so
    # only parse the last line.
    lines = output.strip().splitlines()
    try:
        return int(lines[-1] if lines else "") == 0
    except ValueError as err:
        raise adb_errors.AdbCommandError(
            f"Unexpected `id -u` output on {device_name}: {output!r}"
        ) from err


class _AdbRootContextManager(
    contextlib.AbstractContextManager[None],
    contextlib.AbstractAsyncContextManager[None],
):
    """Context manager for temporarily enabling root privileges via ADB."""

    def __init__(
        self,
        adb_transport: Adb,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> None:
        self._adb: Adb = adb_transport
        self._timeout: float | None = timeout
        self._attempts: int = (
            adb_transport._attempts if attempts is None else attempts
        )

    def __enter__(self) -> None:
        with self._adb._root_lock:
            if self._adb._root_ref_count == 0:
                start_time: float = time.time()
                # Query the device directly; `_is_root` is only a best-effort
                # cache and can be stale if adbd restarted without root.
                try:
                    self._adb._rooted_by_context = not self._adb.verify_root(
                        timeout=self._timeout
                    )
                except adb_errors.AdbUnauthorizedError:
                    raise
                except adb_errors.AdbCommandError as err:
                    if self._attempts <= 1:
                        raise
                    _LOGGER.debug(
                        "Failed to verify root state on %s in `use_adb_root`, "
                        "falling through to `root()`: %s",
                        self._adb._device_name,
                        err,
                    )
                    self._adb._rooted_by_context = True
                if self._adb._rooted_by_context:
                    self._adb.root(
                        timeout=self._adb._get_remaining_timeout(
                            start_time=start_time,
                            timeout=self._timeout,
                            operation="use_adb_root",
                        ),
                        attempts=self._attempts,
                    )
            self._adb._root_ref_count += 1

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> None:
        with self._adb._root_lock:
            self._adb._root_ref_count -= 1
            if self._adb._root_ref_count == 0:
                if self._adb._rooted_by_context:
                    self._adb._rooted_by_context = False
                    self._adb.unroot(
                        timeout=self._timeout, attempts=self._attempts
                    )

    async def __aenter__(self) -> None:
        self.__enter__()

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> None:
        self.__exit__(exc_type, exc_val, exc_tb)


class Adb(transport.Transport):
    """Provides methods for Host-(Fuchsia)Target interactions via ADB.

    Note: The USB detection logic (`verify_supported()`) relies on Linux-specific
    sysfs (`/sys/bus/usb/devices/*`) and is only supported on Linux hosts.

    Args:
        device_name: Fuchsia device name.
        serial_number: Target device serial number.
        ffx_transport: FFX transport instance used to restart wedged
            device-side adbd processes during recovery.
        run_isolated_server: Whether to run an isolated ADB server.
        output_dir: Directory where ADB server logs should be stored.
        vendor_keys_path: Path to custom vendor keys.
        attempts: Default maximum number of attempts for ADB commands.
            Defaults to 3.
        usb: Optional USB affordance object to notify before and after USB
            disconnects triggered by ADB operations (such as `adb root`,
            `adb unroot`, and `_recover_adb_server()`).

    Raises:
        NotSupportedError: If ADB transport is not supported on the device.
        InitializationError: If ADB binary is not found or isolated ADB server fails to start.
        AdbConnectionError: If ADB fails to connect to the device.
    """

    def __init__(
        self,
        device_name: str,
        serial_number: str,
        ffx_transport: ffx.FFX,
        run_isolated_server: bool = True,
        output_dir: Path | str | None = None,
        vendor_keys_path: str | None = None,
        attempts: int = _DEFAULT_RUN_ATTEMPTS,
        usb: usb_affordance.USB | None = None,
    ) -> None:
        self._device_name: str = device_name
        self._serial_number: str = serial_number
        self._ffx: ffx.FFX = ffx_transport
        self._usb: usb_affordance.USB | None = usb
        self._run_isolated_server: bool = run_isolated_server
        self._output_dir: Path | None = Path(output_dir) if output_dir else None
        if self._run_isolated_server and self._output_dir is None:
            _LOGGER.warning(
                "No output_dir provided for %s; isolated ADB server logs will be written to a temporary directory.",
                self._device_name,
            )
        self._attempts: int = attempts
        (
            self._vendor_keys_path,
            self._temp_vendor_keys_dir,
        ) = _resolve_vendor_keys_path(vendor_keys_path)
        self._adb_server: AdbServer | None = None
        self._is_root: bool = False
        self._is_su_root: bool = False
        self._cached_adbd_pid: str | None = None
        self._last_tcp_connect_error: str | None = None
        self._root_lock: threading.Lock = threading.Lock()
        self._root_ref_count: int = 0
        self._rooted_by_context: bool = False
        # Tracks whether `_recover_adb_server()` is currently running so that
        # `check_connection()` (invoked via `self._usb.after_connect()` ->
        # `AndroidDevice.health_check()`) only makes a single `wait-for-device`
        # attempt instead of recursively re-entering `_recover_adb_server()`.
        self._is_recovering: bool = False

        try:
            self.verify_supported()

            self._adb_binary: str = _get_adb_binary()

            if self._run_isolated_server:
                try:
                    _LOGGER.info(
                        "Starting isolated ADB server for %s", self._device_name
                    )
                    self._adb_server = AdbServer(
                        adb_binary_path=self._adb_binary,
                        serial_id=self._serial_number,
                        output_dir=self._output_dir,
                        vendor_keys_path=self._vendor_keys_path,
                    )
                    self._adb_server.start()
                except errors.HoneydewError as err:
                    raise adb_errors.InitializationError(
                        f"Failed to start isolated ADB server for {self._device_name}: {err}"
                    ) from err

            # For TCP targets, `check_connection()` invokes `_connect_tcp()`
            # before running `wait-for-device` (both during `__init__` and
            # after device reboots in `on_device_boot()`).
            self.check_connection(
                timeout=_DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS
            )
            # Cache the remote adbd PID upon initial connection so FFX can kill
            # a wedged adbd out-of-band if ADB later becomes unresponsive.
            self._cache_adbd_pid()
            atexit.register(self.close)
        except BaseException:
            self.close()
            raise

    @property
    def serial_number(self) -> str:
        """Returns the target device's ADB serial number.

        Returns:
            The target device's ADB serial number or TCP `<host>:<port>` address.
        """
        return self._serial_number

    @property
    def binary_path(self) -> str:
        """Returns the path to the host ADB binary.

        Returns:
            Absolute path to the host `adb` binary.
        """
        return self._adb_binary

    @property
    def server_host_port(self) -> tuple[str, int] | None:
        """Returns the isolated ADB server (host, port) tuple, or None.

        Returns:
            A `(host, port)` tuple for the isolated ADB server if running, or
            None if no isolated server is configured.
        """
        if self._adb_server is None:
            return None
        return (self._adb_server.host(), self._adb_server.port())

    @property
    def attempts(self) -> int:
        """Returns the maximum number of attempts for ADB commands.

        Returns:
            The maximum number of attempts for ADB commands.
        """
        return self._attempts

    def restart_server(self) -> None:
        """Restarts the isolated host ADB server, if one is running.

        Use this when the host ADB server holds a stale USB handle to the
        device, e.g. after an in-place USB port reset (`USBDEVFS_RESET`) that
        keeps the device number. Unlike the internal recovery path, this does
        not restart adbd on the device. The restart is intentional, so it does
        not count toward the server's crash restart limit.

        After the restart, re-establishes the ADB connection and re-caches the
        remote adbd PID, as `__init__()` does.

        While ADB is idle, the restart doesn't reset the device's USB stack.
        But if bulk data is in flight, usb-adb-function resets it to flush the
        partial packets, so the USB affordance is notified before and after,
        as `root()` and `unroot()` do.

        Does nothing if no isolated ADB server is configured.

        Must be called from the main thread: `after_connect()` runs the device
        health check, which uses SIGALRM.

        Raises:
            AdbServerError: If the restarted server fails to become ready.
            AdbUnauthorizedError: If the device is unauthorized for ADB
                connections after the restart.
            AdbConnectionError: If ADB fails to reconnect to the device after
                the restart.
            UsbError: If re-establishing the device connection fails after
                the restart.
        """
        _LOGGER.info("Restarting isolated ADB server for %s", self._device_name)
        if self._adb_server is None:
            _LOGGER.info(
                "No isolated ADB server was started for %s during "
                "initialization (run_isolated_server=False), so there is "
                "nothing to restart.",
                self._device_name,
            )
            return
        if self._usb is not None:
            self._usb.before_disconnect()
        self._adb_server.reset_restart_count()
        self._adb_server.restart()
        # Same as `__init__()`: for TCP targets, `check_connection()` also
        # reconnects over TCP before running `wait-for-device`. A single
        # attempt, since a retry in `run()` would call `_recover_adb_server()`,
        # which restarts adbd on the device.
        self.check_connection(
            timeout=_DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS, attempts=1
        )
        # Re-cache the remote adbd PID so FFX can still kill a wedged adbd
        # out-of-band if ADB later becomes unresponsive.
        self._cache_adbd_pid()
        # Re-open the other host transports in case the USB stack was reset.
        # Only done on success so a failure here never masks a restart error.
        if self._usb is not None:
            self._usb.after_connect()

    def on_device_boot(self) -> None:
        """Takes actions after the Fuchsia device reboots.

        Resets cached root state (since Starnix restarts adbd as the shell user
        on boot), verifies the ADB connection is back online, and caches the PID
        of the newly spawned remote adbd process.
        """
        with self._root_lock:
            self._is_root = False
            self._is_su_root = False
            self._rooted_by_context = False
            self._root_ref_count = 0
            self._cached_adbd_pid = None
        self.check_connection(timeout=_DEFAULT_CHECK_CONNECTION_TIMEOUT_SECS)
        # A device reboot spawns a fresh adbd process in Starnix with a new PID;
        # update the cached PID so FFX can kill it if ADB later wedges.
        self._cache_adbd_pid()

    def _build_adb_cmd(
        self, cmd: list[str], include_serial: bool = True
    ) -> list[str]:
        """Constructs the full ADB command line list."""
        adb_cmd = [self._adb_binary]
        if self._adb_server:
            adb_cmd.extend(
                [
                    "-H",
                    self._adb_server.host(),
                    "-P",
                    str(self._adb_server.port()),
                ]
            )
        if include_serial:
            adb_cmd.extend(["-s", self._serial_number])

        # TODO(b/564946370): Remove once 'root' operations are stable. When
        # using a USB-based connection, use 'su root' instead of 'root', which
        # prevents flapping the ADBD instance on-device.
        if self._is_su_root and "shell" in cmd:
            shell_index = cmd.index("shell")
            # Make a copy of the command line passed in. The list type is mutable, so if the
            # same list is used to run a command in a loop, 'su','root' will appear multiple times.
            cmd = cmd.copy()
            cmd.insert(shell_index + 1, "root")
            cmd.insert(shell_index + 1, "su")
        adb_cmd.extend(cmd)
        return adb_cmd

    def _get_command_env(self) -> dict[str, str] | None:
        """Returns the subprocess environment dictionary for ADB commands."""
        env: dict[str, str] | None = None
        if self._vendor_keys_path:
            env = os.environ.copy()
            env["ADB_VENDOR_KEYS"] = self._vendor_keys_path
        elif "ADB_VENDOR_KEYS" in os.environ:
            env = os.environ.copy()
            env.pop("ADB_VENDOR_KEYS", None)
        return env

    def _cache_adbd_pid(self) -> None:
        """Caches the PID of the remote adbd process on the device."""
        try:
            output = host_shell.run(
                cmd=self._build_adb_cmd(
                    ["shell", "pidof", "adbd"], include_serial=True
                ),
                capture_output=True,
                capture_error_in_output=True,
                timeout=10.0,
                env=self._get_command_env(),
            )
            pid = (output or "").strip()
            if pid and pid.isdigit():
                self._cached_adbd_pid = pid
                _LOGGER.debug(
                    "Cached remote adbd PID for %s: %s",
                    self._device_name,
                    self._cached_adbd_pid,
                )
        except errors.HoneydewError as err:
            _LOGGER.debug(
                "Failed to cache remote adbd PID for %s: %s",
                self._device_name,
                err,
            )

    def _restart_adbd(self) -> None:
        """Kills the cached remote adbd process on the device via FFX."""
        if not self._cached_adbd_pid:
            _LOGGER.debug(
                "Not restarting remote adbd on %s (cached_adbd_pid is None).",
                self._device_name,
            )
            return
        _LOGGER.warning(
            "Restarting remote adbd on %s (pid=%s) via FFX...",
            self._device_name,
            self._cached_adbd_pid,
        )
        try:
            self._ffx.run(
                ["starnix", "kill", "-p", self._cached_adbd_pid, "-s", "9"],
                timeout=10.0,
            )
        except errors.HoneydewError as err:
            _LOGGER.warning(
                "Failed to kill remote adbd (pid=%s) on %s via FFX: %s",
                self._cached_adbd_pid,
                self._device_name,
                err,
            )
        finally:
            self._cached_adbd_pid = None

    def _connect_tcp_once(
        self, timeout: float | None = _DEFAULT_TCP_CONNECT_TIMEOUT_SECS
    ) -> bool:
        """Runs a single `adb connect <serial>` command for a TCP target.

        Args:
            timeout: Timeout in seconds for the `adb connect` command.

        Returns:
            True if the TCP connection was established, False otherwise.

        Raises:
            adb_errors.AdbUnauthorizedError: If the device is unauthorized for
                ADB connections.
        """
        cmd = self._build_adb_cmd(
            ["connect", self._serial_number], include_serial=False
        )
        try:
            output = (
                host_shell.run(
                    cmd=cmd,
                    capture_output=True,
                    capture_error_in_output=True,
                    timeout=timeout,
                    env=self._get_command_env(),
                )
                or ""
            )
        except errors.HoneydewError as err:
            err_msg = str(err).strip()
            err_msg_lower = err_msg.lower()
            if (
                "unauthorized" in err_msg_lower
                or "failed to authenticate" in err_msg_lower
            ):
                raise adb_errors.AdbUnauthorizedError(
                    f"Device '{self._device_name}' ({self._serial_number}) is unauthorized. "
                    "Ensure valid ADB vendor keys are bundled or configured via "
                    f"vendor_keys_path / ADB_VENDOR_KEYS. Original error: {err}"
                ) from err
            self._last_tcp_connect_error = err_msg
            _LOGGER.warning(
                "`adb connect %s` failed for %s: %s",
                self._serial_number,
                self._device_name,
                err_msg,
            )
            return False

        output_lower = output.lower()
        if (
            "unauthorized" in output_lower
            or "failed to authenticate" in output_lower
        ):
            raise adb_errors.AdbUnauthorizedError(
                f"Device '{self._device_name}' ({self._serial_number}) is unauthorized. "
                "Ensure valid ADB vendor keys are bundled or configured via "
                f"vendor_keys_path / ADB_VENDOR_KEYS. Output: {output.strip()}"
            )
        if "connected to" in output_lower:
            self._last_tcp_connect_error = None
            return True

        err_msg = output.strip()
        self._last_tcp_connect_error = err_msg
        _LOGGER.warning(
            "`adb connect %s` did not connect for %s: %s",
            self._serial_number,
            self._device_name,
            err_msg,
        )
        return False

    def _connect_tcp(
        self,
        timeout: float | None = _DEFAULT_TCP_CONNECT_TIMEOUT_SECS,
        poll_interval: float = _TCP_CONNECT_POLL_INTERVAL_SECS,
    ) -> None:
        """Connects the host ADB server to a TCP target (`adb connect <serial>`).

        Args:
            timeout: Maximum time in seconds to wait for `adb connect` to
                succeed. Defaults to 10.0 seconds.
            poll_interval: Interval in seconds between `adb connect` attempts.
                Defaults to 1.0 second.

        Raises:
            adb_errors.AdbUnauthorizedError: If the device is unauthorized for
                ADB connections.
            adb_errors.AdbConnectionError: If connecting fails or times out.
        """
        if timeout is not None and timeout <= 0:
            raise adb_errors.AdbConnectionError(
                f"Timed out after {timeout}s connecting ADB over TCP to "
                f"{self._device_name} ({self._serial_number})"
            )
        _LOGGER.info(
            "Connecting ADB over TCP to %s (%s)...",
            self._device_name,
            self._serial_number,
        )
        self._last_tcp_connect_error = None
        cmd_timeout = (
            min(timeout, _DEFAULT_TCP_CONNECT_TIMEOUT_SECS)
            if timeout is not None
            else _DEFAULT_TCP_CONNECT_TIMEOUT_SECS
        )
        unauthorized_err: adb_errors.AdbUnauthorizedError | None = None

        def _try_connect() -> bool:
            nonlocal unauthorized_err
            try:
                return self._connect_tcp_once(timeout=cmd_timeout)
            except adb_errors.AdbUnauthorizedError as err:
                unauthorized_err = err
                return True

        try:
            common.wait_for_state_sync(
                state_fn=_try_connect,
                expected_state=True,
                timeout=timeout,
                wait_time=poll_interval,
            )
        except errors.HoneydewTimeoutError as err:
            last_err_suffix = (
                f" (last error: {self._last_tcp_connect_error})"
                if self._last_tcp_connect_error
                else ""
            )
            raise adb_errors.AdbConnectionError(
                f"Timed out after {timeout}s connecting ADB over TCP to "
                f"{self._device_name} ({self._serial_number}){last_err_suffix}"
            ) from err
        if unauthorized_err is not None:
            raise unauthorized_err

    def _reconnect_offline(self) -> None:
        """Runs `adb reconnect offline` to recover an offline ADB connection."""
        _LOGGER.warning(
            "Device %s is offline. Attempting `adb reconnect offline`...",
            self._device_name,
        )
        try:
            host_shell.run(
                cmd=self._build_adb_cmd(
                    ["reconnect", "offline"], include_serial=False
                ),
                capture_output=True,
                capture_error_in_output=True,
                timeout=10.0,
                env=self._get_command_env(),
            )
            if is_tcp_serial(self._serial_number):
                self._connect_tcp()
        except adb_errors.AdbUnauthorizedError:
            raise
        except errors.HoneydewError as err:
            _LOGGER.warning(
                "`adb reconnect offline` failed for %s: %s",
                self._device_name,
                err,
            )

    def _recover_adb_server(self) -> None:
        """Restarts remote adbd (if cached) and isolated host ADB server, restoring root if needed.

        Killing the remote adbd closes its FunctionFS endpoints, which makes the
        device re-enumerate on the host. The USB affordance is notified before
        and after so that FFX treats the drop as intentional and
        Fuchsia-Controller is re-opened on the re-created USB interface.
        """
        assert self._adb_server is not None
        # Mark recovery in progress because `self._usb.after_connect()` below
        # calls `AndroidDevice.health_check()` -> `self.check_connection()`,
        # which by default runs `wait-for-device` with `attempts=3`. If
        # `wait-for-device` failed on attempt 1 during recovery, `self.run()`
        # would otherwise call `_recover_adb_server()` recursively.
        self._is_recovering = True
        try:
            # Killing on-device `adbd` (in `_restart_adbd()`) or running
            # `adb root` below closes `adbd`'s FunctionFS endpoints (`ep0`),
            # causing the composite USB gadget to unbind and re-enumerate on the
            # host with a new Linux network interface index (`ifindex` / IPv6
            # `scope_id`). Notify the USB affordance before and after so FFX
            # treats the drop as intentional and FFX / FuchsiaController refresh
            # their target address / context on the new USB interface.
            if self._usb is not None:
                self._usb.before_disconnect()
            self._restart_adbd()
            self._adb_server.restart()
            time.sleep(10)
            if is_tcp_serial(self._serial_number):
                self._connect_tcp(timeout=_TCP_RECONNECT_TIMEOUT_SECS)
            if self._is_root:
                _LOGGER.info(
                    "Restoring root privileges on %s after ADB recovery...",
                    self._device_name,
                )
                try:
                    host_shell.run(
                        cmd=self._build_adb_cmd(["root"], include_serial=True),
                        capture_output=True,
                        capture_error_in_output=True,
                        timeout=30.0,
                        env=self._get_command_env(),
                    )
                    if is_tcp_serial(self._serial_number):
                        self._connect_tcp(timeout=_TCP_RECONNECT_TIMEOUT_SECS)
                    host_shell.run(
                        cmd=self._build_adb_cmd(
                            ["wait-for-device"], include_serial=True
                        ),
                        capture_output=True,
                        capture_error_in_output=True,
                        timeout=30.0,
                        env=self._get_command_env(),
                    )
                except adb_errors.AdbUnauthorizedError:
                    raise
                except errors.HoneydewError as err:
                    _LOGGER.warning(
                        "Failed to restore root privileges on %s after ADB recovery: %s",
                        self._device_name,
                        err,
                    )
            if self._usb is not None:
                try:
                    self._usb.after_connect()
                except errors.HoneydewError as err:
                    # `_recover_adb_server()` is a best-effort recovery step
                    # inside `Adb.run()`'s retry loop (similar to the `adb root`
                    # restoration block above). Catch `HoneydewError` and log a
                    # warning so that if the device fails to come back online
                    # during recovery, `Adb.run()` proceeds to its final retry
                    # attempt and raises the expected `AdbError` /
                    # `AdbCommandError` to the caller rather than aborting inside
                    # `_recover_adb_server()` with a `FuchsiaDeviceError` or
                    # `HealthCheckError`.
                    _LOGGER.warning(
                        "Failed to re-establish USB connection on %s after ADB recovery: %s",
                        self._device_name,
                        err,
                    )
            # Cache the newly spawned remote adbd PID after killing the old adbd
            # process and/or restarting adbd as root.
            self._cache_adbd_pid()
        finally:
            self._is_recovering = False

    def _verify_adb_tcp_support(self) -> None:
        """Verifies that the ADB TCP serial host can be resolved locally.

        Raises:
            adb_errors.AdbConnectionError: If the TCP host cannot be resolved.
        """
        host, _, port_str = self._serial_number.rpartition(":")
        if host.startswith("[") and host.endswith("]"):
            host = host[1:-1]
        host_without_scope = host.split("%", 1)[0]
        try:
            socket.getaddrinfo(
                host_without_scope,
                int(port_str),
                type=socket.SOCK_STREAM,
            )
        except OSError as err:
            raise adb_errors.AdbConnectionError(
                f"ADB TCP serial '{self._serial_number}' on "
                f"{self._device_name} could not be resolved: {err}"
            ) from err
        _LOGGER.info(
            "TCP ADB serial '%s' detected for %s; skipping USB sysfs check.",
            self._serial_number,
            self._device_name,
        )

    def verify_supported(self) -> None:
        """Verifies that ADB transport is supported by the Fuchsia device.

        This method should be called in `__init__()` so that if ADB is used
        on a Fuchsia device that does not support it, it will raise
        NotSupportedError.

        Raises:
            NotSupportedError: If ADB transport is not supported or if a TCP
                ADB serial host cannot be resolved.
        """
        # TCP endpoints (`<host>:<port>`) are not enumerable in host USB sysfs
        # (`/sys/bus/usb/devices/*/serial`). For TCP targets, `verify_supported`
        # runs before the isolated ADB server starts in `__init__`, validates
        # that the TCP host can be resolved locally, and skips the USB sysfs
        # check. Full TCP reachability and authentication are verified
        # immediately afterwards by `self.check_connection()` (which runs
        # `self._connect_tcp()` and `wait-for-device`).
        if is_tcp_serial(self._serial_number):
            try:
                self._verify_adb_tcp_support()
                return
            except errors.HoneydewError as err:
                raise errors.NotSupportedError(
                    f"ADB over TCP transport is not supported on {self._device_name}: {err}"
                ) from err
        if not _check_adb_sysfs(self._serial_number):
            raise errors.NotSupportedError(
                f"ADB transport is not supported on {self._device_name}"
            )

    def _get_remaining_timeout(
        self,
        start_time: float,
        timeout: float | None,
        operation: str | None = None,
    ) -> float | None:
        """Calculates the remaining timeout after an operation.

        Args:
            start_time: The start time (from `time.time()`) of the operation.
            timeout: The original total timeout in seconds, or None.
            operation: Name of the operation being performed.

        Returns:
            The remaining timeout in seconds, or None if `timeout` is None.

        Raises:
            AdbTimeoutError: If the timeout has already expired.
        """
        if timeout is None:
            return None
        elapsed: float = time.time() - start_time
        remaining_timeout: float = timeout - elapsed
        if remaining_timeout <= 0:
            op_desc = f" during '{operation}'" if operation else ""
            raise adb_errors.AdbTimeoutError(
                f"Timed out after {timeout}s{op_desc} on {self._device_name}"
            )
        return remaining_timeout

    def _get_device_state_from_adb_devices(self, devices_output: str) -> str:
        """Returns this device's state from `adb devices -l` output.

        Args:
            devices_output: Output of `adb devices -l`.

        Returns:
            The device state as reported by ADB (e.g. "device", "offline",
            "unauthorized", "no permissions"), or "not listed" if this device's
            serial number is not in the output.
        """
        # Skip the "List of devices attached" header line.
        for line in devices_output.splitlines()[1:]:
            tokens: list[str] = line.split()
            if not tokens or tokens[0] != self._serial_number:
                continue
            if tokens[1:3] == ["no", "permissions"]:
                return "no permissions"
            return tokens[1] if len(tokens) > 1 else "unknown"
        return "not listed"

    def _log_adb_devices(self) -> str | None:
        """Logs `adb devices -l` output and this device's state, for triage.

        With the isolated ADB server, the server is started with
        `--one-device <serial>`, so the output lists at most this device.
        Otherwise the shared host ADB server lists every attached device.

        Returns:
            This device's state (see `_get_device_state_from_adb_devices()`),
            or None if `adb devices -l` could not be run.
        """
        try:
            output = (
                host_shell.run(
                    cmd=self._build_adb_cmd(
                        ["devices", "-l"], include_serial=False
                    ),
                    capture_output=True,
                    capture_error_in_output=True,
                    timeout=_COLLECT_ADB_DEVICES_TIMEOUT_SECS,
                    env=self._get_command_env(),
                )
                or ""
            ).strip()
            state: str = self._get_device_state_from_adb_devices(output)
            _LOGGER.info(
                "ADB Triage: %s (serial %s) state on host: %s. "
                "Host `adb devices -l` output (checking for %s):\n%s",
                self._device_name,
                self._serial_number,
                state,
                self._device_name,
                output,
            )
            return state
        except errors.HoneydewError as err:
            _LOGGER.warning(
                "Failed to collect `adb devices -l` output for %s: %s",
                self._device_name,
                err,
            )
            return None

    def check_connection(
        self,
        timeout: float,
        attempts: int | None = None,
    ) -> None:
        """Checks the ADB connection from host to Fuchsia device.

        Args:
            timeout: Maximum amount of time in seconds to wait for each
                connection attempt.
            attempts: Maximum number of attempts to run the `wait-for-device`
                command. Defaults to the instance's configured default attempts.

        Raises:
            AdbUnauthorizedError: If the device is unauthorized for ADB connections.
            AdbConnectionError: If ADB fails to connect to the device.
        """
        # When called from inside `_recover_adb_server()` (via
        # `self._usb.after_connect()` -> `health_check()` -> `check_connection()`),
        # force a single attempt so a failed `wait-for-device` does not
        # recursively re-trigger `_recover_adb_server()`.
        if self._is_recovering:
            attempts = 1
        elif attempts is None:
            attempts = self._attempts
        try:
            _LOGGER.info(
                "Checking ADB connection from host to %s...",
                self._device_name,
            )
            start_time: float = time.time()
            remaining_timeout: float | None = timeout
            # Establish or re-establish the TCP ADB connection before running
            # `wait-for-device` (needed both during `__init__` and after a
            # device reboot in `on_device_boot()`, which drops the TCP socket).
            if is_tcp_serial(self._serial_number):
                self._connect_tcp(timeout=remaining_timeout)
                remaining_timeout = self._get_remaining_timeout(
                    start_time=start_time,
                    timeout=timeout,
                    operation="connect_tcp",
                )
            # Host `adb devices -l` is logged below for any failure, so don't
            # also log it from `run()`.
            self.run(
                ["wait-for-device"],
                timeout=remaining_timeout,
                attempts=attempts,
                log_devices_on_failure=False,
            )
            _LOGGER.info(
                "ADB completed the connection check from host to %s.",
                self._device_name,
            )
        except errors.HoneydewError as err:
            adb_state: str | None = self._log_adb_devices()
            if isinstance(err, adb_errors.AdbConnectionError):
                raise
            # Keep the state before `err`, which can be long, so it stays
            # visible in truncated failure summaries.
            state_info: str = f" (adb state: {adb_state})" if adb_state else ""
            raise adb_errors.AdbConnectionError(
                f"ADB connection check failed for {self._device_name}"
                f"{state_info} with err: {err}"
            ) from err

    def run(
        self,
        cmd: list[str],
        timeout: float | None = None,
        include_serial: bool = True,
        attempts: int | None = None,
        log_devices_on_failure: bool = False,
    ) -> str:
        """Runs an ADB command and returns the output.

        Args:
            cmd: ADB command as a list of strings (excluding 'adb' and '-s <serial>' if include_serial=True).
            timeout: Maximum amount of time in seconds to wait for each command attempt to finish.
                Defaults to None (no time limit), which is recommended to avoid flakiness
                on slow or overloaded test environments. Only pass a timeout when explicitly
                required or for commands expected to fail fast.
            include_serial: Whether to include '-s <serial_number>' in the command.
                Defaults to True. Should be set to False for commands like 'adb devices'.
            attempts: Maximum number of attempts to run the command. Defaults to the instance's
                configured default attempts.
            log_devices_on_failure: Whether to log host `adb devices -l` output and this
                device's state if the command fails after all attempts. Useful for
                connection-related commands such as `wait-for-device`. Defaults to False.

        Returns:
            The combined stdout and stderr of the command.

        Raises:
            adb_errors.AdbUnauthorizedError: If the device is unauthorized for ADB commands.
            adb_errors.AdbTimeoutError: If the command times out.
            adb_errors.AdbCommandError: If the command fails.
        """
        attempts = self._attempts if attempts is None else attempts
        if timeout is not None:
            _LOGGER.info(
                "Timeout of %ss per attempt is set for ADB command '%s'. "
                "Note that timeouts can cause flakiness on overloaded test environments.",
                timeout,
                " ".join(cmd),
            )

        attempt = 1
        try:
            while True:
                # Construct adb_cmd inside the loop to ensure we use the updated port if restarted
                adb_cmd = self._build_adb_cmd(
                    cmd, include_serial=include_serial
                )

                _LOGGER.debug(
                    "Running ADB command (attempt %d/%d): %s",
                    attempt,
                    attempts,
                    adb_cmd,
                )
                env = self._get_command_env()

                try:
                    output: str = (
                        host_shell.run(
                            cmd=adb_cmd,
                            capture_output=True,
                            capture_error_in_output=True,
                            timeout=timeout,
                            env=env,
                        )
                        or ""
                    )
                    if self._adb_server:
                        self._adb_server.reset_restart_count()
                    return output

                except errors.HoneydewTimeoutError as err:
                    if attempt < attempts and self._adb_server:
                        _LOGGER.warning(
                            "ADB command timed out. "
                            "Attempting to restart isolated ADB server and retry..."
                        )
                        self._recover_adb_server()
                        attempt += 1
                        continue
                    raise adb_errors.AdbTimeoutError(
                        f"ADB command '{adb_cmd}' timed out after {timeout} seconds"
                    ) from err

                except errors.HostCmdError as err:
                    err_msg_lower = str(err).lower()
                    if "unauthorized" in err_msg_lower:
                        raise adb_errors.AdbUnauthorizedError(
                            f"Device '{self._device_name}' ({self._serial_number}) is unauthorized. "
                            "Ensure valid ADB vendor keys are bundled or configured via vendor_keys_path / ADB_VENDOR_KEYS. "
                            f"Original error: {err}"
                        ) from err

                    is_retriable = any(
                        x in err_msg_lower
                        for x in [
                            "offline",
                            "not found",
                            "connection refused",
                            "cannot connect",
                            "closed",
                            "protocol fault",
                            "connection reset",
                            "no route to host",
                            "network is unreachable",
                            "connection timed out",
                            # Include the leading ": " (e.g. "failed to read copy response: EOF")
                            # to avoid matching "eof" inside ordinary words or shell syntax errors.
                            ": eof",
                        ]
                    )
                    if is_retriable and attempt < attempts:
                        if "offline" in err_msg_lower and attempt == 1:
                            self._reconnect_offline()
                            time.sleep(2)
                            attempt += 1
                            continue
                        if self._adb_server:
                            _LOGGER.warning(
                                "ADB command failed with connection error: %s. "
                                "Attempting to restart isolated ADB server and retry...",
                                str(err),
                            )
                            self._recover_adb_server()
                            attempt += 1
                            continue
                        elif is_tcp_serial(self._serial_number):
                            _LOGGER.warning(
                                "ADB command failed with connection error: %s. "
                                "Attempting to reconnect ADB over TCP and retry...",
                                str(err),
                            )
                            try:
                                self._connect_tcp(
                                    timeout=_TCP_RECONNECT_TIMEOUT_SECS
                                )
                            except adb_errors.AdbUnauthorizedError:
                                raise
                            except errors.HoneydewError as connect_err:
                                _LOGGER.warning(
                                    "Failed to reconnect ADB over TCP on %s: %s",
                                    self._device_name,
                                    connect_err,
                                )
                            attempt += 1
                            continue
                    raise adb_errors.AdbCommandError(str(err)) from err

                except errors.HoneydewError as err:
                    raise adb_errors.AdbCommandError(
                        f"Failed to run ADB command '{adb_cmd}': {err}"
                    ) from err
        except adb_errors.AdbError:
            if log_devices_on_failure:
                self._log_adb_devices()
            raise

    @property
    def is_su_root(self) -> bool:
        """Whether su_root mode is currently enabled on the ADB transport."""
        return self._is_su_root

    def is_rootable(
        self, timeout: float | None = _ROOT_CHECK_TIMEOUT_SECS
    ) -> bool:
        """Checks whether adbd on the device can run as root.

        Args:
            timeout: Maximum time in seconds to wait for the check. Defaults to
                _ROOT_CHECK_TIMEOUT_SECS; pass None for no time limit.

        Returns:
            True if `ro.debuggable` is 1, False otherwise.

        Raises:
            AdbUnauthorizedError: If the device is unauthorized.
            AdbCommandError: If the property cannot be read.
            AdbTimeoutError: If reading the property times out.
        """
        # Use `exec-out` rather than `getprop` (`adb shell`): `su_root` mode
        # rewrites `shell` commands to `shell su root ...`, which fails on
        # non-debuggable builds where `su` is absent.
        cmd = ["exec-out", "getprop", "ro.debuggable"]
        output = self.run(cmd, timeout=timeout, attempts=1)
        lines = output.strip().splitlines()
        return (lines[-1].strip() if lines else "") == "1"

    def verify_root(
        self, timeout: float | None = _ROOT_CHECK_TIMEOUT_SECS
    ) -> bool:
        """Checks on the device whether adbd runs as root.

        Queries adbd's UID with `id -u`, retrying once on a command error
        without triggering full ADB server recovery (which could mutate root
        state using the stale `_is_root` flag).

        Args:
            timeout: Maximum time in seconds for the check, including the retry.
               Defaults to _ROOT_CHECK_TIMEOUT_SECS; pass None for no time
               limit.

        Returns:
            True if adbd runs as root (UID 0), False otherwise.

        Raises:
            AdbUnauthorizedError: If the device is unauthorized.
            AdbCommandError: If the query fails twice or returns a non-integer.
            AdbTimeoutError: If the check times out.
        """
        # Use `exec-out` rather than `shell`: su_root mode rewrites `shell`
        # commands to `shell su root ...`, which would report UID 0 even when
        # adbd itself is not running as root.
        cmd = ["exec-out", "id", "-u"]
        start_time: float = time.time()
        try:
            self._is_root = _parse_is_root_uid(
                self._device_name,
                self.run(cmd, timeout=timeout, attempts=1),
            )
        except adb_errors.AdbUnauthorizedError:
            raise
        except adb_errors.AdbCommandError as err:
            # Retry command or empty/truncated `exec-out` errors once directly
            # (`attempts=1`); `run(attempts=2)` invokes `_recover_adb_server()`,
            # which kills adbd and restores root from the potentially stale
            # `_is_root` flag.
            _LOGGER.debug(
                "Querying adbd UID on %s failed, retrying once: %s",
                self._device_name,
                err,
            )
            remaining_timeout = self._get_remaining_timeout(
                start_time=start_time,
                timeout=timeout,
                operation="verify_root",
            )
            if (
                remaining_timeout is not None
                and remaining_timeout <= _ROOT_CHECK_RETRY_DELAY_SECS
            ):
                # Not enough budget left to retry; keep the original error.
                raise
            time.sleep(_ROOT_CHECK_RETRY_DELAY_SECS)
            self._is_root = _parse_is_root_uid(
                self._device_name,
                self.run(
                    cmd,
                    timeout=self._get_remaining_timeout(
                        start_time=start_time,
                        timeout=timeout,
                        operation="verify_root",
                    ),
                    attempts=1,
                ),
            )
        _LOGGER.debug(
            "adbd on %s runs as root: %s", self._device_name, self._is_root
        )
        return self._is_root

    def root(
        self,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> None:
        """Restarts ADB daemon on device as the root user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.

        Raises:
            AdbTimeoutError: If the command times out.
            AdbCommandError: If the command fails, or adbd is not running as
                root afterwards (e.g. on a production build).
            AdbConnectionError: If reconnecting to a TCP target fails.
        """
        attempts = self._attempts if attempts is None else attempts
        _LOGGER.info("Enabling root-privileges on %s.", self._device_name)
        start_time: float = time.time()
        # `adb root` restarts the on-device adbd, which closes its FunctionFS
        # endpoints and makes the device re-enumerate on the host. Notify the
        # USB affordance so FFX treats the drop as intentional.
        if self._usb is not None:
            self._usb.before_disconnect()
        root_output = self.run(
            ["root"],
            timeout=timeout / attempts if timeout is not None else None,
            attempts=attempts,
        ).strip()
        remaining_timeout: float | None = self._get_remaining_timeout(
            start_time=start_time, timeout=timeout, operation="root"
        )
        if is_tcp_serial(self._serial_number):
            # `adb root` restarts the on-device adbd daemon as root, which drops
            # the active TCP socket connection. Re-run `adb connect` so the host
            # ADB server reconnects before `wait-for-device`.
            self._connect_tcp(timeout=remaining_timeout)
            remaining_timeout = self._get_remaining_timeout(
                start_time=start_time, timeout=timeout, operation="root"
            )
        self.run(
            ["wait-for-device"],
            timeout=(
                remaining_timeout / attempts
                if remaining_timeout is not None
                else None
            ),
            attempts=attempts,
            log_devices_on_failure=True,
        )
        remaining_timeout = self._get_remaining_timeout(
            start_time=start_time, timeout=timeout, operation="root"
        )
        # `adb root` can exit successfully without granting root (e.g. "adbd
        # cannot run as root in production builds"), so check the device.
        if not self.verify_root(timeout=remaining_timeout):
            reason = "adbd is still not root although the build is debuggable"
            try:
                check_timeout = self._get_remaining_timeout(
                    start_time=start_time,
                    timeout=timeout,
                    operation="root",
                )
                if (
                    check_timeout is None
                    or check_timeout > _ROOT_CHECK_TIMEOUT_SECS
                ):
                    check_timeout = _ROOT_CHECK_TIMEOUT_SECS
                if not self.is_rootable(timeout=check_timeout):
                    reason = (
                        "`ro.debuggable` is not 1 (e.g. a production build)"
                    )
            except adb_errors.AdbError as err:
                reason = f"could not check `ro.debuggable`: {err}"
            raise adb_errors.AdbCommandError(
                f"`adb root` did not make adbd run as root on "
                f"{self._device_name} (output: {root_output!r}): {reason}"
            )
        # `adb root` terminates the running adbd process and spawns a new adbd
        # process as root (UID 0) with a new PID; update the cached PID.
        self._cache_adbd_pid()
        # Re-open the other host transports on the re-enumerated device. Only
        # done on success so a failure here never masks the `adb root` error.
        if self._usb is not None:
            self._usb.after_connect()

    def unroot(
        self,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> None:
        """Restarts ADB daemon on device as the shell user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.

        Raises:
            AdbTimeoutError: If the command times out.
            AdbCommandError: If the command fails.
            AdbConnectionError: If reconnecting to a TCP target fails.
        """
        attempts = self._attempts if attempts is None else attempts
        _LOGGER.info("Disabling root-privileges on %s.", self._device_name)
        start_time: float = time.time()
        # `adb unroot` restarts the on-device adbd, which closes its FunctionFS
        # endpoints and makes the device re-enumerate on the host. Notify the
        # USB affordance so FFX treats the drop as intentional.
        if self._usb is not None:
            self._usb.before_disconnect()
        self.run(
            ["unroot"],
            timeout=timeout / attempts if timeout is not None else None,
            attempts=attempts,
        )
        remaining_timeout: float | None = self._get_remaining_timeout(
            start_time=start_time, timeout=timeout, operation="unroot"
        )
        if is_tcp_serial(self._serial_number):
            # `adb unroot` restarts the on-device adbd daemon as the shell user,
            # which drops the active TCP socket connection. Re-run `adb connect`
            # so the host ADB server reconnects before `wait-for-device`.
            self._connect_tcp(timeout=remaining_timeout)
            remaining_timeout = self._get_remaining_timeout(
                start_time=start_time, timeout=timeout, operation="unroot"
            )
        self.run(
            ["wait-for-device"],
            timeout=(
                remaining_timeout / attempts
                if remaining_timeout is not None
                else None
            ),
            attempts=attempts,
            log_devices_on_failure=True,
        )
        self._is_root = False
        self._rooted_by_context = False
        # `adb unroot` terminates the running adbd process and spawns a new adbd
        # process as shell (UID 2000) with a new PID; update the cached PID.
        self._cache_adbd_pid()
        # Re-open the other host transports on the re-enumerated device. Only
        # done on success so a failure here never masks the `adb unroot` error.
        if self._usb is not None:
            self._usb.after_connect()

    def use_adb_root(
        self,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> _AdbRootContextManager:
        """Temporarily runs adb as root within a context.

        Does not attempt to root/unroot if ADB daemon is already running as the root user.
        Supports both synchronous (`with`) and asynchronous (`async with`) contexts.

        Usage:
        ```
        with self.use_adb_root():
            ... do something ...
        ```
        or in async functions:
        ```
        async with self.use_adb_root():
            ... do something ...
        ```

        Args:
            timeout: Maximum amount of time in seconds to wait for root/unroot commands.
            attempts: Maximum number of attempts to run the root/unroot commands. Defaults to the
                instance's configured default attempts.

        Returns:
            A context manager enabling root on enter and restoring previous root state on exit.
        """
        attempts = self._attempts if attempts is None else attempts
        return _AdbRootContextManager(self, timeout=timeout, attempts=attempts)

    @contextlib.contextmanager
    def use_su_root(
        self,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> Iterator[None]:
        """Temporarily enables running adb as root within a context using su_root.

        Does not attempt to root/unroot if ADB daemon is already running as the root user.

        Args:
            timeout: Maximum amount of time in seconds to wait for root/unroot commands.
            attempts: Maximum number of attempts to run the root/unroot commands. Defaults to the
                instance's configured default attempts.

        Usage:
        ```
        with self.use_su_root():
            ... do something ...
        ```
        """
        attempts = self._attempts if attempts is None else attempts
        adbd_was_root = self._is_su_root
        if not adbd_was_root:
            self.su_root(timeout=timeout, attempts=attempts)

        try:
            yield
        finally:
            if not adbd_was_root:
                self.su_unroot(timeout=timeout, attempts=attempts)

    def su_root(
        self,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> None:
        """Restarts ADB daemon on device as the root user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.
        """
        attempts = self._attempts if attempts is None else attempts
        _LOGGER.info("Enabling root-privileges on %s.", self._device_name)
        # When using a USB-based connection, use 'su root' instead of 'root',
        # which prevents flapping the ADBD instance on-device. This is
        # handled internally by _build_adb_cmd.
        _LOGGER.debug("Using 'su root' instead of 'root'.")
        self._is_su_root = True

    def su_unroot(
        self,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> None:
        """Restarts ADB daemon on device as the shell user.

        Args:
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.
        """
        attempts = self._attempts if attempts is None else attempts
        _LOGGER.info("Disabling root-privileges on %s.", self._device_name)
        _LOGGER.debug("No longer using 'su root'.")
        self._is_su_root = False

    def setprop(
        self,
        prop_name: str,
        value: str,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> None:
        """Sets a system property on the device via `adb shell setprop <prop_name> <value>`.

        Args:
            prop_name: Name of the system property to set.
            value: Value to set the system property to.
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.

        Raises:
            AdbCommandError: If the command fails or times out.
        """
        attempts = self._attempts if attempts is None else attempts
        _LOGGER.debug(
            "Setting property '%s' = '%s' on %s",
            prop_name,
            value,
            self._device_name,
        )
        self.run(
            ["shell", "setprop", shlex.quote(prop_name), shlex.quote(value)],
            timeout=timeout,
            attempts=attempts,
        )

    def getprop(
        self,
        prop_name: str,
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> str:
        """Gets a system property from the device via `adb shell getprop <prop_name>`.

        Args:
            prop_name: Name of the system property to get.
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.

        Returns:
            The value of the property stripped of whitespace.

        Raises:
            AdbCommandError: If the command fails or times out.
        """
        attempts = self._attempts if attempts is None else attempts
        value = self.run(
            ["shell", "getprop", shlex.quote(prop_name)],
            timeout=timeout,
            attempts=attempts,
        ).strip()
        _LOGGER.debug(
            "Got property '%s' = '%s' on %s",
            prop_name,
            value,
            self._device_name,
        )
        return value

    def forward(
        self,
        args: list[str],
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> str:
        """Runs `adb forward <args>` while holding a process-wide port lock.

        `adb forward <local> <remote>` silently replaces an existing binding on
        the same local socket. To prevent this, this method passes
        `--no-rebind` when creating a binding, so that ADB rejects such a
        rebind.

        Usage:
        ```
        adb.forward(["tcp:8080", "tcp:80"])
        adb.forward(["--remove", "tcp:8080"])
        ```

        Args:
            args: Arguments to `adb forward`, e.g. ["tcp:8080", "tcp:80"],
                ["--remove", "tcp:8080"] or ["--list"].
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.

        Returns:
            The output of the `adb forward` command.

        Raises:
            AdbCommandError: If the command fails, including when the local
                socket is already bound.
            AdbTimeoutError: If the command times out.
        """
        attempts = self._attempts if attempts is None else attempts
        forward_args = list(args)
        # `--no-rebind` only applies when creating a binding, not to options
        # such as `--list`, `--remove` or `--remove-all`.
        if args and not args[0].startswith("-"):
            forward_args.insert(0, "--no-rebind")
        with _ADB_PORT_LOCK:
            return self.run(
                ["forward", *forward_args],
                timeout=timeout,
                attempts=attempts,
            )

    def reverse(
        self,
        args: list[str],
        timeout: float | None = None,
        attempts: int | None = None,
    ) -> str:
        """Runs `adb reverse <args>` while holding a process-wide port lock.

        Usage:
        ```
        adb.reverse(["tcp:8080", "tcp:80"])
        adb.reverse(["--remove-all"])
        ```

        Args:
            args: Arguments to `adb reverse`, e.g. ["tcp:8080", "tcp:80"],
                ["--remove", "tcp:8080"] or ["--list"].
            timeout: Maximum amount of time in seconds to wait for the command to finish.
            attempts: Maximum number of attempts to run the command. Defaults to the
                instance's configured default attempts.

        Returns:
            The output of the `adb reverse` command.

        Raises:
            AdbCommandError: If the command fails.
            AdbTimeoutError: If the command times out.
        """
        attempts = self._attempts if attempts is None else attempts
        with _ADB_PORT_LOCK:
            return self.run(
                ["reverse", *args],
                timeout=timeout,
                attempts=attempts,
            )

    def close(self) -> None:
        """Cleans up the ADB transport.

        Safe to call multiple times.
        """
        # Unregister so an explicit close() is not repeated at process exit and
        # the atexit registry no longer keeps this instance alive.
        atexit.unregister(self.close)
        if self._adb_server:
            _LOGGER.info(
                "Stopping isolated ADB server for %s", self._device_name
            )
            self._adb_server.stop()
            self._adb_server = None
        if self._temp_vendor_keys_dir:
            self._temp_vendor_keys_dir.cleanup()
            self._temp_vendor_keys_dir = None
