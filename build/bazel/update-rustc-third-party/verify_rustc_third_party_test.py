#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for verify_rustc_third_party.py."""

import hashlib
import json
import os
import pathlib
import sys
import tempfile
import textwrap
import tomllib
import unittest
from typing import Any

sys.path.insert(0, os.path.dirname(__file__))

import generate_crate_annotations
import verify_rustc_third_party


class VerifyRustcThirdPartyTest(unittest.TestCase):
    def _write(self, root: pathlib.Path, rel_path: str, content: str) -> None:
        path = root / rel_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(content))

    def test_verify_crate_annotations_clean_and_out_of_sync(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            self._write(
                root,
                "third_party/rust_crates/Cargo.toml",
                """\
                [package]
                name = "fuchsia-third-party"
                version = "0.1.0"

                [gn.package.foo."1.0.0"]
                rustflags = ["--cfg=foo_flag"]
                """,
            )
            self._write(
                root,
                "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl",
                """\
                load("@rules_rust//crate_universe:defs.bzl", "crate")

                CRATE_ANNOTATION_OVERWRITES = {}
                """,
            )
            cargo_data = generate_crate_annotations.parse_cargo_toml(
                str(root / "third_party/rust_crates/Cargo.toml")
            )
            overwrites = generate_crate_annotations.parse_annotation_overwrites(
                str(
                    root
                    / "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl"
                )
            )
            merged = generate_crate_annotations.merge_annotations(
                generate_crate_annotations.build_crate_annotations(cargo_data),
                overwrites,
            )
            expected = generate_crate_annotations.render_starlark(merged)
            self._write(
                root,
                "build/bazel/update-rustc-third-party/crate_annotations.bzl",
                expected,
            )

            self.assertEqual(
                verify_rustc_third_party.verify_crate_annotations(root), []
            )

            # Mutate crate_annotations.bzl manually.
            self._write(
                root,
                "build/bazel/update-rustc-third-party/crate_annotations.bzl",
                expected + "\n# manual edit\n",
            )
            findings = verify_rustc_third_party.verify_crate_annotations(root)
            self.assertEqual(len(findings), 1)
            self.assertEqual(
                findings[0].filepath,
                "build/bazel/update-rustc-third-party/crate_annotations.bzl",
            )
            self.assertIn(
                "crate_annotations.bzl is out of sync", findings[0].message
            )
            self.assertIn("# manual edit", findings[0].message)

            # Fail loudly when buildifier_bin does not exist.
            with self.assertRaises(FileNotFoundError):
                verify_rustc_third_party.verify_crate_annotations(
                    root, root / "nonexistent/buildifier"
                )

            # Add a stale version in crate_annotation_overwrites.bzl.
            self._write(
                root,
                "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl",
                """\
                load("@rules_rust//crate_universe:defs.bzl", "crate")

                CRATE_ANNOTATION_OVERWRITES = {
                    "foo": [
                        crate.annotation(
                            version = "9.9.9",
                            gen_build_script = False,
                        ),
                    ],
                }
                """,
            )
            findings = verify_rustc_third_party.verify_crate_annotations(root)
            self.assertEqual(len(findings), 1)
            self.assertEqual(
                findings[0].filepath,
                "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl",
            )
            self.assertIn(
                "crate_annotation_overwrites.bzl is out of sync",
                findings[0].message,
            )
            self.assertIn("9.9.9", findings[0].message)

    def test_verify_postprocess_crates_vendor(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            cargo_toml_str = """\
                [gn.package.restricted_crate."1.0.0"]
                group_visibility = { import = "//third_party/rust_crates/visibility.gni", variable = "restricted_vis" }
            """
            self._write(
                root, "third_party/rust_crates/Cargo.toml", cargo_toml_str
            )
            cargo_toml = tomllib.loads(textwrap.dedent(cargo_toml_str))
            self._write(
                root,
                "third_party/rust_crates/BUILD.bazel",
                """\
                package_group(
                    name = "restricted_vis",
                    packages = ["//src/..."],
                )
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/public_crate-1.0.0/BUILD.bazel",
                """\
                package(default_visibility = ["//visibility:public"])
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/restricted_crate-1.0.0/BUILD.bazel",
                """\
                package(default_visibility = ["//third_party/rust_crates:__subpackages__"])
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/BUILD.bazel",
                """\
                alias(
                    name = "public_crate",
                    actual = "//third_party/rust_crates/vendor/public_crate-1.0.0:public_crate",
                    tags = ["manual"],
                )

                alias(
                    name = "restricted_crate",
                    actual = "//third_party/rust_crates/vendor/restricted_crate-1.0.0:restricted_crate",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:restricted_vis"],
                )
                """,
            )

            self.assertEqual(
                verify_rustc_third_party.verify_postprocess_crates_vendor(
                    root, cargo_toml
                ),
                [],
            )

            # Leave restricted_crate with public default_visibility.
            self._write(
                root,
                "third_party/rust_crates/vendor/restricted_crate-1.0.0/BUILD.bazel",
                """\
                package(default_visibility = ["//visibility:public"])
                """,
            )
            findings = (
                verify_rustc_third_party.verify_postprocess_crates_vendor(
                    root, cargo_toml
                )
            )
            self.assertEqual(len(findings), 2)
            self.assertEqual(
                findings[0].filepath,
                "third_party/rust_crates/vendor/restricted_crate-1.0.0/BUILD.bazel",
            )
            self.assertIn(
                "Restricted crate third_party/rust_crates/vendor/restricted_crate-1.0.0 must have",
                findings[0].message,
            )
            self.assertIn(
                "//third_party/rust_crates:__subpackages__",
                findings[0].message,
            )

    def test_verify_postprocess_crates_vendor_licenses(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            cargo_toml_lic_str = """\
                [patch.crates-io]
                zstd-sys = { path = "forks/zstd-sys" }

                [gn.package.zstd-sys."2.1.2+zstd.1.5.7"]
                uses_fuchsia_license = true
            """
            self._write(
                root, "third_party/rust_crates/Cargo.toml", cargo_toml_lic_str
            )
            cargo_toml_lic = tomllib.loads(textwrap.dedent(cargo_toml_lic_str))
            self._write(root, "third_party/rust_crates/BUILD.bazel", "")
            self._write(root, "third_party/rust_crates/vendor/BUILD.bazel", "")
            self._write(
                root,
                "third_party/rust_crates/forks/zstd-sys/BUILD.bazel",
                """\
                package(
                    default_package_metadata = [":package_info"],
                    default_visibility = ["//visibility:public"],
                )
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/ansi_term-0.12.1/LICENCE",
                "MIT\n",
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/ansi_term-0.12.1/BUILD.bazel",
                """\
                package(default_visibility = ["//visibility:public"])

                license(
                    name = "license",
                    license_kinds = ["@rules_license//licenses/spdx:MIT"],
                )
                """,
            )
            intree_target = root / "src/intree_crate"
            self._write(root, "src/intree_crate/LICENCE", "MIT\n")
            self._write(
                root,
                "src/intree_crate/BUILD.bazel",
                """\
                package(default_visibility = ["//visibility:public"])

                license(
                    name = "license",
                    license_kinds = ["@rules_license//licenses/spdx:MIT"],
                )
                """,
            )
            intree_dir = root / "third_party/rust_crates/intree"
            intree_dir.mkdir(parents=True, exist_ok=True)
            (intree_dir / "intree_crate").symlink_to(intree_target)

            lic_findings = (
                verify_rustc_third_party.verify_postprocess_crates_vendor(
                    root, cargo_toml_lic
                )
            )
            self.assertEqual(len(lic_findings), 3)
            self.assertEqual(
                {f.filepath for f in lic_findings},
                {
                    "third_party/rust_crates/forks/zstd-sys/BUILD.bazel",
                    "third_party/rust_crates/intree/intree_crate/BUILD.bazel",
                    "third_party/rust_crates/vendor/ansi_term-0.12.1/BUILD.bazel",
                },
            )

            self._write(
                root,
                "third_party/rust_crates/forks/zstd-sys/BUILD.bazel",
                """\
                package(
                    default_package_metadata = [
                        "//:license",
                        ":package_info",
                    ],
                    default_visibility = ["//visibility:public"],
                )
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/ansi_term-0.12.1/BUILD.bazel",
                """\
                package(default_visibility = ["//visibility:public"])

                license(
                    name = "license",
                    license_kinds = ["@rules_license//licenses/spdx:MIT"],
                    license_text = "LICENCE",
                )
                """,
            )
            self._write(
                root,
                "src/intree_crate/BUILD.bazel",
                """\
                package(default_visibility = ["//visibility:public"])

                license(
                    name = "license",
                    license_kinds = ["@rules_license//licenses/spdx:MIT"],
                    license_text = "LICENCE",
                )
                """,
            )
            self.assertEqual(
                verify_rustc_third_party.verify_postprocess_crates_vendor(
                    root, cargo_toml_lic
                ),
                [],
            )

    def test_verify_vendor_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            crate_dir = root / "third_party/rust_crates/vendor/foo-1.0.0"
            (crate_dir / "src").mkdir(parents=True)
            cargo_bytes = b'[package]\nname = "foo"\nversion = "1.0.0"\n'
            lib_bytes = b"pub fn hello() {}\n"
            (crate_dir / "Cargo.toml").write_bytes(cargo_bytes)
            (crate_dir / "src/lib.rs").write_bytes(lib_bytes)
            (crate_dir / "LICENSE").write_text("BSD")
            checksum = {
                "files": {
                    "Cargo.toml": hashlib.sha256(cargo_bytes).hexdigest(),
                    "src/lib.rs": hashlib.sha256(lib_bytes).hexdigest(),
                },
                "package": "deadbeef",
            }
            (crate_dir / ".cargo-checksum.json").write_text(
                json.dumps(checksum)
            )

            self.assertEqual(
                verify_rustc_third_party.verify_vendor_dir(root), []
            )

            # Modify src/lib.rs in-place without updating checksum.
            (crate_dir / "src/lib.rs").write_bytes(b"pub fn hacked() {}\n")
            findings = verify_rustc_third_party.verify_vendor_dir(root)
            self.assertEqual(len(findings), 1)
            self.assertEqual(
                findings[0].filepath,
                "third_party/rust_crates/vendor/foo-1.0.0/src/lib.rs",
            )
            self.assertIn("SHA-256 mismatch", findings[0].message)

            # Restore src/lib.rs and remove LICENSE.
            (crate_dir / "src/lib.rs").write_bytes(lib_bytes)
            (crate_dir / "LICENSE").unlink()
            findings = verify_rustc_third_party.verify_vendor_dir(root)
            self.assertEqual(len(findings), 1)
            self.assertIn("Missing LICENSE", findings[0].message)

    def _sample_meta(self) -> dict[str, Any]:
        root_id = (
            "path+file:///third_party/rust_crates#fuchsia-third-party@0.1.0"
        )
        foo_id = (
            "registry+https://github.com/rust-lang/crates.io-index#foo@1.0.0"
        )
        bar_id = (
            "registry+https://github.com/rust-lang/crates.io-index#bar@2.0.0"
        )
        return {
            "packages": [
                {
                    "id": root_id,
                    "name": "fuchsia-third-party",
                    "version": "0.1.0",
                    "source": None,
                    "targets": [
                        {"name": "fuchsia-third-party", "kind": ["lib"]}
                    ],
                    "dependencies": [],
                },
                {
                    "id": foo_id,
                    "name": "foo",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "targets": [{"name": "foo", "kind": ["lib"]}],
                    "dependencies": [{"name": "bar", "features": ["std"]}],
                },
                {
                    "id": bar_id,
                    "name": "bar",
                    "version": "2.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "targets": [{"name": "bar", "kind": ["lib"]}],
                    "dependencies": [],
                },
            ],
            "resolve": {
                "root": root_id,
                "nodes": [
                    {
                        "id": root_id,
                        "features": [],
                        "deps": [
                            {
                                "name": "foo",
                                "pkg": foo_id,
                                "dep_kinds": [{"kind": None, "target": None}],
                            }
                        ],
                    },
                    {
                        "id": foo_id,
                        "features": ["default"],
                        "deps": [
                            {
                                "name": "bar",
                                "pkg": bar_id,
                                "dep_kinds": [{"kind": None, "target": None}],
                            }
                        ],
                    },
                    {
                        "id": bar_id,
                        "features": ["std"],
                        "deps": [],
                    },
                ],
            },
        }

    def test_verify_build_gn(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            cargo_toml_str = """\
                [dependencies]
                foo = "1.0.0"

                [gn.package.foo."1.0.0"]
                rustflags = ["--cfg=custom_foo"]
            """
            cargo_toml = tomllib.loads(textwrap.dedent(cargo_toml_str))
            meta = self._sample_meta()
            clean_build_gn = """\
                group("foo") {
                  public_deps = [ ":foo-v1_0_0" ]
                }

                rustc_library("foo-v1_0_0") {
                  crate_name = "foo"
                  deps = []
                  deps += [ ":bar-v2_0_0" ]
                  rustflags = [
                    "--cfg=feature=\\"default\\"",
                    "--cfg=custom_foo",
                  ]
                }

                rustc_library("bar-v2_0_0") {
                  crate_name = "bar"
                  deps = []
                  rustflags = [
                    "--cfg=feature=\\"std\\"",
                  ]
                }
            """
            self._write(
                root, "third_party/rust_crates/BUILD.gn", clean_build_gn
            )
            self.assertEqual(
                verify_rustc_third_party.verify_build_gn(
                    root, cargo_toml, meta
                ),
                [],
            )

            # Remove :bar-v2_0_0 dep from foo-v1_0_0 in BUILD.gn.
            broken_gn = clean_build_gn.replace('deps += [ ":bar-v2_0_0" ]', "")
            self._write(root, "third_party/rust_crates/BUILD.gn", broken_gn)
            findings = verify_rustc_third_party.verify_build_gn(
                root, cargo_toml, meta
            )
            self.assertEqual(len(findings), 1)
            self.assertIn("foo-v1_0_0", findings[0].message)
            self.assertIn("bar-v2_0_0", findings[0].message)

            # Verify custom `groups` in [gn.package] require the named group target.
            groups_cargo_toml = tomllib.loads(
                textwrap.dedent(
                    """\
                    [dependencies]
                    foo-1_0 = { package = "foo", version = "1.0.0" }

                    [gn.package.foo."1.0.0"]
                    rustflags = ["--cfg=custom_foo"]
                    groups = [{ name = "foo-1.0" }]
                    """
                )
            )
            self._write(
                root, "third_party/rust_crates/BUILD.gn", clean_build_gn
            )
            findings = verify_rustc_third_party.verify_build_gn(
                root, groups_cargo_toml, meta
            )
            self.assertEqual(len(findings), 1)
            self.assertIn("foo-1.0", findings[0].message)

    def test_verify_bazel_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            cargo_toml_str = """\
                [dependencies]
                foo = "1.0.0"
            """
            self._write(
                root, "third_party/rust_crates/Cargo.toml", cargo_toml_str
            )
            cargo_toml = tomllib.loads(textwrap.dedent(cargo_toml_str))
            self._write(
                root,
                "build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl",
                """\
                load("@rules_rust//crate_universe:defs.bzl", "crate")

                CRATE_ANNOTATION_OVERWRITES = {}
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/crates.bzl",
                """\
                _NORMAL_DEPENDENCIES = {
                    "third_party/rust_crates": {
                        "foo": Label("//third_party/rust_crates/vendor/foo-1.0.0:foo"),
                    },
                }

                _PROC_MACRO_DEPENDENCIES = {
                    "third_party/rust_crates": {
                    },
                }
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/BUILD.bazel",
                """\
                alias(
                    name = "foo",
                    actual = "//third_party/rust_crates/vendor/foo-1.0.0:foo",
                    tags = ["manual"],
                )
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/foo-1.0.0/BUILD.bazel",
                """\
                package_info(
                    name = "package_info",
                    package_name = "foo",
                    package_version = "1.0.0",
                )

                rust_library(
                    name = "foo",
                    crate_features = [
                        "default",
                    ],
                    deps = [
                        "//third_party/rust_crates/vendor/bar-2.0.0:bar",
                    ],
                )
                """,
            )
            self._write(
                root,
                "third_party/rust_crates/vendor/bar-2.0.0/BUILD.bazel",
                """\
                package_info(
                    name = "package_info",
                    package_name = "bar",
                    package_version = "2.0.0",
                )

                rust_library(
                    name = "bar",
                    crate_features = [
                        "std",
                    ],
                )
                """,
            )
            meta = self._sample_meta()
            tree_outs = [
                textwrap.dedent(
                    """\
                    0;fuchsia-third-party v0.1.0;;
                    1;foo v1.0.0;default;
                    2;bar v2.0.0;std;
                    """
                )
            ]

            self.assertEqual(
                verify_rustc_third_party.verify_bazel_files(
                    root, cargo_toml, meta, tree_outs
                ),
                [],
            )

            # Mutate bar's crate_features in vendor/bar-2.0.0/BUILD.bazel.
            self._write(
                root,
                "third_party/rust_crates/vendor/bar-2.0.0/BUILD.bazel",
                """\
                package_info(
                    name = "package_info",
                    package_name = "bar",
                    package_version = "2.0.0",
                )

                rust_library(
                    name = "bar",
                    crate_features = [
                        "extra_feature",
                        "std",
                    ],
                )
                """,
            )
            findings = verify_rustc_third_party.verify_bazel_files(
                root, cargo_toml, meta, tree_outs
            )
            self.assertEqual(len(findings), 1)
            self.assertEqual(
                findings[0].filepath,
                "third_party/rust_crates/vendor/bar-2.0.0/BUILD.bazel",
            )
            self.assertIn("extra_feature", findings[0].message)


if __name__ == "__main__":
    unittest.main()
