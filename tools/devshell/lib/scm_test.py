#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for tools/devshell/lib/scm.py."""

import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import scm


class ScmTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = self.enterContext(tempfile.TemporaryDirectory())
        self.root = pathlib.Path(self.temp_dir).resolve()

        # Create mock root repo with shac.textproto
        (self.root / ".git").mkdir(parents=True)
        (self.root / "shac.textproto").touch()

        # Create mock vendor repo with shac.textproto
        self.vendor_repo = self.root / "vendor" / "google"
        self.vendor_repo.mkdir(parents=True)
        (self.vendor_repo / ".git").mkdir(parents=True)
        (self.vendor_repo / "shac.textproto").touch()

        # Create non-repo vendor dir
        (self.root / "vendor" / "not_a_repo").mkdir(parents=True)

    def test_find_active_repositories(self) -> None:
        # Add another vendor repo to verify alphabetical sorting between vendor repos
        arm_repo = self.root / "vendor" / "arm"
        arm_repo.mkdir(parents=True)
        (arm_repo / ".git").mkdir(parents=True)

        groups = scm.get_repo_groups(self.root, self.root)
        self.assertSequenceEqual(
            [group.repo_root for group in groups],
            [self.root, arm_repo, self.vendor_repo],
        )

    def test_find_active_repositories_no_vendor(self) -> None:
        no_vendor_dir = self.enterContext(tempfile.TemporaryDirectory())
        no_vendor_root = pathlib.Path(no_vendor_dir).resolve()
        (no_vendor_root / ".git").mkdir()
        groups = scm.get_repo_groups(no_vendor_root, no_vendor_root)
        self.assertSequenceEqual(
            [group.repo_root for group in groups], [no_vendor_root]
        )

    def test_check_has_shac(self) -> None:
        # shac.star without shac.textproto does not count
        star_only_repo = self.root / "vendor" / "star_only"
        star_only_repo.mkdir(parents=True)
        (star_only_repo / ".git").mkdir()
        (star_only_repo / "shac.star").touch()

        # shac.textproto as a directory does not count
        shac_as_dir_repo = self.root / "vendor" / "shac_dir"
        shac_as_dir_repo.mkdir(parents=True)
        (shac_as_dir_repo / ".git").mkdir()
        (shac_as_dir_repo / "shac.textproto").mkdir()

        groups_by_root = {
            group.repo_root: group
            for group in scm.get_repo_groups(self.root, self.root)
        }
        self.assertTrue(groups_by_root[self.root].has_shac)
        self.assertTrue(groups_by_root[self.vendor_repo].has_shac)
        self.assertFalse(groups_by_root[star_only_repo].has_shac)
        self.assertFalse(groups_by_root[shac_as_dir_repo].has_shac)

    def test_get_repo_root_for_path(self) -> None:
        git_dir = self.enterContext(tempfile.TemporaryDirectory())
        git_root = pathlib.Path(git_dir).resolve()
        (git_root / ".git").mkdir()

        target_file = git_root / "nested" / "file.txt"
        target_file.parent.mkdir(parents=True)
        target_file.touch()

        groups = scm.get_repo_groups(
            self.root, self.root, explicit_files=[str(target_file)]
        )
        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0].repo_root, git_root)
        self.assertSequenceEqual(groups[0].files, ("nested/file.txt",))

    def test_get_repo_root_for_path_git_rev_parse_fallback(self) -> None:
        worktree_dir = self.enterContext(tempfile.TemporaryDirectory())
        worktree_root = pathlib.Path(worktree_dir).resolve()
        target_file = worktree_root / "file.txt"
        target_file.touch()
        with mock.patch.object(
            subprocess,
            "run",
            autospec=True,
            return_value=subprocess.CompletedProcess(
                args=[], returncode=0, stdout=f"{worktree_root}\n"
            ),
        ) as mock_run:
            groups = scm.get_repo_groups(
                self.root, self.root, explicit_files=[str(target_file)]
            )
        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0].repo_root, worktree_root)
        mock_run.assert_called_once()

    def test_canonicalize_path(self) -> None:
        cwd = self.root / "src"
        cwd.mkdir()

        # GN-style //
        gn_path = scm.canonicalize_path("//tools/test.py", cwd, self.root)
        self.assertEqual(gn_path, self.root / "tools" / "test.py")

        # Root alias //
        root_alias_path = scm.canonicalize_path("//", cwd, self.root)
        self.assertEqual(root_alias_path, self.root)

        # Extra leading slashes ///
        extra_slashes_path = scm.canonicalize_path(
            "///tools/test.py", cwd, self.root
        )
        self.assertEqual(extra_slashes_path, self.root / "tools" / "test.py")

        # Relative to cwd
        relative_path = scm.canonicalize_path("foo.py", cwd, self.root)
        self.assertEqual(relative_path, cwd / "foo.py")

        # Absolute
        expected_abs_path = (self.root / "bar.py").resolve()
        abs_path = scm.canonicalize_path(str(expected_abs_path), cwd, self.root)
        self.assertEqual(abs_path, expected_abs_path)

    def test_canonicalize_path_preserves_leaf_symlink(self) -> None:
        target = self.root / "target.py"
        target.touch()
        symlink = self.vendor_repo / "link.py"
        symlink.symlink_to(target)

        resolved = scm.canonicalize_path(
            "//vendor/google/link.py", self.root, self.root
        )
        self.assertEqual(resolved, self.vendor_repo / "link.py")

        groups = scm.get_repo_groups(
            self.root, self.root, explicit_files=["//vendor/google/link.py"]
        )
        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0].repo_root, self.vendor_repo)
        self.assertSequenceEqual(groups[0].files, ("link.py",))

    def test_get_repo_groups_default_mode(self) -> None:
        groups = scm.get_repo_groups(self.root, self.root)
        self.assertEqual(len(groups), 2)
        # Verify root repo is first
        self.assertEqual(groups[0].repo_root, self.root)
        self.assertEqual(groups[1].repo_root, self.vendor_repo)
        for group in groups:
            self.assertTrue(group.has_shac)
            self.assertSequenceEqual(group.files, ())

    def test_get_repo_groups_default_mode_includes_cwd_subrepo(self) -> None:
        subrepo = self.root / "integration"
        subrepo.mkdir(parents=True)
        (subrepo / ".git").mkdir()
        (subrepo / "shac.textproto").touch()

        groups = scm.get_repo_groups(self.root, cwd=subrepo)
        self.assertEqual(len(groups), 3)
        self.assertEqual(groups[0].repo_root, self.root)
        self.assertEqual(groups[1].repo_root, subrepo)
        self.assertEqual(groups[2].repo_root, self.vendor_repo)

    def test_get_repo_groups_explicit_files(self) -> None:
        # Pass files in reverse order with duplicates to verify sorting and deduplication
        explicit = [
            "//vendor/google/bar.py",
            "//src/z_test.cc",
            "//src/a_test.cc",
            "//src/a_test.cc",
            "//src/uncreated_file.cc",
        ]
        groups = scm.get_repo_groups(
            self.root, self.root, explicit_files=explicit
        )
        self.assertEqual(len(groups), 2)

        # Root repo is always first in returned groups
        self.assertEqual(groups[0].repo_root, self.root)
        self.assertEqual(groups[1].repo_root, self.vendor_repo)

        # Files within each group are sorted and deduplicated, including uncreated files
        self.assertSequenceEqual(
            groups[0].files,
            ("src/a_test.cc", "src/uncreated_file.cc", "src/z_test.cc"),
        )
        self.assertSequenceEqual(groups[1].files, ("bar.py",))

    def test_get_repo_groups_empty_explicit(self) -> None:
        groups = scm.get_repo_groups(self.root, self.root, explicit_files=[])
        self.assertSequenceEqual(groups, ())

    def test_get_repo_groups_nested_subrepo(self) -> None:
        subrepo = self.root / "integration"
        subrepo.mkdir(parents=True)
        (subrepo / ".git").mkdir()
        (subrepo / "shac.textproto").touch()
        subrepo_file = subrepo / "manifest.xml"
        subrepo_file.touch()

        groups = scm.get_repo_groups(
            self.root,
            self.root,
            explicit_files=["//integration/manifest.xml", "//src/main.cc"],
        )
        self.assertEqual(len(groups), 2)
        self.assertEqual(groups[0].repo_root, self.root)
        self.assertSequenceEqual(groups[0].files, ("src/main.cc",))
        self.assertEqual(groups[1].repo_root, subrepo)
        self.assertTrue(groups[1].has_shac)
        self.assertSequenceEqual(groups[1].files, ("manifest.xml",))

    def test_get_repo_groups_fallback_external_repo(self) -> None:
        # External git repository outside of fuchsia_dir
        external_dir = self.enterContext(tempfile.TemporaryDirectory())
        external_root = pathlib.Path(external_dir).resolve()
        (external_root / ".git").mkdir()
        (external_root / "shac.textproto").touch()
        external_file = external_root / "external.py"
        external_file.touch()

        groups = scm.get_repo_groups(
            self.root, self.root, explicit_files=[str(external_file)]
        )
        self.assertEqual(len(groups), 1)
        self.assertEqual(groups[0].repo_root, external_root)
        self.assertTrue(groups[0].has_shac)
        self.assertSequenceEqual(groups[0].files, ("external.py",))

    def test_get_repo_groups_mixed_repos(self) -> None:
        external_dir = self.enterContext(tempfile.TemporaryDirectory())
        external_root = pathlib.Path(external_dir).resolve()
        (external_root / ".git").mkdir()
        (external_root / "shac.textproto").touch()
        external_file = external_root / "external.py"
        external_file.touch()

        explicit = [
            str(external_file),
            "//vendor/google/bar.py",
            "//src/test.cc",
        ]
        groups = scm.get_repo_groups(
            self.root, self.root, explicit_files=explicit
        )
        self.assertEqual(len(groups), 3)
        # Root repo is always first
        self.assertEqual(groups[0].repo_root, self.root)
        self.assertSequenceEqual(groups[0].files, ("src/test.cc",))

        # Remaining repos are sorted lexicographically by repo_root
        non_root_roots = [groups[1].repo_root, groups[2].repo_root]
        expected_non_root = sorted([external_root, self.vendor_repo])
        self.assertSequenceEqual(non_root_roots, expected_non_root)

    def test_get_repo_groups_non_repo_file_raises(self) -> None:
        non_repo_dir = self.enterContext(tempfile.TemporaryDirectory())
        non_repo_file = pathlib.Path(non_repo_dir) / "non_repo.txt"
        non_repo_file.touch()

        with mock.patch.object(
            subprocess, "run", autospec=True, side_effect=FileNotFoundError
        ):
            with self.assertRaises(FileNotFoundError):
                scm.get_repo_groups(
                    self.root, self.root, explicit_files=[str(non_repo_file)]
                )

    def test_get_repo_groups_nonexistent_non_repo_file_raises(self) -> None:
        non_repo_dir = self.enterContext(tempfile.TemporaryDirectory())
        nonexistent_file = (
            pathlib.Path(non_repo_dir) / "nonexistent" / "path" / "file.txt"
        )

        with mock.patch.object(
            subprocess, "run", autospec=True, side_effect=FileNotFoundError
        ):
            with self.assertRaises(FileNotFoundError):
                scm.get_repo_groups(
                    self.root,
                    self.root,
                    explicit_files=[str(nonexistent_file)],
                )


if __name__ == "__main__":
    unittest.main()
