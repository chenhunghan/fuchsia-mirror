#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for resolve_skills.py.

Run: python3 -B .agents/skills/review/scripts/resolve_skills_test.py
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import resolve_skills  # pylint: disable=wrong-import-position

ROOT = "/repo"
CONFIG = r"""
# comment
configs: {
  id: "default"
  use_default_subagents: true
  include_filters: {
    project: "fuchsia"
    path_regex: ".*"
  }
}
configs: {
  id: "rust"
  skills: "rust_best_practices"
  include_filters: {
    project: "fuchsia"
    project: "vendor/google"
    path_regex: ".*\\.rs"
  }
}
configs: {
  id: "drivers"
  skills: "src/devices/skills/driver_file_structure"
  skills: "src/devices/skills/fuchsia-suspendable"
  include_filters: {
    project: "fuchsia"
    path_regex: "(.*/)?drivers/.*\\.(rs|cml)"
  }
}
configs: {
  id: "starnix"
  skills: "starnix-guidelines"
  include_filters: {
    path_regex: "(.*/)?starnix/.*"
  }
}
configs: {
  id: "lacewing"
  skills: "src/testing/end_to_end/.agents/skills/lacewing"
  include_filters: {
    project: "fuchsia"
    path_regex: "tests/end_to_end/.*\\.py"
  }
}
configs: {
  id: "docs"
  skills: "docs/skills/edit-fuchsia-doc/SKILL.md"
  include_filters: {
    project: "fuchsia"
    project: "vendor/google"
    path_regex: "docs/.*\\.md"
  }
  exclude_filters: {
    path_regex: "docs/gen/.*"
  }
  exclude_filters: {
    project: "vendor/google"
  }
}
"""

VENDOR_CONFIG = r"""
configs: {
  id: "turquoise-code-review-agent"
  use_default_subagents: true
  include_filters: {
    project: "vendor/google"
    path_regex: ".*"
  }
}
configs: {
  id: "modem"
  skills: "iris/modem/.agents/skills/mtk-t900"
  include_filters: {
    project: "vendor/google"
    path_regex: "iris/modem/.*"
  }
}
configs: {
  id: "bare"
  skills: "vendor-skill"
  include_filters: {
    path_regex: ".*\\.json5"
  }
}
"""


def _resolve(files, project="fuchsia"):
    return {
        r["skill_path"]: r["files"]
        for r in resolve_skills.resolve(CONFIG, files, ROOT, project=project)
    }


class ResolveSkillsTest(unittest.TestCase):
    def test_bare_skill_resolves_under_agents_skills(self):
        self.assertEqual(
            _resolve(["src/lib/foo.rs"]),
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "src/lib/foo.rs"
                ]
            },
        )

    def test_path_skill_resolves_from_repo_root_and_groups_files(self):
        out = _resolve(
            ["src/devices/x/drivers/a.rs", "src/devices/x/drivers/meta/a.cml"]
        )
        self.assertEqual(
            out["/repo/src/devices/skills/fuchsia-suspendable/SKILL.md"],
            ["src/devices/x/drivers/a.rs", "src/devices/x/drivers/meta/a.cml"],
        )
        self.assertEqual(
            out["/repo/.agents/skills/rust_best_practices/SKILL.md"],
            ["src/devices/x/drivers/a.rs"],
        )

    def test_full_match_not_search(self):
        # Like CRACS, path_regex must match the whole path.
        self.assertEqual(_resolve(["src/tests/end_to_end/foo.py"]), {})

    def test_entry_without_project_applies_to_any_project(self):
        out = _resolve(["starnix/kernel/a.rs"], project="vendor/google")
        self.assertIn("/repo/.agents/skills/starnix-guidelines/SKILL.md", out)

    def test_project_filter(self):
        self.assertEqual(
            _resolve(["a/drivers/b.rs"], project="vendor/google"),
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "a/drivers/b.rs"
                ]
            },
        )

    def test_entries_without_skills_add_nothing(self):
        self.assertEqual(_resolve(["docs/README.yaml"]), {})

    def test_manual_automatic_false_entry_is_skipped(self):
        cfg = (
            CONFIG
            + r"""
configs: {
  id: "manual-simplify"
  automatic: false
  skills: "code-simplification"
  include_filters: {
    project: "fuchsia"
    path_regex: ".*\\.rs"
  }
}
"""
        )
        resolved = {
            r["skill_path"]: r["files"]
            for r in resolve_skills.resolve(cfg, ["src/lib/foo.rs"], ROOT)
        }
        self.assertEqual(
            resolved,
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "src/lib/foo.rs"
                ]
            },
        )

    def test_exclude_filters(self):
        # Like CRACS, a file matching any exclude_filters block is dropped.
        self.assertEqual(
            _resolve(["docs/README.md", "docs/gen/api.md"]),
            {"/repo/docs/skills/edit-fuchsia-doc/SKILL.md": ["docs/README.md"]},
        )

    def test_exclude_filter_without_path_regex_excludes_project(self):
        self.assertEqual(
            _resolve(["docs/README.md"], project="vendor/google"), {}
        )

    def test_skill_ending_in_skill_md_is_not_doubled(self):
        self.assertEqual(
            resolve_skills.skill_file("docs/skills/x/SKILL.md", ROOT),
            "/repo/docs/skills/x/SKILL.md",
        )

    def test_extra_configs_resolve_from_their_own_root(self):
        out = {
            r["skill_path"]: r["files"]
            for r in resolve_skills.resolve(
                CONFIG,
                ["iris/modem/a.rs", "x/TESTING.json5"],
                ROOT,
                project="vendor/google",
                extra_configs=[(VENDOR_CONFIG, "/repo/vendor/google")],
            )
        }
        self.assertEqual(
            out,
            {
                "/repo/.agents/skills/rust_best_practices/SKILL.md": [
                    "iris/modem/a.rs"
                ],
                "/repo/vendor/google/iris/modem/.agents/skills/mtk-t900/"
                "SKILL.md": ["iris/modem/a.rs"],
                "/repo/vendor/google/.agents/skills/vendor-skill/SKILL.md": [
                    "x/TESTING.json5"
                ],
            },
        )

    def _write_tree(self, root, vendor=True):
        files = {resolve_skills.CONFIG: CONFIG}
        if vendor:
            files[
                os.path.join("vendor", "google", resolve_skills.CONFIG)
            ] = VENDOR_CONFIG
        for rel, text in files.items():
            path = os.path.join(root, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)

    def _main(self, root, files, project):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(
            io.StringIO()
        ):
            resolve_skills.main(
                [
                    f"--repo_root={root}",
                    f"--files={files}",
                    f"--project={project}",
                ]
            )
        return {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in json.loads(out.getvalue())
        }

    def test_main_loads_vendor_config_for_vendor_project(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            self.assertEqual(
                self._main(root, "iris/modem/a.rs", "vendor/google"),
                {
                    ".agents/skills/rust_best_practices/SKILL.md": [
                        "iris/modem/a.rs"
                    ],
                    "vendor/google/iris/modem/.agents/skills/mtk-t900/"
                    "SKILL.md": ["iris/modem/a.rs"],
                },
            )

    def test_main_ignores_vendor_config_for_fuchsia_project(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            self.assertEqual(self._main(root, "x/TESTING.json5", "fuchsia"), {})

    def test_main_without_vendor_config(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root, vendor=False)
            self.assertEqual(
                self._main(root, "iris/modem/a.rs", "vendor/google"),
                {
                    ".agents/skills/rust_best_practices/SKILL.md": [
                        "iris/modem/a.rs"
                    ]
                },
            )

    def test_repo_root_skips_vendor_google(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            start = os.path.join(root, "vendor", "google", "iris")
            os.makedirs(start)
            self.assertEqual(
                os.path.realpath(resolve_skills._repo_root(start)),
                os.path.realpath(root),
            )

    def test_real_config_parses(self):
        root = os.path.abspath(
            os.path.join(os.path.dirname(__file__), "..", "..", "..", "..")
        )
        path = os.path.join(root, resolve_skills.CONFIG)
        if not os.path.isfile(path):
            self.skipTest("agent_configs.textproto not checked out")
        with open(path, encoding="utf-8") as f:
            text = f.read()
        configs = resolve_skills.parse_textproto(text)["configs"]
        self.assertTrue(all("id" in c for c in configs))
        dts_files = [
            "src/devices/board/drivers/vim3/vim3.dts",
            "src/devices/board/drivers/vim3/vim3.dtsi",
            "src/devices/board/drivers/sorrel/sorrel.dts.S",
            "src/devices/board/drivers/sorrel/sorrel-common.dtsi.S",
        ]
        resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text, dts_files, root, project="fuchsia"
            )
        }
        for skill in (
            "src/devices/skills/driver_bind_devicetree/SKILL.md",
            "src/devices/skills/driver_bind_find_instance_name/SKILL.md",
            "src/devices/skills/devicetree_schema/SKILL.md",
        ):
            self.assertEqual(resolved.get(skill), sorted(dts_files))
        rs_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text, ["src/lib/foo.rs"], root, project="fuchsia"
            )
        }
        self.assertNotIn(
            ".agents/skills/code-simplification/SKILL.md", rs_resolved
        )
        bazel_files = [
            "BUILD.bazel",
            "src/connectivity/bluetooth/BUILD.bazel",
            "build/bazel/rules/cc/fx_cc_library.bzl",
            "build/bazel/bazel_sdk/rules.bzl",
            "src/foo/helper.bzl",
            "build/bazel_sdk/fuchsia_rules_common/bind/providers.bzl",
            "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/defs.bzl",
            "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/workspace/"
            "sdk_templates/package.BUILD.template",
            "third_party/rust_crates/BUILD.bazel",
            "third_party/mini_chromium/buildflags.bzl",
            "third_party/bazel_vendor/rules_cc/BUILD.bazel",
            "third_party/bazel_registries/bcr/defs.bzl",
            "third_party/rust_crates/vendor/serde/BUILD.bazel",
            "third_party/rust_crates/empty/foo/BUILD.bazel",
            "third_party/boringssl/BUILD.generated.bzl",
            "third_party/foo/src/defs.bzl",
        ]
        bazel_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text, bazel_files, root, project="fuchsia"
            )
        }
        self.assertEqual(
            bazel_resolved.get(
                "build/bazel/.agents/skills/fuchsia-bazel-guidelines/SKILL.md"
            ),
            [
                "BUILD.bazel",
                "build/bazel/bazel_sdk/rules.bzl",
                "build/bazel/rules/cc/fx_cc_library.bzl",
                "src/connectivity/bluetooth/BUILD.bazel",
                "src/foo/helper.bzl",
                "third_party/mini_chromium/buildflags.bzl",
                "third_party/rust_crates/BUILD.bazel",
            ],
        )
        self.assertEqual(
            bazel_resolved.get(
                "build/bazel/.agents/skills/"
                "fuchsia-bzl-file-guidelines/SKILL.md"
            ),
            [
                "build/bazel/bazel_sdk/rules.bzl",
                "build/bazel/rules/cc/fx_cc_library.bzl",
                "src/foo/helper.bzl",
                "third_party/mini_chromium/buildflags.bzl",
            ],
        )
        self.assertEqual(
            bazel_resolved.get(
                "build/bazel/rules/.agents/skills/"
                "fuchsia-bazel-rules-and-macros-guidelines/SKILL.md"
            ),
            [
                "build/bazel/bazel_sdk/rules.bzl",
                "build/bazel/rules/cc/fx_cc_library.bzl",
                "build/bazel_sdk/fuchsia_rules_common/bind/providers.bzl",
                "src/foo/helper.bzl",
                "third_party/mini_chromium/buildflags.bzl",
            ],
        )
        self.assertEqual(
            bazel_resolved.get(
                "build/bazel_sdk/.agents/skills/"
                "fuchsia-bazel-sdk-guidelines/SKILL.md"
            ),
            [
                "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/defs.bzl",
                "build/bazel_sdk/bazel_rules_fuchsia/fuchsia/workspace/"
                "sdk_templates/package.BUILD.template",
                "build/bazel_sdk/fuchsia_rules_common/bind/providers.bzl",
            ],
        )
        for rel_skill in bazel_resolved:
            self.assertFalse(
                os.path.islink(os.path.join(root, rel_skill)),
                msg=f"CRACS skill path must not be a symlink: {rel_skill}",
            )
            self.assertTrue(
                os.path.isfile(os.path.join(root, rel_skill)),
                msg=f"missing resolved skill: {rel_skill}",
            )
        vendor_bazel_resolved = resolve_skills.resolve(
            text, bazel_files, root, project="vendor/google"
        )
        self.assertEqual(vendor_bazel_resolved, [])

        wlan_fuchsia_files = [
            "sdk/fidl/fuchsia.wlan.device.service/service.fidl",
            "src/connectivity/wlan/FIDL.md",
            "src/connectivity/wlan/BUILD.bazel",
            "src/connectivity/wlan/wlancfg/src/main.rs",
            "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/mvm/mac.c",
            "src/connectivity/wlan/tests/core/BUILD.gn",
            "src/connectivity/wlan/tests/core/ConnectToApTest.py",
            "src/testing/end_to_end/honeydew/honeydew/affordances/connectivity/"
            "wlan/wlan_policy/wlan_policy.py",
            "src/testing/end_to_end/mobly_base_tests/fuchsia_wlan_base_test/"
            "__init__.py",
            "src/testing/end_to_end/mobly_controller/legacy_access_point/"
            "access_point.py",
        ]
        wlan_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text, wlan_fuchsia_files, root, project="fuchsia"
            )
        }
        self.assertEqual(
            wlan_resolved.get(
                "src/connectivity/wlan/.agents/skills/wlan-review-api/SKILL.md"
            ),
            [
                "sdk/fidl/fuchsia.wlan.device.service/service.fidl",
                "src/connectivity/wlan/FIDL.md",
            ],
        )
        self.assertEqual(
            wlan_resolved.get(
                "src/connectivity/wlan/.agents/skills/"
                "wlan-review-stack/SKILL.md"
            ),
            [
                "src/connectivity/wlan/BUILD.bazel",
                "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/"
                "mvm/mac.c",
                "src/connectivity/wlan/wlancfg/src/main.rs",
            ],
        )
        self.assertEqual(
            wlan_resolved.get(
                "src/connectivity/wlan/.agents/skills/wlan-review-e2e/SKILL.md"
            ),
            [
                "src/connectivity/wlan/tests/core/BUILD.gn",
                "src/connectivity/wlan/tests/core/ConnectToApTest.py",
                "src/testing/end_to_end/honeydew/honeydew/affordances/"
                "connectivity/wlan/wlan_policy/wlan_policy.py",
                "src/testing/end_to_end/mobly_base_tests/"
                "fuchsia_wlan_base_test/__init__.py",
                "src/testing/end_to_end/mobly_controller/legacy_access_point/"
                "access_point.py",
            ],
        )
        vendor_wlan_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text,
                ["tests/end_to_end/wlan/base_functional_test.py"],
                root,
                project="vendor/google",
            )
        }
        self.assertEqual(
            vendor_wlan_resolved.get(
                "src/connectivity/wlan/.agents/skills/wlan-review-e2e/SKILL.md"
            ),
            ["tests/end_to_end/wlan/base_functional_test.py"],
        )

        wlan_simplify_skill = (
            "src/connectivity/wlan/.agents/skills/"
            "wlan-review-simplification/SKILL.md"
        )
        wlan_simplify_files = [
            "sdk/fidl/fuchsia.wlan.device.service/service.fidl",
            "src/connectivity/wlan/BUILD.bazel",
            "src/connectivity/wlan/.agents/skills/wlan-e2e-test-promotion/"
            "scripts/promote_e2e_tests.py",
            "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/mvm/mac.c",
            "src/connectivity/wlan/drivers/wlansoftmac/rust_driver/src/lib.rs",
            "src/connectivity/wlan/lib/sme/src/lib.rs",
            "src/connectivity/wlan/tests/core/BUILD.gn",
            "src/connectivity/wlan/tests/core/ConnectToApTest.py",
            "src/connectivity/wlan/wlancfg/meta/wlancfg.cml",
            "src/connectivity/wlan/wlancfg/src/main.rs",
            "src/testing/end_to_end/honeydew/honeydew/affordances/connectivity/"
            "netstack/netstack.py",
            "src/testing/end_to_end/honeydew/honeydew/affordances/connectivity/"
            "wlan/wlan_policy/wlan_policy.py",
            "src/testing/end_to_end/mobly_base_tests/fuchsia_wlan_base_test/"
            "__init__.py",
            "src/testing/end_to_end/mobly_controller/legacy_access_point/"
            "access_point.py",
            "src/testing/end_to_end/mobly_controller/openwrt_access_point/"
            "__init__.py",
        ]
        wlan_simplify_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text, wlan_simplify_files, root, project="fuchsia"
            )
        }
        self.assertEqual(
            wlan_simplify_resolved.get(wlan_simplify_skill),
            [
                "src/connectivity/wlan/drivers/wlansoftmac/rust_driver/src/"
                "lib.rs",
                "src/connectivity/wlan/lib/sme/src/lib.rs",
                "src/connectivity/wlan/tests/core/ConnectToApTest.py",
                "src/connectivity/wlan/wlancfg/src/main.rs",
                "src/testing/end_to_end/honeydew/honeydew/affordances/"
                "connectivity/wlan/wlan_policy/wlan_policy.py",
                "src/testing/end_to_end/mobly_base_tests/"
                "fuchsia_wlan_base_test/__init__.py",
                "src/testing/end_to_end/mobly_controller/legacy_access_point/"
                "access_point.py",
                "src/testing/end_to_end/mobly_controller/openwrt_access_point/"
                "__init__.py",
            ],
        )
        self.assertFalse(
            os.path.islink(os.path.join(root, wlan_simplify_skill)),
            msg=f"CRACS skill path must not be a symlink: {wlan_simplify_skill}",
        )
        self.assertTrue(
            os.path.isfile(os.path.join(root, wlan_simplify_skill)),
            msg=f"missing resolved skill: {wlan_simplify_skill}",
        )
        vendor_wlan_simplify_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text,
                [
                    "drivers/wlan/broadcom/bcmdhd/src/driver.cc",
                    "tests/end_to_end/wlan/BUILD.gn",
                    "tests/end_to_end/wlan/base_functional_test.py",
                ],
                root,
                project="vendor/google",
            )
        }
        self.assertEqual(
            vendor_wlan_simplify_resolved.get(wlan_simplify_skill),
            ["tests/end_to_end/wlan/base_functional_test.py"],
        )

        wlan_commit_msg_skill = (
            "src/connectivity/wlan/.agents/skills/"
            "wlan-review-commit-message/SKILL.md"
        )
        self.assertEqual(
            wlan_simplify_resolved.get(wlan_commit_msg_skill),
            [
                "sdk/fidl/fuchsia.wlan.device.service/service.fidl",
                "src/connectivity/wlan/.agents/skills/wlan-e2e-test-promotion/"
                "scripts/promote_e2e_tests.py",
                "src/connectivity/wlan/BUILD.bazel",
                "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/mvm/"
                "mac.c",
                "src/connectivity/wlan/drivers/wlansoftmac/rust_driver/src/"
                "lib.rs",
                "src/connectivity/wlan/lib/sme/src/lib.rs",
                "src/connectivity/wlan/tests/core/BUILD.gn",
                "src/connectivity/wlan/tests/core/ConnectToApTest.py",
                "src/connectivity/wlan/wlancfg/meta/wlancfg.cml",
                "src/connectivity/wlan/wlancfg/src/main.rs",
                "src/testing/end_to_end/honeydew/honeydew/affordances/"
                "connectivity/wlan/wlan_policy/wlan_policy.py",
                "src/testing/end_to_end/mobly_base_tests/"
                "fuchsia_wlan_base_test/__init__.py",
                "src/testing/end_to_end/mobly_controller/legacy_access_point/"
                "access_point.py",
                "src/testing/end_to_end/mobly_controller/openwrt_access_point/"
                "__init__.py",
            ],
        )
        self.assertFalse(
            os.path.islink(os.path.join(root, wlan_commit_msg_skill)),
            msg=f"CRACS skill path must not be a symlink: {wlan_commit_msg_skill}",
        )
        self.assertTrue(
            os.path.isfile(os.path.join(root, wlan_commit_msg_skill)),
            msg=f"missing resolved skill: {wlan_commit_msg_skill}",
        )
        vendor_wlan_commit_msg_resolved = {
            os.path.relpath(r["skill_path"], root): r["files"]
            for r in resolve_skills.resolve(
                text,
                [
                    "bundles/buildbot/wlan/BUILD.gn",
                    "connectivity/wlan/BUILD.gn",
                    "drivers/wlan/broadcom/bcmdhd/src/driver.cc",
                    "iris/modem/src/lib.rs",
                    "starnix/android/hal/bluetooth/src/main.rs",
                    "starnix/android/hal/wifi_aidl/src/main.rs",
                    "starnix/android/hal/wifi_supplicant_aidl/src/main.rs",
                    "tests/end_to_end/wlan/BUILD.gn",
                    "tests/end_to_end/wlan/base_functional_test.py",
                ],
                root,
                project="vendor/google",
            )
        }
        self.assertEqual(
            vendor_wlan_commit_msg_resolved.get(wlan_commit_msg_skill),
            [
                "bundles/buildbot/wlan/BUILD.gn",
                "connectivity/wlan/BUILD.gn",
                "drivers/wlan/broadcom/bcmdhd/src/driver.cc",
                "starnix/android/hal/wifi_aidl/src/main.rs",
                "starnix/android/hal/wifi_supplicant_aidl/src/main.rs",
                "tests/end_to_end/wlan/BUILD.gn",
                "tests/end_to_end/wlan/base_functional_test.py",
            ],
        )

    def test_filter_skipped_files_defaults_and_custom(self):
        retained, skipped = resolve_skills.filter_skipped_files(
            [
                "Cargo.lock",
                "third_party/rust_crates/Cargo.lock",
                "src/lib/depfile/src/depfile.rs",
                "src/lib/fsl/socket/socket_stats.pb.h",
                "tools/fidl/foo.golden",
                "docs/images/diagram.png",
                "out/default/args.gn",
                "src/developer/ffx/test_data/out/config.json",
                "docs/README.md",
            ],
            resolve_skills.DEFAULT_SKIP_GLOBS,
        )
        self.assertEqual(
            retained,
            [
                "src/lib/depfile/src/depfile.rs",
                "src/developer/ffx/test_data/out/config.json",
                "docs/README.md",
            ],
        )
        self.assertEqual(
            skipped,
            [
                "Cargo.lock",
                "third_party/rust_crates/Cargo.lock",
                "src/lib/fsl/socket/socket_stats.pb.h",
                "tools/fidl/foo.golden",
                "docs/images/diagram.png",
                "out/default/args.gn",
            ],
        )

    def test_main_envelope_and_skip_flags(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            out = io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(
                io.StringIO()
            ):
                resolve_skills.main(
                    [
                        f"--repo_root={root}",
                        "--files=/src/lib/foo.rs,Cargo.lock,docs/ignore.md,"
                        "docs/slash.md",
                        "--project=fuchsia",
                        "--skip_globs=docs/ignore.md, //docs/slash.md",
                        "--output_format=envelope",
                    ]
                )
            envelope = json.loads(out.getvalue())
            self.assertEqual(envelope["retained_files"], ["src/lib/foo.rs"])
            self.assertEqual(
                envelope["skipped_files"],
                ["Cargo.lock", "docs/ignore.md", "docs/slash.md"],
            )
            self.assertEqual(
                [
                    os.path.relpath(t["skill_path"], root)
                    for t in envelope["tasks"]
                ],
                [".agents/skills/rust_best_practices/SKILL.md"],
            )

    def test_main_no_default_skips(self):
        with tempfile.TemporaryDirectory() as root:
            self._write_tree(root)
            out = io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(
                io.StringIO()
            ):
                resolve_skills.main(
                    [
                        f"--repo_root={root}",
                        "--files=Cargo.lock",
                        "--no_default_skips",
                        "--output_format=envelope",
                    ]
                )
            envelope = json.loads(out.getvalue())
            self.assertEqual(envelope["retained_files"], ["Cargo.lock"])
            self.assertEqual(envelope["skipped_files"], [])


if __name__ == "__main__":
    unittest.main()
