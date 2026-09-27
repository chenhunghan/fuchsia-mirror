# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host package-manager interface for Debian/Ubuntu (APT) systems."""

import functools
import pathlib
import re
import shutil
import subprocess
import sys

# The basename of this script, used to prefix diagnostic and error messages.
_BASENAME = pathlib.Path(__file__).name

# Compiled regular expression matching standard Debian package names.
# Debian package names can consist only of lowercase letters, numbers, and '+', '-', '.', and ':'.
_DEBIAN_PACKAGE_NAME_RE = re.compile(r"^[a-z0-9\-.:+]+$")


def _validate_package_name(package_name: str) -> None:
    """Validates that the package name follows standard Debian naming conventions.

    Raises:
        ValueError: If the package name contains illegal or unsafe characters.
    """
    if not _DEBIAN_PACKAGE_NAME_RE.match(package_name):
        raise ValueError(
            f"[{_BASENAME}] Malformed package name: '{package_name}'"
        )


@functools.cache
def is_available() -> bool:
    """Returns True if 'apt' and 'apt-cache' are available on the host."""
    return (
        shutil.which("apt") is not None
        and shutil.which("apt-cache") is not None
    )


def exists(package_name: str) -> bool:
    """Queries the APT package caches to verify if a package is installable."""
    _validate_package_name(package_name)
    if not is_available():
        return False
    try:
        res = subprocess.run(
            ["apt-cache", "show", package_name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return res.returncode == 0
    except OSError:
        # If the query command fails (e.g. package not found or apt is locked),
        # return False so that the caller can gracefully fall back to displaying
        # manual installation/curl instructions to the developer.
        return False


def get_binary_path(package_name: str, binary_name: str) -> pathlib.Path | None:
    """Finds and returns the absolute path to an installed package's binary.

    This is particularly useful right after package installation, when the binary
    may not yet be added to the active shell's PATH environment variable.

    Returns:
        The Path to the binary if found, or None otherwise.
    """
    # Check common standard system installation locations first (fast path)
    standard_paths = [
        pathlib.Path("/usr/bin") / binary_name,
        pathlib.Path("/usr/local/bin") / binary_name,
        pathlib.Path("/bin") / binary_name,
    ]
    for p in standard_paths:
        if p.is_file():
            return p.resolve()

    # Fall back to querying dpkg (Debian/Ubuntu package manager) if available
    if shutil.which("dpkg"):
        res = subprocess.run(
            ["dpkg", "-L", package_name],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        if res.returncode == 0:
            for line in res.stdout.splitlines():
                p = pathlib.Path(line)
                if p.name == binary_name and p.is_file():
                    return p.resolve()

    return None


def install(package_name: str, interactive: bool = True) -> None:
    """Installs the specified package using 'sudo apt install -y'.

    Raises:
        RuntimeError: If installation is declined or fails.
    """
    _validate_package_name(package_name)
    install_cmd = ["sudo", "apt", "install", "-y", package_name]

    if not interactive or not sys.stdin.isatty():
        raise RuntimeError(
            f"Package '{package_name}' is missing and cannot be installed interactively.\n"
            f"Please run the following command manually:\n"
            f"  {' '.join(install_cmd)}"
        )

    try:
        response = (
            input(
                f"Install '{package_name}' package now?\n"
                f"Command: {' '.join(install_cmd)} [Y/n]: "
            )
            .strip()
            .lower()
        )
        if response not in ("", "y", "yes"):
            raise RuntimeError(
                f"Package '{package_name}' installation declined."
            )

        print(f"[apt] Command: {' '.join(install_cmd)}")
        subprocess.run(install_cmd, check=True)
    except (EOFError, KeyboardInterrupt):
        raise RuntimeError(f"Package '{package_name}' installation declined.")
