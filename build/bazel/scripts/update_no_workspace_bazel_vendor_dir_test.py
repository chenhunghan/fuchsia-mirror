#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

from build.bazel.scripts import update_no_workspace_bazel_vendor_dir


def _write_fake_bazel(script_path: pathlib.Path):
    script_path.write_text(
        """#!/bin/sh
set -eu
if [ "${1:-}" = "mod" ] && [ "${2:-}" = "dump_repo_mapping" ]; then
  printf '{"foo":"+third_party+foo","bazel_tools":"bazel_tools"}\\n'
  exit 0
fi
vendor_dir=""
for arg in "$@"; do
  case "$arg" in
    --vendor_dir=*) vendor_dir="${arg#--vendor_dir=}" ;;
  esac
done
mkdir -p "$vendor_dir/+third_party+foo" "$vendor_dir/_registries/bcr.bazel.build"
printf '' > "$vendor_dir/+third_party+foo/REPO.bazel"
printf 'int x = 1;\\n' > "$vendor_dir/+third_party+foo/foo.c"
printf '# generated\\n' > "$vendor_dir/VENDOR.bazel"
printf 'hash\\n' > "$vendor_dir/@+third_party+foo.marker"
ln -s /nonexistent/external "$vendor_dir/bazel-external"
"""
    )
    script_path.chmod(0o755)


class UpdateNoWorkspaceBazelVendorDirTest(unittest.TestCase):
    @mock.patch.object(
        update_no_workspace_bazel_vendor_dir,
        "_clean_gitignored_files",
        autospec=True,
    )
    def test_auto_discovers_repos_strips_metadata_and_preserves_files(
        self, mock_clean_gitignored_files
    ):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            fake_bazel = root / "fake_bazel.sh"
            _write_fake_bazel(fake_bazel)

            workspace = root / "manifests/third_party"
            workspace.mkdir(parents=True)
            vendor_dir = root / "third_party/bazel_vendor/no_workspace"
            vendor_dir.mkdir(parents=True)
            (vendor_dir / "OWNERS").write_text("owner@example.com\n")
            (vendor_dir / "README.md").write_text("# readme\n")
            (vendor_dir / "stale_repo").mkdir()
            (vendor_dir / "stale_repo/old.c").write_text("stale\n")

            return_code = update_no_workspace_bazel_vendor_dir.main(
                (
                    f"--bazel={fake_bazel}",
                    f"--repo-root={root}",
                )
            )

            self.assertEqual(return_code, 0)
            self.assertTrue((vendor_dir / "+third_party+foo/foo.c").is_file())
            self.assertTrue(
                (vendor_dir / "+third_party+foo/REPO.bazel").is_file()
            )
            self.assertEqual(
                (vendor_dir / "OWNERS").read_text(), "owner@example.com\n"
            )
            self.assertEqual(
                (vendor_dir / "README.md").read_text(), "# readme\n"
            )
            self.assertFalse((vendor_dir / "VENDOR.bazel").exists())
            self.assertFalse((vendor_dir / "bazel-external").is_symlink())
            self.assertFalse((vendor_dir / "_registries").exists())
            self.assertFalse((vendor_dir / "@+third_party+foo.marker").exists())
            self.assertFalse((vendor_dir / "stale_repo").exists())
            mock_clean_gitignored_files.assert_called_once_with(vendor_dir)

    @mock.patch.object(
        update_no_workspace_bazel_vendor_dir,
        "subprocess",
        autospec=True,
        spec_set=True,
    )
    def test_clean_gitignored_files_skips_when_ignored(self, mock_subprocess):
        with tempfile.TemporaryDirectory() as tmp:
            vendor_dir = pathlib.Path(tmp)
            mock_subprocess.DEVNULL = subprocess.DEVNULL
            mock_subprocess.run.return_value = subprocess.CompletedProcess(
                args=("git", "check-ignore"), returncode=0
            )

            update_no_workspace_bazel_vendor_dir._clean_gitignored_files(
                vendor_dir
            )

            self.assertEqual(mock_subprocess.run.call_count, 1)

    @mock.patch.object(
        update_no_workspace_bazel_vendor_dir,
        "subprocess",
        autospec=True,
        spec_set=True,
    )
    def test_clean_gitignored_files_cleans_when_not_ignored(
        self, mock_subprocess
    ):
        with tempfile.TemporaryDirectory() as tmp:
            vendor_dir = pathlib.Path(tmp)
            mock_subprocess.DEVNULL = subprocess.DEVNULL
            mock_subprocess.run.side_effect = (
                subprocess.CompletedProcess(
                    args=("git", "check-ignore"), returncode=1
                ),
                subprocess.CompletedProcess(
                    args=("git", "clean"), returncode=0
                ),
            )

            update_no_workspace_bazel_vendor_dir._clean_gitignored_files(
                vendor_dir
            )

            self.assertEqual(mock_subprocess.run.call_count, 2)


if __name__ == "__main__":
    unittest.main()
