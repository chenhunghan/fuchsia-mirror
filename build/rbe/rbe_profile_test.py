#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for rbe_profile.py."""

import pathlib
import unittest
from unittest import mock

from build.rbe import rbe_profile


class RbeProfileTest(unittest.TestCase):
    """Tests targeting the RbeProfile dataclass properties, serialization, and loader."""

    def test_rbe_profile_instantiation(self) -> None:
        profile = rbe_profile.RbeProfile(
            rbe_instance="projects/fake-project/instances/fake-instance",
            remote_exec_root=pathlib.PurePosixPath("/fake/root"),
            container_image="docker://gcr.io/fake/image",
            worker_pools={
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            profile_name="fake-profile",
            os_family="fake-os",
            auth_type="fake-auth",
        )
        self.assertEqual(
            profile.rbe_instance,
            "projects/fake-project/instances/fake-instance",
        )
        self.assertIsInstance(profile.remote_exec_root, pathlib.PurePosixPath)
        self.assertEqual(
            profile.remote_exec_root, pathlib.PurePosixPath("/fake/root")
        )
        self.assertEqual(profile.container_image, "docker://gcr.io/fake/image")
        self.assertEqual(
            profile.worker_pools,
            {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
        )
        self.assertEqual(profile.profile_name, "fake-profile")
        self.assertEqual(profile.os_family, "fake-os")
        self.assertEqual(profile.auth_type, "fake-auth")
        self.assertEqual(profile.quota_project, "fake-project")

    def test_rbe_profile_from_dict_success(self) -> None:
        data = {
            "rbe_instance": "projects/fake-project/instances/default",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "default",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        profile = rbe_profile.RbeProfile.from_dict(data)
        self.assertEqual(
            profile.rbe_instance, "projects/fake-project/instances/default"
        )
        self.assertIsInstance(profile.remote_exec_root, pathlib.PurePosixPath)
        self.assertEqual(
            profile.remote_exec_root, pathlib.PurePosixPath("/fake/root")
        )
        self.assertEqual(profile.container_image, "docker://gcr.io/fake/image")
        self.assertEqual(
            profile.worker_pools,
            {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
        )
        self.assertEqual(profile.profile_name, "default")
        self.assertEqual(profile.os_family, "fake-os")
        self.assertEqual(profile.auth_type, "fake-auth")

    def test_rbe_profile_from_dict_missing_fields_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            # "worker_pools" is missing
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("worker_pools", str(ctx.exception))

    def test_rbe_profile_from_dict_missing_profile_name_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            # "profile_name" is missing
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("profile_name", str(ctx.exception))

    def test_rbe_profile_from_dict_missing_container_image_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            # "container_image" is missing
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("container_image", str(ctx.exception))

    def test_rbe_profile_from_dict_missing_os_family_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "fake-profile",
            # "os_family" is missing
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("os_family", str(ctx.exception))

    def test_rbe_profile_worker_pools_missing_default_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {"large": "Pool=fake-large-pool"},
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("missing required pool entries", str(ctx.exception))
        self.assertIn("'default'", str(ctx.exception))

    def test_rbe_profile_worker_pools_missing_large_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {"default": "Pool=fake-default-pool"},
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("missing required pool entries", str(ctx.exception))
        self.assertIn("'large'", str(ctx.exception))

    def test_rbe_profile_worker_pools_invalid_platform_variable_raises(
        self,
    ) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "no_equals_sign",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn(
            "must be a 'key=value' platform property string", str(ctx.exception)
        )

    def test_rbe_profile_env_round_trip(self) -> None:
        profile = rbe_profile.RbeProfile(
            rbe_instance="projects/test-project/instances/default",
            remote_exec_root=pathlib.PurePosixPath("/test/exec/root"),
            container_image="docker://gcr.io/fake/image",
            worker_pools={
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            profile_name="default",
            os_family="fake-os",
            auth_type="fake-auth",
        )
        env_dict = profile.to_env_dict()
        self.assertDictEqual(
            env_dict,
            {
                rbe_profile._ENV_RBE_INSTANCE: "projects/test-project/instances/default",
                rbe_profile._ENV_EXEC_ROOT: "/test/exec/root",
                rbe_profile._ENV_CONTAINER_IMAGE: "docker://gcr.io/fake/image",
                rbe_profile._ENV_OS_FAMILY: "fake-os",
                rbe_profile._ENV_WORKER_POOLS: '{"default": "Pool=fake-default-pool", "large": "Pool=fake-large-pool"}',
                rbe_profile._ENV_AUTH_TYPE: "fake-auth",
                rbe_profile._ENV_INTERNAL_PROFILE_NAME: "default",
                rbe_profile._ENV_RBE_PLATFORM: "container-image=docker://gcr.io/fake/image,Pool=fake-default-pool",
            },
        )

        # Test direct from_env_dict deserialization
        reconstructed = rbe_profile.RbeProfile.from_env_dict(env_dict)
        self.assertEqual(reconstructed, profile)

        # Fast path: load reconstructs this exact profile directly from env without disk I/O
        loaded = rbe_profile.load(pathlib.Path("/nonexistent"), env_dict)
        self.assertEqual(loaded, profile)

    def test_from_env_dict_missing_rbe_instance_raises(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_env_dict({})
        self.assertIn("RBE_instance", str(ctx.exception))

    @mock.patch.object(rbe_profile._MANAGER, "load")
    def test_load_with_malformed_worker_pools_json(
        self, mock_load: mock.Mock
    ) -> None:
        env_dict = {
            rbe_profile._ENV_RBE_INSTANCE: "projects/env-project/instances/env-rbe",
            rbe_profile._ENV_EXEC_ROOT: "/env/exec/root",
            rbe_profile._ENV_CONTAINER_IMAGE: "docker://gcr.io/fake/image",
            rbe_profile._ENV_OS_FAMILY: "fake-os",
            rbe_profile._ENV_WORKER_POOLS: "{malformed_json",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.load(pathlib.Path("/fake"), env_dict)
        self.assertIn(
            "Failed to decode worker pools JSON from environment",
            str(ctx.exception),
        )
        mock_load.assert_not_called()

    def test_load_with_relative_exec_root_raises(self) -> None:
        env_dict = {
            rbe_profile._ENV_RBE_INSTANCE: "projects/env-project/instances/env-rbe",
            rbe_profile._ENV_EXEC_ROOT: "relative/path",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.load(pathlib.Path("/fake"), env_dict)
        self.assertIn("must be an absolute path", str(ctx.exception))

    @mock.patch.object(rbe_profile._MANAGER, "load")
    def test_load_delegates_to_manager(self, mock_load: mock.Mock) -> None:
        expected = rbe_profile.fake()
        mock_load.return_value = expected
        loaded = rbe_profile.load(pathlib.Path("/fake"), {})
        self.assertEqual(loaded, expected)
        mock_load.assert_called_once_with(pathlib.Path("/fake"), {}, name=None)

    def test_fake(self) -> None:
        # Default fake
        p = rbe_profile.fake()
        self.assertEqual(
            p.rbe_instance, "projects/fake-project/instances/fake-instance"
        )
        self.assertIsInstance(p.remote_exec_root, pathlib.PurePosixPath)
        self.assertEqual(
            p.remote_exec_root, pathlib.PurePosixPath("/fake/root")
        )
        self.assertEqual(p.container_image, "docker://gcr.io/fake/image")
        self.assertEqual(
            p.worker_pools,
            {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
        )
        self.assertEqual(p.profile_name, "fake-profile")
        self.assertEqual(p.os_family, "fake-os")
        self.assertEqual(p.auth_type, "fake-auth")

        # Overrides
        p_custom = rbe_profile.fake(
            rbe_instance="projects/custom-project/instances/custom-inst",
            auth_type="google",
        )
        self.assertEqual(
            p_custom.rbe_instance,
            "projects/custom-project/instances/custom-inst",
        )
        self.assertEqual(p_custom.auth_type, "google")

        # Unknown override raises ValueError
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.fake(nonexistent_key=True)
        self.assertIn("nonexistent_key", str(ctx.exception))

    def test_validate_remote_exec_root(self) -> None:
        for raw_root, expected_norm in (
            ("/", pathlib.PurePosixPath("/")),
            ("/b/f/w", pathlib.PurePosixPath("/b/f/w")),
            ("/b/./f/w/", pathlib.PurePosixPath("/b/f/w")),
            ("///b///f///w///", pathlib.PurePosixPath("/b/f/w")),
            ("/tmp/buildbox-*", pathlib.PurePosixPath("/tmp/buildbox-*")),
            ("/*/*/*", pathlib.PurePosixPath("/*/*/*")),
        ):
            with self.subTest(valid_root=raw_root):
                validated = rbe_profile._validate_remote_exec_root(
                    pathlib.PurePosixPath(raw_root)
                )
                self.assertIsInstance(validated, pathlib.PurePosixPath)
                self.assertEqual(validated, expected_norm)
                prof = rbe_profile.fake(remote_exec_root=raw_root)
                self.assertEqual(prof.remote_exec_root, expected_norm)

        for invalid_root, expected_msg in (
            ("relative/root", "must be an absolute path"),
            ("//double/slash", "must be an absolute path"),
            ("/b/../w", "must not contain '..'"),
            ("/tmp/**", "does not support recursive '**'"),
            ("/tmp/**/work", "does not support recursive '**'"),
            ("/tmp/buildbox-**", "does not support recursive '**'"),
        ):
            with self.subTest(invalid_root=invalid_root):
                with self.assertRaises(ValueError) as ctx:
                    rbe_profile._validate_remote_exec_root(
                        pathlib.PurePosixPath(invalid_root)
                    )
                self.assertIn(expected_msg, str(ctx.exception))
                with self.assertRaises(ValueError):
                    rbe_profile.fake(remote_exec_root=invalid_root)

    def test_has_glob_magic_and_remote_exec_root_has_wildcards(self) -> None:
        self.assertFalse(rbe_profile._has_glob_magic("plain-dir_1.0"))
        self.assertTrue(rbe_profile._has_glob_magic("buildbox-*"))
        self.assertTrue(rbe_profile._has_glob_magic("worker-?"))
        self.assertTrue(rbe_profile._has_glob_magic("worker-[0-9]"))

        self.assertFalse(
            rbe_profile.fake(
                remote_exec_root="/"
            ).remote_exec_root_has_wildcards
        )
        self.assertFalse(
            rbe_profile.fake(
                remote_exec_root="/b/f/w"
            ).remote_exec_root_has_wildcards
        )
        self.assertTrue(
            rbe_profile.fake(
                remote_exec_root="/tmp/buildbox-*"
            ).remote_exec_root_has_wildcards
        )
        self.assertTrue(
            rbe_profile.fake(
                remote_exec_root="/worker-[0-9]/sandbox"
            ).remote_exec_root_has_wildcards
        )

    def test_matches_remote_exec_root(self) -> None:
        root_slash = rbe_profile.fake(remote_exec_root="/")
        self.assertTrue(root_slash.matches_remote_exec_root("/"))
        self.assertTrue(
            root_slash.matches_remote_exec_root(pathlib.PurePosixPath("/"))
        )
        self.assertFalse(root_slash.matches_remote_exec_root("/b"))
        self.assertFalse(root_slash.matches_remote_exec_root("rel"))
        self.assertFalse(root_slash.matches_remote_exec_root("//double"))

        fixed = rbe_profile.fake(remote_exec_root="/b/f/w")
        self.assertTrue(fixed.matches_remote_exec_root(pathlib.Path("/b/f/w")))
        self.assertTrue(fixed.matches_remote_exec_root("/b/f/../f/w"))
        self.assertFalse(fixed.matches_remote_exec_root("/b/f"))
        self.assertFalse(fixed.matches_remote_exec_root("/b/f/w/sub"))
        self.assertFalse(fixed.matches_remote_exec_root("/b/f/../../etc"))

        wildcard = rbe_profile.fake(remote_exec_root="/tmp/buildbox-*")
        self.assertTrue(
            wildcard.matches_remote_exec_root("/tmp/buildbox-12345")
        )
        self.assertTrue(
            wildcard.matches_remote_exec_root("/tmp/other/../buildbox-12345")
        )
        self.assertFalse(
            wildcard.matches_remote_exec_root("/tmp/buildbox-12345/sub")
        )
        self.assertFalse(wildcard.matches_remote_exec_root("/tmp/other"))
        self.assertFalse(
            wildcard.matches_remote_exec_root("/tmp/BUILDBOX-12345")
        )

    def test_strip_remote_exec_root(self) -> None:
        root_slash = rbe_profile.fake(remote_exec_root="/")
        self.assertIsNone(root_slash.strip_remote_exec_root("/"))
        self.assertEqual(
            root_slash.strip_remote_exec_root(
                pathlib.Path("/tools/fidl/fidlgen_rust_next/askama.toml")
            ),
            pathlib.PurePosixPath("tools/fidl/fidlgen_rust_next/askama.toml"),
        )

        fixed = rbe_profile.fake(remote_exec_root="/b/f/w")
        self.assertIsNone(fixed.strip_remote_exec_root("/b/f/w"))
        self.assertIsNone(fixed.strip_remote_exec_root("relative/foo.h"))
        self.assertIsNone(fixed.strip_remote_exec_root("//double/slash/foo.h"))
        self.assertEqual(
            fixed.strip_remote_exec_root("/b/f/w/src/foo.h"),
            pathlib.PurePosixPath("src/foo.h"),
        )

        wildcard = rbe_profile.fake(remote_exec_root="/tmp/buildbox-*")
        self.assertIsNone(wildcard.strip_remote_exec_root("/tmp/buildbox-abc"))
        self.assertEqual(
            wildcard.strip_remote_exec_root(
                "/tmp/buildbox-abc/out/default/gen/foo.h"
            ),
            pathlib.PurePosixPath("out/default/gen/foo.h"),
        )
        # In-sandbox '..' segments collapse within the sandbox.
        self.assertEqual(
            wildcard.strip_remote_exec_root(
                "/tmp/buildbox-abc/set_by_reclient/a/../../tools/foo.rs"
            ),
            pathlib.PurePosixPath("tools/foo.rs"),
        )
        # '..' segments that escape out of the sandbox are rejected.
        self.assertIsNone(
            wildcard.strip_remote_exec_root(
                "/tmp/buildbox-abc/../../usr/include/stdio.h"
            )
        )

    def test_format_remote_exec_root_mismatch(self) -> None:
        profile = rbe_profile.fake(
            remote_exec_root="/tmp/buildbox-*",
            profile_name="buildgrid",
        )
        msg = profile.format_remote_exec_root_mismatch("/b/f/w")
        self.assertIn("/b/f/w", msg)
        self.assertIn("/tmp/buildbox-*", msg)
        self.assertIn("buildgrid", msg)
        self.assertIn("recursive '**' wildcards are not supported", msg)


if __name__ == "__main__":
    unittest.main()
