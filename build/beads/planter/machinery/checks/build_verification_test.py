#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import pathlib
import subprocess
import tempfile
import unittest

_CHECKS_DIR = pathlib.Path(__file__).resolve().parent


class BuildVerificationTest(unittest.TestCase):
    def test_too_many_test_footer_lines(self) -> None:
        script = _CHECKS_DIR / "build_verification.sh"
        env = dict(os.environ)

        with tempfile.TemporaryDirectory() as tmp:
            repo = pathlib.Path(tmp)
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            (repo / "scripts").mkdir()
            (repo / "scripts" / "fx").write_text(
                "#!/bin/bash\n", encoding="utf-8"
            )
            os.chmod(repo / "scripts" / "fx", 0o755)
            (repo / "src").mkdir(parents=True)
            (repo / "src" / "BUILD.bazel").write_text(
                'fx_cc_library(name = "foo", srcs = ["foo.cc"], alwayslink = True)\n',
                encoding="utf-8",
            )
            (repo / "src" / "foo.cc").write_text(
                "int main() { return 0; }\n", encoding="utf-8"
            )
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(
                ["git", "commit", "-q", "-m", "base"], cwd=repo, check=True
            )
            base_sha = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=repo, text=True
            ).strip()

            # 4 lines total, but one is shac check, so it should not trigger it
            commit_msg = (
                "[bazel_migration] //src\n\n"
                "Bazel-Migration-Target: //src:foo\n"
                "Test: fx bazel test --config=fuchsia_platform //src:foo\n"
                "Test: fx bazel test --config=host //src:foo\n"
                "Test: fx build\n"
                "Test: fx host-tool shac check\n"
                "Change-Id: I1234567890abcdef1234567890abcdef12345678\n"
            )
            (repo / "src" / "foo.cc").write_text(
                "int main() { return 1; }\n", encoding="utf-8"
            )
            subprocess.run(["git", "add", "."], cwd=repo, check=True)
            subprocess.run(
                ["git", "commit", "-q", "-m", commit_msg], cwd=repo, check=True
            )

            run_env = dict(
                env,
                PLANTER_WORKDIR=str(repo),
                PLANTER_TARGET_DIR="src",
                PLANTER_CHANGE_BASE=base_sha,
            )
            out = json.loads(
                subprocess.check_output(
                    ["bash", str(script)], env=run_env, text=True
                )
            )
            cats = {f["category"] for f in out}
            self.assertNotIn("too_many_test_footer_lines", cats)

            # 4 lines total, none are shac check/fx lint
            commit_msg_bad = (
                "[bazel_migration] //src\n\n"
                "Bazel-Migration-Target: //src:foo\n"
                "Test: fx bazel test --config=fuchsia_platform //src:foo\n"
                "Test: fx bazel test --config=host //src:foo\n"
                "Test: fx build\n"
                "Test: fx build 2\n"
                "Change-Id: I1234567890abcdef1234567890abcdef12345679\n"
            )
            subprocess.run(
                ["git", "commit", "--amend", "-q", "-m", commit_msg_bad],
                cwd=repo,
                check=True,
            )

            out_bad = json.loads(
                subprocess.check_output(
                    ["bash", str(script)], env=run_env, text=True
                )
            )
            cats_bad = {f["category"] for f in out_bad}
            self.assertIn("too_many_test_footer_lines", cats_bad)


if __name__ == "__main__":
    unittest.main()
