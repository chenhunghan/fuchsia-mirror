# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Session directory and Unix domain socket path resolution for zxdb-cli.

Runtime state is organized in a two-level hierarchy beneath a per-user base
directory so concurrent checkouts, build directories, and debug sessions never
collide on a single global socket path:

    <base_dir>/<build_hash>/
        .lock                   Advisory flock file serializing directory setup
        <session_id>/
            daemon.sock         Unix domain stream socket for zxdb-daemon

Keeping each session inside its own mode-0700 directory gives the socket and
future per-session artifacts such as log files a single home without lengthening
the socket path compared to a flat filename.

Base directory resolution checks ZXDB_CLI_RUNTIME_DIR for hermetic tests, then
XDG_RUNTIME_DIR or FUCHSIA_XDG_RUNTIME_DIR_FROM_FX, and falls back to
/tmp/zxdb-cli-<uid> whenever XDG is unset or would push daemon.sock past the
107-byte Linux AF_UNIX sun_path limit. Non-override base directories must be
real mode-0700 directories owned by the current UID.

Directory creation and socket setup in prepare_session_socket and empty
directory removal in cleanup_session_dir serialize on .lock so concurrent
teardown can't remove a newly created session directory before bind completes.
"""

import contextlib
import errno
import fcntl
import hashlib
import os
import pathlib
import re
import stat
from collections.abc import Iterator, Sequence

# Fallback directory key when neither FUCHSIA_BUILD_DIR_FROM_FX nor
# FUCHSIA_BUILD_DIR is set in the environment.
_DEFAULT_BUILD_DIR_KEY = "default"
# Reserved directory name rejected as a session ID so future symlinks or
# metadata entries can't collide with a session directory.
_RESERVED_SESSION_NAME = "default"
_DAEMON_SOCKET_FILENAME = "daemon.sock"
_RUNTIME_LOCK_FILENAME = ".lock"

# Bound build-hash and session-ID segment lengths so worst-case socket paths fit
# comfortably inside the 107-byte Linux AF_UNIX sun_path budget.
_BUILD_DIR_HASH_LENGTH = 12
_MAX_IDENTIFIER_LENGTH = 24
# 108-byte Linux AF_UNIX sun_path limit minus the trailing NUL byte.
_MAX_SOCKET_PATH_LENGTH = 107
_IDENTIFIER_HASH_LENGTH = 8
_SAFE_IDENTIFIER_RE = re.compile(r"[^A-Za-z0-9._-]")


def _first_nonempty_env(*names: str) -> str | None:
    """Returns the first stripped non-empty environment variable among names."""
    for name in names:
        if (raw := os.environ.get(name)) is not None and (
            stripped := raw.strip()
        ):
            return stripped
    return None


def _tmp_runtime_base_dir() -> pathlib.Path:
    """Returns the per-UID fallback runtime base directory under /tmp."""
    return pathlib.Path(f"/tmp/zxdb-cli-{os.getuid()}")


def _max_socket_path_bytes(base_dir: pathlib.Path) -> int:
    """Computes the worst-case encoded daemon.sock byte length under base_dir."""
    sample_sock = (
        base_dir
        / ("x" * _BUILD_DIR_HASH_LENGTH)
        / ("x" * _MAX_IDENTIFIER_LENGTH)
        / _DAEMON_SOCKET_FILENAME
    )
    return len(os.fsencode(sample_sock))


def _first_valid_xdg_runtime_dir() -> pathlib.Path | None:
    """Resolves the first usable XDG runtime subdirectory within sun_path bounds.

    Fuchsia's scripts/hermetic-env strips XDG_RUNTIME_DIR and forwards the
    original value in FUCHSIA_XDG_RUNTIME_DIR_FROM_FX, so both variables are
    checked in order. Relative paths violate the XDG Base Directory
    specification and are ignored rather than resolved against the caller's
    working directory.
    """
    for name in ("XDG_RUNTIME_DIR", "FUCHSIA_XDG_RUNTIME_DIR_FROM_FX"):
        if (raw := os.environ.get(name)) is None or not (
            stripped := raw.strip()
        ):
            continue
        xdg_path = pathlib.Path(stripped)
        if not xdg_path.is_absolute():
            continue
        candidate = xdg_path / "zxdb-cli"
        if _max_socket_path_bytes(candidate) <= _MAX_SOCKET_PATH_LENGTH:
            return candidate
    return None


def _ensure_secure_runtime_dir(
    base_dir: pathlib.Path, *, ensure_exists: bool
) -> None:
    """Verifies that base_dir is a real mode-0700 directory owned by this UID.

    Hermetic tests that set ZXDB_CLI_RUNTIME_DIR explicitly may point at test
    fixtures or short symlinks into deep temporary trees, so strict ownership
    and mode checks apply only to XDG and /tmp runtime roots. Read-only callers
    pass ensure_exists=False so querying paths doesn't create directories on
    disk when none exist yet, while still rejecting untrusted directories that
    were pre-created by another user.
    """
    if _first_nonempty_env("ZXDB_CLI_RUNTIME_DIR") is not None:
        if ensure_exists:
            base_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        return
    try:
        st = os.lstat(base_dir)
    except FileNotFoundError:
        if not ensure_exists:
            return
        base_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        st = os.lstat(base_dir)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        raise PermissionError(
            f"Runtime directory {base_dir} must be a real directory, not a symlink or special file."
        )
    if st.st_uid != os.getuid():
        raise PermissionError(
            f"Runtime directory {base_dir} is not owned by UID {os.getuid()}."
        )
    if (stat.S_IMODE(st.st_mode) & 0o077) != 0:
        raise PermissionError(
            f"Runtime directory {base_dir} has insecure permissions "
            f"{oct(stat.S_IMODE(st.st_mode))}."
        )


def _sanitize_identifier(raw: str) -> str:
    """Normalizes a required session ID, rejecting empty or whitespace input."""
    normalized = normalize_session_id(raw)
    if normalized is None:
        raise ValueError("Session ID cannot be empty.")
    return normalized


@contextlib.contextmanager
def _runtime_lock(build_runtime_dir: pathlib.Path) -> Iterator[None]:
    """Acquires an exclusive advisory flock on the build runtime lock file.

    Opening .lock with O_NOFOLLOW rejects symlink attacks inside the runtime
    directory. Because locks placed with flock are associated with an open file
    description rather than the process, independent file descriptors in threads
    or child processes serialize cleanly against each other.
    """
    lock_path = build_runtime_dir / _RUNTIME_LOCK_FILENAME
    fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)


def _remove_empty_session_dir(session_dir: pathlib.Path) -> None:
    """Removes session_dir if it is a real, empty session directory."""
    if (
        session_dir.name.casefold() == _RESERVED_SESSION_NAME
        or session_dir.name.startswith(".")
        or os.path.islink(str(session_dir))
        or not os.path.isdir(str(session_dir))
    ):
        return
    try:
        session_dir.rmdir()
    except OSError as err:
        # Leave the directory intact when per-session logs or artifacts remain,
        # or when another caller already removed it.
        if err.errno not in (errno.ENOTEMPTY, errno.EEXIST, errno.ENOENT):
            raise


def normalize_session_id(raw_session: str | None) -> str | None:
    """Canonicalizes an optional session ID into a safe directory name.

    Empty or whitespace-only values normalize to None so CLI flags can treat
    blank overrides as unset. Names containing characters outside
    [A-Za-z0-9._-] or exceeding 24 characters are sanitized and suffixed with
    an 8-character SHA-256 digest of the stripped input, keeping paths within
    sun_path limits while preventing collisions such as 'a/b' vs 'a_b'.

    Args:
        raw_session: Raw session identifier, or None. Must not match reserved
            directory names ('default' in any case, or any name starting with
            '.' such as '.', '..', or '.lock').

    Returns:
        Canonical directory-safe session ID up to 24 characters, or None when
        raw_session is None or whitespace.
    """
    if raw_session is None:
        return None
    stripped = str(raw_session).strip()
    if not stripped:
        return None
    if stripped.casefold() == _RESERVED_SESSION_NAME or stripped.startswith(
        "."
    ):
        raise ValueError(
            f"Session ID '{stripped}' is reserved and cannot be used."
        )
    sanitized = _SAFE_IDENTIFIER_RE.sub("_", stripped)
    if sanitized == stripped and len(sanitized) <= _MAX_IDENTIFIER_LENGTH:
        return sanitized
    digest = hashlib.sha256(stripped.encode("utf-8")).hexdigest()[
        :_IDENTIFIER_HASH_LENGTH
    ]
    prefix_len = _MAX_IDENTIFIER_LENGTH - _IDENTIFIER_HASH_LENGTH - 1
    return f"{sanitized[:prefix_len]}_{digest}"


def merge_session_ids(first: str | None, second: str | None) -> str | None:
    """Combines two optional normalized session IDs and rejects mismatches.

    Args:
        first: Normalized session ID from the outer flag or source, or None.
        second: Normalized session ID from the inner flag or payload, or None.

    Returns:
        The non-None session ID when at least one input is set, or None when
        both inputs are None.

    Raises:
        ValueError: Both inputs are non-None and specify different session IDs.
    """
    if first is not None and second is not None and first != second:
        raise ValueError(
            f"Conflicting session IDs provided: '{first}' and '{second}'"
        )
    return first if first is not None else second


def get_runtime_base_dir() -> pathlib.Path:
    """Resolves the base runtime directory for zxdb-cli sockets and state.

    Checks ZXDB_CLI_RUNTIME_DIR first for hermetic testing, followed by
    XDG_RUNTIME_DIR/zxdb-cli (or FUCHSIA_XDG_RUNTIME_DIR_FROM_FX/zxdb-cli under
    scripts/hermetic-env), and falls back to /tmp/zxdb-cli-<uid> when unset or
    when the XDG path would push socket paths past the 107-byte Linux AF_UNIX
    sun_path limit.

    Returns:
        Unresolved base runtime directory path. Callers keep this path
        unresolved so short symlink overrides stay under sun_path limits.

    Raises:
        ValueError: ZXDB_CLI_RUNTIME_DIR is set to a relative path or yields
            socket paths exceeding 107 bytes.
    """
    if (runtime_dir := _first_nonempty_env("ZXDB_CLI_RUNTIME_DIR")) is not None:
        candidate = pathlib.Path(runtime_dir)
        if not candidate.is_absolute():
            raise ValueError(
                f"ZXDB_CLI_RUNTIME_DIR '{candidate}' must be an absolute path."
            )
        max_bytes = _max_socket_path_bytes(candidate)
        if max_bytes > _MAX_SOCKET_PATH_LENGTH:
            raise ValueError(
                f"ZXDB_CLI_RUNTIME_DIR '{candidate}' yields socket paths up to "
                f"{max_bytes} bytes (limit {_MAX_SOCKET_PATH_LENGTH})."
            )
        return candidate
    if (xdg_candidate := _first_valid_xdg_runtime_dir()) is not None:
        return xdg_candidate
    return _tmp_runtime_base_dir()


def get_build_dir_hash() -> str:
    """Computes a 12-hex-character key for the active Fuchsia build directory.

    Reads FUCHSIA_BUILD_DIR_FROM_FX or FUCHSIA_BUILD_DIR, anchored against
    FUCHSIA_DIR when relative, and resolves symlinks via os.path.realpath so
    equivalent paths (such as out/default and an out/current symlink) share the
    same runtime subdirectory. Returns 'default' when neither build variable is
    set.
    """
    raw_dir = _first_nonempty_env(
        "FUCHSIA_BUILD_DIR_FROM_FX", "FUCHSIA_BUILD_DIR"
    )
    if raw_dir is None:
        return _DEFAULT_BUILD_DIR_KEY
    if (
        not os.path.isabs(raw_dir)
        and (fuchsia_dir := _first_nonempty_env("FUCHSIA_DIR")) is not None
    ):
        candidate_dir = os.path.join(fuchsia_dir, raw_dir)
    else:
        candidate_dir = raw_dir
    norm_dir = os.path.realpath(candidate_dir)
    return hashlib.sha256(norm_dir.encode("utf-8")).hexdigest()[
        :_BUILD_DIR_HASH_LENGTH
    ]


def get_build_dir_runtime_dir(ensure_exists: bool = False) -> pathlib.Path:
    """Returns the build-scoped runtime directory under the base runtime root.

    Args:
        ensure_exists: When True, creates both the base runtime directory and
            the build-scoped subdirectory with mode 0700.

    Returns:
        Path to <base_dir>/<build_hash>.

    Raises:
        PermissionError: The resolved XDG or /tmp base directory is a symlink,
            not owned by the current UID, or accessible to group/other users.
    """
    base_dir = get_runtime_base_dir()
    build_runtime_dir = base_dir / get_build_dir_hash()
    _ensure_secure_runtime_dir(base_dir, ensure_exists=ensure_exists)
    if ensure_exists:
        build_runtime_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    return build_runtime_dir


def get_default_session_id() -> str:
    """Returns a fallback session identifier derived from os.getsid(0).

    Used when zxdb-daemon is invoked directly without --session-id. Detached
    daemons spawned by DaemonManager with start_new_session=True receive an
    explicit --session-id from their spawner.
    """
    return _sanitize_identifier(str(os.getsid(0)))


def get_session_dir(session_id: str) -> pathlib.Path:
    """Returns the per-session directory path without creating it on disk.

    Args:
        session_id: Non-empty, non-reserved session identifier.
    """
    sid = _sanitize_identifier(session_id)
    return get_build_dir_runtime_dir() / sid


@contextlib.contextmanager
def prepare_session_socket(session_id: str) -> Iterator[pathlib.Path]:
    """Creates a mode-0700 session directory and clears stale sockets under .lock.

    Daemons binding their Unix domain socket must bind inside this context
    manager so .lock remains held until daemon.sock exists on disk. Holding
    .lock across directory creation, stale socket removal, and bind prevents a
    concurrent cleanup_session_dir call for the same session_id from removing
    the newly created empty directory before bind completes.

    Args:
        session_id: Non-empty, non-reserved session identifier.

    Yields:
        Path where the caller should bind daemon.sock while .lock is held.

    Raises:
        PermissionError: The runtime base directory is untrusted or the target
            session directory path is not a real directory.
    """
    sid = _sanitize_identifier(session_id)
    build_runtime_dir = get_build_dir_runtime_dir(ensure_exists=True)
    session_dir = build_runtime_dir / sid
    socket_path = session_dir / _DAEMON_SOCKET_FILENAME
    with _runtime_lock(build_runtime_dir):
        try:
            st = os.lstat(session_dir)
        except FileNotFoundError:
            session_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
            st = os.lstat(session_dir)
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            raise PermissionError(
                f"Session directory {session_dir} must be a real directory."
            )
        session_dir.chmod(0o700)
        if socket_path.exists() or os.path.islink(str(socket_path)):
            socket_path.unlink(missing_ok=True)
        yield socket_path


def get_daemon_socket_path(session_id: str) -> pathlib.Path:
    """Returns <build_runtime_dir>/<sanitized_session_id>/daemon.sock.

    Args:
        session_id: Non-empty, non-reserved session identifier.
    """
    return get_session_dir(session_id=session_id) / _DAEMON_SOCKET_FILENAME


def list_session_ids() -> Sequence[str]:
    """Returns sorted session IDs with socket entries in the build runtime dir.

    Inspects the active build runtime directory without creating missing
    directories on disk, while still enforcing ownership and mode-0700 checks
    on non-override runtime roots. Ignores hidden entries, reserved names,
    symlinks, and directories without a daemon.sock entry.
    """
    build_runtime_dir = get_build_dir_runtime_dir()
    if not os.path.isdir(str(build_runtime_dir)):
        return ()
    try:
        entries = tuple(build_runtime_dir.iterdir())
    except (FileNotFoundError, NotADirectoryError):
        return ()
    session_ids = (
        entry.name
        for entry in entries
        if entry.name.casefold() != _RESERVED_SESSION_NAME
        and not entry.name.startswith(".")
        and not os.path.islink(str(entry))
        and os.path.isdir(str(entry))
        and os.path.lexists(str(entry / _DAEMON_SOCKET_FILENAME))
    )
    return tuple(sorted(session_ids))


def cleanup_session_dir(socket_path: pathlib.Path) -> None:
    """Removes an empty per-session runtime directory under .lock after teardown.

    Ignores custom socket paths located outside the active build runtime
    directory and leaves non-empty session directories intact when per-session
    logs or artifacts remain.

    Args:
        socket_path: Daemon socket path being torn down (resolved or unresolved).
    """
    build_runtime_dir = get_build_dir_runtime_dir()
    resolved_socket = pathlib.Path(socket_path).resolve()
    if resolved_socket.parent.parent != build_runtime_dir.resolve():
        return
    if not os.path.isdir(str(build_runtime_dir)):
        return
    session_dir = build_runtime_dir / resolved_socket.parent.name
    with _runtime_lock(build_runtime_dir):
        _remove_empty_session_dir(session_dir)
