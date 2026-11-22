# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Manages and isolates Google Cloud SDK credentials for the Fuchsia development environment."""
import json
import pathlib
import sys
from collections.abc import Sequence
from typing import Any, TextIO

from build.auth import apt, gcloud

# Type alias representing a parsed JSON dictionary.
_JsonDict = dict[str, Any]

# Filename for the workspace-isolated credentials copy.
_ISOLATED_ADC_FILENAME = gcloud.ADC_SUBPATH.name

# Package name for Google Cloud SDK on APT-based package managers.
_GOOGLE_CLOUD_CLI_PACKAGE = "google-cloud-cli"

_SCRIPT = pathlib.Path(__file__)


class GcloudCredsError(Exception):
    """Base exception for all Google Cloud credentials management and isolation failures."""


def _msg(text: str, file: TextIO | None = None) -> None:
    """Print a message prefixed with the script's basename."""
    if file is None:
        file = sys.stdout
    print(f"[{_SCRIPT.name}] {text}", file=file)


def _mutate_adc_quota_project(adc_data: _JsonDict, quota_project: str) -> bool:
    """Injects or modifies the quota_project_id in ADC JSON data if applicable.

    Returns True if the credentials type is 'authorized_user' and was updated,
    otherwise False.
    """
    if adc_data.get("type") == "authorized_user":
        adc_data["quota_project_id"] = quota_project
        return True
    return False


def isolate_credentials(
    out_dir: pathlib.Path,
    quota_project: str,
    source_credentials_path: pathlib.Path | None = None,
) -> pathlib.Path | None:
    """Creates a workspace-isolated copy of standard Application Default Credentials.

    Reads the global or custom ADC, copies it to the writable shared output
    directory, injects the specified quota_project ID (only if the credentials type
    is 'authorized_user'), and returns the path to the isolated credentials file.

    Benefit of Isolation:
        Isolating credentials under the shared output directory and overriding the
        quota_project prevents RBE and ResultStore development traffic from being billed to
        or interfering with the developer's personal or other non-Fuchsia Google Cloud
        projects. It protects developer quota, prevents billing leakage, and restricts
        all development GCP API interactions securely to the specified Fuchsia project.

    Args:
        out_dir: Path to the writable shared output directory.
        quota_project: The Google Cloud quota project ID to write.
        source_credentials_path: Optional path to standard credentials file.
          If None, detects from GOOGLE_APPLICATION_CREDENTIALS or standard ADC.

    Returns:
        Path to the isolated credentials file, or None if no isolation was performed
        (e.g., for non-user service account credentials).

    Raises:
        GcloudCredsError: If no source credentials file can be located or resolved.
        json.JSONDecodeError: If the source credentials file contains malformed JSON.
        OSError: If reading the source or writing the isolated copy fails.
    """
    adc_src = source_credentials_path or gcloud.resolve_global_adc_path()
    if not adc_src.is_file():
        raise GcloudCredsError(
            f"No Application Default Credentials file found at '{adc_src}'"
        )

    adc_data: _JsonDict = json.loads(adc_src.read_text())
    if not isinstance(adc_data, dict):
        raise json.JSONDecodeError(
            "Credentials JSON is not a dictionary structure", "", 0
        )

    # Only isolate and modify standard user default credentials (authorized_user).
    if _mutate_adc_quota_project(adc_data, quota_project):
        out_dir.mkdir(parents=True, exist_ok=True)
        local_adc_path = out_dir / _ISOLATED_ADC_FILENAME
        local_adc_path.write_text(json.dumps(adc_data, indent=2))
        local_adc_path.chmod(0o600)
        return local_adc_path

    return None


def isolate_credentials_safe(
    out_dir: pathlib.Path,
    quota_project: str,
    source_credentials_path: pathlib.Path | None = None,
) -> pathlib.Path | None:
    """Exception-safe version of isolate_credentials.

    Catches all expected filesystem, parsing, and environment exceptions, and
    silently returns None on any failure, allowing callers to safely fallback.
    """
    try:
        return isolate_credentials(
            out_dir=out_dir,
            quota_project=quota_project,
            source_credentials_path=source_credentials_path,
        )
    except GcloudCredsError as e:
        # Avoid printing warnings for missing files, which is normal for unauthenticated users.
        if "No Application Default Credentials file found" not in str(e):
            _msg(
                f"Warning: Failed to isolate RBE credentials: {e}",
                file=sys.stderr,
            )
    except (OSError, json.JSONDecodeError) as e:
        _msg(
            f"Warning: Failed to isolate RBE credentials: {e}", file=sys.stderr
        )

    return None


def _ensure_gcloud_installed() -> None:
    """Verifies gcloud is in the PATH, prompting/installing if missing and available.

    Raises:
        GcloudCredsError: If gcloud is missing and cannot be automatically installed.
    """
    if gcloud.path():
        return

    if apt.exists(_GOOGLE_CLOUD_CLI_PACKAGE):
        try:
            apt.install(_GOOGLE_CLOUD_CLI_PACKAGE, interactive=True)
            # Re-verify path after successful install
            if not gcloud.path():
                raise GcloudCredsError(
                    "Installation succeeded, but 'gcloud' is still missing from PATH. "
                    "Please restart your terminal."
                )
            return
        except Exception as e:
            raise GcloudCredsError(str(e))

    raise GcloudCredsError(
        "Google Cloud SDK ('gcloud') is missing from your PATH.\n"
        "Please install the Google Cloud SDK before authenticating.\n"
        "For official, secure installation instructions, see:\n"
        "  https://cloud.google.com/sdk/docs/install"
    )


def _run_gcloud_login(extra_args: Sequence[str] = ()) -> None:
    """Launches interactive Google Cloud Application Default Credentials login."""
    _ensure_gcloud_installed()
    try:
        gcloud.login(extra_args)
    except RuntimeError as e:
        raise GcloudCredsError(str(e)) from e


def ensure_oauth(
    *,
    force: bool = False,
    out_dir: pathlib.Path | None = None,
    quota_project: str | None = None,
    gcloud_args: Sequence[str] = (),
) -> pathlib.Path | None:
    """Ensures valid gcloud ADC credentials and optionally returns an isolated copy."""
    _ensure_gcloud_installed()
    adc_src = gcloud.resolve_global_adc_path()
    if force or not adc_src.is_file() or not gcloud.has_valid_adc_token():
        _run_gcloud_login(gcloud_args)
        adc_src = gcloud.get_default_adc_path()
    if out_dir is not None and quota_project:
        return isolate_credentials_safe(
            out_dir=out_dir.resolve(),
            quota_project=quota_project,
            source_credentials_path=adc_src,
        )
    return None
