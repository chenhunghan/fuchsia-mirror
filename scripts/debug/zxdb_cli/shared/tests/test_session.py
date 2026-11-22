# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for zxdb-cli session directory and UDS path helpers.

Covers runtime base directory precedence, Linux AF_UNIX sun_path length bounds,
ownership and mode-0700 enforcement on XDG and /tmp directories, build-directory
and session-ID hashing, session discovery, and multi-threaded lock serialization
across socket setup and empty directory teardown.
"""

import concurrent.futures
import errno
import os
import pathlib
import socket
import stat
import tempfile
import threading
import unittest
from unittest import mock

from shared import session


class TestSessionResolution(unittest.TestCase):
    """Verifies path resolution, security checks, and locking in shared.session."""

    def test_runtime_base_dir_precedence_and_fallbacks(self) -> None:
        """Verifies env precedence, relative-path handling, and sun_path bounds."""
        # Explicit ZXDB_CLI_RUNTIME_DIR wins over XDG_RUNTIME_DIR.
        with mock.patch.dict(
            os.environ,
            {
                "ZXDB_CLI_RUNTIME_DIR": "/custom/runtime",
                "XDG_RUNTIME_DIR": "/xdg/runtime",
            },
            clear=True,
        ):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path("/custom/runtime"),
            )

        # Blank overrides are treated as unset and fall through to XDG.
        with mock.patch.dict(
            os.environ,
            {
                "ZXDB_CLI_RUNTIME_DIR": "   ",
                "XDG_RUNTIME_DIR": "/xdg/runtime",
            },
            clear=True,
        ):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path("/xdg/runtime/zxdb-cli"),
            )

        # scripts/hermetic-env strips XDG_RUNTIME_DIR and exports
        # FUCHSIA_XDG_RUNTIME_DIR_FROM_FX instead.
        with mock.patch.dict(
            os.environ,
            {
                "XDG_RUNTIME_DIR": " \t ",
                "FUCHSIA_XDG_RUNTIME_DIR_FROM_FX": "/xdg/from_fx",
            },
            clear=True,
        ):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path("/xdg/from_fx/zxdb-cli"),
            )

        # Relative XDG paths violate the XDG spec and fall through rather than
        # anchoring sockets to the caller's working directory.
        with mock.patch.dict(
            os.environ,
            {
                "XDG_RUNTIME_DIR": "relative/xdg",
                "FUCHSIA_XDG_RUNTIME_DIR_FROM_FX": "/xdg/from_fx",
            },
            clear=True,
        ):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path("/xdg/from_fx/zxdb-cli"),
            )

        with mock.patch.dict(
            os.environ,
            {"XDG_RUNTIME_DIR": "relative/xdg"},
            clear=True,
        ):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path(f"/tmp/zxdb-cli-{os.getuid()}"),
            )

        # Overlong XDG paths silently fall back to /tmp/zxdb-cli-<uid> so deep
        # session directories don't break AF_UNIX socket creation.
        with mock.patch.dict(
            os.environ,
            {"XDG_RUNTIME_DIR": "/" + "x" * 70},
            clear=True,
        ):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path(f"/tmp/zxdb-cli-{os.getuid()}"),
            )

        # Explicit ZXDB_CLI_RUNTIME_DIR overrides fail fast when relative or
        # too long rather than silently redirecting test sockets to /tmp.
        for invalid_override in ("relative/runtime", "/" + "x" * 70):
            with (
                self.subTest(invalid_override=invalid_override),
                mock.patch.dict(
                    os.environ,
                    {"ZXDB_CLI_RUNTIME_DIR": invalid_override},
                    clear=True,
                ),
            ):
                with self.assertRaises(ValueError):
                    session.get_runtime_base_dir()

        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(
                session.get_runtime_base_dir(),
                pathlib.Path(f"/tmp/zxdb-cli-{os.getuid()}"),
            )

    def test_tmp_runtime_dir_rejects_symlink_and_wrong_owner(self) -> None:
        """Verifies write-path checks reject symlinks, wrong UIDs, and group/other bits."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            tmp_root = pathlib.Path(tmp_dir)
            real_target = tmp_root / "real_target"
            real_target.mkdir()
            symlink_base = tmp_root / "zxdb-cli-symlink"
            symlink_base.symlink_to(real_target)

            with (
                mock.patch.object(
                    session,
                    "_tmp_runtime_base_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=symlink_base,
                ),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                with self.assertRaises(PermissionError):
                    session.get_build_dir_runtime_dir(ensure_exists=True)

            # Wrap session.os rather than mutating global os so pathlib and
            # tempfile still call real os functions while UID comparison fails.
            owned_dir = tmp_root / "zxdb-cli-wrong-uid"
            real_uid = os.getuid()
            with (
                mock.patch.object(
                    session,
                    "_tmp_runtime_base_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=owned_dir,
                ),
                mock.patch.object(
                    session, "os", wraps=os, spec_set=True
                ) as mock_os,
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                mock_os.getuid = mock.create_autospec(
                    os.getuid, spec_set=True, return_value=real_uid + 1
                )
                with self.assertRaises(PermissionError):
                    session.get_build_dir_runtime_dir(ensure_exists=True)

            permissive_dir = tmp_root / "zxdb-cli-permissive"
            permissive_dir.mkdir(mode=0o755)
            permissive_dir.chmod(0o755)
            with (
                mock.patch.object(
                    session,
                    "_tmp_runtime_base_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=permissive_dir,
                ),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                with self.assertRaises(PermissionError):
                    session.get_build_dir_runtime_dir(ensure_exists=True)

            # XDG_RUNTIME_DIR pointing at a shared directory such as /tmp must
            # enforce the same symlink and 0700 checks on <xdg>/zxdb-cli.
            xdg_root = tmp_root / "shared_xdg"
            xdg_root.mkdir(mode=0o755)
            xdg_sub = xdg_root / "zxdb-cli"
            xdg_sub.symlink_to(real_target)
            with mock.patch.dict(
                os.environ, {"XDG_RUNTIME_DIR": str(xdg_root)}, clear=True
            ):
                with self.assertRaises(PermissionError):
                    session.get_build_dir_runtime_dir(ensure_exists=True)
            xdg_sub.unlink()
            xdg_sub.mkdir(mode=0o755)
            xdg_sub.chmod(0o755)
            with mock.patch.dict(
                os.environ, {"XDG_RUNTIME_DIR": str(xdg_root)}, clear=True
            ):
                with self.assertRaises(PermissionError):
                    session.get_build_dir_runtime_dir()

    def test_tmp_runtime_dir_read_and_cleanup_when_missing_or_valid(
        self,
    ) -> None:
        """Verifies read-only queries and cleanup don't create missing directories."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            tmp_root = pathlib.Path(tmp_dir)
            missing_dir = tmp_root / "zxdb-cli-missing"
            with (
                mock.patch.object(
                    session,
                    "_tmp_runtime_base_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=missing_dir,
                ),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                runtime_dir = session.get_build_dir_runtime_dir()
                self.assertEqual(runtime_dir.parent, missing_dir)
                self.assertFalse(missing_dir.exists())
                self.assertSequenceEqual(session.list_session_ids(), ())
                sock = session.get_daemon_socket_path("123")
                session.cleanup_session_dir(sock)
                self.assertFalse(missing_dir.exists())

            valid_dir = tmp_root / "zxdb-cli-valid"
            valid_dir.mkdir(mode=0o700)
            valid_dir.chmod(0o700)
            with (
                mock.patch.object(
                    session,
                    "_tmp_runtime_base_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=valid_dir,
                ),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                self.assertEqual(
                    session.get_build_dir_runtime_dir().parent, valid_dir
                )
                self.assertSequenceEqual(session.list_session_ids(), ())
                _ = session.get_daemon_socket_path("123")

    def test_tmp_runtime_dir_read_and_cleanup_rejects_untrusted(self) -> None:
        """Verifies read-only queries and cleanup still reject pre-created untrusted dirs."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            tmp_root = pathlib.Path(tmp_dir)
            real_target = tmp_root / "real_target"
            real_target.mkdir()
            symlink_base = tmp_root / "zxdb-cli-symlink-read"
            symlink_base.symlink_to(real_target)
            permissive_dir = tmp_root / "zxdb-cli-permissive-read"
            permissive_dir.mkdir(mode=0o755)
            permissive_dir.chmod(0o755)
            for untrusted in (symlink_base, permissive_dir):
                with (
                    self.subTest(untrusted=untrusted.name),
                    mock.patch.object(
                        session,
                        "_tmp_runtime_base_dir",
                        autospec=True,
                        spec_set=True,
                        return_value=untrusted,
                    ),
                    mock.patch.dict(os.environ, {}, clear=True),
                ):
                    with self.assertRaises(PermissionError):
                        session.get_build_dir_runtime_dir()
                    with self.assertRaises(PermissionError):
                        session.list_session_ids()
                    with self.assertRaises(PermissionError):
                        session.get_daemon_socket_path("123")
                    with self.assertRaises(PermissionError):
                        session.cleanup_session_dir(
                            untrusted / "default" / "123" / "daemon.sock"
                        )

            owned_dir = tmp_root / "zxdb-cli-wrong-uid-read"
            owned_dir.mkdir(mode=0o700)
            owned_dir.chmod(0o700)
            real_uid = os.getuid()
            with (
                mock.patch.object(
                    session,
                    "_tmp_runtime_base_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=owned_dir,
                ),
                mock.patch.object(
                    session, "os", wraps=os, spec_set=True
                ) as mock_os,
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                mock_os.getuid = mock.create_autospec(
                    os.getuid, spec_set=True, return_value=real_uid + 1
                )
                with self.assertRaises(PermissionError):
                    session.list_session_ids()
                with self.assertRaises(PermissionError):
                    session.cleanup_session_dir(
                        owned_dir / "default" / "123" / "daemon.sock"
                    )

    def test_build_dir_hash_normalization_and_relative_fuchsia_dir(
        self,
    ) -> None:
        """Verifies relative build dirs and symlinks hash to the same canonical key."""
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(session.get_build_dir_hash(), "default")

        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            workspace = pathlib.Path(tmp_dir) / "fuchsia"
            build_dir = workspace / "out" / "default"
            subdir = workspace / "scripts" / "debug"
            build_dir.mkdir(parents=True)
            subdir.mkdir(parents=True)
            symlink_build = workspace / "out" / "current"
            symlink_build.symlink_to(build_dir)

            with mock.patch.dict(
                os.environ,
                {
                    "FUCHSIA_BUILD_DIR_FROM_FX": "   ",
                    "FUCHSIA_BUILD_DIR": f"{build_dir}/",
                },
                clear=True,
            ):
                abs_hash = session.get_build_dir_hash()
                self.assertEqual(len(abs_hash), 12)

            # Anchor relative build paths against FUCHSIA_DIR rather than cwd so
            # invocations from subdirectories resolve the same daemon runtime.
            original_cwd = os.getcwd()
            try:
                os.chdir(subdir)
                with mock.patch.dict(
                    os.environ,
                    {
                        "FUCHSIA_DIR": str(workspace),
                        "FUCHSIA_BUILD_DIR_FROM_FX": "out/default",
                    },
                    clear=True,
                ):
                    rel_hash = session.get_build_dir_hash()

                with mock.patch.dict(
                    os.environ,
                    {
                        "FUCHSIA_DIR": str(workspace),
                        "FUCHSIA_BUILD_DIR_FROM_FX": "out/current",
                    },
                    clear=True,
                ):
                    symlink_hash = session.get_build_dir_hash()
            finally:
                os.chdir(original_cwd)

            self.assertEqual(abs_hash, rel_hash)
            self.assertEqual(abs_hash, symlink_hash)

    def test_session_id_normalization_and_sanitization(self) -> None:
        """Verifies reserved-name rejection and collision-free hash truncation."""
        with (
            tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir,
            mock.patch.dict(
                os.environ,
                {
                    "ZXDB_CLI_RUNTIME_DIR": tmp_dir,
                    "FUCHSIA_BUILD_DIR_FROM_FX": "/out/default",
                },
                clear=True,
            ),
        ):
            self.assertIsNone(session.normalize_session_id(None))
            self.assertIsNone(session.normalize_session_id(""))
            self.assertIsNone(session.normalize_session_id("   "))
            self.assertEqual(session.normalize_session_id(" 12345 "), "12345")

            for reserved in (
                "default",
                "Default",
                ".",
                "..",
                ".lock",
                ".foo",
                "./foo",
            ):
                with self.subTest(reserved=reserved):
                    with self.assertRaises(ValueError):
                        session.normalize_session_id(reserved)
                    with self.assertRaises(ValueError):
                        session.get_daemon_socket_path(reserved)

            canonical_slash = session.normalize_session_id(
                "session/with:slashes"
            )
            self.assertIsNotNone(canonical_slash)
            assert canonical_slash is not None
            self.assertEqual(len(canonical_slash), 24)
            self.assertTrue(canonical_slash.startswith("session_with_sl_"))
            sock_path = session.get_daemon_socket_path("session/with:slashes")
            self.assertEqual(sock_path.name, "daemon.sock")
            self.assertEqual(sock_path.parent.name, canonical_slash)

            # Distinct unsafe inputs that map to the same underscore prefix still
            # produce distinct canonical IDs via their trailing SHA-256 digest.
            slash_id = session.normalize_session_id("a/b")
            colon_id = session.normalize_session_id("a:b")
            underscore_id = session.normalize_session_id("a_b")
            self.assertEqual(len({slash_id, colon_id, underscore_id}), 3)

            long_a = "a" * 30 + ":"
            long_b = "a" * 30 + "/"
            sanitized_a = session.get_daemon_socket_path(long_a).parent.name
            sanitized_b = session.get_daemon_socket_path(long_b).parent.name
            self.assertEqual(len(sanitized_a), 24)
            self.assertEqual(len(sanitized_b), 24)
            self.assertTrue(sanitized_a.startswith("a" * 15 + "_"))
            self.assertNotEqual(sanitized_a, sanitized_b)

            self.assertEqual(
                session.get_default_session_id(), str(os.getsid(0))
            )

            with self.assertRaises(ValueError):
                session.get_daemon_socket_path("   ")

    def test_merge_session_ids(self) -> None:
        """Verifies top-level and subcommand --session flags merge or reject conflicts."""
        self.assertIsNone(session.merge_session_ids(None, None))
        self.assertEqual(session.merge_session_ids("1111", None), "1111")
        self.assertEqual(session.merge_session_ids(None, "2222"), "2222")
        self.assertEqual(session.merge_session_ids("1111", "1111"), "1111")
        with self.assertRaisesRegex(ValueError, "Conflicting session IDs"):
            session.merge_session_ids("1111", "2222")

    def test_prepare_list_and_cleanup_session_dirs(self) -> None:
        """Verifies socket preparation, session discovery, and empty directory teardown."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            with mock.patch.dict(
                os.environ,
                {
                    "ZXDB_CLI_RUNTIME_DIR": tmp_dir,
                    "FUCHSIA_BUILD_DIR_FROM_FX": "/out/default",
                },
                clear=True,
            ):
                # prepare_session_socket tightens permissions to 0700 and clears
                # stale socket files while holding .lock.
                stale_created = session.get_daemon_socket_path("created")
                stale_created.parent.mkdir(
                    parents=True, exist_ok=True, mode=0o755
                )
                stale_created.parent.chmod(0o755)
                stale_created.write_text("stale")
                with session.prepare_session_socket("created") as prepared_sock:
                    self.assertEqual(prepared_sock, stale_created)
                    self.assertLessEqual(len(str(prepared_sock)), 107)
                    self.assertTrue(prepared_sock.parent.is_dir())
                    self.assertFalse(prepared_sock.exists())
                    self.assertEqual(
                        stat.S_IMODE(prepared_sock.parent.stat().st_mode), 0o700
                    )

                # Symlinked, dangling-symlinked, and regular-file session paths
                # raise PermissionError instead of following symlinks or leaking
                # FileExistsError from mkdir.
                build_runtime_dir = session.get_build_dir_runtime_dir(
                    ensure_exists=True
                )
                outside_dir = pathlib.Path(tmp_dir) / "outside_dir"
                outside_dir.mkdir(mode=0o700)
                (build_runtime_dir / "linked-sess").symlink_to(outside_dir)
                (build_runtime_dir / "dangling-sess").symlink_to(
                    pathlib.Path(tmp_dir) / "nonexistent_target"
                )
                (build_runtime_dir / "file-sess").write_text("not a directory")
                for invalid_sid in (
                    "linked-sess",
                    "dangling-sess",
                    "file-sess",
                ):
                    with self.subTest(invalid_sid=invalid_sid):
                        with self.assertRaises(PermissionError):
                            with session.prepare_session_socket(invalid_sid):
                                pass

                # list_session_ids reports sorted real session dirs that contain
                # daemon.sock while ignoring hidden, reserved, symlinked,
                # non-directory, or empty entries.
                sock_b = session.get_daemon_socket_path("sess-b")
                sock_a = session.get_daemon_socket_path("sess-a")
                sock_b.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                sock_b.touch()
                sock_a.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                sock_a.touch()
                (outside_dir / "daemon.sock").touch()
                (build_runtime_dir / ".hidden").mkdir(exist_ok=True)
                (build_runtime_dir / ".hidden" / "daemon.sock").touch()
                (build_runtime_dir / "default").mkdir(exist_ok=True)
                (build_runtime_dir / "default" / "daemon.sock").touch()
                self.assertSequenceEqual(
                    session.list_session_ids(), ("sess-a", "sess-b")
                )

                # Concurrent removal races on iterdir return (), whereas
                # PermissionError propagates loudly instead of returning ().
                mock_dir = mock.create_autospec(
                    pathlib.Path, instance=True, spec_set=True
                )
                mock_dir.__str__.return_value = str(build_runtime_dir)
                with mock.patch.object(
                    session,
                    "get_build_dir_runtime_dir",
                    autospec=True,
                    spec_set=True,
                    return_value=mock_dir,
                ):
                    for race_err in (
                        FileNotFoundError("removed"),
                        NotADirectoryError("replaced"),
                    ):
                        mock_dir.iterdir.side_effect = race_err
                        self.assertSequenceEqual(session.list_session_ids(), ())
                    mock_dir.iterdir.side_effect = PermissionError("denied")
                    with self.assertRaises(PermissionError):
                        session.list_session_ids()

                if os.geteuid() != 0:
                    build_runtime_dir.chmod(0o000)
                    try:
                        with self.assertRaises(PermissionError):
                            session.list_session_ids()
                    finally:
                        build_runtime_dir.chmod(0o700)

                # Empty session directory is removed once daemon.sock is unlinked,
                # while non-empty directories retaining logs or artifacts stay.
                sock_a.unlink()
                session.cleanup_session_dir(sock_a)
                self.assertFalse(sock_a.parent.exists())
                self.assertSequenceEqual(
                    session.list_session_ids(), ("sess-b",)
                )

                (sock_b.parent / "daemon.log").write_text("keep")
                sock_b.unlink()
                session.cleanup_session_dir(sock_b)
                self.assertTrue(sock_b.parent.is_dir())
                self.assertSequenceEqual(session.list_session_ids(), ())

    def test_concurrent_prepare_session_socket_and_cleanup_same_id(
        self,
    ) -> None:
        """Verifies prepare_session_socket prevents cleanup from deleting session_dir before bind."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            with mock.patch.dict(
                os.environ,
                {
                    "ZXDB_CLI_RUNTIME_DIR": tmp_dir,
                    "FUCHSIA_BUILD_DIR_FROM_FX": "/out/default",
                },
                clear=True,
            ):
                target_sock = session.get_daemon_socket_path("reused-sid")
                bind_lock = threading.Lock()

                def bind_cycle() -> None:
                    with bind_lock:
                        with session.prepare_session_socket(
                            "reused-sid"
                        ) as sock_path:
                            srv = socket.socket(
                                socket.AF_UNIX, socket.SOCK_STREAM
                            )
                            try:
                                srv.bind(str(sock_path))
                                srv.listen(8)
                            finally:
                                srv.close()
                                sock_path.unlink(missing_ok=True)

                def cleanup_cycle() -> None:
                    session.cleanup_session_dir(target_sock)

                with concurrent.futures.ThreadPoolExecutor(
                    max_workers=4
                ) as pool:
                    futures = []
                    for _ in range(25):
                        futures.append(pool.submit(bind_cycle))
                        futures.append(pool.submit(cleanup_cycle))
                    for fut in futures:
                        fut.result()

    def test_cleanup_ignores_out_of_tree_paths(self) -> None:
        """Verifies custom --socket-path values outside the runtime tree are ignored."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            runtime_dir = pathlib.Path(tmp_dir) / "runtime"
            other_root = pathlib.Path(tmp_dir) / "other"
            custom_dir = other_root / "custom"
            custom_dir.mkdir(parents=True)
            custom_sock = custom_dir / "daemon.sock"

            with mock.patch.dict(
                os.environ,
                {
                    "ZXDB_CLI_RUNTIME_DIR": str(runtime_dir),
                    "FUCHSIA_BUILD_DIR_FROM_FX": "/out/default",
                },
                clear=True,
            ):
                session.cleanup_session_dir(custom_sock)
                self.assertTrue(custom_dir.is_dir())

    def test_cleanup_rejects_lock_symlink(self) -> None:
        """Verifies _runtime_lock opens .lock with O_NOFOLLOW and rejects symlinks."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            runtime_dir = pathlib.Path(tmp_dir) / "rt"
            outside_target = pathlib.Path(tmp_dir) / "outside-lock"
            with mock.patch.dict(
                os.environ,
                {
                    "ZXDB_CLI_RUNTIME_DIR": str(runtime_dir),
                    "FUCHSIA_BUILD_DIR_FROM_FX": "/out/default",
                },
                clear=True,
            ):
                build_runtime_dir = session.get_build_dir_runtime_dir(
                    ensure_exists=True
                )
                sock = session.get_daemon_socket_path("sess")
                sock.parent.mkdir(parents=True, exist_ok=True)
                (build_runtime_dir / ".lock").symlink_to(outside_target)
                with self.assertRaises(OSError) as ctx:
                    session.cleanup_session_dir(sock)
                self.assertEqual(ctx.exception.errno, errno.ELOOP)
                self.assertFalse(outside_target.exists())

    def test_short_symlink_runtime_dir_stays_within_sun_path_limit(
        self,
    ) -> None:
        """Verifies unresolved ZXDB_CLI_RUNTIME_DIR symlinks stay within sun_path bounds."""
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp_dir:
            tmp_root = pathlib.Path(tmp_dir)
            deep_target = tmp_root / ("deep_" + "x" * 75)
            deep_target.mkdir(parents=True, mode=0o700)
            short_link = tmp_root / "rt"
            short_link.symlink_to(deep_target)

            with mock.patch.dict(
                os.environ,
                {
                    "ZXDB_CLI_RUNTIME_DIR": str(short_link),
                    "FUCHSIA_BUILD_DIR_FROM_FX": "/out/default",
                },
                clear=True,
            ):
                with session.prepare_session_socket("sess1") as sock_path:
                    self.assertLessEqual(len(os.fsencode(sock_path)), 107)
                    self.assertGreater(
                        len(os.fsencode(sock_path.resolve())), 108
                    )
                    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    try:
                        srv.bind(str(sock_path))
                        self.assertSequenceEqual(
                            session.list_session_ids(), ("sess1",)
                        )
                    finally:
                        srv.close()
                        sock_path.unlink(missing_ok=True)
                session.cleanup_session_dir(sock_path.resolve())
                self.assertFalse(sock_path.parent.exists())


if __name__ == "__main__":
    unittest.main()
