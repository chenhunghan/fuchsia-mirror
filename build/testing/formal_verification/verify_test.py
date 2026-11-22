#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for `verify.py`."""

import os
import pathlib
import tempfile
import unittest
from unittest import mock

import extraction
import lake_audit
import verify


class VerifyTest(unittest.TestCase):
    def _create_placeholder_inputs(self, root: pathlib.Path) -> list[str]:
        lib_rs = root / "lib.rs"
        lakefile = root / "lakefile.lean"
        assumptions = root / "Assumptions.lean"
        spec = root / "Spec.lean"
        contract = root / "Contract.lean"
        proof = root / "Proof.lean"

        for p in (lib_rs, lakefile, assumptions, spec, contract, proof):
            p.write_text("-- placeholder\n")

        return [
            "--crate-name",
            "ebpf_core",
            "--crate-src",
            str(lib_rs),
            "--lakefile",
            str(lakefile),
            "--assumptions-lean",
            str(assumptions),
            "--spec-lean",
            str(spec),
            "--contract-lean",
            str(contract),
            "--proof-lean",
            str(proof),
            "--contract-lean-target",
            "StarnixEbpfContract",
        ]

    def test_skips_cleanly_when_if_toolchains_present_and_unconfigured(
        self,
    ) -> None:
        saved = {
            k: os.environ.pop(k, None)
            for k in (
                "LEAN_BIN_DIR",
                "AENEAS_BIN_DIR",
                "AENEAS_LEAN_DIR",
                "CHARON_RUST_TOOLCHAIN_DIR",
            )
        }
        try:
            with tempfile.TemporaryDirectory() as tmp:
                argv = self._create_placeholder_inputs(pathlib.Path(tmp)) + [
                    "--if-toolchains-present"
                ]
                self.assertEqual(verify.main(argv), 0)
        finally:
            for k, v in saved.items():
                if v is not None:
                    os.environ[k] = v

    def test_raises_when_toolchains_missing_without_flag(self) -> None:
        saved = {
            k: os.environ.pop(k, None)
            for k in (
                "LEAN_BIN_DIR",
                "AENEAS_BIN_DIR",
                "AENEAS_LEAN_DIR",
                "CHARON_RUST_TOOLCHAIN_DIR",
            )
        }
        try:
            with tempfile.TemporaryDirectory() as tmp:
                argv = self._create_placeholder_inputs(pathlib.Path(tmp))
                with self.assertRaisesRegex(
                    ValueError, "Verification toolchains .* must be configured"
                ):
                    verify.main(argv)
        finally:
            for k, v in saved.items():
                if v is not None:
                    os.environ[k] = v

    def test_orchestrates_extraction_and_lake_audit_when_toolchains_present(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            argv = self._create_placeholder_inputs(root) + [
                "--allow-sorry-in-proof"
            ]
            fake_toolchains = extraction.ToolchainConfig(
                lean_bin_dir=root / "lean",
                aeneas_bin_dir=root / "aeneas",
                aeneas_lean_dir=root / "aeneas" / "backends" / "lean",
                charon_rust_toolchain_dir=root / "rust",
            )
            with (
                mock.patch.object(
                    extraction.ToolchainConfig,
                    "resolve",
                    return_value=fake_toolchains,
                ),
                mock.patch.object(
                    extraction,
                    "prepare_lake_overlay",
                    return_value=root / "overlay",
                ) as mock_prepare,
                mock.patch.object(
                    lake_audit,
                    "run_lake_and_audit",
                ) as mock_audit,
            ):
                rc = verify.main(argv)
                self.assertEqual(rc, 0)
                mock_prepare.assert_called_once()
                mock_audit.assert_called_once_with(
                    overlay_dir=root / "overlay",
                    contract_target="StarnixEbpfContract",
                    checker_module=None,
                    allow_sorry_in_proof=True,
                    toolchains=fake_toolchains,
                )

    def test_if_toolchains_present_checks_forbidden_lean_constructs(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            argv = self._create_placeholder_inputs(root) + [
                "--if-toolchains-present"
            ]
            (root / "Proof.lean").write_text(
                "theorem t : True := by sorry\n", encoding="utf-8"
            )
            with mock.patch.object(
                extraction.ToolchainConfig, "resolve", return_value=None
            ):
                with self.assertRaisesRegex(ValueError, "Forbidden `sorry`"):
                    verify.main(argv)
                self.assertEqual(
                    verify.main(argv + ["--allow-sorry-in-proof"]), 0
                )

                (root / "Spec.lean").write_text(
                    "axiom spoof : False\n", encoding="utf-8"
                )
                with self.assertRaisesRegex(
                    ValueError, "Forbidden construct `axiom`"
                ):
                    verify.main(argv + ["--allow-sorry-in-proof"])

    def test_prepare_only_requires_keep_workdir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            argv = self._create_placeholder_inputs(pathlib.Path(tmp)) + [
                "--prepare-only"
            ]
            with self.assertRaises(SystemExit):
                verify.parse_args(argv)

    def test_prepare_only_with_keep_workdir_skips_lake_audit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            workdir = root / "persisted"
            argv = self._create_placeholder_inputs(root) + [
                "--prepare-only",
                "--keep-workdir",
                str(workdir),
            ]
            fake_toolchains = extraction.ToolchainConfig(
                lean_bin_dir=root / "lean",
                aeneas_bin_dir=root / "aeneas",
                aeneas_lean_dir=root / "aeneas" / "backends" / "lean",
                charon_rust_toolchain_dir=root / "rust",
            )
            with (
                mock.patch.object(
                    extraction.ToolchainConfig,
                    "resolve",
                    return_value=fake_toolchains,
                ),
                mock.patch.object(
                    extraction,
                    "prepare_lake_overlay",
                    return_value=workdir / "lean-overlay",
                ) as mock_prepare,
                mock.patch.object(
                    lake_audit,
                    "run_lake_and_audit",
                ) as mock_audit,
            ):
                self.assertEqual(verify.main(argv), 0)
                mock_prepare.assert_called_once()
                mock_audit.assert_not_called()

    def test_parse_args_uses_none_defaults_for_append_flags(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            argv = self._create_placeholder_inputs(pathlib.Path(tmp))
            args = verify.parse_args(argv)
            self.assertIsNone(args.charon_excludes)
            self.assertIsNone(args.charon_args)
            self.assertIsNone(args.aeneas_args)

    def test_resolve_input_path_uses_runfiles_env_vars(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            rel_path = "src/example/Spec.lean"
            for env_var in ("RUNFILES_DIR", "TEST_SRCDIR"):
                runfiles_dir = root / f"{env_var.lower()}.runfiles"
                target = runfiles_dir / "_main" / rel_path
                target.parent.mkdir(parents=True)
                target.write_text("-- spec\n", encoding="utf-8")
                with mock.patch.dict(
                    os.environ,
                    {env_var: str(runfiles_dir)},
                    clear=False,
                ):
                    if env_var == "TEST_SRCDIR":
                        with mock.patch.dict(
                            os.environ, {"RUNFILES_DIR": ""}, clear=False
                        ):
                            self.assertEqual(
                                verify._resolve_input_path(rel_path), target
                            )
                    else:
                        self.assertEqual(
                            verify._resolve_input_path(rel_path), target
                        )


if __name__ == "__main__":
    unittest.main()
